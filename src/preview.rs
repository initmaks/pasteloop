//! The floating window above the panel that shows a card at full size (Space on a card). It is
//! display-only: the panel keeps keyboard focus, and Space or Esc there closes it. The item to
//! show lives in `AppState::preview`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Context, Entity, FocusHandle, Focusable, Global,
    ObjectFit, RetainAllImageCache, SharedString, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, div, image_cache, img, point, prelude::*, px, rgb,
    size, svg,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use tracing::{info, warn};

use crate::cards::{Theme, contrasting_text, parse_hex};
use crate::jobs;
use crate::macos::NativeWindow;
use crate::state::AppState;
use crate::store::{Item, Kind, Store};

/// Window size for content with no natural size (text, links, colors, documents).
const DEFAULT_SIZE: (f64, f64) = (640.0, 360.0);
const MIN_SIZE: (f64, f64) = (360.0, 220.0);
/// Space around an image: the window padding, the title row, and the image frame's inset.
const IMAGE_CHROME: (f64, f64) = (44.0, 72.0);
/// Share of the space above the panel (and of the screen width) the window may take.
const MAX_SHARE: f64 = 0.9;
/// Gap between the preview's bottom edge and the panel's top edge.
const GAP: f64 = 8.0;
/// Text files larger than this show as a file, not as text.
const MAX_TEXT_FILE: u64 = 1024 * 1024;

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "heic", "heif", "tif", "tiff", "webp", "bmp",
];
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "toml", "yaml", "yml", "xml", "html",
    "css", "js", "ts", "tsx", "jsx", "rs", "py", "rb", "go", "java", "kt", "swift", "c", "h",
    "cpp", "hpp", "m", "sh", "zsh", "fish", "sql", "ini", "conf", "cfg", "env",
];

fn has_extension(path: &str, list: &[&str]) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.contains(&e.to_ascii_lowercase().as_str()))
}

/// The single file of a file item, when there is exactly one.
fn single_file(item: &Item) -> Option<&str> {
    match (item.kind, item.files.as_slice()) {
        (Kind::File, [path]) => Some(path),
        _ => None,
    }
}

fn image_file(item: &Item) -> Option<&str> {
    single_file(item).filter(|p| has_extension(p, IMAGE_EXTENSIONS))
}

fn text_file(item: &Item) -> Option<&str> {
    single_file(item).filter(|p| {
        has_extension(p, TEXT_EXTENSIONS)
            && std::fs::metadata(p).is_ok_and(|m| m.len() <= MAX_TEXT_FILE)
    })
}

/// The pixel size of what the preview shows, when that is an image.
fn image_pixels(item: &Item) -> Option<(f64, f64)> {
    match item.kind {
        Kind::Image => Some((item.image_w? as f64, item.image_h? as f64)),
        Kind::File => {
            let (w, h) = image::image_dimensions(image_file(item)?).ok()?;
            Some((w as f64, h as f64))
        }
        _ => None,
    }
    .filter(|(w, h)| *w > 0.0 && *h > 0.0)
}

/// The window frame, centered above the panel: an image at its actual size (in points on this
/// display), anything else at the default size, never more than `MAX_SHARE` of the space above
/// the panel. An image that has to shrink keeps its proportions.
fn frame_for(item: Option<&Item>, panel: &NativeWindow) -> NSRect {
    let panel_frame = panel.frame();
    let screen = panel.screen_visible_frame();
    let bottom = panel_frame.origin.y + panel_frame.size.height + GAP;
    let max_w = (screen.size.width * MAX_SHARE).max(MIN_SIZE.0);
    let max_h = ((screen.origin.y + screen.size.height - bottom) * MAX_SHARE).max(MIN_SIZE.1);
    let (w, h) = match item.and_then(image_pixels) {
        Some((pw, ph)) => {
            let scale = panel.backing_scale().max(1.0);
            let (iw, ih) = (pw / scale, ph / scale);
            let fit = ((max_w - IMAGE_CHROME.0) / iw)
                .min((max_h - IMAGE_CHROME.1) / ih)
                .min(1.0);
            (iw * fit + IMAGE_CHROME.0, ih * fit + IMAGE_CHROME.1)
        }
        None => DEFAULT_SIZE,
    };
    let (w, h) = (w.clamp(MIN_SIZE.0, max_w), h.clamp(MIN_SIZE.1, max_h));
    let x = panel_frame.origin.x + (panel_frame.size.width - w) / 2.0;
    NSRect::new(NSPoint::new(x, bottom), NSSize::new(w, h))
}

