# im — Architecture

A CLI + TUI journaling and task-tracking tool. Mood entries (optionally journal
bodies), numeric trackers, oneshot/recurring/scheduled tasks, all stored
in a single SQLite database. Terminal output is tab-separated and parseable; the
same views run as a fullscreen ratatui TUI when stdout is a TTY.

---

this project is in progress and we don't need to worry about migrations or
breaking changes

Keep _dbg!: it only emits in debug builds.

removed features are treated as if they never existed: no tests, no mentions
in this document, no changelog rows

when running tests, use --test-threads=4

Don't update help.txt

use macros from cba crate (_ebog, ebog!) for logging **when outside the TUI loop**, for calls made while the tui is running, use log (i.e. log::error) for logging.

Day/horizon/interval computation is calendar-aware (jiff), so DST transitions
and variable month lengths are handled correctly; see "4. Date & duration".

## 1. Crate layout

The crate is a **library + thin binary** so that integration tests can import it
as `im::`.

```text
src/
  lib.rs        library façade; main.rs remains the thin binary entry point
  types.rs      shared cross-layer contracts: input payloads, TaskKind,
                ViewMode, ViewVariant, TodayHorizon
  paths.rs      XDG-style paths (config dir, state dir, db, log)
  logger.rs     cba `bog`/env_logger setup
  prompts.rs    interactive cliclack prompts

  cli/          handwritten CLI grammar (no clap crate)
    mod.rs      CliOpts, Command, CLI-only tracker syntax types
    parser.rs   parse_args, parse_cli, parse_from and parser tests
    parse/      entry, task, view, update, tracker, and special grammars

  commands/     command orchestration and write flows
    mod.rs      execute_command and TTY/non-TTY dispatch
    entry.rs    mood/tracker entry writes
    task.rs     oneshot, recurring, and scheduled creation
    update.rs   task completion updates
    maintenance.rs  clear, :db prune/:db backfill/:db doctor, config/moods editing
    diagnostics.rs  :embed and :color utilities

  config/       serde configuration sections
    mod.rs      Config and normalization
    types.rs    mood endpoints, palettes, tracker kinds
    moods.rs    mood/color settings and model initialization
    tasks.rs    task defaults and badge colors
    trackers.rs tracker settings
    views.rs    grid, preview, today, editor, and task-view options

  db/           SQLite pool, schema, models, and feature queries
    mod.rs      database bootstrap plus public query façade
    models.rs   persistence/query result types
    tasks.rs    task CRUD, identity, completion, and task reads
    entries.rs  mood/tracker entries and mutations
    views.rs    today/task-view query composition
    embeddings.rs embedding cache

  date/         all date/time usage lives here (jiff internally; English
                 parsing via the jiff-english subcrate, a chrono-english port)
    mod.rs      Epoch type and time boundaries
    parse.rs    datetime parsing
    parse_duration.rs  duration parsing (fixed seconds + calendar spans)
    span.rs     DbSpan pack/unpack and calendar interval math
    format.rs   date/time/duration formatting

  today.rs      TodayItem model, today query assembly, sorting, plain output
  tracker.rs    tracker-grid calculations and non-TUI tracker output
  task_view.rs  non-TUI task-list output and task-view ordering
  output/       non-TUI formatting for entries, tasks, and today rows
  task/         shared task behavior
    mod.rs      façade and colocated behavior tests
    completion.rs  completion deltas and done state
    scheduling.rs  interval/window and task sort timestamps
    actions.rs     Accept-action state machine and persistence wrappers
  task_tree.rs  parent/child task tree loading and rendering

  embedding.rs  nomic-embed-text-v1.5 ONNX embeddings and saliency helpers
  percentage.rs bounded Percentage value type
  color/        Oklab mood-color projection
    mod.rs      ColorAxes pipeline
    nnls.rs     non-negative least squares
    blend.rs    Oklab blending helpers
    conversion.rs  RGB ↔ Oklab conversion

  ui/           interactive terminal UI (kept separate from feature queries)
    mod.rs      Render lifecycle and shared TUI façade
    action.rs   Action enum
    bindings.rs key bindings
    events.rs   ui/control channels
    event_loop.rs crossterm input parser
    terminal.rs fullscreen terminal wrapper
    suspend.rs  external-editor suspension
    common.rs   shared TUI widgets/helpers
    modal.rs    shared modal payloads
    preview.rs  task and today previews; task context remains a bool for now
    tasks.rs    TasksApp state/actions/drawing
    today.rs    TodayApp state/actions/drawing
```

