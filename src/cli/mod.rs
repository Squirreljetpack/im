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

/// Which file `im :config [<target>]` opens in $VISUAL/$EDITOR.
/// `Main` is the active config; `Moods` the moods file named by
/// `[moods] source`; `Colors` the colors file (`colors.toml`). `:c` is an
/// alias for `:config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigTarget {
    /// `im :config` — the active config file.
    #[default]
    Main,
    /// `im :config moods` — the moods file named by `[moods] source`.
    Moods,
    /// `im :config colors` — the colors file (`colors.toml`).
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
    Embed,
    Score {
        start: String,
        end: String,
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
    /// `im --help` / `im -h` in the initial position (handled in
    /// `parse_cli`, before the command dispatchers — `parse_from` never sees
    /// a help token). Handlers print the contents of `assets/help.txt`.
    Help,
    /// `im :config [moods|colors]` — handlers open a config-style file in
    /// $VISUAL/$EDITOR via [`crate::editor::open_editor_at`]. With no
    /// subcommand the active config is opened (the bundled `assets/config.toml`
    /// is copied to the path first when missing); `moods` opens the moods
    /// file named by `[moods] source`; `colors` opens the colors file
    /// (`colors.toml`). `:c` is an alias for `:config`.
    Config { target: ConfigTarget },
    /// `im -` — a matchmaker-backed viewer listing every configured tracker
    /// (a single name column) with a live preview of each tracker's settings
    /// and a row of colored cells (one per entry of its resolved color
    /// palette).
    Matchmaker,
    /// `im :db prune` — delete completed oneshot tasks and recurring
    /// tasks whose `end_time` has passed.
    Db {
        sub: DbSubcommand,
    },
    /// `im :color <mood>` — embed a mood string (with `"mood "`
    /// prefix) and print the projected Oklab / sRGB color plus intermediate
    /// pipeline values (raw scores, blend factors, per-axis colors).
    /// Diagnostic tool for debugging the mood-color pipeline.
    Color {
        mood: String,
    },
    /// `im :clear [@date]` — clear all mood entries from that day.
    Clear {
        date: Option<String>,
    },
}

/// Subcommands of `im :db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbSubcommand {
    /// `:db prune` — delete completed oneshot tasks and expired recurring
    /// tasks (the former `:prune` command).
    Prune,
    /// `:db backfill` — compute and persist missing mood embeddings and
    /// saliency scores (rendering no longer backfills them inline).
    Backfill,
    /// `:db doctor` — check every tracker entry's storage class against the
    /// tracker's current configured kind and prune the mismatches (orphaned
    /// tracker types included), after an interactive confirm.
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
