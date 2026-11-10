use crate::types::{Entry, Task, TaskRef, TodayHorizon, ViewMode, ViewVariant};

pub const FLAG_CHARACTERS: &str = "qvF";

/// The character that splits a command line into its argument part and its
/// body-text part (tasks and entries alike): any argument made solely of
/// this character (one or more dots) is a body delimiter, and the first
/// such argument wins — everything before it is parsed as command
/// arguments, everything after is joined into `body` verbatim. The dot
/// count of the splitting argument picks the body-editor template: `n`
/// dots open the `n`th template (1-based).
pub const BODY_DELIMITER: char = '.'; // by idiom this should be --, but . or .. feels more linguistic.

/// True when `arg` is a body delimiter: non-empty and made solely of
/// `BODY_DELIMITER` characters.
pub fn is_body_delimiter(arg: &str) -> bool {
    !arg.is_empty() && arg.chars().all(|c| c == BODY_DELIMITER)
}

/// Counts of the leading `-q` / `-v` flag characters. `qv[0]` = number of
/// `q` chars, `qv[1]` = number of `v` chars (combined tokens like `-qv`
/// count once each). Order is not tracked — the logger and handlers only
/// care about presence/counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CliOpts {
    pub qv: [u8; 2],
    /// `-F` in the initial position: run the TUI fullscreen (mm config
    /// `tui.layout = None`) instead of the configured percentage layout.
    pub fullscreen: bool,
}

impl CliOpts {
    pub fn quiet(&self) -> bool {
        self.qv[0] > 0
    }
    pub fn verbose(&self) -> bool {
        self.qv[1] > 0
    }
    /// `-vv`-gated output (e.g. the WP7 grid period suffix).
    pub fn verbose_level(&self) -> u8 {
        self.qv[1]
    }
}

/// A parsed command line: the flags given in the initial position (`-q` /
/// `-v`, as counts) plus the command they apply to. The flags drive log
/// verbosity in `main.rs` and quiet/verbose output in the commands;
/// `cmd` is what `execute_command` dispatches on.
#[derive(Debug, Clone, PartialEq)]
pub struct Cli {
    pub opts: CliOpts,
    pub cmd: Command,
}

/// Which file `im :tool config [<target>]` opens in $VISUAL/$EDITOR.
/// `Main` is the active config; `Moods` the moods file named by
/// `[moods] source`; `Colors` the colors file (`colors.toml`); `Mm` the
/// matchmaker render config (`mm.toml`). `:c` is an alias for
/// `:tool config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ConfigTarget {
    /// The active config file.
    #[default]
    #[value(skip)]
    Main,
    /// The moods file.
    Moods,
    /// The colors file.
    Colors,
    /// The matchmaker render config.
    Mm,
}

/// Output representation for `im :tool datetime`.
///
/// Parsed from a preset name (`civil`, `utc`, `short`, …), a time zone
/// (`America/New_York`, `NewYork`, `+08:00`), or a `strftime` pattern
/// (`%d %m %y`). Every variant except `Utc`/`UnixSeconds` works on the
/// parsed instant's civil time; `Zone` re-renders it in another zone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DatetimeFormat {
    /// RFC 3339 in UTC (e.g. `2024-03-15T14:30:00Z`).
    Utc,
    /// Local civil datetime without a zone (e.g. `2024-03-15 14:30:00`).
    #[default]
    Civil,
    /// Unix epoch seconds.
    UnixSeconds,
    /// Human-friendly short form (e.g. `15 Mar 14:30`).
    Short,
    /// ISO 8601 with the local UTC offset (e.g. `2024-03-15T14:30:00-04:00`).
    Iso,
    /// Calendar date only (`2024-03-15`).
    Date,
    /// Clock time only (`14:30:00`).
    Time,
    /// Day/month/year (`15/03/24`).
    Dmy,
    /// Month/day/year (`03/15/24`).
    Mdy,
    /// The instant rendered in a named or fixed-offset time zone.
    Zone(jiff::tz::TimeZone),
    /// A custom `strftime` pattern.
    Pattern(String),
}

