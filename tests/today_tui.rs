//! Integration tests for the today-view TUI.
//!
//! The TUI runs headless through matchmaker's test support:
//! `IoStream::Test` captures every rendered frame into
//! [`matchmaker::test::TEST_BUFFER`] (no real terminal — raw mode and
//! terminal sizing are bypassed), and an input-less event loop
//! (`EventLoop::as_optional`) never reads keyboard input. The test quits
//! the picker by pushing a custom `Action::Quit` through the render
//! channel after the first frame that contains the expected rows.

use im::{
    config::Config,
    db::test_pool,
    types::{TodayHorizon, ViewVariant},
    ui::today::{TodayApp, TodayRunCfg},
};
use matchmaker::{
    action::Action as MMAction, config::TerminalConfig, event::EventLoop, message::RenderCommand,
    tui::IoStream,
};
use ratatui::layout::Rect;
use sqlx::SqlitePool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Serialize headless TUI tests: they all share the global capture buffer.
static TUI_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Seed a mood row "today" (within `TodayHorizon::Today`).
async fn seed_mood(pool: &SqlitePool, mood: &str, body: &str) {
    sqlx::query("INSERT INTO mood (mood, body, time) VALUES (?, ?, ?)")
        .bind(mood)
        .bind(body)
        .bind(im::date::today_start() + 3600)
        .execute(pool)
        .await
        .unwrap();
}

/// Build the headless run config: capture backend, input-less loop, and a
/// quit trigger that fires once the buffer shows `marker`. For color
/// assertions the marker is the badge's 24-bit SGR prefix — the fill task
/// colors moods in the background, so the colored frame arrives only after
/// the fill's redraw.
fn headless_cfg(marker: &'static str) -> TodayRunCfg {
    TodayRunCfg {
        tui: Some(TerminalConfig {
            stream: IoStream::Test,
            ..Default::default()
        }),
        event_loop: Some(EventLoop::new().as_optional()),
        on_start: Some(Box::new(move |tx| {
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    if matchmaker::test::contents().contains(marker) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                let _ = tx.send(RenderCommand::Action(MMAction::Custom(
                    im::ui::action::ImAction::Quit,
                )));
            });
        })),
    }
}

/// Last line in the capture buffer containing `needle` (frames accumulate
/// in the buffer, so the first match may be a pre-fill frame).
fn captured_line(needle: &str) -> String {
    let buffer = matchmaker::test::contents();
    buffer
        .lines()
        .rev()
        .map(str::trim_end)
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| {
            panic!("no captured line contains {needle:?}.\n--- captured buffer ---\n{buffer}")
        })
        .to_string()
}

/// The today-view row content reaches the terminal: the badge glyph and
/// the label text render in the same row. Regression: rows could render
/// with no content when the view started.
#[tokio::test]
// The guard intentionally spans the awaits: headless TUI tests share the
// global capture buffer and must run one at a time.
#[allow(clippy::await_holding_lock)]
async fn today_tui_row_content_renders() {
    let _guard = TUI_LOCK.lock().unwrap();
    matchmaker::test::clear();

    let pool = test_pool().await.unwrap();
    im::global::set_pool(pool.clone());
    seed_mood(&pool, "sad", "").await;

    let config = Config::default();
    let app = TodayApp::new(config, None, ViewVariant::All, TodayHorizon::Today, false).await;
    // Quit once the mood row shows up.
    app.run_with(headless_cfg("sad")).await.unwrap();

    let line = captured_line("sad");
    // Badge glyph and label text, same row (no formatting assertions).
    let plain = strip_ansi(&line);
    assert!(
        plain.contains('●') && plain.contains("sad"),
        "row content missing: {plain:?}"
    );
}

