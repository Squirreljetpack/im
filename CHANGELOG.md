## [0.2.9] - 2026-10-30

### 🚀 Features

- The recurring creation menu can edit start time, interval, and available
  duration, and the scheduled menu start time and available duration, so a
  value typed on the way in is corrected without cancelling. A duration at or
  beyond the interval means unbounded availability (`always`) instead of a
  validation error, editing the interval re-clamps the duration, and a blank
  answer keeps a scheduled task's current duration.

### 🐛 Bug Fixes

- Today view: completed-on rows merge per interval instead of per task, so a
  completion logged in an interval without an availability window — before a
  moved-forward `start_time`, say — is no longer dropped when the task also
  has window rows. Each row's sum, `last:` and time label are scoped to its
  own interval.

### 💥 Breaking changes

- Utility commands moved under `:tool` (`:tool config|embed|mood|clear`) and
  account commands under `:sync` (`:sync register|login|status|logout`).


## [0.2.8] - 2026-10-30

### 🐛 Bug Fixes

- *(macos)* Link compiler-rt explicitly under fat LTO, fixing a startup
  segfault in ONNX Runtime environment creation (`im :embed`, `im :color`).

## [0.2.7] - 2026-10-30

### 🚀 Features

- `im :color` without a mood lists every anchor mood of the moods file with
  a swatch of its color, one per line; `im :color <mood>` keeps the full
  pipeline diagnostic.
- Today view: `[today_view] swap_all_and_due_recurring_task_views` trades the
  `All` and `Due` recurring-task displays. `Due` is anchored at now (earliest
  window intersecting the horizon, skipping closed windows), and previews are
  scoped to the row's window instead of sharing the whole day.
- No phantom availability windows past a recurring task's expiry, and
  today-view `ends` shows the task's real expiry again.

### 🐛 Bug Fixes

- *(embedding)* Pin the CPU EP on macOS

## [0.2.6] - 2026-10-29

### 🚀 Features (sync)

- `im :auth register|login|status|logout` and `im :sync` (plus
  `im :sync --reset`) sync a database against a Cloudflare Worker
  (`sync-server/`): a per-user Durable Object holds the log, D1 the accounts.
- Concurrent edits merge per field (last write wins) instead of per entity; a
  push breaks at entity boundaries, so a create carries all of its cells.
- Deletes are terminal and slot losers become tombstones, so two devices that
  edit the same field converge instead of duplicating rows.
- Entity ids are UUIDv7, so a row created on any device is unique without a
  round trip; local `short_id`s stay a per-device projection.
- The server address comes from `[sync] server` in the client config
  (`IM_SYNC_URL` overrides it for one command); the account is stored apart
  from the sync state, so `im :sync --reset` does not sign you out.
- A database from an older version (INTEGER keys) is copied aside and
  recreated empty instead of being written with the wrong key type; an
  unreadable file is quarantined the same way.

### 💥 Breaking changes (tracker rework)

- `min`/`max` renamed to `low`/`high`; `kind = "number"` renamed to
  `"integer"`; new `kind = "duration"` (duration-string values stored as
  seconds, displayed like `6m 30s`).
- `interval` now takes a table `{ anchor, span, cumulative = false }`
- Value and bound forms are strict per kind/mode: `float` takes plain
  numbers, `integer` plain whole numbers, `duration` duration strings only,
  `null` no value at all; `low`/`high` mirror the value form of the kind
  (duration strings for `duration` and `null`+replace, whole numbers for
  `text` and `null`+cumulative). Invalid bound forms are dropped with a
  warning at config load.
- New `strict` setting gates logging/updating: numeric kinds gate the value
  against the inclusive `low`..`high` span (inverted bounds gate the span
  between them), `text` gates the message length in characters, `null`
  trackers gate *when* they may be logged (the circular `low`/`high` time
  offset zone; requires both bounds, replace mode only).
- `null` trackers require an interval (dropped with a warning otherwise) and
  store score 0 in every mode — the payload column is unused and today-view
  rows show no payload (the count lives in the grid, the timestamp in the
  entry's own time). Negative durations are rejected at log time.
- Replace-mode insertion is one shared strategy for all kinds: a CLI log in
  an occupied slot drops the slot's previous rows (including rows logged
  with a mood — the mood row itself survives) and inserts the new one.
