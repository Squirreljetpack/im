use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

#[cfg(not(target_env = "musl"))]
use std::{fs::File, io::Cursor};

#[cfg(not(target_env = "musl"))]
pub use rodio;
#[cfg(not(target_env = "musl"))]
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

/// Bundled gentle sound bytes.
pub const BUNDLED_CHIME: &[u8] = include_bytes!("../sounds/chime.wav");
pub const BUNDLED_BELL: &[u8] = include_bytes!("../sounds/bell.wav");
pub const BUNDLED_BOWL: &[u8] = include_bytes!("../sounds/bowl.wav");

fn bundled_sound(name_or_path: &Path) -> Option<&'static [u8]> {
    match name_or_path.to_str()?.trim().to_ascii_lowercase().as_str() {
        "chime" => Some(BUNDLED_CHIME),
        "bell" => Some(BUNDLED_BELL),
        "bowl" => Some(BUNDLED_BOWL),
        _ => None,
    }
}

/// True when `name_or_path` names a bundled sound ("chime", "bell", or "bowl").
pub fn is_bundled_sound_name(name_or_path: impl AsRef<Path>) -> bool {
    bundled_sound(name_or_path.as_ref()).is_some()
}

/// Errors produced when dispatching a notification or playing its audio.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to open audio file: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(not(target_env = "musl"))]
    #[error("failed to open audio output device: {0}")]
    AudioDevice(#[from] rodio::DeviceSinkError),

    #[cfg(not(target_env = "musl"))]
    #[error("failed to decode audio stream: {0}")]
    AudioDecode(#[from] rodio::decoder::DecoderError),

    #[cfg(target_env = "musl")]
    #[error("audio playback is not supported on musl targets")]
    AudioUnsupported,

    #[error("failed to display toast notification: {0}")]
    Toast(#[from] notify_rust::error::Error),

    #[error("unknown sound '{0}' (expected 'chime', 'bell', 'bowl', or path to audio file)")]
    UnknownSound(String),

    #[error("{0}")]
    Dialogue(String),
}

/// Audio source for a notification sound.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SoundSource {
    /// Audio file on disk (WAV, MP3, Ogg Vorbis, FLAC, MP4/AAC).
    Path(PathBuf),
    /// Encoded audio bytes in memory.
    Bytes(Cow<'static, [u8]>),
    /// Pre-constructed `rodio::Source` stream.
    #[cfg(not(target_env = "musl"))]
    #[cfg_attr(feature = "serde", serde(skip))]
    Source(Box<dyn Source + Send>),
}

impl fmt::Debug for SoundSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(p) => f.debug_tuple("Path").field(p).finish(),
            Self::Bytes(b) => f.debug_tuple("Bytes").field(&b.len()).finish(),
            #[cfg(not(target_env = "musl"))]
            Self::Source(_) => f.debug_tuple("Source").field(&"<rodio::Source>").finish(),
        }
    }
}

impl From<PathBuf> for SoundSource {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

impl From<&Path> for SoundSource {
    fn from(path: &Path) -> Self {
        Self::Path(path.to_path_buf())
    }
}

impl From<&'static [u8]> for SoundSource {
    fn from(bytes: &'static [u8]) -> Self {
        Self::Bytes(Cow::Borrowed(bytes))
    }
}

impl From<Vec<u8>> for SoundSource {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(Cow::Owned(bytes))
    }
}

/// Audio configuration attached to a [`Notification`].
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Sound {
    pub source: SoundSource,
    /// Playback duration cap. When `None`, plays until the end of the source.
    #[cfg_attr(feature = "serde", serde(default))]
    pub duration: Option<Duration>,
}

