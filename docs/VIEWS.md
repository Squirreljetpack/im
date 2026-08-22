# VIEWS.md — ViewVariant View System

The shared `ViewVariant` enum controls the three-stage view cycle (`All → A → B → All`) used across the CLI and TUIs. Its interpretation depends on the active view mode:
- **Tasks Views (`@` and `@done`)**: `A` filters to oneshot tasks, while `B` filters to recurring and scheduled tasks.
- **Today View (bare `im`, `@due`, and `@<date>`)**: `A` selects the **journal timeline** (moods, trackers, and completion events), while `B` selects the **tasks-only agenda** (overdue/due tasks, scheduled tasks, and next recurring windows).

---

## 1. Predicates and Shorthand

* `now`: current Unix timestamp.
* `t`: a task row in `todos`.
* `entry`: a completion event record in `todo_completions`.

| Shorthand | Meaning |
| :--- | :--- |
| **`O`** | Oneshot task (`interval_secs IS NULL` and `available_duration_secs IS NULL`). |
| **`R`** | Recurring task (`interval_secs IS NOT NULL`). |
| **`S`** | Scheduled task (`interval_secs IS NULL` and `available_duration_secs IS NOT NULL`). |
| **`done(t)`** | `O`: `completions >= target_count` (or any completion when `target_count <= 0`).<br>`R`: reached target count within the current interval.<br>`S`: has a completion entry (count ≥ 1) or auto-completed when the window elapsed. |
| **`ongoing(S)`** | Scheduled task with no completion entry whose availability window is still open (`now < start_time + duration`). |
| **`failed(S)`** | Scheduled task with an explicit completion entry of count 0. |
| **`auto_completed(S)`** | Scheduled task with no completion entries whose availability window has fully passed (`now >= start_time + duration`). |
| **`expired(R)`** | Recurring task with `end_time` set, `now > end_time`, and not completed in the current interval (tasks view only). |
| **`has_entry(t)`** | At least one completion record exists for task `t` in `todo_completions`. |
| **`availability_passed(t)`** | The availability window end timestamp is `_<= now_` (`start_time + duration` for scheduled tasks, or current interval anchor + duration for recurring tasks). |

### Completion Scoping Rules
- **Interval Scoping**: In the pending and done task views, completion sums for recurring tasks are scoped to their current recurrence interval.
- **Window Scoping**: In the Today View, recurring availability windows scope completions to each window's respective interval.
- **Unscoped History**: The `@done:O` (`ViewVariant::B`) view sums all historical completions across all time.
- **`last_time` Timestamp**: Matches the scope of the completion sum. In task view fetches, recurring tasks carry the unscoped last completion timestamp for sorting and the preview pane's `last:` field.

---

## 2. CLI Syntax

| Command | View Mode | Default Variant | Horizon / Date | Notes |
| :--- | :--- | :--- | :--- | :--- |
| `im !` | Task Creation | — | — | Interactive oneshot task creation prompt. |
| `im @` | Pending Tasks | `All` | — | All open tasks (oneshot, recurring, scheduled). |
| `im @:o` | Pending Tasks | `A` | — | Open oneshot tasks only. |
| `im @:O` | Pending Tasks | `B` | — | Open recurring & scheduled tasks only. |
| `im @done` | Done Tasks | `All` | — | Completed tasks in the current interval / oneshots. |
| `im @done:o` | Done Tasks | `A` | — | Completed oneshot tasks only. |
| `im @done:O` | Done Tasks | `B` | — | Completed recurring history and completed scheduled tasks. |
| `im @due` | Today View | `B` (`tasks`) | `Today` | Pure tasks agenda due today or overdue. |
| `im @due:t` | Today View | `B` (`tasks`) | `Tomorrow` | Tasks agenda for tomorrow. |
| `im @due:w` | Today View | `B` (`tasks`) | `Week` | Tasks agenda for the next 7 days. |
| `im @<date>` | Today View | `All` | `Today` (anchored) | Anchored today view for a specific date. |

