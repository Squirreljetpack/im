# Badge system

A badge is a `(glyph, color)` pair rendered next to a row. Every row type in
the app — tasks (TUI tasks view, CLI task lists) and today-view entries —
derives its badge from the same rules below.

## Source of truth

- `src/badge.rs` — `task_badge(task, config, done_view)`. `done_view` is true when rendering a done-list (@done tasks view / CLI @done list): the done-state glyph stays `◷` / `↻` for scheduled / recurring rows instead of switching to `✓`. It has no effect on oneshot | threshold rows (always `✓` when done).
- `src/today.rs` — all badge/badge-label stuff into `badge.rs` i.e. `completion_badge`, `completion_badge_text`.
  `completion_badge` and `completion_badge_text` usage is unchanged for tracker grids
  (per-interval dot rows in the `:trackers`/mood grids) and in make_preview.

## Shared primitives

`colors` = `config.tasks.colors` (`[c0, c1, ..., cN]`).

- **done** → last color `cN` (always, for every task kind).
- **partial progress** (0 < count < target_count) → binned across
  `colors[..N-1]` by `count / target_count` (binning only, no blending; the
  last color is reserved for 100%).
- **zero entries** → `Reset` (neutral, uncolored).
- **failed / missed** (entry 0, or a non-optional window elapsed with no
  entry) → `colors[0]`.
- **optional & missed** → `Reset`.

`count` is the task's completion count scoped to the **current interval**
(what the queries already select as `completions`); `now` is
`date::now()`.

---

## Oneshot | Threshold

| State | Glyph | Color |
| --- | --- | --- |
| done (`completions >= target_count`) | `✓` | last `cN` |
| not done, **overdue** (`end_time` set and `now > end_time`) | `○` | completion color of count (0 -> colors[0]) |
| not done, not overdue, zero entries | `○` | `Reset` |
| not done, not overdue, partial | `○` | completion color of count (0 -> colors[0]) |

Don't combine branches here.
Undated tasks (no `end_time`) are never overdue.

## Recurring (`interval_secs` set)

The glyph is **`✓` when done, `↻` always otherwise**

| State | Glyph | Color |
| --- | --- | --- |
| done in current interval (`completions >= target_count`) | `✓` (`↻` when done_view) | last `cN` |
| not done, **expired** | `↻` | `DarkGray` |
| not done, **availability passed**, optional | `↻` | `Reset` |
| not done, **availability passed**, non-optional | `↻` | binned |
| not done, **during availability**, zero entries | `↻` | `Reset` |
| not done, **during availability**, partial | `↻` | binned |

expired: `end_time` set and `now > end_time`

## Scheduled (no interval, availability window)

| State | Glyph | Color |
| --- | --- | --- |
| done (entry `>= 1`, or no entry with the window elapsed — auto-completed) | `✓` (`◷` when done_view) | last `cN` |
| failed (entry `0`, , window not open) | `◷` | `colors[0]` |
| failed (entry `0`, , window still open) | `◷` | `colors[0]` |
| ongoing (no entry, window still open) | `◷` | `Reset` |

Unchanged from the current `scheduled_badge` semantics. Do not combine both failed branches into one.

---

## Mood

| State | Glyph | Color |
|---|---|---|
| any mood entry | `●` (configurable: `[badges] mood`) | Oklab mood color (embedded label → color, cached) |

The today-view rows and the previews' mood markers (`moods:` field in the
task preview, `mood:` field in the today preview) read the glyph from
`config.badges.mood`, defaulting to `●`. Computed inline in
`fetch_today_entries` / the preview builders.

## Journal

| State | Glyph | Color |
|---|---|---|
| journal-only entry (empty mood label) | configured `[badges] journal_badge` (glyph and/or color), or none | badge color, else `Reset` |

`journal_badge` accepts a bare char (`'·'`), a color string (`"red"`,
`"#FFB6C1"`), or an object with either or both fields
(`{ badge = '·', color = "red" }`). The glyph defaults to nothing, the
color to `Reset`. Computed inline in `fetch_today_entries`.

## Tracker

| Tracker kind | Glyph | Color |
| --- | --- | --- |
| numeric / float | `◆` (configurable: `[badges] tracker`) | `bin_score_color` (binned by score) |
| text | `◆` (configurable: `[badges] tracker`) | `DarkGray` |

The today-view tracker rows read the glyph from `config.badges.tracker`,
defaulting to `◆`. The tracker grids keep their own hardcoded dots.
Computed inline in `fetch_today_entries`.
