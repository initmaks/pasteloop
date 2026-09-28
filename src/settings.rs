//! User settings: the struct, defaults, JSON load/save, and the on-disk locations the app uses.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::store::Retention;

pub const APP_NAME: &str = "PasteLoop";

pub const DEFAULT_PANEL_HEIGHT: f32 = 404.0;
pub const MIN_PANEL_HEIGHT: f32 = 180.0;
/// Below this height the panel switches to compact mode.
pub const COMPACT_PANEL_HEIGHT: f32 = 240.0;

pub const DEFAULT_CARD_WIDTH: f32 = 250.0;
pub const MIN_CARD_WIDTH: f32 = 160.0;
pub const MAX_CARD_WIDTH: f32 = 400.0;

pub const DEFAULT_CARD_MAX_HEIGHT: f32 = 290.0;
pub const MIN_CARD_MAX_HEIGHT: f32 = 120.0;
pub const MAX_CARD_MAX_HEIGHT: f32 = 500.0;

/// Text-size multiplier applied across the panel and preview.
pub const DEFAULT_TEXT_SCALE: f32 = 1.0;
pub const MIN_TEXT_SCALE: f32 = 0.85;
pub const MAX_TEXT_SCALE: f32 = 1.4;

/// The panel's non-card vertical space: resize grip, toolbar, shortcut bar, and strip padding (checked
/// against the panel's constants at compile time).
pub const PANEL_CHROME: f32 = 114.0;

/// Apps ignored out of the box. Password managers mark their copies concealed, but not all of
/// them do it reliably.
pub const PASSWORD_MANAGER_BUNDLE_IDS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.callpod.keeper.desktop",
    "com.lastpass.LastPass",
    "com.dashlane.Dashlane",
    "me.proton.pass.electron",
    "in.sinew.Enpass-Desktop",
    "com.apple.Passwords",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PasteMode {
    /// Write to the pasteboard and press Cmd-V in the app that was frontmost.
    #[default]
    ActiveApp,
    /// Only write to the pasteboard and show a toast.
    Clipboard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    #[default]
    Command,
    Option,
    Control,
    Shift,
}

