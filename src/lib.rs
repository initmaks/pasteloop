//! PasteLoop: a native macOS clipboard manager. The library holds every module so benches and
//! the CLI can reach the store; `main.rs` only calls [`run`]. This file also owns app setup:
//! hotkeys, status item wiring, the pasteboard poller, and periodic maintenance.

pub mod backup;
pub mod cards;
pub mod cli;
pub mod clipboard;
pub mod import;
pub mod jobs;
pub mod macos;
pub mod panel;
pub mod paste;
pub mod preview;
pub mod queue;
pub mod settings;
pub mod settings_ui;
pub mod state;
pub mod store;
pub mod text_input;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui::{
    App, AppContext as _, AssetSource, AsyncApp, Bounds, Entity, Global, KeyBinding, SharedString,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px,
    size,
};
use gpui_platform::application;
use tracing::{debug, error, info, warn};

use crate::clipboard::{CaptureRules, Skip};
use crate::macos::{StatusCommand, StatusItem, StatusShortcuts};
use crate::panel::{PanelEvent, PanelView};
use crate::settings::Settings;
use crate::state::{AppState, BackupStatus};
use crate::store::Store;
use gpui::Focusable as _;

/// Embedded SVG icons, served to GPUI's `svg()` element by path.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/text.svg" => Some(include_bytes!("../assets/icons/text.svg")),
            "icons/link.svg" => Some(include_bytes!("../assets/icons/link.svg")),
            "icons/image.svg" => Some(include_bytes!("../assets/icons/image.svg")),
            "icons/file.svg" => Some(include_bytes!("../assets/icons/file.svg")),
            "icons/color.svg" => Some(include_bytes!("../assets/icons/color.svg")),
            "icons/search.svg" => Some(include_bytes!("../assets/icons/search.svg")),
            "icons/filter.svg" => Some(include_bytes!("../assets/icons/filter.svg")),
            "icons/plus.svg" => Some(include_bytes!("../assets/icons/plus.svg")),
            "icons/more.svg" => Some(include_bytes!("../assets/icons/more.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

struct Hotkeys(#[allow(dead_code)] GlobalHotKeyManager);
impl Global for Hotkeys {}

struct StatusItemHolder(StatusItem);
impl Global for StatusItemHolder {}

struct ClickMonitor(#[allow(dead_code)] macos::OutsideClickMonitor);
impl Global for ClickMonitor {}

/// The panel window, created once at startup and kept for the app's lifetime.
struct PanelWindow(WindowHandle<PanelView>);
impl Global for PanelWindow {}

fn panel_window(cx: &App) -> Option<WindowHandle<PanelView>> {
    cx.try_global::<PanelWindow>().map(|p| p.0)
}

#[derive(Clone, Copy, Debug)]
enum HotkeyEvent {
    Panel(Instant),
    Queue,
}

/// Entry point: dispatches CLI subcommands, otherwise runs the menu-bar app.
const OLD_BUILD_RUNNING: &str =
    "An older build (Pasted) is running and holds your history. Quit it, then start PasteLoop.";

pub fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(sub) = args.first() {
        match sub.as_str() {
            "search" | "get" | "recent" | "add" | "export" | "stats" | "ocr" | "seed"
            | "import-paste" | "backup" | "restore" => {
                init_logging(false);
                if !settings::adopt_legacy_data_dir() {
                    eprintln!("{OLD_BUILD_RUNNING}");
                    std::process::exit(1);
                }
                if let Err(err) = cli::run_cli(&args) {
                    eprintln!("error: {err:#}");
                    std::process::exit(1);
                }
                return;
            }
            "--help" | "-h" | "help" => {
                println!("{}", cli::USAGE);
                return;
            }
            "--version" | "-V" => {
                println!("pasteloop {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("unknown command: {other}\n{}", cli::USAGE);
                std::process::exit(2);
            }
        }
    }
    init_logging(true);
    if !settings::adopt_legacy_data_dir() {
        eprintln!("{OLD_BUILD_RUNNING}");
        return;
    }
    if !acquire_single_instance_lock() {
        info!("another PasteLoop instance is already running; exiting");
        eprintln!("PasteLoop is already running.");
        return;
    }
    let launched = Instant::now();
    info!("pasteloop {} starting", env!("CARGO_PKG_VERSION"));
    application().with_assets(Assets).run(move |cx: &mut App| {
        if let Err(err) = start(launched, cx) {
            error!("startup failed: {err:#}");
            cx.quit();
        }
    });
}

/// Holds the single-instance advisory lock for the process lifetime.
static INSTANCE_LOCK: OnceLock<std::fs::File> = OnceLock::new();

/// Keeps a second GUI instance from starting: both would register the same hotkey and fight for
/// key focus, so an injected paste could land in the other panel. Takes an exclusive lock on
/// `pasteloop.lock` in `data_dir()` (released by the OS at exit), so `PASTELOOP_DATA_DIR` instances do
/// not block the real app. Returns false when another instance holds it.
fn acquire_single_instance_lock() -> bool {
    let dir = settings::data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return true; // cannot place the lock; do not block startup over it
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("pasteloop.lock"))
    {
        Ok(f) => f,
        Err(_) => return true,
    };
    if file.try_lock().is_err() {
        return false;
    }
    let _ = INSTANCE_LOCK.set(file);
    true
}

fn init_logging(to_file: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("PASTELOOP_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,pasteloop=debug"));
    if to_file {
        let dir = settings::log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pasteloop.log");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_target(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn start(launched: Instant, cx: &mut App) -> Result<()> {
    macos::set_accessory_activation_policy();
    let settings_path = settings::settings_path();
    let settings = Settings::load(&settings_path);
    let store = Arc::new(Store::open(&settings::data_dir()).context("open store")?);
    info!("store at {}", store.dir().display());

    // Status item first: it is the launch-time budget.
    let (status_tx, status_rx) = unbounded::<StatusCommand>();
    let status_item = StatusItem::install(
        status_tx,
        &StatusShortcuts {
            open: settings.activation_hotkey.clone(),
            queue: settings.queue_hotkey.clone(),
        },
    );
    info!(
        "status item visible after {:.1} ms",
        launched.elapsed().as_secs_f64() * 1000.0
    );
    cx.set_global(StatusItemHolder(status_item));

    cx.bind_keys(text_input::key_bindings());
    cx.bind_keys(queue::key_bindings());
    cx.bind_keys(panel_key_bindings(&settings));

    let trusted_at_launch = macos::accessibility_trusted();
    let state = cx.new(|_| AppState::new(Arc::clone(&store), settings.clone()));
    state.update(cx, |s, _| {
        s.accessibility_at_launch = trusted_at_launch;
        s.sweep();
    });
    macos::log_environment();

    let hide_from_capture = settings.hide_from_screen_sharing;
    let panel = cx
        .open_window(
            WindowOptions {
                kind: WindowKind::PopUp,
                titlebar: None,
                focus: false,
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Blurred,
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(1200.), px(settings.panel_height)),
                })),
                ..Default::default()
            },
            {
                let state = state.clone();
                move |window, cx| {
                    let native =
                        macos::NativeWindow::of(window).expect("panel has a native window");
                    native.configure_panel(hide_from_capture);
                    cx.new(|cx| PanelView::new(state, native, window, cx))
                }
            },
        )
        .context("open panel window")?;
    cx.set_global(PanelWindow(panel));
    info!(
        "panel window created after {:.1} ms",
        launched.elapsed().as_secs_f64() * 1000.0
    );

    // Panel events that need app-level handling.
    let panel_view = panel.update(cx, |_, _, cx| cx.entity())?;
    cx.subscribe(&panel_view, |_, event: &PanelEvent, cx| match event {
        PanelEvent::OpenSettings => open_settings_window(cx),
        PanelEvent::Quit => quit_after_backup(cx),
        PanelEvent::PauseToggled => refresh_status_paused(cx),
    })
    .detach();

    register_hotkeys(&settings, panel, state.clone(), cx)?;
    spawn_status_commands(status_rx, panel, state.clone(), cx);
    let link_tx = spawn_link_metadata_receiver(state.clone(), cx);
    spawn_poller(state.clone(), link_tx, cx);
    spawn_maintenance(state.clone(), panel, cx);
    spawn_backup(state.clone(), panel, cx);
    spawn_file_preview_fill(state.clone(), cx);
    watch_outside_clicks(panel, cx);
    info!(
        "ready after {:.1} ms",
        launched.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn refresh_status_paused(cx: &mut App) {
    let paused = panel_window(cx)
        .and_then(|w| {
            w.update(cx, |view, _, cx| view.state().read(cx).is_paused())
                .ok()
        })
        .unwrap_or(false);
    if let Some(holder) = cx.try_global::<StatusItemHolder>() {
        holder.0.set_paused(paused);
    }
}

/// The panel keymap, with the configurable modifiers and collection keys taken from settings.
fn panel_key_bindings(settings: &Settings) -> Vec<KeyBinding> {
    use panel::*;
    let c = Some(panel::KEY_CONTEXT);
    let quick = settings.quick_paste_modifier.keystroke_token();
    let alt = settings.alternate_modifier.keystroke_token();
    let mut bindings = vec![
        KeyBinding::new("escape", HidePanel, c),
        KeyBinding::new("right", NextCard, c),
        KeyBinding::new("left", PrevCard, c),
        KeyBinding::new("cmd-up", FirstCard, c),
        KeyBinding::new("up", FirstCard, c),
        KeyBinding::new("home", FirstCard, c),
        KeyBinding::new("cmd-down", LastCard, c),
        KeyBinding::new("shift-right", ExtendRight, c),
        KeyBinding::new("shift-left", ExtendLeft, c),
        KeyBinding::new("cmd-a", SelectAllCards, c),
        KeyBinding::new("enter", PasteSelected, c),
        KeyBinding::new(&format!("{alt}-enter"), PasteAlternate, c),
        KeyBinding::new("cmd-c", CopyOnly, c),
        KeyBinding::new("space", QuickLook, c),
        KeyBinding::new("cmd-o", OpenItem, c),
        KeyBinding::new("cmd-r", RenameItem, c),
        KeyBinding::new("cmd-e", EditItem, c),
        KeyBinding::new("cmd-n", NewTextItem, c),
        KeyBinding::new("delete", DeleteItems, c),
        KeyBinding::new("backspace", DeleteItems, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-f", FocusSearch, c),
        KeyBinding::new("tab", ToggleFocus, c),
        KeyBinding::new("cmd-g", JumpHome, c),
        KeyBinding::new("shift-cmd-n", NewCollection, c),
        KeyBinding::new(&settings.next_collection_key, NextCollection, c),
        KeyBinding::new(&settings.previous_collection_key, PrevCollection, c),
        KeyBinding::new("cmd-t", PauseCapture, c),
        KeyBinding::new("cmd-,", OpenSettings, c),
        KeyBinding::new("cmd-q", Quit, c),
    ];
    macro_rules! quick_paste_bindings {
        ($($n:literal => $default:ident, $alternate:ident),* $(,)?) => {
            $(
                bindings.push(KeyBinding::new(&format!("{quick}-{}", $n), $default, c));
                if settings.quick_paste_modifier != settings.alternate_modifier {
                    bindings.push(KeyBinding::new(&format!("{alt}-{quick}-{}", $n), $alternate, c));
                }
            )*
        };
    }
    quick_paste_bindings!(
        1 => QuickPaste1, QuickPasteAlt1,
        2 => QuickPaste2, QuickPasteAlt2,
        3 => QuickPaste3, QuickPasteAlt3,
        4 => QuickPaste4, QuickPasteAlt4,
        5 => QuickPaste5, QuickPasteAlt5,
        6 => QuickPaste6, QuickPasteAlt6,
        7 => QuickPaste7, QuickPasteAlt7,
        8 => QuickPaste8, QuickPasteAlt8,
        9 => QuickPaste9, QuickPasteAlt9,
    );
    bindings
}

/// "shift-cmd-v" (GPUI style) → "shift+cmd+v" (global-hotkey style).
fn parse_hotkey(keystroke: &str) -> Result<HotKey> {
    let normalized = keystroke.trim().replace('-', "+");
    HotKey::try_from(normalized.as_str())
        .map_err(|err| anyhow::anyhow!("hotkey {keystroke:?}: {err}"))
}

fn register_hotkeys(
    settings: &Settings,
    panel: WindowHandle<PanelView>,
    state: Entity<AppState>,
    cx: &mut App,
) -> Result<()> {
    if cx.has_global::<Hotkeys>() {
        cx.remove_global::<Hotkeys>();
    }
    let manager = GlobalHotKeyManager::new().context("hotkey manager")?;
    let panel_hotkey = parse_hotkey(&settings.activation_hotkey)?;
    let queue_hotkey = parse_hotkey(&settings.queue_hotkey)?;
    manager
        .register(panel_hotkey)
        .context("register activation hotkey")?;
    if let Err(err) = manager.register(queue_hotkey) {
        warn!("queue hotkey not registered: {err}");
    }
    let (tx, mut rx) = unbounded::<HotkeyEvent>();
    let panel_id = panel_hotkey.id();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() != HotKeyState::Pressed {
            return;
        }
        let ev = if event.id() == panel_id {
            HotkeyEvent::Panel(Instant::now())
        } else {
            HotkeyEvent::Queue
        };
        let _ = tx.unbounded_send(ev);
    }));
    cx.set_global(Hotkeys(manager));
    cx.spawn(async move |cx| {
        while let Some(event) = rx.next().await {
            match event {
                HotkeyEvent::Panel(t0) => panel::toggle(panel, t0, cx).await,
                HotkeyEvent::Queue => queue::toggle(state.clone(), cx).await,
            }
        }
    })
    .detach();
    info!(
        "hotkeys registered: {} / {}",
        settings.activation_hotkey, settings.queue_hotkey
    );
    Ok(())
}

