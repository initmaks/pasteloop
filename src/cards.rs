//! Card rendering per item kind, the theme, relative time formatting, and the virtualized strip
//! element that lays fixed-size cards out in columns.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Div, Element, ElementId, GlobalElementId,
    HighlightStyle, Hitbox, InspectorElementId, InteractiveElement, Interactivity, IntoElement,
    LayoutId, ObjectFit, Overflow, Pixels, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement, StyledText, Window, WindowAppearance, div, hsla, img, point,
    prelude::*, px, rgb, rgba, size, svg,
};
use smallvec::SmallVec;

use crate::store::{Item, Kind};

pub const CARD_GAP: f32 = 12.0;
pub const CARD_RADIUS: f32 = 8.0;
pub const STRIP_PADDING: f32 = 18.0;
pub const CARD_HEADER_HEIGHT: f32 = 34.0;
pub const CARD_FOOTER_HEIGHT: f32 = 24.0;
/// Cards rendered beyond the visible range on each side.
const OVERSCAN: usize = 2;

/// Decoded card images (thumbnails, app and file icons). GPUI's default image cache never frees
/// anything, so scrolling a long history would keep every image decoded; this keeps the most
/// recently drawn ones and drops the rest from memory and the GPU atlas.
pub struct CardImageCache {
    /// Image, and the number of the frame that last drew it.
    items: HashMap<u64, (gpui::ImageCacheItem, u64)>,
    frame: u64,
}

impl CardImageCache {
    /// A few screens of image cards; each decoded thumbnail is up to about 0.6 MB.
    const CAPACITY: usize = 64;

    pub fn new(cx: &mut App) -> gpui::Entity<Self> {
        cx.new(|_| CardImageCache {
            items: HashMap::new(),
            frame: 0,
        })
    }

    /// Called before each frame is laid out. Evicts only images the previous frame did not draw:
    /// those are in no scene, so dropping their atlas sprites is safe, and nothing the next frame
    /// is likely to draw gets decoded again.
    pub fn start_frame(&mut self, window: &mut Window, cx: &mut App) {
        let last_frame = self.frame;
        self.frame += 1;
        if self.items.len() <= Self::CAPACITY {
            return;
        }
        let mut stale: Vec<(u64, u64)> = self
            .items
            .iter()
            .filter(|(_, (_, drawn))| *drawn < last_frame)
            .map(|(key, (_, drawn))| (*drawn, *key))
            .collect();
        stale.sort_unstable();
        // Down to three quarters, so eviction runs once per many frames.
        let excess = self.items.len().saturating_sub(Self::CAPACITY * 3 / 4);
        for (_, key) in stale.into_iter().take(excess) {
            if let Some((item, _)) = self.items.remove(&key)
                && let Some(Ok(image)) = item.get()
            {
                cx.drop_image(image, Some(window));
            }
        }
    }
}

impl gpui::ImageCache for CardImageCache {
    fn load(
        &mut self,
        resource: &gpui::Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Result<Arc<gpui::RenderImage>, gpui::ImageCacheError>> {
        let frame = self.frame;
        let entry = self
            .items
            .entry(gpui::hash(resource))
            .or_insert_with(|| (gpui::ImageCacheItem::new(resource, cx), frame));
        entry.1 = frame;
        entry.0.use_image(window)
    }
}

/// Colors for the current appearance: graphite or zinc neutrals with an amber accent. Rebuilt
/// when the window appearance changes.
#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    pub surface: gpui::Hsla,
    /// Fill of the selected card; other cards are outlined only.
    pub card: gpui::Hsla,
    pub card_hover: gpui::Hsla,
    /// Card outlines.
    pub border: gpui::Hsla,
    pub text: gpui::Hsla,
    pub text_secondary: gpui::Hsla,
    pub text_tertiary: gpui::Hsla,
    pub separator: gpui::Hsla,
    pub accent: gpui::Hsla,
    /// Text on an accent fill.
    pub on_accent: gpui::Hsla,
    pub field: gpui::Hsla,
    pub menu: gpui::Hsla,
    pub reduce_motion: bool,
    /// Multiplier applied to every text size in the UI (appearance setting).
    pub text_scale: f32,
}