**main.rs** is thin: init bogger → `parse_args()` → init logger → load config
(`config.init()`) → open the DB pool → `tui = atty::is(Stdout)` (the single TUI
decision point) → `execute_command(cmd, pool, config, opts, out, tui)`.
`execute_command` takes an explicit `tui: bool`, so the TUI never auto-launches
from inside the library — integration tests always pass `false` and exercise
the plain output. Leading `-q`/`-v` counts gate both the logger and
handler-level output (confirmations, grid titles).

**User-facing logging**: terminal output goes through the `cba` bogger macros
(`ebog!`/`wbog!`/`ibog!`, printed to stdout). `log::*` is piped to the **log
file only** (`Target::Pipe`) and must not be used for user-facing messages.
Exceptions: interactive prompts and TUIs write to stderr (cliclack), and
`:color` writes to the caller's `out`.

---

## 2. Paths & config

**paths.rs** — `IM_CONFIG_DIR` env var overrides the config dir. Config:
`~/.config/im/dev.toml` in debug builds, `config.toml` in release. State dir:
`dirs::state_dir()/im` with `im.db` and `im.log`.

**config.rs** — `Config` (serde, `deny_unknown_fields`, per-field defaults).
Mood anchors live in a separate moods file named by `[moods] source`
(default: the bundled `assets/moods.toml`); `im :moods` opens it in `$EDITOR`.
`Config::init` (called from main) drops tracker names that collide with CLI
syntax and falls back to the default palette when `tasks.colors` has fewer
than 3 entries.

---

## 3. Database schema

`db.rs` runs `CREATE TABLE IF NOT EXISTS` — **no ALTER migrations** (schema
changes mean deleting the dev DB; test DBs are in-memory). `PRAGMA
foreign_keys = ON` everywhere; `journal_mode` is `WAL` in release and `DELETE`
in debug (no `-wal`/`-shm` sidecars during dev; setting it explicitly also
converts a pre-existing WAL db).

```text
mood:           id, mood TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
                time INTEGER (unixepoch), embedding BLOB, score REAL
tracker:        id, type TEXT, score BLOB NOT NULL        -- storage class = declared kind
                CHECK (typeof(score) IN ('integer','text','real')),
                time, mood INTEGER → mood(id)             -- nullable
todos:          id (AUTOINCREMENT), name TEXT NOT NULL, body, priority DEFAULT 5,
                short_id INTEGER UNIQUE,   -- user-facing id; first-free-gap,
                                           -- NULL once a oneshot task is done
                name_embedding BLOB,       -- reserved; never populated
                start_time, available_duration_secs, interval_secs, target_count DEFAULT 0,
                -- interval_secs stores a packed DbSpan (NOT seconds) — a jiff
                -- Span with calendar units, unpacked via date::db_to_span
                optional DEFAULT 0, end_time, parent → todos(id) ON DELETE SET NULL
                -- deleting a parent re-parents its children to root
todo_completions: id, todo_id → todos(id) ON DELETE CASCADE,
                time INTEGER, count INTEGER NOT NULL DEFAULT 1
task_moods:     todo_id → todos(id) ON DELETE CASCADE,
                mood_id → mood(id) ON DELETE CASCADE     -- `im <mood> -<short id>`; today-view Ctrl+L prompt
embedding_cache: text TEXT PRIMARY KEY, embedding BLOB
```

Timestamps are Unix epoch seconds everywhere, via the `date` module.

- **Completion semantics**: `todo_completions` is an event log — one row per
  update, `count` = that update's value; totals are `SUM(count)`. Done-ness and
  deltas are pure, unit-tested `task`-layer functions (`is_task_done`,
  `apply_completion_delta`). Negative deltas are consumed at write time from
  recent entries backwards, so no negative rows are ever stored and totals
  floor at 0. Recurring tasks scope sums to the current calendar interval
  (`current_interval_start`); scheduled tasks keep at most one completion row
  (`1` = done, `0` = failed), replaced in a transaction.
