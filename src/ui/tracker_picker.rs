//! `im -` — a matchmaker-backed tracker TUI: every configured tracker (a
//! single name column) with a live preview of its settings and a row of
//! colored cells. Enter logs an entry for the selected tracker: a confirm
//! for Null trackers, a kind-filtered value prompt otherwise.

use anyhow::Result;
use matchmaker::{
    MatchError, Matchmaker, PickOptions,
    action::Action as MMAction,
    binds::display_help,
    config::HelpDisplayConfig,
    event::{EventLoop, RenderSender},
    message::{Event, RenderCommand},
    nucleo::{Column, Injector, Text as NText, Worker},
    render::MMState,
};
use ratatui::{
    backend::FromCrossterm,
    style::{Color, Style},
    text::{Line, Span, Text},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::OnceCell;

use crate::config::{Config, TrackerKind, TrackerSetting};
use crate::date;
use crate::db::{EntryObject, TrackerObject, TrackerValue};
use crate::global::pool;
use crate::tracker::{enforce_strict, null_zone_contains, parse_tracker_value};
use crate::ui::action::ImAction;
use crate::ui::mm_config::get_mm_cfg;
use crate::ui::overlays::{
    ConfirmOverlay, ConfirmPrompt, InputOverlay, InputPrompt, SharedOverlay,
};
use crate::ui::previewer::Previewer;
use crate::ui::styles;
use crate::ui::toast::{self, Level};
use crate::ui::tracker_prompt;

/// One row in the picker: the tracker's config key and its resolved settings
/// (cloned so the worker owns a `'static` copy).
#[derive(Clone, serde::Serialize)]
pub struct TrackerRow {
    pub name: String,
    pub setting: TrackerSetting,
}

/// A `  field: value` line: field name (with colon) in yellow, value
/// uncolored — mirrors `previewer::field_line` so the tracker preview reads
/// like the other previews.
fn field_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {}: ", label), styles::INPUT_LABEL),
        Span::raw(value),
    ])
}

/// Lowercased label for a tracker kind, shown as the `kind:` field value.
fn kind_label(kind: TrackerKind) -> &'static str {
    match kind {
        TrackerKind::Text => "text",
        TrackerKind::Integer => "integer",
        TrackerKind::Float => "float",
        TrackerKind::Duration => "duration",
        TrackerKind::Null => "null",
    }
}

/// Render a low/high bound in the units that kind logs in: durations as
/// `6m 30s`, text as a character count, null replace offsets as durations,
/// and everything else as a plain number.
fn bound_label(setting: &TrackerSetting, value: f64) -> String {
    match setting.kind {
        TrackerKind::Duration => date::format_tracker_duration(value),
        TrackerKind::Text => {
            let n = value.round() as i64;
            if n == 1 {
                "1 char".to_string()
            } else {
                format!("{n} chars")
            }
        }
        TrackerKind::Null => match setting.interval {
            Some(iv) if !iv.cumulative => date::format_tracker_duration(value),
            _ => (value.round() as i64).to_string(),
        },
        _ => value.to_string(),
    }
}

/// Build the preview body for a tracker, styled like the other previews: a
/// yellow header carrying the tracker name over a rule, then `field: value`
/// lines with yellow field names. `kind` is a field; `bounds` becomes
/// `min`/`max`; `interval` expands into its subfields (anchor / span /
/// cumulative) when set; and the resolved palette ends the pane as a row of
/// colored cells (no hex codes).
fn build_tracker_preview(
    name: &str,
    setting: &TrackerSetting,
    named_months: bool,
) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Blank line so the header reads as a header.
    lines.push(Line::default());

    // Header: the tracker name, uncolored, over a rule.
    lines.push(Line::from(Span::styled(format!(" {name}"), styles::HEADER)));
    lines.push(Line::from(Span::styled(
        "─".repeat(name.len() + 2),
        styles::HINT,
    )));

    // Blank line, then the fields (kind leads them).
    lines.push(Line::default());

    lines.push(field_line("kind", kind_label(setting.kind).to_string()));

    // Bounds become `min` / `max` (a missing side shows `-`).
    if setting.low.is_some() || setting.high.is_some() {
        let low = setting
            .low
            .map(|v| bound_label(setting, v))
            .unwrap_or_else(|| "-".into());
        let high = setting
            .high
            .map(|v| bound_label(setting, v))
            .unwrap_or_else(|| "-".into());
        lines.push(field_line("min", low));
        lines.push(field_line("max", high));
    }

    if setting.strict {
        lines.push(field_line("strict", "yes".to_string()));
    }

    // Interval: subfields when set, otherwise a plain `none`.
    // Interval subfields, flattened to top-level fields (no `interval:` label).
    if let Some(iv) = &setting.interval {
        lines.push(field_line(
            "anchor",
            date::format_civil_datetime(iv.anchor, named_months),
        ));
        lines.push(field_line("span", date::format_span(&iv.span)));
        lines.push(field_line("cumulative", iv.cumulative.to_string()));
    }

    // Colors: a row of cells (one per palette entry), no hex codes.
    let palette = setting.colors();
    lines.push(Line::default());

    // The palette stores crossterm colors; convert to ratatui colors for
    // styling the cells.
    let rat_colors: Vec<Color> = palette.iter().map(|c| Color::from_crossterm(*c)).collect();
    let mut cells: Vec<Span<'static>> = Vec::with_capacity(rat_colors.len() * 2 + 1);
    // Align the strip with the two-space indent of the field lines.
    cells.push(Span::raw("  "));
    for color in &rat_colors {
        cells.push(Span::styled("  ", Style::default().bg(*color)));
        cells.push(Span::raw(" "));
    }
    lines.push(Line::from(cells));

    Text::from(lines)
}