/// The monospace face the panel is set in (ships with macOS).
pub const MONO: &str = "Menlo";

impl Theme {
    /// The theme for a window's current appearance.
    pub fn for_window(window: &Window) -> Theme {
        Theme::new(window.appearance(), crate::macos::reduce_motion())
    }

    /// Text field colors that match this theme (the fields default to black text).
    pub fn input_colors(&self) -> crate::text_input::InputColors {
        crate::text_input::InputColors {
            text: self.text,
            placeholder: self.text_tertiary,
            cursor: self.accent,
            selection: self.accent.opacity(0.3),
        }
    }

    /// A font size scaled by the user's text-size setting.
    pub fn scale(&self, size: f32) -> gpui::Pixels {
        px(size * self.text_scale)
    }

    /// Return a copy with the text-size multiplier applied.
    pub fn with_text_scale(mut self, scale: f32) -> Theme {
        self.text_scale = scale;
        self
    }

    pub fn new(appearance: WindowAppearance, reduce_motion: bool) -> Theme {
        let dark = matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        if dark {
            Theme {
                dark,
                surface: rgba(0x18181BF2).into(),
                card: rgb(0x232326).into(),
                card_hover: rgba(0xFFFFFF08).into(),
                border: rgb(0x3A3A40).into(),
                text: rgb(0xE4E4E7).into(),
                text_secondary: rgb(0xA1A1AA).into(),
                text_tertiary: rgb(0x71717A).into(),
                separator: rgba(0xFFFFFF14).into(),
                accent: rgb(0xF5A524).into(),
                on_accent: rgb(0x18181B).into(),
                field: rgba(0xFFFFFF0F).into(),
                menu: rgba(0x1F1F23F5).into(),
                reduce_motion,
                text_scale: 1.0,
            }
        } else {
            Theme {
                dark,
                surface: rgba(0xFAFAFAF2).into(),
                card: rgb(0xFFFFFF).into(),
                card_hover: rgba(0x0000000A).into(),
                border: rgb(0xD4D4D8).into(),
                text: rgb(0x18181B).into(),
                text_secondary: rgb(0x52525B).into(),
                text_tertiary: rgb(0x71717A).into(),
                separator: rgba(0x00000014).into(),
                accent: rgb(0xD97706).into(),
                on_accent: rgb(0x18181B).into(),
                field: rgba(0x0000000D).into(),
                menu: rgba(0xFFFFFFF5).into(),
                reduce_motion,
                text_scale: 1.0,
            }
        }
    }
}

