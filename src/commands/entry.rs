use anyhow::Result;
use sqlx::SqlitePool;

use crate::global;
use crate::cli::CliOpts;
use crate::config::{Config, TrackerInterval, TrackerKind};
use crate::date;
use crate::db::{EntryObject, NullUpsert, TrackerObject, TrackerValue};
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
    // kind (text/number/float/null) determines how each value is stored.
    // Text/float trackers with an interval keep one entry per calendar
    // interval slot (re-logging in the same slot replaces the previous
    // entry, inside `create_entry`); number trackers always accumulate.
    // Null trackers with an interval either move the slot's entry to now
    // (both min/max set — the entry is a timestamp marker) or increment its
    // count (count mode).
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
                // entry is a timestamp marker. Without an interval the
                // tracker is unsupported (no slot, no count semantics).
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
                // Count mode when either bound is missing: the score counts
                // the logs in the current slot. With both bounds the entry
                // is a time marker (score stays 0, color from the time).
                let count_mode = tracker.min.is_none() || tracker.max.is_none();
                let slot = interval_slot(time_epoch, interval).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Could not compute the interval slot for tracker '{}'",
                        tracker_type
                    )
                })?;
                tracker_objects.push(TrackerObject {
                    tracker_type: tracker_type.clone(),
                    value: TrackerValue::Number(if count_mode { 1 } else { 0 }),
                    replace_slot: None,
                    null_upsert: Some(NullUpsert {
                        slot,
                        increment: count_mode,
                    }),
                });
            }
            _ => {
                if raw.is_empty() {
                    anyhow::bail!("Tracker '{}' requires a value", tracker_type);
                }
                let value = parse_tracker_value(tracker_type, tracker.kind, raw)?;
                let replace_slot = if matches!(tracker.kind, TrackerKind::Text | TrackerKind::Float)
                {
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
                    null_upsert: None,
                });
            }
        }
    }

    // Resolve the mood embedding and its saliency score before opening the
    // transaction. Journal-only entries (empty mood) never embed; the model
    // is bundled into the binary, so the embedder is always available — a
    // per-text embedding failure (e.g. an un-tokenizable string) stores no
    // embedding rather than losing the entry. The score is computed here so
    // color passes later skip the ONNX saliency prediction.
    let embedder = global::embedder();
    let (embedding_blob, score) = if mood.is_empty() {
        (None, None)
    } else {
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

    crate::output::display_entry(&entry_obj, opts)?;

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
        };
        let t = date::today_start() + 10 * 3600; // 10:00 local
        let bucket = interval_slot(t, interval).unwrap();
        assert_eq!(interval_slot(t + 600, interval).unwrap(), bucket); // 10:10
        assert_ne!(interval_slot(t + 1801, interval).unwrap(), bucket); // 10:30:01
    }
}
