# VIEWS.md — ViewVariant view system

The shared `ViewVariant` enum controls the `All → A → B` view cycle used by
both TUIs and the CLI. Its interpretation is context-specific: task views use
`A` for oneshots and `B` for recurring/scheduled tasks, while today views use
`A` to exclude completed items and `B` for tasks-only output.

## Predicates

`now` = current Unix timestamp; `t` = a task row; `entry` = a completion
record in `todo_completions`. Shorthand used in the matrices below:

| shorthand | meaning |
| --- | --- |
| `O` / `R` / `S` | oneshot / recurring / scheduled (from `interval_secs` + `available_duration_secs`) |
| `done(t)` | `O`: `completions >= target` (target 0: any entry) · `R`: reached target in current interval · `S`: entry ≥ 1 or auto-completed |
| `ongoing(S)` | no entry, window still open |
| `failed(S)` | has an entry with count 0 |
| `auto_completed(S)` | no entry, window elapsed |
| `expired(R)` | `end_time` set, `now > end_time`, not done — tasks TUI only; TodayView window rows carry the unscoped last completion in `end_time` instead of the expiry, and the per-window badge has no expired state |
| `has_entry(t)` | any completion row exists |
| `availability_passed(t)` | window end `<= now` — recurring: current-interval-anchored; scheduled: absolute (`start + duration`); the TodayView D10 confirm uses the per-window check instead (see below) |

Completion sums are scoped to the current interval for recurring tasks in the
pending/done views; the TodayView per-window fetch scopes them per availability
window instead. The `@done` history view uses an unscoped sum. `last_time`
follows the completion sum's scoping, except: the tasks-TUI fetches carry the
unscoped last completion for recurring tasks (last completion ever — used by
the pending D9 sort, the done-view sort, and the preview `last:` field), and
TodayView window rows carry the unscoped last completion in `end_time` (the
expiry is not used there). Availability-window checks must first exclude
`expired(t)` tasks — an expired task has no current interval.

## CLI syntax

| Command | Effect |
| --- | --- |
| `im !` | Interactive oneshot creation (name prompted) |
| `im @` | Pending view — `ViewVariant::All` |
| `im @:o` | Pending view — `ViewVariant::A` (oneshots only) |
| `im @:O` | Pending view — `ViewVariant::B` (recurring not availability-filtered + scheduled) |
| `im @done` | Completed tasks — `ViewVariant::All` |
| `im @done:o` | Completed oneshots only — `ViewVariant::A` |
| `im @done:O` | Completed recurring history + completed scheduled — `ViewVariant::B` |
| `im @due` | TodayView, `ViewVariant::B`, `TodayHorizon::Today` |
| `im @due:t` | Tomorrow view, `ViewVariant::B`, `TodayHorizon::Tomorrow` |
| `im @due:w` | Week view, `ViewVariant::B`, `TodayHorizon::Week` |
| `im @<date>` | Anchored TodayView, `ViewVariant::All`, `TodayHorizon::Today` |

## View matrix

### `@` — Pending view

| Variant | Behavior |
| --- | --- |
| `All` | `not done(O)` + `R` (interval-scoped, availability-filtered, not expired) + `ongoing(S)` |
| `A` | `not done(O)` only |
| `B` | `not done(R)` (any not expired, not just availability-filtered) + `! availability_passed(S)` |

all of these also include + any task (all/oneshot_only/not_oneshot_only) with a completion entry within the last `persist_pending_seconds`

Non-complete scheduled tasks in `All` are exactly `ongoing(S)` — failed,
auto-completed and completed `S` are excluded.

### `@done` — Completed tasks

| Variant | Behavior |
| --- | --- |
| `All` | `done(O)` + `S` `has_entry` + `done(R)` in current interval |
| `A` | `done(O)` only |
| `B` | (ALL `R`) + `S` `has_entry` or `auto_completed` |

`@done:o` shows more scheduled tasks than `All` — it adds auto-completed `S`
and every recurring task (never-completed rows included).
Order: done time, newest first — the last completion entry; entry-less
rows fall back per kind: auto-completed `S` to
`start_time + available_duration_secs`, zero-entry `R` history rows to
`start_time` (their `available_duration_secs` is the availability window,
not a completion moment).

### `@due` / `@<date>` — TodayView

Note: the two spellings use different defaults — `@due` starts at
`ViewVariant::B` with the day horizon, `@<date>` at `ViewVariant::All`.