/// A push button: accent-filled when `primary`, else on the field color. Callers add the click.
pub fn button(
    id: impl Into<ElementId>,
    label: &str,
    primary: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(12.))
        .h(px(26.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(12.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .when(primary, |d| d.bg(theme.accent).text_color(theme.on_accent))
        .when(!primary, |d| d.bg(theme.field).text_color(theme.text))
        .child(SharedString::from(label.to_string()))
}

/// The ten collection colors, as AppKit's system colors for the current appearance.
pub const COLLECTION_PALETTE: [&str; 10] = [
    "red", "orange", "yellow", "green", "mint", "teal", "blue", "indigo", "purple", "gray",
];

pub fn collection_color(name: &str, dark: bool) -> gpui::Hsla {
    let (light, dark_hex) = match name {
        "red" => (0xFF3B30, 0xFF453A),
        "orange" => (0xFF9500, 0xFF9F0A),
        "yellow" => (0xFFCC00, 0xFFD60A),
        "green" => (0x34C759, 0x30D158),
        "mint" => (0x00C7BE, 0x63E6E2),
        "teal" => (0x30B0C7, 0x40C8E0),
        "blue" => (0x007AFF, 0x0A84FF),
        "indigo" => (0x5856D6, 0x5E5CE6),
        "purple" => (0xAF52DE, 0xBF5AF2),
        _ => (0x8E8E93, 0x98989D),
    };
    rgb(if dark { dark_hex } else { light }).into()
}

/// "just now", "5 min ago", "3 h ago", "Yesterday", "Sep 12", "Sep 12, 2025".
pub fn relative_time(created_ms: i64, now_ms: i64) -> String {
    let delta = (now_ms - created_ms).max(0) / 1000;
    if delta < 60 {
        return "just now".into();
    }
    if delta < 3600 {
        return format!("{} min ago", delta / 60);
    }
    if delta < 6 * 3600 {
        return format!("{} h ago", delta / 3600);
    }
    use chrono::{Datelike, Local, TimeZone};
    let then = Local.timestamp_millis_opt(created_ms).single();
    let now = Local.timestamp_millis_opt(now_ms).single();
    match (then, now) {
        (Some(then), Some(now)) => {
            let days = now
                .date_naive()
                .signed_duration_since(then.date_naive())
                .num_days();
            if days == 0 {
                then.format("%-I:%M %p").to_string()
            } else if days == 1 {
                "Yesterday".into()
            } else if then.year() == now.year() {
                then.format("%b %-d").to_string()
            } else {
                then.format("%b %-d, %Y").to_string()
            }
        }
        _ => format!("{} d ago", delta / 86_400),
    }
}

/// Byte ranges in `text` where any of `terms` occurs, case-insensitively, for highlighting.
pub fn match_ranges(text: &str, terms: &[String]) -> Vec<std::ops::Range<usize>> {
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths for some scripts; only highlight when it does not.
    if lower.len() != text.len() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    for term in terms {
        let term = term.to_lowercase();
        if term.is_empty() {
            continue;
        }
        let mut start = 0;
        while let Some(pos) = lower[start..].find(&term) {
            let s = start + pos;
            ranges.push(s..s + term.len());
            start = s + term.len();
        }
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

pub fn kind_icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Text => "icons/text.svg",
        Kind::Link => "icons/link.svg",
        Kind::Image => "icons/image.svg",
        Kind::File => "icons/file.svg",
        Kind::Color => "icons/color.svg",
    }
}

/// `#RRGGBB` (the `#` optional) as `0xRRGGBB`.
pub fn parse_hex(hex: &str) -> Option<u32> {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok()
}

/// Near-black or white, whichever reads better on the color `0xRRGGBB`.
pub fn contrasting_text(color: u32) -> gpui::Hsla {
    let [_, r, g, b] = color.to_be_bytes();
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    if luma > 150.0 {
        hsla(0., 0., 0.1, 1.)
    } else {
        hsla(0., 0., 1., 1.)
    }
}

/// Everything a card needs beyond the item itself.
pub struct CardProps<'a> {
    pub theme: &'a Theme,
    /// The card's width in points (from the appearance setting).
    pub card_width: f32,
    pub selected: bool,
    /// The Quick Paste chord shown on the first nine cards, like "⌘1".
    pub keycap: Option<SharedString>,
    /// The Quick Paste modifier is held: every keycap lights up.
    pub keycaps_lit: bool,
    pub compact: bool,
    pub highlight_terms: &'a [String],
    pub thumb: Option<PathBuf>,
    pub collection_colors: Vec<gpui::Hsla>,
    pub now_ms: i64,
    pub flash: bool,
}

/// The card element without event handlers; the panel attaches those.
pub fn render_card(ix: usize, item: &Item, props: CardProps) -> Stateful<Div> {
    let theme = props.theme;
    let mut card = div()
        .id(("card", ix))
        .w(px(props.card_width))
        .h_full()
        .flex()
        .flex_col()
        .rounded(px(CARD_RADIUS))
        .overflow_hidden()
        .border_1()
        .border_color(if props.selected {
            theme.accent
        } else if props.flash {
            theme.accent.opacity(0.6)
        } else {
            theme.border
        })
        .when(props.selected, |d| d.bg(theme.card))
        .when(!props.selected, |d| d.hover(|s| s.bg(theme.card_hover)))
        .text_color(theme.text)
        .text_size(theme.scale(13.));
    if !props.compact {
        card = card.child(render_header(item, &props));
    }
    card = card.child(render_body(item, &props));
    card.child(render_footer(item, &props))
}

/// A keyboard key: accent-filled when `lit`.
pub fn keycap(label: SharedString, lit: bool, theme: &Theme) -> Div {
    div()
        .flex_shrink_0()
        .min_w(px(20.))
        .h(px(20.))
        .px(px(5.))
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::BOLD)
        .when(lit, |d| d.bg(theme.accent).text_color(theme.on_accent))
        .when(!lit, |d| d.bg(theme.field).text_color(theme.text_secondary))
        .child(label)
}

