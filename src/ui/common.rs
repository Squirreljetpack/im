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