impl Modifier {
    pub const ALL: [Modifier; 4] = [
        Modifier::Command,
        Modifier::Option,
        Modifier::Control,
        Modifier::Shift,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Modifier::Command => "Command",
            Modifier::Option => "Option",
            Modifier::Control => "Control",
            Modifier::Shift => "Shift",
        }
    }

    /// The key's symbol, as macOS menus print it.
    pub fn symbol(self) -> &'static str {
        match self {
            Modifier::Command => "⌘",
            Modifier::Option => "⌥",
            Modifier::Control => "⌃",
            Modifier::Shift => "⇧",
        }
    }

    /// The token GPUI's keymap parser understands.
    pub fn keystroke_token(self) -> &'static str {
        match self {
            Modifier::Command => "cmd",
            Modifier::Option => "alt",
            Modifier::Control => "ctrl",
            Modifier::Shift => "shift",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub activation_hotkey: String,
    #[serde(alias = "stack_hotkey")]
    pub queue_hotkey: String,
    #[serde(alias = "next_pinboard_key")]
    pub next_collection_key: String,
    #[serde(alias = "previous_pinboard_key")]
    pub previous_collection_key: String,
    pub quick_paste_modifier: Modifier,
    pub plain_text_modifier: Modifier,
    pub paste_mode: PasteMode,
    pub retention: Retention,
    /// `None` until the user has answered the launch-at-login question once.
    pub launch_at_login: Option<bool>,
    pub panel_height: f32,
    pub card_width: f32,
    pub card_max_height: f32,
    pub text_scale: f32,
    pub skip_concealed: bool,
    pub skip_transient: bool,
    pub ignored_apps: Vec<String>,
    pub hide_from_screen_sharing: bool,
    pub link_previews_network: bool,
    pub images_as_files: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            activation_hotkey: "shift-cmd-v".into(),
            queue_hotkey: "shift-cmd-c".into(),
            next_collection_key: "cmd-right".into(),
            previous_collection_key: "cmd-left".into(),
            quick_paste_modifier: Modifier::Command,
            plain_text_modifier: Modifier::Shift,
            paste_mode: PasteMode::ActiveApp,
            retention: Retention::Month,
            launch_at_login: None,
            panel_height: DEFAULT_PANEL_HEIGHT,
            card_width: DEFAULT_CARD_WIDTH,
            card_max_height: DEFAULT_CARD_MAX_HEIGHT,
            text_scale: DEFAULT_TEXT_SCALE,
            skip_concealed: true,
            skip_transient: true,
            ignored_apps: default_ignored_apps(),
            hide_from_screen_sharing: true,
            link_previews_network: false,
            images_as_files: false,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!("settings.json unreadable ({err}); using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn clamp_panel_height(height: f32, display_height: f32) -> f32 {
        let max = (display_height * 0.6).max(MIN_PANEL_HEIGHT);
        height.clamp(MIN_PANEL_HEIGHT, max)
    }

    pub fn clamp_card_width(width: f32) -> f32 {
        width.clamp(MIN_CARD_WIDTH, MAX_CARD_WIDTH)
    }

    pub fn clamp_card_max_height(height: f32) -> f32 {
        height.clamp(MIN_CARD_MAX_HEIGHT, MAX_CARD_MAX_HEIGHT)
    }

    pub fn clamp_text_scale(scale: f32) -> f32 {
        if scale.is_finite() {
            scale.clamp(MIN_TEXT_SCALE, MAX_TEXT_SCALE)
        } else {
            DEFAULT_TEXT_SCALE
        }
    }

    /// Whole rows of fixed-height cards that fit a given content height.
    pub fn card_rows(&self, content_height: f32) -> usize {
        let h = self.card_max_height.max(1.0);
        let gap = crate::cards::CARD_GAP;
        (((content_height + gap) / (h + gap)).floor() as usize).max(1)
    }

    /// Rows of cards the bar shows at its saved height.
    pub fn bar_rows(&self) -> usize {
        self.card_rows((self.panel_height - PANEL_CHROME).max(1.0))
    }

    /// The content height that fits exactly `rows` fixed-height cards.
    pub fn content_height_for_rows(&self, rows: usize) -> f32 {
        let rows = rows.max(1) as f32;
        let gap = crate::cards::CARD_GAP;
        self.card_max_height * rows + gap * (rows - 1.0)
    }

    /// Bar (window) height that shows exactly `rows` fixed-height cards.
    pub fn panel_height_for_rows(&self, rows: usize) -> f32 {
        PANEL_CHROME + self.content_height_for_rows(rows)
    }
}

pub fn default_ignored_apps() -> Vec<String> {
    PASSWORD_MANAGER_BUNDLE_IDS
        .iter()
        .map(|id| id.to_string())
        .collect()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `~/Library/Application Support/PasteLoop`; overridable with `PASTELOOP_DATA_DIR` for tests and
/// side-by-side installs.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PASTELOOP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    home().join("Library/Application Support").join(APP_NAME)
}

/// Pre-release builds were called Pasted. Moves their data folder (and database files) to the
/// current name once, when only the old one exists. Returns false while an old build is still
/// running: it holds that folder, and starting on an empty new one would strand its history.
pub fn adopt_legacy_data_dir() -> bool {
    std::env::var_os("PASTELOOP_DATA_DIR").is_some()
        || move_legacy_data_dir(&home().join("Library/Application Support"))
}

