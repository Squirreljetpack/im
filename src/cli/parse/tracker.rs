use super::super::{Command, TrackerItem, TrackerPeriod};

pub(crate) fn parse_tracker_command(args: &[String]) -> anyhow::Result<Command> {
    // args[0] is either ":" or one of ":week" / ":month" / ":year". Only the
    // suffix on the first token sets the period; everything after it is an
    // ordered display list where a bare ":" token is a positional mood-grid
    // marker and any other token is a tracker id.
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
        .map(|a| {
            if a == ":" {
                TrackerItem::Mood
            } else {
                TrackerItem::Tracker(a.clone())
            }
        })
        .collect();

    // Bare `:` (no items at all) renders just the mood grid, same as `: :`.
    let items = if items.is_empty() {
        vec![TrackerItem::Mood]
    } else {
        items
    };

    Ok(Command::Tracker { period, items })
}
