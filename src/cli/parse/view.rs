use super::super::Command;
use crate::types::{TodayHorizon, ViewMode, ViewVariant};

pub(crate) fn parse_view_command(args: &[String]) -> anyhow::Result<Command> {
    // Everything after the `@` token joins into the command text: multi-word
    // datetimes work (`im @2024-03-20 14:30`), while a stray token
    // after `@`/`@due`/`@done` is rejected here (or by the handler's date
    // parse for `@ <words>`) — never silently ignored. Only the first word
    // can carry a variant/horizon suffix (`@:o`, `@due:t`); a colon later
    // in the string is part of a time (`@2024-03-20 14:30`).
    let joined = args.join(" ");
    let token = joined.strip_prefix('@').unwrap_or(&joined).trim_start();
    let (word, rest) = match token.split_once(' ') {
        Some((word, rest)) => (word, Some(rest)),
        None => (token, None),
    };
    let (base, suffix) = match word.split_once(':') {
        Some((base, suffix)) => (base, Some(suffix)),
        None => (word, None),
    };
    let reject_extra = |rest: Option<&str>| -> anyhow::Result<()> {
        if let Some(extra) = rest {
            anyhow::bail!(
                "Unexpected argument '{extra}' after '{joined}' — view commands take no further arguments"
            );
        }
        Ok(())
    };

    match base {
        // `@[:o|:O]` → pending view; suffix `` → All, `o` → A, `O` → B.
        // The variant suffix is `o` (A) or `O` (B) — there is no `a`
        // suffix, so `@:a` is invalid.
        "" => {
            reject_extra(rest)?;
            Ok(Command::View {
                mode: ViewMode::PendingTasks,
                show: parse_variant_suffix(suffix)?,
            })
        }
        // `@done[:o|:O]` → completed view with the same suffixes.
        "done" => {
            reject_extra(rest)?;
            Ok(Command::View {
                mode: ViewMode::DoneTasks,
                show: parse_variant_suffix(suffix)?,
            })
        }
        // `@due[:t|:w]` → TodayView at ShowVariant::B, horizon per suffix
        // (`` → Today, `t` → Tomorrow, `w` → Week).
        "due" => {
            reject_extra(rest)?;
            let horizon = match suffix {
                None => TodayHorizon::Today,
                Some("t") => TodayHorizon::Tomorrow,
                Some("w") => TodayHorizon::Week,
                Some(other) => anyhow::bail!("Unknown @due suffix: ':{}' (use :t or :w)", other),
            };
            Ok(Command::Today {
                date: None,
                show: ViewVariant::B,
                horizon,
            })
        }
        // Any other @-word is a today-view date: `im @2024-03-20`
        // (plus optional time words, `im @2024-03-20 14:30`).
        // Parsing happens in the handler with `DATE_DIALECT` (the CLI
        // parser has no config), so an unparseable date fails there with
        // a clear error rather than here.
        _ => {
            if suffix.is_some() {
                anyhow::bail!(
                    "Unknown view suffix: '{}' (use :o or :O after @/@done, :t or :w after @due)",
                    joined
                );
            }
            Ok(Command::Today {
                date: Some(token.to_string()),
                show: ViewVariant::All,
                horizon: TodayHorizon::Today,
            })
        }
    }
}

/// The `ShowVariant` for a `@` / `@done` suffix: `` → All, `o` → A,
/// `O` → B; anything else is rejected (there is no `a` suffix — starting
/// in `A` is only possible via `:o`).
fn parse_variant_suffix(suffix: Option<&str>) -> anyhow::Result<ViewVariant> {
    match suffix {
        None => Ok(ViewVariant::All),
        Some("o") => Ok(ViewVariant::A),
        Some("O") => Ok(ViewVariant::B),
        Some(other) => anyhow::bail!(
            "Unknown view suffix: ':{}' (use :o for oneshots or :O for recurring+scheduled)",
            other
        ),
    }
}
