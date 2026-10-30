use anyhow::Context;

use super::super::{Command, is_body_delimiter};
use crate::types::{Task, TaskKind, TaskRef};

pub(crate) fn parse_task_command(mut args: &[String]) -> anyhow::Result<Command> {
    // The leading "!" has already been stripped by the caller — `args` holds
    // everything after it.

    let parent = if !args.is_empty() {
        parse_parent_task_ref(&args[0])?
    } else {
        None
    };

    if parent.is_some() {
        args = &args[1..];
    }

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

    // ! → interactive oneshot creation
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

    // `! %<interval> [[:]<name>]` → recurring creation. The interval is the
    // recurring field, so it is what `%` prefills; the available duration
    // (how long each occurrence stays open) is prompted in the flow. A `:`
    // word opens the name section exactly as it does for `! @…`, which also
    // lets the interval span words (`! %1 day :exercise`); without one, only
    // the first word is the interval and every word after it is the name.
    if args[0].starts_with('%') {
        let name_start = args.iter().position(|w| w.starts_with(':')).unwrap_or(1);
        let interval_words = &args[..name_start];
        let raw = {
            let mut s = interval_words[0]
                .strip_prefix('%')
                .unwrap_or(&interval_words[0])
                .to_string();
            for w in &interval_words[1..] {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str(w);
            }
            s
        };
        let interval = if raw.is_empty() {
            None
        } else {
            crate::date::parse_span(&raw)
                .with_context(|| format!("Invalid recurring task interval: '{}'", raw))?;
            Some(raw)
        };
        let prefill = join_name(&args[name_start..]);
        return Ok(Command::Task(Task {
            task_type: TaskKind::Recurring,
            name: None,
            priority: None,
            date: None,
            body,
            prefill,
            available_duration: None,
            parent: None,
            interval,
        }));
    }

    // `! :<name> %<interval>` → the interval opens the command, so a `%`
    // word inside the name section is not an interval.
    if args[0].starts_with(':') && args.iter().skip(1).any(|w| w.starts_with('%')) {
        cba::ebog!(
            "@recurring";
            "The interval must precede the name section (found '{}')",
            args[0]
        );
        anyhow::bail!("The interval must come first: use `! %<interval> [[:]<name>]`");
    }

    // `! @ ...` (bare `@`) → scheduled task creation (e.g. `! @`, `! @ :name %1h`)
    if args[0] == "@" {
        return parse_scheduled_task(&args[1..], body);
    }

    // `! @<time> [:name] [%<duration>]` → scheduled task
    if args[0].starts_with('@') {
        return parse_scheduled_task(args, body);
    }

    // Creating oneshot task: ! <name> [@<time> ...]
    let at = args.iter().position(|a| a.starts_with('@'));
    let (name_parts, time_parts) = match at {
        Some(a) => (&args[..a], &args[a..]),
        None => (args, &[][..]),
    };
    for word in time_parts.iter().skip(1) {
        if word.starts_with('@') {
            cba::ebog!(
                "@time";
                "Only one @<time> is allowed per task (found '{}')",
                word
            );
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

fn parse_parent_task_ref(arg: &str) -> anyhow::Result<Option<TaskRef>> {
    let Some(rest) = arg.strip_prefix('+') else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Ok(Some(TaskRef::Pick));
    }
    if rest.chars().all(|c| c.is_ascii_digit()) {
        let id = rest.parse::<i64>().context("Parent id must be a number")?;
        return Ok(Some(TaskRef::Id(id)));
    }
    let words = rest
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    Ok(Some(TaskRef::Words(words)))
}

/// Parse the single-argument task editor command: `im +` (pick), `im +<id>`
/// or `im +<words>` — the whole argument list is one '+' token. Multi-word
/// queries come from a single attached token (`+buy milk` is an *entry*
/// carrying a word-query ref, not an editor invocation).
pub(crate) fn parse_task_edit_command(args: &[String]) -> anyhow::Result<Command> {
    debug_assert_eq!(args.len(), 1, "editor routing passes exactly one argument");
    let first = &args[0];
    let rest = first
        .strip_prefix('+')
        .expect("routing guarantees a leading '+'");
    if rest.is_empty() {
        return Ok(Command::TaskEdit { task: None });
    }
    if rest.chars().all(|c| c.is_ascii_digit()) {
        let id = rest.parse::<i64>().context("Task ID must be a number")?;
        return Ok(Command::TaskEdit {
            task: Some(TaskRef::Id(id)),
        });
    }
    Ok(Command::TaskEdit {
        task: Some(TaskRef::Words(
            rest.split_whitespace().map(String::from).collect(),
        )),
    })
}
/// `! @<time> [:name] [%<duration>] [. [body]]` → scheduled task
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
