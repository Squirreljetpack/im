use std::env::args;

use super::parse::{
    is_numeric_token, parse_count_token, parse_entry_command, parse_special_command,
    parse_task_command, parse_task_edit_command, parse_task_ref, parse_view_command,
};
use super::{Cli, CliOpts, Command, FLAG_CHARACTERS, is_body_delimiter};
use crate::types::{TodayHorizon, ViewVariant};

/// Parse the full command line from `env::args` (skipping argv[0]) into a
/// [`Cli`]: leading `-q` / `-v` flags are stripped into `opts`, the rest is
/// parsed as a [`Command`].
pub fn parse_args() -> anyhow::Result<Cli> {
    let raw: Vec<String> = args().skip(1).collect();
    parse_cli(raw)
}

/// Parse flags + command from a pre-collected argument list. Used by tests.
pub fn parse_cli(args: Vec<String>) -> anyhow::Result<Cli> {
    // Flags are only recognized in the initial position: once a non-flag
    // token shows up, everything after it is the command's own arguments
    // (so `im ok -q` treats `-q` as entry text, not a flag). A flag
    // token is `-` followed by flag characters only (`-q`, `-v`, `-F`,
    // `-qv`, …); `q`/`v` increment the matching count in `opts.qv` and
    // `F` sets `opts.fullscreen`.
    let mut opts = CliOpts::default();
    let mut rest: Vec<String> = Vec::new();
    let mut is_help = false;
    let mut is_version = false;

    let mut in_flags = true;
    for arg in args {
        if in_flags {
            if arg == "--help" {
                is_help = true;
                continue;
            }
            if arg == "--version" {
                is_version = true;
                continue;
            }
            if arg == "--quiet" {
                opts.qv[0] += 1;
                continue;
            }
            match arg.strip_prefix('-') {
                Some(s) if !s.is_empty() && s.chars().all(|c| FLAG_CHARACTERS.contains(c) || c == 'h' || c == 'V') => {
                    for c in s.chars() {
                        match c {
                            'q' => opts.qv[0] += 1,
                            'v' => opts.qv[1] += 1,
                            'F' => opts.fullscreen = true,
                            'h' => is_help = true,
                            'V' => is_version = true,
                            _ => unreachable!(), // all() guard above
                        }
                    }
                    continue; // stays in_flags
                }
                _ => in_flags = false,
            }
        }
        rest.push(arg);
    }

    if is_help {
        return Ok(Cli {
            opts,
            cmd: Command::Help,
        });
    }

    if is_version {
        return Ok(Cli {
            opts,
            cmd: Command::Version,
        });
    }

    Ok(Cli {
        opts,
        cmd: parse_from(rest)?,
    })
}

/// Parse a command from a pre-collected argument list (flags already
/// stripped). Used by tests and internally by [`parse_cli`].
pub fn parse_from(args: Vec<String>) -> anyhow::Result<Command> {
    // No args → Today view (bare `im`). Help and version are handled one level
    // up in parse_cli (`-h` / `--help`, `-V` / `--version`, initial position
    // only) — parse_from treats a `-h`-style or `-V`-style token as entry text.
    if args.is_empty() {
        return Ok(Command::Today {
            date: None,
            show: ViewVariant::All,
            horizon: TodayHorizon::Today,
        });
    }

    let first = &args[0];

    // `im -` (exactly a bare dash) opens the tracker viewer — a
    // matchmaker-backed list of trackers. A dash carrying any other text
    // stays entry text (e.g. `im - ok` logs a mood entry).
    if first == "-" && args.len() == 1 {
        return Ok(Command::Matchmaker);
    }

    // Special commands starting with ':'
    if first.starts_with(':') {
        return parse_special_command(&args);
    }

    // View commands starting with '@'
    if first.starts_with('@') {
        return parse_view_command(&args);
    }

    // Bare `+` is task creation (no parent):
    //   im +                   — interactive oneshot creation
    //   im + [name]            — create oneshot task
    //   im + % [interval]      — create recurring task
    //   im + @ [time]          — create scheduled task
    if first == "+" {
        return parse_task_command(&args[1..], None);
    }

    // Attached `+` token (`+<task>`):
    //   im +task               — task edit (single argument)
    //   im +task <count> ..    — task completion (count is integer)
    //   im +task . [body]      — task-linked journal entry (empty mood)
    //   im +task -tracker ..   — task-linked tracker entry (empty mood)
    //   im +[ref] [name]       — create oneshot linked to parent
    if first.starts_with('+') {
        if args.len() == 1 {
            return parse_task_edit_command(&args);
        }
        let second = &args[1];
        if parse_count_token(second).is_some()
            || is_numeric_token(second)
            || is_body_delimiter(second)
            || (second.starts_with('-') && second != "-")
        {
            return parse_entry_command(&args);
        }
        let parent = parse_task_ref(first)?;
        return parse_task_command(&args[1..], Some(parent));
    }

    // Otherwise, it's an entry command
    parse_entry_command(&args)
}

#[cfg(test)]
mod tests {
    use super::super::{
        BODY_DELIMITER, ConfigTarget, DatetimeFormat, DbSubcommand, SyncSubcommand, ToolSubcommand,
        TrackerItem, TrackerPeriod,
    };
    use super::*;
    use crate::types::{Entry, Task, TaskKind, TaskRef, ViewMode};

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Expected epoch for a date string, parsed with the same fixed
    /// `DATE_DIALECT` the CLI parser uses — tests never assume a
    /// specific dialect value.
    fn ts(s: &str) -> i64 {
        crate::date::parse_datetime(s, crate::date::DATE_DIALECT).unwrap()
    }

