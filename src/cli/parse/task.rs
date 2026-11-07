use anyhow::Context;

use super::super::{Command, is_body_delimiter};
use super::parse_task_ref;
use crate::types::{Task, TaskKind, TaskRef};

pub(crate) fn parse_task_command(
    args: &[String],
    parent: Option<TaskRef>,
) -> anyhow::Result<Command> {
    // everything before the first body delimiter (any arg of only dots)
    // is the command arguments (name/time), everything after it is body
    // text. The body is `Ok(text)` when words followed the delimiter,
    // `Err(n)` when the delimiter was bare (n = its dot count, opening the
    // `n`th template in the handler), and `Err(0)` when absent. How a
    // bare/absent body is resolved (editor or not) is a handler concern.
    let (args, body) = match args.iter().position(|a| is_body_delimiter(a)) {
        Some(d) => {
            let joined = args[d + 1..].join(" ");
            let body = if joined.is_empty() {
                Err(args[d].len())
            } else {
                Ok(joined)
            };
            (&args[..d], body)
        }
        None => (args, Err(0)),
    };

    // interactive oneshot creation
    if args.is_empty() {
        return Ok(Command::Task(Task {
            task_type: TaskKind::Oneshot,
            name: None,
            priority: None,
            date: None,
            body,
            prefill: None,
            available_duration: None,
            parent,
            interval: None,
        }));
    }

    // A parent is only meaningful for oneshot creation; the interactive
    // recurring and scheduled flows have no parent field. Reject rather than
    // silently dropping a `+<ref>` the user supplied.
    if parent.is_some() && (args[0].starts_with('%') || args[0].starts_with('@')) {
        anyhow::bail!("A parent can only be attached when creating a oneshot task");
    }

    // Recurring (`%`) and scheduled (`@`) tasks:
    // `+ % [interval] [:<name>]` / `+ %<interval> [:<name>]` → recurring creation
    if args[0].starts_with('%') {
        return parse_recurring_task(args, body);
    }

    // `+ :<name> %<interval>` → the interval opens the command, so a `%`
    // word inside the name section is not an interval.
    if args[0].starts_with(':') && args.iter().skip(1).any(|w| w.starts_with('%')) {
        anyhow::bail!("The interval must come first: use `+ %<interval> [:<name>]`");
    }

    // `+ @ [time] [:<name>] [%<duration>]` / `+ @<time> [:<name>] [%<duration>]` → scheduled task
    if args[0] == "@" {
        return parse_scheduled_task(&args[1..], body);
    }
    if args[0].starts_with('@') {
        return parse_scheduled_task(args, body);
    }

    // Creating oneshot task: + <name> [@<time> ...], or +<ref> <name> ...
    // when the caller already extracted a parent.
    let at = args.iter().position(|a| a.starts_with('@'));
    let (name_parts, time_parts) = match at {
        Some(a) => (&args[..a], &args[a..]),
        None => (args, &[][..]),
    };
    for word in time_parts.iter().skip(1) {
        if word.starts_with('@') {
            anyhow::bail!("Only one @<time> is allowed per task");
        }
    }

    let name = if name_parts.is_empty() {
        None
    } else {
        let trimmed = name_parts
            .iter()
            .map(|s| if s == ":" { "\n" } else { s.as_str() })
            .collect::<Vec<_>>()
            .join(" ");
        let trimmed = trimmed.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };
    if name.as_deref() == Some("-") {
        anyhow::bail!("Task name cannot be '-'");
    }
    let date = if time_parts.is_empty() {
        None
    } else {
        // `time_parts[0]` is the word that opened the time field, so it
        // always starts with '@' — strip it, append the rest verbatim, and
        // resolve the whole field to an epoch now (an unparseable date
        // fails here, before anything is created).
        let mut parts = time_parts[0][1..].to_string();
        if time_parts.len() > 1 {
            parts.push(' ');
            parts.push_str(&time_parts[1..].join(" "));
        }
        Some(
            crate::date::parse_datetime(&parts, crate::date::DATE_DIALECT)
                .with_context(|| format!("Invalid task start time: '{}'", parts))?,
        )
    };

    Ok(Command::Task(Task {
        task_type: TaskKind::Oneshot,
        name,
        priority: None,
        date,
        body,
        prefill: None,
        available_duration: None,
        parent,
        interval: None,
    }))
}

