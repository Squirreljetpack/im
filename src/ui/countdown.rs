use std::time::Duration;
pub use super::common::{apply_color_only, apply_style_setting, ratatui_to_ct_color};
use crossterm::style::{Color as CtColor, Stylize};
use matchmaker::config::StyleSetting;
use serde::{Deserialize, Serialize};

/// Display style for active session countdown timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CountdownStyle {
    /// Digital countdown timer, e.g. "25:00".
    Timer,
    /// Progress bar with remaining time, e.g. "████████░░░░░░░░  14:32" (default).
    #[default]
    Bar,
    /// Stepped dot indicators with remaining time, e.g. "●●●●○○○○  14:32".
    Dots,
    /// Completion percentage with remaining time, e.g. " 45%  14:32".
    Percent,
    /// Minimal elapsed/remaining duration format, e.g. "14m 32s".
    Minimal,
}

impl std::fmt::Display for CountdownStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timer => write!(f, "timer"),
            Self::Bar => write!(f, "bar"),
            Self::Dots => write!(f, "dots"),
            Self::Percent => write!(f, "percent"),
            Self::Minimal => write!(f, "minimal"),
        }
    }
}

impl CountdownStyle {
    /// Render the current countdown state into a formatted terminal string.
    pub fn render(
        &self,
        total: Duration,
        elapsed: Duration,
        style: &StyleSetting,
    ) -> String {
        let remaining = total.saturating_sub(elapsed);
        let remaining_secs = remaining.as_secs();
        let formatted_time = crate::date::format_countdown(remaining_secs as i64);

        let total_secs_f = total.as_secs_f64().max(0.001);
        let elapsed_secs_f = elapsed.as_secs_f64();
        let fraction = (elapsed_secs_f / total_secs_f).clamp(0.0, 1.0);

        match self {
            CountdownStyle::Timer => apply_style_setting(&formatted_time, style),
            CountdownStyle::Bar => {
                const BAR_WIDTH: usize = 20;
                let filled_len = (fraction * BAR_WIDTH as f64).round() as usize;
                let empty_len = BAR_WIDTH.saturating_sub(filled_len);

                let filled_str = "█".repeat(filled_len);
                let empty_str = "░".repeat(empty_len).with(CtColor::DarkGrey).to_string();

                // For bar glyphs, text modifiers (bold, italic) do not apply and are ignored;
                // colors (fg, bg) are preserved.
                let colored_bar = apply_color_only(&filled_str, style);
                let time_str = apply_style_setting(&formatted_time, style);

                format!("{colored_bar}{empty_str}  {time_str}")
            }
            CountdownStyle::Dots => {
                const DOT_COUNT: usize = 10;
                let filled_count = (fraction * DOT_COUNT as f64).round() as usize;
                let empty_count = DOT_COUNT.saturating_sub(filled_count);

                let filled_str = "●".repeat(filled_count);
                let empty_str = "○".repeat(empty_count).with(CtColor::DarkGrey).to_string();

                let colored_dots = apply_color_only(&filled_str, style);
                let time_str = apply_style_setting(&formatted_time, style);

                format!("{colored_dots}{empty_str}  {time_str}")
            }
            CountdownStyle::Percent => {
                let pct = (fraction * 100.0).floor() as u32;
                let pct_text = format!("{pct:>3}%");
                let styled_pct = apply_style_setting(&pct_text, style);
                let time_str = apply_style_setting(&formatted_time, style);
                format!("{styled_pct}  {time_str}")
            }
            CountdownStyle::Minimal => {
                let minimal_text = format_minimal_duration(remaining_secs);
                apply_style_setting(&minimal_text, style)
            }
        }
    }
}

fn format_minimal_duration(secs: u64) -> String {
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{hours}h {mins}m")
    } else if secs >= 60 {
        let mins = secs / 60;
        let s = secs % 60;
        format!("{mins}m {s}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_countdown_style_display() {
        assert_eq!(CountdownStyle::Timer.to_string(), "timer");
        assert_eq!(CountdownStyle::Bar.to_string(), "bar");
        assert_eq!(CountdownStyle::Dots.to_string(), "dots");
        assert_eq!(CountdownStyle::Percent.to_string(), "percent");
        assert_eq!(CountdownStyle::Minimal.to_string(), "minimal");
    }

    #[test]
    fn test_render_timer_uncolored() {
        let total = Duration::from_secs(1500);
        let elapsed = Duration::from_secs(0);
        let default_style = StyleSetting::default();
        assert_eq!(
            CountdownStyle::Timer.render(total, elapsed, &default_style),
            "25:00"
        );
    }

    #[test]
    fn test_render_bar() {
        let total = Duration::from_secs(100);
        let elapsed = Duration::from_secs(50);
        let default_style = StyleSetting::default();
        let rendered = CountdownStyle::Bar.render(total, elapsed, &default_style);
        assert!(rendered.contains("00:50"));
        assert!(rendered.contains("██████████"));
    }

    #[test]
    fn test_render_dots() {
        let total = Duration::from_secs(100);
        let elapsed = Duration::from_secs(40);
        let default_style = StyleSetting::default();
        let rendered = CountdownStyle::Dots.render(total, elapsed, &default_style);
        assert!(rendered.contains("01:00"));
        assert!(rendered.contains("●●●●"));
    }

    #[test]
    fn test_render_percent() {
        let total = Duration::from_secs(100);
        let elapsed = Duration::from_secs(75);
        let default_style = StyleSetting::default();
        let rendered = CountdownStyle::Percent.render(total, elapsed, &default_style);
        assert!(rendered.contains("75%"));
        assert!(rendered.contains("00:25"));
    }

    #[test]
    fn test_render_minimal() {
        let total = Duration::from_secs(3665);
        let elapsed = Duration::from_secs(0);
        let default_style = StyleSetting::default();
        assert_eq!(
            CountdownStyle::Minimal.render(total, elapsed, &default_style),
            "1h 1m"
        );

        let total = Duration::from_secs(90);
        let elapsed = Duration::from_secs(0);
        assert_eq!(
            CountdownStyle::Minimal.render(total, elapsed, &default_style),
            "1m 30s"
        );

        let total = Duration::from_secs(45);
        let elapsed = Duration::from_secs(0);
        assert_eq!(
            CountdownStyle::Minimal.render(total, elapsed, &default_style),
            "45s"
        );
    }
}