fn move_legacy_data_dir(base: &Path) -> bool {
    let (old, new) = (base.join("Pasted"), base.join(APP_NAME));
    if new.exists() || !old.is_dir() {
        return true;
    }
    if let Ok(lock) = fs::File::open(old.join("pasted.lock"))
        && lock.try_lock().is_err()
    {
        tracing::warn!("an old build is running; not moving {}", old.display());
        return false;
    }
    if let Err(err) = fs::rename(&old, &new) {
        tracing::warn!(
            "could not move {} to {}: {err}",
            old.display(),
            new.display()
        );
        return true;
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = new.join(format!("pasted.sqlite{suffix}"));
        if from.exists() {
            let _ = fs::rename(&from, new.join(format!("pasteloop.sqlite{suffix}")));
        }
    }
    let _ = fs::remove_file(new.join("pasted.lock"));
    tracing::info!(
        "moved the data folder from {} to {}",
        old.display(),
        new.display()
    );
    true
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn log_dir() -> PathBuf {
    home().join("Library/Logs").join(APP_NAME)
}

/// `~/Library/Caches/PasteLoop`, used for temp files handed to other apps (images as files).
pub fn cache_dir() -> PathBuf {
    home().join("Library/Caches").join(APP_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_privacy_requirements() {
        let s = Settings::default();
        assert!(s.skip_concealed);
        assert!(s.skip_transient);
        assert_eq!(s.ignored_apps, PASSWORD_MANAGER_BUNDLE_IDS);
        assert!(!s.link_previews_network);
        assert!(s.hide_from_screen_sharing);
        assert_eq!(s.retention, Retention::Month);
        assert_eq!(s.launch_at_login, None);
        assert_eq!(s.activation_hotkey, "shift-cmd-v");
    }

    #[test]
    fn round_trip_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let s = Settings {
            panel_height: 420.0,
            ..Default::default()
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);

        fs::write(&path, r#"{"retention":"forever","paste_mode":"clipboard"}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.retention, Retention::Forever);
        assert_eq!(loaded.paste_mode, PasteMode::Clipboard);
        assert_eq!(loaded.panel_height, DEFAULT_PANEL_HEIGHT);

        fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert_eq!(
            Settings::load(Path::new("/nonexistent/x.json")),
            Settings::default()
        );
    }

    #[test]
    fn panel_height_clamps() {
        assert_eq!(Settings::clamp_panel_height(10.0, 1000.0), MIN_PANEL_HEIGHT);
        assert_eq!(Settings::clamp_panel_height(900.0, 1000.0), 600.0);
        assert_eq!(Settings::clamp_panel_height(300.0, 1000.0), 300.0);
    }

    #[test]
    fn legacy_data_dir_moves_once_with_its_database() {
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("Pasted");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("pasted.sqlite"), "db").unwrap();
        fs::write(old.join("pasted.sqlite-wal"), "wal").unwrap();
        fs::write(old.join("settings.json"), "{}").unwrap();
        move_legacy_data_dir(base.path());
        let new = base.path().join(APP_NAME);
        assert!(!old.exists());
        assert_eq!(
            fs::read_to_string(new.join("pasteloop.sqlite")).unwrap(),
            "db"
        );
        assert_eq!(
            fs::read_to_string(new.join("pasteloop.sqlite-wal")).unwrap(),
            "wal"
        );
        assert!(new.join("settings.json").exists());

        // A second old folder next to an existing new one is left alone.
        fs::create_dir_all(&old).unwrap();
        move_legacy_data_dir(base.path());
        assert!(old.exists());
    }

    #[test]
    fn legacy_data_dir_stays_while_an_old_build_holds_it() {
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("Pasted");
        fs::create_dir_all(&old).unwrap();
        let lock = fs::File::create(old.join("pasted.lock")).unwrap();
        lock.lock().unwrap();
        assert!(!move_legacy_data_dir(base.path()));
        assert!(old.exists());
        assert!(!base.path().join(APP_NAME).exists());
        drop(lock);
        assert!(move_legacy_data_dir(base.path()));
        assert!(!old.exists());
    }

    #[test]
    fn pre_release_settings_keys_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"stack_hotkey":"ctrl-cmd-q","next_pinboard_key":"cmd-]","previous_pinboard_key":"cmd-["}"#,
        )
        .unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.queue_hotkey, "ctrl-cmd-q");
        assert_eq!(s.next_collection_key, "cmd-]");
        assert_eq!(s.previous_collection_key, "cmd-[");
    }

    #[test]
    fn text_scale_clamps() {
        assert_eq!(Settings::default().text_scale, DEFAULT_TEXT_SCALE);
        assert_eq!(Settings::clamp_text_scale(0.1), MIN_TEXT_SCALE);
        assert_eq!(Settings::clamp_text_scale(5.0), MAX_TEXT_SCALE);
        assert_eq!(Settings::clamp_text_scale(1.1), 1.1);
        assert_eq!(Settings::clamp_text_scale(f32::NAN), DEFAULT_TEXT_SCALE);
    }
}
