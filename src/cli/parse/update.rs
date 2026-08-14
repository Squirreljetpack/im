use anyhow::Context;

use super::super::{Command, UpdateTarget};

pub(crate) fn parse_dash_command(args: &[String]) -> anyhow::Result<Command> {
    // The leading "-" has already been stripped by the caller — `args` holds
    // everything after it:
    //   im -                  → TasksEdit (stub; task editing entry point)
    //   im - <id> [count]     → Update a oneshot task by id
    //   im - <words…> [count] → Update the unique task whose name
    //                                contains the words in their order
    if args.is_empty() {
        return Ok(Command::TasksEdit);
    }

    let first = &args[0];

    // Numeric first arg → oneshot short id (`- <id> [count]` form).
    if let Ok(id) = first.parse::<i64>() {
        if args.len() > 2 {
            anyhow::bail!("Too many arguments. Usage: im - <id> [count]");
        }
        return Ok(Command::Update {
            target: UpdateTarget::OneShot(id),
            count: parse_count(args.get(1))?,
        });
    }

    // Otherwise the query form: `- <words…> [count]`, where a trailing
    // numeric word is the count and the rest is the word query.
    let mut words: Vec<String> = args.to_vec();
    let count = if words.len() > 1 && words.last().is_some_and(|w| w.parse::<i32>().is_ok()) {
        Some(
            words
                .pop()
                .expect("len > 1 checked above")
                .parse::<i32>()
                .context("Count must be a number")?,
        )
    } else {
        None
    };
    if words.is_empty() {
        anyhow::bail!(
            "Invalid update target: '{}'. Use a numeric id or the task's words",
            first
        );
    }

    Ok(Command::Update {
        target: UpdateTarget::Query { words },
        count,
    })
}

/// Parse an optional trailing count for the id update form.
fn parse_count(arg: Option<&String>) -> anyhow::Result<Option<i32>> {
    match arg {
        Some(s) => Ok(Some(s.parse::<i32>().context("Count must be a number")?)),
        None => Ok(None),
    }
}
