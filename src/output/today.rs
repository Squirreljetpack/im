use crate::config::Config;
use crate::today::{EntryKind, TodayEntry};

/// Format today view entries into tab-separated output text.
///
/// The badge cell uses the same render-time derivation as the TUI
/// (`TodayEntry::badge`), reading the process-wide mood-color cache
/// (which the CLI's fetch path fills synchronously beforehand).
pub fn format_today_simple(entries: &[TodayEntry], config: &Config) -> String {
    use crossterm::style::{Color as CtColor, Stylize};
    use ratatui::backend::IntoCrossterm;

    let mut output = String::new();
    for entry in entries {
        let ts = entry.time_label.clone();

        // Journal entries (empty mood label) carry the body as the label.
        let (label, detail) = if entry.kind == EntryKind::Journal {
            (entry.body.to_string(), String::new())
        } else {
            (entry.label.clone(), entry.body.clone())
        };

        // Same badge as the TUI: marker glyph colored with the derived dot
        // color. Reset-colored badges (e.g. 0% tasks) stay plain; entries
        // without a badge (journal entries, no journal_badge configured)
        // render an empty cell.
        let (entry_badge, rat_color) = entry.badge(config);
        let color = rat_color.into_crossterm();
        let badge = match entry_badge {
            None => String::new(),
            Some(c) if color == CtColor::Reset => c.to_string(),
            Some(c) => c.to_string().with(color).to_string(),
        };

        output.push_str(&format!("{}\t{}\t{}\t{}\n", ts, badge, label, detail));
    }
    output
}