impl Sound {
    /// Create a sound from a file path.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self {
            source: SoundSource::Path(path.into()),
            duration: None,
        }
    }

    /// Create a sound from encoded bytes in memory.
    pub fn from_bytes(bytes: impl Into<Cow<'static, [u8]>>) -> Self {
        Self {
            source: SoundSource::Bytes(bytes.into()),
            duration: None,
        }
    }

    /// Resolve sound by bundled name ("chime", "bell", "bowl") or file path.
    pub fn from_name_or_path(name_or_path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = name_or_path.as_ref();
        if let Some(bytes) = bundled_sound(path) {
            return Ok(Self::from_bytes(bytes));
        }
        if path.is_relative() && path.extension().is_none() {
            return Err(Error::UnknownSound(path.display().to_string()));
        }
        if !path.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("audio file not found: {}", path.display()),
            )));
        }
        Ok(Self::from_path(path))
    }

    #[cfg(not(target_env = "musl"))]
    /// Create a sound wrapping any [`rodio::Source`].
    pub fn from_source<S>(source: S) -> Self
    where
        S: Source + Send + 'static,
    {
        Self {
            source: SoundSource::Source(Box::new(source)),
            duration: None,
        }
    }

    /// Cap playback at `duration`.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    #[cfg(not(target_env = "musl"))]
    fn start(self) -> Result<PlayingSound, Error> {
        let mut sink = DeviceSinkBuilder::open_default_sink()?;
        sink.log_on_drop(false);
        let player = Player::connect_new(sink.mixer());

        match (self.source, self.duration) {
            (SoundSource::Path(path), Some(dur)) => {
                let decoder = Decoder::try_from(File::open(path)?)?;
                player.append(decoder.take_duration(dur));
            }
            (SoundSource::Path(path), None) => {
                let decoder = Decoder::try_from(File::open(path)?)?;
                player.append(decoder);
            }
            (SoundSource::Bytes(bytes), Some(dur)) => {
                let decoder = Decoder::try_from(Cursor::new(bytes))?;
                player.append(decoder.take_duration(dur));
            }
            (SoundSource::Bytes(bytes), None) => {
                let decoder = Decoder::try_from(Cursor::new(bytes))?;
                player.append(decoder);
            }
            (SoundSource::Source(source), Some(dur)) => {
                player.append(source.take_duration(dur));
            }
            (SoundSource::Source(source), None) => {
                player.append(source);
            }
        }

        Ok(PlayingSound {
            _sink: sink,
            player,
        })
    }

    #[cfg(target_env = "musl")]
    fn start(self) -> Result<PlayingSound, Error> {
        Err(Error::AudioUnsupported)
    }
}

#[cfg(not(target_env = "musl"))]
struct PlayingSound {
    _sink: MixerDeviceSink,
    player: Player,
}

#[cfg(not(target_env = "musl"))]
impl PlayingSound {
    fn wait(&self) {
        self.player.sleep_until_end();
    }
}

#[cfg(target_env = "musl")]
struct PlayingSound;

#[cfg(target_env = "musl")]
impl PlayingSound {
    fn wait(&self) {}
}

/// Presentation mode for a [`Notification`].
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum NotificationType {
    /// Desktop toast banner.
    #[default]
    Toast,
    /// Modal dialogue window that blocks until dismissed.
    Dialogue,
    /// Terminal bell emitting OSC notification escape sequences.
    Bell,
}

impl std::fmt::Display for NotificationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Toast => write!(f, "toast"),
            Self::Dialogue => write!(f, "dialogue"),
            Self::Bell => write!(f, "bell"),
        }
    }
}

impl NotificationType {
    /// Toast notification.
    pub const fn toast() -> Self {
        Self::Toast
    }

    /// Blocking modal dialogue notification.
    pub const fn dialogue() -> Self {
        Self::Dialogue
    }

    /// Terminal bell emitting OSC notification escape sequences.
    pub const fn bell() -> Self {
        Self::Bell
    }
}

/// Alias for [`NotificationType`].
pub type NotificationKind = NotificationType;