fn render_header(item: &Item, props: &CardProps) -> Div {
    let theme = props.theme;
    let mut header = div()
        .h(px(CARD_HEADER_HEIGHT))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .px(px(10.))
        .border_b_1()
        .border_color(theme.separator)
        .text_size(theme.scale(11.))
        .text_color(theme.text_secondary);
    if let Some(label) = props.keycap.clone() {
        header = header.child(keycap(label, props.keycaps_lit || props.selected, theme));
    }
    // A user-set name replaces the app name.
    let named = item.title.as_deref().filter(|t| !t.trim().is_empty());
    let label = match named {
        Some(title) => title.to_string(),
        None => item
            .source_app_name
            .as_deref()
            .unwrap_or_default()
            .to_lowercase(),
    };
    header
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .when(named.is_some(), |d| d.text_color(theme.text))
                .child(SharedString::from(label)),
        )
        .child(div().flex_shrink_0().child(item.kind.as_str()))
}

fn render_body(item: &Item, props: &CardProps) -> Div {
    let theme = props.theme;
    let body = div()
        .flex_1()
        .min_h(px(0.))
        .overflow_hidden()
        .px(px(12.))
        .py(px(10.));
    match item.kind {
        Kind::Text => {
            let preview = card_preview(item.text.as_deref().unwrap_or(""));
            let ranges = match_ranges(&preview, props.highlight_terms);
            let styled = StyledText::new(preview).with_highlights(ranges.into_iter().map(|r| {
                (
                    r,
                    HighlightStyle {
                        background_color: Some(theme.accent.opacity(0.35)),
                        ..Default::default()
                    },
                )
            }));
            body.line_clamp(12)
                .text_size(theme.scale(13.))
                .line_height(gpui::relative(1.5))
                .child(styled)
        }
        Kind::Link => {
            let url = item.text.clone().unwrap_or_default();
            let mut b = body.flex().flex_col().gap(px(6.));
            if let Some(thumb) = &props.thumb {
                b = b.child(
                    img(thumb.clone())
                        .w_full()
                        .h(px(84.))
                        .rounded(px(4.))
                        .object_fit(ObjectFit::Cover),
                );
            }
            let title = item.link_title.clone().or_else(|| item.link_domain.clone());
            if let Some(title) = title {
                b = b.child(
                    div()
                        .line_clamp(2)
                        .text_size(theme.scale(13.))
                        .child(SharedString::from(title)),
                );
            }
            b.child(
                div()
                    .line_clamp(3)
                    .text_size(theme.scale(12.))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(url)),
            )
        }
        Kind::Image => {
            let frame = div()
                .flex_1()
                .min_h(px(0.))
                .rounded(px(4.))
                .overflow_hidden()
                .bg(theme.field)
                .flex()
                .items_center()
                .justify_center();
            body.flex().child(match &props.thumb {
                Some(thumb) => frame.child(
                    img(thumb.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                ),
                None => frame.child(
                    svg()
                        .path("icons/image.svg")
                        .size(px(28.))
                        .text_color(theme.text_tertiary),
                ),
            })
        }
        Kind::File => {
            let mut b = body.flex().flex_col().gap(px(6.));
            if let Some(icon) = &props.thumb {
                b = b.child(
                    img(icon.clone())
                        .size(px(40.))
                        .object_fit(ObjectFit::Contain),
                );
            }
            for path in item.files.iter().take(6) {
                let name = crate::store::file_name(path).to_string();
                b = b.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .text_size(theme.scale(13.))
                        .child(
                            svg()
                                .path("icons/file.svg")
                                .size(px(14.))
                                .text_color(theme.text_secondary)
                                .flex_shrink_0(),
                        )
                        .child(div().truncate().child(SharedString::from(name))),
                );
            }
            if item.files.len() > 6 {
                b = b.child(
                    div()
                        .text_size(theme.scale(12.))
                        .text_color(theme.text_secondary)
                        .child(format!("+{} more", item.files.len() - 6)),
                );
            }
            b
        }
        Kind::Color => {
            let hex = item.text.clone().unwrap_or_default();
            let color = parse_hex(&hex).unwrap_or(0x808080);
            body.flex()
                .flex_col()
                .gap(px(10.))
                .child(div().flex_1().min_h(px(0.)).rounded(px(4.)).bg(rgb(color)))
                .child(
                    div()
                        .text_size(theme.scale(13.))
                        .child(SharedString::from(hex)),
                )
        }
    }
}

