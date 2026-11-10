use super::super::{Command, TrackerItem, TrackerPeriod};

pub(crate) fn parse_tracker_command(args: &[String]) -> anyhow::Result<Command> {
    // args[0] is either ":" or one of ":week" / ":month" / ":year". Only the
    // suffix on the first token sets the period; everything after it is an
    // ordered display list where a bare ":" token is a positional mood-grid
    // marker and any other token is an item picked by its leading sigil:
    // `-tracker`, `@recurring`, `+query` (task completions), `%query`
    // (entries), or any bare word (a mood-anchor query).
    let first = &args[0];

    let (period, items_from) = if first == ":" {
        // Bare `:` always uses the Week period; args[1..] are the display list.
        (TrackerPeriod::Week, 1)
    } else {
        let period = match first.strip_prefix(":") {
            Some("week") => TrackerPeriod::Week,
            Some("month") => TrackerPeriod::Month,
            Some("year") => TrackerPeriod::Year,
            _ => unreachable!("dispatcher only forwards :, :week, :month, :year"),
        };
        (period, 1)
    };

    let items: Vec<TrackerItem> = args[items_from..]
        .iter()
        .map(|a| parse_item(a))
        .collect::<anyhow::Result<_>>()?;

    // Bare `:` (no items at all) renders just the mood grid, same as `: :`.
    let items = if items.is_empty() {
        vec![TrackerItem::Mood]
    } else {
        items
    };

    Ok(Command::Tracker { period, items })
}

/// One display-list token → its grid item.
fn parse_item(arg: &str) -> anyhow::Result<TrackerItem> {
    if arg.is_empty() {
        // An empty argv token (stray quotes) renders a useless-but-harmless
        // empty anchor query.
        return Ok(TrackerItem::Anchor(String::new()));
    }
    if arg == ":" {
        return Ok(TrackerItem::Mood);
    }
    let (sigil, rest) = arg.split_at(1);
    Ok(match sigil {
        "-" => {
            // Tracker grids require the dash; tracker names never contain
            // one, so the split is unambiguous.
            if rest.is_empty() {
                anyhow::bail!("Empty tracker name in grid item '{arg}'");
            }
            TrackerItem::Tracker(rest.to_string())
        }
        // A bare `@` falls through to the recurring lookup's not-found path.
        "@" => TrackerItem::Recurring(rest.to_string()),
        // Empty queries match every task name / entry name.
        "+" => TrackerItem::TaskHeatmap(rest.to_string()),
        "%" => TrackerItem::EntryHeatmap(rest.to_string()),
        _ => TrackerItem::Anchor(arg.to_string()),
    })
}
