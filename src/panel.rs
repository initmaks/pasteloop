//! The popup panel: toolbar with collection tabs and the search field, the card strip, selection,
//! keyboard actions, context menu, dialogs, toasts, resizing, and the show/hide choreography.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnimationExt as _, App, AsyncApp, ClickEvent, Context, Div, Entity,
    ExternalDragPayload, FileDragPaths, FocusHandle, Focusable, KeyDownEvent,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, Pixels, Point, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Stateful, Window, WindowHandle, actions, anchored, deferred,
    div, point, prelude::*, px, svg,
};
use tracing::{debug, info, warn};

use crate::cards::{
    CARD_GAP, COLLECTION_PALETTE, CardImageCache, CardProps, MONO, STRIP_PADDING, Theme,
    card_strip, collection_color, render_card,
};
use crate::jobs;
use crate::macos::{self, NativeWindow};
use crate::paste::{self, Outcome};
use crate::settings::{COMPACT_PANEL_HEIGHT, Settings};
use crate::state::{AppState, Chip, DateRange, Tab};
use crate::store::{Item, Kind, Store, now_ms};
use crate::text_input::{InputEvent, TextInput};

actions!(
    panel,
    [
        HidePanel,
        NextCard,
        PrevCard,
        FirstCard,
        LastCard,
        ExtendRight,
        ExtendLeft,
        SelectAllCards,
        PasteSelected,
        PasteAlternate,
        CopyOnly,
        QuickLook,
        OpenItem,
        RenameItem,
        EditItem,
        NewTextItem,
        DeleteItems,
        Undo,
        FocusSearch,
        ToggleFocus,
        JumpHome,
        NewCollection,
        NextCollection,
        PrevCollection,
        PauseCapture,
        OpenSettings,
        Quit,
        QuickPaste1,
        QuickPaste2,
        QuickPaste3,
        QuickPaste4,
        QuickPaste5,
        QuickPaste6,
        QuickPaste7,
        QuickPaste8,
        QuickPaste9,
        QuickPasteAlt1,
        QuickPasteAlt2,
        QuickPasteAlt3,
        QuickPasteAlt4,
        QuickPasteAlt5,
        QuickPasteAlt6,
        QuickPasteAlt7,
        QuickPasteAlt8,
        QuickPasteAlt9,
    ]
);

pub const KEY_CONTEXT: &str = "Panel";
const TOOLBAR_HEIGHT: f32 = 44.0;
const TOOLBAR_HEIGHT_COMPACT: f32 = 36.0;
const SEARCH_WIDTH: f32 = 260.0;
const RESIZE_GRIP: f32 = 10.0;
/// The shortcut hints along the bottom (not shown in compact mode).
const SHORTCUT_BAR: f32 = 24.0;
/// Strip padding above and below the cards in compact mode.
const STRIP_PADDING_COMPACT: f32 = 8.0;

fn toolbar_height(compact: bool) -> f32 {
    if compact {
        TOOLBAR_HEIGHT_COMPACT
    } else {
        TOOLBAR_HEIGHT
    }
}

fn strip_padding_y(compact: bool) -> f32 {
    if compact {
        STRIP_PADDING_COMPACT
    } else {
        STRIP_PADDING
    }
}
const _: () = assert!(
    RESIZE_GRIP + TOOLBAR_HEIGHT + SHORTCUT_BAR + 2.0 * STRIP_PADDING
        == crate::settings::PANEL_CHROME,
    "settings::PANEL_CHROME must match the panel's grip, toolbar, shortcut bar, and padding"
);

/// What the panel asks the app to do outside its own window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelEvent {
    OpenSettings,
    Quit,
    PauseToggled,
}

impl gpui::EventEmitter<PanelEvent> for PanelView {}

#[derive(Clone, Debug)]
enum MenuEntry {
    Item {
        label: SharedString,
        action: MenuAction,
        enabled: bool,
    },
    /// Opens `entries` beside the menu on hover or click; the menu stays open.
    Submenu {
        label: SharedString,
        entries: Vec<MenuEntry>,
    },
    Separator,
}

#[derive(Clone, Debug, PartialEq)]
enum MenuAction {
    Paste,
    PasteAlternate,
    Copy,
    AddTo(i64),
    RemoveFrom(i64),
    Rename,
    Edit,
    NewText,
    ExtractText,
    Open,
    ShowInHome(Tab),
    Delete,
    RenameCollection(i64),
    RecolorCollection(i64, String),
    DeleteCollection(i64),
    NewCollection,
    EraseHistory,
    PausePanel,
    Settings,
    Quit,
}

struct ContextMenu {
    position: Point<Pixels>,
    entries: Vec<MenuEntry>,
    /// Card index the menu was opened on, if any.
    item_ix: Option<usize>,
    /// Index in `entries` of the submenu shown beside the menu.
    open_submenu: Option<usize>,
}

/// A paste waiting on a permission dialog; "Copy Only" or "Not Now" completes it.
struct PendingPaste {
    items: Vec<Item>,
    alternate: bool,
}

enum Dialog {
    LaunchAtLogin,
    Backup,
    ConfirmErase { count: i64 },
    ConfirmDeleteCollection { id: i64, name: String },
    Accessibility(PendingPaste),
    Relaunch(Option<PendingPaste>),
    CollectionForm { id: Option<i64>, color: String },
}

struct ResizeDrag {
    start_mouse_y: f64,
    start_height: f32,
}

struct Editor {
    /// `None` for a new item.
    item_id: Option<i64>,
    rename: bool,
}

/// A card (or the whole selection) being dragged: onto a tab to add it to that collection, onto another card to
/// reorder inside a collection, or out of the window as a native drag.
#[derive(Clone, Debug)]
pub struct CardDrag {
    ids: Vec<i64>,
    from: Tab,
    label: String,
}

#[derive(Clone, Copy, Debug)]
pub struct TabDrag {
    id: i64,
}

struct DragPreview {
    label: SharedString,
    theme: Theme,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(6.))
            .rounded(px(8.))
            .bg(self.theme.menu)
            .border_1()
            .border_color(self.theme.accent)
            .shadow_md()
            .text_size(px(12.))
            .text_color(self.theme.text)
            .child(self.label.clone())
    }
}

