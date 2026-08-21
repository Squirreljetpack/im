use anyhow::Result;
use sqlx::SqlitePool;

use crate::global;
use crate::cli::CliOpts;
use crate::config::{Config, TrackerInterval, TrackerKind};
use crate::date;
use crate::db::{EntryObject, TrackerObject, TrackerValue};
use crate::editor::open_editor_for_body;
use crate::tracker::parse_tracker_value;
use crate::types::Entry;

pub(super) async fn record_entry(
    pool: &SqlitePool,
    config: &Config,
    opts: &CliOpts,
    entry: Entry,
) -> Result<()> {
    let mood = entry.mood;
    let trackers = entry.trackers;
    let task_links = entry.task_links;
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

    if mood.is_empty() && trackers.is_empty() && body.is_empty() {
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
                            tracker_type, low, high
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
    };

    let mood_id = crate::db::create_entry(pool, &entry_obj).await?;
    log::debug!("Inserted mood with id={:?}", mood_id);

    // Task links: `-<short id>` tokens resolved to row ids and recorded in
    // the link table — a plain link, not a completion. They need a mood
    // row to attach to, so a tracker-only entry cannot carry links.
    if !task_links.is_empty() {
        let Some(mood_id) = mood_id else {
            anyhow::bail!("Task links (-<id>) require a mood or journal entry to attach to");
        };
        let mut resolved = Vec::with_capacity(task_links.len());
        for short_id in task_links {
            let Some((task_id, _name)) =
                crate::db::fetch_task_id_by_short_id(pool, short_id).await?
            else {
                anyhow::bail!("No task with short id {} exists", short_id);
            };
            resolved.push(task_id);
        }
        crate::db::link_mood_to_tasks(pool, mood_id, &resolved).await?;
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
