use anyhow::Result;
use sqlx::SqlitePool;

use crate::cli::CliOpts;
use crate::config::{Config, TrackerKind};
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
) -> Result<bool> {
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

            // `-q`: the timer still runs (and stays interruptible); the
            // rendering, notifications, and completion logs are suppressed.
            let render = !opts.quiet();

            let _raw = RawGuard::new()?;
            let total_dur = std::time::Duration::from_secs(secs as u64);
            let mut elapsed_active = std::time::Duration::ZERO;
            let mut last_tick = std::time::Instant::now();
            let mut ended_early = false;

            if render {
                let initial = config.session.countdown.render(
                    total_dur,
                    std::time::Duration::ZERO,
                    &config.session.countdown_style,
                );
                print!("\r\x1b[2K{initial}");
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
                                if render {
                                    let formatted = config.session.countdown.render(
                                        total_dur,
                                        elapsed_active,
                                        &config.session.countdown_style,
                                    );
                                    print!("\r\x1b[2K{formatted}");
                                    let _ = std::io::stdout().flush();
                                }
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
                    let formatted = config.session.countdown.render(
                        total_dur,
                        elapsed_active,
                        &config.session.countdown_style,
                    );
                    print!("\r\x1b[2K{formatted}");
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
                    let purpose = entry.purpose.as_deref().unwrap_or("");
                    let notif_title = &config.session.title;
                    let notif_body = if purpose.is_empty() {
                        "Session complete!"
                    } else {
                        purpose
                    };
                    let mut notif = notify::Notification::new(notif_title, notif_body)
                        .kind(config.session.kind)
                        .opt_dismiss(config.session.dismiss);
                    if config.session.delay > 0 {
                        notif = notif.delay(config.session.delay);
                    }
                    if config.session.play_sound || opts.verbose() {
                        if !config.session.sound.as_os_str().is_empty() {
                            if let Ok(mut sound) = notify::Sound::from_name_or_path(&config.session.sound) {
                                if let Some(duration) = config.session.sound_duration {
                                    sound.duration = Some(duration);
                                }
                                notif = notif.sound(sound);
                            }
                        }
                    }
                    let _ = notif.send();
                    cliclack::log::success("Session complete!")?;
                }
            }

            elapsed_active.as_secs() as i64
        };

        let prompt = if config.session.prompt.is_empty() {
            "Thoughts:"
        } else {
            &config.session.prompt
        };
        let thoughts_res = cliclack::input(prompt)
            .default_input("")
            .interact();

        let thoughts: String = match thoughts_res {
            Ok(t) => t,
            Err(_) => {
                // Ctrl-C aborts and skips recording the session (not an
                // error, but nothing was written).
                return Ok(false);
            }
        };

        let thoughts_trimmed = thoughts.trim();
        if thoughts_trimmed.contains('\t') {
            anyhow::bail!("Thoughts cannot contain tab characters");
        }
        let mood = match entry.purpose.as_deref() {
            Some(purpose) if !purpose.is_empty() => {
                if thoughts_trimmed.is_empty() {
                    format!("{purpose}:")
                } else {
                    format!("{purpose}: {thoughts_trimmed}")
                }
            }
            _ => thoughts_trimmed.to_string(),
        };

        (mood, Some(elapsed_secs))
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
                let slot = crate::tracker::interval_slot(interval, time_epoch).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Could not compute the interval slot for tracker '{}'",
                        tracker_type
                    )
                })?;
                // Replace: the slot's previous markers are dropped and a
                // fresh marker stores its absolute seconds from the slot
                // start. Cumulative: every log appends its own row with
                // score 0; the count is the value.
                let position = if interval.cumulative {
                    0
                } else {
                    time_epoch - slot.0
                };
                let replace_slot = if interval.cumulative {
                    None
                } else {
                    Some(slot)
                };
                tracker_objects.push(TrackerObject {
                    tracker_type: tracker_type.clone(),
                    value: TrackerValue::Integer(position),
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
                        .and_then(|iv| crate::tracker::interval_slot(iv, time_epoch))
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
    let mood_text = crate::color::mood_text_for_embedding(&mood);
    let (embedding_blob, score) = if mood_text.is_empty() || config.moods.backfill {
        (None, None)
    } else {
        let embedder = global::embedder_async().await;
        match embedder.embed(mood_text, &config.moods.axes.prefix_string) {
            Ok(v) => (
                Some(global::embedding_to_blob(&v)),
                Some(crate::color::predict_saliency(embedder, mood_text)),
            ),
            Err(_) => (None, None),
        }
    };

    let linked_task_id = match task_ref {
        Some(TaskRef::Pick) => {
            if !atty::is(atty::Stream::Stdin) {
                anyhow::bail!("Picking a task to link requires an interactive terminal");
            }
            let id = crate::ui::oneshots::OneshotPickerApp::new(config.clone(), opts.fullscreen)
                .await?
                .run()
                .await?
                .map(|(id, _)| id)
                .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;
            Some(id)
        }
        Some(task_ref) => Some(super::refs::resolve_task_ref(pool, &task_ref).await?),
        None => None,
    };

    if let (Some(task_id), Some(delta)) = (linked_task_id, entry_count) {
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

    let should_link_mood = !mood.is_empty()
        || !body.is_empty()
        || duration.is_some()
        || !tracker_objects.is_empty();

    let todo_id = if should_link_mood {
        linked_task_id
    } else {
        None
    };

    let entry_obj = EntryObject {
        mood,
        body,
        time: date::LogTime::local(time_epoch),
        location: date::current_location(),
        embedding: embedding_blob,
        score,
        trackers: tracker_objects,
        duration,
        todo_id,
    };

    let mood_id = crate::db::create_entry(pool, &entry_obj).await?;
    log::debug!("Inserted mood with id={:?}", mood_id);

    let wrote = mood_id.is_some() || !entry_obj.trackers.is_empty() || entry_count.is_some();
    crate::output::display_entry(config, &entry_obj, opts)?;

    Ok(wrote)
}

#[cfg(test)]
mod tests {
    use crate::config::TrackerInterval;
    use crate::date;
    use crate::tracker::interval_slot;
    use jiff::Span;

    #[test]
    fn test_format_session_mood() {
        let fmt = |purpose: Option<&str>, thoughts: &str| -> String {
            let thoughts_trimmed = thoughts.trim();
            match purpose {
                Some(p) if !p.is_empty() => {
                    if thoughts_trimmed.is_empty() {
                        format!("{p}:")
                    } else {
                        format!("{p}: {thoughts_trimmed}")
                    }
                }
                _ => thoughts_trimmed.to_string(),
            }
        };

        assert_eq!(fmt(Some("coding"), "focused"), "coding: focused");
        assert_eq!(fmt(Some("coding"), ""), "coding:");
        assert_eq!(fmt(None, "focused"), "focused");
        assert_eq!(fmt(Some(""), "focused"), "focused");
    }

    fn day_interval() -> TrackerInterval {
        TrackerInterval {
            anchor: date::zoned_from_unix_secs(date::day_start(date::now()) - 30 * 86_400)
                .unwrap()
                .datetime(),
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
                anchor: date::zoned_from_unix_secs(date::today_start() - 86_400)
                    .unwrap()
                    .datetime(),
                span: Span::new().minutes(30),
                cumulative: false,
            },
        ] {
            let (start, end) = interval_slot(interval, t).unwrap();
            assert!(t >= start && t < end, "{t} not in [{start}, {end})");
            // Adjacent slot: [end, end + span).
            let (s2, e2) = interval_slot(interval, end).unwrap();
            assert_eq!(s2, end, "slots must be adjacent");
            assert!(e2 > end);
        }
    }

    /// Sub-day intervals: entries in the same 30-min bucket share a slot,
    /// crossing a boundary doesn't.
    #[test]
    fn interval_slot_sub_day() {
        let anchor = date::zoned_from_unix_secs(date::today_start() - 86_400)
            .unwrap()
            .datetime();
        let interval = TrackerInterval {
            anchor,
            span: Span::new().minutes(30),
            cumulative: false,
        };
        let t = date::today_start() + 10 * 3600; // 10:00 local
        let bucket = interval_slot(interval, t).unwrap();
        assert_eq!(interval_slot(interval, t + 600).unwrap(), bucket); // 10:10
        assert_ne!(interval_slot(interval, t + 1801).unwrap(), bucket); // 10:30:01
    }
}
