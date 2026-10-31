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
/// `[moods] source`; `Colors` the colors file (`colors.toml`). `:c` is an
/// alias for `:tool config`.
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
        task: Option<TaskRef>,
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
    /// `im :tool <sub>` — local diagnostics and configuration helpers.
    Tool(ToolSubcommand),
    /// `im -` — a matchmaker-backed viewer listing every configured tracker
    /// (a single name column) with a live preview of each tracker's settings
    /// and a row of colored cells (one per entry of its resolved color
    /// palette).
    Matchmaker,
    /// `im :db <sub>` — database maintenance utilities.
    Db(DbSubcommand),
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

/// Subcommands of `im :db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Parser)]
#[command(
    name = "im :db",
    bin_name = "im :db",
    about = "Database maintenance utilities",
    disable_help_subcommand = true
)]
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
/// `Tracker(name)` renders that tracker's grid (`@name` for recurring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerItem {
    Mood,
    Tracker(String),
}

mod parse;
mod parser;

pub use parser::{parse_args, parse_cli, parse_from};
