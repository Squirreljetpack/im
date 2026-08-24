//! `im -` — a matchmaker-backed viewer that lists every configured tracker
//! (a single name column) with a live preview of each tracker's settings and
//! a row of colored cells (one per entry of its resolved color palette).

use anyhow::Result;
use matchmaker::{
    MatchError, Matchmaker, PickOptions,
    binds::display_help,
    config::HelpDisplayConfig,
    event::EventLoop,
    message::Event,
    nucleo::{Column, Injector, Text as NText, Worker},
    render::MMState,
};
use ratatui::{
    backend::FromCrossterm,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::config::{Config, TrackerKind, TrackerSetting};
use crate::date;
use crate::ui::action::ImAction;
use crate::ui::mm_config::get_mm_cfg;
use crate::ui::previewer::Previewer;

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
        Span::styled(format!("  {}: ", label), Style::default().fg(Color::Yellow)),
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
    lines.push(Line::from(Span::styled(
        format!(" {name}"),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(name.len() + 2),
        Style::default().fg(Color::DarkGray),
    )));

    // Blank line, then the fields (kind leads them).
    lines.push(Line::default());

    lines.push(field_line("kind", kind_label(setting.kind).to_string()));

    // Bounds become `min` / `max` (a missing side shows `-`).
    if setting.low.is_some() || setting.high.is_some() {
        let low = setting
            .low
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into());
        let high = setting
            .high
            .map(|v| v.to_string())
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
            date::format_human_datetime(iv.anchor, named_months),
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
    let mut cells: Vec<Span<'static>> = Vec::with_capacity(rat_colors.len() * 2);
    for color in &rat_colors {
        cells.push(Span::styled("  ", Style::default().bg(*color)));
        cells.push(Span::raw(" "));
    }
    lines.push(Line::from(cells));

    Text::from(lines)
}

pub async fn run_trackers_app(config: &Config) -> Result<()> {
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

    // Enter is the builtin Accept (the default binds map it to `Action::Accept`).
    // The accept hook reports the selected tracker's name; we print it after
    // the picker exits.
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

    // The preview is built synchronously from the already-resolved palette,
    // so the color axes are unused here.
    let axes = Arc::new(OnceCell::new());
    let previewer = Previewer::new(axes);
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

    let mut options = PickOptions::new()
        .event_loop(event_loop)
        .preview(preview_view)
        .overlay_config(overlay_cfg);

    // Prime the list once before the first frame.
    options = options.initializer(move |state| {
        state.worker_restart();
        let injector = state.injector();
        for row in &rows {
            let _ = injector.push(row.clone());
        }
    });

    options = options.ext_handler(|_action: ImAction, _state| {});

    match mm.pick(options).await {
        Ok(names) => {
            for name in names {
                println!("{name}");
            }
            Ok(())
        }
        Err(MatchError::NoMatch) | Err(MatchError::Abort(_)) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
