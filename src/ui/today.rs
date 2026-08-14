use anyhow::Result;
use cba::bait::ResultExt;
use matchmaker::{
    MatchError, Matchmaker, PickOptions,
    action::Action as MMAction,
    binds::display_help,
    config::{HelpDisplayConfig, TerminalConfig},
    event::{EventLoop, RenderSender},
    message::{Event, Interrupt, RenderCommand},
    nucleo::{
        injector::Injector,
        {Column, Text, Worker},
    },
    render::MMState,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::color::ColorAxes;
use crate::config::{Config, TrackerKind};
use crate::global::{config, pool, GLOBAL_CONFIG};
use crate::db::TaskRow;
use crate::task::{
    AcceptAction, accept_action, apply_accept_action, apply_completion_delta, reset_task_progress,
};
use crate::today::{EntryKind, TodayEntry, fetch_today_entries};
use crate::types::{TaskKind, TodayHorizon, ViewVariant};
use crate::ui::action::ImAction;
use crate::ui::common::BADGE_GAP;
use crate::ui::mm_config::get_mm_cfg;
use crate::ui::overlays::{
    ConfirmOverlay, ConfirmPrompt, InputOverlay, InputPrompt, SharedOverlay,
};
use crate::ui::previewer::Previewer;

// ---------- Today View App ----------

/// The item kind reported by the today view's accept hook, paired with the
/// row id of the selected entry (`task_id` for tasks, the mood/tracker
/// entry id otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Task,
    Tracker,
    Mood,
}

impl From<EntryKind> for ItemKind {
    fn from(kind: EntryKind) -> Self {
        match kind {
            // Journal entries are mood rows with an empty mood label.
            EntryKind::Task(_) => ItemKind::Task,
            EntryKind::Tracker(_) => ItemKind::Tracker,
            EntryKind::Mood | EntryKind::Journal => ItemKind::Mood,
        }
    }
}

pub struct TodayApp {
    pub entries: Vec<TodayEntry>,
    pub horizon: TodayHorizon,
    pub show: ViewVariant,
    pub day_epoch: Option<i64>,
    pub day_label: String,
    /// The built mood-color model (`MoodConfig::init_with`), threaded to
    /// the previewer and the background color fill. The config itself lives
    /// in [`global::GLOBAL_CONFIG`].
    pub axes: ColorAxes,
    pub sort_by_priority: bool,
    /// Raw mood rows for the background color fill (startup fetch): the
    /// fill takes them by move, so entries stay embedding-free.
    pub mood_rows: Vec<crate::db::MoodRow>,
    /// Guards concurrent color fills (startup + refresh overlap).
    fill_running: Arc<AtomicBool>,
    /// Last cursor position (results index), restored after repopulation.
    cursor: u32,
    /// `im -F`: run the picker fullscreen (`tui.layout = None`) instead of
    /// the mm.toml `[tui]` percentage layout.
    pub fullscreen: bool,
}

impl TodayApp {
    pub async fn new(
        config: Config,
        axes: ColorAxes,
        day_epoch: Option<i64>,
        show: ViewVariant,
        horizon: TodayHorizon,
        fullscreen: bool,
    ) -> Self {
        let _ = GLOBAL_CONFIG.set(config.clone());
        let crate::today::TodayFetch { entries, mood_rows } =
            fetch_today_entries(&pool(), &config, horizon, day_epoch, show)
                .await
                .unwrap_or_default();
        let day_label = day_label_for(day_epoch);
        let mut app = Self {
            entries,
            mood_rows,
            horizon,
            show,
            day_epoch,
            day_label,
            axes,
            sort_by_priority: false,
            fill_running: Arc::new(AtomicBool::new(false)),
            cursor: 0,
            fullscreen,
        };
        app.apply_sort();
        app
    }

    fn apply_sort(&mut self) {
        if self.sort_by_priority {
            self.entries.sort_by(|a, b| {
                b.priority
                    .cmp(&a.priority)
                    .then(crate::today::today_sort(a, b))
            });
        } else {
            self.entries.sort_by(crate::today::today_sort);
        }
    }

    pub async fn run(self) -> Result<()> {
        self.run_with(TodayRunCfg::default()).await
    }