/// Parse a `:tool datetime --format` value.
pub fn parse_datetime_format(raw: &str) -> Result<DatetimeFormat, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("datetime format cannot be empty".to_string());
    }
    match key.to_ascii_lowercase().as_str() {
        "utc" | "gmt" | "zulu" => return Ok(DatetimeFormat::Utc),
        "civil" | "local" => return Ok(DatetimeFormat::Civil),
        "unixseconds" | "unix" | "epoch" => return Ok(DatetimeFormat::UnixSeconds),
        "short" => return Ok(DatetimeFormat::Short),
        "iso" | "iso8601" => return Ok(DatetimeFormat::Iso),
        "date" => return Ok(DatetimeFormat::Date),
        "time" => return Ok(DatetimeFormat::Time),
        "dmy" | "dd-mm-yy" => return Ok(DatetimeFormat::Dmy),
        "mdy" | "mm-dd-yy" => return Ok(DatetimeFormat::Mdy),
        _ => {}
    }
    if let Some(zone) = parse_datetime_zone(key) {
        return Ok(DatetimeFormat::Zone(zone));
    }
    if key.contains('%') {
        return Ok(DatetimeFormat::Pattern(key.to_string()));
    }
    Err(format!(
        "unknown datetime format '{key}' (try civil, utc, short, unix, iso, date, time, dmy, \
         mdy; a time zone like America/New_York or +08:00; or a strftime pattern like '%d %m %y')"
    ))
}

/// A zone value: an IANA name (or an unambiguous part of one) or a fixed
/// UTC offset such as `+8`, `+0530`, or `-05:00`.
fn parse_datetime_zone(raw: &str) -> Option<jiff::tz::TimeZone> {
    let key = raw.trim();
    let lower = key.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("utc")
        .or_else(|| lower.strip_prefix("gmt"))
        && let Some(zone) = parse_fixed_zone(rest)
    {
        return Some(zone);
    }
    if let Some(zone) = parse_fixed_zone(key) {
        return Some(zone);
    }
    parse_zone_name(key)
}

/// Resolve a zone by exact name or by a case-insensitive substring of one.
/// Separators are ignored, so `NewYork`, `new_york`, and `America/New_York`
/// all match. Only unambiguous results are accepted; canonical `Area/City`
/// names win over legacy top-level aliases, since otherwise every city would
/// be ambiguous (`Singapore` and `Asia/Singapore` both exist).
fn parse_zone_name(raw: &str) -> Option<jiff::tz::TimeZone> {
    if let Ok(zone) = jiff::tz::TimeZone::get(raw) {
        return Some(zone);
    }
    let needle = normalize_zone_name(raw);
    if needle.is_empty() {
        return None;
    }
    let mut canonical = Vec::new();
    let mut legacy = Vec::new();
    for name in jiff::tz::db().available() {
        let name = name.as_str();
        if normalize_zone_name(name).contains(&needle) {
            if name.contains('/') {
                canonical.push(name.to_string());
            } else {
                legacy.push(name.to_string());
            }
        }
    }
    let matches = if canonical.is_empty() { legacy } else { canonical };
    match matches.as_slice() {
        [name] => jiff::tz::TimeZone::get(name).ok(),
        _ => None,
    }
}

fn normalize_zone_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn parse_fixed_zone(raw: &str) -> Option<jiff::tz::TimeZone> {
    let (sign, rest) = match raw.as_bytes().first()? {
        b'+' => (1, &raw[1..]),
        b'-' => (-1, &raw[1..]),
        _ => return None,
    };
    let (hours, minutes) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else {
        match rest.len() {
            0 => return None,
            1 | 2 => (rest, "0"),
            3 => rest.split_at(1),
            4 => rest.split_at(2),
            _ => return None,
        }
    };
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if hours > 25 || minutes >= 60 {
        return None;
    }
    let offset = jiff::tz::Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()?;
    Some(jiff::tz::TimeZone::fixed(offset))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Entry(Entry),
    View {
        mode: ViewMode,
        show: ViewVariant,
    },
    Tracker {
        period: TrackerPeriod,
        items: Vec<TrackerItem>,
    },
    Task(Task),
    TaskEdit {
        task: TaskRef,
    },
    /// `im` with no args — today view; `im @<date>` anchors it to
    /// an arbitrary day (any date string that parses); `im @due[:t|:w]`
    /// opens the today view at `ShowVariant::B` with the day/tomorrow/week
    /// horizon.
    Today {
        date: Option<String>,
        show: ViewVariant,
        horizon: TodayHorizon,
    },
    /// `im --help` / `im -h` / `im -vh` in the initial position (handled in
    /// `parse_cli`, before the command dispatchers). Handlers print the
    /// contents of `assets/help.txt` (or `assets/vhelp.txt` when verbose).
    /// Help for the clap-parsed `:` groups is printed and exits during parsing.
    Help,
    /// `im --version` / `im -V` in the initial position (handled in
    /// `parse_cli`, before the command dispatchers). Handlers print the
    /// package version derived from Cargo.
    Version,
    /// `im :tool <sub>` — local diagnostics and configuration helpers.
    Tool(ToolSubcommand),
    /// `im -` — a matchmaker-backed viewer listing every configured tracker
    /// (a single name column) with a live preview of each tracker's settings
    /// and a row of colored cells (one per entry of its resolved color
    /// palette).
    Matchmaker,
    /// `im :sync` — exchange events with the other devices. A subcommand
    /// manages the account instead; `reset` discards the local copy first,
    /// asking unless `force` says the queue is expendable.
    Sync {
        sub: Option<SyncSubcommand>,
        reset: bool,
        force: bool,
    },
}

