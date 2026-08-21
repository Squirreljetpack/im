//! The oneshot parent picker: a pending-tasks view used to choose a
//! oneshot task's parent interactively (`im ! -`).
//!
//! Reuses the tasks view's shared pieces ([`TasksApp`], its columns, the
//! accept hook and the action handler) with three differences:
//! - the done view is unreachable (the `CycleMode` bind is dropped and the
//!   handler ignores the action even if the config file binds it),
//! - the preview pane is forced off (`preview.show = false`) so the picker
//!   uses the full width,
//! - Enter runs the builtin matchmaker `Accept` (fires the accept hook and
//!   finishes the pick) instead of the tasks view's Update state machine.
//!
//! Editing, deleting, refreshing and the other actions remain available.

use anyhow::Result;
use matchmaker::{
    MatchError, Matchmaker, PickOptions,
    action::{Action as MMAction, Actions},
    binds::{key},
    message::Interrupt,
    nucleo::Worker,
};
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::db::TaskRow;
use crate::global::GLOBAL_CONFIG;
use crate::types::{ViewMode, ViewVariant};
use crate::ui::action::ImAction;
use crate::ui::mm_config::get_mm_cfg;
use crate::ui::overlays::{ConfirmOverlay, InputOverlay, SharedOverlay};
use crate::ui::tasks::{TaskCtx, TasksApp, handler, repopulate, run_edit, task_columns, tasks_accept_hook};

/// A picker over pending tasks (the oneshot parent picker).
pub struct OneshotPickerApp {
    /// The wrapped tasks app, locked to the pending view.
    inner: TasksApp,
}

impl OneshotPickerApp {
    pub async fn new(config: Config, fullscreen: bool) -> Result<Self> {
        let _ = GLOBAL_CONFIG.set(config.clone());
        let inner = TasksApp::new(
            ViewMode::PendingTasks,
            config,
            ViewVariant::default(),
            fullscreen,
        )
        .await;
        Ok(Self { inner })
    }

    /// Run the picker. Returns the accepted task's (stable id, short id),
    /// or `None` when the pick is cancelled (Esc / ctrl-c).
    pub async fn run(self) -> Result<Option<(i64, Option<i64>)>> {
        let (mut render_cfg, mut binds, mut tui_cfg, overlay_cfg) = get_mm_cfg();
        // The picker is full-width: no preview pane.
        render_cfg.preview.show = false.into();
        if self.inner.fullscreen {
            tui_cfg.layout = None;
        }
        // The datetime column (visible index 1) is fixed at 8 wide, like
        // the tasks view.
        render_cfg.results.width_overrides = vec![0, 8, 0];

        // Enter picks: the builtin Accept fires the accept hook and
        // finishes the pick. The mode-cycle bind is dropped — the picker
        // never shows the done view.
        binds.remove(&key!(enter).into());
        binds.remove(&key!(tab).into());
        binds.insert(key!(enter).into(), Actions::from(MMAction::Accept));

        let view = Arc::new(Mutex::new(self.inner));
        let worker = Worker::new(
            // The default column: label (index 2).
            task_columns(&view),
            2,
        );

        let mut mm = Matchmaker::new(worker, tasks_accept_hook);
        mm.config_render(render_cfg);
        mm.config_tui(tui_cfg);

        let confirm_ov = Arc::new(Mutex::new(ConfirmOverlay::<TaskRow>::new()));
        let input_ov = Arc::new(Mutex::new(InputOverlay::<TaskRow>::new()));

        let mut options = PickOptions::new()
            .binds(binds)
            .overlay_config(overlay_cfg)
            .overlay(SharedOverlay::new(confirm_ov.clone()))
            .overlay(SharedOverlay::new(input_ov.clone()));

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

        // External editor (Edit on a task row): same flow as the tasks view.
        mm.register_interrupt_handler(Interrupt::Execute, {
            let ctx = ctx.clone();
            move |_state| run_edit(&ctx)
        });

        options = options.ext_handler(move |action: ImAction, state| {
            // The picker never cycles to the done view, even if the config
            // file binds a key to CycleMode.
            if matches!(action, ImAction::CycleMode) {
                return;
            }
            handler(action, state, &ctx);
        });

        match mm.pick(options).await {
            Ok(v) => Ok(v.into_iter().next()),
            Err(MatchError::NoMatch) | Err(MatchError::Abort(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