| Variant | Behavior |
| --- | --- |
| `All` | All tasks/trackers/mood sections for the day (oneshots, recurring, scheduled, completed today) |
| `A` | Same but completed tasks filtered out; done rows dropped from regular task lists — for recurring tasks this is per window (a window whose own interval reached target is dropped, not-done windows stay) |
| `B` | Tasks only — no trackers, no mood sections; otherwise the same as `All` (completed tasks and completion-today rows included) |

Time cell: done rows show the completion time (an entry-less auto-completed
scheduled task falls back to `start + duration`); not-done scheduled rows
show `start_time`. Recurring tasks appear once per availability window that
intersects the horizon — `All` shows every such window, `B` shows only the
next (earliest) window per task, `A` shows only not-done windows. A recurring
window's time cell is the last completion inside the window's interval once
it is completed — and also once it has passed (`now >= window_end`), falling
back to the window end when the interval has no completion — while an open
or future not-done window shows the window start. The D10 confirm modal
("availability window passed") applies per window: it triggers when
`now >= window_end` on a not-done window. The preview pane for a recurring
window shows `last:` from the window row's `end_time` — the unscoped last
completion, not the expiry — and omits the `ends` field; in the tasks TUI the
preview's `last:` reads `last_time` (unscoped for recurring tasks) and `ends`
reads the real expiry.

### Task filter (oneshot section)

The oneshot section is scoped by a `TasksFilter`, bound to the view
variant: `All` uses `config.today_view.initial_tasks_filter` (the CLI and
the TUI's starting state), `A` pins `Horizon`, `B` pins `Overdue`. The
TUI header shows the variant's filter label — today app: `[show:
all|journal|tasks]` (A is called "journal" even though it carries task
rows, minus completed ones); tasks app: `[show: all|oneshot|other]`.
Scheduled and recurring fetches are unaffected. Due time = `end_time`
when set, else the `start_time` fallback (legacy rows / undated tasks).

| Filter (variant) | Oneshot rows |
| --- | --- |
| `None` (config only) | No task rows at all — the entire task section (oneshots, scheduled, recurring, completed-today) is hidden: a journal-only view |
| `All` (default; `All` variant) | Any oneshot task — open or completed, any date, no bounds |
| `Overdue` (`B` variant) | Dated oneshots due within the horizon or overdue (`end_time` set and `<= horizon_end`); undated tasks are never overdue and stay out |
| `Pending` (config only) | Open (incomplete) oneshot tasks, any date — the undated inbox |
| `Horizon` (`A` variant) | Open oneshot tasks due within the horizon; overdue (due before the day start) excluded |

Completed-today rows (the step-5 merge) are independent of the filter and
still surface in `All` and `B`; in `Pending` / `Horizon` / `Overdue` the
regular oneshot list is incomplete-only, so a completed task appears once,
via its completion row.

## Who sets the variant

- **CLI**: `@` / `@done` start at `All`, with the `:o` / `:O` suffixes;
  `@due[:t|:w]` is fixed at `B`; `@<date>` at `All`. The oneshot task
  filter follows the variant: `All` uses
  `config.today_view.initial_tasks_filter`, `A` pins `Horizon`, `B` pins
  `Overdue`.
- **TUI**: cycles `All → A → B → All` via the `CycleFilter` action, starting
  from the command's suffix. The tasks app cycles modes; the today app cycles
  horizons (`Today → Tomorrow → Week`).

## Plain (non-TTY) output

All non-TTY output is newline-separated rows, tab-separated columns. The TUI
and the text writers consume the same `TodayEntry`s, so badge/color logic is
shared.

- **Task lists** (`format_tasks_simple`): `id \t interval \t next_available \t pri \t name \t status`. `id` is the user-facing short id — empty for completed oneshot tasks (their short id was cleared). `interval`/`next_available` are single spaces for oneshot tasks; `next_available` is the next interval-window start for recurring tasks. `status` is the completion badge (see ARCHITECTURE §8).
- **Today view** (`format_today_simple`): `ts \t marker \t label \t detail` rows from moods, tracker entries, and tasks. Markers: `●` mood (Oklab projection), `◆` tracker, `○`/`✓` oneshot, `↻`/`✓` recurring, `✓`/`◷` scheduled; the mood and tracker glyphs are configurable via `[badges] mood` / `[badges] tracker`, and journal entries (empty mood) use `[badges] journal_badge` (glyph and/or color; color defaults to `Reset`) when set. Empty day → `Nothing logged today.`
- **Tracker grids** (`write_tracker_grid`, `: [week|month|year] [ids]`): per-day/per-interval dot sequences — mood dots colored by the Oklab projection, tracker dots binned by score vs `low`/`high` (cumulative slots bin the per-slot sum; null replace markers bin the circular `[low, high]` time-offset zone, cumulative null markers bin the per-slot count), recurring-task dots from the per-interval completion sum via the completion badge.

## Appendix: formal definitions

All terms below are used throughout this document. `now` = current Unix
timestamp; `t` = a task row; `entry` = a completion record in
`todo_completions`.

```pseudocode
interval_start(t) =
    if t.interval_secs != null and t.start_time != null:
        max(t.start_time, t.start_time + ((now - t.start_time) / t.interval_secs) * t.interval_secs)
    else:
        t.start_time

