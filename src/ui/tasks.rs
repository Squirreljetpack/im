use anyhow::Result;
use matchmaker::{
    MatchError, Matchmaker, PickOptions,
    action::Action as MMAction,
    binds::display_help,
    config::HelpDisplayConfig,
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
use sqlx::SqlitePool;
use std::sync::{Arc, Mutex};

use crate::color::ColorAxes;
use crate::config::Config;
use crate::db::TaskRow;
use crate::global::{config, pool, GLOBAL_CONFIG};
use crate::task::{
    AcceptAction, accept_action, apply_accept_action, apply_completion_delta, availability_passed,
    reset_task_progress,
};
use crate::types::{ViewMode, ViewVariant};
use crate::ui::action::ImAction;
use crate::ui::common::{ct_to_ratatui_color, mode_label, BADGE_GAP};
use crate::ui::mm_config::get_mm_cfg;
use crate::ui::overlays::{
    ConfirmOverlay, ConfirmPrompt, InputOverlay, InputPrompt, SharedOverlay,
};
use crate::ui::previewer::Previewer;

// ---------- Interactive App ----------

pub struct TasksApp {
    pub tasks: Vec<TaskRow>,
    pub mode: ViewMode,
    pub show: ViewVariant,
    /// The built mood-color model (`MoodConfig::init_with`), threaded to
    /// the previewer. The config itself lives in [`global::GLOBAL_CONFIG`].
    pub axes: ColorAxes,
    pub sort_by_due: bool,
    /// Last cursor position (results index), restored after repopulation.
    pub(crate) cursor: u32,
    /// `im -F`: run the picker fullscreen (`tui.layout = None`) instead of
    /// the mm.toml `[tui]` percentage layout.
    pub fullscreen: bool,
}

impl TasksApp {
    pub async fn new(
        mode: ViewMode,
        config: Config,
        show: ViewVariant,
        axes: ColorAxes,
        fullscreen: bool,
    ) -> Self {
        let _ = GLOBAL_CONFIG.set(config.clone());
        let tasks = fetch_tasks(&pool(), mode, show, config.tasks_view.persist_pending_seconds)
            .await
            .unwrap_or_default();
        let mut app = Self {
            tasks,
            mode,
            show,
            axes,
            sort_by_due: true,
            cursor: 0,
            fullscreen,
        };
        app.apply_sort();
        app
    }

    fn apply_sort(&mut self) {
        if self.mode == ViewMode::DoneTasks {
            let date_key = |t: &TaskRow| crate::task::completed_sort_time(t);
            if self.sort_by_due {
                self.tasks.sort_by_key(|t| std::cmp::Reverse(date_key(t)));
            } else {
                self.tasks.sort_by_key(|t| {
                    (
                        std::cmp::Reverse(t.priority),
                        std::cmp::Reverse(date_key(t)),
                    )
                });
            }
        } else {
            let now = crate::date::now();
            let date_key = |t: &TaskRow| crate::task::pending_sort_time(t, now);
            if self.sort_by_due {
                self.tasks
                    .sort_by_key(|t| (date_key(t), std::cmp::Reverse(t.priority)));
            } else {
                self.tasks
                    .sort_by_key(|t| (std::cmp::Reverse(t.priority), date_key(t)));
            }
        }
    }

    pub async fn run(self) -> Result<()> {
        let (mut render_cfg, binds, mut tui_cfg, overlay_cfg) = get_mm_cfg();
        if self.fullscreen {
            tui_cfg.layout = None;
        }
        // The date column (visible index 1) is fixed at 8 wide — "Tu 08:00"
        // — so rows do not reflow as the cell text length varies.
        render_cfg.results.width_overrides = vec![0, 8, 0];

        // Shared view state: async tasks update the data, the render thread
        // (Repopulate) pushes it into the worker.
        let view = Arc::new(Mutex::new(self));
        let preview_axes = { view.lock().unwrap().axes.clone() };

        let worker = Worker::new(
            // The default column: label (index 2).
            task_columns(&view),
            2,
        );

        // The accept hook reports the selected task's (stable id, short id)
        // for programmatic accept flows. Enter is the custom `Action::Accept`
        // (the view's accept state machine), never the builtin matchmaker
        // accept, so `pick` only finishes on Quit/Esc; the hook still fires
        // if something triggers the builtin accept.
        let mut mm = Matchmaker::new(worker, tasks_accept_hook);

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
        let previewer = Previewer::new(preview_axes);
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
                    Some(task) => previewer.update_task(task),
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
                        Some(task) => previewer.update_task(task),
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

        let confirm_ov = Arc::new(Mutex::new(ConfirmOverlay::<TaskRow>::new()));
        let input_ov = Arc::new(Mutex::new(InputOverlay::<TaskRow>::new()));

        // Overlay order: 0 = confirm, 1 = input.
        let mut options = PickOptions::new()
            .event_loop(event_loop)
            .preview(preview_view)
            .overlay_config(overlay_cfg)
            .overlay(SharedOverlay::new(confirm_ov.clone()))
            .overlay(SharedOverlay::new(input_ov.clone()));

        // The render channel: async tasks push actions back into the render
        // loop through this sender.
        let render_tx = options.render_tx();

        let ctx = TaskCtx {
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

        // External editor: `Interrupt::Execute` runs with the TUI exited and
        // the event loop paused, so $EDITOR owns the terminal.
        mm.register_interrupt_handler(Interrupt::Execute, {
            let ctx = ctx.clone();
            move |_state| run_edit(&ctx)
        });

        options = options.ext_handler(move |action: ImAction, state| handler(action, state, &ctx));

        match mm.pick(options).await {
            Ok(_) | Err(MatchError::NoMatch) | Err(MatchError::Abort(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

// ---------- Shared context & async flows ----------

/// The three task-view columns (label, pri, datetime), shared by the
/// tasks view and the oneshot parent picker. Column cells carry no base
/// Text style; badge colors (and other per-span styling) live on the
/// spans inside.
pub(crate) fn task_columns(view: &Arc<Mutex<TasksApp>>) -> [Column<TaskRow, ()>; 3] {
    // The label column: badge glyph (span-colored) + name; recurring
    // tasks with a target count add an `m/n` sub-line.
    let label_column = {
        let view = view.clone();
        Column::new("label", move |item: &TaskRow, _: &()| {
            let (glyph, ct_color) = {
                let v = view.lock().unwrap();
                crate::badge::task_badge(item, config(), v.mode == ViewMode::DoneTasks)
            };
            let color = ct_to_ratatui_color(ct_color);
            let mut line = vec![
                Span::styled(format!("{glyph}{BADGE_GAP}"), Style::default().fg(color)),
                Span::styled(item.name.clone(), Style::default().fg(Color::White)),
            ];
            if item.target_count > 0 {
                line.push(Span::styled(
                    format!(" {}/{}", item.completions.unwrap_or(0), item.target_count),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Text::from(Line::from(line))
        })
        .with_raw(|item: &TaskRow, _: &()| std::borrow::Cow::Owned(item.name.clone()))
    };
    [
        // Same column set as the today view: priority (visible here),
        // datetime, label.
        Column::new("pri", |item: &TaskRow, _: &()| {
            Text::from(item.priority.to_string())
        }),
        Column::new("datetime", move |item: &TaskRow, _: &()| {
            // The today view's time-cell formatting: "HH:MM" today,
            // weekday prefix within the week, full date beyond; empty
            // for undated oneshots.
            let now = crate::date::now();
            let time = crate::task::pending_sort_time(item, now);
            Text::from(crate::today::task_time_label(
                item,
                time,
                crate::date::today_start(),
            ))
        }),
        label_column,
    ]
}

/// The accept hook for the tasks view and the oneshot parent picker:
/// reports the selected task's (stable id, short id).
pub(crate) fn tasks_accept_hook(
    state: &mut MMState<'_, TaskRow, ()>,
) -> Vec<(i64, Option<i64>)> {
    state
        .current_raw()
        .map(|task| (task.id, task.short_id))
        .into_iter()
        .collect()
}

/// Everything the render thread and the async handler tasks share.
#[derive(Clone)]
pub(crate) struct TaskCtx {
    pub(crate) view: Arc<Mutex<TasksApp>>,
    pub(crate) tx: RenderSender<ImAction>,
    pub(crate) confirm: Arc<Mutex<ConfirmOverlay<TaskRow>>>,
    pub(crate) input: Arc<Mutex<InputOverlay<TaskRow>>>,
    pub(crate) edit: Arc<Mutex<Option<EditPayload>>>,
}

/// Payload staged by the Edit action and consumed by the editor interrupt
/// handler while the TUI is suspended.
pub(crate) enum EditPayload {
    TaskBody { id: i64, body: String },
}

/// Dispatch a custom action on the render thread.
pub(crate) fn handler(action: ImAction, state: &mut MMState<'_, TaskRow, ()>, ctx: &TaskCtx) {
    match action {
        ImAction::Quit => state.should_quit = true,
        ImAction::ToggleSort => {
            let mut v = ctx.view.lock().unwrap();
            v.sort_by_due = !v.sort_by_due;
            v.apply_sort();
            drop(v);
            repopulate(state, ctx);
        }
        ImAction::CycleMode => {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                {
                    let mut v = ctx.view.lock().unwrap();
                    v.mode = match v.mode {
                        ViewMode::PendingTasks => ViewMode::DoneTasks,
                        ViewMode::DoneTasks => ViewMode::PendingTasks,
                    };
                }
                refresh_tasks(&ctx).await;
            });
        }
        ImAction::CycleFilter => {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                {
                    let mut v = ctx.view.lock().unwrap();
                    v.show = v.show.next();
                }
                refresh_tasks(&ctx).await;
            });
        }
        ImAction::Refresh => {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                refresh_tasks(&ctx).await;
            });
        }
        ImAction::Repopulate => repopulate(state, ctx),
        ImAction::Update => {
            if let Some(task) = state.current_raw().cloned() {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    accept_selected(&ctx, task).await;
                });
            }
        }
        ImAction::Delete => {
            if let Some(task) = state.current_raw().cloned() {
                open_confirm(ctx.clone(), delete_confirm(&task, ctx.clone()));
            }
        }
        ImAction::Edit => {
            if let Some(task) = state.current_raw().cloned() {
                *ctx.edit.lock().unwrap() = Some(EditPayload::TaskBody {
                    id: task.id,
                    body: task.body,
                });
                state.set_interrupt(Interrupt::Execute, String::new());
            }
        }
        // The tasks app has no link targets; the editor payload is always
        // staged synchronously here, so EditExecute is unused.
        ImAction::Link | ImAction::EditExecute => {}
    }
}

/// Rebuild the worker from the shared view state, refresh the ui border
/// title, and restore the cursor position.
pub(crate) fn repopulate(state: &mut MMState<'_, TaskRow, ()>, ctx: &TaskCtx) {
    let (tasks, title, cursor) = {
        let v = ctx.view.lock().unwrap();
        (v.tasks.clone(), tasks_header(&v), v.cursor)
    };
    state.worker_restart();
    let injector = state.injector();
    for task in &tasks {
        let _ = injector.push(task.clone());
    }
    state.ui.config.border.title = title;
    if !state.picker_ui.results.cursor_disabled() {
        let pos = cursor.min(tasks.len().saturating_sub(1) as u32);
        state.picker_ui.results.cursor_jump(pos);
    }
}

pub(crate) fn tasks_header(v: &TasksApp) -> String {
    format!(
        "{} [sort: {}] [show: {}]",
        mode_label(v.mode),
        if v.sort_by_due { "due" } else { "priority" },
        v.show.tasks_label()
    )
}

/// Stage a confirm prompt and activate overlay 0.
fn open_confirm(ctx: TaskCtx, prompt: ConfirmPrompt) {
    ctx.confirm.lock().unwrap().set_prompt(prompt);
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Overlay(0)));
}