fn spawn_status_commands(
    mut rx: UnboundedReceiver<StatusCommand>,
    panel: WindowHandle<PanelView>,
    state: Entity<AppState>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        while let Some(command) = rx.next().await {
            debug!("status command: {command:?}");
            match command {
                StatusCommand::TogglePanel => panel::toggle(panel, Instant::now(), cx).await,
                StatusCommand::ToggleQueue => queue::toggle(state.clone(), cx).await,
                StatusCommand::PauseFor(duration) => {
                    cx.update(|cx| {
                        state.update(cx, |s, cx| {
                            s.pause_for(duration);
                            cx.notify();
                        });
                        refresh_status_paused(cx);
                    });
                }
                StatusCommand::Resume => {
                    cx.update(|cx| {
                        state.update(cx, |s, cx| {
                            s.resume();
                            cx.notify();
                        });
                        refresh_status_paused(cx);
                    });
                }
                StatusCommand::OpenSettings => cx.update(open_settings_window),
                StatusCommand::EraseHistory => {
                    panel::show(panel, Instant::now(), cx).await;
                    cx.update(|cx| {
                        panel.update(cx, |view, _, cx| view.confirm_erase(cx)).ok();
                    });
                }
                StatusCommand::About => {
                    macos::open_url("https://github.com/initmaks/pasteloop");
                }
                StatusCommand::Quit => cx.update(quit_after_backup),
                StatusCommand::ScreenParametersChanged => panel::reanchor(panel, cx).await,
            }
        }
    })
    .detach();
}