/// `+ % [interval] [:<name>]` / `+ %[interval] [:<name>]` → recurring task
/// creation. A `:` word opens the name section; everything before it (after
/// stripping the leading `%`) is the interval.
fn parse_recurring_task(args: &[String], body: Result<String, usize>) -> anyhow::Result<Command> {
    let colon = args
        .iter()
        .position(|w| w.starts_with(':'))
        .unwrap_or(args.len());
    let interval_parts = &args[..colon];
    let name_parts = &args[colon..];

    let joined = interval_parts.join(" ");
    let raw = joined.strip_prefix('%').unwrap_or(&joined).trim();
    let interval = if raw.is_empty() {
        None
    } else {
        crate::date::parse_span(raw).with_context(|| {
            format!(
                "Invalid recurring task interval: '{}' (name starts with ':')",
                raw
            )
        })?;
        Some(raw.to_string())
    };

    let prefill = if name_parts.is_empty() {
        None
    } else {
        join_name(name_parts)
    };

    Ok(Command::Task(Task {
        task_type: TaskKind::Recurring,
        name: None,
        priority: None,
        date: None,
        body,
        prefill,
        available_duration: None,
        parent: None,
        interval,
    }))
}

/// Join the words of a `:`-marked name section: a bare `:` becomes a
/// newline, a `:` prefix on a word is its marker and is stripped, and plain
/// words are kept verbatim.
fn join_name(words: &[String]) -> Option<String> {
    let joined = words
        .iter()
        .map(|w| if w == ":" { "\n" } else { w.strip_prefix(':').unwrap_or(w) })
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parse the single-argument task editor command: `im +<id>` or `im +<words>`
/// — the whole argument list is one '+' token.
pub(crate) fn parse_task_edit_command(args: &[String]) -> anyhow::Result<Command> {
    debug_assert_eq!(args.len(), 1, "editor routing passes exactly one argument");
    let task = match parse_task_ref(&args[0])? {
        TaskRef::Pick => anyhow::bail!("Task reference required for editing"),
        r => r,
    };
    Ok(Command::TaskEdit { task })
}
/// `+ @ [time] [:name] [%<duration>] [. [body]]` → scheduled task
/// creation. `args` holds the command words (the body split already
/// happened in `parse_task_command`); `body` carries the post-delimiter
/// text or the bare delimiter's dot count.
fn parse_scheduled_task(args: &[String], body: Result<String, usize>) -> anyhow::Result<Command> {
    // The first marker word ends the time field. The dispatcher guarantees
    // the first word starts with '@', so the time field is never empty.
    let colon = args.iter().position(|w| w.starts_with(':'));
    let pct = args.iter().position(|w| w.starts_with('%'));
    let first = match (colon, pct) {
        (Some(c), Some(p)) => c.min(p),
        (Some(c), None) => c,
        (None, Some(p)) => p,
        (None, None) => args.len(),
    };

    let time_parts = &args[..first];
    let tail = &args[first..];

    let (mut name_parts, mut duration) = (&[][..], &[][..]);

    match tail.split_first() {
        None => {}
        Some((first_word, rest)) => {
            let first_is_name = first_word.starts_with(':');
            let other_marker = if first_is_name { '%' } else { ':' };

            match rest.iter().position(|w| w.starts_with(other_marker)) {
                Some(i) => {
                    let first_field = &tail[..=i];
                    let second_field = &tail[i + 1..];

                    if first_is_name {
                        name_parts = first_field;
                        duration = second_field;
                    } else {
                        duration = first_field;
                        name_parts = second_field;
                    }
                }
                None => {
                    if first_is_name {
                        name_parts = tail;
                    } else {
                        duration = tail;
                    }
                }
            }
        }
    }

    let date = if time_parts.is_empty() {
        None
    } else {
        let joined = time_parts.join(" ");
        let s = joined.strip_prefix('@').unwrap_or(&joined);
        Some(
            crate::date::parse_datetime(s, crate::date::DATE_DIALECT).with_context(|| {
                format!(
                    "Invalid scheduled task start time: '{}' \
                 (name starts with ':', duration with '%')",
                    s
                )
            })?,
        )
    };

    let name = join_name(name_parts);

    let available_duration = if duration.is_empty() {
        None
    } else {
        // Only the first word carries the `%` marker — strip it so the
        // segment joins cleanly. Marker words that land later in the
        // segment (a stray `:` or a duplicate `%`) are left verbatim and
        // fail the duration parse, which is the intended error path.
        let mut words = duration.iter();
        let mut d = words
            .next()
            .map(|w| w.strip_prefix('%').unwrap_or(w).to_string())
            .unwrap_or_default();
        for w in words {
            if !d.is_empty() {
                d.push(' ');
            }
            d.push_str(w);
        }
        Some(
            crate::date::parse_duration_secs(&d)
                .with_context(|| format!("Invalid scheduled task duration: '{}'", d))?,
        )
    };

    Ok(Command::Task(Task {
        task_type: TaskKind::Scheduled,
        name,
        priority: None,
        date,
        body,
        prefill: None,
        available_duration,
        parent: None,
        interval: None,
    }))
}