/// What the tracker creation flow shares with the async inserters: the
/// render channel, the two prompt overlays (0 = confirm, 1 = input), and the
/// "an entry was logged" flag reported to the caller.
#[derive(Clone)]
struct TrackerCtx {
    tx: RenderSender<ImAction>,
    confirm: Arc<Mutex<ConfirmOverlay<TrackerRow>>>,
    input: Arc<Mutex<InputOverlay<TrackerRow>>>,
    logged: Arc<AtomicBool>,
}

impl TrackerCtx {
    fn action(&self, action: ImAction) {
        let _ = self
            .tx
            .send(RenderCommand::Action(MMAction::Custom(action)));
    }

    fn success(&self, message: impl Into<String>) {
        self.action(ImAction::Toast(message.into()));
    }

    fn error(&self, message: impl Into<String>) {
        self.action(ImAction::ToastError(message.into()));
    }
}

pub async fn run_trackers_app(config: &Config) -> Result<bool> {
    let (render_cfg, binds, tui_cfg, overlay_cfg) = get_mm_cfg();

    let rows: Vec<TrackerRow> = config
        .tracker
        .iter()
        .map(|(name, setting)| TrackerRow {
            name: name.clone(),
            setting: setting.clone(),
        })
        .collect();

    let columns = [Column::new("name", |item: &TrackerRow, _: &()| {
        NText::from(item.name.clone())
    })];

    let worker = Worker::new(columns, 0);

    // Only the named-months flag is needed from config to build the preview;
    // capture it (Copy) so the `'static` handlers need no global lookup.
    let named_months = config.preview.named_months;

    // Enter is the custom `ImAction::Update` (see `default_binds`): it opens
    // the entry prompt. Alt-enter still runs the builtin Accept.
    let mut mm = Matchmaker::new(
        worker,
        |state: &mut MMState<'_, TrackerRow, ()>| -> Vec<String> {
            state.map_selected_to_vec(|_, item| item.name.clone())
        },
    );

    let event_loop = EventLoop::with_binds(binds)
        .with_tick_rate(render_cfg.ui.tick_rate)
        .with_mouse_events(render_cfg.ui.mouse_events)
        .with_scroll_debounce(render_cfg.ui.mouse_scroll_debounce_ms);
    let binds_ptr = event_loop.get_binds_ptr();

    mm.config_render(render_cfg);
    mm.config_tui(tui_cfg);

    // The event loop's controller: a preview compute that lands after its
    // frame was drawn nudges the loop with a tick so the pane repaints.
    let event_tx = event_loop.controller();

    // The preview is built synchronously from the already-resolved palette,
    // so the color axes (and the tick nudge) go unused here.
    let axes = Arc::new(OnceCell::new());
    let previewer = Previewer::new(axes, event_tx);
    let preview_view = previewer.view();
    let help_previewer = previewer.clone();

    mm.register_event_handler(
        Event::CursorChange | Event::PreviewChange | Event::Synced,
        {
            let previewer = previewer.clone();
            move |state, _| {
                // Frames that explicitly set the preview (alt-h help) are
                // owned by the PreviewSet handler; don't clobber them.
                if state.contains(Event::PreviewSet) {
                    return;
                }
                state.preview_set_payload = None;
                if !state.preview_visible() {
                    return;
                }
                match state.current_raw().cloned() {
                    Some(row) => previewer.set_text(build_tracker_preview(
                        &row.name,
                        &row.setting,
                        named_months,
                    )),
                    None => previewer.stop(),
                }
            }
        },
    );

    // alt-h help: render the live bind map; closing help falls back to the
    // cursor-tracked preview.
    mm.register_event_handler(Event::PreviewSet, {
        let previewer = help_previewer;
        let binds_ptr = binds_ptr.clone();
        let help_config = HelpDisplayConfig::default();
        move |state, _| {
            if !state.preview_visible() {
                return;
            }
            match state.preview_set_payload() {
                Some(Err(m)) => {
                    let text = if m.lines.iter().all(|l| l.spans.is_empty()) {
                        display_help(&binds_ptr.load(), &help_config)
                    } else {
                        m
                    };
                    previewer.set_text(text);
                }
                None => match state.current_raw().cloned() {
                    Some(row) => previewer.set_text(build_tracker_preview(
                        &row.name,
                        &row.setting,
                        named_months,
                    )),
                    None => previewer.stop(),
                },
                Some(Ok(_)) => previewer.stop(),
            }
        }
    });

    let confirm_ov = Arc::new(Mutex::new(ConfirmOverlay::<TrackerRow>::new()));
    let input_ov = Arc::new(Mutex::new(InputOverlay::<TrackerRow>::new()));

    // Overlay order: 0 = confirm, 1 = input.
    let mut options = PickOptions::new()
        .event_loop(event_loop)
        .preview(preview_view)
        .overlay_config(overlay_cfg)
        .overlay(SharedOverlay::new(confirm_ov.clone()))
        .overlay(SharedOverlay::new(input_ov.clone()));

    let ctx = TrackerCtx {
        tx: options.render_tx(),
        confirm: confirm_ov.clone(),
        input: input_ov.clone(),
        logged: Arc::new(AtomicBool::new(false)),
    };

    // Prime the list once before the first frame.
    options = options.initializer(move |state| {
        state.worker_restart();
        let injector = state.injector();
        for row in &rows {
            let _ = injector.push(row.clone());
        }
    });

    let logged = ctx.logged.clone();
    options = options.ext_handler(move |action: ImAction, state| handler(action, state, &ctx));
    options = options.ext_aliaser(|action, state| {
        crate::ui::toast::clear_on_cursor_move(state, &action);
        action.into()
    });

    match mm.pick(options).await {
        Ok(_) | Err(MatchError::NoMatch) | Err(MatchError::Abort(_)) => {
            Ok(logged.load(Ordering::Relaxed))
        }
        Err(e) => Err(e.into()),
    }
}

