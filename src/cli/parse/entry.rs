use super::super::{Command, is_body_delimiter};
use super::{is_numeric_token, parse_count_token, parse_task_ref};
use crate::types::{Entry, TaskKind, TaskRef};

pub(crate) fn parse_entry_command(args: &[String]) -> anyhow::Result<Command> {
    let (head, body_parts, delimiter_dots) = match args.iter().position(|a| is_body_delimiter(a)) {
        Some(d) => (&args[..d], &args[d + 1..], Some(args[d].len())),
        None => (args, &[][..], None),
    };

    let body = match delimiter_dots {
        Some(dots) if body_parts.is_empty() => Err(dots),
        Some(_) => Ok(body_parts.join(" ")),
        None => Err(0),
    };

    if let Some(first) = head.first()
        && first.starts_with('%')
    {
        return parse_session_command(head, body);
    }

    parse_standard_entry_command(head, body)
}

fn parse_session_command(
    head: &[String],
    body: Result<String, usize>,
) -> anyhow::Result<Command> {
    let first = &head[0];
    let (duration_str, tail) = if first == "%" {
        if head.len() < 2 || head[1].starts_with('-') || head[1].starts_with('+') {
            anyhow::bail!("Mood session requires a duration (e.g. 'im %25m')");
        }
        (&head[1][..], &head[2..])
    } else {
        (&first[1..], &head[1..])
    };

    if duration_str.is_empty() {
        anyhow::bail!("Mood session requires a duration (e.g. 'im %25m')");
    }

    // Pure number is parsed in seconds; otherwise humantime parses it.
    let secs: i64 = if duration_str.chars().all(|c| c.is_ascii_digit()) {
        duration_str.parse::<i64>()?
    } else {
        match crate::date::parse_duration_secs(duration_str) {
            Ok(s) => s,
            Err(_) => {
                anyhow::bail!("Invalid mood session duration: '{}'", duration_str);
            }
        }
    };

    if secs <= 0 {
        anyhow::bail!("Invalid mood session duration: '{}'", duration_str);
    }

    // The first '+' or '-' indicates the end of the purpose block.
    // Tasks and trackers freely mix afterwards.
    let split = tail
        .iter()
        .position(|arg| arg.starts_with('+') || (arg.starts_with('-') && arg != "-"))
        .unwrap_or(tail.len());
    let purpose_words = &tail[..split];
    let (task_ref, count, trackers) = parse_links_and_trackers(&tail[split..])?;

    let purpose = if purpose_words.is_empty() {
        None
    } else {
        Some(purpose_words.join(" "))
    };

    if let Some(ref p) = purpose {
        if p.contains(':') {
            anyhow::bail!("Session purpose cannot contain ':'");
        }
        if p.contains('\t') {
            anyhow::bail!("Session purpose cannot contain tab characters");
        }
    }

    Ok(Command::Entry(Entry {
        mood: String::new(),
        trackers,
        task_ref,
        count,
        body,
        duration: Some(secs),
        purpose,
    }))
}

fn parse_standard_entry_command(
    head: &[String],
    body: Result<String, usize>,
) -> anyhow::Result<Command> {
    // The first '+' or '-' indicates the end of the mood block.
    // Tasks and trackers freely mix afterwards.
    let split = head
        .iter()
        .position(|arg| arg.starts_with('+') || (arg.starts_with('-') && arg != "-"))
        .unwrap_or(head.len());
    let mood_words = &head[..split];
    let (task_ref, count, trackers) = parse_links_and_trackers(&head[split..])?;

    let mood = if mood_words.is_empty() {
        String::new()
    } else {
        mood_words
            .iter()
            .map(|s| if s == ":" { "\n" } else { s.as_str() })
            .collect::<Vec<_>>()
            .join(" ")
    };

    if mood.contains('\t') {
        anyhow::bail!("Mood cannot contain tab characters");
    }

    let has_body = match &body {
        Ok(b) => !b.trim().is_empty(),
        Err(dots) => *dots > 0,
    };

    if task_ref.is_some()
        && count.is_none()
        && mood.is_empty()
        && trackers.is_empty()
        && !has_body
    {
        anyhow::bail!(
            "'+' task reference creates no mood entry here, so the link has nothing \
             to attach to (log a mood/journal/session first)"
        );
    }

    if mood.is_empty() && trackers.is_empty() && !has_body && count.is_none() {
        anyhow::bail!("Nothing to log");
    }

    Ok(Command::Entry(Entry {
        mood,
        trackers,
        task_ref,
        count,
        body,
        duration: None,
        purpose: None,
    }))
}

fn parse_links_and_trackers(
    tokens: &[String],
) -> anyhow::Result<(Option<TaskRef>, Option<i32>, Vec<(String, String)>)> {
    let mut task_ref: Option<TaskRef> = None;
    let mut count: Option<i32> = None;
    let mut trackers: Vec<(String, String)> = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.starts_with('+') {
            if let Some(existing) = &task_ref {
                anyhow::bail!(
                    "At most one task reference ('+') is allowed per entry (found '{}' after '{}')",
                    tok,
                    task_ref_summary(existing)
                );
            }
            let (r, c, consumed) = parse_task_ref_and_count(&tokens[i..])?;
            task_ref = Some(r);
            count = c;
            i += consumed;
        } else if tok.starts_with('-') && tok != "-" {
            let rest = &tok[1..];
            let stripped = rest.strip_prefix('-').unwrap_or(rest);
            if is_numeric_token(stripped) {
                anyhow::bail!("Pure numeric trackers are not allowed ('{}')", tok);
            }
            let tracker_name = rest.to_string();
            i += 1;
            let mut val_tokens = Vec::new();
            while i < tokens.len() {
                let next = &tokens[i];
                if next.starts_with('+') {
                    break;
                }
                if next.starts_with('-') && next != "-" && !is_numeric_token(&next[1..]) {
                    break;
                }
                val_tokens.push(next.clone());
                i += 1;
            }
            trackers.push((tracker_name, val_tokens.join(" ")));
        } else {
            anyhow::bail!(
                "Unexpected word '{}': once tasks or trackers begin, only more '+task [count]' or '-tracker [value]' may follow",
                tok
            );
        }
    }

    Ok((task_ref, count, trackers))
}

fn parse_task_ref_and_count(slice: &[String]) -> anyhow::Result<(TaskRef, Option<i32>, usize)> {
    let token = &slice[0];
    let r = parse_task_ref(token)?;
    let mut count = None;
    let mut consumed = 1;

    if !matches!(r, TaskRef::Pick) && slice.len() > 1 {
        let second = &slice[1];
        if is_numeric_token(second) {
            match parse_count_token(second) {
                Some(n) => {
                    count = Some(n);
                    consumed = 2;
                }
                None => {
                    anyhow::bail!("Invalid completion count: '{}'", second);
                }
            }
        }
    }

    Ok((r, count, consumed))
}

fn task_ref_summary(r: &TaskRef) -> String {
    match r {
        TaskRef::Pick => "+".to_string(),
        TaskRef::Id(id) => format!("+{id}"),
        TaskRef::Words { words, kind } => {
            let prefix = match kind {
                TaskKind::Oneshot => "",
                TaskKind::Recurring => "@",
                TaskKind::Scheduled => "%",
            };
            format!("+{prefix}{}", words.join(" "))
        }
    }
}
