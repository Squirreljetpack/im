use anyhow::Result;

use crate::cli::CliOpts;
use crate::config::TrackerKind;
use crate::db::TrackerValue;

/// Display a logged entry: mood, trackers, and body as field/value
/// rows (tab-separated, vertically aligned). Quiet suppresses the whole
/// confirmation; otherwise all rows are always shown.
pub fn display_entry(
    config: &crate::config::Config,
    entry: &crate::db::EntryObject,
    opts: &CliOpts,
) -> Result<()> {
    if opts.quiet() {
        return Ok(());
    }
    let mut rows: Vec<(String, String)> = Vec::new();
    if !entry.mood.is_empty() {
        rows.push(("Mood".to_string(), entry.mood.clone()));
    }
    if let Some(dur) = entry.duration {
        rows.push(("Duration".to_string(), crate::date::format_duration(dur)));
    }
    for tracker in &entry.trackers {
        // Null tracker rows store score 0 (the entry is a timestamp/count
        // marker) and carry no payload — the confirmation shows an empty
        // value; duration rows show the formatted time instead of raw
        // seconds; other kinds show the logged value.
        let value = match config.tracker.get(&tracker.tracker_type) {
            Some(s) if s.kind == TrackerKind::Null => String::new(),
            Some(s) if s.kind == TrackerKind::Duration => {
                match tracker.value {
                    TrackerValue::Float(f) => crate::date::format_tracker_duration(f),
                    _ => tracker.value.to_string(),
                }
            }
            _ => tracker.value.to_string(),
        };
        rows.push((tracker.tracker_type.clone(), value));
    }
    if !entry.body.is_empty() {
        rows.push(("Body".to_string(), entry.body.clone()));
    }
    super::print_rows(&rows);
    Ok(())
}