- **Short ids**: `todos.id` is a stable `AUTOINCREMENT` PK; the user-facing
  `short_id` is the first free positive gap. Completing a oneshot task clears
  its `short_id` (recycled on reset); recurring tasks keep theirs (done-ness
  is interval-scoped and transient). Allocation is a pure read; the UNIQUE
  column makes concurrent double-allocation fail loudly at INSERT.

---

## 4. Date & duration parsing (date/)

All parsing lives in `date/`; callers only see `Epoch` (i64 epoch seconds),
duration seconds, packed `DbSpan`s, and `jiff::Span`s. Internal math runs on
jiff with the local time zone.

- **Datetime parsing** (`parse.rs`): `parse_datetime` via the `jiff-english`
  subcrate (a chrono-english port on jiff), anchored at `Zoned::now()`, with
  the compile-time `DATE_DIALECT` constant; `parse_date` additionally
  day-aligns (backs `im @<date>`).
- **Durations** (`parse_duration.rs`): `humantime` for fixed windows;
  `parse_span` parses calendar intervals ("1 day", "1 month") for recurring
  tasks and trackers.
- **Intervals** (`span.rs`): `DbSpan` packs a jiff `Span` into one i64 for
  storage; `current_interval_start_zoned` computes DST-safe calendar interval
  boundaries; `interval_index` numbers intervals.
- **Formatting** (`format.rs`): jiff strtime — times, ISO dates/datetimes,
  humantime durations.
- **Boundaries** (`mod.rs`): `now`, day/week/month/year starts, rolling
  variants (`rolling_month_start`, `aligned_year_start`).

---

## 5. CLI parsing (cli/)

Manual parser, no clap crate. `parse_args()` (from `env::args`) and
`parse_from(Vec<String>)` (unit-testable). A leading `-q`/`-v` flag run is
stripped into `CliOpts { qv: [u8; 2] }` counts; `-h`/`--help` in the initial
position short-circuits to `Command::Help`; after the first non-flag token
everything is command text. `parse_from` dispatches on `args[0]`:

| Input | Command |
| --- | --- |
| (no args) | `Today { date: None }` — today view |
| `@<date>` | `Today { date: Some(date) }` — today view anchored to that day (parsed by the handler with `DATE_DIALECT`) |
| `--help` / `-h` | `Help` — bundled `assets/help.txt` via `include_str!` |
| plain words (`happy`, `good ...`) | `Entry { mood, trackers, .. }` — mood entry; trackers as `-type score` |
| `..` (bare, at the end) | opens the body editor |
| `!` (bare) | `Task { OneShot, name: None, body: None }` — interactive oneshot creation |
| `! <name> [@date] [..]` | `Task { OneShot, .. }` — `@YYYY-MM-DD` is the **due** time (`end_time`); a second `@`-word is rejected |
| `! -<parent_id> [name] [@time]` | `Task { OneShot, parent }` — attached to the task whose short id is `<parent_id>` (resolved in the handler) |
| `! @` / `! @ <name>` | `Task { Recurring, prefill }` — interactive recurring creation |
| `! @<time> [:name] [%<duration>] [.. [body]]` | `Task { Scheduled, .. }` — scheduled creation; immediate when all fields came from the CLI, else interactive. The space discriminator is load-bearing: `! @ 10pm` is recurring, `! @10pm` is scheduled |
| `@[:o\|:O]` | `View { PendingTasks, show }` — pending tasks (all / oneshots only / recurring+scheduled) |
| `@done[:o\|:O]` | `View { DoneTasks, show }` — completed tasks |
| `@due[:t\|:w]` | `Today { date: None, show: B, horizon: Today\|Tomorrow\|Week }` — today view, tasks only |
| `- id [count]` | `Update { OneShot(id), count }` — `id` is the user-facing short id; `count` may be negative |
| `- words… [count]` | `Update { Query(words), count }` — unique oneshot task whose name contains the words in order |
| `-` alone | `TasksEdit` — stub ("not yet implemented") |
| `:` / `:week\|month\|year` / `:` + ids | `Tracker { period, items }` — dot-sequence tracker views (`:g` bails "not yet implemented") |
| `:embed` | `Embed` — stdin lines → one 768-dim vector per line |
| `:score "start" "end"` | `Score` — stub (`todo!()`) |
| `:config` | `Config` — opens the live config in `$VISUAL`/`$EDITOR` |
| `:db prune` | `Db { Prune }` — prunes expired/completed tasks, clears the embedding cache |
| `:db backfill` | `Db { Backfill }` — computes and persists missing mood embeddings + saliency scores |
| `:db doctor` | `Db { Doctor }` — prunes tracker entries whose storage class no longer matches the configured kind |
| `:color <mood>` | `Color` — full mood-color pipeline diagnostic |
| `:clear [@date]` | `Clear` — deletes that day's mood entries (interactive confirm) |