/// What a text card shows: the first 14 lines, at most 1200 characters. Built from the start of
/// the text, so a long clip costs no more per frame than a short one.
fn card_preview(text: &str) -> String {
    const LINES: usize = 14;
    const CHARS: usize = 1200;
    let mut chars = text
        .lines()
        .take(LINES)
        .enumerate()
        .flat_map(|(n, line)| (n > 0).then_some('\n').into_iter().chain(line.chars()))
        .peekable();
    let mut preview = String::new();
    let mut count = 0;
    while let Some(c) = chars.next() {
        count += 1;
        if count == CHARS && chars.peek().is_some() {
            preview.push('…');
            break;
        }
        preview.push(c);
    }
    preview
}

fn render_footer(item: &Item, props: &CardProps) -> Div {
    let theme = props.theme;
    let metric = match item.kind {
        Kind::Text => {
            let n = item.text.as_ref().map_or(0, |t| t.chars().count());
            if item.text_blob.is_some() {
                format!("{}+ chars", n)
            } else {
                format!("{n} chars")
            }
        }
        Kind::Link => item.link_domain.clone().unwrap_or_default(),
        Kind::Image => match (item.image_w, item.image_h) {
            (Some(w), Some(h)) => format!("{w}×{h}"),
            _ => String::new(),
        },
        Kind::File => {
            if item.files.len() == 1 {
                "1 file".into()
            } else {
                format!("{} files", item.files.len())
            }
        }
        Kind::Color => "Color".into(),
    };
    let mut left = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.))
        .child(SharedString::from(relative_time(
            item.created_at,
            props.now_ms,
        )));
    for color in &props.collection_colors {
        left = left.child(div().size(px(6.)).rounded_full().bg(*color));
    }
    div()
        .h(px(CARD_FOOTER_HEIGHT))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .px(px(10.))
        .text_size(theme.scale(11.))
        .text_color(theme.text_tertiary)
        .child(left)
        .child(div().truncate().child(SharedString::from(metric)))
}

// ---- strip element -------------------------------------------------------------------------

/// A horizontally scrolling row of fixed-width cards that only renders the visible ones.
/// Structured like GPUI's `uniform_list`, using the public `Interactivity` hooks for scrolling.
type RenderItem = Box<dyn Fn(usize, &mut Window, &mut App) -> AnyElement>;

pub struct CardStrip {
    interactivity: Interactivity,
    count: usize,
    render_item: RenderItem,
    scroll_handle: Option<ScrollHandle>,
    scroll_to: Option<usize>,
    card_width: Pixels,
    card_height: Pixels,
    rows: usize,
    gap: Pixels,
}