/// Parse duration string (humantime format or floating-point seconds).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    if let Ok(secs) = s.trim().parse::<f64>()
        && secs >= 0.0
        && secs.is_finite()
    {
        return Ok(Duration::from_secs_f64(secs));
    }
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

/// Cross-platform desktop notification.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Notification {
    pub title: String,
    pub message: String,
    #[cfg_attr(feature = "serde", serde(default))]
    pub sound: Option<Sound>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub delay: u8,
    #[cfg_attr(feature = "serde", serde(default))]
    pub dismiss: Option<Duration>,
    #[cfg_attr(feature = "serde", serde(default, rename = "type"))]
    pub kind: NotificationType,
}

impl Notification {
    /// Create a notification with the given `title` and `message`.
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            sound: None,
            delay: 0,
            dismiss: None,
            kind: NotificationType::default(),
        }
    }

    /// Attach a [`Sound`] to play when the notification fires.
    pub fn sound(mut self, sound: Sound) -> Self {
        self.sound = Some(sound);
        self
    }

    /// Set the delay in seconds to wait before showing the notification.
    pub fn delay(mut self, seconds: u8) -> Self {
        self.delay = seconds;
        self
    }

    /// Set the auto-dismiss timeout for the notification.
    pub fn dismiss(mut self, dismiss: Duration) -> Self {
        self.dismiss = Some(dismiss);
        self
    }

    /// Set the optional auto-dismiss timeout for the notification.
    pub fn opt_dismiss(mut self, dismiss: Option<Duration>) -> Self {
        self.dismiss = dismiss;
        self
    }

    /// Set the [`NotificationType`].
    pub fn kind(mut self, kind: NotificationType) -> Self {
        self.kind = kind;
        self
    }

    /// Configure as a toast notification.
    pub fn toast(mut self) -> Self {
        self.kind = NotificationType::Toast;
        self
    }

    /// Configure as a toast that auto-dismisses after `dismiss`.
    pub fn toast_with_dismiss(mut self, dismiss: Duration) -> Self {
        self.kind = NotificationType::Toast;
        self.dismiss = Some(dismiss);
        self
    }

    /// Configure as a modal dialogue.
    pub fn dialogue(mut self) -> Self {
        self.kind = NotificationType::Dialogue;
        self
    }

    /// Configure as a terminal bell emitting OSC escape sequences.
    pub fn bell(mut self) -> Self {
        self.kind = NotificationType::Bell;
        self
    }

    /// Dispatch the notification according to its delay, sound, and presentation mode.
    ///
    /// For [`NotificationType::Toast`], waits for any attached sound to finish playing.
    /// For [`NotificationType::Dialogue`], blocks until the user dismisses the dialogue (or
    /// auto-dismiss timeout expires) and stops any playing sound immediately upon dismissal.
    /// For [`NotificationType::Bell`], emits terminal OSC notification sequences and waits for
    /// any attached sound to finish playing.
    pub fn send(self) -> Result<(), Error> {
        if self.delay > 0 {
            thread::sleep(Duration::from_secs(u64::from(self.delay)));
        }

        let mut audio_err = None;
        let playing = match self.sound {
            Some(sound) => match sound.start() {
                Ok(p) => Some(p),
                Err(e) => {
                    audio_err = Some(e);
                    None
                }
            },
            None => None,
        };

        let visual_res = match self.kind {
            NotificationType::Toast => {
                let res = show_toast(&self.title, &self.message, self.dismiss);
                if let Some(playing) = playing {
                    playing.wait();
                }
                res
            }
            NotificationType::Dialogue => {
                // Dropping `playing` when `show_dialogue` returns stops the sound immediately
                // if the user dismisses the dialogue before the audio ends.
                let _playing = playing;
                show_dialogue(&self.title, &self.message, self.dismiss)
            }
            NotificationType::Bell => {
                let res = emit_osc_bell(&self.title, &self.message);
                if let Some(playing) = playing {
                    playing.wait();
                }
                res
            }
        };

        visual_res?;
        if let Some(e) = audio_err {
            return Err(e);
        }

        Ok(())
    }
}