- The TUI update action on a `null` row only moves its timestamp to now,
  within its current interval slot (cross-slot moves are rejected, the
  strict zone still applies); on value-bearing kinds it runs the same strict
  pipeline as the CLI.
- `:db doctor` prunes any nonzero row of a `null` tracker (all modes).

## [0.2.5] - 2026-08-14

### 🚀 Features

- Editor no longer opens automatically for interactive flows.
- Config adjustments
- Body templates
- Adjust today view filters
- Config.tasks.overdue_color + badge::task_label_color
- Today-view preview polish + overdue date column + task children
- Link actions in the today view (Ctrl+L)
- Improve jiff-english robustness
- TUI migration to matchmaker

## [0.2.4] - 2026-08-07

### 🚀 Features

- Db :doctor
- Jiff-english
- Duration trackers
- Tui interaction for trackers

## [Unreleased]

### 🚀 Features

- `[badges]` config section: journal badge (glyph and/or color — a bare
  char, a color string, or `{ badge = '·', color = "red" }`; color defaults
  to `Reset`), plus `tracker` and `mood` glyph overrides for the today
  view and previews. `journal_badge` moved from `[today_view]`.
- Today view task filter: `TasksFilter` (`none` / `all` / `overdue` /
  `pending` / `horizon`) replaces `config.today_view.include_overdue` as
  `initial_tasks_filter`, bound to the view variant — `All` uses the
  configured filter, `A` pins `Horizon`, `B` pins `Overdue`; `none` hides
  the whole task section (journal-only view). Undated
  oneshots created before today are no longer dropped from the `All`
  variant. `CycleShow` renamed `CycleFilter`; TUI headers label the
  variants per app (`all | journal | tasks` in the today view,
  `all | oneshot | other` in the tasks view).

- Renamed: the binary is now `im` (previously `feeling`), and the logged
  mood entries are consistently called moods — DB schema (`mood` table,
  `tracker.mood`, `task_moods.mood_id`), code identifiers, CLI usage
  strings, and docs. Data paths moved to `~/.config/im` / `im.db`;
  existing `feeling` databases and config dirs are not migrated and must
  be moved/deleted by hand.
- Task↔mood links: `im <mood> -<short id>` records a link; the task
  preview shows a `moods:` field with colored badges
- `:db` command replaces `:prune` — `:db prune` (old behavior) and
  `:db backfill` (persist missing mood embeddings/scores)
- `:db doctor` — checks tracker entries against their configured kinds and
  prunes entries whose storage class no longer matches (orphaned tracker
  types and stale nonzero time-marker entries included), after an
  interactive confirm
- Tracker intervals are calendar-aware (`interval = ["2020-01-01 00:00", "1 day"]`)
- New `null` tracker kind: valueless timestamp markers with time-of-day
  coloring (e.g. sleep start) or per-interval counting

### 🐛 Bug Fixes

- Recurring-task and tracker intervals are calendar-based (jiff): DST and
  variable month lengths no longer drift interval boundaries
- Interval storage switched from seconds to packed jiff spans

## [0.2.3] - 2026-08-07

### 🐛 Bug Fixes

- Reorganize parse, fix overflow

## [0.2.2] - 2026-08-07

### 🚀 Features

- Show name of attached parent during interactive task creation

## [0.2.1] - 2026-08-06

- Release pipeline updates
- misc

## [0.2.0] - 2026-08-06

### 🚀 Features

- Nomic embedder, ort runtime
- Reorganize views
- Reorganize config

### 💼 Other

- CliOpts flag counts threaded into handlers
- ColorAxesSettings flattened into MoodConfig; :color verbose axes dump
- Scheduled task syntax — ':' description, '@' duration markers
- Lock invalid-timestamp behavior with regression tests
- Task creation confirmation polish
- Cliclack intro for task flows; prefill fields logged
- 'feeling @<date>' — today view anchored to arbitrary days
- Grid titles + verbose gating; text-tracker entry timestamps
- Shared Enter-action across both TUIs

## [0.1.0] - 2026-08-01

Initial commit
