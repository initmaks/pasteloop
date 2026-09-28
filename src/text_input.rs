//! A GPUI text input, single line (search field, rename) or multi line (edit and new text item),
//! following the pattern of GPUI's `examples/input.rs`. Implements `EntityInputHandler` so IME
//! composition (Japanese, Chinese, dead keys) and the character palette work.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill, font, point,
    prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        LineStart,
        LineEnd,
        Newline,
        Submit,
        Cancel,
        TabForward,
        Paste,
        Cut,
        Copy,
        ShowCharacterPalette,
    ]
);

pub const KEY_CONTEXT: &str = "TextInput";
const FONT_SIZE: Pixels = px(13.);

/// Bindings scoped to the `TextInput` context. Registered once at startup.
pub fn key_bindings() -> Vec<KeyBinding> {
    let c = Some(KEY_CONTEXT);
    vec![
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("home", LineStart, c),
        KeyBinding::new("end", LineEnd, c),
        KeyBinding::new("ctrl-a", LineStart, c),
        KeyBinding::new("ctrl-e", LineEnd, c),
        KeyBinding::new("enter", Submit, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("cmd-enter", Submit, c),
        KeyBinding::new("escape", Cancel, c),
        KeyBinding::new("tab", TabForward, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, c),
    ]
}

/// What the owner of the field needs to know. `Changed` fires on every edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Changed,
    Submit,
    Cancel,
    Tab,
    /// The field wrote the general pasteboard (Cmd-C or Cmd-X).
    CopiedToClipboard,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputColors {
    pub text: Hsla,
    pub placeholder: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
}

impl Default for InputColors {
    fn default() -> Self {
        InputColors {
            text: gpui::black(),
            placeholder: gpui::black().opacity(0.4),
            cursor: gpui::blue(),
            selection: gpui::blue().opacity(0.25),
        }
    }
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    multiline: bool,
    monospace: bool,
    pub colors: InputColors,
    /// Paragraph layouts from the last paint, one per `\n`-separated line, with their y offsets.
    last_layout: Vec<(WrappedLine, Pixels)>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Pixels,
    is_selecting: bool,
    /// Remembered x for Up/Down so the caret does not drift on short lines.
    preferred_x: Option<Pixels>,
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        multiline: bool,
    ) -> Self {
        TextInput {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            multiline,
            monospace: false,
            colors: InputColors::default(),
            last_layout: Vec::new(),
            last_bounds: None,
            last_line_height: px(18.),
            is_selecting: false,
            preferred_x: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.preferred_x = None;
        cx.notify();
    }

    pub fn set_monospace(&mut self, mono: bool, cx: &mut Context<Self>) {
        self.monospace = mono;
        cx.notify();
    }

    pub fn monospace(&self) -> bool {
        self.monospace
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Inserts `text` at the caret (replacing the selection), as typing would.
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, text, window, cx);
    }

    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        cx.notify();
    }

    // ---- actions -------------------------------------------------------------------------------

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            cx.propagate();
            return;
        }
        if let Some(ix) = self.vertical_target(-1) {
            self.move_to(ix, cx);
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            cx.propagate();
            return;
        }
        if let Some(ix) = self.vertical_target(1) {
            self.move_to(ix, cx);
        }
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.vertical_target(-1) {
            self.select_to(ix, cx);
        }
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.vertical_target(1) {
            self.select_to(ix, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let cursor = self.cursor_offset();
        let start = self.content[..cursor].rfind('\n').map_or(0, |i| i + 1);
        self.move_to(start, cx);
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let cursor = self.cursor_offset();
        let end = self.content[cursor..]
            .find('\n')
            .map_or(self.content.len(), |i| cursor + i);
        self.move_to(end, cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submit);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Cancel);
    }

    fn tab_forward(&mut self, _: &TabForward, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Tab);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            // Nothing selected in the field: let the panel copy the selected card instead.
            cx.propagate();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.content[self.selected_range.clone()].to_string(),
        ));
        cx.emit(InputEvent::CopiedToClipboard);
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.content[self.selected_range.clone()].to_string(),
        ));
        cx.emit(InputEvent::CopiedToClipboard);
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        self.preferred_x = None;
        let ix = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(ix, cx);
        } else if event.click_count == 2 {
            let (start, end) = self.word_bounds(ix);
            self.selected_range = start..end;
            self.selection_reversed = false;
            cx.notify();
        } else if event.click_count >= 3 {
            self.selected_range = 0..self.content.len();
            cx.notify();
        } else {
            self.move_to(ix, cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.preferred_x = None;
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    // ---- cursor arithmetic ---------------------------------------------------------------------

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn word_bounds(&self, ix: usize) -> (usize, usize) {
        let mut start = 0;
        let mut end = self.content.len();
        // A click past the end selects the last word, not the whole field.
        let ix = ix.min(self.content.len().saturating_sub(1));
        for (offset, word) in self.content.split_word_bound_indices() {
            if offset <= ix && ix < offset + word.len() {
                start = offset;
                end = offset + word.len();
                break;
            }
        }
        (start, end)
    }

    /// The caret position for `index` in the last layout, relative to the element origin.
    fn position_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        caret_position(&self.last_layout, self.last_line_height, index)
    }

    fn index_for_point(&self, local: Point<Pixels>) -> usize {
        if self.content.is_empty() || self.last_layout.is_empty() {
            return 0;
        }
        let mut paragraph_start = 0;
        for (i, (line, y)) in self.last_layout.iter().enumerate() {
            let height = line.size(self.last_line_height).height;
            let is_last = i + 1 == self.last_layout.len();
            if local.y < *y + height || is_last {
                let p = point(local.x, (local.y - *y).max(px(0.)));
                let local_ix = match line.closest_index_for_position(p, self.last_line_height) {
                    Ok(ix) | Err(ix) => ix,
                };
                return paragraph_start + local_ix.min(line.len());
            }
            paragraph_start += line.len() + 1;
        }
        self.content.len()
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.index_for_point(position - bounds.origin)
    }

    fn vertical_target(&mut self, direction: i32) -> Option<usize> {
        let cursor = self.cursor_offset();
        let pos = self.position_for_index(cursor)?;
        let x = *self.preferred_x.get_or_insert(pos.x);
        let target_y =
            pos.y + self.last_line_height * (direction as f32) + self.last_line_height / 2.;
        if target_y < px(0.) {
            return Some(0);
        }
        let total: Pixels = self
            .last_layout
            .iter()
            .map(|(l, _)| l.size(self.last_line_height).height)
            .sum();
        if target_y > total {
            return Some(self.content.len());
        }
        Some(self.index_for_point(point(x, target_y)))
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        utf16_to_utf8_within(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

/// The caret position for byte `index` in `lines` (one per paragraph, with its y offset).
fn caret_position(
    lines: &[(WrappedLine, Pixels)],
    line_height: Pixels,
    index: usize,
) -> Option<Point<Pixels>> {
    let mut paragraph_start = 0;
    for (line, y) in lines {
        let paragraph_end = paragraph_start + line.len();
        if index >= paragraph_start && index <= paragraph_end {
            let local = line.position_for_index(index - paragraph_start, line_height)?;
            return Some(point(local.x, local.y + *y));
        }
        paragraph_start = paragraph_end + 1;
    }
    None
}

/// UTF-16 offset to UTF-8 offset inside `text`, clamped to its length.
fn utf16_to_utf8_within(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= utf16 {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        self.content.get(range).map(|s| s.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = if self.multiline {
            new_text.to_string()
        } else {
            new_text.replace(['\n', '\r'], " ")
        };
        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.preferred_x = None;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        // The selection macOS hands us is relative to the marked text, in UTF-16 units.
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| {
                let start = utf16_to_utf8_within(new_text, r.start);
                let end = utf16_to_utf8_within(new_text, r.end);
                range.start + start..range.start + end
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let start = self.position_for_index(range.start)?;
        let end = self.position_for_index(range.end).unwrap_or(start);
        Some(Bounds::from_corners(
            bounds.origin + start,
            bounds.origin + point(end.x.max(start.x + px(1.)), end.y + self.last_line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let utf8_index = self.index_for_point(point - bounds.origin);
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    lines: Vec<(WrappedLine, Pixels)>,
    line_height: Pixels,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl TextElement {
    fn shape(
        &self,
        width: Option<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> (Vec<(WrappedLine, Pixels)>, Pixels) {
        let input = self.input.read(cx);
        let style = window.text_style();
        let (display_text, text_color) = if input.content.is_empty() {
            (input.placeholder.clone(), input.colors.placeholder)
        } else {
            (input.content.clone(), input.colors.text)
        };
        let mut base_font = style.font();
        if input.monospace {
            base_font = font("Menlo");
        }
        let run = TextRun {
            len: display_text.len(),
            font: base_font,
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = input
            .marked_range
            .as_ref()
            .filter(|_| !input.content.is_empty())
        {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };
        let line_height = window.line_height();
        let wrap = if input.multiline { width } else { None };
        let shaped = window
            .text_system()
            .shape_text(display_text, FONT_SIZE, &runs, wrap, None)
            .unwrap_or_default();
        let mut y = px(0.);
        let mut lines = Vec::with_capacity(shaped.len());
        for line in shaped {
            let height = line.size(line_height).height;
            lines.push((line, y));
            y += height;
        }
        (lines, line_height)
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let multiline = self.input.read(cx).multiline;
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        if multiline {
            let input = self.input.clone();
            let layout =
                window.request_measured_layout(style, move |known, available, window, cx| {
                    let width = known.width.or(match available.width {
                        gpui::AvailableSpace::Definite(w) => Some(w),
                        _ => None,
                    });
                    let element = TextElement {
                        input: input.clone(),
                    };
                    let (lines, line_height) = element.shape(width, window, cx);
                    let height: Pixels = lines
                        .last()
                        .map_or(line_height, |(l, y)| *y + l.size(line_height).height);
                    size(width.unwrap_or(px(0.)), height.max(line_height))
                });
            (layout, ())
        } else {
            style.size.height = window.line_height().into();
            (window.request_layout(style, [], cx), ())
        }
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (lines, line_height) = self.shape(Some(bounds.size.width), window, cx);
        let input = self.input.read(cx);
        let colors = input.colors.clone();
        let selected = input.selected_range.clone();
        let cursor_ix = input.cursor_offset();
        let empty = input.content.is_empty();

        // Caret and selection come from the freshly shaped lines, not the last frame's.
        let cursor_pos = if empty {
            Some(point(px(0.), px(0.)))
        } else {
            caret_position(&lines, line_height, cursor_ix)
        };
        let cursor = cursor_pos.map(|p| {
            fill(
                Bounds::new(bounds.origin + point(p.x, p.y), size(px(2.), line_height)),
                colors.cursor,
            )
        });
        let mut selection = Vec::new();
        if !selected.is_empty() && !empty {
            // One quad per visual row the selection touches.
            let mut paragraph_start = 0;
            for (line, y) in &lines {
                let paragraph_end = paragraph_start + line.len();
                let sel_start = selected.start.max(paragraph_start);
                let sel_end = selected.end.min(paragraph_end);
                if sel_start <= sel_end
                    && selected.start <= paragraph_end
                    && selected.end >= paragraph_start
                {
                    let rows = line.wrap_boundaries().len() + 1;
                    let start_p = line.position_for_index(sel_start - paragraph_start, line_height);
                    let end_p = line.position_for_index(sel_end - paragraph_start, line_height);
                    if let (Some(s), Some(e)) = (start_p, end_p) {
                        let start_row = (s.y / line_height).floor() as usize;
                        let end_row = (e.y / line_height).floor() as usize;
                        for row in start_row..=end_row.min(rows - 1) {
                            let x0 = if row == start_row { s.x } else { px(0.) };
                            let x1 = if row == end_row { e.x } else { line.width() };
                            let row_y = *y + line_height * (row as f32);
                            let width = (x1 - x0).max(px(2.));
                            selection.push(fill(
                                Bounds::new(
                                    bounds.origin + point(x0, row_y),
                                    size(width, line_height),
                                ),
                                colors.selection,
                            ));
                        }
                    }
                }
                paragraph_start = paragraph_end + 1;
            }
        }
        PrepaintState {
            lines,
            line_height,
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }
        let lines = std::mem::take(&mut prepaint.lines);
        for (line, y) in &lines {
            let _ = line.paint(
                bounds.origin + point(px(0.), *y),
                prepaint.line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        let line_height = prepaint.line_height;
        self.input.update(cx, |input, _| {
            input.last_layout = lines;
            input.last_bounds = Some(bounds);
            input.last_line_height = line_height;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::tab_forward))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .when(self.multiline, |d| d.h_full())
            .text_size(FONT_SIZE)
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::utf16_to_utf8_within;

    #[test]
    fn utf16_offsets_map_inside_marked_text() {
        // "かな" is two 3-byte chars, one UTF-16 unit each.
        assert_eq!(utf16_to_utf8_within("かな", 0), 0);
        assert_eq!(utf16_to_utf8_within("かな", 1), 3);
        assert_eq!(utf16_to_utf8_within("かな", 2), 6);
        assert_eq!(utf16_to_utf8_within("かな", 9), 6);
        // A surrogate pair counts as two UTF-16 units and four bytes.
        assert_eq!(utf16_to_utf8_within("😀a", 2), 4);
    }
}