/// Height of a standard macOS title bar, which sits above a window's content.
const SETTINGS_TITLE_BAR: f64 = 28.0;

/// The settings window: a normal window opened from a task, never synchronously from the
/// popup's handler (see docs/architecture.md, Windows). One instance at a time.
fn open_settings_window(cx: &mut App) {
    if let Some(existing) = cx.try_global::<SettingsWindow>().map(|w| w.0) {
        let alive = existing
            .update(cx, |_, window, _| window.activate_window())
            .is_ok();
        if alive {
            macos::activate_app();
            return;
        }
    }
    cx.spawn(async move |cx| {
        cx.update(|cx| {
            let Some(panel) = panel_window(cx) else {
                return;
            };
            let Ok(state) = panel.update(cx, |view, _, _| view.state().clone()) else {
                return;
            };
            let previous = macos::frontmost_app();
            // Keep the bar visible so appearance changes apply to it in place.
            state.update(cx, |s, cx| {
                s.settings_open = true;
                cx.notify();
            });
            panel.update(cx, |view, _, cx| view.preview_show(cx)).ok();
            // Above the bar, which stays up as a live preview, and squeezed to fit the space.
            let bar_height = state.read(cx).settings.panel_height as f64;
            let place = macos::place_above_bar(bar_height, 760.0, 580.0, SETTINGS_TITLE_BAR);
            let bounds = Bounds {
                origin: point(px(place.x as f32), px(place.y as f32)),
                size: size(px(place.width as f32), px(place.height as f32)),
            };
            let result = cx.open_window(
                WindowOptions {
                    kind: WindowKind::Normal,
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("PasteLoop Settings".into()),
                        ..Default::default()
                    }),
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    display_id: Some(gpui::DisplayId::new(place.display as u64)),
                    is_resizable: true,
                    is_minimizable: true,
                    window_min_size: Some(size(px(640.), px((place.height as f32).min(460.)))),
                    ..Default::default()
                },
                {
                    let state = state.clone();
                    move |window, cx| cx.new(|cx| settings_ui::SettingsView::new(state, window, cx))
                },
            );
            match result {
                Ok(handle) => {
                    cx.set_global(SettingsWindow(handle));
                    if let Ok(view) = handle.update(cx, |view, window, cx| {
                        window.focus(&view.focus_handle(cx), cx);
                        cx.entity()
                    }) {
                        cx.subscribe(&view, on_settings_event).detach();
                    }
                    let window_id = handle.window_id();
                    let closed_state = state.clone();
                    cx.on_window_closed(move |cx, closed| {
                        if closed == window_id {
                            cx.remove_global::<SettingsWindow>();
                            closed_state.update(cx, |s, cx| {
                                s.settings_open = false;
                                cx.notify();
                            });
                            if let Some(panel) = panel_window(cx) {
                                panel.update(cx, |view, _, cx| view.hide(cx)).ok();
                            }
                            if let Some(app) = previous.clone() {
                                macos::activate_pid(app.pid);
                            }
                            info!("settings window closed");
                        }
                    })
                    .detach();
                    macos::activate_app();
                    info!("settings window opened");
                }
                Err(err) => warn!("settings window failed: {err:#}"),
            }
        });
    })
    .detach();
}

