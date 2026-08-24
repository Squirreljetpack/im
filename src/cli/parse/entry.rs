use super::super::{Command, is_body_delimiter};
use crate::types::{Entry, TaskRef};

pub(crate) fn parse_entry_command(args: &[String]) -> anyhow::Result<Command> {
    // Body split comes first (like tasks): everything before the first
    // body delimiter (any arg of only dots) is parsed as mood / tracker
    // args; everything after is joined verbatim into `body` (a later
    // delimiter inside the body is literal text). A bare delimiter — no
    // text after it — carries its dot count in `Err(n)`; the handler opens
    // the editor with the `n`th template. No delimiter at all is `Err(0)`.
    //
    // Trackers are parsed only at the beginning and end of the head — the
    // mood words must be contiguous:
    //   im <mood> [-tracker value] [delimiter [body]]  — tracker(s) after
    //   the mood
    //   im -tracker value                       — tracker only (no mood)
    //   im [-tracker value]… <mood>             — tracker(s) before the mood
    // Once a `-tracker value` pair has been consumed *after* the mood
    // started, the rest of the head must stay tracker-shaped: another
    // `-tracker value` pair or the end of the head. A bare word after that
    // point is an error, e.g. `im pretty ok -sleep 8 but not great`
    // (the word after `8` is not another valid tracker pattern or the end
    // of the head). The body delimiter moves free text into the body
    // instead: `im pretty ok -sleep 8 . but not great` is valid.
    //
    // A single task reference (`+<id>` / `+<words>` / bare `+` to pick)
    // may appear anywhere in the head; the resolved task is linked to the
    // mood entry (`mood.todo_id`) at write time. At most one `+` per entry,
    // and a `+`-prefixed token is never consumed as a tracker value (so
    // `-tracker +7` parses as a valueless tracker plus a task ref).
    let (head, body_parts, delimiter_dots) = match args.iter().position(|a| is_body_delimiter(a)) {
        Some(d) => (&args[..d], &args[d + 1..], Some(args[d].len())),
        None => (args, &[][..], None),
    };

    let mut mood_parts: Vec<String> = Vec::new();
    let mut trackers: Vec<(String, String)> = Vec::new();
    let mut task_ref: Option<TaskRef> = None;
    // Completion delta from a nonempty `+ref <count>` pair.
    let mut count: Option<i32> = None;
    // Set once a `-tracker value` pair is seen after the mood started; from
    // then on a bare word is rejected (only tracker pairs / end of head).
    let mut after_mood_tracker = false;

    let mut i = 0;
    while i < head.len() {
        let arg = &head[i];
        match arg.as_str() {
            s if s.starts_with('+') => {
                // Task reference: +<id>, +<words>, or bare + (pick).
                if let Some(existing) = &task_ref {
                    cba::ebog!(
                        "task_ref";
                        "At most one task reference ('+') is allowed per entry \
                         (found '{}' after '{}')",
                        s,
                        task_ref_summary(existing)
                    );
                    anyhow::bail!("At most one task reference ('+') is allowed per entry");
                }
                // Like a tracker pair, a ref after the mood starts the
                // end-of-line run: only trackers/delimiter/end may follow.
                let trailing = !mood_parts.is_empty();
                let r = parse_task_ref_token(s)?;
                // A nonempty ref expects a count payload: consume one plain
                // numeric word. A bare '+' never takes a payload; a '-'
                // token only counts when purely numeric (`+7 -1` untoggles)
                // — other dash tokens stay trackers.
                let mut consumed = 1;
                if !s.eq("+") && i + 1 < head.len() {
                    let negative = head[i + 1].starts_with('-')
                        && head[i + 1].len() > 1
                        && head[i + 1][1..].chars().all(|c| c.is_ascii_digit());
                    let plain = !head[i + 1].starts_with('-') && !head[i + 1].starts_with('+');
                    if (negative || plain)
                        && let Ok(n) = head[i + 1].parse::<i32>()
                    {
                        count = Some(n);
                        consumed = 2;
                    }
                }
                task_ref = Some(r);
                if trailing {
                    after_mood_tracker = true;
                }
                i += consumed;
            }
            s if s.starts_with('-') && s != "-" => {
                // Tracker entry: -type value (e.g., -sleep 8, -accomplishment "fixed 2 bugs").
                // A -type followed by another dash token, a '+' task ref, or
                // by the end of the line parses as a valueless tracker
                // (Null trackers — `-sleep -xyz -withvalue abc` chains work);
                // the handler rejects empty values for text/number/float
                // kinds.
                let tracker_type = s[1..].to_string();
                if i + 1 < head.len()
                    && !head[i + 1].starts_with('-')
                    && !head[i + 1].starts_with('+')
                {
                    if !mood_parts.is_empty() {
                        after_mood_tracker = true;
                    }
                    trackers.push((tracker_type, head[i + 1].clone()));
                    i += 2;
                } else {
                    if !mood_parts.is_empty() {
                        after_mood_tracker = true;
                    }
                    trackers.push((tracker_type, String::new()));
                    i += 1;
                }
            }
            _ if after_mood_tracker => {
                cba::ebog!(
                    "tracker";
                    "Unexpected word '{}' after a tracker value: trackers are parsed only at the \
                     beginning and end of the line — after a mood's tracker pair, only more \
                     '-tracker value' pairs, a body delimiter (a word of only dots), or the \
                     end of the line may follow",
                    arg
                );
                anyhow::bail!(
                    "Unexpected word '{}' after the tracker value: once a tracker follows the \
                     mood, only more '-tracker value' pairs, a body delimiter (a word of \
                     only dots), or the end of the line may follow",
                    arg
                );
            }
            _ => {
                mood_parts.push(arg.clone());
                i += 1;
            }
        }
    }

    let (mood, duration) = if mood_parts.is_empty() {
        (String::new(), None)
    } else {
        let joined = mood_parts.join(" ");
        let trimmed = joined.trim();
        if trimmed == "%" {
            cba::ebog!(
                "session";
                "Mood session requires a duration (e.g. 'im %25m')"
            );
            anyhow::bail!("Mood session requires a duration (e.g. 'im %25m')");
        } else if let Some(rest) = joined.trim_start().strip_prefix('%') {
            let rest_trimmed = rest.trim();
            if rest_trimmed.is_empty() {
                cba::ebog!(
                    "session";
                    "Mood session requires a duration (e.g. 'im %25m')"
                );
                anyhow::bail!("Mood session requires a duration (e.g. 'im %25m')");
            }
            match crate::date::parse_duration_secs(rest_trimmed) {
                Ok(secs) => (String::new(), Some(secs)),
                Err(_) => {
                    cba::ebog!(
                        "session";
                        "Invalid mood session duration: '{}'",
                        rest_trimmed
                    );
                    anyhow::bail!("Invalid mood session duration: '{}'", rest_trimmed);
                }
            }
        } else {
            (joined, None)
        }
    };
    let body = match delimiter_dots {
        Some(dots) if body_parts.is_empty() => Err(dots),
        Some(_) => Ok(body_parts.join(" ")),
        None => Err(0),
    };

    // A ref without a count payload needs something to attach to: a line
    // with no mood, no session and no literal body creates no mood row.
    // (A bare delimiter opens the body editor, so it may still produce one
    // — the write path re-checks.) With a payload the task is updated
    // regardless, so no mood row is needed (`im +7 2`).
    if task_ref.is_some()
        && count.is_none()
        && mood.is_empty()
        && duration.is_none()
        && !matches!(body, Ok(ref b) if !b.trim().is_empty())
        && !matches!(body, Err(dots) if dots > 0)
    {
        cba::ebog!(
            "task_ref";
            "'+' task reference creates no mood entry here, so the link has nothing \
             to attach to (log a mood/journal/session first)"
        );
        anyhow::bail!(
            "'+' task reference creates no mood entry here, so the link has nothing \
             to attach to (log a mood/journal/session first)"
        );
    }

    // Mood must not contain tabs: view output uses tab separators.
    if mood.contains('\t') {
        anyhow::bail!("Mood cannot contain tab characters");
    }

    Ok(Command::Entry(Entry {
        mood,
        trackers,
        task_ref,
        count,
        body,
        duration,
    }))
}

/// Parse a single '+'-prefixed head token into a [`TaskRef`]: digits are
/// always an id (even `+0`), a bare `+` picks interactively, anything else
/// is a one-word query.
fn parse_task_ref_token(token: &str) -> anyhow::Result<TaskRef> {
    debug_assert!(token.starts_with('+'));
    let rest = &token[1..];
    if rest.is_empty() {
        return Ok(TaskRef::Pick);
    }
    if rest.chars().all(|c| c.is_ascii_digit()) {
        let id = rest.parse::<i64>()?;
        return Ok(TaskRef::Id(id));
    }
    Ok(TaskRef::Words(vec![rest.to_string()]))
}

/// Short human-readable form of a task ref for error messages.
fn task_ref_summary(r: &TaskRef) -> String {
    match r {
        TaskRef::Pick => "+".to_string(),
        TaskRef::Id(id) => format!("+{id}"),
        TaskRef::Words(w) => format!("+{}", w.join(" ")),
    }
}
