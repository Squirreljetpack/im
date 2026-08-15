use matchmaker::{
    action::Action as MMAction,
    config::OverlayLayoutSettings,
    render::MMState,
    ui::{Frame, Overlay, OverlayEffect, SizeHint, utils},
};
use ratatui::{
    layout::{Alignment, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
};
use std::sync::{Arc, Mutex};

use crate::ui::action::ImAction as AppAction;

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
        self.area = utils::default_area(
            [SizeHint::from(width), SizeHint::from(7)],
            layout,
            ui_area,
        );
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = self.area;
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .padding(Padding::new(2, 2, 0, 1));
        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        let lines = self.prompt.prompt.len() as u16;
        let prompt_area = Rect::new(inner_area.x, inner_area.y, inner_area.width, lines);
        frame.render_widget(Paragraph::new(Text::from(self.prompt.prompt.clone())), prompt_area);

        let mut spans = Vec::new();
        for (i, (label, _)) in self.prompt.options.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let selected = i == self.prompt.cursor;
            let style = if selected {
                Style::default()
                    .bg(Color::White)
                    .fg(Color::Black)
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default().add_modifier(Modifier::ITALIC)
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

        let buttons_area = Rect::new(
            inner_area.x,
            inner_area.y + lines + 1,
            inner_area.width,
            1,
        );
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

    /// Content width: label + shown input + trailing space, plus borders.
    fn content_width(&self) -> u16 {
        let shown = self
            .prompt
            .placeholder
            .as_ref()
            .filter(|_| self.prompt.input.is_empty())
            .map(String::as_str)
            .unwrap_or(&self.prompt.input);
        (self.prompt.label.len() + shown.len() + 1) as u16 + 2
    }

    /// Content height: one line (two with an error), plus borders.
    fn content_height(&self) -> u16 {
        (if self.prompt.error.is_some() { 2 } else { 1 }) + 2
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
        let width = self.content_width();
        let height = self.content_height();
        self.area = utils::default_area(
            [SizeHint::from(width), SizeHint::from(height)],
            layout,
            ui_area,
        );
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        // The box tracks the input: re-center and re-size every frame.
        self.area = utils::default_area(
            [
                SizeHint::from(self.content_width().min(self.ui_area.width)),
                SizeHint::from(self.content_height().min(self.ui_area.height)),
            ],
            &self.layout,
            &self.ui_area,
        );
        let area = self.area;
        frame.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(self.prompt.title.clone());
        let inner_area = block.inner(area);
        frame.render_widget(block, area);

        let shown = self
            .prompt
            .placeholder
            .as_ref()
            .filter(|_| self.prompt.input.is_empty())
            .map(|s| s.as_str())
            .unwrap_or(&self.prompt.input);
        let input_line = Line::from(vec![
            Span::styled(self.prompt.label.clone(), Style::default().fg(Color::Yellow)),
            Span::styled(shown.to_string(), Style::default().fg(Color::White)),
        ]);
        frame.render_widget(
            Paragraph::new(input_line),
            Rect::new(inner_area.x, inner_area.y, inner_area.width, 1),
        );

        let cursor_x = area.x + 1 + self.prompt.label.len() as u16 + self.prompt.input.len() as u16;
        frame.set_cursor_position(Position::new(
            cursor_x.min(area.right().saturating_sub(1)),
            area.y + 1,
        ));

        if let Some(ref err) = self.prompt.error {
            let err_line = Line::from(Span::styled(
                format!(" ✗ {err}"),
                Style::default().fg(Color::LightRed),
            ));
            frame.render_widget(
                Paragraph::new(err_line),
                Rect::new(inner_area.x, inner_area.y + 1, inner_area.width, 1),
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