fn handler(action: ImAction, state: &mut MMState<'_, TrackerRow, ()>, ctx: &TrackerCtx) {
    match action {
        ImAction::Update => {
            if let Some(row) = state.current_raw().cloned() {
                if let Some(message) = strict_null_error(&row.name, &row.setting) {
                    toast::set(state, Level::Error, message);
                    return;
                }
                entry_prompt(ctx.clone(), row);
            }
        }
        ImAction::Toast(message) => toast::set(state, Level::Success, message),
        ImAction::ToastError(message) => toast::set(state, Level::Error, message),
        _ => {}
    }
}

fn entry_prompt(ctx: TrackerCtx, row: TrackerRow) {
    match row.setting.kind {
        TrackerKind::Null => {
            if row.setting.interval.is_none() {
                ctx.error(format!(
                    "tracker '{}' requires an interval to log",
                    row.name
                ));
                return;
            }
            if let Some(message) = strict_null_error(&row.name, &row.setting) {
                ctx.error(message);
                return;
            }
            let prompt = null_confirm(row, ctx.clone());
            open_confirm(ctx, prompt);
        }
        _ => {
            let now = date::now();
            let slot = row
                .setting
                .interval
                .and_then(|iv| iv.slot_for(date::LogTime::local(now)));
            let replacing = row.setting.interval.is_some_and(|iv| !iv.cumulative);
            if replacing && slot.is_none() {
                ctx.error(format!(
                    "tracker '{}': could not compute the interval slot",
                    row.name
                ));
                return;
            }
            if replacing {
                let ctx2 = ctx.clone();
                let row2 = row.clone();
                tokio::spawn(async move {
                    let replaced = match slot {
                        Some(slot) => replaced_value(&row2, slot).await,
                        None => None,
                    };
                    let prompt = value_prompt(row2, ctx2.clone(), slot, replaced);
                    open_input(ctx2, prompt);
                });
            } else {
                let prompt = value_prompt(row, ctx.clone(), slot, None);
                open_input(ctx, prompt);
            }
        }
    }
}

