use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Corner of the monitor the popup is pinned to.
///
/// The tray icon's own coordinates are deliberately ignored: StatusNotifierItem
/// does not report them, so anchoring to a monitor corner is the only approach
/// that behaves identically on X11 and Wayland.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anchor {
    TopLeft,
    /// Where a tray usually is, so it is the default.
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Jellyfin base URL, e.g. "https://jellyfin.example.org". Trailing slash trimmed on load.
    pub server: Option<String>,
    pub username: Option<String>,

    pub anchor: Anchor,
    pub margin: i32,
    pub width: i32,
    pub height: i32,

    /// Hide the popup when it loses focus. Off by default under X11 tiling WMs
    /// where focus follows mouse would make the popup unusable.
    pub hide_on_focus_loss: bool,

    /// How long auto-hide waits after the blur, in milliseconds.
    ///
    /// Zero means the next main-loop turn, which is as immediate as GTK allows
    /// and the right answer on most setups. A delay only helps where the window
    /// manager or compositor is still moving things around when the blur
    /// arrives; it costs visibility, because whatever the compositor does with a
    /// window that is about to disappear happens in plain sight for that long.
    pub hide_delay_ms: u64,

    /// `_NET_WM_WINDOW_TYPE` for the popup on X11, without the
    /// `_NET_WM_WINDOW_TYPE_` prefix: "utility", "dialog", "dock", "normal".
    ///
    /// Defaults to **utility**, which is what this window actually is by EWMH's
    /// definition - a persistent auxiliary window, not a document window - and
    /// what keeps window manager and compositor rules written for ordinary
    /// windows from applying to it. That is not cosmetic: with AwesomeWM and
    /// picom, being an ordinary `normal` window made the popup flicker every time
    /// it lost focus. Set "normal" to get GTK's own behaviour back.
    ///
    /// GTK4 dropped `set_type_hint` with no replacement, so this is applied by
    /// hand - see `ui::x11`.
    pub x11_window_type: Option<String>,

    /// Tracks pulled per random-play refill.
    pub random_batch: u32,
    pub cache_max_mb: u64,
    pub prefetch_next: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: None,
            username: None,
            anchor: Anchor::default(),
            margin: 8,
            width: 380,
            height: 520,
            hide_on_focus_loss: false,
            hide_delay_ms: 0,
            x11_window_type: Some("utility".to_string()),
            random_batch: 100,
            cache_max_mb: 500,
            prefetch_next: true,
        }
    }
}

impl Config {
    /// Load config.toml, falling back to defaults when the file is absent.
    /// A malformed file is a hard error: silently ignoring it hides typos.
    pub fn load() -> Result<Self> {
        let path = config_dir()?.join("config.toml");
        if !path.exists() {
            tracing::info!(path = %path.display(), "no config file, using defaults");
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;

        if let Some(server) = &mut cfg.server {
            let trimmed = server.trim_end_matches('/').to_string();
            *server = trimmed;
        }

        Ok(cfg)
    }
}

/// An explicit light/dark choice. Absent means follow the desktop's preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorScheme {
    Light,
    Dark,
}

/// What happens when a track, or the whole queue, runs out.
///
/// Lives here rather than in `player` because it is persisted state and
/// `config` cannot depend on `player` - the dependency already runs the other
/// way (the cache and the queue state file both come from here).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Repeat {
    /// Playback stops at the end of the queue.
    #[default]
    Off,
    /// The queue starts over from its first track.
    All,
    /// The current track plays again.
    One,
}

impl Repeat {
    /// Next state for the button, in the order the operator asked for: off →
    /// all → one → off.
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

/// State the UI writes for itself.
///
/// Deliberately not part of config.toml: that file is hand-written, and
/// rewriting it whenever a switch is flipped would drop the user's comments and
/// formatting. This one is machine-owned, so it can be regenerated freely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// None until the user picks one, so a fresh install follows the system.
    pub color_scheme: Option<ColorScheme>,
    /// None means "whatever config.toml says", which is how the older key stays
    /// meaningful for anyone who set it there.
    pub hide_on_focus_loss: Option<bool>,
    /// Stops the no-art panel's pattern from moving. The panel itself stays -
    /// this is about motion, not about the decoration.
    pub reduce_motion: bool,
    /// Ceiling on the track cache, in megabytes. None means take config.toml's
    /// `cache_max_mb`, so a value set there still means something until the
    /// settings page is touched - same arrangement as `hide_on_focus_loss`.
    pub cache_max_mb: Option<u64>,
    /// Survives a restart alongside the queue itself: coming back to a resumed
    /// queue with repeat silently switched off would be a surprise.
    ///
    /// Written by the *player*, not the UI, because MPRIS can change it too
    /// (`LoopStatus`) and the player is where both paths meet.
    pub repeat: Repeat,
}

impl Settings {
    /// Never fails: a missing or corrupt state file must not stop the player
    /// from starting, and the defaults are perfectly usable.
    pub fn load() -> Self {
        let Ok(path) = settings_path() else {
            return Self::default();
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str(&raw) {
            Ok(settings) => settings,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "ignoring unreadable settings");
                Self::default()
            }
        }
    }

    /// Read, change, write.
    ///
    /// Always through this rather than by saving a freshly built value: the file
    /// holds every setting, so writing one field from a struct literal would
    /// silently reset the others.
    pub fn update(change: impl FnOnce(&mut Self)) -> Result<()> {
        let mut settings = Self::load();
        change(&mut settings);
        settings.save()
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serialising settings")?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
    }
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "trayplay")
        .context("cannot determine XDG base directories")
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// Used by the track cache from the audio milestone onwards.
#[allow(dead_code)]
pub fn cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().to_path_buf())
}

/// Machine-written state, kept out of the config directory so a hand-edited
/// config.toml and a generated file never sit side by side. `state_dir` is
/// Linux-only in the directories crate, hence the fallback.
fn state_dir() -> Result<PathBuf> {
    let dirs = project_dirs()?;
    Ok(dirs
        .state_dir()
        .unwrap_or_else(|| dirs.data_local_dir())
        .to_path_buf())
}

fn settings_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("settings.toml"))
}

/// The persisted queue. Its own file rather than a field in `Settings`: it is
/// rewritten on every track change and is orders of magnitude larger than the
/// handful of taste settings, which have no business being reserialised with it.
pub fn queue_state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("queue.json"))
}

/// Path of the optional user theme, watched for changes at runtime.
pub fn user_theme_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("theme.css"))
}