pub struct PanelView {
    state: Entity<AppState>,
    native: NativeWindow,
    window_handle: gpui::AnyWindowHandle,
    focus_handle: FocusHandle,
    search: Entity<TextInput>,
    editor_input: Entity<TextInput>,
    collection_name: Entity<TextInput>,
    scroll: ScrollHandle,
    scroll_to: Option<usize>,
    theme: Theme,
    quick_paste_held: bool,
    context_menu: Option<ContextMenu>,
    dialog: Option<Dialog>,
    resize: Option<ResizeDrag>,
    editor: Option<Editor>,
    filter_popover: bool,
    /// Window position the filter popover anchors to (the filter button's click point).
    filter_anchor: gpui::Point<Pixels>,
    /// Bumped on every show so the entrance animation restarts.
    show_generation: usize,
    /// Bumped on tab changes; the sign of `tab_direction` picks the slide direction.
    tab_generation: usize,
    tab_direction: i8,
    height: f32,
    shown_at: Option<Instant>,
    now_ms: i64,
    /// Thumbnail keys (see `Item::thumb_key`) whose file is on disk, so a frame never stats.
    /// Keyed by thumbnail rather than item, since a file card's changes when it gets a picture.
    /// Shared with the frame's card closure; copied only when a thumbnail lands.
    thumbs_ready: Rc<HashSet<String>>,
    /// Thumbnails being made, or failed; a failure is not retried this session.
    thumbs_tried: HashSet<String>,
    /// Decoded card images, bounded (see `CardImageCache`).
    image_cache: Entity<CardImageCache>,
    /// The toolbar's position counter.
    odometer: Odometer,
    hovered: Option<usize>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl PanelView {
    pub fn new(
        state: Entity<AppState>,
        native: NativeWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, "Search", false));
        let editor_input = cx.new(|cx| TextInput::new(cx, "Text", true));
        let collection_name = cx.new(|cx| TextInput::new(cx, "Collection name", false));
        let height = state.read(cx).settings.panel_height;
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe(&state, Self::on_state_change));
        subscriptions.push(cx.subscribe(&search, Self::on_search_event));
        subscriptions.push(cx.subscribe(&editor_input, Self::on_editor_event));
        subscriptions.push(cx.subscribe(&collection_name, Self::on_collection_form_event));
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.dismiss_on_click_away("panel lost key status", cx);
            }
        }));
        subscriptions.push(cx.observe_window_appearance(window, |this, window, cx| {
            this.refresh_theme(window, cx);
            cx.notify();
        }));
        let mut view = PanelView {
            state,
            native,
            window_handle: window.window_handle(),
            focus_handle: cx.focus_handle(),
            search,
            editor_input,
            collection_name,
            scroll: ScrollHandle::new(),
            scroll_to: None,
            theme: Theme::for_window(window),
            quick_paste_held: false,
            context_menu: None,
            dialog: None,
            resize: None,
            editor: None,
            filter_popover: false,
            filter_anchor: point(px(0.), px(TOOLBAR_HEIGHT)),
            show_generation: 0,
            tab_generation: 0,
            tab_direction: 0,
            height,
            shown_at: None,
            now_ms: now_ms(),
            thumbs_ready: Default::default(),
            thumbs_tried: Default::default(),
            image_cache: CardImageCache::new(cx),
            odometer: Odometer::default(),
            hovered: None,
            _subscriptions: subscriptions,
        };
        view.refresh_theme(window, cx);
        view
    }

    fn refresh_theme(&mut self, window: &Window, cx: &mut Context<Self>) {
        let text_scale = self.state.read(cx).settings.text_scale;
        self.theme = Theme::for_window(window).with_text_scale(text_scale);
        let colors = self.theme.input_colors();
        for input in [&self.search, &self.editor_input, &self.collection_name] {
            if input.read(cx).colors != colors {
                input.update(cx, |i, _| i.colors = colors.clone());
            }
        }
    }

    pub fn state(&self) -> &Entity<AppState> {
        &self.state
    }

    pub fn native(&self) -> &NativeWindow {
        &self.native
    }

    // ---- show / hide -------------------------------------------------------------------------

    /// Records the paste target and returns the frame to show at. Called inside a GPUI update;
    /// the caller then orders the window in from its task.
    pub fn prepare_show(
        &mut self,
        t0: Instant,
        cx: &mut Context<Self>,
    ) -> objc2_foundation::NSRect {
        let front = macos::frontmost_app();
        // Pick up a bar height changed in Settings since the last show.
        let height = self.state.read(cx).settings.panel_height;
        self.height = height;
        let (frame, display_height) = macos::panel_frame_under_mouse(height as f64);
        let clamped = Settings::clamp_panel_height(height, display_height as f32);
        let frame = if (clamped - height).abs() > 0.5 {
            self.height = clamped;
            macos::panel_frame_under_mouse(clamped as f64).0
        } else {
            frame
        };
        self.shown_at = Some(t0);
        self.now_ms = now_ms();
        // Everything the first frame depends on is set before the window is ordered in; see
        // `render`, which reveals the window once that frame is drawn.
        self.show_generation += 1;
        self.odometer.snap = true;
        self.context_menu = None;
        self.quick_paste_held = false;
        // Ask once whether to open at login; only meaningful from the .app bundle.
        // The next time, whether to back up to iCloud Drive.
        if self.dialog.is_none() && macos::running_from_bundle() {
            let settings = &self.state.read(cx).settings;
            if settings.launch_at_login.is_none() {
                self.dialog = Some(Dialog::LaunchAtLogin);
            } else if settings.icloud_backup.is_none() && crate::backup::root().is_some() {
                self.dialog = Some(Dialog::Backup);
            }
        }
        let resumed = self.state.update(cx, |s, cx| {
            s.target_app = front;
            s.panel_visible = true;
            s.expire_pause();
            cx.notify();
            s.begin_show()
        });
        if !resumed {
            self.search.update(cx, |s, cx| s.set_text("", cx));
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        self.scroll_to = self.state.read(cx).selection.cursor;
        info!(
            "panel opened {}",
            if resumed {
                "where it was left"
            } else {
                "at the newest card"
            }
        );
        self.offer_relaunch(cx);
        frame
    }

    /// Shows the relaunch sheet once per launch when Accessibility was granted after this
    /// process started (see `paste::decide`). Returns whether it was shown.
    fn offer_relaunch(&mut self, cx: &mut Context<Self>) -> bool {
        let needed = {
            let s = self.state.read(cx);
            s.settings.paste_mode == crate::settings::PasteMode::ActiveApp
                && !s.accessibility_at_launch
                && !s.relaunch_prompted
                && macos::accessibility_trusted()
        };
        if !needed || self.dialog.is_some() {
            return false;
        }
        self.state.update(cx, |s, _| s.relaunch_prompted = true);
        self.dialog = Some(Dialog::Relaunch(None));
        info!("accessibility granted after launch; asking to relaunch");
        cx.notify();
        true
    }

    /// Focuses the results and scrolls the selection into view after the window is up.
    pub fn after_show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_theme(window, cx);
        window.focus(&self.focus_handle, cx);
        self.reveal_fallback(cx);
        cx.notify();
    }

    /// The window is ordered in transparent and `render` reveals it after its first frame. If
    /// macOS withholds that frame (a transparent window can count as occluded), reveal it anyway.
    fn reveal_fallback(&self, cx: &mut Context<Self>) {
        let native = self.native.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            if native.reveal() {
                warn!("panel revealed by the fallback; its first frame did not arrive in time");
            }
        })
        .detach();
    }

    /// Hides the panel after the user went elsewhere (another app, a click outside our windows),
    /// unless a dialog is open or the Settings window is showing the bar for a live preview.
    pub fn dismiss_on_click_away(&mut self, reason: &str, cx: &mut Context<Self>) {
        let s = self.state.read(cx);
        if s.panel_visible && !s.settings_open && self.dialog.is_none() {
            debug!("{reason}; hiding");
            self.hide(cx);
        }
    }

    /// Dismisses the panel (Esc, clicking away). Reopening soon continues the same browse.
    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.close(true, cx);
    }

    /// Hides after an item was used (pasted, copied, opened); the next show starts at the
    /// newest card.
    fn finish(&mut self, cx: &mut Context<Self>) {
        self.close(false, cx);
    }

    /// Hides from inside an update: state first, the window from a task.
    fn close(&mut self, resumable: bool, cx: &mut Context<Self>) {
        if self.state.read(cx).panel_visible {
            info!("panel hidden (resumable={resumable})");
        }
        self.context_menu = None;
        self.filter_popover = false;
        self.dialog = None;
        self.editor = None;
        self.close_preview(cx);
        let was_visible = self.state.read(cx).panel_visible;
        self.state.update(cx, |s, cx| {
            s.panel_visible = false;
            // A panel that was already gone (after a paste) keeps what that ended with.
            if was_visible {
                s.resume_until = resumable.then(|| Instant::now() + crate::state::RESUME_WINDOW);
            }
            cx.notify();
        });
        let native = self.native.clone();
        cx.spawn(async move |_, _| native.order_out_transparent())
            .detach();
        // While hidden the app only polls the pasteboard; give back what the last showing used.
        let store = Arc::clone(&self.state.read(cx).store);
        cx.background_executor()
            .spawn(async move {
                store.release_memory();
                macos::release_free_memory();
            })
            .detach();
    }

    // ---- actions -----------------------------------------------------------------------------

    fn hide_panel(&mut self, _: &HidePanel, _: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.is_some() || self.filter_popover {
            self.context_menu = None;
            self.filter_popover = false;
            cx.notify();
            return;
        }
        if self.dialog.is_some() {
            self.dialog = None;
            cx.notify();
            return;
        }
        if self.state.read(cx).preview.is_some() {
            self.close_preview(cx);
            return;
        }
        if self.editor.is_some() {
            self.editor = None;
            cx.notify();
            return;
        }
        // On the results, Escape hides even while searching: the search is kept for the resume
        // window. Escape in the search field clears it instead (`defer_hide_or_clear`).
        self.hide(cx);
    }

    fn move_by(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let cursor = self.state.update(cx, |s, cx| {
            let c = s.move_cursor(delta, extend);
            cx.notify();
            c
        });
        self.scroll_to = cursor;
        self.sync_preview_to_cursor(cx);
        cx.notify();
    }

    fn next_card(&mut self, _: &NextCard, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, false, cx);
    }

    fn prev_card(&mut self, _: &PrevCard, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, false, cx);
    }

    fn extend_right(&mut self, _: &ExtendRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, true, cx);
    }

    fn extend_left(&mut self, _: &ExtendLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, true, cx);
    }

    fn first_card(&mut self, _: &FirstCard, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.select_first();
            cx.notify();
        });
        self.scroll_to = Some(0);
        self.sync_preview_to_cursor(cx);
        cx.notify();
    }

    fn last_card(&mut self, _: &LastCard, _: &mut Window, cx: &mut Context<Self>) {
        let last = self.state.update(cx, |s, cx| {
            s.load_until(|_| false);
            s.select_last();
            cx.notify();
            s.selection.cursor
        });
        self.scroll_to = last;
        cx.notify();
    }

    fn select_all_cards(&mut self, _: &SelectAllCards, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.select_all();
            cx.notify();
        });
    }

    fn paste_selected(&mut self, _: &PasteSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return self.accept_dialog(cx);
        }
        let items = self.state.read(cx).selected_items();
        self.paste_items(items, false, cx);
    }

    fn paste_alternate(&mut self, _: &PasteAlternate, _: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let items = self.state.read(cx).selected_items();
        self.paste_items(items, true, cx);
    }

    fn quick_paste(&mut self, n: usize, alternate: bool, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        let item = self.state.read(cx).items.get(n.saturating_sub(1)).cloned();
        if let Some(item) = item {
            self.paste_items(vec![item], alternate, cx);
        }
    }

    /// What the open dialog's highlighted button does; Return does the same while it is up.
    fn accept_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::LaunchAtLogin => {
                match macos::set_launch_at_login(true) {
                    Ok(()) => info!("launch at login enabled"),
                    Err(err) => warn!("launch at login failed: {err}"),
                }
                self.state.update(cx, |s, cx| {
                    s.settings.launch_at_login = Some(true);
                    s.save_settings();
                    s.show_toast(match macos::login_item_status() {
                        macos::LoginItemStatus::RequiresApproval => {
                            "Approve PasteLoop in System Settings → Login Items"
                        }
                        _ => "PasteLoop will open at login",
                    });
                    cx.notify();
                });
                self.expire_toast_later(cx);
            }
            Dialog::Backup => {
                info!("iCloud Drive backup turned on");
                self.state.update(cx, |s, cx| {
                    s.settings.icloud_backup = Some(true);
                    s.save_settings();
                    s.backup.requested = true;
                    cx.notify();
                });
            }
            Dialog::ConfirmErase { .. } => self.state.update(cx, |s, cx| {
                s.erase_history();
                cx.notify();
            }),
            Dialog::ConfirmDeleteCollection { id, .. } => {
                let store = Arc::clone(&self.state.read(cx).store);
                if let Err(err) = store.delete_collection(id) {
                    warn!("delete collection failed: {err:#}");
                }
                self.state.update(cx, |s, cx| {
                    s.refresh_collections();
                    s.set_tab(Tab::History);
                    cx.notify();
                });
            }
            Dialog::Accessibility(_) => {
                info!("opening Accessibility settings");
                macos::accessibility_trusted_with_prompt();
                macos::open_accessibility_settings();
                self.hide(cx);
            }
            Dialog::Relaunch(_) => match macos::spawn_relauncher() {
                Ok(()) => {
                    info!("relaunching for the Accessibility grant");
                    cx.emit(PanelEvent::Quit);
                }
                Err(err) => {
                    warn!("relaunch helper failed: {err}");
                    self.toast_then_hide("Couldn't relaunch — quit and reopen PasteLoop", cx);
                }
            },
            // Its name field submits on Return itself.
            form @ Dialog::CollectionForm { .. } => self.dialog = Some(form),
        }
        cx.notify();
    }

    /// The paste flow: plan on the main thread (reads blobs), then run the write/inject/hide
    /// sequence as a task so window ordering happens outside the borrow.
    fn paste_items(&mut self, items: Vec<Item>, alternate: bool, cx: &mut Context<Self>) {
        if items.is_empty() {
            return;
        }
        // While pasting into apps is not possible yet, every attempt explains why (until the
        // user turns that off), instead of silently copying and hiding.
        let (ask, at_launch) = {
            let s = self.state.read(cx);
            (
                s.settings.paste_mode == crate::settings::PasteMode::ActiveApp
                    && s.settings.ask_for_accessibility,
                s.accessibility_at_launch,
            )
        };
        if ask && self.dialog.is_none() {
            let pending = PendingPaste { items, alternate };
            let trusted = macos::accessibility_trusted();
            if !trusted || !at_launch {
                info!("paste needs Accessibility (trusted={trusted}); asking");
                self.dialog = Some(if trusted {
                    Dialog::Relaunch(Some(pending))
                } else {
                    Dialog::Accessibility(pending)
                });
                cx.notify();
                return;
            }
            return self.paste_now(pending.items, pending.alternate, cx);
        }
        self.paste_now(items, alternate, cx)
    }

    /// Pastes (or copies, when pasting into apps is not possible) without asking first.
    fn paste_now(&mut self, items: Vec<Item>, alternate: bool, cx: &mut Context<Self>) {
        let (store, images_as_files, form) = {
            let s = self.state.read(cx);
            (
                Arc::clone(&s.store),
                s.settings.images_as_files,
                paste::Form::for_paste(s.settings.paste_plain_text, alternate),
            )
        };
        let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
        let plan = paste::plan(&items, &store, form, images_as_files);
        let state = self.state.clone();
        let native = self.native.clone();
        self.context_menu = None;
        cx.spawn(async move |this, cx| {
            let outcome = paste::perform(plan, ids, state.clone(), native, cx).await;
            cx.update(|cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| match outcome {
                        Outcome::Pasted => this.finish(cx),
                        Outcome::CopiedOnly => {
                            this.toast_then_hide("Copied — press ⌘V to paste", cx)
                        }
                        Outcome::NeedsAccessibility => this
                            .toast_then_hide("Copied — grant Accessibility to paste directly", cx),
                        Outcome::NeedsRelaunch => this
                            .toast_then_hide("Copied — relaunch PasteLoop to paste directly", cx),
                    });
                }
            });
        })
        .detach();
    }

    fn toast_then_hide(&mut self, text: &str, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.show_toast(text);
            cx.notify();
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::state::TOAST_DURATION)
                .await;
            cx.update(|cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        this.state.update(cx, |s, _| s.toast = None);
                        this.finish(cx);
                    });
                }
            });
        })
        .detach();
    }

    fn copy_only(&mut self, _: &CopyOnly, _: &mut Window, cx: &mut Context<Self>) {
        let items = self.state.read(cx).selected_items();
        if items.is_empty() {
            return;
        }
        let (store, images_as_files, form) = {
            let s = self.state.read(cx);
            (
                Arc::clone(&s.store),
                s.settings.images_as_files,
                paste::Form::for_paste(s.settings.paste_plain_text, false),
            )
        };
        let plan = paste::plan(&items, &store, form, images_as_files);
        let count = paste::copy_only(plan);
        self.state
            .update(cx, |s, _| s.own_change_count = Some(count));
        info!("copied {} item(s) to the pasteboard", items.len());
        let state = self.state.clone();
        cx.spawn(async move |_, cx| crate::queue::after_own_write(state, cx))
            .detach();
        self.finish(cx);
    }

    /// Space: toggles the preview window, which shows the card under the cursor at full size.
    fn quick_look(&mut self, _: &QuickLook, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.state.read(cx).cursor_item().cloned() else {
            return;
        };
        let showing = self.state.read(cx).preview.as_ref().map(|p| p.id) == Some(item.id);
        if showing {
            self.close_preview(cx);
        } else {
            self.editor = None;
            self.context_menu = None;
            self.set_preview(item, cx);
        }
    }

    /// Shows `item` in the preview window, sized for it.
    fn set_preview(&mut self, item: Item, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.preview = Some(item);
            cx.notify();
        });
        let state = self.state.clone();
        let native = self.native.clone();
        cx.spawn(async move |_, cx| crate::preview::show(state, native, cx).await)
            .detach();
    }

    /// Closes the floating preview window and clears its shared state.
    fn close_preview(&mut self, cx: &mut Context<Self>) {
        if self.state.read(cx).preview.is_some() {
            self.state.update(cx, |s, cx| {
                s.preview = None;
                cx.notify();
            });
            cx.spawn(async move |_, cx| crate::preview::hide(cx).await)
                .detach();
        }
    }

    /// While the preview window is open, follow the selection: preview the current card.
    fn sync_preview_to_cursor(&mut self, cx: &mut Context<Self>) {
        if self.state.read(cx).preview.is_none() {
            return;
        }
        let item = self.state.read(cx).cursor_item().cloned();
        match item {
            Some(item) => self.set_preview(item, cx),
            None => self.close_preview(cx),
        }
    }

    /// OCR on demand: the recognized text becomes a new text item.
    fn extract_text(&mut self, item: Item, cx: &mut Context<Self>) {
        let store = Arc::clone(&self.state.read(cx).store);
        let task = cx.background_executor().spawn(async move {
            let text = jobs::ocr_item(&store, &item)?;
            match text {
                Some(text) => Ok(Some(store.insert(&crate::store::NewItem {
                    source_app_name: Some("PasteLoop OCR".into()),
                    ..crate::store::NewItem::text(text)
                })?)),
                None => Ok::<_, anyhow::Error>(None),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            cx.update(|cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        let message = match result {
                            Ok(Some((item, _))) => {
                                this.state.update(cx, |s, cx| {
                                    s.reload_list();
                                    if let Some(ix) = s.items.iter().position(|i| i.id == item.id) {
                                        s.select(ix);
                                    }
                                    cx.notify();
                                });
                                "Text extracted"
                            }
                            Ok(None) => "No text found in the image",
                            Err(err) => {
                                warn!("extract text failed: {err:#}");
                                "Could not extract text"
                            }
                        };
                        this.state.update(cx, |s, cx| {
                            s.show_toast(message);
                            cx.notify();
                        });
                        this.expire_toast_later(cx);
                    });
                }
            });
        })
        .detach();
    }

    fn open_item(&mut self, _: &OpenItem, _: &mut Window, cx: &mut Context<Self>) {
        let item = self.state.read(cx).cursor_item().cloned();
        let Some(item) = item else { return };
        match item.kind {
            Kind::Link => {
                if let Some(url) = &item.text {
                    macos::open_url(url);
                }
            }
            Kind::File => {
                for path in &item.files {
                    macos::open_path(path);
                }
            }
            _ => return,
        }
        self.finish(cx);
    }

    fn rename_item(&mut self, _: &RenameItem, _: &mut Window, cx: &mut Context<Self>) {
        let item = self.state.read(cx).cursor_item().cloned();
        if let Some(item) = item {
            self.open_editor(Some(item), true, cx);
        }
    }

    fn edit_item(&mut self, _: &EditItem, _: &mut Window, cx: &mut Context<Self>) {
        let item = self.state.read(cx).cursor_item().cloned();
        if let Some(item) = item
            && item.kind == Kind::Text
        {
            self.open_editor(Some(item), false, cx);
        }
    }

    fn new_text_item(&mut self, _: &NewTextItem, _: &mut Window, cx: &mut Context<Self>) {
        self.open_editor(None, false, cx);
    }

    fn open_editor(&mut self, item: Option<Item>, rename: bool, cx: &mut Context<Self>) {
        let store = Arc::clone(&self.state.read(cx).store);
        let text = match (&item, rename) {
            (Some(item), true) => item.title.clone().unwrap_or_default(),
            (Some(item), false) => store.full_text(item).ok().flatten().unwrap_or_default(),
            (None, _) => String::new(),
        };
        let mono = item
            .as_ref()
            .map(|i| {
                i.text
                    .as_deref()
                    .is_some_and(crate::clipboard::looks_like_code)
            })
            .unwrap_or(false)
            && !rename;
        self.editor = Some(Editor {
            item_id: item.as_ref().map(|i| i.id),
            rename,
        });
        self.close_preview(cx);
        self.context_menu = None;
        self.editor_input.update(cx, |input, cx| {
            input.set_monospace(mono, cx);
            input.set_text(text, cx);
        });
        // The editor needs keyboard focus; this handler has no `Window`, so defer one tick.
        self.defer_with_window(cx, |this, window, cx| {
            this.editor_input
                .update(cx, |input, cx| input.focus(window, cx));
        });
        cx.notify();
    }

    fn commit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let text = self.editor_input.read(cx).text().to_string();
        let store = Arc::clone(&self.state.read(cx).store);
        let result = match (editor.item_id, editor.rename) {
            (Some(id), true) => store.rename(id, Some(&text)).map(|_| Some(id)),
            (Some(id), false) => store.update_text(id, &text).map(|i| Some(i.id)),
            (None, _) => {
                if text.trim().is_empty() {
                    Ok(None)
                } else {
                    store
                        .insert(&crate::store::NewItem::text(text))
                        .map(|(i, _)| Some(i.id))
                }
            }
        };
        match result {
            Ok(id) => {
                self.state.update(cx, |s, cx| {
                    s.reload_list();
                    if let Some(id) = id
                        && let Some(ix) = s.items.iter().position(|i| i.id == id)
                    {
                        s.select(ix);
                    }
                    cx.notify();
                });
            }
            Err(err) => warn!("edit failed: {err:#}"),
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn delete_items(&mut self, _: &DeleteItems, _: &mut Window, cx: &mut Context<Self>) {
        let deleted = self.state.update(cx, |s, cx| {
            let ids = s.delete_selected();
            cx.notify();
            ids
        });
        if !deleted.is_empty() {
            self.scroll_to = self.state.read(cx).selection.cursor;
            cx.notify();
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let restored = self.state.update(cx, |s, cx| {
            let n = s.undo_delete();
            cx.notify();
            n
        });
        if restored > 0 {
            self.scroll_to = self.state.read(cx).selection.cursor;
            cx.notify();
        }
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).is_focused(window) {
            self.filter_popover = !self.filter_popover;
        } else {
            self.search.update(cx, |s, cx| {
                s.focus(window, cx);
                s.select_all_text(cx);
            });
        }
        cx.notify();
    }

    fn toggle_focus(&mut self, _: &ToggleFocus, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).is_focused(window) {
            window.focus(&self.focus_handle, cx);
        } else {
            self.search.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    fn jump_home(&mut self, _: &JumpHome, _: &mut Window, cx: &mut Context<Self>) {
        let item = self.state.read(cx).cursor_item().cloned();
        let Some(item) = item else { return };
        self.jump_to_home(&item, None, cx);
    }

    fn jump_to_home(&mut self, item: &Item, tab: Option<Tab>, cx: &mut Context<Self>) {
        let tab = tab.unwrap_or_else(|| {
            let s = self.state.read(cx);
            match s.search.home {
                Some(Tab::Collection(id)) if item.collection_ids.contains(&id) => {
                    Tab::Collection(id)
                }
                _ => Tab::History,
            }
        });
        self.search.update(cx, |s, cx| s.set_text("", cx));
        let id = item.id;
        let ix = self.state.update(cx, |s, cx| {
            s.search.home = None;
            s.clear_search();
            s.set_tab(tab);
            s.load_until(|s| s.items.iter().any(|i| i.id == id));
            let ix = s.items.iter().position(|i| i.id == id);
            if let Some(ix) = ix {
                s.select(ix);
            }
            s.highlight_item = Some((id, Instant::now() + Duration::from_millis(600)));
            cx.notify();
            ix
        });
        self.scroll_to = ix;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(650))
                .await;
            cx.update(|cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        this.state.update(cx, |s, _| s.highlight_item = None);
                        cx.notify();
                    });
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn new_collection(&mut self, _: &NewCollection, window: &mut Window, cx: &mut Context<Self>) {
        self.open_collection_form(None, window, cx);
    }

    fn open_collection_form(
        &mut self,
        id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, color) = match id.and_then(|id| self.state.read(cx).collection(id).cloned()) {
            Some(p) => (p.name, p.color),
            None => (String::new(), COLLECTION_PALETTE[6].to_string()),
        };
        self.collection_name.update(cx, |input, cx| {
            input.set_text(name, cx);
            input.select_all_text(cx);
            input.focus(window, cx);
        });
        self.dialog = Some(Dialog::CollectionForm { id, color });
        self.context_menu = None;
        cx.notify();
    }

    fn commit_collection_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::CollectionForm { id, color }) = self.dialog.take() else {
            return;
        };
        let name = self.collection_name.read(cx).text().trim().to_string();
        if name.is_empty() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }
        let store = Arc::clone(&self.state.read(cx).store);
        let result = match id {
            Some(id) => store
                .update_collection(id, Some(&name), Some(&color))
                .map(|_| id),
            None => store.create_collection(&name, &color).map(|p| p.id),
        };
        match result {
            Ok(id) => {
                self.state.update(cx, |s, cx| {
                    s.refresh_collections();
                    s.set_tab(Tab::Collection(id));
                    cx.notify();
                });
            }
            Err(err) => warn!("collection save failed: {err:#}"),
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn next_collection(&mut self, _: &NextCollection, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, cx);
    }

    fn prev_collection(&mut self, _: &PrevCollection, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, cx);
    }

    pub fn cycle_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.change_tab(delta >= 0, cx, |s| s.cycle_tab(delta));
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let (tabs, current, searching) = {
            let s = self.state.read(cx);
            (s.tabs(), s.tab, s.search.active)
        };
        // The tab already showing: no reload and no slide, just back to its first card.
        if tab == current && !searching {
            self.state.update(cx, |s, cx| {
                s.select_first();
                cx.notify();
            });
            self.scroll_to = Some(0);
            self.sync_preview_to_cursor(cx);
            cx.notify();
            return;
        }
        let from = tabs.iter().position(|t| *t == current).unwrap_or(0);
        let to = tabs.iter().position(|t| *t == tab).unwrap_or(0);
        self.change_tab(to >= from, cx, |s| s.set_tab(tab));
    }

    /// Switches tabs through `apply`, clearing the search and sliding the strip in from the
    /// right (`forward`) or the left.
    fn change_tab(
        &mut self,
        forward: bool,
        cx: &mut Context<Self>,
        apply: impl FnOnce(&mut AppState),
    ) {
        self.search.update(cx, |s, cx| s.set_text("", cx));
        self.state.update(cx, |s, cx| {
            apply(s);
            cx.notify();
        });
        self.scroll_to = Some(0);
        self.tab_generation += 1;
        self.tab_direction = if forward { 1 } else { -1 };
        cx.notify();
    }

    fn add_to_collection(&mut self, ids: &[i64], collection: i64, cx: &mut Context<Self>) {
        let store = Arc::clone(&self.state.read(cx).store);
        for id in ids {
            if let Err(err) = store.add_to_collection(*id, collection) {
                warn!("adding to a collection failed: {err:#}");
            }
        }
        info!("added {} item(s) to collection {collection}", ids.len());
        self.reload(cx);
    }

    fn remove_from_collection(&mut self, id: i64, collection: i64, cx: &mut Context<Self>) {
        let store = Arc::clone(&self.state.read(cx).store);
        if let Err(err) = store.remove_from_collection(id, collection) {
            warn!("removing from a collection failed: {err:#}");
        }
        self.reload(cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.reload_list();
            cx.notify();
        });
        cx.notify();
    }

    /// Moves collection `id` to the position of `target` in the tab order.
    fn move_tab(&mut self, id: i64, target: i64, cx: &mut Context<Self>) {
        if id == target {
            return;
        }
        let mut order: Vec<i64> = self
            .state
            .read(cx)
            .collections
            .iter()
            .map(|p| p.id)
            .collect();
        let Some(from) = order.iter().position(|p| *p == id) else {
            return;
        };
        order.remove(from);
        let to = order
            .iter()
            .position(|p| *p == target)
            .unwrap_or(order.len());
        order.insert(to, id);
        let store = Arc::clone(&self.state.read(cx).store);
        if let Err(err) = store.reorder_collections(&order) {
            warn!("reorder collections failed: {err:#}");
        }
        self.state.update(cx, |s, cx| {
            s.refresh_collections();
            cx.notify();
        });
        cx.notify();
    }

    /// Drops dragged cards before card `target_ix` inside the current collection.
    fn reorder_in_collection(&mut self, drag: &CardDrag, target_ix: usize, cx: &mut Context<Self>) {
        let (tab, searching, ids) = {
            let s = self.state.read(cx);
            (
                s.tab,
                s.search.active,
                s.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            )
        };
        let Tab::Collection(collection) = tab else {
            return;
        };
        if searching || drag.from != tab {
            return;
        }
        let target_id = ids.get(target_ix).copied();
        let mut order: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| !drag.ids.contains(id))
            .collect();
        let at = target_id
            .and_then(|t| order.iter().position(|id| *id == t))
            .unwrap_or(order.len());
        for (offset, id) in drag.ids.iter().enumerate() {
            order.insert(at + offset, *id);
        }
        let store = Arc::clone(&self.state.read(cx).store);
        if let Err(err) = store.reorder_collection_items(collection, &order) {
            warn!("reorder collection items failed: {err:#}");
        }
        self.reload(cx);
    }

    fn pause_capture(&mut self, _: &PauseCapture, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            if s.is_paused() {
                s.resume();
            } else {
                s.pause_for(None);
            }
            s.show_toast(if s.is_paused() {
                "Capture paused"
            } else {
                "Capture resumed"
            });
            cx.notify();
        });
        cx.emit(PanelEvent::PauseToggled);
        self.expire_toast_later(cx);
    }

    fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PanelEvent::OpenSettings);
    }

    fn quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PanelEvent::Quit);
    }

    pub fn expire_toast_later(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(crate::state::TOAST_DURATION)
                .await;
            cx.update(|cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        this.state.update(cx, |s, cx| {
                            if s.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
                                s.toast = None;
                                cx.notify();
                            }
                        });
                    });
                }
            });
        })
        .detach();
    }

    // ---- dialogs opened from outside ----------------------------------------------------------

    pub fn confirm_erase(&mut self, cx: &mut Context<Self>) {
        let count = self.state.read(cx).store.count_items().unwrap_or(0);
        self.dialog = Some(Dialog::ConfirmErase { count });
        cx.notify();
    }

    fn run_menu_action(
        &mut self,
        action: MenuAction,
        item_ix: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.context_menu = None;
        let item = item_ix.and_then(|ix| self.state.read(cx).items.get(ix).cloned());
        match action {
            MenuAction::Paste => {
                if let Some(item) = item {
                    self.paste_items(vec![item], false, cx);
                }
            }
            MenuAction::PasteAlternate => {
                if let Some(item) = item {
                    self.paste_items(vec![item], true, cx);
                }
            }
            MenuAction::Copy => self.copy_only(&CopyOnly, window, cx),
            MenuAction::AddTo(collection) => {
                if let Some(item) = item {
                    self.add_to_collection(&[item.id], collection, cx);
                }
            }
            MenuAction::RemoveFrom(collection) => {
                if let Some(item) = item {
                    self.remove_from_collection(item.id, collection, cx);
                }
            }
            MenuAction::Rename => {
                if let Some(item) = item {
                    self.open_editor(Some(item), true, cx);
                }
            }
            MenuAction::Edit => {
                if let Some(item) = item {
                    self.open_editor(Some(item), false, cx);
                }
            }
            MenuAction::NewText => self.open_editor(None, false, cx),
            MenuAction::ExtractText => {
                if let Some(item) = item {
                    self.extract_text(item, cx);
                }
            }
            MenuAction::Open => {
                if let Some(ix) = item_ix {
                    self.state.update(cx, |s, _| s.select(ix));
                    self.open_item(&OpenItem, window, cx);
                }
            }
            MenuAction::ShowInHome(tab) => {
                if let Some(item) = item {
                    self.jump_to_home(&item, Some(tab), cx);
                }
            }
            MenuAction::Delete => {
                if let Some(ix) = item_ix {
                    let selected = self.state.read(cx).selection.contains(ix);
                    if !selected {
                        self.state.update(cx, |s, _| s.select(ix));
                    }
                    self.delete_items(&DeleteItems, window, cx);
                }
            }
            MenuAction::RenameCollection(id) => self.open_collection_form(Some(id), window, cx),
            MenuAction::RecolorCollection(id, color) => {
                let store = Arc::clone(&self.state.read(cx).store);
                if let Err(err) = store.update_collection(id, None, Some(&color)) {
                    warn!("recolor failed: {err:#}");
                }
                self.state.update(cx, |s, cx| {
                    s.refresh_collections();
                    cx.notify();
                });
            }
            MenuAction::DeleteCollection(id) => {
                let name = self
                    .state
                    .read(cx)
                    .collection(id)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                self.dialog = Some(Dialog::ConfirmDeleteCollection { id, name });
            }
            MenuAction::NewCollection => self.open_collection_form(None, window, cx),
            MenuAction::EraseHistory => self.confirm_erase(cx),
            MenuAction::PausePanel => self.pause_capture(&PauseCapture, window, cx),
            MenuAction::Settings => cx.emit(PanelEvent::OpenSettings),
            MenuAction::Quit => cx.emit(PanelEvent::Quit),
        }
        cx.notify();
    }

    fn card_menu(&self, ix: usize, cx: &App) -> Vec<MenuEntry> {
        let s = self.state.read(cx);
        let Some(item) = s.items.get(ix) else {
            return Vec::new();
        };
        let mut entries = vec![MenuEntry::Item {
            label: "Paste".into(),
            action: MenuAction::Paste,
            enabled: true,
        }];
        if let Some(label) = alternate_paste_label(item.kind, s.settings.paste_plain_text) {
            entries.push(MenuEntry::Item {
                label: label.into(),
                action: MenuAction::PasteAlternate,
                enabled: true,
            });
        }
        entries.extend([
            MenuEntry::Item {
                label: "Copy".into(),
                action: MenuAction::Copy,
                enabled: true,
            },
            MenuEntry::Separator,
            MenuEntry::Submenu {
                label: "Add to Collection".into(),
                entries: pin_entries(&s.collections, item),
            },
        ]);
        if let Tab::Collection(id) = s.tab
            && !s.search.active
        {
            entries.push(MenuEntry::Item {
                label: "Remove from Collection".into(),
                action: MenuAction::RemoveFrom(id),
                enabled: true,
            });
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Item {
            label: "Rename".into(),
            action: MenuAction::Rename,
            enabled: true,
        });
        entries.push(MenuEntry::Item {
            label: "Edit".into(),
            action: MenuAction::Edit,
            enabled: item.kind == Kind::Text,
        });
        if item.kind == Kind::Image {
            entries.push(MenuEntry::Item {
                label: "Extract Text".into(),
                action: MenuAction::ExtractText,
                enabled: true,
            });
        }
        if matches!(item.kind, Kind::Link | Kind::File) {
            entries.push(MenuEntry::Item {
                label: if item.kind == Kind::Link {
                    "Open Link".into()
                } else {
                    "Open".into()
                },
                action: MenuAction::Open,
                enabled: true,
            });
        }
        if s.search.active {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item {
                label: "Show in Clipboard".into(),
                action: MenuAction::ShowInHome(Tab::History),
                enabled: true,
            });
            for id in &item.collection_ids {
                if let Some(board) = s.collection(*id) {
                    entries.push(MenuEntry::Item {
                        label: format!("Show in {}", board.name).into(),
                        action: MenuAction::ShowInHome(Tab::Collection(*id)),
                        enabled: true,
                    });
                }
            }
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Item {
            label: "Delete".into(),
            action: MenuAction::Delete,
            enabled: true,
        });
        entries
    }

    fn overflow_menu(&self, cx: &App) -> Vec<MenuEntry> {
        let paused = self.state.read(cx).is_paused();
        vec![
            MenuEntry::Item {
                label: "New Text Item".into(),
                action: MenuAction::NewText,
                enabled: true,
            },
            MenuEntry::Item {
                label: "New Collection…".into(),
                action: MenuAction::NewCollection,
                enabled: true,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: if paused {
                    "Resume Capture".into()
                } else {
                    "Pause Capture".into()
                },
                action: MenuAction::PausePanel,
                enabled: true,
            },
            MenuEntry::Item {
                label: "Erase History…".into(),
                action: MenuAction::EraseHistory,
                enabled: true,
            },
            MenuEntry::Separator,
            MenuEntry::Item {
                label: "Settings…".into(),
                action: MenuAction::Settings,
                enabled: true,
            },
            MenuEntry::Item {
                label: "Quit PasteLoop".into(),
                action: MenuAction::Quit,
                enabled: true,
            },
        ]
    }

    // ---- events from child entities -----------------------------------------------------------

    /// Cmd-C/Cmd-X inside one of our fields writes the pasteboard; the poller must skip it.
    fn note_own_clipboard_write(&mut self, cx: &mut Context<Self>) {
        let count = macos::pasteboard_change_count();
        self.state
            .update(cx, |s, _| s.own_change_count = Some(count));
    }

    fn on_search_event(
        &mut self,
        _: Entity<TextInput>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::CopiedToClipboard => self.note_own_clipboard_write(cx),
            InputEvent::Changed => {
                let text = self.search.read(cx).text().to_string();
                self.state.update(cx, |s, cx| {
                    s.set_search_text(text);
                    cx.notify();
                });
                self.scroll_to = Some(0);
            }
            InputEvent::Submit => {
                // Return moves focus to the results with the filter kept.
                self.defer_focus_results(cx);
            }
            InputEvent::Cancel => self.defer_hide_or_clear(cx),
            InputEvent::Tab => {
                let accepted = self.state.update(cx, |s, cx| {
                    let ok = s.accept_suggestion();
                    cx.notify();
                    ok
                });
                if accepted {
                    let text = self.state.read(cx).search.text.clone();
                    self.search.update(cx, |input, cx| input.set_text(text, cx));
                } else {
                    self.defer_focus_results(cx);
                }
            }
        }
        cx.notify();
    }

    fn on_editor_event(
        &mut self,
        _: Entity<TextInput>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::CopiedToClipboard => self.note_own_clipboard_write(cx),
            InputEvent::Submit => {
                self.defer_with_window(cx, |this, window, cx| this.commit_editor(window, cx))
            }
            InputEvent::Cancel => {
                self.editor = None;
                self.defer_focus_results(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    fn on_collection_form_event(
        &mut self,
        _: Entity<TextInput>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::CopiedToClipboard => self.note_own_clipboard_write(cx),
            InputEvent::Submit => self.defer_with_window(cx, |this, window, cx| {
                this.commit_collection_form(window, cx)
            }),
            InputEvent::Cancel => {
                self.dialog = None;
                self.defer_focus_results(cx);
            }
            _ => {}
        }
        cx.notify();
    }

    // ---- raw input -----------------------------------------------------------------------------

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Any printable keystroke while the results have focus starts a search.
        if self.search.read(cx).is_focused(window)
            || self.editor.is_some()
            || self.dialog.is_some()
            || self.state.read(cx).preview.is_some()
        {
            return;
        }
        let m = &event.keystroke.modifiers;
        if m.platform || m.control || m.function {
            return;
        }
        let Some(ch) = event.keystroke.key_char.as_deref() else {
            return;
        };
        if ch.is_empty() || ch.chars().all(|c| c.is_control() || c.is_whitespace()) {
            return;
        }
        let ch = ch.to_string();
        self.search.update(cx, |input, cx| {
            input.focus(window, cx);
            input.set_text("", cx);
            input.insert_text(&ch, window, cx);
        });
        window.prevent_default();
        cx.notify();
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifier = self.state.read(cx).settings.quick_paste_modifier;
        let held = match modifier {
            crate::settings::Modifier::Command => event.modifiers.platform,
            crate::settings::Modifier::Option => event.modifiers.alt,
            crate::settings::Modifier::Control => event.modifiers.control,
            crate::settings::Modifier::Shift => event.modifiers.shift,
        };
        if held != self.quick_paste_held {
            self.quick_paste_held = held;
            cx.notify();
        }
    }

    fn on_card_click(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.modifiers();
        if event.click_count() >= 2 {
            let item = self.state.read(cx).items.get(ix).cloned();
            if let Some(item) = item {
                self.paste_items(vec![item], modifiers.shift, cx);
            }
            return;
        }
        self.state.update(cx, |s, cx| {
            if modifiers.platform {
                s.toggle_select(ix);
            } else if modifiers.shift {
                s.extend_to(ix);
            } else {
                s.select(ix);
            }
            cx.notify();
        });
        self.sync_preview_to_cursor(cx);
    }

    fn on_card_right_click(
        &mut self,
        ix: usize,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.state.read(cx).selection.contains(ix);
        if !selected {
            self.state.update(cx, |s, cx| {
                s.select(ix);
                cx.notify();
            });
        }
        self.context_menu = Some(ContextMenu {
            position: event.position,
            entries: self.card_menu(ix, cx),
            item_ix: Some(ix),
            open_submenu: None,
        });
        cx.notify();
    }

    fn on_strip_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A vertical mouse wheel scrolls the strip horizontally; trackpads already send x.
        if let ScrollDelta::Lines(lines) = event.delta
            && lines.x.abs() < 0.01
            && lines.y.abs() > 0.01
        {
            let offset = self.scroll.offset();
            let max = self.scroll.max_offset().x;
            let next = (offset.x + px(lines.y * 40.)).clamp(-max, px(0.));
            self.scroll.set_offset(point(next, offset.y));
        }
        self.maybe_load_more(cx);
    }

    fn maybe_load_more(&mut self, cx: &mut Context<Self>) {
        let (has_more, len) = {
            let s = self.state.read(cx);
            (s.has_more, s.items.len())
        };
        if !has_more || len == 0 {
            return;
        }
        let stride = self.state.read(cx).settings.card_width + CARD_GAP;
        let offset = -self.scroll.offset().x;
        let max = self.scroll.max_offset().x;
        if max > px(0.) && offset >= max - px(stride * 3.) {
            self.state.update(cx, |s, cx| {
                s.load_more();
                cx.notify();
            });
        }
    }

    fn on_resize_down(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (_, y) = macos::mouse_location();
        self.resize = Some(ResizeDrag {
            start_mouse_y: y,
            start_height: self.height,
        });
        cx.notify();
        // Mouse moves stop arriving once the pointer leaves the window, so track the drag by
        // polling the global pointer while the button stays down.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let down = macos::left_mouse_button_down();
                let done = cx.update(|cx| {
                    this.upgrade()
                        .map(|this| {
                            this.update(cx, |this, cx| {
                                if down {
                                    this.resize_step(cx);
                                    false
                                } else {
                                    this.finish_resize(cx);
                                    true
                                }
                            })
                        })
                        .unwrap_or(true)
                });
                if done {
                    break;
                }
            }
        })
        .detach();
    }

    fn resize_step(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = &self.resize else { return };
        let (_, y) = macos::mouse_location();
        let (_, display_height) = macos::panel_frame_under_mouse(1.0);
        // Follow the pointer smoothly. Cards keep their fixed height, so a partial row shows as
        // empty space below until there is room for the next row.
        let wanted = drag.start_height + (y - drag.start_mouse_y) as f32;
        let height = Settings::clamp_panel_height(wanted, display_height as f32);
        if (height - self.height).abs() >= 1.0 {
            self.set_bar_height(height, cx);
        }
    }

    fn finish_resize(&mut self, cx: &mut Context<Self>) {
        if self.resize.take().is_some() {
            // Snap down to remove any partial-row empty space, then persist.
            let settings = self.state.read(cx).settings.clone();
            let rows = settings.card_rows(self.content_height());
            let (_, display_height) = macos::panel_frame_under_mouse(1.0);
            let snapped = Settings::clamp_panel_height(
                self.chrome() + settings.content_height_for_rows(rows),
                display_height as f32,
            );
            self.set_bar_height(snapped, cx);
            self.state.update(cx, |s, _| {
                s.settings.panel_height = snapped;
                s.save_settings();
            });
            info!("panel height set to {snapped} ({rows} row(s))");
            cx.notify();
        }
    }

    /// Non-card vertical space at the current height (grip, toolbar, strip padding).
    fn chrome(&self) -> f32 {
        let compact = self.height < COMPACT_PANEL_HEIGHT;
        let shortcut_bar = if compact { 0. } else { SHORTCUT_BAR };
        RESIZE_GRIP + toolbar_height(compact) + shortcut_bar + strip_padding_y(compact) * 2.
    }

    /// Height available to the card strip's content.
    fn content_height(&self) -> f32 {
        (self.height - self.chrome()).max(1.0)
    }

    /// Shows the bar without taking key focus, so it stays visible next to the Settings window
    /// for a live appearance preview. Skips the entrance animation so re-showing it on focus
    /// changes does not flicker.
    pub fn preview_show(&mut self, cx: &mut Context<Self>) {
        if self.state.read(cx).panel_visible {
            return;
        }
        let frame = self.prepare_show(Instant::now(), cx);
        let native = self.native.clone();
        cx.spawn(async move |_, _| native.order_front(frame))
            .detach();
        self.reveal_fallback(cx);
        cx.notify();
    }

    /// Orders the preview bar out (used when Settings loses focus) without the full hide flow.
    /// The browse is kept, so refocusing Settings brings the bar back as it was.
    pub fn preview_hide(&mut self, cx: &mut Context<Self>) {
        if !self.state.read(cx).panel_visible {
            return;
        }
        self.state.update(cx, |s, cx| {
            s.panel_visible = false;
            s.resume_until = Some(Instant::now() + crate::state::RESUME_WINDOW);
            cx.notify();
        });
        let native = self.native.clone();
        cx.spawn(async move |_, _| native.order_out_transparent())
            .detach();
        cx.notify();
    }

    /// Reacts to state changes: while Settings is open, apply a bar-height change (the Rows
    /// control) to the live bar so it updates in place.
    fn on_state_change(&mut self, _: Entity<AppState>, cx: &mut Context<Self>) {
        // The text size can change from the settings window while the bar is shown; unlike the
        // colors it needs no window to recompute.
        self.theme.text_scale = self.state.read(cx).settings.text_scale;
        if self.resize.is_none() && self.state.read(cx).settings_open {
            let target = self.state.read(cx).settings.panel_height;
            if (target - self.height).abs() >= 1.0 {
                self.set_bar_height(target, cx);
            }
        }
        cx.notify();
    }

    /// Sets the bar height and resizes the window (from a task, outside the borrow).
    fn set_bar_height(&mut self, height: f32, cx: &mut Context<Self>) {
        self.height = height;
        let native = self.native.clone();
        let mut frame = native.frame();
        frame.size.height = height as f64;
        cx.spawn(async move |_, _| native.set_frame(frame)).detach();
        cx.notify();
    }

    // ---- rendering ---------------------------------------------------------------------------

    /// Makes sure the thumbnails of `items` (the cards about to show) exist. Each item is checked
    /// on disk once and then remembered; missing thumbnails are made off the frame, and file
    /// icons (AppKit, main thread) after it.
    fn ensure_thumbnails(&mut self, items: &[Item], cx: &mut Context<Self>) {
        let store = Arc::clone(&self.state.read(cx).store);
        for item in items {
            // Keyed by thumbnail, not item: a file card's thumbnail changes once it gets a
            // picture.
            let Some(key) = item.thumb_key().map(str::to_string) else {
                continue;
            };
            if self.thumbs_ready.contains(&key) || self.thumbs_tried.contains(&key) {
                continue;
            }
            if store.thumb_path(&key).exists() {
                Rc::make_mut(&mut self.thumbs_ready).insert(key);
                continue;
            }
            self.thumbs_tried.insert(key.clone());
            let item = item.clone();
            let store = Arc::clone(&store);
            cx.spawn(async move |this, cx| {
                let file_icon = match item.kind {
                    Kind::File if item.image_blob.is_none() => item
                        .files
                        .first()
                        .and_then(|p| macos::file_icon_png(p, 64.0)),
                    _ => None,
                };
                let made = cx
                    .background_executor()
                    .spawn(async move {
                        let result = match file_icon {
                            Some(png) => jobs::write_thumbnail(&store, &item.content_hash, &png)
                                .map(|_| true),
                            None => jobs::ensure_thumbnail(&store, &item).map(|p| p.is_some()),
                        };
                        result.unwrap_or_else(|err| {
                            warn!("thumbnail failed for item {}: {err:#}", item.id);
                            false
                        })
                    })
                    .await;
                cx.update(|cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| {
                            if made {
                                this.thumbs_tried.remove(&key);
                                Rc::make_mut(&mut this.thumbs_ready).insert(key);
                                cx.notify();
                            }
                        });
                    }
                });
            })
            .detach();
        }
    }

    fn render_tabs(&self, compact: bool, cx: &mut Context<Self>) -> Div {
        let theme = self.theme.clone();
        let s = self.state.read(cx);
        let active = s.tab;
        let searching = s.search.active;
        let boards = s.collections.clone();
        let mut tabs = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .min_w(px(0.))
            .overflow_hidden();
        let make_tab = |label: SharedString,
                        tab: Tab,
                        color: Option<gpui::Hsla>,
                        cx: &mut Context<Self>| {
            let is_active = active == tab && !searching;
            let theme = theme.clone();
            let label_for_preview = label.clone();
            div()
                .id(match tab {
                    Tab::History => ("tab", 0u64),
                    Tab::Collection(id) => ("tab", id as u64 + 1),
                })
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .px(px(10.))
                .h(px(if compact { 24. } else { 28. }))
                .text_size(px(12.))
                .cursor_pointer()
                .border_b_2()
                .when(is_active, |d| {
                    d.text_color(theme.text).border_color(theme.accent)
                })
                .when(!is_active, |d| {
                    d.text_color(theme.text_secondary)
                        .border_color(gpui::transparent_black())
                        .hover(|s| s.text_color(theme.text))
                })
                .when_some(color, |d, c| {
                    d.child(div().size(px(8.)).rounded_full().bg(c))
                })
                .child(label)
                .when(matches!(tab, Tab::Collection(_)), |d| {
                    let accent = theme.accent;
                    d.drag_over::<CardDrag>(move |style, _, _, _| style.bg(accent.opacity(0.3)))
                        .drag_over::<TabDrag>(move |style, _, _, _| style.bg(accent.opacity(0.3)))
                        .on_drop(cx.listener(move |this, drag: &CardDrag, _, cx| {
                            if let Tab::Collection(id) = tab {
                                this.add_to_collection(&drag.ids, id, cx);
                            }
                        }))
                        .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                            if let Tab::Collection(target) = tab {
                                this.move_tab(drag.id, target, cx);
                            }
                        }))
                })
                .when_some(
                    match tab {
                        Tab::Collection(id) => Some(id),
                        Tab::History => None,
                    },
                    |d, id| {
                        let theme = theme.clone();
                        let name = label_for_preview.clone();
                        d.on_drag(TabDrag { id }, move |_, _, _, cx| {
                            cx.new(|_| DragPreview {
                                label: name.clone(),
                                theme: theme.clone(),
                            })
                        })
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        if let Tab::Collection(id) = tab {
                            let mut entries = vec![MenuEntry::Item {
                                label: "Rename…".into(),
                                action: MenuAction::RenameCollection(id),
                                enabled: true,
                            }];
                            for name in COLLECTION_PALETTE {
                                entries.push(MenuEntry::Item {
                                    label: format!("Color: {name}").into(),
                                    action: MenuAction::RecolorCollection(id, name.to_string()),
                                    enabled: true,
                                });
                            }
                            entries.push(MenuEntry::Separator);
                            entries.push(MenuEntry::Item {
                                label: "Delete Collection…".into(),
                                action: MenuAction::DeleteCollection(id),
                                enabled: true,
                            });
                            this.context_menu = Some(ContextMenu {
                                position: event.position,
                                entries,
                                item_ix: None,
                                open_submenu: None,
                            });
                            cx.notify();
                        }
                    }),
                )
        };
        tabs = tabs.child(make_tab("Clipboard".into(), Tab::History, None, cx));
        for board in boards {
            let color = collection_color(&board.color, theme.dark);
            tabs = tabs.child(make_tab(
                board.name.clone().into(),
                Tab::Collection(board.id),
                Some(color),
                cx,
            ));
        }
        tabs
    }

    fn toolbar_button(
        &self,
        id: &'static str,
        icon: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let theme = self.theme.clone();
        div()
            .id(id)
            .size(px(28.))
            .rounded(px(8.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(theme.field))
            .child(
                svg()
                    .path(icon)
                    .size(px(14.))
                    .text_color(theme.text_secondary),
            )
            .on_click(cx.listener(on_click))
    }

    fn render_search(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme.clone();
        let chips = self.state.read(cx).search.chips.clone();
        let suggestion = self.state.read(cx).suggest_chip();
        let mut field = div()
            .w(px(SEARCH_WIDTH))
            .h(px(28.))
            .rounded(px(8.))
            .bg(theme.field)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .px(px(8.))
            .child(
                svg()
                    .path("icons/search.svg")
                    .size(px(12.))
                    .text_color(theme.text_secondary)
                    .flex_shrink_0(),
            );
        for (ix, chip) in chips.iter().enumerate() {
            let label = chip.label();
            field = field.child(
                div()
                    .id(("chip", ix))
                    .flex_shrink_0()
                    .px(px(6.))
                    .h(px(20.))
                    .rounded(px(6.))
                    .bg(theme.accent.opacity(0.25))
                    .text_size(px(11.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .cursor_pointer()
                    .child(SharedString::from(label))
                    .child(div().text_color(theme.text_secondary).child("×"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.state.update(cx, |s, cx| {
                            s.remove_chip(ix);
                            cx.notify();
                        });
                    })),
            );
        }
        field = field.child(div().flex_1().min_w(px(40.)).child(self.search.clone()));
        if let Some(chip) = suggestion {
            field = field.child(
                div()
                    .flex_shrink_0()
                    .text_size(px(10.))
                    .text_color(theme.text_secondary)
                    .child(format!("⇥ {}", chip.label())),
            );
        }
        field
    }

    fn render_toolbar(
        &self,
        compact: bool,
        counter: Option<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme.clone();
        div()
            .h(px(toolbar_height(compact)))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(STRIP_PADDING))
            .child(self.render_tabs(compact, cx))
            // Paused state lives in the toolbar so it never takes height from the cards.
            .when(self.state.read(cx).is_paused(), |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .px(px(8.))
                        .h(px(20.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .bg(theme.field)
                        .text_size(px(11.))
                        .text_color(theme.text_secondary)
                        .child("Paused · ⌘T"),
                )
            })
            .child(div().flex_1())
            .child(self.render_search(cx))
            .child(div().flex_1())
            .children(counter)
            .child(
                self.toolbar_button("filter", "icons/filter.svg", cx, |this, event, _, cx| {
                    if let ClickEvent::Mouse(m) = event {
                        this.filter_anchor = m.up.position;
                    }
                    this.filter_popover = !this.filter_popover;
                    this.context_menu = None;
                    cx.notify();
                }),
            )
            .child(
                self.toolbar_button("add", "icons/plus.svg", cx, |this, _, window, cx| {
                    this.open_collection_form(None, window, cx)
                }),
            )
            .child(
                self.toolbar_button("more", "icons/more.svg", cx, |this, event, _, cx| {
                    let position = match event {
                        ClickEvent::Mouse(m) => m.up.position,
                        _ => point(px(0.), px(0.)),
                    };
                    this.context_menu = Some(ContextMenu {
                        position,
                        entries: this.overflow_menu(cx),
                        item_ix: None,
                        open_submenu: None,
                    });
                    cx.notify();
                }),
            )
            .border_b_1()
            .border_color(theme.separator)
    }

    /// "012 / 348": the number of the first card in view, on digit wheels that roll to each new
    /// value like a mechanical counter, over the length of the list.
    fn render_counter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        const DIGIT_W: f32 = 9.;
        const DIGIT_H: f32 = 16.;
        let (count, total, has_more, card_width, rows) = {
            let s = self.state.read(cx);
            (
                s.items.len(),
                s.total,
                s.has_more,
                s.settings.card_width,
                s.settings.card_rows(self.content_height()),
            )
        };
        if count == 0 {
            return None;
        }
        let stride = card_width + CARD_GAP;
        let scrolled = f32::from(-self.scroll.offset().x);
        // A card counts as passed once most of it has scrolled off.
        let first_col = ((scrolled + stride * 0.5) / stride).floor().max(0.) as usize;
        let value = (first_col * rows + 1).min(count);
        let length = total.unwrap_or(count).max(count);
        let digits = length.to_string().len().max(2);
        if self.odometer.step(value, digits) {
            window.request_animation_frame();
        }
        let theme = &self.theme;
        let wheel = |position: f32| {
            div()
                .w(px(DIGIT_W))
                .h(px(DIGIT_H))
                .relative()
                .overflow_hidden()
                .rounded(px(2.))
                .bg(theme.field)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .w_full()
                        .top(px(-position.rem_euclid(10.) * DIGIT_H))
                        .flex()
                        .flex_col()
                        .children((0..=10).map(|d| {
                            div()
                                .h(px(DIGIT_H))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(SharedString::from((d % 10).to_string()))
                        })),
                )
        };
        let more = if total.is_none() && has_more { "+" } else { "" };
        Some(
            div()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(1.))
                .text_size(px(11.))
                .text_color(theme.text_secondary)
                .children(self.odometer.wheels.iter().rev().map(|w| wheel(*w)))
                .child(
                    div()
                        .ml(px(5.))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(format!("/ {length}{more}"))),
                )
                .into_any_element(),
        )
    }

    /// The keys that matter, along the bottom edge.
    fn render_shortcut_bar(&self, cx: &mut Context<Self>) -> Div {
        let theme = &self.theme;
        let s = &self.state.read(cx).settings;
        let quick = s.quick_paste_modifier.symbol();
        let alt = s.alternate_modifier.symbol();
        let other_style = if s.paste_plain_text {
            "formatted"
        } else {
            "plain text"
        };
        let hints = [
            "↩ paste".to_string(),
            format!("{alt}↩ {other_style}"),
            "↑ first card".to_string(),
            "space preview".to_string(),
            format!("{quick}1–9 paste by number"),
            "esc close".to_string(),
        ];
        div()
            .h(px(SHORTCUT_BAR))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(18.))
            .px(px(STRIP_PADDING))
            .border_t_1()
            .border_color(theme.separator)
            .text_size(px(11.))
            .text_color(theme.text_tertiary)
            .children(hints.into_iter().map(SharedString::from))
    }

    fn render_strip(&mut self, compact: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme.clone();
        let (
            items,
            selection,
            terms,
            tab,
            searching,
            now_ms,
            collections,
            highlight,
            is_empty_search,
        ) = {
            let s = self.state.read(cx);
            (
                s.items.clone(),
                s.selection.clone(),
                s.search.terms(),
                s.tab,
                s.search.active,
                self.now_ms,
                s.collections.clone(),
                s.highlight_item.map(|(id, _)| id),
                s.search.active && s.items.is_empty(),
            )
        };
        if items.is_empty() {
            let message = if is_empty_search {
                let chips: Vec<String> = self
                    .state
                    .read(cx)
                    .search
                    .chips
                    .iter()
                    .map(|c| c.label())
                    .collect();
                if chips.is_empty() {
                    "No results".to_string()
                } else {
                    format!("No results for {}", chips.join(", "))
                }
            } else {
                match tab {
                    Tab::History => "Nothing copied yet".to_string(),
                    Tab::Collection(_) => {
                        "Drag a card here, or right-click a card → Add to Collection".to_string()
                    }
                }
            };
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_secondary)
                .text_size(px(13.))
                .child(SharedString::from(message))
                .into_any_element();
        }
        let (card_width, card_height, rows) = {
            let s = &self.state.read(cx).settings;
            (
                s.card_width,
                s.card_max_height,
                s.card_rows(self.content_height()),
            )
        };
        {
            // Roughly the cards on screen plus a margin; exactness does not matter here.
            let first_col = ((-self.scroll.offset().x) / px(card_width + CARD_GAP))
                .floor()
                .max(0.) as usize;
            let first = (first_col * rows).min(items.len());
            let end = (first + 16 * rows).min(items.len());
            self.ensure_thumbnails(&items[first..end], cx);
        }
        let thumbs_ready = Rc::clone(&self.thumbs_ready);
        let view = cx.entity().downgrade();
        let quick_paste_held = self.quick_paste_held;
        let modifier = self.state.read(cx).settings.quick_paste_modifier.symbol();
        let keycaps: Vec<SharedString> = (1..=9)
            .map(|n| SharedString::from(format!("{modifier}{n}")))
            .collect();
        let store = Arc::clone(&self.state.read(cx).store);
        let hovered = self.hovered;
        let tab_generation = self.tab_generation;
        let tab_direction = self.tab_direction;
        let strip = card_strip("strip", items.len(), move |ix, _window, _cx| {
            let Some(view) = view.upgrade() else {
                return div().into_any_element();
            };
            let item = &items[ix];
            let thumb = item
                .thumb_key()
                .filter(|key| thumbs_ready.contains(*key))
                .map(|key| store.thumb_path(key));
            let collection_colors: Vec<gpui::Hsla> = if searching || matches!(tab, Tab::History) {
                item.collection_ids
                    .iter()
                    .filter_map(|id| collections.iter().find(|p| p.id == *id))
                    .map(|p| collection_color(&p.color, theme.dark))
                    .collect()
            } else {
                Vec::new()
            };
            let selected = selection.contains(ix);
            let props = CardProps {
                theme: &theme,
                card_width,
                selected,
                keycap: keycaps.get(ix).cloned(),
                keycaps_lit: quick_paste_held,
                compact: compact && hovered != Some(ix),
                highlight_terms: &terms,
                thumb,
                collection_colors,
                now_ms,
                flash: highlight == Some(item.id),
            };
            let (click_view, right_view, hover_view, drop_view) =
                (view.clone(), view.clone(), view.clone(), view);
            let drag_ids: Vec<i64> = if selection.contains(ix) && selection.set.len() > 1 {
                selection
                    .set
                    .iter()
                    .filter_map(|i| items.get(*i))
                    .map(|i| i.id)
                    .collect()
            } else {
                vec![item.id]
            };
            let drag_label = if drag_ids.len() > 1 {
                format!("{} items", drag_ids.len())
            } else {
                crate::store::truncate_chars(&item.display_title(), 40)
            };
            let payload_ids = drag_ids.clone();
            let payload_items = Arc::clone(&items);
            let drag = CardDrag {
                ids: drag_ids,
                from: tab,
                label: drag_label,
            };
            let preview_theme = theme.clone();
            let drag_store = Arc::clone(&store);
            let accent = theme.accent;
            render_card(ix, item, props)
                .on_drag(drag, move |drag: &CardDrag, _, _, cx| {
                    let label: SharedString = drag.label.clone().into();
                    let theme = preview_theme.clone();
                    cx.new(|_| DragPreview { label, theme })
                })
                .external_drag_payload::<CardDrag>(move |_, window, cx| {
                    let dragged: Vec<Item> = payload_items
                        .iter()
                        .filter(|i| payload_ids.contains(&i.id))
                        .cloned()
                        .collect();
                    external_payload(&dragged, &drag_store, window, cx)
                })
                .drag_over::<CardDrag>(move |style, _, _, _| {
                    style.border_color(accent.opacity(0.6))
                })
                .on_drop(move |drag: &CardDrag, _, cx| {
                    drop_view.update(cx, |this, cx| this.reorder_in_collection(drag, ix, cx))
                })
                .on_click(move |event: &ClickEvent, window, cx| {
                    click_view.update(cx, |this, cx| this.on_card_click(ix, event, window, cx))
                })
                .on_mouse_down(
                    MouseButton::Right,
                    move |event: &MouseDownEvent, window, cx| {
                        right_view.update(cx, |this, cx| {
                            this.on_card_right_click(ix, event, window, cx)
                        })
                    },
                )
                .on_hover(move |hovered: &bool, _, cx| {
                    hover_view.update(cx, |this, cx| {
                        let next = if *hovered {
                            Some(ix)
                        } else if this.hovered == Some(ix) {
                            None
                        } else {
                            this.hovered
                        };
                        if next != this.hovered {
                            this.hovered = next;
                            cx.notify();
                        }
                    })
                })
                .into_any_element()
        })
        .layout(card_width, card_height, rows)
        .with_scroll_handle(&self.scroll)
        .scroll_to_item(self.scroll_to.take())
        .flex_1()
        .px(px(STRIP_PADDING))
        .py(px(strip_padding_y(compact)))
        .on_scroll_wheel(cx.listener(Self::on_strip_scroll));
        let content = if tab_generation > 0 && !self.theme.reduce_motion {
            let offset = 40. * tab_direction as f32;
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .child(strip)
                .with_animation(
                    ("tab-slide", tab_generation),
                    gpui::Animation::new(Duration::from_millis(140))
                        .with_easing(gpui::ease_out_quint()),
                    move |el, delta| {
                        el.left(px(offset * (1. - delta)))
                            .opacity(0.4 + 0.6 * delta)
                    },
                )
                .into_any_element()
        } else {
            strip.into_any_element()
        };
        gpui::image_cache(self.image_cache.clone())
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn render_context_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let theme = self.theme.clone();
        let item_ix = menu.item_ix;
        let viewport = window.viewport_size();
        // Submenus open to the right unless that would leave the window, and grow upward in the
        // lower half so they stay inside the bar.
        let open_left = menu.position.x + px(MENU_WIDTH * 2. + 16.) > viewport.width;
        let open_up = menu.position.y > viewport.height / 2.;
        // A plain row. On the top level, hovering it closes an open submenu.
        let item_row = |id: gpui::ElementId,
                        label: SharedString,
                        action: MenuAction,
                        enabled: bool,
                        top_level: bool,
                        cx: &mut Context<Self>| {
            div()
                .id(id)
                .px(px(12.))
                .h(px(24.))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(!enabled, |d| d.text_color(theme.text_secondary))
                .when(enabled, |d| {
                    d.hover(|s| s.bg(theme.accent).text_color(theme.on_accent))
                })
                .child(label)
                .when(enabled, |d| {
                    d.on_click(cx.listener(move |this, _, window, cx| {
                        this.run_menu_action(action.clone(), item_ix, window, cx);
                    }))
                })
                .when(top_level, |d| {
                    d.on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if *hovered
                            && let Some(menu) = &mut this.context_menu
                            && menu.open_submenu.take().is_some()
                        {
                            cx.notify();
                        }
                    }))
                })
                .into_any_element()
        };
        let mut top = menu_surface(&theme);
        for (ix, entry) in menu.entries.iter().enumerate() {
            top = top.child(match entry {
                MenuEntry::Separator => menu_separator(&theme).into_any_element(),
                MenuEntry::Item {
                    label,
                    action,
                    enabled,
                } => item_row(
                    ("menu", ix).into(),
                    label.clone(),
                    action.clone(),
                    *enabled,
                    true,
                    cx,
                ),
                MenuEntry::Submenu { label, entries } => {
                    let open = menu.open_submenu == Some(ix);
                    let mut row = div()
                        .id(("menu", ix))
                        .relative()
                        .px(px(12.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .cursor_pointer()
                        .when(open, |d| d.bg(theme.accent).text_color(theme.on_accent))
                        .child(label.clone())
                        .child("›")
                        .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                            if *hovered
                                && let Some(menu) = &mut this.context_menu
                                && menu.open_submenu != Some(ix)
                            {
                                menu.open_submenu = Some(ix);
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(menu) = &mut this.context_menu {
                                menu.open_submenu = Some(ix);
                                cx.notify();
                            }
                        }));
                    if open {
                        let mut sub = menu_surface(&theme)
                            .absolute()
                            .when(open_left, |d| d.right_full().mr(px(4.)))
                            .when(!open_left, |d| d.left_full().ml(px(4.)))
                            .when(open_up, |d| d.bottom(px(-5.)))
                            .when(!open_up, |d| d.top(px(-5.)));
                        for (sub_ix, sub_entry) in entries.iter().enumerate() {
                            sub = sub.child(match sub_entry {
                                MenuEntry::Item {
                                    label,
                                    action,
                                    enabled,
                                } => item_row(
                                    ("submenu", sub_ix).into(),
                                    label.clone(),
                                    action.clone(),
                                    *enabled,
                                    false,
                                    cx,
                                ),
                                // One level of nesting is all the menus use.
                                _ => menu_separator(&theme).into_any_element(),
                            });
                        }
                        row = row.child(sub);
                    }
                    row.into_any_element()
                }
            });
        }
        // A transparent backdrop takes clicks outside the menu (and its submenu) and closes it.
        let close = cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.context_menu = None;
            cx.notify();
        });
        let close_right = cx.listener(|this, _: &MouseDownEvent, _, cx| {
            this.context_menu = None;
            cx.notify();
        });
        Some(
            deferred(
                div()
                    .absolute()
                    .inset_0()
                    .child(
                        div()
                            .id("menu-backdrop")
                            .absolute()
                            .inset_0()
                            .occlude()
                            .on_mouse_down(MouseButton::Left, close)
                            .on_mouse_down(MouseButton::Right, close_right),
                    )
                    .child(
                        anchored()
                            .position(menu.position)
                            .anchor(Anchor::TopLeft)
                            .snap_to_window_with_margin(px(8.))
                            .child(top),
                    ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    fn render_filter_popover(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.filter_popover {
            return None;
        }
        let theme = self.theme.clone();
        let s = self.state.read(cx);
        let chips = s.search.chips.clone();
        let mut apps: Vec<crate::store::AppInfo> = s.apps.values().cloned().collect();
        apps.sort_by_key(|a| std::cmp::Reverse(a.last_seen_at));
        apps.truncate(8);
        let collections = s.collections.clone();
        let section = |title: &str| {
            div()
                .text_size(px(10.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text_secondary)
                .mt(px(6.))
                .child(SharedString::from(title.to_string()))
        };
        let chip_button = |id: gpui::ElementId,
                           label: String,
                           chip: Chip,
                           active: bool,
                           cx: &mut Context<Self>| {
            let theme = theme.clone();
            div()
                .id(id)
                .px(px(8.))
                .h(px(22.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .cursor_pointer()
                .when(active, |d| d.bg(theme.accent.opacity(0.3)))
                .when(!active, |d| d.hover(|s| s.bg(theme.field)))
                .child(SharedString::from(label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let chip = chip.clone();
                    this.state.update(cx, |s, cx| {
                        if let Some(pos) = s.search.chips.iter().position(|c| *c == chip) {
                            s.remove_chip(pos);
                        } else {
                            s.add_chip(chip);
                        }
                        cx.notify();
                    });
                }))
        };
        let mut panel = div()
            .w(px(320.))
            .p(px(10.))
            .rounded(px(10.))
            .bg(theme.menu)
            .border_1()
            .border_color(theme.separator)
            .shadow_lg()
            .text_size(px(12.))
            .text_color(theme.text)
            .flex()
            .flex_col()
            .gap(px(4.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.filter_popover = false;
                cx.notify();
            }));
        panel = panel.child(section("Type"));
        let mut row = div().flex().flex_row().flex_wrap().gap(px(4.));
        for kind in Kind::ALL {
            let chip = Chip::Kind(kind);
            let active = chips.contains(&chip);
            row = row.child(chip_button(
                gpui::ElementId::Name(format!("kind-{}", kind.as_str()).into()),
                kind.label().to_string(),
                chip,
                active,
                cx,
            ));
        }
        panel = panel.child(row);
        panel = panel.child(section("Date"));
        let mut row = div().flex().flex_row().flex_wrap().gap(px(4.));
        for range in DateRange::ALL {
            let chip = Chip::Date(range);
            let active = chips.contains(&chip);
            row = row.child(chip_button(
                gpui::ElementId::Name(format!("date-{}", range.label()).into()),
                range.label().to_string(),
                chip,
                active,
                cx,
            ));
        }
        panel = panel.child(row);
        if !apps.is_empty() {
            panel = panel.child(section("App"));
            let mut row = div().flex().flex_row().flex_wrap().gap(px(4.));
            for app in apps {
                let name = app.name.clone().unwrap_or_else(|| app.bundle_id.clone());
                let chip = Chip::App {
                    bundle_id: app.bundle_id.clone(),
                    name: name.clone(),
                };
                let active = chips.contains(&chip);
                row = row.child(chip_button(
                    gpui::ElementId::Name(format!("app-{}", app.bundle_id).into()),
                    name,
                    chip,
                    active,
                    cx,
                ));
            }
            panel = panel.child(row);
        }
        if !collections.is_empty() {
            panel = panel.child(section("Collection"));
            let mut row = div().flex().flex_row().flex_wrap().gap(px(4.));
            for board in collections {
                let chip = Chip::Collection {
                    id: board.id,
                    name: board.name.clone(),
                };
                let active = chips.contains(&chip);
                row = row.child(chip_button(
                    ("collection", board.id as u64).into(),
                    board.name.clone(),
                    chip,
                    active,
                    cx,
                ));
            }
            panel = panel.child(row);
        }
        Some(
            deferred(
                anchored()
                    .position(self.filter_anchor)
                    .anchor(Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(div().pr(px(1.)).child(panel)),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn dialog_button(
        &self,
        id: &'static str,
        label: &str,
        primary: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        crate::cards::button(id, label, primary, &self.theme)
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    fn render_dialog(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let theme = self.theme.clone();
        let mut body = div()
            .w(px(420.))
            .p(px(16.))
            .rounded(px(12.))
            .bg(theme.menu)
            .border_1()
            .border_color(theme.separator)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(theme.text)
            .flex()
            .flex_col()
            .gap(px(12.));
        let buttons = div().flex().flex_row().justify_end().gap(px(8.));
        body = match dialog {
            Dialog::LaunchAtLogin => body
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Open PasteLoop at login?"))
                .child("PasteLoop only captures what you copy while it is running. You can change this later in Settings.")
                .child(
                    buttons
                        .child(self.dialog_button("login-no", "Not Now", false, cx, |this, _, cx| {
                            this.dialog = None;
                            this.state.update(cx, |s, _| {
                                s.settings.launch_at_login = Some(false);
                                s.save_settings();
                            });
                            cx.notify();
                        }))
                        .child(self.dialog_button("login-yes", "Open at Login", true, cx, |this, _, cx| {
                            this.accept_dialog(cx)
                        })),
                ),
            Dialog::Backup => body
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Back up your history to iCloud Drive?"))
                .child("PasteLoop keeps a copy in iCloud Drive → PasteLoop and restores it automatically on a new Mac or after reinstalling macOS. iCloud Drive is encrypted, but Apple holds the key unless Advanced Data Protection is on. You can change this later in Settings.")
                .child(
                    buttons
                        .child(self.dialog_button("backup-no", "Not Now", false, cx, |this, _, cx| {
                            this.dialog = None;
                            this.state.update(cx, |s, _| {
                                s.settings.icloud_backup = Some(false);
                                s.save_settings();
                            });
                            cx.notify();
                        }))
                        .child(self.dialog_button("backup-yes", "Back Up", true, cx, |this, _, cx| {
                            this.accept_dialog(cx)
                        })),
                ),
            Dialog::ConfirmErase { count } => body
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Erase History?"))
                .child(format!("This deletes {count} item(s) that are not in a collection. This cannot be undone."))
                .child(
                    buttons
                        .child(self.dialog_button("cancel", "Cancel", false, cx, |this, _, cx| {
                            this.dialog = None;
                            cx.notify();
                        }))
                        .child(self.dialog_button("erase", "Erase", true, cx, |this, _, cx| {
                            this.accept_dialog(cx)
                        })),
                ),
            Dialog::ConfirmDeleteCollection { name, .. } => {
                body.child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!("Delete “{name}”?")))
                    .child("Items stay in your clipboard history; only the collection goes away.")
                    .child(
                        buttons
                            .child(self.dialog_button("cancel", "Cancel", false, cx, |this, _, cx| {
                                this.dialog = None;
                                cx.notify();
                            }))
                            .child(self.dialog_button("delete", "Delete", true, cx, |this, _, cx| {
                                this.accept_dialog(cx)
                            })),
                    )
            }
            Dialog::Accessibility(_) => body
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Allow PasteLoop to paste for you"))
                .child(
                    "To paste directly into the app you were using, PasteLoop presses ⌘V on your behalf. \
                     macOS only allows that with Accessibility permission. Until then, items are \
                     copied to the clipboard for you to paste manually.",
                )
                .child(
                    buttons
                        .child(self.dialog_button("never", "Don't Ask Again", false, cx, |this, _, cx| {
                            this.state.update(cx, |s, _| {
                                s.settings.ask_for_accessibility = false;
                                s.save_settings();
                            });
                            info!("accessibility reminder turned off");
                            if let Some(Dialog::Accessibility(p)) = this.dialog.take() {
                                this.paste_now(p.items, p.alternate, cx);
                            }
                            cx.notify();
                        }))
                        .child(self.dialog_button("later", "Copy Only", false, cx, |this, _, cx| {
                            if let Some(Dialog::Accessibility(p)) = this.dialog.take() {
                                this.paste_now(p.items, p.alternate, cx);
                            }
                            cx.notify();
                        }))
                        .child(self.dialog_button("open", "Open System Settings", true, cx, |this, _, cx| {
                            this.accept_dialog(cx)
                        })),
                ),
            Dialog::Relaunch(_) => body
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Relaunch PasteLoop to finish enabling paste"))
                .child(
                    "Accessibility permission was granted after PasteLoop started. macOS only lets an \
                     app press ⌘V for you if it had the permission when it launched, so until you \
                     relaunch, items are copied to the clipboard for you to paste manually.",
                )
                .child(
                    buttons
                        .child(self.dialog_button("relaunch-later", "Not Now", false, cx, |this, _, cx| {
                            if let Some(Dialog::Relaunch(Some(p))) = this.dialog.take() {
                                this.paste_now(p.items, p.alternate, cx);
                            }
                            cx.notify();
                        }))
                        .child(self.dialog_button("relaunch", "Relaunch PasteLoop", true, cx, |this, _, cx| {
                            this.accept_dialog(cx)
                        })),
                ),
            Dialog::CollectionForm { id, color } => {
                let editing = id.is_some();
                let current = color.clone();
                let mut swatches = div().flex().flex_row().gap(px(6.));
                for name in COLLECTION_PALETTE {
                    let c = collection_color(name, theme.dark);
                    let selected = name == current;
                    let name_owned = name.to_string();
                    swatches = swatches.child(
                        div()
                            .id(gpui::ElementId::Name(format!("swatch-{name}").into()))
                            .size(px(20.))
                            .rounded_full()
                            .bg(c)
                            .cursor_pointer()
                            .border_2()
                            .border_color(if selected { theme.text } else { gpui::transparent_black() })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(Dialog::CollectionForm { color, .. }) = &mut this.dialog {
                                    *color = name_owned.clone();
                                }
                                cx.notify();
                            })),
                    );
                }
                body.child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(if editing { "Rename Collection" } else { "New Collection" }))
                    .child(
                        div()
                            .h(px(30.))
                            .px(px(8.))
                            .rounded(px(8.))
                            .bg(theme.field)
                            .flex()
                            .items_center()
                            .child(self.collection_name.clone()),
                    )
                    .child(swatches)
                    .child(
                        buttons
                            .child(self.dialog_button("cancel", "Cancel", false, cx, |this, window, cx| {
                                this.dialog = None;
                                window.focus(&this.focus_handle, cx);
                                cx.notify();
                            }))
                            .child(self.dialog_button("save", if editing { "Save" } else { "Create" }, true, cx, |this, window, cx| {
                                this.commit_collection_form(window, cx)
                            })),
                    )
            }
        };
        Some(modal(body))
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let editor = self.editor.as_ref()?;
        let theme = self.theme.clone();
        let title = match (editor.item_id, editor.rename) {
            (_, true) => "Rename",
            (Some(_), false) => "Edit Text",
            (None, false) => "New Text Item",
        };
        let mono = self.editor_input.read(cx).monospace();
        let body = div()
            .w(px(560.))
            .max_h(px(self.height - 40.))
            .p(px(14.))
            .rounded(px(12.))
            .bg(theme.menu)
            .border_1()
            .border_color(theme.separator)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(theme.text)
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .when(!editor.rename, |d| {
                        d.child(
                            div()
                                .id("mono-toggle")
                                .text_size(px(11.))
                                .text_color(theme.text_secondary)
                                .cursor_pointer()
                                .child(if mono { "Monospaced ✓" } else { "Monospaced" })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.editor_input
                                        .update(cx, |input, cx| input.set_monospace(!mono, cx));
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("editor-scroll")
                    .flex_1()
                    .min_h(px(if editor.rename { 30. } else { 120. }))
                    .max_h(px(self.height - 140.))
                    .overflow_y_scroll()
                    .p(px(8.))
                    .rounded(px(8.))
                    .bg(theme.field)
                    .child(self.editor_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_secondary)
                            .child(if editor.rename {
                                "Return to save · Esc to cancel"
                            } else {
                                "⌘Return to save · Shift-Return for a new line · Esc to cancel"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(self.dialog_button(
                                "editor-cancel",
                                "Cancel",
                                false,
                                cx,
                                |this, window, cx| {
                                    this.editor = None;
                                    window.focus(&this.focus_handle, cx);
                                    cx.notify();
                                },
                            ))
                            .child(self.dialog_button(
                                "editor-save",
                                "Save",
                                true,
                                cx,
                                |this, window, cx| this.commit_editor(window, cx),
                            )),
                    ),
            );
        Some(modal(body))
    }

    fn render_toast(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let toast = self.state.read(cx).toast.clone()?;
        if Instant::now() >= toast.until {
            return None;
        }
        let theme = self.theme.clone();
        Some(
            div()
                .absolute()
                .bottom(px(12.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(14.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .bg(theme.menu)
                        .border_1()
                        .border_color(theme.separator)
                        .shadow_md()
                        .text_size(px(12.))
                        .text_color(theme.text)
                        .child(SharedString::from(toast.text)),
                )
                .into_any_element(),
        )
    }
}

impl PanelView {
    /// Runs `f` with the panel's window on the next tick. Child-entity event callbacks have no
    /// `Window`, and the panel is already borrowed by the dispatch, so work that needs either
    /// is deferred through the window handle.
    fn defer_with_window(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let window = self.window_handle;
        cx.spawn(async move |this, cx| {
            cx.update(|cx| {
                if let Some(this) = this.upgrade() {
                    let _ = window.update(cx, |_, window, cx| {
                        this.update(cx, |view, cx| f(view, window, cx))
                    });
                }
            });
        })
        .detach();
    }

    fn defer_focus_results(&self, cx: &mut Context<Self>) {
        self.defer_with_window(cx, |this, window, cx| window.focus(&this.focus_handle, cx));
    }

    fn defer_hide_or_clear(&self, cx: &mut Context<Self>) {
        self.defer_with_window(cx, |this, _, cx| {
            let searching = this.state.read(cx).search.active;
            if searching {
                this.search.update(cx, |s, cx| s.set_text("", cx));
                this.state.update(cx, |s, cx| {
                    s.clear_search();
                    cx.notify();
                });
            } else {
                this.hide(cx);
            }
        });
    }
}

/// The context menu's second paste entry for this kind, or none when both pastes are the same.
fn alternate_paste_label(kind: Kind, plain_by_default: bool) -> Option<&'static str> {
    match kind {
        Kind::Text | Kind::Link if plain_by_default => Some("Paste with Formatting"),
        Kind::Text | Kind::Link => Some("Paste as Plain Text"),
        Kind::Image | Kind::File => Some("Paste as Path"),
        Kind::Color => None,
    }
}

/// The "Add to Collection" submenu: each collection (checked when the item is in it; choosing a
/// checked one removes it), then "New Collection…".
fn pin_entries(boards: &[crate::store::Collection], item: &Item) -> Vec<MenuEntry> {
    let mut entries: Vec<MenuEntry> = boards
        .iter()
        .map(|b| {
            let member = item.collection_ids.contains(&b.id);
            MenuEntry::Item {
                label: if member {
                    format!("✓ {}", b.name).into()
                } else {
                    b.name.clone().into()
                },
                action: if member {
                    MenuAction::RemoveFrom(b.id)
                } else {
                    MenuAction::AddTo(b.id)
                },
                enabled: true,
            }
        })
        .collect();
    if !entries.is_empty() {
        entries.push(MenuEntry::Separator);
    }
    entries.push(MenuEntry::Item {
        label: "New Collection…".into(),
        action: MenuAction::NewCollection,
        enabled: true,
    });
    entries
}

/// Digit wheels for the position counter. Wheel `i` (ones first) holds the counter's value at
/// that place, unwrapped (`value / 10^i`), so rolling from 19 to 20 turns the ones wheel forward
/// through 9 to 0 while the tens wheel turns from 1 to 2.
#[derive(Default)]
struct Odometer {
    wheels: Vec<f32>,
    last_step: Option<Instant>,
    /// The next step jumps straight to the value (a fresh showing).
    snap: bool,
}

impl Odometer {
    /// Eases the wheels toward `value`; returns whether they are still turning.
    fn step(&mut self, value: usize, digits: usize) -> bool {
        let now = Instant::now();
        let dt = self
            .last_step
            .map_or(0., |t| (now - t).as_secs_f32())
            .min(0.05);
        self.last_step = Some(now);
        let target = |i: usize| (value / 10usize.pow(i as u32)) as f32;
        let snap = std::mem::take(&mut self.snap);
        if snap || self.wheels.len() != digits {
            self.wheels = (0..digits).map(target).collect();
            return false;
        }
        // A one-step roll settles in about a quarter second, whatever the frame rate.
        let ease = 1. - (-dt * 20.).exp();
        let mut turning = false;
        for (i, wheel) in self.wheels.iter_mut().enumerate() {
            let goal = target(i);
            // A long jump spins through its last ten steps only.
            if (goal - *wheel).abs() > 10. {
                *wheel = goal - 10. * (goal - *wheel).signum();
            }
            *wheel += (goal - *wheel) * ease;
            if (goal - *wheel).abs() < 0.01 {
                *wheel = goal;
            } else {
                turning = true;
            }
        }
        turning
    }
}

/// Width of a context menu and of its submenus.
const MENU_WIDTH: f32 = 200.;

fn menu_surface(theme: &Theme) -> Div {
    div()
        .w(px(MENU_WIDTH))
        .py(px(4.))
        .rounded(px(8.))
        .bg(theme.menu)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .occlude()
        .text_size(px(12.))
        .text_color(theme.text)
        .flex()
        .flex_col()
}

fn menu_separator(theme: &Theme) -> Div {
    div().h(px(1.)).my(px(4.)).bg(theme.separator)
}

/// `body` centered over a dimmed panel, above everything else.
fn modal(body: Div) -> gpui::AnyElement {
    deferred(
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.25))
            .child(body),
    )
    .with_priority(3)
    .into_any_element()
}

/// What a card offers to other apps when the drag leaves the window. Files and (when the
/// setting is on) images go through GPUI's native file drag; everything else starts a text
/// drag session of its own.
fn external_payload(
    items: &[Item],
    store: &Store,
    window: &mut Window,
    cx: &mut App,
) -> Option<ExternalDragPayload> {
    let mut files: Vec<(std::path::PathBuf, bool)> = Vec::new();
    let mut reps: Option<paste::Representations> = None;
    for item in items {
        match item.kind {
            Kind::File => {
                for path in &item.files {
                    let p = std::path::PathBuf::from(path);
                    let is_dir = p.is_dir();
                    files.push((p, is_dir));
                }
            }
            Kind::Image => {
                let r = paste::representations(item, store, paste::Form::default(), true);
                if let Some(path) = r.image_file {
                    files.push((path, false));
                }
            }
            _ => {
                let r = paste::representations(item, store, paste::Form::default(), false);
                match &mut reps {
                    None => reps = Some(r),
                    Some(acc) => {
                        if let (Some(a), Some(b)) = (&mut acc.plain, r.plain) {
                            a.push('\n');
                            a.push_str(&b);
                        }
                    }
                }
            }
        }
    }
    if !files.is_empty() {
        return Some(ExternalDragPayload::Files(FileDragPaths::new(files)));
    }
    if let Some(reps) = reps
        && macos::begin_text_drag(window, &reps)
    {
        debug!("started native text drag");
        // AppKit runs the session and swallows the mouse-up; GPUI would keep its in-window drag
        // alive until the next click. Watch the button and end the drag ourselves.
        window
            .spawn(cx, async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(32))
                        .await;
                    if !macos::left_mouse_button_down() {
                        break;
                    }
                }
                macos::post_left_mouse_up_to_self();
            })
            .detach();
    }
    None
}

impl Focusable for PanelView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.image_cache
            .update(cx, |images, cx| images.start_frame(window, cx));
        if let Some(t0) = self.shown_at.take() {
            info!(
                "hotkey -> first render: {:.1} ms",
                t0.elapsed().as_secs_f64() * 1000.0
            );
            // The window came in transparent so the frame from its last showing never flashes;
            // make it visible once this fresh frame is drawn.
            let native = self.native.clone();
            window.on_next_frame(move |_, _| {
                native.reveal();
            });
        }
        let compact = self.height < COMPACT_PANEL_HEIGHT;
        let theme = self.theme.clone();

        let mut root = div()
            .id("panel-root")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::hide_panel))
            .on_action(cx.listener(Self::next_card))
            .on_action(cx.listener(Self::prev_card))
            .on_action(cx.listener(Self::first_card))
            .on_action(cx.listener(Self::last_card))
            .on_action(cx.listener(Self::extend_right))
            .on_action(cx.listener(Self::extend_left))
            .on_action(cx.listener(Self::select_all_cards))
            .on_action(cx.listener(Self::paste_selected))
            .on_action(cx.listener(Self::paste_alternate))
            .on_action(cx.listener(Self::copy_only))
            .on_action(cx.listener(Self::quick_look))
            .on_action(cx.listener(Self::open_item))
            .on_action(cx.listener(Self::rename_item))
            .on_action(cx.listener(Self::edit_item))
            .on_action(cx.listener(Self::new_text_item))
            .on_action(cx.listener(Self::delete_items))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::toggle_focus))
            .on_action(cx.listener(Self::jump_home))
            .on_action(cx.listener(Self::new_collection))
            .on_action(cx.listener(Self::next_collection))
            .on_action(cx.listener(Self::prev_collection))
            .on_action(cx.listener(Self::pause_capture))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::quit))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .text_color(theme.text)
            .font_family(MONO)
            .text_size(px(12.));
        macro_rules! quick_paste_actions {
            ($($n:literal => $default:ident, $alternate:ident),* $(,)?) => {
                $(
                    root = root
                        .on_action(cx.listener(|this, _: &$default, _, cx| this.quick_paste($n, false, cx)))
                        .on_action(cx.listener(|this, _: &$alternate, _, cx| this.quick_paste($n, true, cx)));
                )*
            };
        }
        quick_paste_actions!(
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

        // Resize grip along the top edge. It is a flow element because an absolute one
        // hit-tested a few points below where it painted.
        root = root.child(
            div()
                .id("resize-grip")
                .w_full()
                .h(px(RESIZE_GRIP))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor(gpui::CursorStyle::ResizeUpDown)
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_resize_down))
                .child(
                    div()
                        .w(px(36.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(theme.text_secondary.opacity(0.35)),
                ),
        );
        let counter = self.render_counter(window, cx);
        root = root.child(self.render_toolbar(compact, counter, cx));
        root = root.child(self.render_strip(compact, cx));
        if !compact {
            root = root.child(self.render_shortcut_bar(cx));
        }
        if let Some(menu) = self.render_context_menu(window, cx) {
            root = root.child(menu);
        }
        if let Some(popover) = self.render_filter_popover(cx) {
            root = root.child(popover);
        }
        if let Some(dialog) = self.render_dialog(cx) {
            root = root.child(dialog);
        }
        if let Some(editor) = self.render_editor(cx) {
            root = root.child(editor);
        }
        if let Some(toast) = self.render_toast(cx) {
            root = root.child(toast);
        }
        // Entrance: 24 pt rise with a fade, 160 ms ease-out-quint; reduce motion keeps the fade.
        let reduce_motion = self.theme.reduce_motion;
        root.with_animation(
            ("show", self.show_generation),
            gpui::Animation::new(Duration::from_millis(160)).with_easing(gpui::ease_out_quint()),
            move |root, delta| {
                let root = root.opacity(delta.max(0.05));
                if reduce_motion {
                    root
                } else {
                    root.top(px(24. * (1. - delta)))
                }
            },
        )
    }
}

