use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::config::{Config, TrackerInterval, TrackerKind};
use crate::date;
use crate::db::{EntryObject, TrackerObject, TrackerValue};
use crate::editor::open_editor_for_body;
use crate::global;
use crate::tracker::parse_tracker_value;
use crate::types::{Entry, TaskRef};

pub(super) async fn record_entry(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    entry: Entry,
) -> Result<()> {
    let is_session = entry.duration.is_some();
    if is_session && !atty::is(atty::Stream::Stdin) {
        anyhow::bail!("Mood sessions require an interactive terminal");
    }

    let (mood, duration) = if let Some(secs) = entry.duration {
        let elapsed_secs = {
            use std::io::Write;

            /// Re-enables raw mode on drop so any exit path (bail, `?`,
            /// early return) leaves the terminal cooked.
            struct RawGuard;
            impl RawGuard {
                fn new() -> std::io::Result<Self> {
                    crossterm::terminal::enable_raw_mode()?;
                    Ok(Self)
                }
            }
            impl Drop for RawGuard {
                fn drop(&mut self) {
                    let _ = crossterm::terminal::disable_raw_mode();
                }
            }

            // `-q`: the timer still runs (and stays interruptible); only
            // the rendering and the completion logs are suppressed.
            let render = !opts.quiet();

            let _raw = RawGuard::new()?;
            let total_dur = std::time::Duration::from_secs(secs as u64);
            let mut elapsed_active = std::time::Duration::ZERO;
            let mut last_tick = std::time::Instant::now();
            let mut ended_early = false;

            if render {
                print!("\r{}", crate::date::format_countdown(secs));
                let _ = std::io::stdout().flush();
            }

            while elapsed_active < total_dur {
                if crossterm::event::poll(std::time::Duration::from_millis(200))?
                    && let crossterm::event::Event::Key(key) = crossterm::event::read()?
                {
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                        && key.code == crossterm::event::KeyCode::Char('c')
                    {
                        println!();
                        anyhow::bail!("Session aborted!");
                    }
                    if key.code == crossterm::event::KeyCode::Enter {
                        // Pause the timer while the confirm prompt runs:
                        // leave raw mode for cliclack, re-enter after.
                        let _ = crossterm::terminal::disable_raw_mode();
                        if render {
                            print!("\r\x1b[2K");
                            let _ = std::io::stdout().flush();
                        }
                        let confirm_res = cliclack::confirm("End the session? [y/N]")
                            .initial_value(false)
                            .interact();
                        match confirm_res {
                            Ok(true) => {
                                ended_early = true;
                                break;
                            }
                            Ok(false) => {
                                crossterm::terminal::enable_raw_mode()?;
                                last_tick = std::time::Instant::now();
                            }
                            Err(_) => {
                                anyhow::bail!("Session aborted!");
                            }
                        }
                    }
                }
                let now = std::time::Instant::now();
                elapsed_active += now.duration_since(last_tick);
                last_tick = now;
                if elapsed_active >= total_dur {
                    break;
                }
                if render {
                    let remaining = total_dur.saturating_sub(elapsed_active);
                    print!(
                        "\r{}",
                        crate::date::format_countdown(remaining.as_secs() as i64)
                    );
                    let _ = std::io::stdout().flush();
                }
            }

            drop(_raw);
            if render {
                print!("\r\x1b[2K");
                let _ = std::io::stdout().flush();
                if ended_early {
                    cliclack::log::info("Session ended")?;
                } else {
                    crate::notify::notify("Session Complete", "Mood session finished!");
                    cliclack::log::success("Session complete!")?;
                }
            }

            elapsed_active.as_secs() as i64
        };

        let thoughts_res = cliclack::input(&config.editor.pomo_prompt)
            .default_input("")
            .interact();

        let thoughts: String = match thoughts_res {
            Ok(t) => t,
            Err(_) => {
                // Ctrl-C aborts and skips recording the session (return Ok).
                return Ok(());
            }
        };

        (thoughts.trim().to_string(), Some(elapsed_secs))
    } else {
        (entry.mood, None)
    };

    let trackers = entry.trackers;
    let task_ref = entry.task_ref;
    let entry_count = entry.count;
    let body = entry.body;

    // Body resolution: `Ok(text)` is post-delimiter text used as-is;
    // `Err(0)` is a line without a body delimiter; `Err(n)` is a bare
    // delimiter of `n` dots — the body editor opens seeded with the `n`th
    // mood template (see `open_editor_for_body`).
    let body = match body {
        Ok(b) => b,
        Err(0) => String::new(),
        Err(dots) => open_editor_for_body(&config.editor.mood_template, dots)?,
    };

    // A ref with a count payload alone is still work to do (`im +7 2`):
    // the completion applies without a mood row.
    if mood.is_empty()
        && trackers.is_empty()
        && body.is_empty()
        && duration.is_none()
        && entry_count.is_none()
    {
        anyhow::bail!("Nothing to log");
    }

    // Determine the timestamp (Unix epoch in seconds).
    let time_epoch = date::now();

    // Parse and validate tracker values against their declared kind.
    // Raw strings are interpreted here (not in the parser) so the config's
    // kind (text/integer/float/duration/null) determines how each value is
    // stored. Replace mode (interval + `cumulative: false`) is one shared
    // insertion strategy: the slot's previous entries of the tracker are
    // dropped and the new row inserted (`replace_slot`, inside
    // `create_entry`); cumulative mode appends every log. `strict` gates the
    // raw value (or, for null trackers, the entry time) before inserting.
    let mut tracker_objects: Vec<TrackerObject> = Vec::with_capacity(trackers.len());
    for (tracker_type, raw) in &trackers {
        let tracker = config.tracker.get(tracker_type).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown tracker type '{}' not found in config",
                tracker_type
            )
        })?;
        match tracker.kind {
            TrackerKind::Null => {
                // A trailing `-<name>` with no value; for Null trackers the
                // entry is a timestamp marker.
                if !raw.is_empty() {
                    anyhow::bail!(
                        "Null tracker '{}' does not take a value (use '-{}' with no value)",
                        tracker_type,
                        tracker_type
                    );
                }
                let Some(interval) = tracker.interval else {
                    anyhow::bail!(
                        "Null tracker '{}' requires an interval to log (see config)",
                        tracker_type
                    );
                };
                // strict gates *when* the tracker may be logged: the entry
                // time must fall in the circular [low, high] offset zone.
                if tracker.strict {
                    let (Some(low), Some(high)) = (tracker.low, tracker.high) else {
                        anyhow::bail!(
                            "tracker '{}': strict requires both low and high bounds",
                            tracker_type
                        );
                    };
                    if !crate::tracker::null_zone_contains(tracker, time_epoch) {
                        anyhow::bail!(
                            "tracker '{}': cannot log at this time — outside the strict [{}, {}] offset zone",
                            tracker_type,
                            low,
                            high
                        );
                    }
                }
                // Replace: the slot's previous markers are dropped and a
                // fresh marker inserted (score 0). Cumulative: every log
                // appends its own row (score 0).
                let replace_slot = if interval.cumulative {
                    None
                } else {
                    Some(interval_slot(time_epoch, interval).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Could not compute the interval slot for tracker '{}'",
                            tracker_type
                        )
                    })?)
                };
                tracker_objects.push(TrackerObject {
                    tracker_type: tracker_type.clone(),
                    value: TrackerValue::Integer(0),
                    replace_slot,
                });
            }
            _ => {
                if raw.is_empty() {
                    anyhow::bail!("Tracker '{}' requires a value", tracker_type);
                }
                let value = parse_tracker_value(tracker_type, tracker.kind, raw)?;
                // strict gates the raw logged value.
                if tracker.strict {
                    let measure = match &value {
                        TrackerValue::Text(s) => crate::tracker::text_len_chars(s) as f64,
                        TrackerValue::Integer(n) => *n as f64,
                        TrackerValue::Float(f) => *f,
                    };
                    crate::tracker::enforce_strict(tracker_type, tracker, measure)
                        .map_err(anyhow::Error::msg)?;
                }
                // Replace mode keeps one row per interval slot (the slot's
                // previous row is dropped inside `create_entry`); cumulative
                // and non-interval trackers always append.
                let replace_slot = if tracker.interval.is_some_and(|iv| !iv.cumulative) {
                    tracker
                        .interval
                        .and_then(|iv| interval_slot(time_epoch, iv))
                } else {
                    None
                };
                tracker_objects.push(TrackerObject {
                    tracker_type: tracker_type.clone(),
                    value,
                    replace_slot,
                });
            }
        }
    }
    // Journal-only entries (empty mood) never embed. When moods.backfill is
    // enabled, mood entries are also inserted with NULL embedding/score to keep
    // logging fast; UI/CLI color passes compute and backfill them on display.
    // The model is bundled into the binary, so the embedder is always
    // available — a per-text embedding failure (e.g. an un-tokenizable string)
    // stores no embedding rather than losing the entry.
    let (embedding_blob, score) = if mood.is_empty() || config.moods.backfill {
        (None, None)
    } else {
        let embedder = global::embedder_async().await;
        match embedder.embed(&mood, &config.moods.axes.prefix_string) {
            Ok(v) => (
                Some(global::embedding_to_blob(&v)),
                Some(crate::color::predict_saliency(embedder, &mood)),
            ),
            Err(_) => (None, None),
        }
    };

    let entry_obj = EntryObject {
        mood,
        body,
        time: time_epoch,
        embedding: embedding_blob,
        score,
        trackers: tracker_objects,
        duration,
        todo_id: None,
    };

    let mood_id = crate::db::create_entry(pool, &entry_obj).await?;
    log::debug!("Inserted mood with id={:?}", mood_id);

    // Task reference: a single `+<ref>` (`+<id>` / `+<words>` / bare `+`
    // to pick interactively). A count payload (`+7 2`) applies completions
    // to the resolved oneshot task; the ref is otherwise — or additionally
    // — linked to the mood entry via `mood.todo_id`. A link needs a mood row
    // to attach to, so a tracker-only line can only carry a payload.
    if let Some(task_ref) = task_ref {
        let task_id = match task_ref {
            TaskRef::Id(short_id) => {
                let Some((id, _name)) =
                    crate::db::fetch_task_id_by_short_id(pool, short_id).await?
                else {
                    anyhow::bail!("No task with short id {short_id} exists");
                };
                id
            }
            TaskRef::Words(words) => {
                let matches = crate::db::fetch_task_matching_words(pool, &words).await?;
                match matches.len() {
                    0 => anyhow::bail!("No task found matching query '{}'", words.join(" ")),
                    1 => matches[0].id,
                    n => anyhow::bail!(
                        "Multiple tasks match query '{}' (found {})",
                        words.join(" "),
                        n
                    ),
                }
            }
            TaskRef::Pick => {
                if !atty::is(atty::Stream::Stdin) {
                    anyhow::bail!("Picking a task to link requires an interactive terminal");
                }
                crate::ui::oneshots::OneshotPickerApp::new(config.clone(), opts.fullscreen)
                    .await?
                    .run()
                    .await?
                    .map(|(id, _)| id)
                    .ok_or_else(|| anyhow::anyhow!("Cancelled"))?
            }
        };

        if let Some(delta) = entry_count {
            // Completions only apply to incomplete oneshot tasks.
            let info = crate::db::fetch_oneshot_task_by_id_for_update(pool, task_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Task with id {task_id} is not an addressable oneshot (completed \
                         tasks and other kinds take no completions)"
                    )
                })?;
            super::update::update_oneshot(pool, opts, &info, Some(delta)).await?;
        }

        match mood_id {
            Some(mood_id) => crate::db::link_mood_to_tasks(pool, mood_id, &[task_id]).await?,
            None if entry_count.is_none() => {
                anyhow::bail!("Task links (+<ref>) require a mood or journal entry to attach to");
            }
            None => {}
        }
    }

    crate::output::display_entry(config, &entry_obj, opts)?;

    Ok(())
}

