use ratatui::{
    backend::FromCrossterm,
    layout::Alignment,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use cba::bring::StrExt;

use crate::date;
use crate::today::{EntryKind, TodayEntry};

/// A `  field: value` line: the field name (with colon) in yellow, the
/// value uncolored. Field names are lowercase.
fn field_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {}: ", label), Style::default().fg(Color::Yellow)),
        Span::raw(value),
    ])
}

/// The entry's timestamp, right-aligned and dark gray.
fn date_line(ts: i64) -> Line<'static> {
    Line::from(Span::styled(
        date::format_datetime(ts),
        Style::default().fg(Color::DarkGray),
    ))
    .alignment(Alignment::Right)
}

/// Build the preview pane lines for a task row. With `today`, the row is
/// a today-view entry row: for recurring tasks `end_time` carries the
/// unscoped last completion (not the expiry), so `last` is read from it
/// and the `ends` field is skipped. `preview.show_last_when_done` controls
/// whether done rows still show their `last` field (`last:` is otherwise
/// shown only while the task is not done). `tree` is the selected task's
/// task tree (loaded on selection change): when the task has children,
/// they are rendered after the body.
///
/// The layout, top to bottom:
///
/// - a blank line, then the heading: the type name ("Task" / "Recurring"
///   / "Scheduled", full caps, bold) in its own color, indented one space,
///   over a dark-grey rule as wide as the title plus two;
/// - the task name, indented, white, italic;
/// - a blank line, then the fields (`id`, `priority`, `creation`/`due` for
///   oneshot, `start` for scheduled, and the recurring metadata (`next`,
///   `interval`, `duration`, plus `ends`/`optional` when set / `duration`,
///   `state`) as `field: value` lines with yellow lowercase field names;
/// - the progress bar for counted tasks (a blank line on each side), then
///   the body when nonempty (a blank line, then the body indented two
///   spaces), then the children subtree when the task has any (a blank
///   line, then the tree rows).
///
/// Row text for a task inside the preview pane: `{badge} {name}
/// (#{short_id})`, with the name ellipsized to 16 columns (ellipsis
/// included) and the short id appended in parentheses while the task has
/// one. When `body` is set, the task body follows on its own line. The
/// badge glyph comes from [`crate::badge::task_badge`]; `config` is
/// accepted up front so the badge and format can become configurable
/// later.
fn task_row_text(config: &crate::config::Config, task: &crate::db::TaskRow, body: bool) -> String {
    let glyph = crate::badge::task_badge(task, config, false).0;
    let mut text = format!(
        "{glyph} {}",
        task.name.ellipsize(16, std::fmt::Alignment::Left)
    );
    if let Some(short_id) = task.short_id {
        text.push_str(&format!(" (#{short_id})"));
    }
    if body && !task.body.is_empty() {
        text.push('\n');
        text.push_str(&task.body);
    }
    text
}

