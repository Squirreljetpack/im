//! Footer toasts: a one-line status message rendered through matchmaker's
//! footer row (the same mechanism `fist` uses). A toast stays until another
//! toast replaces it or a new execute flow starts.

use matchmaker::render::MMState;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

/// Severity of a toast, deciding its glyph and color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

fn glyph(level: Level) -> (&'static str, Color) {
    match level {
        Level::Info => ("•", Color::Cyan),
        Level::Success => ("✓", Color::Green),
        Level::Warning => ("!", Color::Yellow),
        Level::Error => ("✗", Color::Red),
    }
}

/// Replace the footer with `message`, prefixed by the level's glyph.
pub fn set<T: matchmaker::SSS, D>(
    state: &mut MMState<'_, T, D>,
    level: Level,
    message: impl Into<String>,
) {
    let (glyph, color) = glyph(level);
    let line = Line::from(vec![
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(message.into(), Style::default().fg(Color::White)),
    ]);
    state.footer_ui.set(Text::from(line));
}

/// Hide the footer again.
pub fn clear<T: matchmaker::SSS, D>(state: &mut MMState<'_, T, D>) {
    state.footer_ui.clear(false);
}