    pub async fn run_with(self, cfg: TodayRunCfg) -> Result<()> {
        let (mut render_cfg, binds, mut tui_cfg, overlay_cfg) = get_mm_cfg();
        if self.fullscreen {
            tui_cfg.layout = None;
        }
        if let Some(tui) = cfg.tui {
            tui_cfg = tui;
        }
        // The priority column (index 0) starts hidden in the today view;
        // unhide it with the `UnhideColumn` bind.
        render_cfg.results.hidden_columns.set(0);
        // The time column (first visible column) is fixed at 8 wide —
        // "Tu 08:00" — so rows do not reflow as the cell text length
        // varies.
        render_cfg.results.width_overrides = vec![8, 0];

        let preview_axes = self.axes.clone();
        let view = Arc::new(Mutex::new(self));

        let columns = [
            // Column cells carry no base Text style; badge colors (and
            // other per-span styling) live on the spans inside.
            Column::new("pri", |item: &TodayEntry, _: &()| {
                Text::from(item.priority.to_string())
            }),
            Column::new("datetime", |item: &TodayEntry, _: &()| {
                Text::from(item.time_label.clone())
            }),
            Column::new("label", move |item: &TodayEntry, _: &()| {
                // Journal entries show the first body line in the label column.
                let label = if item.kind == EntryKind::Journal {
                    item.body.lines().next().unwrap_or("").to_string()
                } else {
                    item.label.clone()
                };
                // The badge is derived at render time (glyph + color from
                // the global config and the mood-color cache) and prefixes
                // the label — the badge itself is not stored on the entry.
                let (badge, color) = {
                    let (badge, color) = item.badge(config());
                    (badge, color)
                };
                let mut spans = Vec::with_capacity(2);
                if let Some(glyph) = badge {
                    spans.push(Span::styled(
                        format!("{glyph}{BADGE_GAP}"),
                        Style::default().fg(color),
                    ));
                }
                spans.push(Span::styled(label, Style::default().fg(Color::White)));
                Text::from(Line::from(spans))
            })
        ];

        let worker = Worker::new(
            columns,
            // Default column: label (index 2, after priority + time).
            2,
        );
        // The accept hook reports the selected entry (its item kind + row
        // id) for programmatic accept flows. Enter is the custom
        // `Action::Accept` (the view's accept state machine), never the
        // builtin matchmaker accept, so `pick` only finishes on Quit/Esc;
        // the hook still fires if something triggers the builtin accept.
        let mut mm = Matchmaker::new(worker, |state: &mut MMState<'_, '_, TodayEntry, ()>| {
            state
                .current_raw()
                .and_then(|e| {
                    e.id.or(e.task_id)
                        .map(|id| (ItemKind::from(e.kind), id))
                })
                .into_iter()
                .collect()
        });

        // The event loop owns the binds; the alt-h help renders the live
        // bind map through the loop's pointer (`get_binds_ptr`), so the
        // help always reflects the current bindings.
        let event_loop = EventLoop::with_binds(binds)
            .with_tick_rate(render_cfg.ui.tick_rate)
            .with_mouse_events(render_cfg.ui.mouse_events)
            .with_scroll_debounce(render_cfg.ui.mouse_scroll_debounce_ms);
        let binds_ptr = event_loop.get_binds_ptr();

        mm.config_render(render_cfg);
        mm.config_tui(tui_cfg);

        // Previewer: the event listener owns it (the `Preview` widget built
        // by `view()` holds its own clones of the shared string). A
        // generation counter drops stale async results.
        let previewer = Previewer::new(preview_axes.clone());
        let preview_view = previewer.view();
        // Clone for the PreviewSet handler below (the cursor handler moves
        // the original).
        let help_previewer = previewer.clone();
        mm.register_event_handler(
            Event::CursorChange | Event::PreviewChange | Event::Synced,
            move |state, _| {
                // Frames that explicitly set the preview (alt-h help) are
                // owned by the PreviewSet handler; don't clobber them with
                // the cursor-tracked preview.
                if state.contains(Event::PreviewSet) {
                    return;
                }
                // Reset so the next alt-h press counts as a change (the
                // help toggle) instead of re-showing stale help.
                state.preview_set_payload = None;
                if !state.preview_visible() {
                    return;
                }
                match state.current_raw().cloned() {
                    Some(entry) => previewer.update_today(entry),
                    None => previewer.stop(),
                }
            },
        );

        // alt-h help: the render loop stages the payload for the builtin
        // `Action::Help` as `Event::PreviewSet`. An empty payload renders
        // the keybinding help from the event loop's live bind map; a
        // non-empty payload is shown verbatim; None (the second alt-h)
        // falls back to the cursor-tracked preview.
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
                        Some(entry) => previewer.update_today(entry),
                        None => previewer.stop(),
                    },
                    // The Help action always stages an Err payload; a
                    // command template has no meaning in this previewer.
                    Some(Ok(_)) => previewer.stop(),
                }
            }
        });

        // Track the cursor so repopulation can restore its position.
        mm.register_event_handler(Event::CursorChange, {
            let view = view.clone();
            move |state, _| {
                if let Some(idx) = state.current_index() {
                    view.lock().unwrap().cursor = idx;
                }
            }
        });

        let confirm_ov = Arc::new(Mutex::new(ConfirmOverlay::<TodayEntry>::new()));
        let input_ov = Arc::new(Mutex::new(InputOverlay::<TodayEntry>::new()));

        // Overlay order: 0 = confirm, 1 = input.
        let mut options = PickOptions::new()
            .event_loop(event_loop)
            .preview(preview_view)
            .overlay_config(overlay_cfg)
            .overlay(SharedOverlay::new(confirm_ov.clone()))
            .overlay(SharedOverlay::new(input_ov.clone()));

        let render_tx = options.render_tx();

        let ctx = TodayCtx {
            view: view.clone(),
            tx: render_tx.clone(),
            confirm: confirm_ov.clone(),
            input: input_ov.clone(),
            edit: Arc::new(Mutex::new(None)),
        };

        // The initializer runs once before the first frame: prime the list
        // and the border title through the same repopulate path that keeps
        // them fresh after mutations.
        options = options.initializer({
            let ctx = ctx.clone();
            move |state| repopulate(state, &ctx)
        });

        // Background color fill for the startup fetch: the mood rows move
        // in (embeddings never copied) and the fill redraws when it adds
        // colors.
        let fill_running = { view.lock().unwrap().fill_running.clone() };
        let initial_rows = {
            let mut v = view.lock().unwrap();
            std::mem::take(&mut v.mood_rows)
        };
        spawn_mood_fill(
            initial_rows,
            preview_axes,
            render_tx.clone(),
            fill_running,
        );

        // External editor: `Interrupt::Execute` runs with the TUI exited and
        // the event loop paused, so $EDITOR owns the terminal.
        mm.register_interrupt_handler(Interrupt::Execute, {
            let ctx = ctx.clone();
            move |_state| run_edit(&ctx)
        });

        if let Some(event_loop) = cfg.event_loop {
            options = options.event_loop(event_loop);
        }

        // Test hook: fire after the render channel exists and before `pick`
        // blocks — tests quit the headless picker by pushing a custom
        // `Action::Quit` through it (after the first render).
        if let Some(on_start) = cfg.on_start {
            on_start(ctx.tx.clone());
        }

        options = options.ext_handler(move |action: ImAction, state| handler(action, state, &ctx));

        match mm.pick(options).await {
            Ok(_) | Err(MatchError::NoMatch) | Err(MatchError::Abort(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Test-only knobs for [`TodayApp::run_with`]: run the TUI headless
/// (matchmaker's `IoStream::Test` capture backend) with an input-less
/// event loop, and quit it from outside.
#[derive(Default)]
pub struct TodayRunCfg {
    /// Overrides the TUI config from `mm.toml` (e.g. `stream = Test`).
    pub tui: Option<TerminalConfig>,
    /// Overrides the event loop (e.g. `EventLoop::new().as_optional()`
    /// for headless runs with no terminal input).
    pub event_loop: Option<EventLoop<ImAction>>,
    /// Called just before `pick` with the render channel; tests spawn a
    /// task that pushes `Action::Quit` through it after the first render.
    pub on_start: Option<Box<dyn Fn(RenderSender<ImAction>) + Send + Sync>>,
}

// ---------- Shared context & async flows ----------

/// Everything the render thread and the async handler tasks share.
#[derive(Clone)]
struct TodayCtx {
    view: Arc<Mutex<TodayApp>>,
    tx: RenderSender<ImAction>,
    confirm: Arc<Mutex<ConfirmOverlay<TodayEntry>>>,
    input: Arc<Mutex<InputOverlay<TodayEntry>>>,
    edit: Arc<Mutex<Option<EditPayload>>>,
}

/// Payload staged by the Edit action and consumed by the editor interrupt
/// handler while the TUI is suspended.
enum EditPayload {
    TaskBody {
        id: i64,
        body: String,
    },
    MoodBody {
        id: i64,
        body: String,
    },
    TrackerValue {
        id: i64,
        kind: TrackerKind,
        value: String,
    },
}

/// The link direction of a link prompt, chosen by the selected entry's
/// kind when the prompt opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    /// Mood/journal entry → task: inserts a `task_moods` row.
    MoodToTask,
    /// Tracker entry → mood: sets the tracker's `mood` column (replacing
    /// any existing link).
    TrackerToMood,
    /// Task entry → task: sets the task's `parent` column (replacing any
    /// existing parent).
    TaskToParent,
}

/// Dispatch a custom action on the render thread.
fn handler(action: ImAction, state: &mut MMState<'_, '_, TodayEntry, ()>, ctx: &TodayCtx) {
    match action {
        ImAction::Quit => state.should_quit = true,
        ImAction::ToggleSort => {
            let mut v = ctx.view.lock().unwrap();
            v.sort_by_priority = !v.sort_by_priority;
            v.apply_sort();
            drop(v);
            repopulate(state, ctx);
        }
        ImAction::CycleMode => {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                {
                    let mut v = ctx.view.lock().unwrap();
                    v.horizon = v.horizon.next();
                }
                refresh_today(&ctx).await;
            });
        }
        ImAction::CycleFilter => {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                {
                    let mut v = ctx.view.lock().unwrap();
                    v.show = v.show.next();
                }
                refresh_today(&ctx).await;
            });
        }
        ImAction::Refresh => {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                refresh_today(&ctx).await;
            });
        }
        ImAction::Repopulate => repopulate(state, ctx),
        ImAction::Update => {
            if let Some(entry) = state.current_raw().cloned() {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    accept_entry(&ctx, entry).await;
                });
            }
        }
        ImAction::Delete => {
            if let Some(entry) = state.current_raw().cloned() {
                open_confirm(ctx.clone(), delete_confirm(&entry, ctx.clone()));
            }
        }
        ImAction::Link => {
            if let Some(entry) = state.current_raw().cloned() {
                open_link(ctx.clone(), &entry);
            }
        }
        ImAction::Edit => {
            let Some(entry) = state.current_raw().cloned() else {
                return;
            };
            edit_selected(ctx, state, &entry);
        }
        // The editor payload may be staged from an async task (live task
        // fetch); EditExecute raises the interrupt once it is staged.
        ImAction::EditExecute => {
            state.set_interrupt(Interrupt::Execute, String::new());
        }
    }
}