/// The preview window handle, created on first use and kept alive.
pub struct PreviewWindow(pub WindowHandle<PreviewView>);
impl Global for PreviewWindow {}

pub struct PreviewView {
    state: Entity<AppState>,
    native: NativeWindow,
    focus_handle: FocusHandle,
    /// The full text of the text item on show, keyed by (id, updated_at). The view re-renders
    /// on every state change; this keeps it from re-reading a long text from disk each time.
    text: Option<(i64, i64, PreviewText)>,
    /// Decoded images of the item on show, freed when another item (or none) is shown.
    images: Entity<RetainAllImageCache>,
    images_for: Option<i64>,
    /// Items whose preview-size copy is being made or failed (a failure is not retried).
    scaling: HashSet<i64>,
    _subscription: gpui::Subscription,
}

#[derive(Clone)]
struct PreviewText {
    text: SharedString,
    mono: bool,
}

impl PreviewView {
    fn new(state: Entity<AppState>, native: NativeWindow, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        PreviewView {
            state,
            native,
            focus_handle: cx.focus_handle(),
            text: None,
            images: RetainAllImageCache::new(cx),
            images_for: None,
            scaling: HashSet::new(),
            _subscription: subscription,
        }
    }

    pub fn native(&self) -> &NativeWindow {
        &self.native
    }
}

impl Focusable for PreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PreviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (item, store, text_scale) = {
            let s = self.state.read(cx);
            (
                s.preview.clone(),
                Arc::clone(&s.store),
                s.settings.text_scale,
            )
        };
        let theme = Theme::for_window(window).with_text_scale(text_scale);
        let hint = match item.as_ref().map(|i| i.kind) {
            Some(Kind::Text) => "Space or Esc to close · ⌘E to edit",
            Some(Kind::Link) | Some(Kind::File) => "Space or Esc to close · ⌘O to open",
            _ => "Space or Esc to close",
        };
        let shown = item.as_ref().map(|i| i.id);
        if shown != self.images_for {
            self.images_for = shown;
            self.images
                .update(cx, |images, cx| images.clear(window, cx));
            // Shown transparent (see `show`); visible once this item's first frame is drawn.
            let native = self.native.clone();
            window.on_next_frame(move |_, _| {
                native.reveal();
            });
        }
        let text = item
            .as_ref()
            .filter(|i| i.kind == Kind::Text || text_file(i).is_some())
            .map(|i| self.text_for(i, &store));
        let image = item.as_ref().and_then(|i| match i.kind {
            Kind::Image => self.image_for(i, &store, cx),
            _ => image_file(i).map(PathBuf::from),
        });
        let body = match &item {
            Some(item) => image_cache(self.images.clone())
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .child(render_item(item, text, image, &store, &theme))
                .into_any_element(),
            None => div().flex_1().into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.))
            .bg(theme.menu)
            .text_color(theme.text)
            .font_family(crate::cards::MONO)
            .p(px(14.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .flex_shrink_0()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Preview"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.text_secondary)
                            .child(hint),
                    ),
            )
            .child(body)
    }
}

