//! Interactive prompts (cliclack) for the task, entry, and confirm flows.
//!
//! Every interactive input/confirm in the CLI and TUI lives under this module.
//! Prompts are pure UI: no SQL, no file I/O. Flow-level banner and feedback
//! output lives in `crate::output`; database access lives in `crate::db`;
//! the flows that stitch prompts and writes together live in
//! `crate::commands`.

mod confirm;
mod create;
mod task;

pub use confirm::*;
pub use create::*;
pub use task::*;

/// Gray for text-box placeholders, matching the ratatui views'
/// `Color::DarkGray`. cliclack's default dims placeholder text, which most
/// terminals render as barely different from normal text; an explicit bright
/// black (ANSI 8) reads as muted everywhere.
fn muted() -> console::Style {
    console::Style::new().black().bright()
}

/// Renders input placeholders in gray and leaves hints, option labels and the
/// confirm separator on cliclack's default dim.
struct MutedPlaceholders;

impl cliclack::Theme for MutedPlaceholders {
    fn placeholder_style(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Cancel => console::Style::new().hidden(),
            _ => muted(),
        }
    }

    /// The default feeds option labels and value hints through
    /// `placeholder_style`, so every entry of a select list would be gray.
    /// Both keep the default rendering here.
    fn radio_item(
        &self,
        state: &cliclack::ThemeState,
        selected: bool,
        label: &str,
        hint: &str,
    ) -> String {
        match state {
            cliclack::ThemeState::Cancel | cliclack::ThemeState::Submit if !selected => {
                return String::new();
            }
            _ => {}
        }

        let radio = self.radio_symbol(state, selected);
        let space1 = if radio.is_empty() { "" } else { " " };
        let space2 = if label.is_empty() { "" } else { " " };

        let label = if selected {
            self.input_style(state)
        } else {
            console::Style::new().dim()
        }
        .apply_to(label);

        let hint = match state {
            cliclack::ThemeState::Active | cliclack::ThemeState::Error(_)
                if !hint.is_empty() && selected =>
            {
                console::Style::new()
                    .dim()
                    .apply_to(format!("({})", hint))
                    .to_string()
            }
            _ => String::new(),
        };

        format!("{radio}{space1}{label}{space2}{hint}")
    }

    /// The default dims the ` / ` separator between the two answers via
    /// `placeholder_style`; keep that dim explicitly.
    fn format_confirm(&self, state: &cliclack::ThemeState, confirm: bool) -> String {
        let yes = self.radio_item(state, confirm, "Yes", "");
        let no = self.radio_item(state, !confirm, "No", "");
        let divider = match state {
            cliclack::ThemeState::Active => console::Style::new().dim().apply_to(" / ").to_string(),
            _ => String::new(),
        };
        format!(
            "{bar}  {yes}{divider}{no}\n",
            bar = self.bar_color(state).apply_to("\u{2502}")
        )
    }
}

/// Installs the process-wide prompt theme. Called once at startup so every
/// cliclack prompt inherits the gray input placeholder.
pub fn install_theme() {
    cliclack::set_theme(MutedPlaceholders);
}