/// The `[start, end)` replacement slot containing `time_epoch` for a
/// calendar interval (anchor + span): `[anchor + span*k, anchor + span*(k+1))`.
/// Slots tile the timeline in both directions from the anchor.
fn interval_slot(time_epoch: i64, interval: TrackerInterval) -> Option<(i64, i64)> {
    crate::date::interval_slot_unix_secs(interval.anchor, interval.span, time_epoch)
}

#[cfg(test)]
mod tests {
    use super::interval_slot;
    use crate::config::TrackerInterval;
    use crate::date;
    use jiff::Span;

    fn day_interval() -> TrackerInterval {
        TrackerInterval {
            anchor: date::day_start(date::now()) - 30 * 86_400,
            span: Span::new().days(1),
            cumulative: false,
        }
    }

    /// The slot always contains the entry and slots are adjacent.
    #[test]
    fn interval_slot_contains_entry() {
        let t = date::today_start() + 12 * 3600;
        for interval in [
            day_interval(),
            TrackerInterval {
                anchor: date::today_start() - 86_400,
                span: Span::new().minutes(30),
                cumulative: false,
            },
        ] {
            let (start, end) = interval_slot(t, interval).unwrap();
            assert!(t >= start && t < end, "{t} not in [{start}, {end})");
            // Adjacent slot: [end, end + span).
            let (s2, e2) = interval_slot(end, interval).unwrap();
            assert_eq!(s2, end, "slots must be adjacent");
            assert!(e2 > end);
        }
    }

    /// Sub-day intervals: entries in the same 30-min bucket share a slot,
    /// crossing a boundary doesn't.
    #[test]
    fn interval_slot_sub_day() {
        let anchor = date::today_start() - 86_400;
        let interval = TrackerInterval {
            anchor,
            span: Span::new().minutes(30),
            cumulative: false,
        };
        let t = date::today_start() + 10 * 3600; // 10:00 local
        let bucket = interval_slot(t, interval).unwrap();
        assert_eq!(interval_slot(t + 600, interval).unwrap(), bucket); // 10:10
        assert_ne!(interval_slot(t + 1801, interval).unwrap(), bucket); // 10:30:01
    }
}
