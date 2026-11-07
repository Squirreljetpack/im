use crossterm::style::{Color as CtColor, Stylize};
use matchmaker::config::StyleSetting;

/// The gap between a row's badge glyph and its text in the label column:
/// 4 spaces, kept in one place so both views pad identically.
pub const BADGE_GAP: &str = "    ";

pub fn mode_label(mode: crate::types::ViewMode) -> &'static str {
    match mode {
        crate::types::ViewMode::PendingTasks => "@ Pending Tasks",
        crate::types::ViewMode::DoneTasks => "@done Completed",
    }
}

pub fn ct_to_ratatui_color(c: crossterm::style::Color) -> ratatui::style::Color {
    match c {
        crossterm::style::Color::Reset => ratatui::style::Color::Reset,
        crossterm::style::Color::Black => ratatui::style::Color::Black,
        crossterm::style::Color::DarkRed => ratatui::style::Color::Red,
        crossterm::style::Color::DarkGreen => ratatui::style::Color::Green,
        crossterm::style::Color::DarkYellow => ratatui::style::Color::Yellow,
        crossterm::style::Color::DarkBlue => ratatui::style::Color::Blue,
        crossterm::style::Color::DarkMagenta => ratatui::style::Color::Magenta,
        crossterm::style::Color::DarkCyan => ratatui::style::Color::Cyan,
        crossterm::style::Color::Grey => ratatui::style::Color::Gray,
        crossterm::style::Color::DarkGrey => ratatui::style::Color::DarkGray,
        crossterm::style::Color::Red => ratatui::style::Color::LightRed,
        crossterm::style::Color::Green => ratatui::style::Color::LightGreen,
        crossterm::style::Color::Yellow => ratatui::style::Color::LightYellow,
        crossterm::style::Color::Blue => ratatui::style::Color::LightBlue,
        crossterm::style::Color::Magenta => ratatui::style::Color::LightMagenta,
        crossterm::style::Color::Cyan => ratatui::style::Color::LightCyan,
        crossterm::style::Color::White => ratatui::style::Color::White,
        crossterm::style::Color::Rgb { r, g, b } => ratatui::style::Color::Rgb(r, g, b),
        crossterm::style::Color::AnsiValue(v) => ratatui::style::Color::Indexed(v),
    }
}

/// Convert ratatui Color into crossterm Color.
pub fn ratatui_to_ct_color(c: ratatui::style::Color) -> CtColor {
    match c {
        ratatui::style::Color::Reset => CtColor::Reset,
        ratatui::style::Color::Black => CtColor::Black,
        ratatui::style::Color::Red => CtColor::DarkRed,
        ratatui::style::Color::Green => CtColor::DarkGreen,
        ratatui::style::Color::Yellow => CtColor::DarkYellow,
        ratatui::style::Color::Blue => CtColor::DarkBlue,
        ratatui::style::Color::Magenta => CtColor::DarkMagenta,
        ratatui::style::Color::Cyan => CtColor::DarkCyan,
        ratatui::style::Color::Gray => CtColor::Grey,
        ratatui::style::Color::DarkGray => CtColor::DarkGrey,
        ratatui::style::Color::LightRed => CtColor::Red,
        ratatui::style::Color::LightGreen => CtColor::Green,
        ratatui::style::Color::LightYellow => CtColor::Yellow,
        ratatui::style::Color::LightBlue => CtColor::Blue,
        ratatui::style::Color::LightMagenta => CtColor::Magenta,
        ratatui::style::Color::LightCyan => CtColor::Cyan,
        ratatui::style::Color::White => CtColor::White,
        ratatui::style::Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        ratatui::style::Color::Indexed(v) => CtColor::AnsiValue(v),
    }
}

/// Apply foreground, background, and font modifiers to a text segment.
pub fn apply_style_setting(text: &str, style: &StyleSetting) -> String {
    let mut content = text.to_string().stylize();
    if let Some(fg) = style.fg {
        content = content.with(ratatui_to_ct_color(fg));
    }
    if let Some(bg) = style.bg {
        content = content.on(ratatui_to_ct_color(bg));
    }
    let m = style.modifier;
    if m.contains(ratatui::style::Modifier::BOLD) {
        content = content.bold();
    }
    if m.contains(ratatui::style::Modifier::DIM) {
        content = content.dim();
    }
    if m.contains(ratatui::style::Modifier::ITALIC) {
        content = content.italic();
    }
    if m.contains(ratatui::style::Modifier::UNDERLINED) {
        content = content.underlined();
    }
    if m.contains(ratatui::style::Modifier::SLOW_BLINK)
        || m.contains(ratatui::style::Modifier::RAPID_BLINK)
    {
        content = content.slow_blink();
    }
    if m.contains(ratatui::style::Modifier::REVERSED) {
        content = content.reverse();
    }
    if m.contains(ratatui::style::Modifier::CROSSED_OUT) {
        content = content.crossed_out();
    }
    content.to_string()
}

/// Apply foreground and background colors to a glyph segment, ignoring font modifiers.
pub fn apply_color_only(text: &str, style: &StyleSetting) -> String {
    let mut content = text.to_string().stylize();
    if let Some(fg) = style.fg {
        content = content.with(ratatui_to_ct_color(fg));
    }
    if let Some(bg) = style.bg {
        content = content.on(ratatui_to_ct_color(bg));
    }
    content.to_string()
}

pub fn format_text(
    text: &str,
    style: &matchmaker::config::StyleSetting,
) -> ratatui::text::Text<'static> {
    let rat_style: ratatui::style::Style = (*style).into();
    ratatui::text::Text::from(ratatui::text::Line::from(ratatui::text::Span::styled(
        text.to_string(),
        rat_style,
    )))
}