/// Strip CSI sequences (`\x1b[...final-byte`) so row layout can be
/// asserted on the visible text alone.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for d in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&d) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Convert CSI codes to readable text: cursor position codes (H/f) become
/// newlines, everything else is dropped.
// fn csi_to_lines(s: &str) -> Vec<String> {
//     let mut out = String::new();
//     let mut chars = s.chars().peekable();
//     while let Some(c) = chars.next() {
//         if c == '\x1b' && chars.peek() == Some(&'[') {
//             chars.next();
//             for d in chars.by_ref() {
//                 if ('\u{40}'..='\u{7e}').contains(&d) {
//                     if d == 'H' || d == 'f' {
//                         out.push('\n');
//                     }
//                     break;
//                 }
//             }
//         } else {
//             out.push(c);
//         }
//     }
//     out.lines().map(|l| strip_ansi(l.trim_end())).collect()
// }

/// Reconstruct the last screen state from the raw ANSI capture: the diff
/// writer skips unchanged cells (e.g. the spaces inside the ui border
/// title), so rows written in multiple segments — `┌Today`, `[sort:`, … —
/// must be reassembled by absolute cursor position. `2J` clears the grid;
/// cursor-home codes (`H`/`f`) move the write position; all other CSI
/// sequences are dropped.
fn screen_rows(raw: &str) -> Vec<String> {
    let mut grid: Vec<Vec<char>> = Vec::new();
    let (mut row, mut col) = (0usize, 0usize);
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            for d in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&d) {
                    if d == 'H' || d == 'f' {
                        let mut parts = params.split(';');
                        let r: usize = parts
                            .next()
                            .filter(|p| !p.is_empty())
                            .map(|p| p.parse().unwrap_or(1))
                            .unwrap_or(1);
                        let c: usize = parts
                            .next()
                            .filter(|p| !p.is_empty())
                            .map(|p| p.parse().unwrap_or(1))
                            .unwrap_or(1);
                        row = r.saturating_sub(1);
                        col = c.saturating_sub(1);
                    } else if d == 'J' && params == "2" {
                        grid.clear();
                        row = 0;
                        col = 0;
                    }
                    break;
                }
                params.push(d);
            }
        } else if c != '\n' && c != '\r' {
            while grid.len() <= row {
                grid.push(Vec::new());
            }
            while grid[row].len() < col {
                grid[row].push(' ');
            }
            if grid[row].len() == col {
                grid[row].push(c);
            } else {
                grid[row][col] = c;
            }
            col += 1;
        }
    }
    grid.into_iter().map(|r| r.into_iter().collect()).collect()
}

/// The ui border (menu) title renders the same label on the top border
/// line, prefixed by the `┌` corner glyph.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn today_tui_menu_title() {
    let _guard = TUI_LOCK.lock().unwrap();
    matchmaker::test::clear();

    let pool = test_pool().await.unwrap();
    im::global::set_pool(pool.clone());
    seed_mood(&pool, "sad", "").await;
    let config = Config::default();
    let app = TodayApp::new(config, None, ViewVariant::All, TodayHorizon::Today, false).await;
    // Quit on any rendered frame (the title is set by the initializer).
    app.run_with(headless_cfg("Today")).await.unwrap();

    let rows = screen_rows(&matchmaker::test::contents());
    let title = rows
        .iter()
        .find(|l| l.starts_with('┌') && l.contains("[sort:") && l.contains("[show:"))
        .unwrap_or_else(|| panic!("menu title not captured: {rows:?}"));
    // The top border line is `┌<title>────┐…`; the title ends where the
    // horizontal fill begins.
    let title_text = title
        .trim_start_matches(['┌', ' '])
        .split('─')
        .next()
        .unwrap()
        .trim();
    assert_eq!(
        title_text,
        "Today [sort: time] [show: all]",
        "menu title must render the full header label: {title:?}"
    );
}

