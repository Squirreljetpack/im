use std::path::PathBuf;
use std::time::Duration;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Configuration for timed mood sessions (`im %<duration>`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// Notification title for session completion. Defaults to "im".
    pub title: String,

    /// Whether to play the configured completion sound without `-v`. `-v`
    /// enables it per run.
    pub play_sound: bool,

    /// Sound to play on completion ("chime", "bell", "bowl", or path to audio file).
    /// Relative paths resolve against the config directory. Empty = no sound.
    #[serde(default)]
    pub sound: PathBuf,

    /// Optional audio playback duration cap (None = full length of audio file).
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration"
    )]
    pub sound_duration: Option<Duration>,

    /// Delay in seconds before showing the notification.
    pub delay: u8,

    /// Notification kind (toast, dialogue, or bell). Defaults to bell in im.
    #[serde(default)]
    pub kind: notify::NotificationKind,

    /// Auto-dismiss timeout for toast and dialogue notifications (None = indefinite).
    #[serde(
        default,
        deserialize_with = "deserialize_opt_duration",
        serialize_with = "serialize_opt_duration"
    )]
    pub dismiss: Option<Duration>,

    /// Prompt text shown after a session when requesting thoughts/notes.
    pub prompt: String,

    /// Countdown display (timer, bar, dots, percent, minimal).
    #[serde(default)]
    pub countdown: crate::ui::CountdownStyle,

    /// Style attributes (fg, bg, modifier) applied to the countdown display.
    #[serde(default)]
    pub countdown_style: matchmaker::config::StyleSetting,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            title: "im".to_string(),
            play_sound: false,
            sound: PathBuf::new(),
            sound_duration: None,
            delay: 0,
            kind: notify::NotificationKind::Bell,
            dismiss: None,
            prompt: "Thoughts:".to_string(),
            countdown: crate::ui::CountdownStyle::Bar,
            countdown_style: matchmaker::config::StyleSetting::default(),
        }
    }
}

fn deserialize_opt_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DurationRepr {
        Secs(u64),
        SecsFloat(f64),
        Str(String),
    }

    let repr = Option::<DurationRepr>::deserialize(deserializer)?;
    match repr {
        None => Ok(None),
        Some(DurationRepr::Secs(s)) => Ok(Some(Duration::from_secs(s))),
        Some(DurationRepr::SecsFloat(s)) => Ok(Some(Duration::from_secs_f64(s))),
        Some(DurationRepr::Str(s)) => {
            let dur = notify::parse_duration(&s).map_err(serde::de::Error::custom)?;
            Ok(Some(dur))
        }
    }
}

fn serialize_opt_duration<S>(dur: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match dur {
        Some(d) => serializer.serialize_str(&humantime::format_duration(*d).to_string()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_config_defaults() {
        let default_cfg = SessionConfig::default();
        assert_eq!(default_cfg.title, "im");
        assert!(!default_cfg.play_sound);
        assert!(default_cfg.sound.as_os_str().is_empty());
        assert!(default_cfg.sound_duration.is_none());
        assert_eq!(default_cfg.delay, 0);
        assert_eq!(default_cfg.kind, notify::NotificationKind::Bell);
        assert!(default_cfg.dismiss.is_none());
        assert_eq!(default_cfg.prompt, "Thoughts:");
        assert_eq!(default_cfg.countdown, crate::ui::CountdownStyle::Bar);
        assert_eq!(
            default_cfg.countdown_style,
            matchmaker::config::StyleSetting::default()
        );
    }

    #[test]
    fn test_session_config_deserialize() {
        let toml_str = r#"
            title = "Focus"
            play_sound = true
            sound = "chime"
            sound_duration = "3s"
            delay = 2
            kind = "dialogue"
            dismiss = "15s"
            prompt = "Notes:"
            countdown = "dots"
            countdown_style = { fg = "cyan", modifier = "BOLD" }
        "#;
        let cfg: SessionConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.title, "Focus");
        assert!(cfg.play_sound);
        assert_eq!(cfg.sound.to_str().unwrap(), "chime");
        assert_eq!(cfg.sound_duration, Some(Duration::from_secs(3)));
        assert_eq!(cfg.delay, 2);
        assert_eq!(cfg.kind, notify::NotificationKind::Dialogue);
        assert_eq!(cfg.dismiss, Some(Duration::from_secs(15)));
        assert_eq!(cfg.prompt, "Notes:");
        assert_eq!(cfg.countdown, crate::ui::CountdownStyle::Dots);
        assert_eq!(
            cfg.countdown_style.fg,
            Some(ratatui::style::Color::Cyan)
        );
        assert!(cfg.countdown_style.modifier.contains(ratatui::style::Modifier::BOLD));

        let cfg: SessionConfig = toml::from_str("").unwrap();
        assert!(!cfg.play_sound);
        assert!(cfg.sound.as_os_str().is_empty());
    }
}
