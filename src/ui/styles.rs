//! Shared styles for im's TUI views. Keep colors and modifiers here so
//! prompts, overlays and toasts agree on the visual language.

use ratatui::style::{Color, Modifier, Style};

/// Modal overlay border.
pub const OVERLAY_BORDER: Style = Style::new().fg(Color::White);

/// Header/emphasis text (e.g. preview titles).
pub const HEADER: Style = Style::new().add_modifier(Modifier::BOLD);

/// Label preceding an input or a preview field (e.g. `Value: `).
pub const INPUT_LABEL: Style = Style::new().fg(Color::Yellow);

/// Typed input text.
pub const INPUT_TEXT: Style = Style::new().fg(Color::White);

/// Placeholder shown while an input is empty.
pub const PLACEHOLDER: Style = Style::new().fg(Color::DarkGray);

/// Validation error line.
pub const ERROR: Style = Style::new().fg(Color::LightRed);

/// Secondary note, field rule, or muted hint.
pub const HINT: Style = Style::new().fg(Color::DarkGray);

/// Emphasized label inside a hint (e.g. `replaces:`).
pub const HINT_EMPHASIS: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::ITALIC);

/// Value following a hint label.
pub const HINT_VALUE: Style = Style::new().fg(Color::White);

/// Unselected confirm option.
pub const CONFIRM_OPTION: Style = Style::new().add_modifier(Modifier::ITALIC);

/// Selected confirm option; its first char is additionally bold in the
/// overlay.
pub const CONFIRM_OPTION_SELECTED: Style = Style::new()
    .bg(Color::White)
    .fg(Color::Black)
    .add_modifier(Modifier::ITALIC);

/// Toast body text.
pub const TOAST_MESSAGE: Style = Style::new().fg(Color::White);

/// Toast glyphs by level.
pub const TOAST_INFO: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
pub const TOAST_SUCCESS: Style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
pub const TOAST_WARNING: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
pub const TOAST_ERROR: Style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