/// Send a simple toast notification with `title` and `message`.
pub fn notify(title: impl Into<String>, message: impl Into<String>) -> Result<(), Error> {
    Notification::new(title, message).send()
}

/// Shared CLI parser for `notify` binary and `im :tool notify`.
#[derive(clap::Parser, Debug, Clone, PartialEq, Eq)]
#[command(name = "notify", version, about = "Cross-platform desktop notification CLI")]
pub struct NotifyCli {
    /// Notification body text (words are space-joined).
    #[arg(value_name = "MESSAGE")]
    pub message: Vec<String>,

    /// Notification title.
    #[arg(short, long, default_value = "")]
    pub title: String,

    /// Sound to play (-s enables default chime; or specify chime, bell, bowl, or audio file path).
    #[arg(short, long, num_args = 0..=1)]
    pub sound: Option<Option<PathBuf>>,

    /// Maximum audio playback duration (e.g. 5s).
    #[arg(long = "sound-duration", alias = "duration", value_parser = parse_duration)]
    pub sound_duration: Option<Duration>,

    /// Seconds to wait before showing the notification.
    #[arg(short, long, default_value_t = 0)]
    pub delay: u8,

    /// Presentation kind: toast, dialogue, or bell.
    #[arg(short = 'k', long = "kind", value_enum, default_value_t = NotificationType::Toast)]
    pub kind: NotificationType,

    /// Auto-dismiss timeout for toast and dialogue notifications (e.g. 5s).
    #[arg(long, value_parser = parse_duration)]
    pub dismiss: Option<Duration>,
}

impl NotifyCli {
    /// Execute the notification command.
    pub fn run(self) -> Result<(), Error> {
        let sound = match self.sound {
            Some(None) => Some(Sound::from_name_or_path("chime")?),
            Some(Some(path)) => {
                if path.as_os_str().is_empty() {
                    None
                } else {
                    Some(Sound::from_name_or_path(path)?)
                }
            }
            None => None,
        };

        let message = if self.message.is_empty() {
            String::new()
        } else {
            self.message.join(" ")
        };

        let mut notif = Notification::new(self.title, message)
            .kind(self.kind)
            .delay(self.delay)
            .opt_dismiss(self.dismiss);
        if let Some(mut s) = sound {
            if let Some(dur) = self.sound_duration {
                s.duration = Some(dur);
            }
            notif = notif.sound(s);
        }
        notif.send()
    }
}

fn sanitize_osc(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_ascii_control() || *c == ' ')
        .collect()
}

fn emit_osc_bell(title: &str, message: &str) -> Result<(), Error> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let safe_title = sanitize_osc(&title.replace(';', ":"));
    let safe_message = sanitize_osc(message);
    // OSC 777 (rxvt-unicode, kitty, foot, ghostty, wezterm), OSC 9 (iTerm2, Windows Terminal), and BEL (\x07)
    write!(
        out,
        "\x1b]777;notify;{safe_title};{safe_message}\x07\x1b]9;{safe_message}\x07\x07"
    )?;
    out.flush()?;
    Ok(())
}

fn show_toast(title: &str, message: &str, dismiss: Option<Duration>) -> Result<(), Error> {
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(message);
    if let Some(dur) = dismiss {
        n.timeout(notify_rust::Timeout::from(dur));
    }
    n.show()?;
    Ok(())
}