/// Subcommands of `im :tool`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Parser)]
#[command(
    name = "im :tool",
    bin_name = "im :tool",
    about = "Local diagnostics and configuration helpers",
    disable_help_subcommand = true
)]
pub enum ToolSubcommand {
    /// Print an embedding vector for each stdin line.
    Embed,
    /// Score a start and end mood (stub).
    Score {
        /// Mood to start from
        start: String,
        /// Mood to end at
        end: String,
    },
    /// Open the config, moods file, or colors file in $VISUAL/$EDITOR.
    Config {
        /// File to open (defaults to the active config)
        #[arg(value_enum)]
        target: Option<ConfigTarget>,
    },
    /// Show the mood-color palette, or diagnose one mood.
    Mood {
        /// Mood to diagnose (words are joined)
        #[arg(value_name = "MOOD", num_args = 0.., allow_hyphen_values = true)]
        mood: Vec<String>,
    },
    /// Clear all mood entries from a day.
    Clear {
        /// Day to clear (defaults to today)
        #[arg(value_name = "DATE", num_args = 0..)]
        date: Vec<String>,
    },
    /// Parse a date/time expression and print the resulting instant.
    Datetime {
        /// Expression to parse (words are joined; defaults to `now`)
        #[arg(
            value_name = "EXPR",
            num_args = 0..,
            allow_negative_numbers = true
        )]
        expr: Vec<String>,
        /// Output format: civil, utc, short, unix, iso, date, time, dmy, mdy;
        /// a time zone (America/New_York, NewYork, +08:00); or a strftime
        /// pattern
        #[arg(
            long,
            value_name = "FORMAT",
            default_value = "civil",
            allow_hyphen_values = true,
            value_parser = parse_datetime_format
        )]
        format: DatetimeFormat,
    },
    /// Send a desktop notification or dialogue.
    Notify(notify::NotifyCli),
    /// Database maintenance utilities.
    #[command(subcommand)]
    Db(DbSubcommand),
}

/// Subcommands of `im :sync`; a bare `im :sync` runs a sync instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum SyncSubcommand {
    /// Create an account and sign this device in.
    Register,
    /// Sign this device in to an existing account.
    Login,
    /// Show the account this device is signed in as.
    Status,
    /// Forget the account on this device; local data stays.
    Logout,
}

/// Subcommands of `im :tool db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Subcommand)]
pub enum DbSubcommand {
    /// Delete finished and expired tasks.
    Prune,
    /// Fill in missing mood data.
    Backfill,
    /// Check tracker data and remove mismatches after confirmation.
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerPeriod {
    Week,
    Month,
    Year,
}

/// One item in a `im :` display list. `Mood` is a positional marker
/// (a bare `:` token in the args) that renders the mood grid at that spot;
/// `Tracker(name)` (`-name`) renders that tracker's dot grid;
/// `Recurring(name)` (`@name`) the recurring task's completion dots; the
/// remaining three are per-day heatmaps — `TaskHeatmap(query)` (`+query`)
/// over task completions, `EntryHeatmap(query)` (`%query`) over entry
/// counts, and `Anchor(query)` (any bare word) over the day's semantic
/// similarity to the query as a mood anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerItem {
    Mood,
    Tracker(String),
    Recurring(String),
    TaskHeatmap(String),
    EntryHeatmap(String),
    Anchor(String),
}

mod parse;
mod parser;

pub use parser::{parse_args, parse_cli, parse_from};
