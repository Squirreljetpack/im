use anyhow::Result;

use crate::cli::CliOpts;

/// Display a logged entry: mood, trackers, and body as field/value
/// rows (tab-separated, vertically aligned). Quiet suppresses the whole
/// confirmation; otherwise all rows are always shown.
pub fn display_entry(entry: &crate::db::EntryObject, opts: &CliOpts) -> Result<()> {
    if opts.quiet() {
        return Ok(());
    }
    let mut rows: Vec<(String, String)> = Vec::new();
    if !entry.mood.is_empty() {
        rows.push(("Mood".to_string(), entry.mood.clone()));
    }
    for tracker in &entry.trackers {
        rows.push((tracker.tracker_type.clone(), tracker.value.to_string()));
    }
    if !entry.body.is_empty() {
        rows.push(("Body".to_string(), entry.body.clone()));
    }
    super::print_rows(&rows);
    Ok(())
}