/// Stage a confirm prompt and activate overlay 0.
fn open_confirm(ctx: TrackerCtx, prompt: ConfirmPrompt) {
    ctx.confirm.lock().unwrap().set_prompt(prompt);
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Overlay(0)));
}

/// Stage an input prompt and activate overlay 1.
fn open_input(ctx: TrackerCtx, prompt: InputPrompt) {
    ctx.input.lock().unwrap().set_prompt(prompt);
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Overlay(1)));
}

/// Null trackers take no value: Enter is a confirm to log one now.
fn null_confirm(row: TrackerRow, ctx: TrackerCtx) -> ConfirmPrompt {
    let lines = vec![Line::from(format!("Log '{}'?", row.name))];

    let name = row.name.clone();
    let setting = row.setting.clone();
    ConfirmPrompt {
        prompt: lines,
        options: vec![("Yes", 0), ("No", 0)],
        cursor: 0,
        on_accept: Some(Box::new(move |idx| {
            if idx == 0 {
                if let Some(message) = strict_null_error(&name, &setting) {
                    ctx.error(message);
                    return;
                }
                let name = name.clone();
                let setting = setting.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    insert_entry(&ctx, &name, &setting, None).await;
                });
            }
        })),
    }
}

/// Value-bearing trackers: a kind-filtered input, validated (form and the
/// strict gate) as it is typed. The title shows the interval slot being
/// logged; when a replace-mode slot already holds a value, it is shown above
/// the input.
fn value_prompt(
    row: TrackerRow,
    ctx: TrackerCtx,
    slot: Option<(i64, i64)>,
    replaced: Option<String>,
) -> InputPrompt {
    let name = row.name.clone();
    let setting = row.setting.clone();
    let kind = setting.kind;
    let validator_name = name.clone();
    let validator_setting = setting.clone();
    InputPrompt {
        title: log_title(&name, slot),
        hint: replaced.map(replaces_hint),
        label: "Value: ".to_string(),
        placeholder: (kind == TrackerKind::Duration).then(|| "6m 30s".to_string()),
        input: String::new(),
        error: None,
        allowed: Some(Box::new(tracker_prompt::chars(kind))),
        validator: Some(Box::new(move |s: &str| {
            tracker_prompt::validate(&validator_name, Some(&validator_setting), kind, s)
        })),
        on_submit: Some(Box::new(move |val| {
            let val = val.trim().to_string();
            let name = name.clone();
            let setting = setting.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                insert_entry(&ctx, &name, &setting, Some(&val)).await;
            });
        })),
    }
}

fn log_title(name: &str, slot: Option<(i64, i64)>) -> String {
    match slot {
        Some((start, end)) => format!(
            "Log '{name}' ({} - {})",
            date::format_datetime_short(start),
            date::format_datetime_short(end)
        ),
        None => format!("Log '{name}'"),
    }
}

/// The value currently in `slot` for a replace-mode tracker, formatted the
/// way that kind displays itself. `None` when the slot is empty.
async fn replaced_value(row: &TrackerRow, slot: (i64, i64)) -> Option<String> {
    let interval = row.setting.interval?;
    let (start, end) = slot;
    let entries = match crate::db::fetch_tracker_entries(
        &pool(),
        &row.name,
        start - crate::db::CIVIL_FETCH_MARGIN,
        end + crate::db::CIVIL_FETCH_MARGIN,
    )
    .await
    {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!("Failed to fetch the replaced tracker value: {e:#}");
            return None;
        }
    };
    let entry = entries
        .into_iter()
        .filter(|entry| interval.start_for(entry.time) == Some(start))
        .last()?;
    Some(format_replaced(row.setting.kind, &entry.score))
}

/// Render a stored tracker score in the tracker's own units.
fn format_replaced(kind: TrackerKind, raw: &str) -> String {
    let value = match kind {
        TrackerKind::Duration => raw
            .parse::<f64>()
            .map(date::format_tracker_duration)
            .unwrap_or_else(|_| raw.to_string()),
        TrackerKind::Float => raw
            .parse::<f64>()
            .map(|value| value.to_string())
            .unwrap_or_else(|_| raw.to_string()),
        TrackerKind::Integer => raw
            .parse::<i64>()
            .map(|value| value.to_string())
            .unwrap_or_else(|_| raw.to_string()),
        TrackerKind::Text | TrackerKind::Null => raw.to_string(),
    };
    value.replace('\n', " ").replace('\r', " ")
}