Tracker names cannot begin with `@` (reserved for recurring ids). Tabs in
mood/name fields are an error (output is tab-separated).

---

## 6. Handlers (commands/)

`execute_command<W: Write>(cmd, pool, config, opts, out, tui)` matches the
`Command` enum; `opts` gates confirmations and verbose output.

- **Entry** — transactional mood (+ tracker) insert. Trackers parse against
  their declared kind (text/integer/float/duration/null) with clear errors;
  every kind takes a strict value form (plain number, whole number, duration
  string, verbatim, none) and `strict = true` gates the raw value against
  `low`/`high` before the insert (null trackers gate the entry time against
  the circular offset zone). Replace mode (`interval` + `cumulative: false`)
  is one shared insertion strategy for all kinds: the slot's previous rows
  are dropped, then the new row is inserted. `-<short id>` tokens link the
  entry to a task. Non-empty moods are embedded and saliency-scored
  **before** the transaction opens.
- **Task** — oneshot `! <name> [@<time>] [.. [body]]` validates name/date and
  resolves the body (`.. text` as-is, bare `..` opens the editor, none = no
  body). Bare `!`, `! @` (recurring) and incomplete `! @<time>` (scheduled)
  run interactive cliclack flows that bail on non-interactive stdin.
- **Update** — applies the completion delta (interval-aware for recurring) and
  prints the new total; `- <id>` uses the short id, `- words…` a subsequence
  name match. Completed tasks have no short id and are not addressable by it.
- **Today / View** — TUI apps when `tui`, else plain writers
  (`today::write_today_view`, `task_view::write_task_view`).
- **Tracker** — `tracker::write_tracker_grid` (no TUI path yet).
- **Db** — `:db prune` (completed/expired tasks + embedding cache),
  `:db backfill` (missing embeddings/scores), `:db doctor` (storage-class
  mismatches; interactive confirm).
- **Clear** — deletes a day's mood/tracker entries (interactive confirm).
- **Color / Embed** — pipeline diagnostic / stdin-lines embedding dump;
  **Score** — stub.

---

## 7. Views & output

View composition — variants, filters, horizons, sort keys, and the plain
tab-separated row formats — is specified in **docs/VIEWS.md**. The TUI and the
plain text writers consume the same `TodayEntry`s, so both renderers share the
exact badge/color logic. The completion badge (§8) is the single
completion-status renderer.

---

## 8. Completion badge

`views::completion_badge(config, count, target_count)` (plus
`completion_badge_text`) is the **single** completion-status display used
everywhere — CLI task lists, the TUI table, the preview pane, the recurring
tracker:

| state | render |
| --- | --- |
| no entries / interval sum 0 (0%) | `◯` (U+25EF, uncolored — `Color::Reset`, no ANSI emitted) |
| 100% (`count >= target`, or any `count > 0` when `target_count <= 0`) | `●` in `colors.last()` (no `DONE` word) |
| in between (`0 < count < target`) | `●` binned over `colors[..len-1]` — text `● n/m` |

`target_count = 0` never shows an `n/m` fraction. Binning is **discrete only —
no blending** (the continuous Oklab mood projection is the deliberate
exception). `config.tasks.colors` is guaranteed nonempty (`Config::init` falls
back to the default dark-red/dark-yellow/dark-green palette), so index 0 and
`len-1` always exist.

---

## 9. TUI (ui/)

The interactive TUI uses **`matchmaker-lib`** (a high-performance terminal fuzzy-finder and TUI engine powered by nucleo).

Both `TasksApp` (`src/ui/tasks.rs`) and `TodayApp` (`src/ui/today.rs`) run separate `Matchmaker` instances configured with columns, event handlers, previewers, custom action bindings, and overlays.