fn wait_with_dismiss(
    mut child: std::process::Child,
    dismiss: Option<Duration>,
    program: &str,
) -> Result<(), Error> {
    if let Some(dur) = dismiss {
        let start = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => return Ok(()),
                Ok(None) => {
                    if start.elapsed() >= dur {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(Error::Dialogue(format!("failed waiting for {program}: {e}")));
                }
            }
        }
    } else {
        child
            .wait()
            .map_err(|e| Error::Dialogue(format!("failed waiting for {program}: {e}")))?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn show_dialogue(title: &str, message: &str, dismiss: Option<Duration>) -> Result<(), Error> {
    let child = Command::new("osascript")
        .arg("-e")
        .arg("on run argv")
        .arg("-e")
        .arg("display dialog (item 2 of argv) with title (item 1 of argv) buttons {\"OK\"} default button \"OK\"")
        .arg("-e")
        .arg("end run")
        .arg("--")
        .arg(title)
        .arg(message)
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::Dialogue(format!("failed to execute osascript: {e}")))?;

    wait_with_dismiss(child, dismiss, "osascript")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn show_dialogue(title: &str, message: &str, dismiss: Option<Duration>) -> Result<(), Error> {
    let title_arg = format!("--title={title}");
    let text_arg = format!("--text={message}");

    let candidates: &[(&str, &[&str])] = &[
        ("zenity", &["--info", &title_arg, &text_arg, "--no-markup"]),
        ("kdialog", &["--title", title, "--msgbox", message]),
        (
            "yad",
            &[
                "--info",
                &title_arg,
                &text_arg,
                "--no-markup",
                "--button=OK:0",
            ],
        ),
        ("gxmessage", &["-center", "-title", title, message]),
        ("xmessage", &["-center", "-title", title, message]),
    ];

    for (bin, args) in candidates {
        match Command::new(bin)
            .args(*args)
            .stdout(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => return wait_with_dismiss(child, dismiss, bin),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Error::Dialogue(format!("failed to execute {bin}: {e}")));
            }
        }
    }

    Err(Error::Dialogue(
        "no supported dialogue program found in PATH (tried zenity, kdialog, yad, gxmessage, xmessage)".to_string(),
    ))
}

#[cfg(target_os = "windows")]
fn show_dialogue(title: &str, message: &str, dismiss: Option<Duration>) -> Result<(), Error> {
    let child = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.MessageBox]::Show($env:NOTIFY_MESSAGE, $env:NOTIFY_TITLE) | Out-Null",
        ])
        .env("NOTIFY_TITLE", title)
        .env("NOTIFY_MESSAGE", message)
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::Dialogue(format!("failed to execute powershell: {e}")))?;

    wait_with_dismiss(child, dismiss, "powershell")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_sounds_resolve() {
        assert!(Sound::from_name_or_path("chime").is_ok());
        assert!(Sound::from_name_or_path("bell").is_ok());
        assert!(Sound::from_name_or_path("bowl").is_ok());
    }

    #[test]
    fn test_sound_from_name_or_path_unknown_relative_no_ext_errors() {
        assert!(matches!(
            Sound::from_name_or_path("nonexistent"),
            Err(Error::UnknownSound(_))
        ));
    }

    #[test]
    fn test_is_bundled_sound_name() {
        assert!(is_bundled_sound_name("chime"));
        assert!(is_bundled_sound_name(" Bell "));
        assert!(!is_bundled_sound_name("sounds/chime.wav"));
        assert!(!is_bundled_sound_name(""));
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn test_notify_cli_kind_parsing() {
        use clap::Parser;

        let cli = NotifyCli::try_parse_from(["notify", "hello"]).unwrap();
        assert_eq!(cli.kind, NotificationKind::Toast);

        let cli = NotifyCli::try_parse_from(["notify", "-k", "bell", "hello"]).unwrap();
        assert_eq!(cli.kind, NotificationKind::Bell);

        let cli = NotifyCli::try_parse_from(["notify", "-k", "dialogue", "hello"]).unwrap();
        assert_eq!(cli.kind, NotificationKind::Dialogue);

        let cli = NotifyCli::try_parse_from(["notify", "--kind", "toast", "hello"]).unwrap();
        assert_eq!(cli.kind, NotificationKind::Toast);
    }
}