/// The line above the value input: the entry the replace-mode log overwrites.
fn replaces_hint(value: String) -> Line<'static> {
    let value = truncate_hint(value);
    let value = if value.is_empty() {
        "(empty)".to_string()
    } else {
        value
    };
    Line::from(vec![
        Span::styled("replaces:", styles::HINT_EMPHASIS),
        Span::styled(format!(" {value}"), styles::HINT_VALUE),
    ])
}

/// Keep an unusually long text value from turning the prompt into a wall.
fn truncate_hint(value: String) -> String {
    const MAX_CHARS: usize = 120;
    if value.chars().count() <= MAX_CHARS {
        return value;
    }
    let mut shortened: String = value.chars().take(MAX_CHARS).collect();
    shortened.push('…');
    shortened
}

/// The error for a strict null tracker whose strict when-zone excludes now.
fn strict_null_error(name: &str, setting: &TrackerSetting) -> Option<String> {
    (setting.kind == TrackerKind::Null
        && setting.interval.is_some()
        && setting.strict
        && !null_zone_contains(setting, date::now()))
    .then(|| {
        format!(
            "tracker '{name}': cannot log now — {}",
            null_strict_hint(setting)
        )
    })
}

fn null_strict_hint(setting: &TrackerSetting) -> String {
    let (Some(low), Some(high)) = (setting.low, setting.high) else {
        return "strict".to_string();
    };
    let lo = date::format_tracker_duration(low);
    let hi = date::format_tracker_duration(high);
    let zone = if low == high {
        format!("exactly at {lo}")
    } else if low < high {
        format!("{lo}–{hi}")
    } else {
        format!("{lo}–{hi} (wrap)")
    };
    let status = if null_zone_contains(setting, date::now()) {
        "inside"
    } else {
        "outside"
    };
    format!("strict {zone} · now {status}")
}

/// Parse, strict-gate, and insert one tracker entry — the same pipeline as
/// the CLI (`parse_tracker_value` → `enforce_strict` → `db::create_entry`).
/// Null replace markers store their seconds from the slot start; cumulative
/// null markers store 0. Replace-mode interval trackers drop the slot's
/// previous entry inside `create_entry`.
async fn insert_entry(ctx: &TrackerCtx, name: &str, setting: &TrackerSetting, raw: Option<&str>) {
    let now = date::now();
    let (value, replace_slot) = match setting.kind {
        TrackerKind::Null => {
            let Some(interval) = setting.interval else {
                ctx.error(format!("tracker '{name}' requires an interval to log"));
                return;
            };
            if let Some(message) = strict_null_error(name, setting) {
                ctx.error(message);
                return;
            }
            let Some(slot) = interval.slot_for(date::LogTime::local(now)) else {
                ctx.error(format!(
                    "tracker '{name}': could not compute the interval slot"
                ));
                return;
            };
            let position = if interval.cumulative { 0 } else { now - slot.0 };
            let replace_slot = if interval.cumulative {
                None
            } else {
                Some(slot)
            };
            (TrackerValue::Integer(position), replace_slot)
        }
        _ => {
            let Some(raw) = raw else { return };
            let value = match parse_tracker_value(name, setting.kind, raw) {
                Ok(value) => value,
                Err(e) => {
                    ctx.error(format!("{e:#}"));
                    return;
                }
            };
            if setting.strict
                && enforce_strict(name, setting, tracker_prompt::measure(&value)).is_err()
            {
                ctx.error(tracker_prompt::strict_message(
                    name,
                    setting,
                    setting.kind,
                    &value,
                ));
                return;
            }
            let replace_slot = match setting.interval {
                Some(iv) if !iv.cumulative => match iv.slot_for(date::LogTime::local(now)) {
                    Some(slot) => Some(slot),
                    None => {
                        ctx.error(format!(
                            "tracker '{name}': could not compute the interval slot"
                        ));
                        return;
                    }
                },
                _ => None,
            };
            (value, replace_slot)
        }
    };

    let entry = EntryObject {
        mood: String::new(),
        body: String::new(),
        time: date::LogTime::local(now),
        location: date::current_location(),
        embedding: None,
        score: None,
        trackers: vec![TrackerObject {
            tracker_type: name.to_string(),
            value,
            replace_slot,
        }],
        duration: None,
        todo_id: None,
    };
    match crate::db::create_entry(&pool(), &entry).await {
        Ok(_) => {
            ctx.logged.store(true, Ordering::Relaxed);
            ctx.success(format!("Logged {name}"));
        }
        Err(e) => ctx.error(format!("{e:#}")),
    }
}