pub fn build_preview(
    task: &crate::db::TaskRow,
    today: bool,
    config: &crate::config::Config,
    linked_moods: &[crate::db::MoodRow],
    axes: Option<&crate::color::ColorAxes>,
    parent: Option<&crate::db::TaskRow>,
    tree: Option<&crate::task_tree::TaskTree>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Start with a blank line so the heading reads as a heading.
    lines.push(Line::default());

    let (title, title_color) = if task.is_recurring() {
        ("RECURRING", Color::Blue)
    } else if task.is_scheduled() {
        ("SCHEDULED", Color::LightRed)
    } else {
        ("TASK", Color::Yellow)
    };
    lines.push(Line::from(Span::styled(
        format!(" {}", title),
        Style::default()
            .fg(title_color)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(title.len() + 2),
        Style::default().fg(Color::DarkGray),
    )));

    // Task name, indented, white, italic.
    lines.push(Line::from(Span::styled(
        format!("  {}", task.name),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::ITALIC),
    )));

    // Blank line, then the fields.
    lines.push(Line::default());
    // The short id is shown only while the task is not completed — a
    // completed task's short id is cleared, so the ID field disappears.
    if !task.is_done()
        && let Some(short_id) = task.short_id
    {
        lines.push(field_line("id", short_id.to_string()));
    }
    lines.push(field_line("priority", task.priority.to_string()));
    // The parent task, when the task is attached to one: the parent's
    // row text (`{badge} {name} (#{short id})`) after the field label —
    // the short id is omitted for completed parents (their id is
    // cleared).
    if let Some(parent) = parent {
        lines.push(Line::from(vec![
            Span::styled("  parent: ", Style::default().fg(Color::Yellow)),
            Span::raw(task_row_text(config, parent, false)),
        ]));
    }
    // The last completion on the task. Done rows show it only when
    // `preview.show_last_when_done` is set (the default). Today-view
    // recurring rows carry the unscoped last completion in `end_time`
    // (their `last_time` is window-scoped); everywhere else `last_time` is
    // unscoped for recurring rows too.
    if !task.is_done() || config.preview.show_last_when_done {
        let last = if today && task.is_recurring() {
            task.end_time
        } else {
            task.last_time
        };
        if let Some(last) = last {
            lines.push(field_line(
                "last",
                date::format_human_datetime(last, config.preview.named_months),
            ));
        }
    }

    if task.is_recurring() {
        // Recurring tasks show when the next interval opens instead of
        // a fixed start time.
        if let Some(st) = task.start_time {
            let now = date::now();
            let next = match task.interval_span() {
                Some(span) if crate::date::span_rough_seconds(span) > 0.0 => {
                    if now <= st {
                        st
                    } else {
                        // Next interval start = end of the current interval.
                        crate::date::interval_end_unix_secs(st, span, now).unwrap_or(st)
                    }
                }
                _ => st,
            };
            lines.push(field_line(
                "next",
                date::format_human_datetime(next, config.preview.named_months),
            ));
        }
    } else if task.is_scheduled() {
        // Scheduled tasks show the window start.
        if let Some(st) = task.start_time {
            lines.push(field_line(
                "start",
                date::format_human_datetime(st, config.preview.named_months),
            ));
        }
    } else {
        // Oneshot tasks: the creation time always, and the due time only
        // when one was set (`! name @<time>` → end_time). Overdue tasks
        // color the `due:` label with the configured overdue color
        // (`badge::task_label_color`, falling back to the usual yellow).
        if let Some(st) = task.start_time {
            lines.push(field_line(
                "creation",
                date::format_human_datetime(st, config.preview.named_months),
            ));
        }
        if let Some(et) = task.end_time {
            let color =
                crate::badge::task_label_color(task, date::now(), config.tasks.overdue_color)
                    .map(Color::from_crossterm)
                    .unwrap_or(Color::Yellow);
            lines.push(Line::from(vec![
                Span::styled("  due: ", Style::default().fg(color)),
                Span::raw(date::format_human_datetime(et, config.preview.named_months)),
            ]));
        }
    }

    // Scheduled window: the availability duration and the current state
    // (ongoing / completed / auto-completed / failed).
    if task.is_scheduled() {
        if let Some(avail) = task.available_duration_secs {
            lines.push(field_line("duration", date::format_duration(avail)));
        }
        let now = date::now();
        let state = match task.completions {
            Some(c) if c > 0 => "completed",
            Some(_) => "failed",
            None => {
                let elapsed = task.start_time.unwrap_or(now)
                    + task.available_duration_secs.unwrap_or(0)
                    < now;
                if elapsed { "auto-completed" } else { "ongoing" }
            }
        };
        lines.push(field_line("state", state.to_string()));
    }

    // Recurring metadata: interval, availability window, end, optional.
    if task.is_recurring() {
        if let Some(span) = task.interval_span() {
            lines.push(field_line("interval", date::format_span(&span)));
        }
        if let Some(avail) = task.available_duration_secs {
            lines.push(field_line("duration", date::format_duration(avail)));
        }
        // Today-view rows carry the unscoped last completion in `end_time`
        // instead of the expiry — no `ends` field there.
        if !today && let Some(ref s) = task.end_datetime(config.preview.named_months) {
            lines.push(field_line("ends", s.clone()));
        }
        // The optional flag is only shown when the task is skippable.
        if task.optional != 0 {
            lines.push(field_line("optional", "Yes".to_string()));
        }
    }

    // Linked moods (`im good -5` recorded the link): a `moods:` field
    // with one `  - {badge} {mood text}` line per linked mood. The badge
    // color resolves via the process-wide mood-color cache; on a miss the
    // pipeline runs here (background preview task — never the render
    // thread) and writes the result back.
    if !linked_moods.is_empty() {
        lines.push(field_line("moods", String::new()));
        for mood in linked_moods {
            // Journal-only rows (empty mood) have no badge to show.
            if mood.mood.is_empty() {
                continue;
            }
            let color = crate::color::mood_color_with_backfill(axes, mood)
                .map(|oklab| {
                    let rgb = oklab.to_srgb();
                    Color::Rgb(rgb.r, rgb.g, rgb.b)
                })
                .unwrap_or(Color::DarkGray);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  - {}", config.badges.mood.unwrap_or('●')),
                    Style::default().fg(color),
                ),
                Span::raw(format!(" {}", mood.mood)),
            ]));
        }
    }

    // Progress bar for counted tasks: after the fields and above the
    // body, with a blank line on each side.
    if task.target_count > 0 {
        lines.push(Line::default());
        let done = task.completions.unwrap_or(0);
        let target = task.target_count;
        let bar_width = 20usize;
        let filled = ((done as f64 / target as f64) * bar_width as f64).round() as usize;
        let filled = filled.min(bar_width);
        let empty = bar_width.saturating_sub(filled);
        let bar = format!(
            "  [{}{}] {}/{}",
            "█".repeat(filled),
            "░".repeat(empty),
            done,
            target
        );
        lines.push(Line::from(Span::styled(
            bar,
            Style::default().fg(if done >= target {
                Color::Green
            } else {
                Color::White
            }),
        )));
    }

    // Body: a blank line, then the text indented.
    if !task.body.is_empty() {
        lines.push(Line::default());
        for line_str in task.body.lines() {
            lines.push(Line::from(format!("  {}", line_str)));
        }
    }

    // Children (the task tree's subtree below this task): a blank line,
    // then the subtree via `TaskTree::draw`, starting at indent 2 — each
    // child renders through `task_row_text` (`- {badge} {name}
    // (#{short id})`, with the body on following lines when present).
    if let Some(tree) = tree
        && !tree.root.children.is_empty()
    {
        lines.push(Line::default());
        lines.extend(
            tree.draw(2, 2, |child| format!("- {}", task_row_text(config, child, true)))
                .into_iter()
                .map(Line::raw),
        );
    }

    lines
}