impl PreviewView {
    /// The file to show for an image item: the original when it is small, else a preview-size
    /// copy, with the card thumbnail standing in while that copy is made.
    fn image_for(
        &mut self,
        item: &Item,
        store: &Arc<Store>,
        cx: &mut Context<Self>,
    ) -> Option<PathBuf> {
        let blob = item.image_blob.as_deref()?;
        let long_edge = item.image_w.unwrap_or(0).max(item.image_h.unwrap_or(0));
        if long_edge <= jobs::PREVIEW_LONG_EDGE as i64 {
            return Some(store.blob_path(blob));
        }
        let scaled = store.thumb_path(&jobs::preview_key(blob));
        if scaled.exists() {
            return Some(scaled);
        }
        if self.scaling.insert(item.id) {
            let (item, store) = (item.clone(), Arc::clone(store));
            cx.spawn(async move |this, cx| {
                let id = item.id;
                let made = cx
                    .background_executor()
                    .spawn(async move { jobs::ensure_preview_image(&store, &item) })
                    .await;
                match made {
                    Ok(_) => {
                        this.update(cx, |this, cx| {
                            this.scaling.remove(&id);
                            cx.notify();
                        })
                        .ok();
                    }
                    Err(err) => warn!("preview image failed for item {id}: {err:#}"),
                }
            })
            .detach();
        }
        Some(store.thumb_path(blob)).filter(|p| p.exists())
    }

    fn text_for(&mut self, item: &Item, store: &Store) -> PreviewText {
        if let Some((id, updated, text)) = &self.text
            && (*id, *updated) == (item.id, item.updated_at)
        {
            return text.clone();
        }
        let full = match text_file(item) {
            Some(path) => std::fs::read(path)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default(),
            None => store.full_text(item).ok().flatten().unwrap_or_default(),
        };
        let text = PreviewText {
            mono: crate::clipboard::looks_like_code(&full),
            text: full.into(),
        };
        self.text = Some((item.id, item.updated_at, text.clone()));
        text
    }
}

/// Renders the item at full size according to its kind. `text` is the full text of a text item
/// or text file, and `image` the file to show for an image item or image file.
fn render_item(
    item: &Item,
    text: Option<PreviewText>,
    image: Option<PathBuf>,
    store: &Store,
    theme: &Theme,
) -> gpui::AnyElement {
    let panel = |d: gpui::Div| d.flex_1().min_h(px(0.)).rounded(px(8.)).bg(theme.field);
    match item.kind {
        Kind::Text => {
            let PreviewText { text, mono } = text.unwrap_or(PreviewText {
                text: SharedString::default(),
                mono: false,
            });
            let mut d = panel(div())
                .id("preview-scroll")
                .overflow_y_scroll()
                .p(px(14.))
                .text_size(theme.scale(14.));
            if mono {
                d = d.font_family("Menlo").text_size(theme.scale(13.));
            }
            d.child(text).into_any_element()
        }
        Kind::Image => {
            let path = image;
            let d = panel(div())
                .flex()
                .items_center()
                .justify_center()
                .p(px(8.));
            match path {
                Some(path) => d
                    .child(img(path).size_full().object_fit(ObjectFit::Contain))
                    .into_any_element(),
                None => d
                    .child(
                        svg()
                            .path("icons/image.svg")
                            .size(px(64.))
                            .text_color(theme.text_secondary),
                    )
                    .into_any_element(),
            }
        }
        Kind::Link => {
            // Domain, the title if one was captured, and the URL. The preview never fetches.
            let url = item.text.clone().unwrap_or_default();
            let domain = item.link_domain.clone().unwrap_or_else(|| url.clone());
            let mut col = panel(div())
                .id("preview-link-scroll")
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(16.))
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .text_color(theme.text_secondary)
                        .text_size(theme.scale(13.))
                        .child(
                            svg()
                                .path("icons/link.svg")
                                .size(px(14.))
                                .text_color(theme.text_secondary),
                        )
                        .child(SharedString::from(domain)),
                );
            if let Some(title) = item.link_title.clone().filter(|t| !t.trim().is_empty()) {
                col = col.child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(theme.scale(18.))
                        .child(SharedString::from(title)),
                );
            }
            col.child(
                div()
                    .text_size(theme.scale(15.))
                    .text_color(theme.text)
                    .child(SharedString::from(url)),
            )
            .into_any_element()
        }
        Kind::Color => {
            let hex = item.text.clone().unwrap_or_default();
            let color = parse_hex(&hex).unwrap_or(0x808080);
            div()
                .flex_1()
                .min_h(px(0.))
                .rounded(px(8.))
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(color))
                .text_color(contrasting_text(color))
                .font_family("Menlo")
                .text_size(theme.scale(22.))
                .child(SharedString::from(hex))
                .into_any_element()
        }
        Kind::File => {
            if let Some(path) = image {
                return panel(div())
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(8.))
                    .child(img(path).size_full().object_fit(ObjectFit::Contain))
                    .into_any_element();
            }
            if let Some(PreviewText { text, mono }) = text {
                let mut d = panel(div())
                    .id("preview-file-text")
                    .overflow_y_scroll()
                    .p(px(14.))
                    .text_size(theme.scale(14.));
                if mono {
                    d = d.text_size(theme.scale(13.));
                }
                return d.child(text).into_any_element();
            }
            let icon = store.thumb_path(&item.content_hash);
            let mut col = panel(div())
                .id("preview-file-scroll")
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .p(px(14.))
                .overflow_y_scroll();
            col = col.child(if icon.exists() {
                img(icon)
                    .size(px(96.))
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            } else {
                svg()
                    .path("icons/file.svg")
                    .size(px(64.))
                    .text_color(theme.text_secondary)
                    .into_any_element()
            });
            for path in &item.files {
                let size = std::fs::metadata(path)
                    .map(|m| format_size(m.len()))
                    .unwrap_or_else(|_| "missing".to_string());
                col = col.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(2.))
                        .child(SharedString::from(
                            crate::store::file_name(path).to_string(),
                        ))
                        .child(
                            div()
                                .text_size(theme.scale(12.))
                                .text_color(theme.text_secondary)
                                .child(SharedString::from(format!("{path} · {size}"))),
                        ),
                );
            }
            col.into_any_element()
        }
    }
}

fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else {
        format!("{:.1} GB", b / KB / KB / KB)
    }
}

fn ensure_window(state: Entity<AppState>, cx: &mut App) -> Option<WindowHandle<PreviewView>> {
    if let Some(handle) = cx.try_global::<PreviewWindow>().map(|w| w.0) {
        return Some(handle);
    }
    let hide = state.read(cx).settings.hide_from_screen_sharing;
    let result = cx.open_window(
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
                size: size(px(DEFAULT_SIZE.0 as f32), px(DEFAULT_SIZE.1 as f32)),
            })),
            ..Default::default()
        },
        {
            let state = state.clone();
            move |window, cx| {
                let native = NativeWindow::of(window).expect("preview window");
                native.configure_panel(hide);
                cx.new(|cx| PreviewView::new(state, native, cx))
            }
        },
    );
    match result {
        Ok(handle) => {
            cx.set_global(PreviewWindow(handle));
            Some(handle)
        }
        Err(err) => {
            warn!("preview window failed: {err:#}");
            None
        }
    }
}

/// Frame that sits centered just above the panel.
/// Shows the preview window above the panel, sized for `AppState::preview`, which must already be
/// set. Window ordering runs outside any GPUI update, so this is async like the queue window.
pub async fn show(state: Entity<AppState>, panel_native: NativeWindow, cx: &mut AsyncApp) {
    let shown = cx.update(|cx| {
        let item = state.read(cx).preview.clone();
        let native =
            ensure_window(state, cx).and_then(|h| h.update(cx, |v, _, _| v.native.clone()).ok());
        native.map(|native| (native, item))
    });
    let Some((native, item)) = shown else {
        warn!("preview: no window handle; not shown");
        return;
    };
    let frame = frame_for(item.as_ref(), &panel_native);
    info!(
        "preview: order_front at ({:.0},{:.0}) size {:.0}x{:.0}",
        frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
    );
    // Order in without taking key focus, so the panel keeps it and Space/Esc still work.
    native.order_front(frame);
    // If macOS withholds the first frame of a transparent window, show it anyway.
    cx.background_executor()
        .timer(std::time::Duration::from_millis(100))
        .await;
    if native.reveal() {
        warn!("preview revealed by the fallback; its first frame did not arrive in time");
    }
}

/// Orders the preview window out. Safe to call when it was never shown.
pub async fn hide(cx: &mut AsyncApp) {
    let native = cx.update(|cx| {
        cx.try_global::<PreviewWindow>()
            .map(|w| w.0)
            .and_then(|h| h.update(cx, |v, _, _| v.native.clone()).ok())
    });
    if let Some(native) = native {
        native.order_out_transparent();
    }
}
