//! In-process `Interrupt::Execute` dispatch.
//!
//! Matchmaker suspends the picker and restores the terminal before running
//! the interrupt handler, so these helpers can run the same interactive flows
//! the CLI uses. Async flows are driven with [`block_on`]; their outcome (or
//! failure) becomes a footer toast. `discriminant_payload` says which flow
//! the render thread staged; the interrupt payload carries its argument.

use std::future::Future;

use matchmaker::{message::Interrupt, render::MMState};

use crate::cli::CliOpts;
use crate::db::Id;
use crate::global::{config, pool};
use crate::ui::action::ExecuteAction;
use crate::ui::toast::{self, Level};

/// Stage an execute flow: set the discriminant and raise the interrupt.
pub(crate) fn raise<T: matchmaker::SSS, D>(
    state: &mut MMState<'_, T, D>,
    action: ExecuteAction,
    payload: String,
) {
    state.discriminant_payload = Some(action as u8);
    state.set_interrupt(Interrupt::Execute, payload);
}

/// Consume the staged discriminant. Unknown/absent values are no-ops.
pub(crate) fn take<T: matchmaker::SSS, D>(state: &mut MMState<'_, T, D>) -> Option<ExecuteAction> {
    match state.discriminant_payload.take()? {
        0 => Some(ExecuteAction::EditBody),
        1 => Some(ExecuteAction::EditTask),
        2 => Some(ExecuteAction::Create),
        3 => Some(ExecuteAction::Sync),
        _ => None,
    }
}

/// Run the `im +<task>` field editor for `id` and toast its outcome.
pub(crate) fn edit_task<T: matchmaker::SSS, D>(
    state: &mut MMState<'_, T, D>,
    fullscreen: bool,
    id: Id,
) {
    toast::clear(state);
    let opts = CliOpts {
        fullscreen,
        ..Default::default()
    };
    let pool = pool();
    match block_on(|| crate::commands::edit_task_by_id(&pool, config(), &opts, id)) {
        Ok(Some(msg)) => toast::set(state, Level::Success, msg),
        Ok(None) => {}
        Err(e) => toast_error(state, &e),
    }
}

/// Run the creation menu and toast its outcome.
pub(crate) fn create<T: matchmaker::SSS, D>(
    state: &mut MMState<'_, T, D>,
    fullscreen: bool,
    include_entries: bool,
) {
    toast::clear(state);
    let opts = CliOpts {
        fullscreen,
        ..Default::default()
    };
    let pool = pool();
    match block_on(|| crate::commands::create_interactive(&pool, config(), &opts, include_entries))
    {
        Ok(Some(msg)) => toast::set(state, Level::Success, msg),
        Ok(None) => {}
        Err(e) => toast_error(state, &e),
    }
}

/// Run `im :sync` and toast its outcome.
pub(crate) fn sync<T: matchmaker::SSS, D>(state: &mut MMState<'_, T, D>) {
    toast::clear(state);
    let pool = pool();
    match block_on(|| crate::commands::sync_command(&pool, config(), None, false, false)) {
        Ok(()) => toast::set(state, Level::Success, "Sync complete"),
        Err(e) => toast_error(state, &e),
    }
}

/// Drive a future to completion while the event loop is suspended. The
/// handler is synchronous inside a tokio task, so a multi-thread runtime is
/// stepped aside with `block_in_place`; headless tests use a current-thread
/// runtime, where that would panic, so their flows run on a scratch runtime.
fn block_on<F, Fut>(make: F) -> Fut::Output
where
    F: FnOnce() -> Fut + Send,
    Fut: Future,
    Fut::Output: Send,
{
    let handle = tokio::runtime::Handle::current();
    if matches!(
        handle.runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread
    ) {
        tokio::task::block_in_place(|| handle.block_on(make()))
    } else {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("execute flow runtime")
                        .block_on(make())
                })
                .join()
                .expect("execute flow panicked")
        })
    }
}

fn toast_error<T: matchmaker::SSS, D>(state: &mut MMState<'_, T, D>, error: &anyhow::Error) {
    if is_cancel(error) {
        return;
    }
    toast::set(state, Level::Error, one_line(error));
}

/// cliclack reports Esc/Ctrl-C as an `io::ErrorKind::Interrupted`; prompts
/// wrap that as "Prompt cancelled: …", while direct `?` paths keep the raw
/// error. A cancelled flow is not an error to report.
fn is_cancel(error: &anyhow::Error) -> bool {
    error.to_string().starts_with("Prompt cancelled")
        || error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
            .any(|e| e.kind() == std::io::ErrorKind::Interrupted)
}

fn one_line(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}