interval_end(t) =
    if t.interval_secs != null:
        interval_start(t) + t.interval_secs
    else:
        null

is_in_interval(t):
    return interval_start(t) <= now < interval_end(t)

---

done(t):
    // Recurring: reached target in current interval (pending/done views);
    // the TodayView checks per window instead — window-scoped completions
    // vs target on the window's own row
    // Scheduled: has any completion entry (entry >= 1) or auto-completed
    // Oneshot/Threshold: completions >= target_count (target 0: any entry)

completed(t):
    // Has at least one completion entry (entry count >= 1)
    return completions(t) >= 1

failed(t):
    // Has a completion entry with count 0 (window closed, never done)
    return completions(t) == 0 and has_entry(t)

auto_completed(t):
    // Scheduled task: no entry, but availability window has elapsed
    return has_no_entry(t) and t.interval_secs is null
        and t.available_duration_secs is not null
        and t.start_time + t.available_duration_secs <= now

ongoing(t):
    // Scheduled task: no entry, window still open
    return has_no_entry(t) and t.interval_secs is null
        and t.available_duration_secs is not null
        and t.start_time + t.available_duration_secs > now

expired(t):
    // Recurring task: end_time set and now past end_time,
    // not done in current interval
    return t.end_time is not null and now > t.end_time
        and not done(t)

partial(t):
    // Recurring: has some completions but not yet at target
    return completions(t) > 0 and completions(t) < target_count(t)
        and t.interval_secs is not null

has_entry(t):
    return exists tc in todo_completions where tc.todo_id = t.id

optional(t):
    // Task's optional flag (t.optional != 0)
    return t.optional != 0

has_no_entry(t):
    return not has_entry(t)

window_elapsed(t):
    return t.available_duration_secs is not null
        and t.start_time + t.available_duration_secs <= now

completions(t):
    // Interval-scoped sum for recurring; unscoped sum for done-view
    // (determined by the query variant, not this definition)
    return SUM(tc.count) over completion entries for task t

unscoped_completions(t):
    // Sum over ALL completion entries ever (no interval filter)
    return SUM(tc.count) over all completion entries for task t

availability_passed(t):
    // Window end is anchored to the current interval for recurring tasks
    // (start_time is the chain origin and never advances) and absolute for
    // scheduled tasks. The TodayView D10 check uses the per-window variant
    // below instead: window_end(k) <= now on the window's own row.
    return t.available_duration_secs is not null
        and (if t.interval_secs is not null
             then current_interval_start(t, now) + t.available_duration_secs
             else t.start_time + t.available_duration_secs) <= now

window_start(k):
    // Start of the k-th availability window of a recurring task
    return t.start_time + k * t.interval_secs

window_end(k):
    // End of the k-th availability window: available_duration_secs when
    // set and shorter than the interval, else the whole interval;
    // truncated by end_time when set.
    dur = if t.available_duration_secs is not null
              and t.available_duration_secs < t.interval_secs
          then t.available_duration_secs else t.interval_secs
    return min(window_start(k) + dur, t.end_time)

recurring_window_time(k, now):
    // TodayView time cell for the k-th window. Completions are scoped to
    // the window's whole interval [window_start(k), window_start(k) + interval).
    if done_window(k) or now >= window_end(k):
        if interval_completions(k) >= 1:
            return last_completion_in_interval(k)
        else:
            return window_end(k)
    else:
        return window_start(k)
current interval, so the availability-window check does not apply to it.

is_recurring(t):
    return t.interval_secs is not null

is_scheduled(t):
    return t.interval_secs is null
        and t.available_duration_secs is not null

is_oneshot(t):
    return t.interval_secs is null
        and t.available_duration_secs is null

is_overdue(t):
    return t.end_time is not null and now > t.end_time
    and not done(t)

is_due(t):
    return t.end_time is not null and now <= t.end_time
    and not done(t)
```