/// Stage an input prompt and activate overlay 1.
fn open_input(ctx: TaskCtx, prompt: InputPrompt) {
    ctx.input.lock().unwrap().set_prompt(prompt);
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Overlay(1)));
}

/// Refetch the task list for the current view settings and signal the
/// render thread to repopulate.
async fn refresh_tasks(ctx: &TaskCtx) {
    let (mode, show, persist) = {
        let v = ctx.view.lock().unwrap();
        (
            v.mode,
            v.show,
            config().tasks_view.persist_pending_seconds,
        )
    };
    if let Ok(tasks) = fetch_tasks(&pool(), mode, show, persist).await {
        let mut v = ctx.view.lock().unwrap();
        v.tasks = tasks;
        v.apply_sort();
    }
    let _ = ctx.tx.send(RenderCommand::Action(MMAction::Custom(
        ImAction::Repopulate,
    )));
}

/// The Accept entry point: expired-history guard, D10 availability confirm,
/// then the shared accept state machine.
async fn accept_selected(ctx: &TaskCtx, task: TaskRow) {
    let now = crate::date::now();
    let mode = { ctx.view.lock().unwrap().mode };
    // D3: `@done:b` history rows (expired recurring tasks) are not
    // actionable — log and ignore.
    if task.is_recurring() && task.end_time.is_some_and(|end| now > end) {
        log::error!("task {} is expired", task.id);
        return;
    }
    // D10: in the pending view, Accept on a recurring task whose
    // availability window has passed asks first (default Yes).
    if mode != ViewMode::DoneTasks
        && task.is_recurring()
        && !task.is_done()
        && availability_passed(&task, now)
    {
        let ctx2 = ctx.clone();
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
                        run_task_accept(&ctx2, task.clone()).await;
                    });
                }
            })),
        };
        open_confirm(ctx.clone(), prompt);
        return;
    }
    run_task_accept(ctx, task).await;
}