struct SettingsWindow(WindowHandle<settings_ui::SettingsView>);
impl Global for SettingsWindow {}

fn on_settings_event(
    _: Entity<settings_ui::SettingsView>,
    event: &settings_ui::SettingsEvent,
    cx: &mut App,
) {
    let Some(panel) = panel_window(cx) else {
        return;
    };
    // The live preview bar follows the Settings window's focus; handle it before anything else.
    if let settings_ui::SettingsEvent::BarPreview(active) = event {
        let active = *active;
        panel
            .update(cx, |view, _, cx| {
                if active {
                    view.preview_show(cx);
                } else {
                    view.preview_hide(cx);
                }
            })
            .ok();
        return;
    }
    let Ok(state) = panel.update(cx, |view, _, _| view.state().clone()) else {
        return;
    };
    let settings = state.read(cx).settings.clone();
    settings_ui::log_change(&settings);
    match event {
        settings_ui::SettingsEvent::Changed => {
            apply_settings(&settings, panel, cx);
            state.update(cx, |s, cx| {
                s.sweep();
                if s.settings.icloud_backup == Some(true) && s.backup.status == BackupStatus::Off {
                    s.backup.requested = true; // just turned on: back up now
                }
                cx.notify();
            });
        }
        settings_ui::SettingsEvent::BackUpNow => {
            state.update(cx, |s, cx| {
                s.backup.requested = true;
                cx.notify();
            });
        }
        settings_ui::SettingsEvent::HotkeysChanged => {
            match register_hotkeys(&settings, panel, state.clone(), cx) {
                Ok(()) => info!("hotkeys re-registered"),
                Err(err) => warn!("hotkey registration failed: {err:#}"),
            }
        }
        settings_ui::SettingsEvent::EraseHistory => {
            state.update(cx, |s, cx| {
                s.erase_history();
                cx.notify();
            });
        }
        // Handled above, before this match.
        settings_ui::SettingsEvent::BarPreview(_) => {}
    }
}