/// Rebuild the worker from the shared view state, refresh the ui border
/// title, and restore the cursor position.
fn repopulate(state: &mut MMState<'_, '_, TodayEntry, ()>, ctx: &TodayCtx) {
    let (entries, title, cursor) = {
        let v = ctx.view.lock().unwrap();
        (v.entries.clone(), today_header(&v), v.cursor)
    };
    state.worker_restart();
    let injector = state.injector();
    for entry in &entries {
        let _ = injector.push(entry.clone());
    }
    state.ui.config.border.title = title;
    if !state.picker_ui.results.cursor_disabled() {
        let pos = cursor.min(entries.len().saturating_sub(1) as u32);
        state.picker_ui.results.cursor_jump(pos);
    }
}

/// Title label for the anchored day: "Today" / "Yesterday" / DD-MM-YY,
/// plus the horizon/sort/show indicators.
fn today_header(v: &TodayApp) -> String {
    let horizon_suffix = if v.horizon == TodayHorizon::Today {
        String::new()
    } else {
        format!(" ({})", v.horizon.label())
    };
    format!(
        "{}{} [sort: {}] [show: {}]",
        v.day_label,
        horizon_suffix,
        if v.sort_by_priority {
            "priority"
        } else {
            "time"
        },
        v.show.today_label()
    )
}