/// The hotkey flow: show, or cycle collections when already open, or hide when there are none.
pub async fn toggle(handle: WindowHandle<PanelView>, t0: Instant, cx: &mut AsyncApp) {
    let Ok((visible, has_collections)) = cx.update(|cx| {
        handle.update(cx, |view, _, cx| {
            let s = view.state.read(cx);
            (s.panel_visible, !s.collections.is_empty())
        })
    }) else {
        return;
    };
    if visible {
        cx.update(|cx| {
            handle
                .update(cx, |view, _, cx| {
                    if has_collections {
                        view.cycle_tab(1, cx);
                    } else {
                        view.hide(cx);
                    }
                })
                .ok();
        });
        return;
    }
    show(handle, t0, cx).await;
}

pub async fn show(handle: WindowHandle<PanelView>, t0: Instant, cx: &mut AsyncApp) {
    let Ok((native, frame)) = cx.update(|cx| {
        handle.update(cx, |view, _, cx| {
            (view.native.clone(), view.prepare_show(t0, cx))
        })
    }) else {
        return;
    };
    native.show_at(frame);
    cx.update(|cx| {
        handle
            .update(cx, |view, window, cx| view.after_show(window, cx))
            .ok();
    });
}

/// Re-anchors a visible panel after displays change.
pub async fn reanchor(handle: WindowHandle<PanelView>, cx: &mut AsyncApp) {
    let Ok(Some((native, frame))) = cx.update(|cx| {
        handle.update(cx, |view, _, cx| {
            if !view.state.read(cx).panel_visible {
                return None;
            }
            let (frame, _) = macos::panel_frame_under_mouse(view.height as f64);
            Some((view.native.clone(), frame))
        })
    }) else {
        return;
    };
    native.set_frame(frame);
}

