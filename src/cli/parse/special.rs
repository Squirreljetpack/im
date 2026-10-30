use super::super::{AuthSubcommand, Command, ConfigTarget};
use super::tracker::parse_tracker_command;

pub(crate) fn parse_special_command(args: &[String]) -> anyhow::Result<Command> {
    let first = &args[0];

    if first == ":embed" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        return Ok(Command::Embed);
    }

    if first == ":score" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        if args.len() < 3 {
            anyhow::bail!("Usage: im :score \"start\" \"end\"");
        }
        return Ok(Command::Score {
            start: args[1].trim_matches('"').to_string(),
            end: args[2].trim_matches('"').to_string(),
        });
    }

    if first == ":config" || first == ":c" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        let target = match args.get(1).map(String::as_str) {
            None => ConfigTarget::Main,
            Some("moods") => ConfigTarget::Moods,
            Some("colors") => ConfigTarget::Colors,
            Some(other) => anyhow::bail!(
                "Unknown :config subcommand '{}' (expected moods or colors)",
                other
            ),
        };
        if args.len() > 2 {
            anyhow::bail!("Usage: im :config [moods|colors]");
        }
        return Ok(Command::Config { target });
    }

    if first == ":db" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        return match args.get(1).map(String::as_str) {
            Some("prune") => {
                if args.len() != 2 {
                    anyhow::bail!("Usage: im :db prune");
                }
                Ok(Command::Db {
                    sub: crate::cli::DbSubcommand::Prune,
                })
            }
            Some("backfill") => {
                if args.len() != 2 {
                    anyhow::bail!("Usage: im :db backfill");
                }
                Ok(Command::Db {
                    sub: crate::cli::DbSubcommand::Backfill,
                })
            }
            Some("doctor") => {
                if args.len() != 2 {
                    anyhow::bail!("Usage: im :db doctor");
                }
                Ok(Command::Db {
                    sub: crate::cli::DbSubcommand::Doctor,
                })
            }
            Some(other) => anyhow::bail!(
                "Unknown :db subcommand '{}' (expected prune, backfill, or doctor)",
                other
            ),
            None => anyhow::bail!("Usage: im :db prune | :db backfill | :db doctor"),
        };
    }

    if first == ":color" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        if args.len() < 2 {
            anyhow::bail!("Usage: im :color \"<mood>\"");
        }
        return Ok(Command::Color {
            mood: args[1..].join(" ").trim_matches('"').to_string(),
        });
    }

    if first == ":clear" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        let date_arg = if args.len() > 1 {
            let joined = args[1..].join(" ");
            let trimmed = joined.trim();
            let stripped = trimmed.strip_prefix('@').unwrap_or(trimmed).trim();
            if stripped.is_empty() {
                None
            } else {
                Some(stripped.to_string())
            }
        } else {
            None
        };
        return Ok(Command::Clear { date: date_arg });
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

    if first == ":auth" {
        return parse_auth_command(&args[1..]);
    }

    if first == ":sync" {
        if let Some(help) = help_flag(&args[1..]) {
            return Ok(help);
        }
        let reset = match args.get(1).map(String::as_str) {
            None => false,
            Some("--reset") => true,
            Some(other) => {
                anyhow::bail!("`im :sync` takes no argument other than --reset, got {other}")
            }
        };
        if args.len() > 2 {
            anyhow::bail!("`im :sync` takes at most one argument");
        }
        return Ok(Command::Sync { reset });
    }

    anyhow::bail!("Unknown special command: {}", first)
}

/// Help requested by a subcommand: `--help` / `-h` anywhere among its own
/// arguments (`im :db prune --help`). Each subcommand resolves this for
/// itself, so one that grows its own text can answer with a different
/// command instead of the top-level [`Command::Help`].
fn help_flag(args: &[String]) -> Option<Command> {
    args.iter()
        .any(|a| a == "--help" || a == "-h")
        .then_some(Command::Help)
}

/// `im :` and its period variants — the tracker grids, whose arguments are
/// all tracker ids. Kept apart from the named subcommands because these take
/// no `--help` token (an id may legitimately look like a flag).
fn is_bare_tracker_form(first: &str) -> bool {
    matches!(first, ":" | ":week" | ":month" | ":year")
}

/// `im :auth <register|login|status|logout>`.
fn parse_auth_command(args: &[String]) -> anyhow::Result<Command> {
    if let Some(help) = help_flag(args) {
        return Ok(help);
    }
    let Some(name) = args.first() else {
        anyhow::bail!("`im :auth` needs a subcommand: register, login, status or logout");
    };
    let sub = match name.as_str() {
        "register" => AuthSubcommand::Register,
        "login" => AuthSubcommand::Login,
        "status" => AuthSubcommand::Status,
        "logout" => AuthSubcommand::Logout,
        other => anyhow::bail!(
            "unknown auth subcommand '{other}' (expected register, login, status or logout)"
        ),
    };
    if args.len() > 1 {
        anyhow::bail!("`im :auth {name}` takes no further arguments");
    }
    Ok(Command::Auth { sub })
}