/// The Accept-action state machine: modal-less toggles apply directly,
/// `ResetConfirm` / `CompletePrompt` open their overlays.
async fn run_task_accept(ctx: &TaskCtx, task: TaskRow) {
    let now = crate::date::now();
    let mode = { ctx.view.lock().unwrap().mode };
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
            refresh_tasks(ctx).await;
        }
        AcceptAction::Reset => {
            // The @done view asks before resetting; everywhere else a done
            // once-only/target-1 task resets directly.
            if mode == ViewMode::DoneTasks {
                open_confirm(ctx.clone(), reset_confirm(&task, ctx.clone()));
            } else {
                if let Err(e) = reset_task_progress(&pool(), &task).await {
                    log::error!("Failed to reset task progress for {}: {e}", task.id);
                }
                refresh_tasks(ctx).await;
            }
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
                        refresh_tasks(&ctx2).await;
                    });
                })),
            };
            open_input(ctx.clone(), prompt);
        }
    }
}

/// "Reset progress of 'X'?" confirm (default Yes).
fn reset_confirm(task: &TaskRow, ctx: TaskCtx) -> ConfirmPrompt {
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
                    refresh_tasks(&ctx).await;
                });
            }
        })),
    }
}

/// "Delete 'X'?" confirm (default No), with the recurrence warning.
fn delete_confirm(task: &TaskRow, ctx: TaskCtx) -> ConfirmPrompt {
    let task = task.clone();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Delete",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::raw(format!(" '{}'?", task.name)),
    ])];
    if task.is_recurring() {
        lines.push(Line::from(Span::styled(
            "  This task will stop recurring!",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    ConfirmPrompt {
        prompt: lines,
        options: vec![("Yes", 0), ("No", 0)],
        cursor: 1,
        on_accept: Some(Box::new(move |idx| {
            if idx == 0 {
                tokio::spawn(async move {
                    if let Err(e) = crate::db::delete_task(&pool(), task.id).await {
                        log::error!("Failed to delete task {}: {e}", task.id);
                    }
                    refresh_tasks(&ctx).await;
                });
            }
        })),
    }
}

/// Runs inside the `Interrupt::Execute` handler: the TUI is exited and the
/// event loop paused, so the blocking editor call owns the terminal.
pub(crate) fn run_edit(ctx: &TaskCtx) {
    let Some(EditPayload::TaskBody { id, body }) = ctx.edit.lock().unwrap().take() else {
        return;
    };
    let ctx = ctx.clone();
    match crate::editor::open_editor_on_text(&body) {
        Ok(new_body) => {
            tokio::spawn(async move {
                if let Err(e) = crate::db::update_todo_body(&pool(), id, &new_body).await {
                    log::error!("Failed to update task body: {e}");
                }
                refresh_tasks(&ctx).await;
            });
        }
        Err(e) => log::error!("Editor: {e}"),
    }
}

// ---------- Fetch helper ----------

/// Task rows for the current view mode; SQL lives in sql.rs (shared with
/// the CLI task view).
pub(crate) async fn fetch_tasks(
    pool: &SqlitePool,
    mode: ViewMode,
    show: ViewVariant,
    persist_pending_seconds: i64,
) -> Result<Vec<TaskRow>> {
    crate::db::fetch_tasks_for_view(pool, mode, show, persist_pending_seconds).await
}
