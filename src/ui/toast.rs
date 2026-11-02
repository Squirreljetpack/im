//! Footer toasts: a one-line status message rendered through matchmaker's
//! footer row (the same mechanism `fist` uses). A toast stays until another
//! toast replaces it, a new execute flow starts, or the cursor moves.

use matchmaker::{action::Action as MMAction, render::MMState};
use ratatui::{
    style::Style,
    text::{Line, Span, Text},
};

use crate::ui::styles;

/// Severity of a toast, deciding its glyph and color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warning,
    Error,
}

fn glyph(level: Level) -> (&'static str, Style) {
    match level {
        Level::Info => ("•", styles::TOAST_INFO),
        Level::Success => ("✓", styles::TOAST_SUCCESS),
        Level::Warning => ("!", styles::TOAST_WARNING),
        Level::Error => ("✗", styles::TOAST_ERROR),
    }
}

/// Replace the footer with `message`, prefixed by the level's glyph.
pub fn set<T: matchmaker::SSS, D>(
    state: &mut MMState<'_, T, D>,
    level: Level,
    message: impl Into<String>,
) {
    let (glyph, style) = glyph(level);
    let line = Line::from(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(message.into(), styles::TOAST_MESSAGE),
    ]);
    state.footer_ui.set(Text::from(line));
}

/// Hide the footer again.
pub fn clear<T: matchmaker::SSS, D>(state: &mut MMState<'_, T, D>) {
    state.footer_ui.clear(false);
}

/// Clear the toast when a result-cursor navigation action is about to run.
/// Registered as each view's action aliaser, so a toast survives the
/// programmatic repopulation that follows an execute flow and disappears on
/// the first user move.
pub fn clear_on_cursor_move<T: matchmaker::SSS, D>(
    state: &mut MMState<'_, T, D>,
    action: &MMAction<crate::ui::action::ImAction>,
) {
    if matches!(
        action,
        MMAction::Up(_)
            | MMAction::Down(_)
            | MMAction::Pos(_)
            | MMAction::HalfPageUp
            | MMAction::HalfPageDown
    ) {
        clear(state);
    }
}
