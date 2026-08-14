//! Lightweight previewer: owns the preview content as a shared
//! `Text<'static>` that matchmaker's `Preview` widget renders every frame.
//! The UI event listener calls [`Previewer::update_today`] /
//! [`Previewer::update_task`] with the current item (cloned once at the
//! call site); each call bumps an internal generation counter — dropping
//! stale in-flight computes — and spawns the async build, which writes the
//! shared string only when its generation is still current.

use matchmaker::preview::{AppendOnly, Preview};
use ratatui::text::Text;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::global::{config, pool};
use crate::color::ColorAxes;
use crate::db::TaskRow;
use crate::today::TodayEntry;
use crate::ui::preview::{build_preview, build_today_preview};

/// The previewer shared between the render thread (via [`Self::view`]) and
/// the UI event listener (which owns it). The config and pool come from
/// [`crate::global`] (stored by the app constructors / `run_app`); the
/// color model is threaded in.
#[derive(Clone)]
pub struct Previewer {
    /// The built mood-color model (`MoodConfig::init_with`), used by the
    /// preview's linked-mood backfill.
    axes: ColorAxes,
    /// The preview content the `Preview` widget renders; `None` = blank.
    string: Arc<Mutex<Option<Text<'static>>>>,
    /// Bumped on every update/stop; in-flight computes check their captured
    /// value before writing, so stale results never overwrite newer ones.
    generation: Arc<AtomicU64>,
}

impl Previewer {
    pub fn new(axes: ColorAxes) -> Self {
        Self {
            axes,
            string: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The widget to attach via `PickOptions::preview`. The append-only
    /// lines stay empty (the string override wins); the changed flag is a
    /// no-op — matchmaker re-reads the string on every frame.
    pub fn view(&self) -> Preview {
        Preview::new(
            AppendOnly::new(),
            self.string.clone(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// Show fixed text in the preview pane (e.g. the alt-h keybinding
    /// help), replacing the item preview until the next update/stop.
    /// Bumps the generation counter so in-flight item computes don't
    /// overwrite it.
    pub fn set_text(&self, text: Text<'static>) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.string.lock().unwrap() = Some(text);
    }

    /// Blank the preview and invalidate in-flight computes.
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.string.lock().unwrap() = None;
    }

    /// Kickstart a preview compute for a today-view entry.
    pub fn update_today(&self, entry: TodayEntry) {
        let my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let axes = self.axes.clone();
        let string = self.string.clone();
        let generation = self.generation.clone();
        tokio::spawn(async move {
            let lines = if let Some(task_id) = entry.task_id {
                let task_opt = if let Some(w) = &entry.recurring_window {
                    Some(w.task.clone())
                } else {
                    crate::db::fetch_task_by_id(&pool(), task_id, crate::date::now())
                        .await
                        .ok()
                        .flatten()
                };
                let linked_moods = crate::db::fetch_linked_moods(&pool(), task_id)
                    .await
                    .unwrap_or_default();
                let tree = crate::task_tree::TaskTree::load(&pool(), task_id)
                    .await
                    .ok()
                    .flatten();
                // The parent row (the preview `parent:` field) — fetched
                // alongside the tree when the task is attached to one.
                let parent = match task_opt.as_ref().and_then(|t| t.parent) {
                    Some(pid) => crate::db::fetch_task_by_id(&pool(), pid, crate::date::now())
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                };
                if let Some(task) = task_opt {
                    build_preview(
                        &task,
                        true,
                        config(),
                        &linked_moods,
                        Some(&axes),
                        parent.as_ref(),
                        tree.as_ref(),
                    )
                } else {
                    build_today_preview(&entry, config())
                }
            } else {
                build_today_preview(&entry, config())
            };
            if generation.load(Ordering::SeqCst) == my_gen {
                *string.lock().unwrap() = Some(Text::from(lines));
            }
        });
    }

    /// Kickstart a preview compute for a tasks-view row.
    pub fn update_task(&self, task: TaskRow) {
        let my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let axes = self.axes.clone();
        let string = self.string.clone();
        let generation = self.generation.clone();
        tokio::spawn(async move {
            let linked_moods = crate::db::fetch_linked_moods(&pool(), task.id)
                .await
                .unwrap_or_default();
            let tree = crate::task_tree::TaskTree::load(&pool(), task.id)
                .await
                .ok()
                .flatten();
            // The parent row (the preview `parent:` field) — fetched
            // alongside the tree when the task is attached to one.
            let parent = match task.parent {
                Some(pid) => crate::db::fetch_task_by_id(&pool(), pid, crate::date::now())
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let lines = build_preview(
                &task,
                false,
                config(),
                &linked_moods,
                Some(&axes),
                parent.as_ref(),
                tree.as_ref(),
            );
            if generation.load(Ordering::SeqCst) == my_gen {
                *string.lock().unwrap() = Some(Text::from(lines));
            }
        });
    }
}