fn day_label_for(day_epoch: Option<i64>) -> String {
    match day_epoch {
        None => "Today".to_string(),
        Some(ts) if ts == crate::date::today_start() => "Today".to_string(),
        Some(ts) if ts == crate::date::today_start() - 86400 => "Yesterday".to_string(),
        Some(ts) => crate::date::format_date(ts),
    }
}

/// Stage a confirm prompt and activate overlay 0.
fn open_confirm(ctx: TodayCtx, prompt: ConfirmPrompt) {
    ctx.confirm.lock().unwrap().set_prompt(prompt);
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Overlay(0)));
}

/// Stage an input prompt and activate overlay 1.
fn open_input(ctx: TodayCtx, prompt: InputPrompt) {
    ctx.input.lock().unwrap().set_prompt(prompt);
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Overlay(1)));
}

/// Refetch the entry list for the current view settings and signal the
/// render thread to repopulate.
async fn refresh_today(ctx: &TodayCtx) {
    let (axes, horizon, day_epoch, show) = {
        let v = ctx.view.lock().unwrap();
        (v.axes.clone(), v.horizon, v.day_epoch, v.show)
    };
    let crate::today::TodayFetch { entries, mood_rows } =
        fetch_today_entries(&pool(), config(), horizon, day_epoch, show)
            .await
            .unwrap_or_default();
    let fill_running = {
        let mut v = ctx.view.lock().unwrap();
        v.entries = entries;
        v.mood_rows = mood_rows;
        v.apply_sort();
        v.fill_running.clone()
    };
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Custom(
        ImAction::Repopulate,
    )));
    // Background fill with the freshly fetched rows (embeddings move in).
    let rows = {
        let mut v = ctx.view.lock().unwrap();
        std::mem::take(&mut v.mood_rows)
    };
    spawn_mood_fill(rows, axes, ctx.tx.clone(), fill_running);
}