    #[test]
    fn test_parse_mood_simple() {
        let cmd = parse_from(args(&["ok"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert!(entry.trackers.is_empty());
                assert_eq!(entry.body, Err(0));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_mood_with_editor() {
        let cmd = parse_from(args(&["ok", "."])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert_eq!(entry.body, Err(1));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_mood_with_trackers() {
        let cmd = parse_from(args(&["good", "-sleep", "8", "-water", "5"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(entry.trackers.len(), 2);
                assert_eq!(entry.trackers[0], ("sleep".to_string(), "8".to_string()));
                assert_eq!(entry.trackers[1], ("water".to_string(), "5".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_mood_multiline() {
        let cmd = parse_from(args(&["comfortably", "numb"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "comfortably numb");
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_tracker_only() {
        let cmd = parse_from(args(&["-sleep", "10"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.trackers.len(), 1);
                assert_eq!(entry.trackers[0], ("sleep".to_string(), "10".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_task_bare_is_interactive() {
        // `+` alone → interactive oneshot creation, no parent. (Regression
        // guard: parent parsing used to index into an empty args slice.)
        // No delimiter → body is `Err(0)`; the handler writes no body.
        let cmd = parse_from(args(&["+"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, None);
                assert_eq!(task.parent, None);
                assert_eq!(task.body, Err(0));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot() {
        let cmd = parse_from(args(&["+", "do", "something"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("do something".to_string()));
                assert_eq!(task.priority, None);
                assert_eq!(task.parent, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_with_parent() {
        let cmd = parse_from(args(&["+7", "do", "something"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("do something".to_string()));
                assert_eq!(task.parent, Some(TaskRef::Id(7)));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_bare_plus_after_plus_is_name() {
        // `im + +` creates a oneshot named "+". There is no interactive parent specifier.
        let cmd = parse_from(args(&["+", "+"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("+".to_string()));
                assert_eq!(task.parent, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_plus_word_after_ref_is_name() {
        // `im +1 +2` creates a child of task 1 named "+2": a '+' word after
        // `+<ref>` is name text, never a second entry task reference.
        let cmd = parse_from(args(&["+1", "+2"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("+2".to_string()));
                assert_eq!(task.parent, Some(TaskRef::Id(1)));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_parent_initial_position_only() {
        // Once a parent is parsed, later '-' words are ordinary text.
        let cmd = parse_from(args(&["+7", "buy", "-milk"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("buy -milk".to_string()));
                assert_eq!(task.parent, Some(TaskRef::Id(7)));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_dash_name_is_not_parent() {
        // A non-numeric '-word' is a name, not a parent flag.
        let cmd = parse_from(args(&["+", "-groceries"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("-groceries".to_string()));
                assert_eq!(task.parent, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_with_date() {
        let cmd = parse_from(args(&["+", "task", "@2024-03-20"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("task".to_string()));
                assert_eq!(task.date, Some(ts("2024-03-20")));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_datetime_multiple_words() {
        // Everything after the @ word joins the time field, so shell-split
        // datetimes survive: @2024-03-20 14:30:00 → "2024-03-20 14:30:00".
        let cmd = parse_from(args(&["+", "task", "@2024-03-20", "14:30:00"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("task".to_string()));
                assert_eq!(task.date, Some(ts("2024-03-20 14:30:00")));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_name_is_trimmed() {
        let cmd = parse_from(args(&["+", "  buy milk  "])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("buy milk".to_string()));
                assert_eq!(task.date, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_empty_name_after_trim_is_none() {
        // A whitespace-only name trims to empty → name None (the
        // handler rejects it with "Task name is required").
        let cmd = parse_from(args(&["+", "   "])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_at_in_body_is_literal() {
        // After the body delimiter, @ words are never treated as times.
        let cmd = parse_from(args(&["+", "task", ".", "@notdate"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("task".to_string()));
                assert_eq!(task.date, None);
                assert_eq!(task.body, Ok("@notdate".to_string()));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_oneshot_two_at_times_rejected() {
        // Only one @-word is allowed before the body delimiter; a second
        // is an error.
        assert!(parse_from(args(&["+", "task", "@a", "@b"])).is_err());
        // .. but inside the body state a second @ is fine (literal).
        let cmd = parse_from(args(&["+", "task", "@2024-03-20", ".", "@b"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.date, Some(ts("2024-03-20")));
                assert_eq!(task.body, Ok("@b".to_string()));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_delimiter_inside_body_is_literal() {
        // Everything after the first body delimiter is body text, including
        // a later delimiter token (the split is positional, not stateful
        // scanning).
        let cmd = parse_from(args(&["+", "task", ".", "see", ".", "note"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("task".to_string()));
                assert_eq!(task.body, Ok(format!("see {} note", BODY_DELIMITER)));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_create_bare() {
        // + @ → interactive scheduled creation with unprefilled start time.
        let cmd = parse_from(args(&["+", "@"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, None);
                assert_eq!(task.date, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_recurring_create_bare() {
        // + % → interactive recurring creation, prompting for the interval.
        let cmd = parse_from(args(&["+", "%"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Recurring);
                assert_eq!(task.name, None);
                assert_eq!(task.prefill, None);
                assert_eq!(task.interval, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_recurring_create_with_name() {
        // `+ %1day :exercise more` → the `:` opens the name section, so its
        // marker is stripped from the prefilled name.
        let cmd = parse_from(args(&["+", "%1day", ":exercise", "more"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Recurring);
                assert_eq!(task.name, None);
                assert_eq!(task.interval, Some("1day".to_string()));
                assert_eq!(task.prefill, Some("exercise more".to_string()));
            }
            _ => panic!("Expected Task command"),
        }

        let cmd = parse_from(args(&["+", "%1 day", ":exercise"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.interval, Some("1 day".to_string()));
                assert_eq!(task.prefill, Some("exercise".to_string()));
            }
            _ => panic!("Expected Task command"),
        }

        // A `:` word ends the interval field, which lets it span words
        // (both attached `%1 day` and bare `% 1 day`).
        let cmd = parse_from(args(&["+", "%1", "day", ":exercise"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.interval, Some("1 day".to_string()));
                assert_eq!(task.prefill, Some("exercise".to_string()));
            }
            _ => panic!("Expected Task command"),
        }
        let cmd = parse_from(args(&["+", "%", "1", "day", ":exercise"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.interval, Some("1 day".to_string()));
                assert_eq!(task.prefill, Some("exercise".to_string()));
            }
            _ => panic!("Expected Task command"),
        }

        // Multi-word interval without a name.
        let cmd = parse_from(args(&["+", "%", "1", "day"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.interval, Some("1 day".to_string()));
                assert_eq!(task.prefill, None);
            }
            _ => panic!("Expected Task command"),
        }

        // Once `:` opens the name section, a `%` word further along stays part of the name.
        let cmd = parse_from(args(&["+", "%1day", ":exercise", "%2day"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.interval, Some("1day".to_string()));
                assert_eq!(task.prefill, Some("exercise %2day".to_string()));
            }
            _ => panic!("Expected Task command"),
        }

        // A bare `:` name marker leaves nothing to prefill.
        let cmd = parse_from(args(&["+", "%1day", ":"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.interval, Some("1day".to_string()));
                assert_eq!(task.prefill, None);
            }
            _ => panic!("Expected Task command"),
        }

        // Without `:`, trailing words join the interval and fail interval parsing.
        assert!(parse_from(args(&["+", "%1day", "exercise"])).is_err());
        assert!(parse_from(args(&["+", "%1day", "exercise", ":more"])).is_err());
        assert!(parse_from(args(&["+", ":exercise", "%1day"])).is_err());

        // An unparseable interval fails at parse time, before any prompt.
        assert!(parse_from(args(&["+", "%bogus", ":exercise"])).is_err());

        // Whitespace-only interval trims to absent.
        let cmd = parse_from(args(&["+", "%", "  "])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.prefill, None);
                assert_eq!(task.interval, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_date_only() {
        // `+ @10pm` → scheduled creation with only the start time.
        let cmd = parse_from(args(&["+", "@10pm"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, None);
                assert_eq!(task.date, Some(ts("10pm")));
                assert_eq!(task.available_duration, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_date_with_extra_words_is_rejected() {
        // `+ @10pm meeting` (no markers) keeps the whole first field as the
        // date — and the date field is parsed strictly (a trailing word
        // after `10pm` would parse leniently in jiff-english, but the main
        // crate rejects it), so the field errors instead of silently
        // becoming a name.
        let err = parse_from(args(&["+", "@10pm", "meeting"])).unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid scheduled task start time"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_task_scheduled_name() {
        // `+ @10pm :meeting` → start time + name.
        let cmd = parse_from(args(&["+", "@10pm", ":meeting"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, Some("meeting".to_string()));
                assert_eq!(task.date, Some(ts("10pm")));
                assert_eq!(task.available_duration, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_multiword_name() {
        // `+ @10pm :a :b` → name words join; a `:`-word mid-
        // name just continues it.
        let cmd = parse_from(args(&["+", "@10pm", ":a", ":b"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, Some("a b".to_string()));
                assert_eq!(task.date, Some(ts("10pm")));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_duration() {
        // `+ @10pm %2 hours` → start time + duration, no name.
        let cmd = parse_from(args(&["+", "@10pm", "%2", "hours"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, None);
                assert_eq!(task.date, Some(ts("10pm")));
                assert_eq!(task.available_duration, Some(2 * 3600));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_name_and_duration() {
        // `+ @10pm :meeting %2 hours` → all three fields.
        let cmd = parse_from(args(&["+", "@10pm", ":meeting", "%2", "hours"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, Some("meeting".to_string()));
                assert_eq!(task.date, Some(ts("10pm")));
                assert_eq!(task.available_duration, Some(2 * 3600));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_name_after_duration_allowed() {
        // Name may come after the duration too: `+ @10pm %2 hours :meeting`.
        let cmd = parse_from(args(&["+", "@10pm", "%2", "hours", ":meeting"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, Some("meeting".to_string()));
                assert_eq!(task.date, Some(ts("10pm")));
                assert_eq!(task.available_duration, Some(2 * 3600));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_duplicate_duration_rejected() {
        assert!(parse_from(args(&["+", "@10pm", "%2", "hours", "%30", "minutes"])).is_err());
    }

    #[test]
    fn test_parse_task_scheduled_interleave_deferred() {
        // Interleaving is tolerated, not rejected: the simple splitter
        // hands each segment to parse_duration/parse_datetime, and the
        // parse rejects the garbage. A `:`-word that lands inside the
        // duration segment makes the duration unparseable...
        assert!(parse_from(args(&["+", "@10pm", ":meeting", "%2", "hours", ":again"])).is_err());
        // ...while a trailing `%`-word after the name resumed stays in the
        // name verbatim (names are free-form).
        let cmd = parse_from(args(&["+", "@10pm", "%2", "hours", ":meeting", "%30"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, Some("meeting %30".to_string()));
                assert_eq!(task.date, Some(ts("10pm")));
                assert_eq!(task.available_duration, Some(2 * 3600));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_bad_duration_rejected() {
        // A malformed duration fails fast at parse time.
        assert!(parse_from(args(&["+", "@10pm", "%2", "elephants"])).is_err());
    }

    #[test]
    fn test_parse_task_scheduled_body() {
        // `+ @10pm :meeting . take notes` → body after the delimiter.
        let cmd = parse_from(args(&["+", "@10pm", ":meeting", ".", "take", "notes"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.name, Some("meeting".to_string()));
                assert_eq!(task.body, Ok("take notes".to_string()));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_scheduled_bare_delimiter_empty_body() {
        // Bare delimiter → body is `Err(1)`; the handler opens the editor
        // with the first template (direct creation; the interactive flow
        // errors).
        let cmd = parse_from(args(&["+", "@10pm", ":meeting", "."])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Scheduled);
                assert_eq!(task.body, Err(1));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_recurring_body() {
        // `+ %1day :exercise . notes` → recurring creation with the interval
        // and name pre-filled and the post-delimiter text as the body.
        let cmd = parse_from(args(&["+", "%1day", ":exercise", ".", "notes"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Recurring);
                assert_eq!(task.prefill, Some("exercise".to_string()));
                assert_eq!(task.body, Ok("notes".to_string()));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_recurring_bare_delimiter_empty_body() {
        let cmd = parse_from(args(&["+", "%1day", ":exercise", "."])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Recurring);
                assert_eq!(task.prefill, Some("exercise".to_string()));
                assert_eq!(task.body, Err(1));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_at_name_with_extra_args_fails_at_parse_time() {
        // `+ @exercise now` → the leading @ starts the time field, which
        // swallows the rest into the date; "exercise now" is not a
        // parseable datetime, and since the date is resolved at parse time
        // now, the command fails here rather than in the handler.
        assert!(parse_from(args(&["+", "@exercise", "now"])).is_err());
    }

    #[test]
    fn test_parse_task_recurring_create() {
        // Recurring task creation via + %
        let cmd = parse_from(args(&["+", "%"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Recurring);
                assert_eq!(task.name, None);
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_view_oneshot_list() {
        // Bare `+` is interactive oneshot creation now — name prompted,
        // no body (body `Err(0)` → nothing in the handler). The
        // pending-oneshots list lives at `@:o`.
        let cmd = parse_from(args(&["+"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, None);
                assert_eq!(task.body, Err(0));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_view_recurring() {
        let cmd = parse_from(args(&["@"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::PendingTasks);
                assert_eq!(show, ViewVariant::All);
            }
            _ => panic!("Expected View command"),
        }
    }

    #[test]
    fn test_parse_view_variant_suffixes() {
        // @:o / @:O → pending view at A / B.
        let cmd = parse_from(args(&["@:o"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::PendingTasks);
                assert_eq!(show, ViewVariant::A);
            }
            _ => panic!("Expected View command"),
        }
        let cmd = parse_from(args(&["@:O"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::PendingTasks);
                assert_eq!(show, ViewVariant::B);
            }
            _ => panic!("Expected View command"),
        }
        // @done:o / @done:O / @done → done view at A / B / All.
        let cmd = parse_from(args(&["@done:o"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::DoneTasks);
                assert_eq!(show, ViewVariant::A);
            }
            _ => panic!("Expected View command"),
        }
        let cmd = parse_from(args(&["@done:O"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::DoneTasks);
                assert_eq!(show, ViewVariant::B);
            }
            _ => panic!("Expected View command"),
        }
        let cmd = parse_from(args(&["@done"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::DoneTasks);
                assert_eq!(show, ViewVariant::All);
            }
            _ => panic!("Expected View command"),
        }
    }

    #[test]
    fn test_parse_view_rejects_extra_args() {
        // Extra words join into the command text: `@due extra` and
        // `@done extra` fail their suffix checks, while `@ <date> extra`
        // becomes a multi-word date that the handler rejects at parse time
        // (it can no longer be silently ignored).
        assert!(parse_from(args(&["@due", "extra"])).is_err());
        assert!(parse_from(args(&["@due:t", "extra"])).is_err());
        assert!(parse_from(args(&["@done", "extra"])).is_err());
        assert!(parse_from(args(&["@done:O", "extra"])).is_err());
        assert!(parse_from(args(&["@:o", "extra"])).is_err());
        let cmd = parse_from(args(&["@", "extra"])).unwrap();
        assert!(matches!(cmd, Command::Today { date: Some(d), .. } if d == "extra"));
        let cmd = parse_from(args(&["@2024-03-15", "extra"])).unwrap();
        assert!(matches!(cmd, Command::Today { date: Some(d), .. } if d == "2024-03-15 extra"));
    }

    #[test]
    fn test_parse_view_invalid_suffixes() {
        // There is no `a` suffix; unknown suffixes are rejected.
        assert!(parse_from(args(&["@:a"])).is_err());
        assert!(parse_from(args(&["@done:a"])).is_err());
        assert!(parse_from(args(&["@:x"])).is_err());
        assert!(parse_from(args(&["@due:x"])).is_err());
    }

    #[test]
    fn test_parse_view_done() {
        let cmd = parse_from(args(&["@done"])).unwrap();
        match cmd {
            Command::View { mode, show } => {
                assert_eq!(mode, ViewMode::DoneTasks);
                assert_eq!(show, ViewVariant::All);
            }
            _ => panic!("Expected View command"),
        }
    }

    #[test]
    fn test_parse_view_due() {
        // @due → TodayView at ShowVariant::B with the Today horizon.
        let cmd = parse_from(args(&["@due"])).unwrap();
        match cmd {
            Command::Today {
                date,
                show,
                horizon,
            } => {
                assert_eq!(date, None);
                assert_eq!(show, ViewVariant::B);
                assert_eq!(horizon, TodayHorizon::Today);
            }
            _ => panic!("Expected Today command"),
        }
    }

    #[test]
    fn test_parse_view_due_horizons() {
        // @due:t / @due:w → TodayView at B with the Tomorrow / Week horizon.
        let cmd = parse_from(args(&["@due:t"])).unwrap();
        match cmd {
            Command::Today {
                date,
                show,
                horizon,
            } => {
                assert_eq!(date, None);
                assert_eq!(show, ViewVariant::B);
                assert_eq!(horizon, TodayHorizon::Tomorrow);
            }
            _ => panic!("Expected Today command"),
        }
        let cmd = parse_from(args(&["@due:w"])).unwrap();
        match cmd {
            Command::Today {
                date,
                show,
                horizon,
            } => {
                assert_eq!(date, None);
                assert_eq!(show, ViewVariant::B);
                assert_eq!(horizon, TodayHorizon::Week);
            }
            _ => panic!("Expected Today command"),
        }
    }

    #[test]
    fn test_parse_tracker_week() {
        let cmd = parse_from(args(&[":"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                // Bare `:` renders just the mood grid.
                assert_eq!(items, vec![TrackerItem::Mood]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_month() {
        let cmd = parse_from(args(&[":month"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Month);
                // No display list: mood grid only.
                assert_eq!(items, vec![TrackerItem::Mood]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_year() {
        let cmd = parse_from(args(&[":year"])).unwrap();
        match cmd {
            Command::Tracker { period, .. } => {
                assert_eq!(period, TrackerPeriod::Year);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_colon_second_arg_is_anchor() {
        // `: month` is an anchor query for "month", not a period or a
        // tracker: only the first-token suffix sets the period, and bare
        // words are mood-anchor queries now.
        let cmd = parse_from(args(&[":", "month"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(items, vec![TrackerItem::Anchor("month".to_string())]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_with_ids() {
        // Trackers require the leading `-`; `@name` is recurring, and bare
        // words are anchor queries.
        let cmd = parse_from(args(&[":", "@1", "@2", "-sleep"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(
                    items,
                    vec![
                        TrackerItem::Recurring("1".to_string()),
                        TrackerItem::Recurring("2".to_string()),
                        TrackerItem::Tracker("sleep".to_string())
                    ]
                );
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_period_with_ids() {
        let cmd = parse_from(args(&[":month", "@1", "-sleep"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Month);
                assert_eq!(
                    items,
                    vec![
                        TrackerItem::Recurring("1".to_string()),
                        TrackerItem::Tracker("sleep".to_string())
                    ]
                );
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_first_arg_rejected() {
        // `:foo` with no space is rejected in the dispatcher; the safe
        // entry is `im : foo` with a space.
        let cmd = parse_from(args(&[":", "foo"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(items, vec![TrackerItem::Anchor("foo".to_string())]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_mood_marker_positional() {
        // `: @1 : -sleep` renders @1 grid, mood grid, sleep grid, in order.
        let cmd = parse_from(args(&[":", "@1", ":", "-sleep"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(
                    items,
                    vec![
                        TrackerItem::Recurring("1".to_string()),
                        TrackerItem::Mood,
                        TrackerItem::Tracker("sleep".to_string())
                    ]
                );
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_colon_colon_is_mood_only() {
        // `: :` is the same as bare `:`: mood grid only.
        let cmd = parse_from(args(&[":", ":"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(items, vec![TrackerItem::Mood]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_tracker_suffix_period_with_mood_marker() {
        // `:week : -sleep` → Week period, mood grid then sleep grid.
        let cmd = parse_from(args(&[":week", ":", "-sleep"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(
                    items,
                    vec![TrackerItem::Mood, TrackerItem::Tracker("sleep".to_string())]
                );
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    /// The heatmap item sigils: `+query` (task completions), `%query`
    /// (entries). A bare `+` / `%` is the empty query, matching every
    /// task / entry name.
    #[test]
    fn test_parse_tracker_heatmap_items() {
        let cmd = parse_from(args(&[":", "+deepwork", "%", "+", "%sleep"])).unwrap();
        match cmd {
            Command::Tracker { period, items } => {
                assert_eq!(period, TrackerPeriod::Week);
                assert_eq!(
                    items,
                    vec![
                        TrackerItem::TaskHeatmap("deepwork".to_string()),
                        TrackerItem::EntryHeatmap(String::new()),
                        TrackerItem::TaskHeatmap(String::new()),
                        TrackerItem::EntryHeatmap("sleep".to_string())
                    ]
                );
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    /// A lone `-` has no tracker name; every other `-name` token is one.
    #[test]
    fn test_parse_tracker_dash_requires_name() {
        assert!(parse_from(args(&[":", "-"])).is_err());
        let cmd = parse_from(args(&[":", "-run_times"])).unwrap();
        match cmd {
            Command::Tracker { items, .. } => {
                assert_eq!(items, vec![TrackerItem::Tracker("run_times".to_string())]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    /// A bare `@` keeps the recurring-item shape; the empty name is handled
    /// by the recurring lookup's not-found path.
    #[test]
    fn test_parse_tracker_bare_at() {
        let cmd = parse_from(args(&[":", "@"])).unwrap();
        match cmd {
            Command::Tracker { items, .. } => {
                assert_eq!(items, vec![TrackerItem::Recurring(String::new())]);
            }
            _ => panic!("Expected Tracker command"),
        }
    }

    #[test]
    fn test_parse_dash() {
        // A bare `im -` opens the tracker viewer (the matchmaker app).
        let cmd = parse_from(args(&["-"])).unwrap();
        assert_eq!(cmd, Command::Matchmaker);

        // `im -miles 7` is a tracker entry. A dash with
        // other text stays an entry; only the lone dash is the viewer.
        let cmd = parse_from(args(&["-miles", "7"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.trackers, vec![("miles".to_string(), "7".to_string())]);
                assert_eq!(entry.task_ref, None);
            }
            _ => panic!("Expected Entry command"),
        }
        // Pure numeric trackers like `-7` or `-3.5` are rejected.
        assert!(parse_from(args(&["-7", "ran"])).is_err());
        assert!(parse_from(args(&["-3.5", "ran"])).is_err());
    }

    #[test]
    fn test_parse_plus_command_edit_and_entry_variants() {
        // ---- bare `+`: interactive oneshot creation -------------------
        let cmd = parse_from(args(&["+"])).unwrap();
        assert_eq!(
            cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                name: None,
                priority: None,
                date: None,
                body: Err(0),
                prefill: None,
                available_duration: None,
                parent: None,
                interval: None,
            })
        );

        // ---- single argument with `+`: the interactive editor ---------
        // im +7 -> edit task 7
        let cmd = parse_from(args(&["+7"])).unwrap();
        assert_eq!(
            cmd,
            Command::TaskEdit {
                task: TaskRef::Id(7),
            }
        );

        // im +groceries -> edit the unique word-query match
        let cmd = parse_from(args(&["+groceries"])).unwrap();
        assert_eq!(
            cmd,
            Command::TaskEdit {
                task: TaskRef::Words(vec!["groceries".to_string()]),
            }
        );

        // ---- anything more: entry or task creation --------------------
        fn entry_of(cmd: Command) -> crate::types::Entry {
            match cmd {
                Command::Entry(e) => e,
                other => panic!("Expected Entry command, got {other:?}"),
            }
        }

        // im + 2 — bare '+' with a follower creates a oneshot task named "2".
        let cmd = parse_from(args(&["+", "2"])).unwrap();
        assert_eq!(
            cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                name: Some("2".to_string()),
                priority: None,
                date: None,
                body: Err(0),
                prefill: None,
                available_duration: None,
                parent: None,
                interval: None,
            })
        );

        // im +7 2 — completion payload, no mood row needed.
        let e = entry_of(parse_from(args(&["+7", "2"])).unwrap());
        assert_eq!(e.task_ref, Some(TaskRef::Id(7)));
        assert_eq!(e.count, Some(2));
        assert_eq!(e.mood, "");

        // im +7 buy milk — creates oneshot linked to parent 7.
        let cmd = parse_from(args(&["+7", "buy", "milk"])).unwrap();
        assert_eq!(
            cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                name: Some("buy milk".to_string()),
                priority: None,
                date: None,
                body: Err(0),
                prefill: None,
                available_duration: None,
                parent: Some(TaskRef::Id(7)),
                interval: None,
            })
        );

        // im +7 buy milk . whole milk — creates oneshot linked to parent 7 with body.
        let cmd = parse_from(args(&["+7", "buy", "milk", ".", "whole", "milk"])).unwrap();
        assert_eq!(
            cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                name: Some("buy milk".to_string()),
                priority: None,
                date: None,
                body: Ok("whole milk".to_string()),
                prefill: None,
                available_duration: None,
                parent: Some(TaskRef::Id(7)),
                interval: None,
            })
        );

        // im +buy milk — creates oneshot linked to parent query "buy".
        let cmd = parse_from(args(&["+buy", "milk"])).unwrap();
        assert_eq!(
            cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                name: Some("milk".to_string()),
                priority: None,
                date: None,
                body: Err(0),
                prefill: None,
                available_duration: None,
                parent: Some(TaskRef::Words(vec!["buy".to_string()])),
                interval: None,
            })
        );

        // Mood with trailing ref: link-only when nothing follows.
        let e = entry_of(parse_from(args(&["good", "+7", ".", "stuff"])).unwrap());
        assert_eq!(e.mood, "good");
        assert_eq!(e.task_ref, Some(TaskRef::Id(7)));
        assert_eq!(e.count, None);
        assert_eq!(e.body, Ok("stuff".to_string()));

        // Trailing ref with numeric payload: link AND complete.
        let e = entry_of(parse_from(args(&["good", "+7", "3"])).unwrap());
        assert_eq!(e.mood, "good");
        assert_eq!(e.task_ref, Some(TaskRef::Id(7)));
        assert_eq!(e.count, Some(3));

        // Untoggle via a negative numeric payload.
        let e = entry_of(parse_from(args(&["+toggle", "-1"])).unwrap());
        assert_eq!(e.task_ref, Some(TaskRef::Words(vec!["toggle".to_string()])));
        assert_eq!(e.count, Some(-1));

        // A trailing ref closes the line like a tracker pair: a bare word
        // after it is rejected.
        assert!(parse_from(args(&["good", "+7", "later"])).is_err());

        // Sessions compose with refs.
        let e = entry_of(parse_from(args(&["%25m", "+7"])).unwrap());
        assert_eq!(e.duration, Some(1500));
        assert_eq!(e.task_ref, Some(TaskRef::Id(7)));

        // At most one ref per entry. (A second '+' word directly after a
        // leading '+<ref>' is child-creation name text instead — see the
        // task tests.)
        assert!(parse_from(args(&["good", "+1", "+2"])).is_err());
        assert!(parse_from(args(&["good", "+", "+2"])).is_err());

        // Invalid completion counts (fractional or overflowing) error cleanly.
        let err = parse_from(args(&["+7", "2.5"])).unwrap_err();
        assert!(err.to_string().contains("Invalid completion count"), "{err}");
        let err = parse_from(args(&["+7", "-3.5"])).unwrap_err();
        assert!(err.to_string().contains("Invalid completion count"), "{err}");
        let err = parse_from(args(&["+7", "99999999999"])).unwrap_err();
        assert!(err.to_string().contains("Invalid completion count"), "{err}");
        let err = parse_from(args(&["good", "+7", "2.5"])).unwrap_err();
        assert!(err.to_string().contains("Invalid completion count"), "{err}");
        let err = parse_from(args(&["+task", "-3.5"])).unwrap_err();
        assert!(err.to_string().contains("Invalid completion count"), "{err}");

        // Pure numeric trackers (including double dash) are disallowed.
        let err = parse_from(args(&["good", "--5"])).unwrap_err();
        assert!(err.to_string().contains("Pure numeric trackers are not allowed"), "{err}");

        // im + +7 %1d creates a oneshot named "+7 %1d" (no parent).
        let cmd = parse_from(args(&["+", "+7", "%1d"])).unwrap();
        assert_eq!(
            cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                name: Some("+7 %1d".to_string()),
                priority: None,
                date: None,
                body: Err(0),
                prefill: None,
                available_duration: None,
                parent: None,
                interval: None,
            })
        );

        // A parent cannot be attached to recurring or scheduled creation:
        // the parsed ref is rejected instead of silently dropped.
        let err = parse_from(args(&["+7", "%1d"])).unwrap_err();
        assert!(err.to_string().contains("oneshot task"), "{err}");
        let err = parse_from(args(&["+7", "@10pm"])).unwrap_err();
        assert!(err.to_string().contains("oneshot task"), "{err}");

        // A bare '-' is never a task name; it stays an entry token.
        let err = parse_from(args(&["+7", "-"])).unwrap_err();
        assert!(err.to_string().contains("Task name cannot be '-'"), "{err}");
        let err = parse_from(args(&["+", "-"])).unwrap_err();
        assert!(err.to_string().contains("Task name cannot be '-'"), "{err}");
    }

    #[test]
    fn test_parse_tracker_valueless_chains() {
        // `good -sleep -xyz -withvalue abc -null3`: valueless trackers may
        // be chained, and a dash token is never consumed as the previous
        // tracker's value.
        let cmd = parse_from(args(&[
            "good",
            "-sleep",
            "-xyz",
            "-withvalue",
            "abc",
            "-null3",
        ]))
        .unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(
                    entry.trackers,
                    vec![
                        ("sleep".to_string(), String::new()),
                        ("xyz".to_string(), String::new()),
                        ("withvalue".to_string(), "abc".to_string()),
                        ("null3".to_string(), String::new()),
                    ]
                );
            }
            _ => panic!("Expected Entry command"),
        }

        // A valueless tracker before the mood followed by a bare word still
        // consumes it as the value (config-free parser).
        let cmd = parse_from(args(&["-sleep", "-xyz", "good"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(
                    entry.trackers,
                    vec![
                        ("sleep".to_string(), String::new()),
                        ("xyz".to_string(), "good".to_string()),
                    ]
                );
            }
            _ => panic!("Expected Entry command"),
        }

        // Valueless trackers interleave after the mood.
        let cmd = parse_from(args(&["good", "-sleep", "-awake", "-xyz"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(
                    entry.trackers,
                    vec![
                        ("sleep".to_string(), String::new()),
                        ("awake".to_string(), String::new()),
                        ("xyz".to_string(), String::new()),
                    ]
                );
                assert_eq!(entry.task_ref, None);
            }
            _ => panic!("Expected Entry command"),
        }
        // Pure numeric trackers are rejected
        assert!(parse_from(args(&["good", "-1"])).is_err());
    }

    #[test]
    fn test_parse_tracker_in_final_position() {
        // im <mood> [-tracker value] — trackers after the mood
        let cmd = parse_from(args(&["good", "-sleep", "8"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(entry.trackers, vec![("sleep".to_string(), "8".to_string())]);
                assert_eq!(entry.body, Err(0));
            }
            _ => panic!("Expected Entry command"),
        }

        // … with a trailing body delimiter after the tracker pair.
        let cmd = parse_from(args(&["good", "-sleep", "8", "-water", "5", ".", "later"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(
                    entry.trackers,
                    vec![
                        ("sleep".to_string(), "8".to_string()),
                        ("water".to_string(), "5".to_string())
                    ]
                );
                assert_eq!(entry.body, Ok("later".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_tracker_accumulates_values() {
        // Trackers accumulate values until the next '-' or '+' token.
        // Mood is parsed first, not after trackers.
        let cmd = parse_from(args(&["good", "-notes", "took", "a", "long", "walk", "-water", "5"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(
                    entry.trackers,
                    vec![
                        ("notes".to_string(), "took a long walk".to_string()),
                        ("water".to_string(), "5".to_string())
                    ]
                );
            }
            _ => panic!("Expected Entry command"),
        }

        // Valueless trackers and multi-word values
        let cmd = parse_from(args(&["-sleep", "8", "hours", "-notes", "felt", "fine"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(
                    entry.trackers,
                    vec![
                        ("sleep".to_string(), "8 hours".to_string()),
                        ("notes".to_string(), "felt fine".to_string())
                    ]
                );
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_tracker_and_task_refs_freely_mix() {
        // Trackers and task refs can freely mix.
        // +task -tracker y -> tracker entry + task link
        let cmd = parse_from(args(&["+task", "-tracker", "y"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.trackers, vec![("tracker".to_string(), "y".to_string())]);
                assert_eq!(
                    entry.task_ref,
                    Some(TaskRef::Words(vec!["task".to_string()]))
                );
                assert_eq!(entry.count, None);
            }
            _ => panic!("Expected Entry command"),
        }

        // +task 3 -tracker -> null tracker + task update & link
        let cmd = parse_from(args(&["+task", "3", "-tracker"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.trackers, vec![("tracker".to_string(), String::new())]);
                assert_eq!(
                    entry.task_ref,
                    Some(TaskRef::Words(vec!["task".to_string()]))
                );
                assert_eq!(entry.count, Some(3));
            }
            _ => panic!("Expected Entry command"),
        }

        // -tracker +7 -> null tracker + task link
        let cmd = parse_from(args(&["-tracker", "+7"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.trackers, vec![("tracker".to_string(), String::new())]);
                assert_eq!(entry.task_ref, Some(TaskRef::Id(7)));
                assert_eq!(entry.count, None);
            }
            _ => panic!("Expected Entry command"),
        }

        // With a mood present, tracker then task ref:
        let cmd = parse_from(args(&["good", "-tracker", "+7"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(entry.trackers, vec![("tracker".to_string(), String::new())]);
                assert_eq!(entry.task_ref, Some(TaskRef::Id(7)));
                assert_eq!(entry.count, None);
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_entry_task_ref() {
        // Mood first, then tracker and trailing '+' task ref:
        let cmd = parse_from(args(&["mood", "-tracker", "+7"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "mood");
                assert_eq!(entry.trackers, vec![("tracker".to_string(), String::new())]);
                assert_eq!(entry.task_ref, Some(TaskRef::Id(7)));
            }
            _ => panic!("Expected Entry command"),
        }

        // Word query ref (including multi-word quoted token).
        let cmd = parse_from(args(&["good", "+buy milk"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(
                    entry.task_ref,
                    Some(TaskRef::Words(vec!["buy".to_string(), "milk".to_string()]))
                );
            }
            _ => panic!("Expected Entry command"),
        }

        // Bare '+' picks interactively. Digits are always ids (+0 too).
        let cmd = parse_from(args(&["ok", "+"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Entry(e) if e.task_ref == Some(TaskRef::Pick)
        ));
        let cmd = parse_from(args(&["ok", "+0"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Entry(e) if e.task_ref == Some(TaskRef::Id(0))
        ));

        // At most one '+' per entry.
        assert!(parse_from(args(&["+1", "-tracker", "+2"])).is_err());
        assert!(parse_from(args(&["good", "+1", "+2"])).is_err());
        assert!(parse_from(args(&["good", "+", "+2"])).is_err());
    }

    #[test]
    fn test_parse_tracker_then_delimiter_body_split_first() {
        // Body split comes first (like tasks): the delimiter never becomes
        // a tracker's value, and free text after it is body verbatim.

        // im -awake . brush my teeth — valueless tracker + body.
        let cmd = parse_from(args(&["-awake", ".", "brush", "my", "teeth"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.trackers, vec![("awake".to_string(), String::new())]);
                assert_eq!(entry.body, Ok("brush my teeth".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // im <mood> -awake . brush my teeth — tracker after the mood.
        let cmd = parse_from(args(&["good", "-awake", ".", "brush", "my", "teeth"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(entry.trackers, vec![("awake".to_string(), String::new())]);
                assert_eq!(entry.body, Ok("brush my teeth".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // A second delimiter inside the body is literal text (task parity).
        let cmd = parse_from(args(&["ok", ".", "see", ".", "note"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert_eq!(entry.body, Ok(format!("see {} note", BODY_DELIMITER)));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_cli_strips_initial_flags() {
        // -q and --quiet before the command
        let cli = parse_cli(args(&["-q", "ok"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 0]);
        assert_eq!(
            cli.cmd,
            Command::Entry(Entry {
                mood: "ok".to_string(),
                trackers: vec![],
                task_ref: None,
                count: None,
                body: Err(0),
                duration: None,
                purpose: None,
            })
        );
        let cli = parse_cli(args(&["--quiet", "ok"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 0]);

        // -v before a task creation (bare `+` is interactive creation now)
        let cli = parse_cli(args(&["-v", "+"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 1]);
        assert!(matches!(
            cli.cmd,
            Command::Task(Task {
                task_type: TaskKind::Oneshot,
                ..
            })
        ));

        // both flags, before a tracker view
        let cli = parse_cli(args(&["-q", "-v", ":week"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 1]);
        assert!(matches!(cli.cmd, Command::Tracker { .. }));

        // combined token: -qv sets both
        let cli = parse_cli(args(&["-qv", "ok"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 1]);
        assert!(matches!(cli.cmd, Command::Entry(_)));

        // order is not tracked: -vq is the same counts as -qv
        let cli = parse_cli(args(&["-vq", "-", "ok"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 1]);
        assert!(
            matches!(cli.cmd, Command::Entry(e) if e.mood == "- ok"),
            "'-' and 'ok' are entry text now that the dash update form is gone"
        );

        // repeated flags stack up as counts (-vvq → 1 quiet, 2 verbose)
        let cli = parse_cli(args(&["-vvq", "ok"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 2]);
        assert!(matches!(cli.cmd, Command::Entry(_)));

        // flag alone → Today (same as no args)
        let cli = parse_cli(args(&["-q"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 0]);
        assert_eq!(
            cli.cmd,
            Command::Today {
                date: None,
                show: ViewVariant::All,
                horizon: TodayHorizon::Today,
            }
        );

        // -F (fullscreen) sets the flag; combinable with q/v
        let cli = parse_cli(args(&["-F"])).unwrap();
        assert!(cli.opts.fullscreen);
        assert_eq!(cli.opts.qv, [0, 0]);
        assert!(matches!(cli.cmd, Command::Today { .. }));

        let cli = parse_cli(args(&["-qF", "ok"])).unwrap();
        assert!(cli.opts.fullscreen);
        assert_eq!(cli.opts.qv, [1, 0]);
        assert!(matches!(cli.cmd, Command::Entry(_)));

        // no flags
        let cli = parse_cli(args(&["ok"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 0]);
        assert!(!cli.opts.fullscreen);
        assert!(matches!(cli.cmd, Command::Entry(_)));
    }

    #[test]
    fn test_parse_cli_flags_initial_position_only() {
        // Once a non-flag token appears, -q is entry text: a trailing
        // -<name> with no value parses as a valueless tracker (Null
        // trackers; the handler rejects it for text/number/float kinds).
        let cli = parse_cli(args(&["ok", "-q"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 0]);
        assert!(
            matches!(cli.cmd, Command::Entry(e) if e.trackers == vec![("q".to_string(), String::new())])
        );

        // A combined -qv token is a flag now, not entry text.
        let cli = parse_cli(args(&["-qv", "ok"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 1]);
        assert!(matches!(cli.cmd, Command::Entry(_)));

        // A bare dash is entry text (mood '-'), never a flag; the -q after
        // it is a valueless tracker, not a flag either.
        let cli = parse_cli(args(&["-", "-q"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 0]);
        assert!(matches!(
            cli.cmd,
            Command::Entry(e)
                if e.mood == "-"
                    && e.trackers == vec![("q".to_string(), String::new())]
        ));

        // Tokens with non-flag characters stop the flag run (-q5 is entry
        // text: a valueless tracker reference, like any trailing -<name>).
        let cli = parse_cli(args(&["-q5"])).unwrap();
        assert!(
            matches!(cli.cmd, Command::Entry(e) if e.trackers == vec![("q5".to_string(), String::new())])
        );
        // A purely numeric -<name> is rejected: pure numeric trackers aren't allowed.
        assert!(parse_cli(args(&["-5"])).is_err());
    }

    #[test]
    fn test_parse_embed() {
        let cmd = parse_from(args(&[":tool", "embed"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Embed));
    }

    #[test]
    fn test_parse_score() {
        let cmd = parse_from(args(&[":tool", "score", "happy", "sad"])).unwrap();
        match cmd {
            Command::Tool(ToolSubcommand::Score { start, end }) => {
                assert_eq!(start, "happy");
                assert_eq!(end, "sad");
            }
            _ => panic!("Expected Score subcommand"),
        }
    }

    #[test]
    fn test_parse_empty_returns_today() {
        // `im` with no args → Today view (All, Today horizon).
        let today = Command::Today {
            date: None,
            show: ViewVariant::All,
            horizon: TodayHorizon::Today,
        };
        let cmd = parse_from(vec![]).unwrap();
        assert_eq!(cmd, today.clone());

        // The same through parse_cli, with or without a leading flag.
        assert_eq!(parse_cli(vec![]).unwrap().cmd, today.clone());
        assert_eq!(parse_cli(args(&["-q"])).unwrap().cmd, today);
    }

    #[test]
    fn test_parse_today_with_date() {
        // `im @2024-03-20` → today view anchored to that date
        // (All, Today horizon).
        let cmd = parse_from(args(&["@2024-03-20"])).unwrap();
        assert_eq!(
            cmd,
            Command::Today {
                date: Some("2024-03-20".to_string()),
                show: ViewVariant::All,
                horizon: TodayHorizon::Today,
            }
        );

        // Multi-word datetimes still work through the @ token.
        let cmd = parse_from(args(&["@2024-03-20", "14:30"])).unwrap();
        match cmd {
            Command::Today {
                date,
                show,
                horizon,
            } => {
                // Multi-word datetimes join into the date: the dispatcher
                // passes the full command text through.
                assert_eq!(date, Some("2024-03-20 14:30".to_string()));
                assert_eq!(show, ViewVariant::All);
                assert_eq!(horizon, TodayHorizon::Today);
            }
            _ => panic!("Expected Today command"),
        }

        // Relative dates parse too.
        let cmd = parse_from(args(&["@yesterday"])).unwrap();
        assert!(matches!(cmd, Command::Today { date: Some(_), .. }));

        // `@y` is a view-command alias for `@yesterday` — carried through
        // as the date token, resolved by the handler's date parse.
        let cmd = parse_from(args(&["@y"])).unwrap();
        assert_eq!(
            cmd,
            Command::Today {
                date: Some("y".to_string()),
                show: ViewVariant::All,
                horizon: TodayHorizon::Today,
            }
        );

        // Unparseable dates still parse at the CLI level (the handler is
        // the authority, parsing with `DATE_DIALECT`) — assert the date is
        // carried through.
        let cmd = parse_from(args(&["@bogus"])).unwrap();
        assert_eq!(
            cmd,
            Command::Today {
                date: Some("bogus".to_string()),
                show: ViewVariant::All,
                horizon: TodayHorizon::Today,
            }
        );

        // The task views are untouched.
        assert!(matches!(
            parse_from(args(&["@done"])).unwrap(),
            Command::View {
                mode: ViewMode::DoneTasks,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_help_is_cli_level_only() {
        // `-h` / `--help` are handled in parse_cli (initial position only).
        let cli = parse_cli(args(&["-h"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 0]);
        assert_eq!(cli.cmd, Command::Help);

        let cli = parse_cli(args(&["--help"])).unwrap();
        assert_eq!(cli.cmd, Command::Help);

        // Help wins over other initial-position flags.
        let cli = parse_cli(args(&["-q", "-h"])).unwrap();
        assert_eq!(cli.opts.qv, [1, 0]);
        assert_eq!(cli.cmd, Command::Help);

        let cli = parse_cli(args(&["-vh"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 1]);
        assert!(cli.opts.verbose());
        assert_eq!(cli.cmd, Command::Help);

        let cli = parse_cli(args(&["-hv"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 1]);
        assert!(cli.opts.verbose());
        assert_eq!(cli.cmd, Command::Help);

        let cli = parse_cli(args(&["-v", "-h"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 1]);
        assert!(cli.opts.verbose());
        assert_eq!(cli.cmd, Command::Help);

        let cli = parse_cli(args(&["-h", "-v"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 1]);
        assert!(cli.opts.verbose());
        assert_eq!(cli.cmd, Command::Help);

        let cli = parse_cli(args(&["--help", "-v"])).unwrap();
        assert_eq!(cli.opts.qv, [0, 1]);
        assert!(cli.opts.verbose());
        assert_eq!(cli.cmd, Command::Help);

        // After a non-flag token, -h is entry text (a valueless tracker
        // reference), not help.
        let cli = parse_cli(args(&["ok", "-h"])).unwrap();
        assert!(
            matches!(cli.cmd, Command::Entry(e) if e.trackers == vec![("h".to_string(), String::new())])
        );
    }

    #[test]
    fn test_parse_config() {
        let cmd = parse_from(args(&[":tool", "config"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Config { target: None }));
    }

    #[test]
    fn test_parse_config_c_alias() {
        let cmd = parse_from(args(&[":c"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Config { target: None }));
    }

    #[test]
    fn test_parse_config_moods() {
        let cmd = parse_from(args(&[":tool", "config", "moods"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Config {
                target: Some(ConfigTarget::Moods)
            })
        );
        let cmd = parse_from(args(&[":c", "moods"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Config {
                target: Some(ConfigTarget::Moods)
            })
        );
    }

    #[test]
    fn test_parse_config_colors() {
        let cmd = parse_from(args(&[":tool", "config", "colors"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Config {
                target: Some(ConfigTarget::Colors)
            })
        );
        let cmd = parse_from(args(&[":c", "colors"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Config {
                target: Some(ConfigTarget::Colors)
            })
        );
    }

    #[test]
    fn test_parse_config_mm() {
        let cmd = parse_from(args(&[":tool", "config", "mm"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Config {
                target: Some(ConfigTarget::Mm)
            })
        );
        let cmd = parse_from(args(&[":c", "mm"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Config {
                target: Some(ConfigTarget::Mm)
            })
        );
    }

    #[test]
    fn test_parse_sync() {
        assert_eq!(
            parse_from(args(&[":sync"])).unwrap(),
            Command::Sync {
                sub: None,
                reset: false,
                force: false
            }
        );
        assert_eq!(
            parse_from(args(&[":sync", "--reset"])).unwrap(),
            Command::Sync {
                sub: None,
                reset: true,
                force: false
            }
        );
        assert_eq!(
            parse_from(args(&[":sync", "--reset", "--force"])).unwrap(),
            Command::Sync {
                sub: None,
                reset: true,
                force: true
            }
        );

        for (name, sub) in [
            ("register", SyncSubcommand::Register),
            ("login", SyncSubcommand::Login),
            ("status", SyncSubcommand::Status),
            ("logout", SyncSubcommand::Logout),
        ] {
            assert_eq!(
                parse_from(args(&[":sync", name])).unwrap(),
                Command::Sync {
                    sub: Some(sub),
                    reset: false,
                    force: false
                }
            );
        }
    }

    #[test]
    fn test_parse_moods_retired() {
        // `:moods` is retired in favor of `:tool config moods`.
        let result = parse_from(args(&[":moods"]));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_matchmaker() {
        let cmd = parse_from(args(&["-"])).unwrap();
        assert_eq!(cmd, Command::Matchmaker);
        // A dash with extra text stays entry text, not the viewer.
        assert!(matches!(
            parse_from(args(&["-", "extra"])).unwrap(),
            Command::Entry(_)
        ));
    }

    #[test]
    fn test_parse_db_prune() {
        let cmd = parse_from(args(&[":tool", "db", "prune"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Db(DbSubcommand::Prune)));
        let err = parse_from(args(&[":db", "prune"])).unwrap_err();
        assert!(err.to_string().contains(":tool db"), "{err}");
    }

    #[test]
    fn test_parse_db_backfill() {
        let cmd = parse_from(args(&[":tool", "db", "backfill"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Db(DbSubcommand::Backfill)));
    }

    #[test]
    fn test_parse_db_doctor() {
        let cmd = parse_from(args(&[":tool", "db", "doctor"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Db(DbSubcommand::Doctor)));
    }

    #[test]
    fn test_parse_mood() {
        let cmd = parse_from(args(&[":tool", "mood", "drained"])).unwrap();
        match cmd {
            Command::Tool(ToolSubcommand::Mood { mood }) => assert_eq!(mood, vec!["drained"]),
            _ => panic!("Expected Mood subcommand"),
        }
    }

    #[test]
    fn test_parse_mood_multword() {
        let cmd = parse_from(args(&[":tool", "mood", "mood", "drained"])).unwrap();
        match cmd {
            Command::Tool(ToolSubcommand::Mood { mood }) => {
                assert_eq!(mood, vec!["mood", "drained"])
            }
            _ => panic!("Expected Mood subcommand"),
        }
    }

    #[test]
    fn test_parse_datetime() {
        let cmd = parse_from(args(&[":tool", "datetime", "next", "friday", "8pm"])).unwrap();
        match cmd {
            Command::Tool(ToolSubcommand::Datetime { expr, format }) => {
                assert_eq!(expr, vec!["next", "friday", "8pm"]);
                assert_eq!(format, DatetimeFormat::Civil);
            }
            _ => panic!("Expected Datetime subcommand"),
        }

        // A bare expression defaults to now, and the format flag may come
        // before or after the positional words.
        let cmd = parse_from(args(&[":tool", "datetime"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Tool(ToolSubcommand::Datetime { expr, format })
                if expr.is_empty() && format == DatetimeFormat::Civil
        ));
        let cmd = parse_from(args(&[":tool", "datetime", "--format", "utc", "now"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Tool(ToolSubcommand::Datetime { expr, format })
                if expr == vec!["now"] && format == DatetimeFormat::Utc
        ));
        let cmd = parse_from(args(&[":tool", "datetime", "now", "--format", "short"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Tool(ToolSubcommand::Datetime { expr, format })
                if expr == vec!["now"] && format == DatetimeFormat::Short
        ));

        // Negative day shorthand (`-3` = three days ago) is an expression,
        // not a flag; aliases resolve to the same format.
        let cmd = parse_from(args(&[":tool", "datetime", "-3"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Tool(ToolSubcommand::Datetime { expr, .. }) if expr == vec!["-3"]
        ));
        let cmd = parse_from(args(&[":tool", "datetime", "--format", "unix", "now"])).unwrap();
        assert!(matches!(
            cmd,
            Command::Tool(ToolSubcommand::Datetime {
                format: DatetimeFormat::UnixSeconds,
                ..
            })
        ));
    }

    #[test]
    fn test_parse_bare_mood_lists_palette() {
        // Bare :tool mood has no mood to diagnose - it lists the anchor palette.
        let cmd = parse_from(args(&[":tool", "mood"])).unwrap();
        match cmd {
            Command::Tool(ToolSubcommand::Mood { mood }) => assert!(mood.is_empty()),
            _ => panic!("Expected Mood subcommand"),
        }
    }

    #[test]
    fn test_parse_rejects_tabs_in_mood() {
        // Tabs are rejected at parse time (view output uses tab separators).
        let result = parse_from(args(&["ok\ttab"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("tab characters"));
    }

    #[test]
    fn test_parse_mood_session_variants() {
        // im %25m
        let cmd = parse_from(args(&["%25m"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, None);
            }
            _ => panic!("Expected Entry command"),
        }

        // im % (bare % not supported -> error)
        assert!(parse_from(args(&["%"])).is_err());

        // im % 25m
        let cmd = parse_from(args(&["%", "25m"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, None);
            }
            _ => panic!("Expected Entry command"),
        }

        // im %1500 (pure number in seconds)
        let cmd = parse_from(args(&["%1500"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, None);
            }
            _ => panic!("Expected Entry command"),
        }

        // im %25m study math (purpose space-joined)
        let cmd = parse_from(args(&["%25m", "study", "math"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, Some("study math".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // im %25m study math -sleep 8
        let cmd = parse_from(args(&["%25m", "study", "math", "-sleep", "8"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, Some("study math".to_string()));
                assert_eq!(entry.trackers, vec![("sleep".to_string(), "8".to_string())]);
            }
            _ => panic!("Expected Entry command"),
        }

        // im %25m study math +5 -sleep 8 . notes (+task before trackers)
        let cmd = parse_from(args(&["%25m", "study", "math", "+5", "-sleep", "8", ".", "notes"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, Some("study math".to_string()));
                assert_eq!(entry.task_ref, Some(crate::types::TaskRef::Id(5)));
                assert_eq!(entry.trackers, vec![("sleep".to_string(), "8".to_string())]);
                assert_eq!(entry.body, Ok("notes".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // im %25m study math -sleep 8 +5 . notes (+task and trackers freely mix)
        let cmd = parse_from(args(&["%25m", "study", "math", "-sleep", "8", "+5", ".", "notes"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.purpose, Some("study math".to_string()));
                assert_eq!(entry.task_ref, Some(crate::types::TaskRef::Id(5)));
                assert_eq!(entry.trackers, vec![("sleep".to_string(), "8".to_string())]);
                assert_eq!(entry.body, Ok("notes".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // im %25m +5 . notes
        let cmd = parse_from(args(&["%25m", "+5", ".", "notes"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.duration, Some(1500));
                assert_eq!(entry.task_ref, Some(crate::types::TaskRef::Id(5)));
                assert_eq!(entry.body, Ok("notes".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // im %25m +5 study math: first '+' ends the purpose block, so 'study' is unexpected
        assert!(parse_from(args(&["%25m", "+5", "study", "math"])).is_err());

        // Session purpose containing ':' rejected
        assert!(parse_from(args(&["%25m", "study:", "math"])).is_err());
        assert!(parse_from(args(&["%25m", "study: math"])).is_err());

        // Duplicate +task rejected in session
        assert!(parse_from(args(&["%25m", "study", "+1", "+2"])).is_err());
        assert!(parse_from(args(&["%25m", "study", "+", "+2"])).is_err());
    }

    #[test]
    fn test_parse_entry_body_result() {
        // Bare `.` at end, no text after → `Err(1)`: the handler opens the
        // body editor with the first template.
        let cmd = parse_from(args(&["ok", "."])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert_eq!(entry.body, Err(1));
            }
            _ => panic!("Expected Entry command"),
        }

        // Delimiter at end with text after → body is the joined text, no
        // editor (text wins over the editor prompt).
        let cmd = parse_from(args(&["ok", ".", "later", "thoughts"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert_eq!(entry.body, Ok("later thoughts".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // Delimiter anywhere in the middle splits: pre-delimiter is mood,
        // post-delimiter is body. `[".", "ok"]` puts "ok" into body,
        // leaving mood empty.
        let cmd = parse_from(args(&[".", "ok"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "");
                assert_eq!(entry.body, Ok("ok".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // Delimiter in the middle with text on both sides — body is the
        // joined post-delimiter text.
        let cmd = parse_from(args(&["ok", "more", ".", "journal", "entry"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok more");
                assert_eq!(entry.body, Ok("journal entry".to_string()));
            }
            _ => panic!("Expected Entry command"),
        }

        // No delimiter at all: `Err(0)` → no body, no editor.
        let cmd = parse_from(args(&["ok"])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert_eq!(entry.body, Err(0));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_entry_dot_count_selects_template_index() {
        // Bare delimiters of 1, 2, 3 and 4 dots carry their dot count in
        // `Err(n)` — the handler seeds the editor with the nth template
        // (out of range falls back to the hint).
        for (dots, expected) in [(".", 1), ("..", 2), ("...", 3), ("....", 4)] {
            let cmd = parse_from(args(&["ok", dots])).unwrap();
            match cmd {
                Command::Entry(entry) => {
                    assert_eq!(entry.mood, "ok");
                    assert_eq!(entry.body, Err(expected));
                }
                _ => panic!("Expected Entry command"),
            }
        }

        // Same for tasks.
        let cmd = parse_from(args(&["+", "task", ".."])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.name, Some("task".to_string()));
                assert_eq!(task.body, Err(2));
            }
            _ => panic!("Expected Task command"),
        }

        // Trackers still parse before the delimiter, whatever its length.
        let cmd = parse_from(args(&["good", "-sleep", "8", "..."])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "good");
                assert_eq!(entry.trackers, vec![("sleep".to_string(), "8".to_string())]);
                assert_eq!(entry.body, Err(3));
            }
            _ => panic!("Expected Entry command"),
        }
    }

    #[test]
    fn test_parse_not_delimiter_requires_only_dots() {
        // Args mixing dots with other characters are ordinary words, not
        // delimiters — the body stays `Err(0)` and the words land in the
        // mood.
        for word in ["a.b", ".x", "..x", "x..", "1.5"] {
            let cmd = parse_from(args(&["ok", word])).unwrap();
            match cmd {
                Command::Entry(entry) => {
                    assert_eq!(entry.mood, format!("ok {word}"));
                    assert_eq!(entry.body, Err(0));
                }
                _ => panic!("Expected Entry command"),
            }
        }

        // An empty arg is not a delimiter either.
        let cmd = parse_from(args(&["ok", ""])).unwrap();
        match cmd {
            Command::Entry(entry) => assert_eq!(entry.body, Err(0)),
            _ => panic!("Expected Entry command"),
        }

        // A dash-dot arg is a valueless tracker, not a delimiter.
        let cmd = parse_from(args(&["ok", "-."])).unwrap();
        match cmd {
            Command::Entry(entry) => {
                assert_eq!(entry.mood, "ok");
                assert_eq!(entry.trackers, vec![(".".to_string(), String::new())]);
                assert_eq!(entry.body, Err(0));
            }
            _ => panic!("Expected Entry command"),
        }

        // Tasks: a dotted word before the delimiter is part of the name.
        let cmd = parse_from(args(&["+", "task", "v1.2"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.name, Some("task v1.2".to_string()));
                assert_eq!(task.body, Err(0));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_task_delimiter_in_middle_splits_name_and_body() {
        let cmd = parse_from(args(&["+", "do", "thing", ".", "body", "text"])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("do thing".to_string()));
                assert_eq!(task.body, Ok("body text".to_string()));
            }
            _ => panic!("Expected Task command"),
        }

        // Delimiter at end with empty body → body is `Err(1)`; direct
        // creation opens the editor, the interactive flow errors.
        let cmd = parse_from(args(&["+", "do", "thing", "."])).unwrap();
        match cmd {
            Command::Task(task) => {
                assert_eq!(task.task_type, TaskKind::Oneshot);
                assert_eq!(task.name, Some("do thing".to_string()));
                assert_eq!(task.body, Err(1));
            }
            _ => panic!("Expected Task command"),
        }
    }

    #[test]
    fn test_parse_clear() {
        let cmd = parse_from(args(&[":tool", "clear"])).unwrap();
        assert_eq!(cmd, Command::Tool(ToolSubcommand::Clear { date: vec![] }));

        let cmd = parse_from(args(&[":tool", "clear", "@2024-03-20"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Clear {
                date: vec!["@2024-03-20".to_string()]
            })
        );

        let cmd = parse_from(args(&[":tool", "clear", "2024-03-20"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Clear {
                date: vec!["2024-03-20".to_string()]
            })
        );

        let cmd = parse_from(args(&[":tool", "clear", "@"])).unwrap();
        assert_eq!(
            cmd,
            Command::Tool(ToolSubcommand::Clear {
                date: vec!["@".to_string()]
            })
        );
    }
}