- **Matchmaker Configuration (`mm_config.rs`)**: Loads `mm.toml` from the user config dir (`crate::paths::config_dir()`), falling back to the bundled default (`assets/config/mm.toml`, via `include_str!`). User keybinds in the file extend the code defaults (`default_binds`).
- **Action Dispatch (`action.rs`)**: Custom application actions (`Action`) implement `ActionExt` with `Display` and `FromStr` parsing. `Repopulate` and `EditExecute` are internal actions dispatched by async handler tasks over the render channel — not user-bindable.
- **Shared state & the render channel**: each app keeps its list/`mode`/`show`/sort state in a `std::sync::Mutex` shared by the render thread and async DB tasks. The custom-action handler is synchronous; DB work runs in `tokio::spawn` tasks that update the shared state and push `Repopulate` through `PickOptions::render_tx()`. The `Repopulate` handler restarts the worker (`state.worker_restart()`), re-injects the items (`state.injector()`), refreshes the header line, and restores the cursor position.
- **Worker & Columns**:
  - `TasksApp`: `Worker<TaskRow, ()>` with `ID`, `Pri`, and `Name` columns.
  - `TodayApp`: `Worker<TodayEntry, ()>` with `Time`, `Badge`, and `Label` columns.
- **Previewer (`preview.rs`)**: Asynchronously formats previews for the focused row using `build_preview` / `build_today_preview` and streams content via `PreviewMessage::Set`.
- **Overlays (`overlays.rs`)**: `ConfirmPrompt` / `InputPrompt` state staged through `SharedOverlay` (a generic `Arc<Mutex<_>>` bridge), activated by pushing matchmaker's builtin `Action::Overlay(n)` through the render channel (`0` = confirm, `1` = input). While an overlay is active it consumes every action.
  - `ConfirmOverlay`: Yes/No confirmation dialogs (Delete, Reset progress, Availability window); Left/Right move the cursor, Y/N hotkeys pick an option, Enter accepts, Esc cancels.
  - `InputOverlay`: text input for the task completion count, tracker value updates (`Update:`), tracker edits (`Edit 'type'`), and item linking (`Link:`). Inputs carry char filters and submit validators; invalid submits keep the overlay open with an error line.

### Accept — the shared task toggle

Both TUIs route the Accept action through the same pure decision fn
(`task::accept_action`), executed by `task::apply_accept_action`:

| task kind | state | Accept → |
| --- | --- | --- |
| scheduled | no entry / done / failed | toggles directly, never a modal: `none → 1 (done) → 0 (failed) → none (clear, before the window end) / 1 (after)` |
| once-only / `target_count ≤ 1` | not done / done | toggles: complete (+1) ↔ reset immediately (no modal) — except the tasks TUI's @done view, where the reset asks first |
| `target_count > 1` | not complete | `InputOverlay` — numeric completion delta prompt |
| `target_count > 1` | complete | "Reset progress?" confirm overlay, **default Yes** (recurring tasks reset only the current interval — earlier history survives) |

### Item Editing (`Action::Edit`)

On the selected item: task/mood bodies and text-tracker payloads open the
external editor (`editor::open_editor_on_text`) via matchmaker's
`Interrupt::Execute` — the render loop pauses the event loop, exits the
alternate screen and raw mode, runs the interrupt handler (which blocks on
`$EDITOR`), then restores the TUI; `integer`/`float`/`duration` trackers
use an in-TUI input overlay validated with the same per-kind parser and
strict gate as the CLI; null tracker rows move their timestamp to now
(`update_tracker_time`) — nothing else.

### Item Linking (`Action::Link`, today view only, `Ctrl+L`)

Opens a bare "Link: " id prompt (the Count:/Update: modal shape — digits
only, no validation, no info display; empty input cancels). The direction
follows the selected entry kind, and the typed number is the **raw row id**
passed straight to the db function, whose `Result` is just logged
(`.elog()`):