/// Rebinds the configurable keys and applies the screen-sharing setting to every window.
fn apply_settings(settings: &Settings, panel: WindowHandle<PanelView>, cx: &mut App) {
    cx.clear_key_bindings();
    cx.bind_keys(text_input::key_bindings());
    cx.bind_keys(queue::key_bindings());
    cx.bind_keys(panel_key_bindings(settings));
    let hide = settings.hide_from_screen_sharing;
    panel
        .update(cx, |view, _, _| view.native().set_hidden_from_capture(hide))
        .ok();
    if let Some(queue) = cx.try_global::<queue::QueueWindow>().map(|w| w.0) {
        queue
            .update(cx, |view, _, _| view.native().set_hidden_from_capture(hide))
            .ok();
    }
    if let Some(preview) = cx.try_global::<preview::PreviewWindow>().map(|w| w.0) {
        preview
            .update(cx, |view, _, _| view.native().set_hidden_from_capture(hide))
            .ok();
    }
}

/// First check after launch, then every few minutes (see `backup` for when one is due).
const BACKUP_FIRST_CHECK: Duration = Duration::from_secs(20);
const BACKUP_CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// How long quitting waits for a final backup.
const BACKUP_AT_QUIT: Duration = Duration::from_secs(20);

/// What the backup loop's worker found.
enum BackupProbe {
    Unavailable,
    Denied,
    Restore(PathBuf, backup::Manifest),
    Changes(String, i64),
}

/// Backs up to iCloud Drive once changes settle, and restores a backup on a new Mac.
fn spawn_backup(state: Entity<AppState>, panel: WindowHandle<PanelView>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut next_check = Instant::now() + BACKUP_FIRST_CHECK;
        let mut next_upload_poll = Instant::now();
        loop {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let (enabled, requested, store, uploading) = cx.update(|cx| {
                let s = state.read(cx);
                let uploading = (s.backup.uploaded == Some(false))
                    .then(|| s.backup.snapshot.clone())
                    .flatten();
                (
                    s.settings.icloud_backup == Some(true),
                    s.backup.requested,
                    Arc::clone(&s.store),
                    uploading,
                )
            });
            if !enabled {
                set_backup_status(&state, BackupStatus::Off, cx);
                continue;
            }
            let Some(root) = backup::root() else {
                set_backup_status(&state, BackupStatus::Unavailable, cx);
                continue;
            };
            if let Some(snapshot) = uploading
                && Instant::now() >= next_upload_poll
            {
                next_upload_poll = Instant::now() + Duration::from_secs(30);
                let uploaded = macos::icloud_uploaded(&snapshot);
                cx.update(|cx| {
                    state.update(cx, |s, cx| {
                        s.backup.uploaded = uploaded;
                        cx.notify();
                    })
                });
            }
            if !requested && Instant::now() < next_check {
                continue;
            }
            next_check = Instant::now() + BACKUP_CHECK_EVERY;
            check_backup(&state, store, root, panel, cx).await;
        }
    })
    .detach();
}

/// One backup check: restore a backup this new library should take in, else back up when due.
async fn check_backup(
    state: &Entity<AppState>,
    store: Arc<Store>,
    root: PathBuf,
    panel: WindowHandle<PanelView>,
    cx: &mut AsyncApp,
) {
    let probe = {
        let (store, root) = (Arc::clone(&store), root.clone());
        backup::run_worker(move || -> Result<BackupProbe> {
            match backup::access(&root) {
                backup::Access::Ready => {}
                backup::Access::Off => return Ok(BackupProbe::Unavailable),
                backup::Access::Denied => return Ok(BackupProbe::Denied),
            }
            if let Some((folder, manifest)) = backup::restore_candidate(&store, &root)? {
                return Ok(BackupProbe::Restore(folder, manifest));
            }
            let (fingerprint, live) = store.fingerprint()?;
            Ok(BackupProbe::Changes(fingerprint, live))
        })
        .await
    };
    match probe {
        Ok(Ok(BackupProbe::Unavailable)) => set_backup_status(state, BackupStatus::Unavailable, cx),
        Ok(Ok(BackupProbe::Denied)) => {
            warn!("backup: access to iCloud Drive denied");
            let hint = "allow PasteLoop under System Settings → Privacy & Security → Files and \
                        Folders → iCloud Drive";
            set_backup_status(state, BackupStatus::Failed(hint.to_string()), cx)
        }
        Ok(Ok(BackupProbe::Restore(folder, manifest))) => {
            restore_backup(state, store, root, folder, manifest, panel, cx).await
        }
        Ok(Ok(BackupProbe::Changes(fingerprint, live))) => {
            let due = cx.update(|cx| {
                state.update(cx, |s, _| {
                    if matches!(
                        s.backup.status,
                        BackupStatus::Off | BackupStatus::Unavailable
                    ) {
                        s.backup.status = BackupStatus::Idle;
                    }
                    s.backup.observe(fingerprint.clone(), live, Instant::now())
                })
            });
            if due {
                run_backup(state, store, root, fingerprint, cx).await;
            }
        }
        Ok(Err(err)) => {
            warn!("backup check failed: {err:#}");
            set_backup_status(state, BackupStatus::Failed(format!("{err:#}")), cx);
        }
        Err(_) => warn!("backup check worker stopped"),
    }
}