/// Build the preview pane for a today-view entry. Same heading shape as
/// [`build_preview`], titled after the entry type in full caps and bold:
/// "MOOD" (cyan, italic) when the entry carries a mood, "JOURNAL"
/// (gray) for moodless journal-only entries, "TRACKER" (dark gray) for
/// tracker entries. Journal-only entries skip the mood segment, showing
/// the body directly after the date. A blank line always follows the
/// date. Tracker entries attached to a mood (`tracker.mood`) show a
/// `mood:` field with the badge in the mood's own color (the sync
/// mood-color pipeline; `axes` comes from `config.moods.color_axes`).
pub(crate) fn build_today_preview(
    entry: &TodayEntry,
    config: &crate::config::Config,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Start with a blank line so the heading reads as a heading.
    lines.push(Line::default());

    let (title, title_style): (String, Style) = match entry.kind {
        EntryKind::Mood => (
            "MOOD".to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        ),
        EntryKind::Journal => (
            "JOURNAL".to_string(),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        EntryKind::Tracker(_) => (
            "TRACKER".to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        // Task entries normally render via build_preview (they carry a
        // selected TaskRow); this is the fallback for entries reaching here
        // without one.
        EntryKind::Task(_) => (
            "TASK".to_string(),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
    };
    lines.push(Line::from(Span::styled(format!(" {}", title), title_style)));
    lines.push(Line::from(Span::styled(
        "─".repeat(title.chars().count() + 2),
        Style::default().fg(Color::DarkGray),
    )));

    if entry.kind == EntryKind::Journal {
        // Journal-only: skip the mood segment — the date, then the body
        // directly (always a blank line after the date).
        lines.push(date_line(entry.time));
        if let Some(dur) = entry.duration {
            lines.push(field_line("duration", date::format_duration(dur)));
        }
        lines.push(Line::default());
        for line_str in entry.body.lines() {
            lines.push(Line::from(format!("  {}", line_str)));
        }
        return lines;
    }

    // Mood string (or tracker label), indented, white, italic.
    if !entry.label.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", entry.label),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::ITALIC),
        )));
    }

    // Date after the name, right-aligned and dark gray, with a blank
    // line always following it.
    lines.push(date_line(entry.time));

    if let Some(dur) = entry.duration {
        lines.push(field_line("duration", date::format_duration(dur)));
    }

    lines.push(Line::default());

    // Interval trackers show when the next interval opens — like recurring
    // tasks.
    if let Some((anchor, span)) = entry.tracker_interval
        && crate::date::span_rough_seconds(span) > 0.0
    {
        let now = date::now();
        let next = if now <= anchor {
            anchor
        } else {
            // Next interval start = end of the current interval.
            crate::date::interval_end_unix_secs(anchor, span, now).unwrap_or(anchor)
        };
        lines.push(field_line(
            "next",
            date::format_human_datetime(next, config.preview.named_months),
        ));
    }

    // `prev:` shows the previous entry of this kind whenever one exists.
    if let Some(prev) = entry.tracker_prev {
        lines.push(field_line(
            "prev",
            date::format_human_datetime(prev, config.preview.named_months),
        ));
    }

    // Tracker entries attached to a mood (`tracker.mood`): a `mood:`
    // field with the badge in the mood's own color. The color is a pure
    // lookup in the process-wide mood-color cache — the mood's raw row
    // rides the fetch's color handoff, so the background fill covers it
    // (a miss renders the neutral fallback and self-heals on repopulate).
    // Journal-only linked rows (empty mood) have no badge to show.
    if let Some(mood) = entry.linked_mood.as_ref()
        && !mood.is_empty()
    {
        let color = crate::color::cached_mood_color(mood)
            .map(|oklab| {
                let rgb = oklab.to_srgb();
                Color::Rgb(rgb.r, rgb.g, rgb.b)
            })
            .unwrap_or(Color::DarkGray);
        lines.push(Line::from(vec![
            Span::styled("  mood: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                config.badges.mood.unwrap_or('●').to_string(),
                Style::default().fg(color),
            ),
            Span::raw(format!(" {}", mood)),
        ]));
    }

    // Linked trackers and tasks (mood entries): a `linked:` field with one
    // `  - {tracker}: {payload}` line per attached tracker (the name in the
    // tracker's own color, matching the main `name: value` label format;
    // payload omitted when the tracker carries none) and one
    // `  - {badge} {task name}` line per linked task.
    if !entry.linked_trackers.is_empty() || !entry.linked_tasks.is_empty() {
        lines.push(field_line("linked", String::new()));
        for t in &entry.linked_trackers {
            let mut spans = vec![
                Span::raw("  - "),
                Span::styled(format!("{}:", t.name), Style::default().fg(t.color)),
            ];
            if !t.payload.is_empty() {
                spans.push(Span::raw(format!(" {}", t.payload)));
            }
            lines.push(Line::from(spans));
        }
        for t in &entry.linked_tasks {
            let mut spans = vec![Span::raw("  - ")];
            if let Some(badge) = t.badge {
                spans.push(Span::styled(
                    badge.to_string(),
                    Style::default().fg(t.color),
                ));
            }
            spans.push(Span::raw(format!(" {}", t.name)));
            lines.push(Line::from(spans));
        }
    }

    // Body: a blank line, then the text indented.
    if !entry.body.is_empty() {
        lines.push(Line::default());
        for line_str in entry.body.lines() {
            lines.push(Line::from(format!("  {}", line_str)));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recurring_row() -> crate::db::TaskRow {
        crate::db::TaskRow {
            id: 1,
            short_id: Some(7),
            name: "water plants".to_string(),
            body: String::new(),
            priority: 3,
            start_time: Some(1_700_000_000),
            available_duration_secs: Some(3600),
            interval_secs: Some(crate::date::span_to_db(&jiff::Span::new().days(1))),
            target_count: 0,
            optional: 0,
            end_time: Some(1_700_500_000),
            parent: None,
            completions: Some(0),
            last_time: Some(1_700_400_000),
        }
    }

    fn config() -> crate::config::Config {
        let mut c = crate::config::Config::default();
        // The tests exercise the `last:` field on done rows.
        c.preview.show_last_when_done = true;
        c
    }

    /// The values of the `field: value` lines, e.g. `["id: 7", "last: ..."]`.
    fn fields(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.to_string()).collect();
                text.strip_prefix("  ").map(|s| s.trim().to_string())
            })
            .filter(|s| s.contains(':'))
            .collect()
    }

    #[test]
    fn test_build_preview_today_recurring_last_from_end_time() {
        let task = recurring_row();
        let lines = build_preview(&task, true, &config(), &[], None, None, None);
        let fields = fields(&lines);
        // `last` reads the unscoped completion carried in `end_time`, and
        // the `ends` field is skipped (end_time is not the expiry here).
        assert!(
            fields.iter().any(|f| f == &format!(
                "last: {}",
                date::format_human_datetime(1_700_500_000, true)
            )),
            "expected last: from end_time, got {fields:?}"
        );
        assert!(!fields.iter().any(|f| f.starts_with("ends:")), "{fields:?}");
    }

    #[test]
    fn test_build_preview_not_today_recurring_last_from_last_time() {
        let task = recurring_row();
        let lines = build_preview(&task, false, &config(), &[], None, None, None);
        let fields = fields(&lines);
        assert!(
            fields.iter().any(|f| f == &format!(
                "last: {}",
                date::format_human_datetime(1_700_400_000, true)
            )),
            "expected last: from last_time, got {fields:?}"
        );
        assert!(fields.iter().any(|f| f == &format!(
            "ends: {}",
            date::format_human_datetime(1_700_500_000, true)
        )));
    }

    #[test]
    fn test_build_preview_done_shows_last() {
        let mut task = recurring_row();
        task.completions = Some(1); // target 0 -> done
        let fields = fields(&build_preview(
            &task,
            true,
            &config(),
            &[],
            None,
            None,
            None,
        ));
        assert!(
            fields.iter().any(|f| f == &format!(
                "last: {}",
                date::format_human_datetime(1_700_500_000, true)
            )),
            "expected last: on a done row, got {fields:?}"
        );
    }

    /// A tracker entry preview shows `prev:` (the previous entry of this
    /// kind, human-formatted) when one exists, and no `prev:`/`last:` field
    /// at all otherwise.
    #[test]
    fn test_build_today_preview_prev() {
        let mk = |tracker_prev: Option<i64>| TodayEntry {
            id: Some(1),
            time: 1_700_000_000,
            time_label: "18:00".to_string(),
            kind: EntryKind::Tracker(crate::config::TrackerKind::Float),
            label: "sleep: 7.5".to_string(),
            body: String::new(),
            task_id: None,
            priority: 0,
            task: None,
            score: None,
            linked_mood: None,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
            duration: None,
        };
        let rendered: Vec<String> = build_today_preview(&mk(Some(1_699_000_000)), &config())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            rendered.iter().any(|l| l == &format!(
                "  prev: {}",
                date::format_human_datetime(1_699_000_000, true)
            )),
            "expected a prev: field, got {rendered:?}"
        );
        assert!(
            !rendered.iter().any(|l| l.contains("last:")),
            "{rendered:?}"
        );

        let rendered: Vec<String> = build_today_preview(&mk(None), &config())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            !rendered.iter().any(|l| l.trim_start().starts_with("prev:")),
            "expected no prev: field, got {rendered:?}"
        );
    }

    /// A mood entry with attached trackers and linked tasks shows a
    /// `linked:` field with one `  - {tracker} {payload}` line per tracker
    /// (name in the tracker's color) and one `  - {badge} {task name}` line
    /// per task; null-tracker payloads are omitted.
    #[test]
    fn test_build_today_preview_linked() {
        use crate::today::{LinkedTask, LinkedTracker};
        let entry = TodayEntry {
            id: Some(1),
            time: 1_700_000_000,
            time_label: "18:00".to_string(),
            kind: EntryKind::Mood,
            label: "good".to_string(),
            body: String::new(),
            task_id: None,
            priority: 0,
            task: None,
            score: None,
            linked_mood: None,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: vec![
                LinkedTracker {
                    name: "sleep".to_string(),
                    payload: "7.5".to_string(),
                    color: Color::LightBlue,
                },
                // Null trackers carry the entry moment as their payload.
                LinkedTracker {
                    name: "sitting".to_string(),
                    payload: "3-15 14:30".to_string(),
                    color: Color::LightYellow,
                },
            ],
            linked_tasks: vec![LinkedTask {
                badge: Some('✓'),
                color: Color::Green,
                name: "water plants".to_string(),
            }],
            duration: None,
        };
        let rendered: Vec<String> = build_today_preview(&entry, &config())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.trim_start().starts_with("linked:")),
            "expected a linked: field, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "  - sleep: 7.5"),
            "expected a '  - sleep: 7.5' line, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "  - sitting: 3-15 14:30"),
            "expected a '  - sitting: 3-15 14:30' line, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "  - ✓ water plants"),
            "expected a '  - ✓ water plants' line, got {rendered:?}"
        );
    }

    /// A task with linked moods shows a `moods:` field with one
    /// `  - ● mood` line per linked mood (empty-mood journal rows are
    /// skipped).
    /// A tracker entry attached to a mood (`tracker.mood`) shows a
    /// `mood:` field with the badge + mood text; entries without a linked
    /// mood (or linked to a journal-only row) show nothing.
    #[test]
    fn test_build_today_preview_linked_mood() {
        let mk = |linked_mood: Option<String>| TodayEntry {
            id: Some(1),
            time: 1_700_000_000,
            time_label: "18:00".to_string(),
            kind: EntryKind::Tracker(crate::config::TrackerKind::Float),
            label: "sleep: 7.5".to_string(),
            body: String::new(),
            task_id: None,
            priority: 0,
            task: None,
            score: None,
            linked_mood,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
            duration: None,
        };
        let rendered: Vec<String> = build_today_preview(&mk(Some("good".to_string())), &config())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            rendered.iter().any(|l| l == "  mood: ● good"),
            "expected a '  mood: ● good' line, got {rendered:?}"
        );
        // No linked mood → no mood: field.
        let rendered: Vec<String> = build_today_preview(&mk(None), &config())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            !rendered.iter().any(|l| l.trim_start().starts_with("mood:")),
            "unexpected mood: field, got {rendered:?}"
        );
        // A journal-only linked row (empty mood) has no badge to show.
        let rendered: Vec<String> = build_today_preview(&mk(Some(String::new())), &config())
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            !rendered.iter().any(|l| l.trim_start().starts_with("mood:")),
            "unexpected mood: field for a journal-only link, got {rendered:?}"
        );
    }

    /// The date line is always followed by a blank line in the today-view
    /// preview (journal entries included).
    #[test]
    fn test_build_today_preview_blank_line_after_date() {
        let mk = |kind: EntryKind| TodayEntry {
            id: Some(1),
            time: 1_700_000_000,
            time_label: "18:00".to_string(),
            kind,
            label: if kind == EntryKind::Journal {
                String::new()
            } else {
                "good".to_string()
            },
            body: "body".to_string(),
            task_id: None,
            priority: 0,
            task: None,
            score: None,
            linked_mood: None,
            recurring_window: None,
            tracker_interval: None,
            tracker_prev: None,
            linked_trackers: Vec::new(),
            linked_tasks: Vec::new(),
            duration: None,
        };
        for kind in [EntryKind::Journal, EntryKind::Mood] {
            let rendered: Vec<String> = build_today_preview(&mk(kind), &config())
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
                .collect();
            let date_idx = rendered
                .iter()
                .position(|l| l == &date::format_datetime(1_700_000_000))
                .expect("a date line");
            assert!(
                rendered.get(date_idx + 1).is_some_and(|l| l.is_empty()),
                "expected a blank line after the date, got {rendered:?}"
            );
        }
    }

    /// The `due:` label of an overdue oneshot task uses the configured
    /// overdue color; a not-overdue task keeps the yellow label.
    #[test]
    fn test_build_preview_overdue_due_label() {
        let mut task = recurring_row();
        task.interval_secs = None;
        task.available_duration_secs = None; // oneshot: not scheduled
        task.start_time = Some(date::now() - 86_400);
        task.end_time = Some(date::now() + 86_400);
        task.completions = None;
        let cfg = config();
        let overdue = Color::from_crossterm(cfg.tasks.overdue_color);

        // Not overdue (end_time in the future): yellow label.
        let due_style = |lines: &[Line<'static>]| {
            lines
                .iter()
                .find(|l| {
                    l.spans
                        .first()
                        .is_some_and(|s| s.content.as_ref() == "  due: ")
                })
                .and_then(|l| l.spans.first())
                .and_then(|s| s.style.fg)
        };
        let lines = build_preview(&task, false, &cfg, &[], None, None, None);
        assert_eq!(due_style(&lines), Some(Color::Yellow));

        // Overdue (end_time in the past): overdue color.
        task.end_time = Some(date::now() - 1);
        let lines = build_preview(&task, false, &cfg, &[], None, None, None);
        assert_eq!(due_style(&lines), Some(overdue));
    }

    /// A task with children shows the subtree after the body, separated
    /// by a blank line; a task without children shows nothing extra.
    #[test]
    fn test_build_preview_children() {
        use crate::task_tree::{TaskTree, TaskTreeNode};
        let mut task = recurring_row();
        task.interval_secs = None;
        task.body = "parent body".to_string();
        let child = crate::db::TaskRow {
            id: 2,
            short_id: Some(2),
            name: "child task".to_string(),
            body: String::new(),
            priority: 5,
            start_time: None,
            available_duration_secs: None,
            interval_secs: None,
            target_count: 0,
            optional: 0,
            end_time: None,
            parent: Some(1),
            completions: None,
            last_time: None,
        };
        let tree = TaskTree {
            root: TaskTreeNode {
                row: task.clone(),
                children: vec![TaskTreeNode {
                    row: child,
                    children: Vec::new(),
                }],
            },
        };

        let rendered: Vec<String> =
            build_preview(&task, false, &config(), &[], None, None, Some(&tree))
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
                .collect();
        let body_idx = rendered.iter().position(|l| l == "  parent body").unwrap();
        assert_eq!(rendered.get(body_idx + 1), Some(&String::new()));
        // The subtree starts at indent 2 (`draw(2, ...)`); each row
        // renders through `task_row_text`, so the child's short id
        // follows the name.
        assert_eq!(
            rendered.get(body_idx + 2),
            Some(&"  - ○ child task (#2)".to_string())
        );

        // No children → nothing after the body.
        let bare = TaskTree {
            root: TaskTreeNode {
                row: task.clone(),
                children: Vec::new(),
            },
        };
        let rendered: Vec<String> =
            build_preview(&task, false, &config(), &[], None, None, Some(&bare))
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
                .collect();
        assert_eq!(rendered.last(), Some(&"  parent body".to_string()));
    }

    /// `task_row_text` renders `{badge} {name ellipsized to 16}
    /// (#{short_id})`, appends the body on following lines when `body`
    /// is set, and omits the short id when the task has none.
    #[test]
    fn test_task_row_text() {
        let mut task = recurring_row();
        // A plain oneshot: drop the recurring interval and the schedule.
        task.interval_secs = None;
        task.start_time = None;
        task.end_time = None;
        task.available_duration_secs = None;
        task.name = "very long child name that exceeds sixteen".to_string();
        task.body = "line one\nline two".to_string();
        task.short_id = Some(7);

        let cfg = config();
        assert_eq!(
            task_row_text(&cfg, &task, true),
            "○ very long child… (#7)\nline one\nline two"
        );
        assert_eq!(task_row_text(&cfg, &task, false), "○ very long child… (#7)");

        task.short_id = None;
        assert_eq!(task_row_text(&cfg, &task, false), "○ very long child…");
    }

    /// A task attached to a parent shows a `parent:` field with the
    /// parent's badge glyph, its name ellipsized to 16 columns, and its
    /// short id in parentheses.
    #[test]
    fn test_build_preview_parent_field() {
        let mut task = recurring_row();
        task.interval_secs = None;
        task.parent = Some(1);
        let parent = crate::db::TaskRow {
            id: 1,
            short_id: Some(42),
            name: "a very long parent task name that will not fit".to_string(),
            body: String::new(),
            priority: 5,
            start_time: Some(1_700_000_000),
            available_duration_secs: None,
            interval_secs: None,
            target_count: 0,
            optional: 0,
            end_time: None,
            parent: None,
            completions: None,
            last_time: None,
        };

        let lines = build_preview(&task, false, &config(), &[], None, Some(&parent), None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let parent_line = rendered
            .iter()
            .find(|l| l.starts_with("  parent: "))
            .unwrap();
        // `○ a very long par… (#42)`: the name is truncated to 16
        // columns (ellipsis included) with a trailing ellipsis; the short
        // id follows in parens.
        assert_eq!(parent_line, "  parent: ○ a very long par… (#42)");

        // A completed parent (short id cleared) shows no `(#…)` suffix.
        let mut done_parent = parent.clone();
        done_parent.short_id = None;
        done_parent.completions = Some(1);
        done_parent.target_count = 1;
        let lines = build_preview(&task, false, &config(), &[], None, Some(&done_parent), None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let parent_line = rendered
            .iter()
            .find(|l| l.starts_with("  parent: "))
            .unwrap();
        assert_eq!(parent_line, "  parent: ✓ a very long par…");

        // No parent row → no `parent:` field.
        let lines = build_preview(&task, false, &config(), &[], None, None, None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(!rendered.iter().any(|l| l.starts_with("  parent: ")));
    }

    #[test]
    fn test_build_preview_linked_moods() {
        let task = recurring_row();
        let mood = crate::db::MoodRow {
            id: 1,
            mood: "good".to_string(),
            body: String::new(),
            time: 1_700_000_000,
            embedding: None,
            score: None,
            duration: None,
            todo_id: None,
        };
        let lines = build_preview(&task, true, &config(), &[mood], None, None, None);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.trim_start().starts_with("moods:")),
            "expected a moods: field, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l == "  - ● good"),
            "expected a '  - ● good' line, got {rendered:?}"
        );
    }
}