pub fn card_strip(
    id: impl Into<ElementId>,
    count: usize,
    render_item: impl 'static + Fn(usize, &mut Window, &mut App) -> AnyElement,
) -> CardStrip {
    let mut interactivity = Interactivity::new();
    interactivity.element_id = Some(id.into());
    interactivity.base_style.overflow.x = Some(Overflow::Scroll);
    CardStrip {
        interactivity,
        count,
        render_item: Box::new(render_item),
        scroll_handle: None,
        scroll_to: None,
        card_width: px(crate::settings::DEFAULT_CARD_WIDTH),
        card_height: px(crate::settings::DEFAULT_CARD_MAX_HEIGHT),
        rows: 1,
        gap: px(CARD_GAP),
    }
}

impl CardStrip {
    /// Tracks scrolling with `handle` (needed for `scroll_to_item` and end-of-list detection).
    pub fn with_scroll_handle(mut self, handle: &ScrollHandle) -> Self {
        self.scroll_handle = Some(handle.clone());
        <CardStrip as StatefulInteractiveElement>::track_scroll(self, handle)
    }

    /// Scrolls the minimum amount so card `ix` is fully visible on the next frame.
    pub fn scroll_to_item(mut self, ix: Option<usize>) -> Self {
        self.scroll_to = ix;
        self
    }

    /// Card size and row count (from the appearance settings). Cards keep this size and never
    /// stretch; a bar taller than `rows` cards shows empty space below them.
    pub fn layout(mut self, card_width: f32, card_height: f32, rows: usize) -> Self {
        self.card_width = px(card_width.max(120.));
        self.card_height = px(card_height.max(1.));
        self.rows = rows.max(1);
        self
    }

    pub fn stride(&self) -> Pixels {
        self.card_width + self.gap
    }
}

impl Styled for CardStrip {
    fn style(&mut self) -> &mut gpui::StyleRefinement {
        &mut self.interactivity.base_style
    }
}

impl InteractiveElement for CardStrip {
    fn interactivity(&mut self) -> &mut Interactivity {
        &mut self.interactivity
    }
}

impl StatefulInteractiveElement for CardStrip {}

impl IntoElement for CardStrip {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CardStrip {
    type RequestLayoutState = SmallVec<[AnyElement; 16]>;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        self.interactivity.element_id.clone()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout_id = self.interactivity.request_layout(
            global_id,
            inspector_id,
            window,
            cx,
            |style, window, cx| window.request_layout(style, None, cx),
        );
        (layout_id, SmallVec::new())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        items: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        let style = self
            .interactivity
            .compute_style(global_id, None, window, cx);
        let padding = style
            .padding
            .to_pixels(bounds.size.into(), window.rem_size());
        let padded = Bounds::from_corners(
            bounds.origin + point(padding.left, padding.top),
            bounds.bottom_right() - point(padding.right, padding.bottom),
        );
        let stride = self.stride();
        let count = self.count;
        let avail_h = bounds.size.height - padding.top - padding.bottom;
        let rows = self.rows;
        // Cards are top-aligned at their fixed height and only shrink when the bar is shorter.
        let row_height = self.card_height.min(avail_h).max(px(1.));
        // Cards flow column-major: column `c` holds items [c*rows, c*rows+rows).
        let cols = count.div_ceil(rows);
        let items_width = if cols == 0 {
            px(0.)
        } else {
            self.card_width * (cols as f32) + self.gap * ((cols - 1) as f32)
        };
        // GPUI adds the element's padding to the content size when clamping the scroll offset.
        let content_size = size(items_width, avail_h);

        if let (Some(target), Some(handle)) = (self.scroll_to.take(), self.scroll_handle.as_ref())
            && target < count
        {
            let col = target / rows;
            let item_left = stride * (col as f32);
            let item_right = item_left + self.card_width;
            let viewport = bounds.size.width - padding.left - padding.right;
            let offset = handle.offset();
            let current = -offset.x;
            let max_scroll =
                (content_size.width + padding.left + padding.right - bounds.size.width).max(px(0.));
            let wanted = if item_left < current {
                item_left
            } else if item_right > current + viewport {
                item_right - viewport
            } else {
                current
            };
            handle.set_offset(point(-wanted.clamp(px(0.), max_scroll), offset.y));
        }

