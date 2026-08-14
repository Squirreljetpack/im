#![allow(clippy::derivable_impls)]

use cba::{ebog, wbog};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::cli::FLAG_CHARACTERS;

#[cfg(debug_assertions)]
pub const DEFAULT_CONFIG: &str = include_str!("../../assets/dev.toml");
#[cfg(not(debug_assertions))]
pub const DEFAULT_CONFIG: &str = include_str!("../../assets/config.toml");

#[cfg(debug_assertions)]
pub const DEFAULT_MOODS: &str = include_str!("../../assets/moods.dev.toml");
#[cfg(not(debug_assertions))]
pub const DEFAULT_MOODS: &str = include_str!("../../assets/moods.toml");

mod types;
pub use types::*;

mod moods;
pub use moods::*;
mod tasks;
pub use tasks::*;
mod trackers;
pub use trackers::*;
mod views;
pub use views::*;

/// The whole configuration file (`config.toml`). Every section is optional
/// — a missing section or key falls back to a built-in default, so a config
/// can be as small as a single `[tracker.sleep]` block.
///
/// Sections: `[moods]` (color settings; the anchor pairs live in the file
/// named by `[moods] source`), `[tasks]` (defaults for new tasks, badge
/// colors), `[tracker.<name>]` (trackers), `[grid]` (tracker grid
/// ranges), `[tasks_view]` and `[today_view]` (view options), `[badges]`
/// (row marker glyphs), `[editor]` (body editor).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub moods: MoodConfig,

    #[serde(default)]
    pub tasks: TasksConfig,

    #[serde(default)]
    pub tracker: HashMap<String, TrackerSetting>,

    #[serde(default)]
    pub grid: GridViewConfig,

    #[serde(default)]
    pub preview: PreviewConfig,

    #[serde(default)]
    pub tasks_view: TasksViewConfig,

    #[serde(default)]
    pub today_view: TodayViewConfig,

    #[serde(default)]
    pub badges: BadgesConfig,

    #[serde(default)]
    pub editor: EditorConfig,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG).expect("bundled assets/config.toml must parse into Config")
    }
}

impl Config {
    /// Normalize a loaded config before use: drop trackers whose
    /// names cannot be addressed from the CLI, clear non-positive tracker
    /// intervals (they would divide by zero when computing replacement
    /// slots), and fall back to the default badge palette when fewer than
    /// three colors are configured in `tasks.colors`. Run automatically at
    /// startup, before any command is handled.
    ///
    /// The bundled default config never needs this; a user-edited config
    /// may. See [`is_valid_tracker_name`] for the exact tracker-name rules.
    pub fn init(&mut self) {
        // Drop trackers whose names are unusable: a `:` prefix collides with
        // the grid-view `:` command, `-`/whitespace can't be addressed as
        // `-name value`, names made purely of the flag characters
        // (`q`/`v`) would be swallowed by the leading `-q`/`-v` flags, and
        // purely numeric names collide with the `! -<parent_id>` flag.
        self.tracker.retain(|name, _| {
            if !is_valid_tracker_name(name) {
                cba::ebog!(
                    "config";
                    "Dropping unusable tracker '{}': names cannot begin with ':', contain '-' or whitespace, be purely numeric, or consist solely of flag characters '{}'",
                    name, FLAG_CHARACTERS
                );
                false
            } else {
                true
            }
        });
        // Validate tracker-level color overrides: only an empty palette is
        // unusable — single- and two-color palettes are fine, every badge
        // path degrades to the first/last (or sole) color — so clear it
        // and warn.
        for (name, setting) in self.tracker.iter_mut() {
            if let Some(ref colors) = setting.colors
                && colors.is_empty() {
                    wbog!(
                        "config";
                        "Ignoring empty colors override on Tracker '{}'",
                        name
                    );
                    setting.colors = None;
                }
            // Text trackers have no score: their override palette, when
            // present, must be exactly one color (the entry-badge color);
            // anything else is meaningless, so clear it and warn.
            if setting.kind == TrackerKind::Text
                && let Some(ref colors) = setting.colors
                    && colors.len() != 1 {
                        wbog!(
                            "config";
                            "Ignoring colors override on text Tracker '{}' with {} entries (text trackers take exactly 1 color)",
                            name,
                            colors.len()
                        );
                        setting.colors = None;
                    }
            // A zero interval span would break the calendar slot math, so
            // clear it and warn (parse_span already rejects non-positive
            // input, so this only guards hand-constructed values).
            if setting
                .interval
                .is_some_and(|iv| crate::date::span_to_db(&iv.span) == 0)
            {
                ebog!(
                    "config";
                    "Ignoring zero interval setting on Tracker '{name}'"
                );
                setting.interval = None;
            }
        }
        // tasks.colors drives the completion badge and numeric binning; it
        // needs at least 3 entries to be meaningful, so fall back to the
        // default palette when fewer than three are configured.
        if self.tasks.colors.len() < 3 {
            wbog!(
                "Less than 3 colors defined for config.tasks.colors, overriding with the default."
            );
            self.tasks.colors = Default::default();
        }
    }
}

