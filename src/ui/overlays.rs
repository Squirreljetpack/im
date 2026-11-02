use matchmaker::{
    action::Action as MMAction,
    config::OverlayLayoutSettings,
    render::MMState,
    ui::{Frame, Overlay, OverlayEffect, SizeHint, utils},
};
use ratatui::{
    layout::{Alignment, Position, Rect},
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};
use std::sync::{Arc, Mutex};

use crate::ui::action::ImAction as AppAction;
use crate::ui::styles;

/// Fired once with the chosen option index.
pub type ConfirmCallback = Box<dyn FnOnce(usize) + Send + Sync>;
/// Fired once with the accepted input.
pub type SubmitCallback = Box<dyn FnOnce(String) + Send + Sync>;
/// Decides which chars may be typed.
pub type CharFilter = Box<dyn Fn(char) -> bool + Send + Sync>;
/// Validates the input on submit.
pub type InputValidator = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Data for a Yes/No-style confirm overlay.
pub struct ConfirmPrompt {
    /// Prompt lines shown above the buttons.
    pub prompt: Vec<Line<'static>>,
    /// `(label, hotkey index)`: the hotkey is the char of `label` at
    /// `hotkey`; typing it (case-insensitive) picks that option.
    pub options: Vec<(&'static str, usize)>,
    /// Selected option (0-based).
    pub cursor: usize,
    /// Fired once with the chosen option index when the overlay accepts.
    pub on_accept: Option<ConfirmCallback>,
}

impl Default for ConfirmPrompt {
    fn default() -> Self {
        Self {
            prompt: vec![Line::from("Confirm?")],
            options: vec![("Yes", 0), ("No", 0)],
            cursor: 1, // Default to "No"
            on_accept: None,
        }
    }
}

/// A Yes/No confirm dialog: navigate with Left/Right, accept with Enter,
/// pick an option directly with its hotkey char, cancel with Esc/q.
pub struct ConfirmOverlay<T: matchmaker::SSS> {
    pub prompt: ConfirmPrompt,
    pub area: Rect,
    _marker: std::marker::PhantomData<T>,
}

impl<T: matchmaker::SSS> Default for ConfirmOverlay<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: matchmaker::SSS> ConfirmOverlay<T> {
    pub fn new() -> Self {
        Self {
            prompt: ConfirmPrompt::default(),
            area: Rect::default(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn set_prompt(&mut self, prompt: ConfirmPrompt) {
        self.prompt = prompt;
    }

    fn accept(&mut self) -> OverlayEffect {
        if let Some(cb) = self.prompt.on_accept.take() {
            cb(self.prompt.cursor);
        }
        OverlayEffect::Disable
    }
}

impl<T: matchmaker::SSS> Overlay<AppAction, T, ()> for ConfirmOverlay<T> {
    fn handle_input(&mut self, c: char, _state: &mut MMState<'_, T, ()>) -> OverlayEffect {
        for (i, (name, hotkey)) in self.prompt.options.iter().enumerate() {
            if let Some(hc) = name.chars().nth(*hotkey)
                && hc.eq_ignore_ascii_case(&c)
            {
                self.prompt.cursor = i;
                return self.accept();
            }
        }
        OverlayEffect::None
    }

    fn handle_action(
        &mut self,
        action: &MMAction<AppAction>,
        _state: &mut MMState<'_, T, ()>,
    ) -> OverlayEffect {
        match action {
            MMAction::BackwardChar => {
                let n = self.prompt.options.len().max(1);
                self.prompt.cursor = if self.prompt.cursor == 0 {
                    n - 1
                } else {
                    self.prompt.cursor - 1
                };
            }
            MMAction::ForwardChar => {
                let n = self.prompt.options.len().max(1);
                self.prompt.cursor = (self.prompt.cursor + 1) % n;
            }
            MMAction::Accept | MMAction::Custom(AppAction::Update) => return self.accept(),
            MMAction::Quit(_) | MMAction::Custom(AppAction::Quit) => return OverlayEffect::Disable,
            _ => {}
        }
        OverlayEffect::None
    }

    fn area(&mut self, ui_area: &Rect, layout: &OverlayLayoutSettings) {
        let width = (ui_area.width / 2).clamp(40, ui_area.width.saturating_sub(2));
        self.area =
            utils::default_area([SizeHint::from(width), SizeHint::from(7)], layout, ui_area);
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = self.area;
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(styles::OVERLAY_BORDER)
            .padding(Padding::new(2, 2, 0, 1));
        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        let lines = self.prompt.prompt.len() as u16;
        let prompt_area = Rect::new(inner_area.x, inner_area.y, inner_area.width, lines);
        frame.render_widget(
            Paragraph::new(Text::from(self.prompt.prompt.clone())),
            prompt_area,
        );

        let mut spans = Vec::new();
        for (i, (label, _)) in self.prompt.options.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let selected = i == self.prompt.cursor;
            let style = if selected {
                styles::CONFIRM_OPTION_SELECTED
            } else {
                styles::CONFIRM_OPTION
            };
            for (ci, ch) in label.chars().enumerate() {
                let s = if ci == 0 {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                spans.push(Span::styled(ch.to_string(), s));
            }
        }

        let buttons_area = Rect::new(inner_area.x, inner_area.y + lines + 1, inner_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            buttons_area,
        );
    }
}

/// Data for a text-input overlay.
pub struct InputPrompt {
    /// Border title.
    pub title: String,
    /// Optional note rendered above the input row, e.g. the value a replace
    /// log will overwrite.
    pub hint: Option<Line<'static>>,
    /// Prompt label rendered before the input, flush against the border.
    pub label: String,
    /// Shown in place of the input while it is empty (e.g. the count
    /// modal's "1"); the cursor still parks at the real input length.
    pub placeholder: Option<String>,
    /// Current input.
    pub input: String,
    /// Validation error shown under the input while `Some`.
    pub error: Option<String>,
    /// Chars that may be typed into the input; rejected chars are ignored.
    pub allowed: Option<CharFilter>,
    /// Validates the input on submit. `Err` keeps the overlay open and
    /// shows the message as the error.
    pub validator: Option<InputValidator>,
    /// Fired once with the accepted input when the overlay submits.
    pub on_submit: Option<SubmitCallback>,
}

impl Default for InputPrompt {
    fn default() -> Self {
        Self {
            title: "Input:".to_string(),
            hint: None,
            label: "> ".to_string(),
            placeholder: None,
            input: String::new(),
            error: None,
            allowed: None,
            validator: None,
            on_submit: None,
        }
    }
}

/// Minimum total width of an input overlay (borders included), so the
/// border title is legible before anything is typed.
const MIN_INPUT_WIDTH: u16 = 40;

/// An input overlay may grow to 90% of the host area to fit its title and
/// error text; beyond that the error wraps.
const MAX_INPUT_WIDTH_PERCENT: u32 = 90;

/// A single-line text input: type to append, Backspace/Delete pops,
/// Ctrl+W removes the trailing word, Ctrl+U clears, Enter submits,
/// Esc cancels.
pub struct InputOverlay<T: matchmaker::SSS> {
    pub prompt: InputPrompt,
    pub area: Rect,
    /// Cached host area + layout; the content-sized box is recomputed from
    /// the current input in [`Self::draw`] so it tracks what is typed.
    ui_area: Rect,
    layout: OverlayLayoutSettings,
    _marker: std::marker::PhantomData<T>,
}

impl<T: matchmaker::SSS> Default for InputOverlay<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: matchmaker::SSS> InputOverlay<T> {
    pub fn new() -> Self {
        Self {
            prompt: InputPrompt::default(),
            area: Rect::default(),
            ui_area: Rect::default(),
            layout: OverlayLayoutSettings::default(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn set_prompt(&mut self, prompt: InputPrompt) {
        self.prompt = prompt;
    }

    fn shown(&self) -> &str {
        self.prompt
            .placeholder
            .as_ref()
            .filter(|_| self.prompt.input.is_empty())
            .map(String::as_str)
            .unwrap_or(&self.prompt.input)
    }

    /// Total width (borders included): enough for the label/input, the border
    /// title, and the current error, capped at 90% of the host area. When the
    /// cap is hit, `error_lines` wraps the error instead.
    fn width_for(&self, ui_area: &Rect) -> u16 {
        let input =
            (self.prompt.label.chars().count() + self.shown().chars().count() + 1 + 2) as u16;
        let title = self.prompt.title.chars().count().saturating_add(4) as u16;
        let hint = self.prompt.hint.as_ref().map_or(0, |line| {
            line.width().saturating_add(2).min(u16::MAX as usize) as u16
        });
        let error = self.prompt.error.as_ref().map_or(0, |err| {
            err.chars().count().saturating_add(5).min(u16::MAX as usize) as u16
        });
        let max = (u32::from(ui_area.width) * MAX_INPUT_WIDTH_PERCENT / 100) as u16;
        input
            .max(title)
            .max(hint)
            .max(error)
            .max(MIN_INPUT_WIDTH)
            .min(max)
            .max(1)
    }

    /// Number of lines the hint occupies at `inner_width` (0 without one).
    fn hint_lines(&self, inner_width: u16) -> u16 {
        let Some(hint) = &self.prompt.hint else {
            return 0;
        };
        Paragraph::new(hint.clone())
            .wrap(Wrap { trim: false })
            .line_count(inner_width.max(1))
            .min(usize::from(u16::MAX)) as u16
    }

    /// Number of lines the error occupies at `inner_width` (0 without one).
    fn error_lines(&self, inner_width: u16) -> u16 {
        let Some(err) = &self.prompt.error else {
            return 0;
        };
        let paragraph = Paragraph::new(format!(" ✗ {err}")).wrap(Wrap { trim: false });
        paragraph
            .line_count(inner_width.max(1))
            .min(usize::from(u16::MAX)) as u16
    }

    /// Total height: the wrapped hint, the input line, the wrapped error
    /// lines, and borders.
    fn height_for(&self, inner_width: u16) -> u16 {
        1 + self.hint_lines(inner_width) + self.error_lines(inner_width) + 2
    }

    fn submit(&mut self) -> OverlayEffect {
        let input = std::mem::take(&mut self.prompt.input);
        let err = self.prompt.validator.as_ref().and_then(|v| v(&input).err());
        if let Some(msg) = err {
            self.prompt.input = input;
            self.prompt.error = Some(msg);
            return OverlayEffect::None;
        }
        self.prompt.error = None;
        if let Some(cb) = self.prompt.on_submit.take() {
            cb(input);
        }
        OverlayEffect::Disable
    }
}

impl<T: matchmaker::SSS> Overlay<AppAction, T, ()> for InputOverlay<T> {
    fn handle_input(&mut self, c: char, _state: &mut MMState<'_, T, ()>) -> OverlayEffect {
        if c == '\n' || c == '\r' {
            return self.submit();
        }
        if self.prompt.allowed.as_ref().is_none_or(|f| f(c)) {
            self.prompt.input.push(c);
            self.prompt.error = None;
        }
        OverlayEffect::None
    }

    fn handle_action(
        &mut self,
        action: &MMAction<AppAction>,
        _state: &mut MMState<'_, T, ()>,
    ) -> OverlayEffect {
        match action {
            // Backspace (builtin) and the Delete key (custom binding) pop
            // the last char, as in the pre-matchmaker modals.
            MMAction::DeleteChar | MMAction::Custom(AppAction::Delete) => {
                self.prompt.input.pop();
                self.prompt.error = None;
            }
            MMAction::DeleteWord | MMAction::BackwardWord => {
                pop_word(&mut self.prompt.input);
                self.prompt.error = None;
            }
            MMAction::ClearQuery => {
                self.prompt.input.clear();
            }
            MMAction::Accept | MMAction::Custom(AppAction::Update) => return self.submit(),
            MMAction::Quit(_) | MMAction::Custom(AppAction::Quit) => return OverlayEffect::Disable,
            _ => {}
        }
        OverlayEffect::None
    }

    fn area(&mut self, ui_area: &Rect, layout: &OverlayLayoutSettings) {
        self.ui_area = *ui_area;
        self.layout = layout.clone();
        let width = self.width_for(ui_area);
        let height = self.height_for(width.saturating_sub(2));
        self.area = utils::default_area(
            [SizeHint::from(width), SizeHint::from(height)],
            layout,
            ui_area,
        );
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        // The box tracks the input: re-center and re-size every frame.
        let width = self.width_for(&self.ui_area).min(self.ui_area.width);
        let height = self
            .height_for(width.saturating_sub(2))
            .min(self.ui_area.height);
        self.area = utils::default_area(
            [SizeHint::from(width), SizeHint::from(height)],
            &self.layout,
            &self.ui_area,
        );
        let area = self.area;
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(styles::OVERLAY_BORDER)
            .title(self.prompt.title.clone());
        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        let hint_height = self.hint_lines(inner_area.width);
        if let Some(ref hint) = self.prompt.hint {
            frame.render_widget(
                Paragraph::new(hint.clone()).wrap(Wrap { trim: false }),
                Rect::new(inner_area.x, inner_area.y, inner_area.width, hint_height),
            );
        }
        let input_y = inner_area.y + hint_height;

        let placeholder = self.prompt.input.is_empty() && self.prompt.placeholder.is_some();
        let shown = self.shown().to_string();
        let shown_style = if placeholder {
            styles::PLACEHOLDER
        } else {
            styles::INPUT_TEXT
        };
        let input_line = Line::from(vec![
            Span::styled(self.prompt.label.clone(), styles::INPUT_LABEL),
            Span::styled(shown, shown_style),
        ]);
        frame.render_widget(
            Paragraph::new(input_line),
            Rect::new(inner_area.x, input_y, inner_area.width, 1),
        );

        let cursor_x = area.x + 1 + self.prompt.label.len() as u16 + self.prompt.input.len() as u16;
        frame.set_cursor_position(Position::new(
            cursor_x.min(area.right().saturating_sub(1)),
            input_y,
        ));

        if let Some(ref err) = self.prompt.error {
            let err_line = Line::from(Span::styled(format!(" ✗ {err}"), styles::ERROR));
            frame.render_widget(
                Paragraph::new(err_line).wrap(Wrap { trim: false }),
                Rect::new(
                    inner_area.x,
                    input_y + 1,
                    inner_area.width,
                    self.error_lines(inner_area.width),
                ),
            );
        }
    }
}

/// Remove the trailing word (whitespace run, then non-whitespace run).
fn pop_word(input: &mut String) {
    let trimmed = input.trim_end_matches(char::is_whitespace).len();
    input.truncate(trimmed);
    let cut = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    input.truncate(cut);
}

/// Owns an overlay behind `Arc<Mutex<_>>` so handler tasks (other threads)
/// can stage prompts while the render thread drives the overlay.
pub struct SharedOverlay<O> {
    inner: Arc<Mutex<O>>,
}

impl<O> SharedOverlay<O> {
    pub fn new(inner: Arc<Mutex<O>>) -> Self {
        Self { inner }
    }

    pub fn handle(&self) -> &Arc<Mutex<O>> {
        &self.inner
    }
}

impl<A: matchmaker::action::ActionExt, T: matchmaker::SSS, D: 'static, O: Overlay<A, T, D>>
    Overlay<A, T, D> for SharedOverlay<O>
{
    fn on_enable(&mut self, area: &Rect, state: &mut MMState<'_, T, D>) {
        self.inner.lock().unwrap().on_enable(area, state)
    }

    fn on_disable(&mut self) {
        self.inner.lock().unwrap().on_disable()
    }

    fn handle_input(&mut self, c: char, state: &mut MMState<'_, T, D>) -> OverlayEffect {
        self.inner.lock().unwrap().handle_input(c, state)
    }

    fn handle_action(
        &mut self,
        action: &MMAction<A>,
        state: &mut MMState<'_, T, D>,
    ) -> OverlayEffect {
        self.inner.lock().unwrap().handle_action(action, state)
    }

    fn area(&mut self, ui_area: &Rect, layout: &OverlayLayoutSettings) {
        self.inner.lock().unwrap().area(ui_area, layout)
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        self.inner.lock().unwrap().draw(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_width_has_a_floor_and_a_90_percent_ceiling() {
        let mut overlay = InputOverlay::<String>::new();
        overlay.prompt.title = "Log mile (10-31 23:00 - 11-1 23:00)".into();
        let host = Rect::new(0, 0, 80, 40);
        assert!(overlay.width_for(&host) >= MIN_INPUT_WIDTH);

        overlay.prompt.error = Some("Cannot parse a very long value: ".repeat(8));
        assert_eq!(overlay.width_for(&host), 72);
        assert!(overlay.error_lines(70) > 1);
    }

    #[test]
    fn input_width_never_exceeds_a_narrow_host() {
        let overlay = InputOverlay::<String>::new();
        let host = Rect::new(0, 0, 30, 20);
        assert!(overlay.width_for(&host) <= 27);
    }

    #[test]
    fn input_hint_adds_a_row_and_wraps_when_capped() {
        let mut overlay = InputOverlay::<String>::new();
        let host = Rect::new(0, 0, 80, 40);
        assert_eq!(overlay.height_for(38), 3);

        overlay.prompt.hint = Some(Line::from("replaces: 20m"));
        assert_eq!(overlay.hint_lines(38), 1);
        assert_eq!(overlay.height_for(38), 4);

        overlay.prompt.hint = Some(Line::from("replaces: ".to_string() + &"x".repeat(200)));
        assert_eq!(overlay.width_for(&host), 72);
        assert!(overlay.hint_lines(70) > 1);
    }
}
