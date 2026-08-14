use super::super::Command;
use super::tracker::parse_tracker_command;

pub(crate) fn parse_special_command(args: &[String]) -> anyhow::Result<Command> {
    let first = &args[0];

    if first == ":embed" {
        return Ok(Command::Embed);
    }

    if first == ":score" {
        if args.len() < 3 {
            anyhow::bail!("Usage: im :score \"start\" \"end\"");
        }
        return Ok(Command::Score {
            start: args[1].trim_matches('"').to_string(),
            end: args[2].trim_matches('"').to_string(),
        });
    }

    if first == ":config" || first == ":c" {
        if args.len() != 1 {
            anyhow::bail!("Usage: im :config");
        }
        return Ok(Command::Config);
    }

    if first == ":moods" {
        if args.len() != 1 {
            anyhow::bail!("Usage: im :moods");
        }
        return Ok(Command::Moods);
    }

    if first == ":db" {
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
        if args.len() < 2 {
            anyhow::bail!("Usage: im :color \"<mood>\"");
        }
        return Ok(Command::Color {
            mood: args[1..].join(" ").trim_matches('"').to_string(),
        });
    }

    if first == ":clear" {
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
    if first == ":" || matches!(first.as_str(), ":week" | ":month" | ":year") {
        return parse_tracker_command(args);
    }

    // Grid view stub: :g (unimplemented)
    if first == ":g" {
        anyhow::bail!("Grid view (:g) is not yet implemented");
    }

    anyhow::bail!("Unknown special command: {}", first)
}