/// Background color fill: compute colors for the rows the process-wide
/// cache is missing, then redraw so the next frame picks them up. A
/// second fill while one is running reuses the cache and no-ops.
fn spawn_mood_fill(
    rows: Vec<crate::db::MoodRow>,
    axes: ColorAxes,
    tx: RenderSender<ImAction>,
    running: Arc<AtomicBool>,
) {
    if running.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        let added = crate::color::compute_mood_colors(&rows, &axes);
        running.store(false, Ordering::Release);
        if added > 0 {
            let _ = tx.send(RenderCommand::Redraw);
        }
    });
}

/// The Accept entry point: tracker rows relog/prompt, recurring windows
/// check their availability, task rows run the accept state machine.
async fn accept_entry(ctx: &TodayCtx, entry: TodayEntry) {
    if let EntryKind::Tracker(kind) = entry.kind {
        tracker_accept(ctx, kind, entry.id, &entry.label).await;
        return;
    }
    // Enter on a mood/journal row edits its body — the same $EDITOR flow
    // as the Edit action: the payload is staged and EditExecute raises the
    // editor interrupt on the render thread.
    if matches!(entry.kind, EntryKind::Mood | EntryKind::Journal) {
        let Some(id) = entry.id else { return };
        *ctx.edit.lock().unwrap() = Some(EditPayload::MoodBody {
            id,
            body: entry.body.clone(),
        });
        let _ = ctx
            .tx
            .send(RenderCommand::Action(MMAction::Custom(ImAction::EditExecute)));
        return;
    }
    if let Some(window) = &entry.recurring_window {
        // D10: Accept on a recurring task whose availability window has
        // passed asks first (default Yes). The check is per window
        // (`now >= window_end` on a not-done window).
        if !window.task.is_done() && crate::date::now() >= window.window_end {
            let ctx2 = ctx.clone();
            let task = window.task.clone();
            let prompt = ConfirmPrompt {
                prompt: vec![
                    Line::from(vec![
                        Span::styled(
                            "The availability window for",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::ITALIC),
                        ),
                        Span::raw(format!(" '{}' has passed.", task.name)),
                    ]),
                    Line::from(Span::styled(
                        "  Update anyway?",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::ITALIC),
                    )),
                ],
                options: vec![("Yes", 0), ("No", 0)],
                cursor: 0,
                on_accept: Some(Box::new(move |idx| {
                    if idx == 0 {
                        tokio::spawn(async move {
                            run_today_task_accept(&ctx2, task).await;
                        });
                    }
                })),
            };
            open_confirm(ctx.clone(), prompt);
            return;
        }
        run_today_task_accept(ctx, window.task.clone()).await;
        return;
    }
    let Some(task_id) = entry.task_id else {
        return;
    };
    let task = crate::db::fetch_task_by_id(&pool(), task_id, crate::date::now())
        .await
        .ok()
        .flatten();
    if let Some(task) = task {
        run_today_task_accept(ctx, task).await;
    }
}

/// Accept on a tracker row: null trackers re-log in place (time → now,
/// and in count mode the count increments too, mirroring the CLI);
/// value-bearing kinds open the "Update:" prompt.
async fn tracker_accept(ctx: &TodayCtx, kind: TrackerKind, tracker_id: Option<i64>, label: &str) {
    let Some(tracker_id) = tracker_id else {
        return;
    };
    if kind == TrackerKind::Null {
        let increment = match label.split_once(':') {
            Some((name, _)) => config()
                .tracker
                .get(name.trim())
                .is_some_and(|t| t.min.is_none() || t.max.is_none()),
            None => false,
        };
        let _ =
            crate::db::relog_null_tracker(&pool(), tracker_id, crate::date::now(), increment).await;
        refresh_today(ctx).await;
        return;
    }
    let tracker_type = label
        .split_once(':')
        .map(|(n, _)| n.trim().to_string())
        .unwrap_or_default();
    open_input(ctx.clone(), update_tracker_prompt(ctx, tracker_id, &tracker_type, kind));
}