/// Writes a backup and records it; `fingerprint` is the library as of just before.
async fn run_backup(
    state: &Entity<AppState>,
    store: Arc<Store>,
    root: PathBuf,
    fingerprint: String,
    cx: &mut AsyncApp,
) {
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.backup.status = BackupStatus::Running;
            s.backup.requested = false;
            cx.notify();
        })
    });
    let stamp = fingerprint.clone();
    let result = backup::run_worker(move || -> Result<backup::BackupReport> {
        let report = backup::back_up(&store, &settings::settings_path(), &root)?;
        store.set_meta("backup_at", &report.manifest.created_at.to_string())?;
        store.set_meta("backup_items", &report.manifest.items.to_string())?;
        store.set_meta("backup_stamp", &stamp)?;
        Ok(report)
    })
    .await;
    match result {
        Ok(Ok(report)) => {
            info!(
                "backup: {} items, {} KB snapshot, {} blobs copied, {} pruned, in {:.1} s to {}",
                report.manifest.items,
                report.snapshot_bytes / 1024,
                report.copied_blobs,
                report.pruned_blobs,
                report.elapsed.as_secs_f64(),
                report.folder.display()
            );
            let snapshot = report.folder.join("history.sqlite.lzfse");
            let uploaded = macos::icloud_uploaded(&snapshot);
            cx.update(|cx| {
                state.update(cx, |s, cx| {
                    let b = &mut s.backup;
                    b.status = BackupStatus::Idle;
                    b.last_at = Some(report.manifest.created_at);
                    b.last_items = report.manifest.items;
                    b.backed_up = Some(fingerprint);
                    b.changed_at = None;
                    b.deleted = false;
                    b.snapshot = Some(snapshot);
                    b.uploaded = uploaded;
                    cx.notify();
                })
            });
        }
        Ok(Err(err)) => {
            warn!("backup failed: {err:#}");
            set_backup_status(state, BackupStatus::Failed(format!("{err:#}")), cx);
        }
        Err(_) => warn!("backup worker stopped"),
    }
}

/// Restores a backup into this (new) library, then applies its settings and shows the result.
async fn restore_backup(
    state: &Entity<AppState>,
    store: Arc<Store>,
    root: PathBuf,
    folder: PathBuf,
    manifest: backup::Manifest,
    panel: WindowHandle<PanelView>,
    cx: &mut AsyncApp,
) {
    info!(
        "restoring {} items backed up on {} from {}",
        manifest.items,
        manifest.device,
        folder.display()
    );
    set_backup_status(state, BackupStatus::Restoring, cx);
    let result =
        backup::run_worker(move || backup::restore(&store, &root, &folder, &manifest)).await;
    match result {
        Ok(Ok(report)) => {
            info!(
                "restore: merged {} items and {} collections; {} blob files still downloading",
                report.merged.items, report.merged.collections, report.missing_blobs
            );
            cx.update(|cx| {
                let settings = state.update(cx, |s, cx| {
                    if let Some(mut restored) = report.settings {
                        restored.launch_at_login = s.settings.launch_at_login;
                        restored.icloud_backup = s.settings.icloud_backup;
                        s.settings = restored;
                        s.save_settings();
                    }
                    s.backup.status = BackupStatus::Idle;
                    s.backup.requested = true;
                    s.reload_list();
                    s.refresh_collections();
                    s.refresh_apps();
                    s.show_toast(format!(
                        "Restored {} items from iCloud Drive",
                        report.merged.items
                    ));
                    cx.notify();
                    s.settings.clone()
                });
                apply_settings(&settings, panel, cx);
                if let Err(err) = register_hotkeys(&settings, panel, state.clone(), cx) {
                    warn!("hotkey registration failed after restore: {err:#}");
                }
            });
        }
        Ok(Err(err)) => {
            warn!("restore failed: {err:#}");
            set_backup_status(state, BackupStatus::Failed(format!("{err:#}")), cx);
        }
        Err(_) => warn!("restore worker stopped"),
    }
}

fn set_backup_status(state: &Entity<AppState>, status: BackupStatus, cx: &mut AsyncApp) {
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            if s.backup.status != status {
                s.backup.status = status;
                cx.notify();
            }
        })
    });
}