// Tracker-name validity for `Config::init`. A name is usable only when it
// is non-empty, does not begin with `:` (grid-view command syntax), contains
// no `-` or whitespace, is not made purely of the leading flag
// characters (`q` / `v`), and is not purely numeric — `-123` would
// collide with the `! -<parent_id>` task flag.
fn is_valid_tracker_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with(':') {
        return false;
    }
    if name.contains('-') || name.chars().any(char::is_whitespace) {
        return false;
    }
    if name.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    !name.chars().all(|c| FLAG_CHARACTERS.contains(c))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_tracker_name_validation() {
        assert_eq!(FLAG_CHARACTERS, "qvF");
        // Valid names
        assert!(is_valid_tracker_name("sleep"));
        assert!(is_valid_tracker_name("run_times"));
        assert!(is_valid_tracker_name("mood_notes"));
        assert!(is_valid_tracker_name("query")); // 'q' inside a longer name is fine
        assert!(is_valid_tracker_name("vibe"));
        assert!(is_valid_tracker_name("flag")); // 'F' inside a longer name is fine
        // ':' prefix collides with grid view specifiers
        assert!(!is_valid_tracker_name(":foo"));
        // forbid '-' or whitespace
        assert!(!is_valid_tracker_name("sleep-time"));
        assert!(!is_valid_tracker_name("my sleep"));
        assert!(!is_valid_tracker_name("my\tsleep"));
        // Names made purely of flag characters (q / v / F) are reserved
        assert!(!is_valid_tracker_name("qv"));
        assert!(!is_valid_tracker_name("F"));
        assert!(!is_valid_tracker_name("Fqv"));
        // Purely numeric names collide with the `! -<parent_id>` flag
        assert!(!is_valid_tracker_name("123"));
        assert!(!is_valid_tracker_name("0"));
        assert!(!is_valid_tracker_name(""));
    }

    #[test]
    fn test_init_clears_zero_intervals() {
        let mut config = Config::default();
        let zero = TrackerInterval {
            anchor: 0,
            span: jiff::Span::new(),
        };
        config.tracker.insert(
            "zero".to_string(),
            TrackerSetting::new(TrackerKind::Float).with_interval(zero),
        );
        let good = TrackerInterval {
            anchor: 0,
            span: jiff::Span::new().days(1),
        };
        config.tracker.insert(
            "good".to_string(),
            TrackerSetting::new(TrackerKind::Float).with_interval(good),
        );
        config.init();

        assert_eq!(config.tracker["zero"].interval, None);
        assert_eq!(config.tracker["good"].interval, Some(good));
    }

    #[test]
    fn test_init_drops_invalid_trackers() {
        let mut config = Config::default(); // debug: assets/dev.toml trackers
        for bad in [
            ":collide",
            "sleep-time",
            "two words",
            "q",
            "v",
            "qv",
            "123",
            "",
        ] {
            config
                .tracker
                .insert(bad.to_string(), TrackerSetting::default());
        }
        config.init();

        for bad in [
            ":collide",
            "sleep-time",
            "two words",
            "q",
            "v",
            "qv",
            "123",
            "",
        ] {
            assert!(
                !config.tracker.contains_key(bad),
                "tracker {:?} should have been dropped",
                bad
            );
        }
        // The bundled dev.toml trackers survive untouched.
        for good in [
            "sleep",
            "run_times",
            "water",
            "notes",
            "steps",
            "mood_notes",
            "temperature",
        ] {
            assert!(
                config.tracker.contains_key(good),
                "tracker {:?} should survive init",
                good
            );
        }
    }

    #[test]
    fn test_init_clears_empty_tracker_colors() {
        let mut config = Config::default();

        // An empty colors override should be cleared and a warning emitted
        config.tracker.insert(
            "bad_colors".to_string(),
            TrackerSetting {
                colors: Some(ColorBins::from(vec![])),
                ..Default::default()
            },
        );
        // Single- and two-color palettes are valid now — they should be kept
        // (explicit non-text kinds: text trackers are restricted to 1 color).
        config.tracker.insert(
            "one_color".to_string(),
            TrackerSetting {
                kind: TrackerKind::Number,
                colors: Some(ColorBins::from(vec![crossterm::style::Color::DarkRed])),
                ..Default::default()
            },
        );
        config.tracker.insert(
            "two_colors".to_string(),
            TrackerSetting {
                kind: TrackerKind::Number,
                colors: Some(ColorBins::from(vec![
                    crossterm::style::Color::DarkRed,
                    crossterm::style::Color::DarkGreen,
                ])),
                ..Default::default()
            },
        );
        // colors with 3+ entries should be kept
        config.tracker.insert(
            "good_colors".to_string(),
            TrackerSetting {
                kind: TrackerKind::Number,
                colors: Some(ColorBins::from(vec![
                    crossterm::style::Color::DarkRed,
                    crossterm::style::Color::DarkYellow,
                    crossterm::style::Color::DarkGreen,
                ])),
                ..Default::default()
            },
        );
        // None colors should be left as None
        config
            .tracker
            .insert("no_colors".to_string(), TrackerSetting::default());

        config.init();

        assert!(config.tracker["bad_colors"].colors.is_none());
        assert_eq!(
            config.tracker["one_color"].colors.as_ref().unwrap().len(),
            1
        );
        assert_eq!(
            config.tracker["two_colors"].colors.as_ref().unwrap().len(),
            2
        );
        assert!(config.tracker["good_colors"].colors.is_some());
        assert_eq!(
            config.tracker["good_colors"].colors.as_ref().unwrap().len(),
            3
        );
        assert!(config.tracker["no_colors"].colors.is_none());
    }

    #[test]
    fn test_init_clears_non_single_text_tracker_colors() {
        let mut config = Config::default();

        // A text tracker override must be exactly 1 color (entry-badge
        // color); 2+ entries are cleared with a warning.
        config.tracker.insert(
            "bad_text".to_string(),
            TrackerSetting {
                kind: TrackerKind::Text,
                colors: Some(ColorBins::from(vec![
                    crossterm::style::Color::DarkRed,
                    crossterm::style::Color::DarkGreen,
                ])),
                ..Default::default()
            },
        );
        // A single-color text override is valid and survives.
        config.tracker.insert(
            "good_text".to_string(),
            TrackerSetting {
                kind: TrackerKind::Text,
                colors: Some(ColorBins::from(vec![crossterm::style::Color::DarkRed])),
                ..Default::default()
            },
        );
        // Non-text trackers are unaffected by the single-color rule.
        config.tracker.insert(
            "number_multi".to_string(),
            TrackerSetting {
                kind: TrackerKind::Number,
                colors: Some(ColorBins::from(vec![
                    crossterm::style::Color::DarkRed,
                    crossterm::style::Color::DarkGreen,
                ])),
                ..Default::default()
            },
        );

        config.init();

        assert!(config.tracker["bad_text"].colors.is_none());
        assert_eq!(
            config.tracker["good_text"].colors.as_ref().unwrap().len(),
            1
        );
        assert_eq!(
            config.tracker["number_multi"].colors.as_ref().unwrap().len(),
            2
        );
    }

    #[test]
    fn test_init_replaces_small_tasks_colors() {
        // Fewer than 3 colors in tasks.colors — both the empty and the
        // single-color case — are replaced with the default palette.
        for small in [
            Vec::<crossterm::style::Color>::new(),
            vec![crossterm::style::Color::DarkRed],
        ] {
            let mut config = Config::default();
            config.tasks.colors = ColorBins::from(small);
            config.init();
            assert_eq!(config.tasks.colors.len(), 3);
        }
    }

    #[test]
    fn test_editor_config_serde_defaults() {
        // Missing [editor] section → all template lists default to empty
        // (out-of-range dot counts fall back to the legacy hint).
        let cfg: Config = toml::from_str("").expect("empty toml parses");
        assert!(cfg.editor.mood_template.is_empty());
        assert!(cfg.editor.task_template.is_empty());
        assert!(cfg.editor.recurring_template.is_empty());
        assert!(cfg.editor.scheduled_template.is_empty());

        // Empty [editor] section → same defaults.
        let cfg: Config = toml::from_str("[editor]\n").expect("empty editor table parses");
        assert!(cfg.editor.mood_template.is_empty());
        assert!(cfg.editor.task_template.is_empty());

        // Explicit arrays are honored, including empty entries (blank
        // document) and multiple entries (selected by dot count).
        let cfg: Config = toml::from_str(
            "[editor]\n\
             mood_template = [\"templates/mood.txt\", \"\"]\n\
             task_template = []\n\
             recurring_template = [\"templates/recurring.txt\"]\n\
             scheduled_template = [\"templates/scheduled.txt\"]\n",
        )
        .expect("template arrays parse");
        assert_eq!(
            cfg.editor.mood_template,
            vec![PathBuf::from("templates/mood.txt"), PathBuf::new()]
        );
        assert!(cfg.editor.task_template.is_empty());
        assert_eq!(
            cfg.editor.recurring_template,
            vec![PathBuf::from("templates/recurring.txt")]
        );
        assert_eq!(
            cfg.editor.scheduled_template,
            vec![PathBuf::from("templates/scheduled.txt")]
        );

        // The removed `hint` key is rejected (deny_unknown_fields).
        assert!(toml::from_str::<Config>("[editor]\nhint = true\n").is_err());
    }

    #[test]
    fn test_moods_source_serde_roundtrip() {
        // [moods] with only `source` (all settings missing) → settings default.
        let cfg: Config = toml::from_str("[moods]\nsource = \"moods.toml\"\n")
            .expect("[moods] with only source parses");
        assert_eq!(cfg.moods.axes.prefix_string, "person says: ");
        assert_eq!(cfg.moods.axes.blend_steepness, 2.0);
        assert_eq!(cfg.moods.source, PathBuf::from("moods.toml"));

        // Explicit settings are honored through the flatten.
        let cfg: Config = toml::from_str(
            "[moods]\nblend_steepness = 3.5\ntop_k = 8\nsource = \"my-moods.toml\"\n",
        )
        .expect("[moods] with settings parses");
        assert_eq!(cfg.moods.axes.blend_steepness, 3.5);
        assert_eq!(cfg.moods.axes.top_k, 8);
        assert_eq!(cfg.moods.source, PathBuf::from("my-moods.toml"));

        // A missing `source` key defaults to the empty path.
        let empty: Config = toml::from_str("").expect("empty toml parses");
        assert!(empty.moods.source.as_os_str().is_empty());

        // Unknown keys under [moods] are rejected (deny_unknown_fields holds
        // through the flattened ColorAxesSettings).
        assert!(
            toml::from_str::<Config>("[moods]\nbogus_key = 1\n").is_err(),
            "unknown [moods] key must be rejected"
        );

        // Full round-trip: serialize then re-parse keeps source + settings.
        let serialized = toml::to_string(&cfg).expect("serializes");
        let reparsed: Config = toml::from_str(&serialized).expect("re-parses");
        assert_eq!(reparsed.moods.axes.blend_steepness, 3.5);
        assert_eq!(reparsed.moods.source, cfg.moods.source);
    }

    #[test]
    fn test_moods_file_deserialization() {
        // The bundled moods file must parse and yield anchors.
        let moods = MoodsFile::default();
        assert!(!moods.pairs.is_empty());
        assert!(moods.pairs.iter().all(|p| !p.mood.is_empty()));

        // A moods file with explicit entries deserializes.
        let moods: MoodsFile = toml::from_str(
            "[[pairs]]\nmood = \"happy\"\ncolor = \"#FF0000\"\n\
             [[pairs]]\nmood = \"sad\"\ncolor = \"blue\"\n",
        )
        .expect("moods file parses");
        assert_eq!(moods.pairs.len(), 2);
        assert_eq!(moods.pairs[0].mood, "happy");
        assert_eq!(moods.pairs[1].color, crossterm::style::Color::Blue);

        // Unknown keys in the moods file are rejected.
        assert!(toml::from_str::<MoodsFile>("bogus = 1\n").is_err());
    }

    #[test]
    fn test_tracker_interval_serde() {
        // The interval deserializes from ["<anchor timestamp>", "<span>"];
        // anchors are RFC 3339 timestamps with an explicit UTC offset.
        let cfg: Config = toml::from_str(
            r#"
            [tracker.sleep]
            interval = ["2020-01-01T00:00:00Z", "1 day"]
            kind = "null"
            "#,
        )
        .expect("interval array parses");
        let iv = cfg.tracker["sleep"].interval.expect("interval set");
        assert_eq!(
            iv.anchor,
            "2020-01-01T00:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_second()
        );
        assert_eq!(iv.span.fieldwise(), jiff::Span::new().days(1).fieldwise());
        assert_eq!(cfg.tracker["sleep"].kind, TrackerKind::Null);

        // Old plain-string form is rejected with a clear message.
        let err =
            toml::from_str::<Config>("[tracker.sleep]\ninterval = \"1 day\"\nkind = \"float\"\n")
                .unwrap_err();
        assert!(
            err.to_string().contains("expected a sequence"),
            "unexpected error: {err}"
        );

        // Anchors without an explicit UTC offset are rejected.
        assert!(toml::from_str::<Config>(
            "[tracker.sleep]\ninterval = [\"2020-01-01 00:00\", \"1 day\"]\nkind = \"null\"\n"
        )
        .is_err());

        // Serialization roundtrip (anchors serialize back as RFC 3339).
        let serialized = toml::to_string(&cfg).unwrap();
        let reparsed: Config = toml::from_str(&serialized).expect("re-parses");
        let iv2 = reparsed.tracker["sleep"]
            .interval
            .expect("interval survives");
        assert_eq!(iv, iv2);

        // A zero span is rejected at parse time.
        assert!(toml::from_str::<Config>(
            "[tracker.sleep]\ninterval = [\"2020-01-01T00:00:00Z\", \"0 days\"]\n"
        )
        .is_err());
    }

    #[test]
    fn test_tracker_min_max_number_or_duration() {
        // min/max accept plain numbers or humantime duration strings
        // (converted to seconds, 1.0 = 1 s).
        let cfg: Config = toml::from_str(
            r#"
            [tracker.sleep]
            kind = "null"
            min = "20h"
            max = "4h"
            [tracker.rating]
            kind = "float"
            min = 0
            max = 9
            [tracker.pushups]
            kind = "number"
            min = 10
            [tracker.mile]
            kind = "float"
            min = "4m"
            max = "10m"
            "#,
        )
        .expect("trackers parse");
        assert_eq!(cfg.tracker["sleep"].min, Some(72000.0));
        assert_eq!(cfg.tracker["sleep"].max, Some(14400.0));
        assert_eq!(cfg.tracker["rating"].min, Some(0.0));
        assert_eq!(cfg.tracker["rating"].max, Some(9.0));
        assert_eq!(cfg.tracker["pushups"].min, Some(10.0));
        assert_eq!(cfg.tracker["pushups"].max, None);
        assert_eq!(cfg.tracker["mile"].min, Some(240.0));
        assert_eq!(cfg.tracker["mile"].max, Some(600.0));

        // A string that is neither a number nor a duration is rejected.
        assert!(
            toml::from_str::<Config>("[tracker.sleep]\nkind = \"null\"\nmin = \"bogus\"\n")
                .is_err()
        );
    }

    #[test]
    fn test_load_pairs_default_when_source_empty() {
        // Empty source (the default) resolves to the bundled pairs, and
        // never touches the filesystem.
        let config = MoodConfig::default();
        assert!(config.source.as_os_str().is_empty());
        let pairs = config.load_pairs();
        assert_eq!(pairs.len(), MoodsFile::default().pairs.len());
        assert!(!pairs.is_empty());
    }
}