/// alt-h renders the keybinding help in the preview pane (the builtin
/// `Action::Help` stages a PreviewSet payload that the view's handler
/// renders from the event loop's live bind map); a second alt-h toggles
/// back to the cursor-tracked entry preview.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn today_tui_alt_h_help_toggles() {
    let _guard = TUI_LOCK.lock().unwrap();
    matchmaker::test::clear();

    let pool = test_pool().await.unwrap();
    im::global::set_pool(pool.clone());
    seed_mood(&pool, "sad", "").await;
    let config = Config::default();
    let app = TodayApp::new(config, None, ViewVariant::All, TodayHorizon::Today, false).await;

    // The on_start task records the last screen at each stage; the
    // assertions run after run_with returns so a failed stage can never
    // strand the picker without its quit action.
    let help_screen = Arc::new(Mutex::new(Vec::<String>::new()));
    let toggled_screen = Arc::new(Mutex::new(Vec::<String>::new()));
    let help_screen_inner = help_screen.clone();
    let toggled_screen_inner = toggled_screen.clone();

    let cfg = TodayRunCfg {
        tui: Some(TerminalConfig {
            stream: IoStream::Test,
            ..Default::default()
        }),
        event_loop: Some(EventLoop::new().as_optional()),
        on_start: Some(Box::new(move |tx| {
            let help_screen = help_screen_inner.clone();
            let toggled_screen = toggled_screen_inner.clone();
            tokio::spawn(async move {
                // Wait for the item to be matched and rendered, then open the help.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
                while tokio::time::Instant::now() < deadline {
                    if matchmaker::test::contents().contains("sad") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                let _ = tx.send(RenderCommand::Action(MMAction::Help(String::new())));

                // The help renders as `<key> = <action>` lines in the
                // preview pane (the help is sorted by action, so the
                // top row is a stable anchor).
                let mut rows = Vec::new();
                while tokio::time::Instant::now() < deadline {
                    rows = screen_rows(&matchmaker::test::contents());
                    if rows.iter().any(|l| l.contains("Alt-Enter = Accept")) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                *help_screen.lock().unwrap() = rows;

                // Toggle off: the preview falls back to the entry preview
                // (the MOOD heading), and the help text disappears.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
                let _ = tx.send(RenderCommand::Action(MMAction::Help(String::new())));
                let mut rows = Vec::new();
                while tokio::time::Instant::now() < deadline {
                    rows = screen_rows(&matchmaker::test::contents());
                    if !rows.iter().any(|l| l.contains("Alt-Enter")) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                *toggled_screen.lock().unwrap() = rows;

                let _ = tx.send(RenderCommand::Action(MMAction::Custom(
                    im::ui::action::ImAction::Quit,
                )));
            });
        })),
    };

    app.run_with(cfg).await.unwrap();

    let help = help_screen.lock().unwrap();
    assert!(
        help.iter().any(|l| l.contains("Alt-Enter = Accept")),
        "alt-h help not rendered: {help:?}"
    );
    let toggled = toggled_screen.lock().unwrap();
    assert!(
        !toggled.iter().any(|l| l.contains("Alt-Enter")),
        "alt-h help must toggle off: {toggled:?}"
    );
    assert!(
        toggled.iter().any(|l| l.contains("MOOD")),
        "entry preview must return after the help toggle: {toggled:?}"
    );
}

/// Shrinking the window to very small sizes must not panic: the results
/// width allocation is re-derived at the new width (regression for the
/// stale `widths_buffer` overshoot in matchmaker's
/// `ResultsUI::update_dimensions`).
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn today_tui_resize_shrinks_to_small_window() {
    let _guard = TUI_LOCK.lock().unwrap();
    matchmaker::test::clear();

    let pool = test_pool().await.unwrap();
    im::global::set_pool(pool.clone());
    seed_mood(&pool, "sad", "").await;
    seed_mood(&pool, "okay", "").await;
    let config = Config::default();
    let app = TodayApp::new(config, None, ViewVariant::All, TodayHorizon::Today, false).await;

    let cfg = TodayRunCfg {
        tui: Some(TerminalConfig {
            stream: IoStream::Test,
            ..Default::default()
        }),
        event_loop: Some(EventLoop::new().as_optional()),
        on_start: Some(Box::new(move |tx| {
            tokio::spawn(async move {
                // Sweep through small sizes, one resize per frame-ish.
                for (w, h) in [(60, 20), (40, 12), (30, 10), (24, 8), (18, 6), (12, 5), (8, 4)] {
                    let _ = tx.send(RenderCommand::Resize(Rect::new(0, 0, w, h)));
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = tx.send(RenderCommand::Action(MMAction::Custom(
                    im::ui::action::ImAction::Quit,
                )));
            });
        })),
    };

    app.run_with(cfg).await.unwrap();
}