/// The "Update:" prompt for a value-bearing tracker: kind-filtered input
/// validated with the shared tracker parser on submit. Used by Accept on a
/// tracker row and by Edit on Number/Float trackers.
fn update_tracker_prompt(
    ctx: &TodayCtx,
    tracker_id: i64,
    tracker_type: &str,
    kind: TrackerKind,
) -> InputPrompt {
    let ctx2 = ctx.clone();
    let tracker_type = tracker_type.to_string();
    InputPrompt {
        title: "Update".to_string(),
        label: "Update: ".to_string(),
        placeholder: None,
        input: String::new(),
        error: None,
        allowed: Some(Box::new(move |c: char| match kind {
            TrackerKind::Number => c.is_ascii_digit() || c == '-',
            TrackerKind::Float => c.is_ascii_digit() || c == '-' || c == '.',
            TrackerKind::Text => true,
            TrackerKind::Null => false,
        })),
        validator: Some(Box::new(move |s: &str| {
            if s.trim().is_empty() {
                Err("requires a value".to_string())
            } else {
                crate::tracker::parse_tracker_value(&tracker_type, kind, s.trim())
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            }
        })),
        on_submit: Some(Box::new(move |val| {
            tokio::spawn(async move {
                let _ =
                    crate::db::update_tracker_score(&pool(), tracker_id, kind, val.trim()).await;
                refresh_today(&ctx2).await;
            });
        })),
    }
}

/// The Accept-action state machine for a task: modal-less toggles apply
/// directly, `ResetConfirm` / `CompletePrompt` open their overlays.
/// Reset applies directly here (the confirm is tasks-view-only).
async fn run_today_task_accept(ctx: &TodayCtx, task: TaskRow) {
    let now = crate::date::now();
    let action = accept_action(
        task.completions,
        task.is_scheduled(),
        task.target_count,
        task.start_time,
        task.available_duration_secs,
        now,
    );
    match action {
        AcceptAction::Complete | AcceptAction::SetFailed | AcceptAction::Clear => {
            if let Err(e) = apply_accept_action(&pool(), &task, action).await {
                log::error!("Failed to apply accept action to task {}: {e}", task.id);
            }
            refresh_today(ctx).await;
        }
        AcceptAction::Reset => {
            if let Err(e) = reset_task_progress(&pool(), &task).await {
                log::error!("Failed to reset task progress for {}: {e}", task.id);
            }
            refresh_today(ctx).await;
        }
        AcceptAction::ResetConfirm => {
            // target_count > 1 done: ask first (default Yes).
            open_confirm(ctx.clone(), reset_confirm(&task, ctx.clone()));
        }
        AcceptAction::CompletePrompt => {
            // target_count > 1 not done: the numeric prompt.
            let ctx2 = ctx.clone();
            let task_id = task.id;
            let prompt = InputPrompt {
                title: "Update".to_string(),
                label: "Count: ".to_string(),
                placeholder: Some("1".to_string()),
                input: String::new(),
                error: None,
                allowed: Some(Box::new(|c: char| c.is_ascii_digit() || c == '-')),
                validator: Some(Box::new(|s: &str| {
                    if s.is_empty() {
                        Ok(())
                    } else {
                        s.parse::<i32>()
                            .map(|_| ())
                            .map_err(|_| "invalid number".to_string())
                    }
                })),
                on_submit: Some(Box::new(move |val| {
                    // Empty input adds 1; 0 is a no-op.
                    let delta = if val.trim().is_empty() {
                        1
                    } else {
                        val.trim().parse::<i32>().unwrap_or(1)
                    };
                    tokio::spawn(async move {
                        let _ = apply_completion_delta(&pool(), task_id, delta).await;
                        refresh_today(&ctx2).await;
                    });
                })),
            };
            open_input(ctx.clone(), prompt);
        }
    }
}