---

## 3. View Matrix

### 3.1. `@` — Pending Tasks View

| Variant | Label | Filter Behavior |
| :--- | :--- | :--- |
| **`All`** | `all` | Open oneshots (`not done(O)`), active recurring tasks (`R`, interval-scoped, availability-filtered, not expired), and open scheduled tasks (`ongoing(S)`). |
| **`A`** | `oneshot` | Open oneshots only (`not done(O)`). |
| **`B`** | `other` | Open recurring tasks (`not done(R)`, all non-expired, not availability-filtered) and open scheduled tasks (`!availability_passed(S)`). |

* **Recent Completions Persistence**: All pending variants include tasks that had a completion recorded within the last `persist_pending_seconds` (default: 300s), scoped to the matching task kinds.
* **Scheduled Exclusion**: Failed, auto-completed, and completed scheduled tasks are excluded from `All`.

---

### 3.2. `@done` — Completed Tasks View

| Variant | Label | Filter Behavior |
| :--- | :--- | :--- |
| **`All`** | `all` | Completed oneshots (`done(O)`), scheduled tasks with completions (`S` with `has_entry`), and recurring tasks completed in their current interval (`done(R)`). |
| **`A`** | `oneshot` | Completed oneshots only (`done(O)`). |
| **`B`** | `other` | Full recurring task history (all `R`, including zero-entry rows) plus scheduled tasks with completions or auto-completed (`S` with `has_entry` or `auto_completed`). |

* **Sorting**: Sorted by completion timestamp descending (newest first). Entry-less fallback: auto-completed scheduled tasks fall back to `start_time + available_duration_secs`; zero-entry recurring history rows fall back to `start_time`.

---

### 3.3. Today View (`im`, `@due`, `@<date>`)

The Today View renders a combined timeline/agenda of entries within the selected horizon (`Today`, `Tomorrow`, or `Week`).

| Variant | Label | Contents |
| :--- | :--- | :--- |
| **`All`** | `[show: all]` | **Complete day summary**: Moods, tracker entries, open oneshot tasks, scheduled tasks overlapping the horizon, recurring windows, and tasks completed today (merged with completion timestamps). |
| **`A`** | `[show: journal]` | **Journal event timeline**: Moods, tracker entries, and **task completion events** occurring within the horizon. Open/uncompleted tasks are hidden; each logged completion appears at its exact timestamp, showing cumulative progress (e.g. 1/5, 2/5, 3/5). |
| **`B`** | `[show: due]` | **Tasks agenda**: Tasks only (moods and tracker entries hidden). Shows dated oneshots due within the horizon or overdue (`end_time <= horizon_end`), scheduled tasks overlapping the horizon, and the earliest upcoming availability window per recurring task. Completed-today tasks are not explicitly merged. |

#### Today View Time Cell Rules
- **Journal Entries & Completions**: Timestamp of the mood, tracker, or task completion event (`today_time_label`).
- **Open Oneshots**: Empty for undated oneshots (sort after timed entries); deadline (`end_time`) for dated oneshots.
- **Scheduled Tasks**: Done rows show the completion timestamp (or window end for auto-completed tasks); open rows show the start time.
- **Recurring Windows**:
  - `All`: Emits an entry for every availability window intersecting the horizon.
  - `B`: Emits only the next (earliest) upcoming window per recurring task.
  - Completed or passed windows (`now >= window_end`) display the last completion in the interval, or fallback to the window end.
  - Open or future windows display the window start time.

---

## 4. Variant Cycling

- **CLI**: Variant selection is specified via command suffixes (`:o` for `A`, `:O` for `B`, default `All`). `@due` starts fixed at variant `B`.
- **TUI**: Pressing `f` (`CycleFilter`) cycles variants in order: `All → A → B → All`.
  - In the **Tasks TUI**, this toggles between `all`, `oneshot`, and `other`.
  - In the **Today TUI**, this toggles between `all`, `journal`, and `tasks`. Pressing `h` cycles horizons (`Today → Tomorrow → Week`).