        let card_width = self.card_width;
        self.interactivity.prepaint(
            global_id,
            inspector_id,
            bounds,
            content_size,
            window,
            cx,
            |_style, scroll_offset, hitbox, window, cx| {
                if count > 0 {
                    let scrolled = -scroll_offset.x;
                    let first_col =
                        ((scrolled / stride).floor().max(0.) as usize).saturating_sub(OVERSCAN);
                    let last_col = (((scrolled + bounds.size.width) / stride).ceil() as usize
                        + OVERSCAN)
                        .min(cols);
                    let content_mask = ContentMask { bounds };
                    window.with_content_mask(Some(content_mask), |window| {
                        for col in first_col..last_col {
                            for r in 0..rows {
                                let ix = col * rows + r;
                                if ix >= count {
                                    break;
                                }
                                let mut item = (self.render_item)(ix, window, cx);
                                let origin = padded.origin
                                    + scroll_offset
                                    + point(
                                        stride * (col as f32),
                                        (row_height + self.gap) * (r as f32),
                                    );
                                let available = size(
                                    AvailableSpace::Definite(card_width),
                                    AvailableSpace::Definite(row_height),
                                );
                                item.layout_as_root(available, window, cx);
                                item.prepaint_at(origin, window, cx);
                                items.push(item);
                            }
                        }
                    });
                }
                hitbox
            },
        )
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        items: &mut Self::RequestLayoutState,
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.interactivity.paint(
            global_id,
            inspector_id,
            bounds,
            hitbox.as_ref(),
            window,
            cx,
            |_, window, cx| {
                for item in items.iter_mut() {
                    item.paint(window, cx);
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        use chrono::{Local, TimeZone};
        let at = |y, m, d, h| {
            Local
                .with_ymd_and_hms(y, m, d, h, 0, 0)
                .unwrap()
                .timestamp_millis()
        };
        let now = at(2026, 6, 15, 18);
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now + 5_000, now), "just now");
        assert_eq!(relative_time(now - 90_000, now), "1 min ago");
        assert_eq!(relative_time(now - 2 * 3_600_000, now), "2 h ago");
        assert_eq!(relative_time(at(2026, 6, 15, 9), now), "9:00 AM");
        assert_eq!(relative_time(at(2026, 6, 14, 20), now), "Yesterday");
        assert_eq!(relative_time(at(2026, 3, 2, 12), now), "Mar 2");
        assert_eq!(relative_time(at(2025, 12, 31, 12), now), "Dec 31, 2025");
    }

    #[test]
    fn highlight_ranges_merge_and_ignore_case() {
        let text = "The Quick brown quick fox";
        let ranges = match_ranges(text, &["quick".into(), "ick b".into()]);
        assert_eq!(ranges, vec![4..11, 16..21]);
        assert!(match_ranges(text, &[]).is_empty());
        assert!(match_ranges("", &["x".into()]).is_empty());
    }

    #[test]
    fn colors_parse_and_contrast() {
        assert_eq!(parse_hex("#FFFFFF"), Some(0xFFFFFF));
        assert_eq!(parse_hex("000000"), Some(0));
        assert_eq!(parse_hex("#FFF"), None);
        assert!(contrasting_text(0xFFFFFF).l < 0.5);
        assert!(contrasting_text(0x000000).l > 0.5);
    }

    #[test]
    fn card_preview_is_bounded() {
        assert_eq!(card_preview("a\nb"), "a\nb");
        let lines: String = (0..30).map(|i| format!("{i}\n")).collect();
        assert_eq!(card_preview(&lines).lines().count(), 14);
        let long = "x".repeat(5000);
        let preview = card_preview(&long);
        assert_eq!(preview.chars().count(), 1200);
        assert!(preview.ends_with('…'));
        let exact = "y".repeat(1200);
        assert_eq!(card_preview(&exact), exact);
    }
}
