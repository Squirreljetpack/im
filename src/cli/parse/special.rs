use clap::Parser;

use super::super::{Command, DbSubcommand, ToolSubcommand};
use super::groups::SyncCli;
use super::tracker::parse_tracker_command;

pub(crate) fn parse_special_command(args: &[String]) -> anyhow::Result<Command> {
    let first = &args[0];

    if first == ":tool" {
        let sub = ToolSubcommand::parse_from(clap_argv(&args[1..]));
        return Ok(Command::Tool(sub));
    }

    // `:c` is a shorthand for `:tool config`; clap renders its help and
    // validates its optional target under the canonical name.
    if first == ":c" {
        let sub = ToolSubcommand::parse_from(
            std::iter::once("im".to_string())
                .chain(std::iter::once("config".to_string()))
                .chain(args[1..].iter().cloned()),
        );
        return Ok(Command::Tool(sub));
    }

    if first == ":db" {
        let sub = DbSubcommand::parse_from(clap_argv(&args[1..]));
        return Ok(Command::Db(sub));
    }

    // Tracker view: dispatches on two syntaxes:
    //   `:`                — default period (Week), then an ordered display
    //                        list of trackers and `:` mood-grid markers
    //   `:week|month|year` — period as a bare suffix on the first token
    // A stray `:<unknown>` token (e.g. `:foo`) is rejected: if the user
    // meant an id filter, they need to type `im : foo` with a space.
    if is_bare_tracker_form(first) {
        return parse_tracker_command(args);
    }

    if first == ":sync" {
        let SyncCli { sub, reset } = SyncCli::parse_from(clap_argv(&args[1..]));
        return Ok(Command::Sync { sub, reset });
    }

    anyhow::bail!("Unknown special command: {}", first)
}

/// Clap expects argv[0] to be the program name; `bin_name` on each group
/// replaces it with the colon spelling in usage and help.
fn clap_argv(args: &[String]) -> impl Iterator<Item = String> + '_ {
    std::iter::once("im".to_string()).chain(args.iter().cloned())
}

/// `im :` and its period variants — the tracker grids, whose arguments are
/// all tracker ids. Kept apart from the named subcommands because these take
/// no `--help` token (an id may legitimately look like a flag).
fn is_bare_tracker_form(first: &str) -> bool {
    matches!(first, ":" | ":week" | ":month" | ":year")
}