/// Quits, first backing up changes that are not in iCloud Drive yet (waiting at most
/// `BACKUP_AT_QUIT`).
fn quit_after_backup(cx: &mut App) {
    let state = panel_window(cx).and_then(|p| p.update(cx, |v, _, _| v.state().clone()).ok());
    let Some(state) = state else {
        cx.quit();
        return;
    };
    let (enabled, store, backed_up) = {
        let s = state.read(cx);
        (
            s.settings.icloud_backup == Some(true),
            Arc::clone(&s.store),
            s.backup.backed_up.clone(),
        )
    };
    let root =
        backup::root().filter(|root| enabled && backup::access(root) == backup::Access::Ready);
    let Some(root) = root else {
        cx.quit();
        return;
    };
    let job = backup::run_worker(move || -> Result<bool> {
        let (fingerprint, _) = store.fingerprint()?;
        if backed_up.as_deref() == Some(fingerprint.as_str()) {
            return Ok(false);
        }
        let report = backup::back_up(&store, &settings::settings_path(), &root)?;
        store.set_meta("backup_at", &report.manifest.created_at.to_string())?;
        store.set_meta("backup_items", &report.manifest.items.to_string())?;
        store.set_meta("backup_stamp", &fingerprint)?;
        Ok(true)
    });
    cx.spawn(async move |cx| {
        let timeout = cx.background_executor().timer(BACKUP_AT_QUIT);
        match futures::future::select(job, timeout).await {
            futures::future::Either::Left((Ok(Ok(true)), _)) => info!("backed up before quitting"),
            futures::future::Either::Left((Ok(Ok(false)), _)) => {}
            futures::future::Either::Left((Ok(Err(err)), _)) => {
                warn!("backup before quitting failed: {err:#}")
            }
            futures::future::Either::Left((Err(_), _)) => warn!("backup worker stopped"),
            futures::future::Either::Right(_) => warn!("backup before quitting timed out"),
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}

/// Hides the panel on any click outside our windows (see `OutsideClickMonitor`).
fn watch_outside_clicks(panel: WindowHandle<PanelView>, cx: &mut App) {
    let (tx, mut rx) = unbounded::<()>();
    let Some(monitor) = macos::OutsideClickMonitor::start(tx) else {
        warn!("outside-click monitor unavailable; relying on key-status changes");
        return;
    };
    cx.set_global(ClickMonitor(monitor));
    cx.spawn(async move |cx| {
        while rx.next().await.is_some() {
            cx.update(|cx| {
                panel
                    .update(cx, |view, _, cx| {
                        view.dismiss_on_click_away("click outside the panel", cx)
                    })
                    .ok();
            });
        }
    })
    .detach();
}

/// Receives LinkPresentation results (delivered from a background queue) and stores them.
fn spawn_link_metadata_receiver(
    state: Entity<AppState>,
    cx: &mut App,
) -> futures::channel::mpsc::UnboundedSender<(i64, macos::LinkMetadata)> {
    let (tx, mut rx) = unbounded::<(i64, macos::LinkMetadata)>();
    cx.spawn(async move |cx| {
        while let Some((id, metadata)) = rx.next().await {
            let store = cx.update(|cx| Arc::clone(&state.read(cx).store));
            let has_data = metadata.title.is_some() || metadata.image_png.is_some();
            if !has_data {
                continue;
            }
            let task = cx.background_executor().spawn(async move {
                store.set_link_metadata(
                    id,
                    metadata.title.as_deref(),
                    metadata.image_png.as_deref(),
                )
            });
            if let Err(err) = task.await {
                warn!("link metadata store failed: {err:#}");
                continue;
            }
            info!("link metadata stored for item {id}");
            // The new preview image has its own thumbnail key, so the panel fetches it.
            cx.update(|cx| {
                state.update(cx, |s, cx| {
                    s.reload_list();
                    cx.notify();
                });
            });
        }
    })
    .detach();
    tx
}

/// Runs the after-insert jobs for a captured item: OCR for images, link metadata when the
/// network setting allows it. Never blocks capture.
fn spawn_item_jobs(
    item: crate::store::Item,
    state: &Entity<AppState>,
    link_tx: &futures::channel::mpsc::UnboundedSender<(i64, macos::LinkMetadata)>,
    cx: &mut gpui::AsyncApp,
) {
    let (store, network) = cx.update(|cx| {
        let s = state.read(cx);
        (Arc::clone(&s.store), s.settings.link_previews_network)
    });
    match item.kind {
        // Images, and files that came with a picture: recognize their text for search.
        crate::store::Kind::Image | crate::store::Kind::File if item.image_blob.is_some() => {
            let state = state.clone();
            let task = cx.background_executor().spawn(async move {
                match jobs::ocr_item(&store, &item) {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(err) => {
                        warn!("ocr failed for item {}: {err:#}", item.id);
                        false
                    }
                }
            });
            cx.spawn(async move |cx| {
                if task.await {
                    cx.update(|cx| {
                        state.update(cx, |s, cx| {
                            s.reload_list();
                            cx.notify();
                        });
                    });
                }
            })
            .detach();
        }
        crate::store::Kind::Link if network => {
            if let Some(url) = &item.text {
                macos::fetch_link_metadata(item.id, url, 10.0, link_tx.clone());
            }
        }
        _ => {}
    }
}

/// Once per launch, in the background: pictures for file cards whose image file is still on
/// disk, and recognized text for file pictures that have none yet (a re-run Paste import adds
/// pictures without it).
fn spawn_file_preview_fill(state: Entity<AppState>, cx: &mut App) {
    let store = Arc::clone(&state.read(cx).store);
    cx.spawn(async move |cx| {
        let filled = backup::run_worker(move || jobs::fill_file_previews(&store)).await;
        match filled {
            Ok(Ok((0, 0))) => {}
            Ok(Ok((previews, recognized))) => {
                info!("file previews: {previews} added, text recognized in {recognized}");
                cx.update(|cx| {
                    state.update(cx, |s, cx| {
                        s.reload_list();
                        cx.notify();
                    })
                });
            }
            Ok(Err(err)) => warn!("file previews failed: {err:#}"),
            Err(_) => warn!("file preview worker stopped"),
        }
    })
    .detach();
}

/// How long a copy may show an empty type list before it is given up on. A copy made on an
/// iPhone (Universal Clipboard) appears here first and its types arrive once macOS has fetched
/// them from the phone, without another change of the change count.
const TYPES_WAIT: Duration = Duration::from_secs(5);

/// Polls the general pasteboard every 100 ms. The type list is read in the background (for a
/// copy from another device it is fetched over the network); the skip rules run on the main
/// thread against the change count and types; reading the data, classifying, and storing run
/// in the background.
fn spawn_poller(
    state: Entity<AppState>,
    link_tx: futures::channel::mpsc::UnboundedSender<(i64, macos::LinkMetadata)>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let mut last = macos::pasteboard_change_count();
        // A change whose types have not arrived yet, and since when.
        let mut waiting: Option<(i64, Instant)> = None;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let now_count = macos::pasteboard_change_count();
            let retry = waiting.is_some_and(|(c, _)| c == now_count);
            if now_count == last && !retry {
                continue;
            }
            let started = Instant::now();
            let (count, types) = cx
                .background_executor()
                .spawn(async { macos::pasteboard_types() })
                .await;
            last = count;
            if types.is_empty() {
                let since = match waiting {
                    Some((c, since)) if c == count => since,
                    _ => {
                        debug!("pasteboard changed with no types yet; waiting for them");
                        Instant::now()
                    }
                };
                waiting = (since.elapsed() < TYPES_WAIT).then_some((count, since));
                if waiting.is_none() {
                    debug!("skipped: no types after {TYPES_WAIT:?}");
                }
                continue;
            }
            if waiting.take().is_some() {
                info!(
                    "pasteboard types arrived after the change ({} types)",
                    types.len()
                );
            }
            let front = macos::frontmost_app();
            let bundle = front.as_ref().and_then(|a| a.bundle_id.clone());
            let (skip, store) = cx.update(|cx| {
                let s = state.read(cx);
                let rules = CaptureRules {
                    paused: s.is_paused(),
                    skip_concealed: s.settings.skip_concealed,
                    skip_transient: s.settings.skip_transient,
                    ignored_bundle_ids: &s.settings.ignored_apps,
                    own_change_count: s.own_change_count,
                };
                (
                    clipboard::check_skip(count, &types, bundle.as_deref(), &rules),
                    Arc::clone(&s.store),
                )
            });
            match skip {
                // Our own paste: touch_used already moved the item to the front.
                Some(Skip::OwnWrite) => continue,
                Some(other) => {
                    debug!("skipped: {other:?}");
                    continue;
                }
                None => {}
            }
            let source = front.clone();
            let task = cx.background_executor().spawn(async move {
                let snapshot = macos::read_pasteboard(count, types).ok_or(Skip::Superseded)?;
                let new = clipboard::build_item(snapshot, source.as_ref())?;
                Ok::<_, Skip>(store.insert(&new))
            });
            let (item, was_new) = match task.await {
                Ok(Ok(inserted)) => inserted,
                Ok(Err(err)) => {
                    warn!("insert failed: {err:#}");
                    continue;
                }
                Err(skip) => {
                    debug!("skipped: {skip:?}");
                    continue;
                }
            };
            info!(
                "captured {:?} ({} bytes) from {:?} in {:.1} ms (new={})",
                item.kind,
                item.size_bytes,
                item.source_app_name,
                started.elapsed().as_secs_f64() * 1000.0,
                was_new
            );
            let queue_active = cx.update(|cx| {
                state.update(cx, |s, cx| {
                    s.on_item_captured(&item, was_new);
                    cx.notify();
                    s.queue.active
                })
            });
            if queue_active {
                queue::on_capture(item.clone(), state.clone(), cx).await;
            }
            if was_new {
                spawn_item_jobs(item, &state, &link_tx, cx);
            }
        }
    })
    .detach();
}