---

## 5. Plain (Non-TTY) Output

When stdout is not a TTY (or piped), output is printed as tab-separated columns with newline-terminated rows:

- **Task Lists (`format_tasks_simple`)**:
  ```text
  id \t interval \t next_available \t priority \t name \t status
  ```
  - `id`: Short ID (empty for completed oneshots whose short IDs were cleared).
  - `interval` / `next_available`: Spaced for oneshots; next availability window start for recurring tasks.
  - `status`: Derived completion badge.

- **Today View (`format_today_simple`)**:
  ```text
  time_label \t badge \t label \t detail
  ```
  - Markers: `●` mood (Oklab color projection), `◆` tracker, `○`/`✓` oneshot, `↻`/`✓` recurring, `✓`/`◷` scheduled.
  - Empty day outputs: `Nothing logged today.`

- **Tracker Grids (`write_tracker_grid`)**:
  Per-interval dot sequences colored by mood Oklab projections, numeric tracker scores binned into threshold ranges, and recurring task completion sums.

---

## Appendix: Formal Definitions

```pseudocode
interval_start(t):
    if t.interval_secs != null and t.start_time != null:
        return max(t.start_time, t.start_time + ((now - t.start_time) / t.interval_secs) * t.interval_secs)
    else:
        return t.start_time

interval_end(t):
    if t.interval_secs != null:
        return interval_start(t) + t.interval_secs
    else:
        return null

is_in_interval(t):
    return interval_start(t) <= now < interval_end(t)

---

done(t):
    if is_scheduled(t):
        return completions(t) >= 1 or auto_completed(t)
    else if is_recurring(t):
        return completions(t) >= t.target_count
    else:
        return (t.target_count <= 0 and has_entry(t)) or (t.target_count > 0 and completions(t) >= t.target_count)

completed(t):
    return completions(t) >= 1

failed(t):
    return completions(t) == 0 and has_entry(t)

auto_completed(t):
    return has_no_entry(t) and is_scheduled(t)
        and t.start_time + t.available_duration_secs <= now

ongoing(t):
    return has_no_entry(t) and is_scheduled(t)
        and t.start_time + t.available_duration_secs > now

expired(t):
    return t.end_time is not null and now > t.end_time and not done(t)

partial(t):
    return completions(t) > 0 and completions(t) < t.target_count

has_entry(t):
    return exists tc in todo_completions where tc.todo_id = t.id

has_no_entry(t):
    return not has_entry(t)

optional(t):
    return t.optional != 0

completions(t):
    // Scoped to current interval for recurring tasks; full history for unscoped queries
    return SUM(tc.count) over completion entries for task t

unscoped_completions(t):
    return SUM(tc.count) over all completion entries ever for task t

availability_passed(t):
    if t.available_duration_secs is null:
        return false
    if t.interval_secs is not null:
        return interval_start(t) + t.available_duration_secs <= now
    else:
        return t.start_time + t.available_duration_secs <= now

window_start(t, k):
    return t.start_time + k * t.interval_secs

window_end(t, k):
    dur = if t.available_duration_secs is not null and t.available_duration_secs < t.interval_secs
          then t.available_duration_secs else t.interval_secs
    return min(window_start(t, k) + dur, t.end_time)

recurring_window_time(w, now):
    if w.task.is_done() or now >= w.window_end:
        return w.task.last_time or w.window_end
    else:
        return w.window_start

is_recurring(t):
    return t.interval_secs is not null

is_scheduled(t):
    return t.interval_secs is null and t.available_duration_secs is not null

is_oneshot(t):
    return t.interval_secs is null and t.available_duration_secs is null

is_overdue(t):
    return t.end_time is not null and now > t.end_time and not done(t)

is_due(t):
    return t.end_time is not null and now <= t.end_time and not done(t)
```