- mood/journal entry → `db::link_mood_to_task` — `INSERT OR IGNORE` into
  `task_moods` (the task preview's `moods:` section picks it up).
- tracker entry → `db::link_tracker_to_mood` — `UPDATE tracker SET mood`,
  replacing the tracker's attachment or inserting one (the tracker
  preview's `mood:` field).
- task entry → `db::set_task_parent` — `UPDATE todos SET parent`, replacing
  the parent (the task preview's children section).

The tasks app ignores `Action::Link`.

### Applications

- **`TasksApp`** (`ui/tasks.rs`) — task-list app (`@`, `@done`); modals for
  completion deltas, delete (default No), reset (default Yes), and the
  availability confirm (D10); preview pane with task details.
- **`TodayApp`** (`ui/today.rs`) — today view; cycles horizons (Today →
  Tomorrow → Week); mood/task body edits, tracker edits, and deletion with
  confirm modals.

---

## 10. Embeddings (embedding.rs)

- Model: **nomic-embed-text-v1.5** int8 QDQ ONNX + saliency adaptor +
  tokenizer, vendored at `assets/model/` and `include_bytes!`-d — runtime
  inference is fully offline (~131 MB, tracked with git-lfs).
- **Runtime**: ONNX Runtime (`ort` 2.0). Sessions are `&mut`-heavy, so each
  model sits behind a `Mutex`; the global `EMBEDDER: OnceLock` panics on load
  failure — the model is a build/runtime invariant.
- **Shape**: 768-dim, mean pooling over the attention mask, L2-normalized;
  dynamic input shapes, 2048-token cap.
- **Cache**: SQLite `embedding_cache` keyed by `prefix + text`;
  `get_or_embed_cached` on miss; `:db prune` clears it (lazily re-embedded).
- **build.rs**: regenerates the model when `EMBED_MODEL` mismatches
  `assets/model/.embed_model_stamp` (pixi/python quantize script); also
  bundles `assets/help.txt` into the README.

---

## 11. Color (color/) — Oklab mood projection

Mood colors are a projection onto config `[moods]` pair anchors: each pair
mood is embedded (prefix-anchored, SQLite-cached) and its **basis ray** is the
L2-normalized difference from a neutral base embedding.

- **Build** (`ColorAxes::build_async`): embeds base + pairs, computes basis
  rays and each pair's Oklab target, precomputes the **Gram matrix** (AᵀA);
  cached on `MoodConfig` by idempotent `MoodConfig::init_with`.
- **Regression** (`regression_weights`): shift = embedding − base;
  Lawson–Hanson **NNLS** (`color/nnls.rs`) over the Gram matrix; weights
  filtered by `min_contribution`, truncated to `top_k`, power-rescaled;
  saliency from the ONNX adaptor or the stored `mood.score` override.
- **Blend** (`weights_to_color`): saliency-gated weighted Oklab blend of the
  contributing basis colors; `None` → neutral baseline.
- **Render** (`mood_color_cached`): per-mood cache, prefers stored embedding
  BLOBs and backfills legacy rows; `:color <mood>` (`diagnose_color`) prints
  every intermediate value.

---

## 12. Editor (editor.rs)

`open_editor_for_body` reads `VISUAL` then `EDITOR` to read a body (errors if
neither is set — no silent fallback). The template the editor opens with is
determined by the bare body delimiter's dot count, indexing into the
corresponding `[editor] *_template` array field. `%%` comment blocks are
stripped after editing.

---

## 13. Testing strategy

- Unit tests live in each module: parsers (`parse_from` on arg vecs),
  `apply_delta_to_counts`, interval math, `accept_action` over the full task
  grid, config serde round-trips.
- Integration tests (`tests/integration.rs`) run the full parse → handler →
  DB path (`parse_from` + `execute_command`, `tui: false`, `Vec<u8>` writer)
  and assert on captured stdout. Recurring tasks are seeded via raw SQL
  (cliclack prompts bail on non-interactive stdin).
- Embedding-dependent paths run for real (the model is bundled into the test
  binary — no download gate).
- `db::test_pool()` gives each test a fresh in-memory schema.

---

## 14. Deferred / intentionally out of scope

- `:score` — stub (`todo!()`); `:g` grid view — not implemented.
- No CLI flags for view variants: the `@[:o|:O]` / `@done[:o|:O]` suffixes
  (`ViewVariant`) cover the former toggles (env vars removed).
- No DB migrations — delete the dev DB on schema changes.
- Trackers have no TUI path; entry creation is CLI-only (no TUI input forms).