/// Hourly retention sweep, once-a-minute clock refresh while visible, timed-pause expiry,
/// external-write detection via `PRAGMA data_version`, and hiding when the target app quits.
fn spawn_maintenance(state: Entity<AppState>, panel: WindowHandle<PanelView>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut last_sweep = Instant::now();
        let mut last_minute = Instant::now();
        let mut last_version = cx.update(|cx| state.read(cx).store.data_version().unwrap_or(0));
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let visible = cx.update(|cx| {
                let (mut s, pause_changed) = state.update(cx, |s, cx| {
                    let changed = s.expire_pause();
                    if changed {
                        cx.notify();
                    }
                    (s.panel_visible, changed)
                });
                if pause_changed {
                    // Outside the state update: the status refresh reads the state entity.
                    refresh_status_paused(cx);
                }
                if let Some(target) = state.read(cx).target_app.clone()
                    && s
                    && !macos::pid_is_running(target.pid)
                {
                    info!("target app quit; hiding panel");
                    panel.update(cx, |view, _, cx| view.hide(cx)).ok();
                    s = false;
                }
                s
            });
            if visible && last_minute.elapsed() >= Duration::from_secs(60) {
                last_minute = Instant::now();
                cx.update(|cx| {
                    panel.update(cx, |_, _, cx| cx.notify()).ok();
                });
            }
            if visible {
                let version =
                    cx.update(|cx| state.read(cx).store.data_version().unwrap_or(last_version));
                if version != last_version {
                    last_version = version;
                    cx.update(|cx| {
                        state.update(cx, |s, cx| {
                            s.refresh_collections();
                            s.reload_list();
                            cx.notify();
                        });
                    });
                }
            }
            if last_sweep.elapsed() >= Duration::from_secs(3600) {
                last_sweep = Instant::now();
                cx.update(|cx| {
                    state.update(cx, |s, cx| {
                        s.sweep();
                        cx.notify();
                    });
                });
            }
        }
    })
    .detach();
}