/// "Reset progress of 'X'?" confirm (default Yes).
fn reset_confirm(task: &TaskRow, ctx: TodayCtx) -> ConfirmPrompt {
    let task = task.clone();
    ConfirmPrompt {
        prompt: vec![Line::from(vec![
            Span::styled(
                "Reset progress of",
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::raw(format!(" '{}'?", task.name)),
        ])],
        options: vec![("Yes", 0), ("No", 0)],
        cursor: 0,
        on_accept: Some(Box::new(move |idx| {
            if idx == 0 {
                tokio::spawn(async move {
                    if let Err(e) = reset_task_progress(&pool(), &task).await {
                        log::error!("Failed to reset task progress for {}: {e}", task.id);
                    }
                    refresh_today(&ctx).await;
                });
            }
        })),
    }
}

/// "Delete ...?" confirm (default No), with the recurrence warning. Every
/// entry kind is deletable; journal entries have an empty label → the
/// prompt says "Delete journal entry?".
fn delete_confirm(entry: &TodayEntry, ctx: TodayCtx) -> ConfirmPrompt {
    let entry = entry.clone();
    let is_recurring = entry.kind == EntryKind::Task(TaskKind::Recurring);
    let mut lines = if entry.label.is_empty() {
        vec![Line::from(Span::styled(
            "Delete journal entry?",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::ITALIC),
        ))]
    } else {
        vec![Line::from(vec![
            Span::styled(
                "Delete",
                Style::default()
                    .fg(Color::Red)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::raw(format!(" '{}'?", entry.label)),
        ])]
    };
    if is_recurring {
        lines.push(Line::from(Span::styled(
            "  This task will stop recurring!",
            Style::default().add_modifier(Modifier::ITALIC),
        )));
    }
    ConfirmPrompt {
        prompt: lines,
        options: vec![("Yes", 0), ("No", 0)],
        cursor: 1,
        on_accept: Some(Box::new(move |idx| {
            if idx == 0 {
                tokio::spawn(async move {
                    match entry.kind {
                        EntryKind::Mood | EntryKind::Journal => {
                            if let Some(id) = entry.id
                                && let Err(e) = crate::db::delete_mood(&pool(), id).await
                            {
                                log::error!("Failed to delete mood {id}: {e}");
                            }
                        }
                        EntryKind::Tracker(_) => {
                            if let Some(id) = entry.id
                                && let Err(e) = crate::db::delete_tracker_entry(&pool(), id).await
                            {
                                log::error!("Failed to delete tracker entry {id}: {e}");
                            }
                        }
                        EntryKind::Task(_) => {
                            if let Some(task_id) = entry.task_id
                                && let Err(e) = crate::db::delete_task(&pool(), task_id).await
                            {
                                log::error!("Failed to delete task {task_id}: {e}");
                            }
                        }
                    }
                    refresh_today(&ctx).await;
                });
            }
        })),
    }
}

/// Open the Link prompt for the selected entry: mood/journal → link the
/// mood to a task, tracker → link the tracker to a mood, task → link the
/// task to a parent task. The typed id is the raw row id (no validation);
/// empty input cancels.
fn open_link(ctx: TodayCtx, entry: &TodayEntry) {
    let (kind, target_id) = match entry.kind {
        EntryKind::Mood | EntryKind::Journal => {
            let Some(id) = entry.id else { return };
            (LinkKind::MoodToTask, id)
        }
        EntryKind::Tracker(_) => {
            let Some(id) = entry.id else { return };
            (LinkKind::TrackerToMood, id)
        }
        EntryKind::Task(_) => {
            let Some(task_id) = entry.task_id else { return };
            (LinkKind::TaskToParent, task_id)
        }
    };
    let ctx2 = ctx.clone();
    let prompt = InputPrompt {
        title: "Link".to_string(),
        label: "Link: ".to_string(),
        placeholder: None,
        input: String::new(),
        error: None,
        allowed: Some(Box::new(|c: char| c.is_ascii_digit())),
        validator: None,
        on_submit: Some(Box::new(move |val| {
            let Some(id) = val.trim().parse::<i64>().ok() else {
                return;
            };
            tokio::spawn(async move {
                let result = match kind {
                    LinkKind::MoodToTask => {
                        crate::db::link_mood_to_task(&pool(), target_id, id).await
                    }
                    LinkKind::TrackerToMood => {
                        crate::db::link_tracker_to_mood(&pool(), target_id, id).await
                    }
                    LinkKind::TaskToParent => {
                        crate::db::set_task_parent(&pool(), target_id, id).await
                    }
                };
                let _ = result.elog();
                refresh_today(&ctx2).await;
            });
        })),
    };
    open_input(ctx, prompt);
}

/// Edit the selected entry: task/mood bodies and text-tracker payloads
/// open the external editor (TUI suspended via `Interrupt::Execute`);
/// number/float trackers open an in-TUI input overlay validated against
/// the tracker kind.
fn edit_selected(ctx: &TodayCtx, state: &mut MMState<'_, '_, TodayEntry, ()>, entry: &TodayEntry) {
    match entry.kind {
        EntryKind::Task(_) => {
            if let Some(window) = &entry.recurring_window {
                *ctx.edit.lock().unwrap() = Some(EditPayload::TaskBody {
                    id: window.task.id,
                    body: window.task.body.clone(),
                });
                state.set_interrupt(Interrupt::Execute, String::new());
            } else if let Some(task_id) = entry.task_id {
                let edit = ctx.edit.clone();
                let tx = ctx.tx.clone();
                tokio::spawn(async move {
                    if let Ok(Some(task)) =
                        crate::db::fetch_task_by_id(&pool(), task_id, crate::date::now()).await
                    {
                        *edit.lock().unwrap() = Some(EditPayload::TaskBody {
                            id: task.id,
                            body: task.body,
                        });
                        let _ = tx.send(RenderCommand::Action(MMAction::Custom(
                            ImAction::EditExecute,
                        )));
                    }
                });
            }
        }
        EntryKind::Tracker(kind) => {
            let Some(tracker_id) = entry.id else { return };
            let Some((tracker_type, current)) = entry.label.split_once(':') else {
                return;
            };
            match kind {
                // Text payloads edit via the external editor.
                TrackerKind::Text => {
                    *ctx.edit.lock().unwrap() = Some(EditPayload::TrackerValue {
                        id: tracker_id,
                        kind,
                        value: current.trim().to_string(),
                    });
                    state.set_interrupt(Interrupt::Execute, String::new());
                }
                // Number/Float payloads route to the Update prompt — the
                // same overlay as Accept on the row.
                TrackerKind::Number | TrackerKind::Float => {
                    open_input(
                        ctx.clone(),
                        update_tracker_prompt(ctx, tracker_id, tracker_type.trim(), kind),
                    );
                }
                // Null payloads are not editable.
                TrackerKind::Null => {}
            }
        }
        EntryKind::Mood | EntryKind::Journal => {
            let Some(id) = entry.id else { return };
            *ctx.edit.lock().unwrap() = Some(EditPayload::MoodBody {
                id,
                body: entry.body.clone(),
            });
            state.set_interrupt(Interrupt::Execute, String::new());
        }
    }
}

/// Runs inside the `Interrupt::Execute` handler: the TUI is exited and the
/// event loop paused, so the blocking editor call owns the terminal.
fn run_edit(ctx: &TodayCtx) {
    let Some(payload) = ctx.edit.lock().unwrap().take() else {
        return;
    };
    let ctx = ctx.clone();
    match payload {
        EditPayload::TaskBody { id, body } => match crate::editor::open_editor_on_text(&body) {
            Ok(new_body) => {
                tokio::spawn(async move {
                    let _ = crate::db::update_todo_body(&pool(), id, &new_body).await;
                    refresh_today(&ctx).await;
                });
            }
            Err(e) => log::error!("Editor: {e}"),
        },
        EditPayload::MoodBody { id, body } => match crate::editor::open_editor_on_text(&body) {
            Ok(new_body) => {
                tokio::spawn(async move {
                    let _ = crate::db::update_mood_body(&pool(), id, &new_body).await;
                    refresh_today(&ctx).await;
                });
            }
            Err(e) => log::error!("Editor: {e}"),
        },
        EditPayload::TrackerValue { id, kind, value } => {
            match crate::editor::open_editor_on_text(&value) {
                Ok(new_value) => {
                    tokio::spawn(async move {
                        let _ = crate::db::update_tracker_score(&pool(), id, kind, &new_value).await;
                        refresh_today(&ctx).await;
                    });
                }
                Err(e) => log::error!("Editor: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::day_label_for;

    #[test]
    fn test_day_label() {
        let today = crate::date::today_start();
        // Anchored today (explicit or implicit) → "Today".
        assert_eq!(day_label_for(None), "Today");
        assert_eq!(day_label_for(Some(today)), "Today");
        // Yesterday.
        assert_eq!(day_label_for(Some(today - 86400)), "Yesterday");
        // Any other day → DD-MM-YY.
        let other = crate::date::parse_datetime("2024-03-15", crate::date::DATE_DIALECT).unwrap();
        assert_eq!(
            day_label_for(Some(crate::date::day_start(other))),
            "15-03-24"
        );
    }
}