#[cfg(test)]
mod tests {
    use super::{Odometer, alternate_paste_label};

    #[test]
    fn alternate_paste_is_named_for_the_kind() {
        use crate::store::Kind;
        assert_eq!(
            alternate_paste_label(Kind::Text, true),
            Some("Paste with Formatting")
        );
        assert_eq!(
            alternate_paste_label(Kind::Link, false),
            Some("Paste as Plain Text")
        );
        assert_eq!(
            alternate_paste_label(Kind::Image, true),
            Some("Paste as Path")
        );
        assert_eq!(alternate_paste_label(Kind::Color, true), None);
    }

    #[test]
    fn odometer_snaps_then_rolls_to_the_value() {
        let mut odo = Odometer {
            snap: true,
            ..Default::default()
        };
        assert!(!odo.step(12, 3));
        assert_eq!(odo.wheels, vec![12., 1., 0.]);

        // A long jump starts ten steps short of the value, then eases in.
        assert!(odo.step(348, 3));
        assert!(odo.wheels[0] >= 338. && odo.wheels[0] < 348.);
        let start = std::time::Instant::now();
        while odo.step(348, 3) {
            assert!(start.elapsed().as_secs_f32() < 2.0, "wheels never settled");
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        assert_eq!(odo.wheels, vec![348., 34., 3.]);

        // More digits than before: the new wheel set starts at the value.
        assert!(!odo.step(1024, 4));
        assert_eq!(odo.wheels, vec![1024., 102., 10., 1.]);
    }
}
