//! All Objective-C and Apple-framework glue. Every `unsafe` block in the application lives in
//! this file, each with a `// SAFETY:` comment. Nothing here knows about the store or the panel
//! state; it only translates between AppKit and plain Rust values.

use std::ffi::c_void;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use gpui::Window;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDidChangeScreenParametersNotification, NSBitmapImageFileType, NSBitmapImageRep,
    NSDraggingItem, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSImage, NSMenu,
    NSMenuItem, NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardTypeHTML,
    NSPasteboardTypePNG, NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardTypeTIFF,
    NSPasteboardTypeURL, NSPasteboardWriting, NSRunningApplication, NSScreen, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength, NSView, NSWindow, NSWindowSharingType, NSWorkspace,
};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID};
use objc2_foundation::{
    NSData, NSDictionary, NSNotificationCenter, NSNumber, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tracing::{debug, warn};

use crate::clipboard::{self, Snapshot};
use crate::paste::Representations;

/// Virtual key code of the `V` key on every layout (`kVK_ANSI_V`).
const VK_V: u16 = 9;

/// The marker objc2 requires for main-thread-only AppKit calls. GPUI runs all of our code on the
/// main thread except the executors we explicitly hand work to, so this only fails on misuse.
pub fn main_thread() -> MainThreadMarker {
    MainThreadMarker::new().expect("AppKit calls must run on the main thread")
}

/// Turns the process into a menu-bar-only application. GPUI's app delegate forces the `Regular`
/// policy in `applicationDidFinishLaunching`, so this must run inside the `run` callback.
pub fn set_accessory_activation_policy() {
    let app = NSApplication::sharedApplication(main_thread());
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

/// `NSApp.activate`, for windows that must become key (settings). Cooperative activation on
/// macOS 14+ may ignore it unless the user just interacted with us, which is the case here.
#[allow(deprecated)] // `activate()` is ignored for a non-frontmost app under cooperative activation
pub fn activate_app() {
    NSApplication::sharedApplication(main_thread()).activateIgnoringOtherApps(true);
}

/// Returns the heap's free pages to the system, which the allocator otherwise keeps around.
pub fn release_free_memory() {
    unsafe extern "C" {
        fn malloc_zone_pressure_relief(zone: *mut c_void, goal: usize) -> usize;
    }
    // SAFETY: a null zone means every zone and a goal of 0 means as much as possible, per
    // malloc/malloc.h; the call only trims free pages.
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

// ---- windows -------------------------------------------------------------------------------

/// The `NSWindow` behind a GPUI window. `None` only if the window has been closed.
pub fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    // SAFETY: GPUI hands out the pointer to its live `NSView` for this window, and the window
    // outlives this call because we hold `&Window`.
    let view: &NSView = unsafe { &*(appkit.ns_view.as_ptr() as *const NSView) };
    view.window()
}

/// The `NSWindow` behind a GPUI window, usable from foreground tasks. Ordering a window in or
/// out synchronously inside a GPUI update re-enters GPUI through AppKit's key-window
/// notifications ("RefCell already borrowed"), so every show, hide, and frame change goes
/// through this handle from a spawned task, exactly as GPUI's own `activate_window` does.
#[derive(Clone)]
pub struct NativeWindow(Retained<NSWindow>);

impl NativeWindow {
    pub fn of(window: &Window) -> Option<Self> {
        ns_window(window).map(NativeWindow)
    }

    /// One-time setup for the popup panel that GPUI's `WindowKind::PopUp` does not cover: the
    /// panel must stay put when the app deactivates, become key whenever we ask, and stay out
    /// of screen recordings when the privacy setting says so.
    pub fn configure_panel(&self, hide_from_capture: bool) {
        self.0.setHidesOnDeactivate(false);
        self.set_hidden_from_capture(hide_from_capture);
        // SAFETY: GPUI creates PopUp windows from its NSPanel subclass, so the selector exists;
        // the argument is a plain BOOL.
        let _: () = unsafe { msg_send![&*self.0, setBecomesKeyOnlyIfNeeded: false] };
    }

    pub fn set_hidden_from_capture(&self, hidden: bool) {
        let sharing = if hidden {
            NSWindowSharingType::None
        } else {
            NSWindowSharingType::ReadOnly
        };
        self.0.setSharingType(sharing);
    }

    /// Moves the window to `frame` (AppKit screen coordinates, origin bottom-left) and makes it
    /// key without activating the application.
    pub fn show_at(&self, frame: NSRect) {
        if !rects_equal(self.0.frame(), frame) {
            self.0.setFrame_display(frame, true);
        }
        self.0.makeKeyAndOrderFront(None);
    }

    /// Orders the window in at `frame` without making it key, so the window that has keyboard
    /// focus (the panel) keeps it. `orderFrontRegardless`, because AppKit ignores a plain
    /// `orderFront:` from a background accessory app.
    pub fn order_front(&self, frame: NSRect) {
        if !rects_equal(self.0.frame(), frame) {
            self.0.setFrame_display(frame, true);
        }
        self.0.orderFrontRegardless();
    }

    pub fn order_out(&self) {
        self.0.orderOut(None);
    }

    /// Orders the window out, transparent and shrunk to a point. Transparent, so the next showing
    /// does not flash the frame it last drew (the caller makes it opaque once a fresh frame is
    /// drawn); shrunk, so its full-size drawing surfaces (a full-width bar is tens of megabytes,
    /// triple-buffered) are freed while it is hidden. Every showing sets the frame again.
    pub fn order_out_transparent(&self) {
        self.0.orderOut(None);
        self.0.setAlphaValue(0.0);
        let origin = self.0.frame().origin;
        self.0
            .setFrame_display(NSRect::new(origin, NSSize::new(1.0, 1.0)), false);
    }

    pub fn set_alpha(&self, alpha: f64) {
        self.0.setAlphaValue(alpha);
    }

    /// Makes the window opaque if it is not yet; returns whether it had to.
    pub fn reveal(&self) -> bool {
        let hidden = self.0.alphaValue() < 1.0;
        if hidden {
            self.0.setAlphaValue(1.0);
        }
        hidden
    }

    pub fn set_frame(&self, frame: NSRect) {
        self.0.setFrame_display(frame, true);
    }

    pub fn frame(&self) -> NSRect {
        self.0.frame()
    }

    /// Device pixels per point on the window's display (2 on Retina).
    pub fn backing_scale(&self) -> f64 {
        self.0.backingScaleFactor()
    }

    /// The visible frame (without menu bar and Dock) of the window's display.
    pub fn screen_visible_frame(&self) -> NSRect {
        self.0
            .screen()
            .map(|screen| screen.visibleFrame())
            .unwrap_or_else(visible_frame_under_mouse)
    }
}

fn rects_equal(a: NSRect, b: NSRect) -> bool {
    (a.origin.x - b.origin.x).abs() < 0.5
        && (a.origin.y - b.origin.y).abs() < 0.5
        && (a.size.width - b.size.width).abs() < 0.5
        && (a.size.height - b.size.height).abs() < 0.5
}

/// A rectangle of `height` points anchored to the bottom of the visible frame (above the Dock)
/// of the screen under the mouse pointer, spanning its full visible width. Also returns the
/// height of that screen's visible area so the caller can clamp the panel height.
pub fn panel_frame_under_mouse(height: f64) -> (NSRect, f64) {
    let visible = visible_frame_under_mouse();
    let height = height.min(visible.size.height);
    (
        NSRect::new(
            NSPoint::new(visible.origin.x, visible.origin.y),
            NSSize::new(visible.size.width, height),
        ),
        visible.size.height,
    )
}

/// The visible frame (without menu bar and Dock) of the screen under the mouse, else the main
/// screen's.
fn visible_frame_under_mouse() -> NSRect {
    let mtm = main_thread();
    let mouse = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    screens
        .iter()
        .find(|screen| rect_contains(screen.frame(), mouse))
        .or_else(|| NSScreen::mainScreen(mtm))
        .or_else(|| screens.iter().next())
        .map(|s| s.visibleFrame())
        .unwrap_or_else(|| NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)))
}

fn rect_contains(rect: NSRect, p: NSPoint) -> bool {
    p.x >= rect.origin.x
        && p.x < rect.origin.x + rect.size.width
        && p.y >= rect.origin.y
        && p.y < rect.origin.y + rect.size.height
}

/// Top-right corner of the screen under the mouse, for the Paste Queue window.
pub fn queue_frame_under_mouse(width: f64, height: f64) -> NSRect {
    let visible = visible_frame_under_mouse();
    NSRect::new(
        NSPoint::new(
            visible.origin.x + visible.size.width - width - 16.0,
            visible.origin.y + visible.size.height - height - 16.0,
        ),
        NSSize::new(width, height),
    )
}

/// Watches mouse-downs in other applications (clicks outside every window of ours) and sends a
/// message for each. Unlike losing key status, this also catches clicks in the app the panel
/// pastes into, which stays active while our non-activating panel is up. Mouse monitors need no
/// Accessibility permission. Stops when dropped.
pub struct OutsideClickMonitor(Retained<AnyObject>);

impl OutsideClickMonitor {
    pub fn start(tx: UnboundedSender<()>) -> Option<OutsideClickMonitor> {
        let mask =
            NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::OtherMouseDown;
        let handler = block2::RcBlock::new(move |_: std::ptr::NonNull<NSEvent>| {
            let _ = tx.unbounded_send(());
        });
        NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &handler)
            .map(OutsideClickMonitor)
    }
}

impl Drop for OutsideClickMonitor {
    fn drop(&mut self) {
        // SAFETY: the object came from `addGlobalMonitorForEventsMatchingMask:handler:` and is
        // removed once.
        unsafe { NSEvent::removeMonitor(&self.0) };
    }
}

/// The mouse position in AppKit screen coordinates (origin bottom-left).
pub fn mouse_location() -> (f64, f64) {
    let p = NSEvent::mouseLocation();
    (p.x, p.y)
}

// ---- applications --------------------------------------------------------------------------

/// The application that owns the keyboard right now: the one a paste should land in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontApp {
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub name: Option<String>,
}

pub fn frontmost_app() -> Option<FrontApp> {
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    Some(FrontApp {
        pid: app.processIdentifier(),
        bundle_id: app.bundleIdentifier().map(|s| s.to_string()),
        name: app.localizedName().map(|s| s.to_string()),
    })
}

pub fn pid_is_running(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|a| !a.isTerminated())
}

/// Asks macOS to bring `pid` to the front. Returns whether the request was made; whether it
/// took effect only shows up on a later run-loop turn, so callers that need it wait on a timer
/// instead of polling here (sleeping the main thread would stall the very run loop that
/// delivers the activation).
pub fn activate_pid(pid: i32) -> bool {
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return false;
    };
    if app.isActive() {
        return true;
    }
    // macOS 14 replaced "ignoring other apps" with cooperative activation: the active app (us)
    // yields, then the target may activate.
    NSApplication::sharedApplication(main_thread()).yieldActivationToApplication(&app);
    app.activateWithOptions(NSApplicationActivationOptions::empty());
    true
}

pub fn pid_is_active(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_some_and(|a| a.isActive())
}

/// Whether the left mouse button is down right now (for drags that leave the window).
pub fn left_mouse_button_down() -> bool {
    NSEvent::pressedMouseButtons() & 1 == 1
}

/// Finder's icon for a file or folder path at about `size` px.
pub fn file_icon_png(path: &str, size: f64) -> Option<Vec<u8>> {
    let icon = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path));
    image_png_at(&icon, size)
}

fn image_png_at(image: &NSImage, size: f64) -> Option<Vec<u8>> {
    image.setSize(NSSize::new(size, size));
    let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size, size));
    // SAFETY: `rect` is a valid pointer for the call; no context or hints are passed.
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    png_bytes(&NSBitmapImageRep::initWithCGImage(
        NSBitmapImageRep::alloc(),
        &cg,
    ))
}

fn png_bytes(rep: &NSBitmapImageRep) -> Option<Vec<u8>> {
    let properties = NSDictionary::new();
    // SAFETY: the properties dictionary is empty, so its generic type is trivially correct.
    let png =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }?;
    Some(png.to_vec())
}

pub fn open_url(url: &str) -> bool {
    match NSURL::URLWithString(&NSString::from_str(url)) {
        Some(url) => NSWorkspace::sharedWorkspace().openURL(&url),
        None => false,
    }
}

pub fn open_path(path: &str) -> bool {
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    NSWorkspace::sharedWorkspace().openURL(&url)
}

pub fn open_accessibility_settings() {
    open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

pub fn reduce_motion() -> bool {
    let workspace = NSWorkspace::sharedWorkspace();
    // SAFETY: `accessibilityDisplayShouldReduceMotion` is a BOOL property of NSWorkspace since
    // macOS 10.12.
    unsafe { msg_send![&*workspace, accessibilityDisplayShouldReduceMotion] }
}

// ---- pasteboard ----------------------------------------------------------------------------

pub fn pasteboard_change_count() -> i64 {
    NSPasteboard::generalPasteboard().changeCount() as i64
}

/// The general pasteboard's change count and type list, which is all the skip rules need.
pub fn pasteboard_types() -> (i64, Vec<String>) {
    let pasteboard = NSPasteboard::generalPasteboard();
    // The count first: if a copy lands before the types are read, the recheck after the data
    // read in `read_pasteboard` sees the count move and drops this read.
    let change_count = pasteboard.changeCount() as i64;
    let types = pasteboard
        .types()
        .map(|t| t.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    (change_count, types)
}

/// Reads the data the classifier will use: file URLs, else the image when the types make an
/// image (see [`clipboard::image_wins`]), else plain text, RTF, and HTML. Runs on a background
/// thread, because promised data makes the source app render it while we wait. Returns `None`
/// when a newer copy replaced the pasteboard during the read.
pub fn read_pasteboard(change_count: i64, types: Vec<String>) -> Option<Snapshot> {
    objc2::rc::autoreleasepool(|_| {
        let pasteboard = NSPasteboard::generalPasteboard();
        let mut snapshot = read_pasteboard_data(&pasteboard, types);
        snapshot.change_count = change_count;
        (pasteboard.changeCount() as i64 == change_count).then_some(snapshot)
    })
}

fn read_pasteboard_data(pasteboard: &NSPasteboard, types: Vec<String>) -> Snapshot {
    let has = |t: &str| types.iter().any(|x| x == t);
    let mut snapshot = Snapshot::default();
    let data_for = |name: &str| -> Option<Vec<u8>> {
        pasteboard
            .dataForType(&NSString::from_str(name))
            .map(|d| d.to_vec())
    };
    if has(clipboard::UTI_FILE_URL) {
        // SAFETY: reading a constant exported by AppKit.
        let file_url_type = unsafe { NSPasteboardTypeFileURL };
        for item in pasteboard.pasteboardItems().iter().flat_map(|a| a.iter()) {
            if let Some(s) = item.stringForType(file_url_type)
                && let Some(url) = NSURL::URLWithString(&s)
                && let Some(path) = url.path()
            {
                snapshot.file_paths.push(path.to_string());
            }
        }
    } else if clipboard::image_wins(&types) {
        let uti = [
            clipboard::UTI_PNG,
            clipboard::UTI_JPEG,
            clipboard::UTI_HEIC,
            clipboard::UTI_TIFF,
        ]
        .into_iter()
        .find(|t| has(t));
        snapshot.image = match uti {
            Some(uti) => data_for(uti).map(|d| (d, uti)),
            // Any other raster type ImageIO can read (GIF, WebP, BMP, JPEG 2000, ...).
            None => types
                .iter()
                .find(|t| clipboard::is_image_type(t))
                .and_then(|t| data_for(t))
                .map(|d| (d, "public.image")),
        };
    } else {
        // SAFETY: reading a constant exported by AppKit.
        let string_t = unsafe { NSPasteboardTypeString };
        snapshot.text = pasteboard
            .stringForType(string_t)
            .or_else(|| pasteboard.stringForType(&NSString::from_str("NSStringPboardType")))
            .map(|s| s.to_string());
        if has(clipboard::UTI_RTF) {
            snapshot.rtf = data_for(clipboard::UTI_RTF);
        }
        if has(clipboard::UTI_HTML) {
            snapshot.html = data_for(clipboard::UTI_HTML);
        }
    }
    snapshot.types = types;
    snapshot
}

/// Replaces the pasteboard with `reps` and returns the new change count so the poller can
/// recognize our own write.
pub fn write_pasteboard(reps: &Representations) -> i64 {
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // SAFETY: reading constants exported by AppKit.
    let (png_t, tiff_t, file_url_t) = unsafe {
        (
            NSPasteboardTypePNG,
            NSPasteboardTypeTIFF,
            NSPasteboardTypeFileURL,
        )
    };
    let mut objects: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::new();
    if !reps.file_paths.is_empty() {
        for path in &reps.file_paths {
            let item = NSPasteboardItem::new();
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            if let Some(s) = url.absoluteString() {
                item.setString_forType(&s, file_url_t);
            }
            objects.push(ProtocolObject::from_retained(item));
        }
    } else {
        let item = text_pasteboard_item(reps);
        if let Some(png) = &reps.image_png {
            let data = NSData::with_bytes(png);
            item.setData_forType(&data, png_t);
            // Older apps only read TIFF.
            if let Some(rep) = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
                && let Some(tiff) = rep.TIFFRepresentation()
            {
                item.setData_forType(&tiff, tiff_t);
            }
        }
        if let Some(file) = &reps.image_file {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&file.to_string_lossy()));
            if let Some(s) = url.absoluteString() {
                item.setString_forType(&s, file_url_t);
            }
        }
        objects.push(ProtocolObject::from_retained(item));
    }
    let array = objc2_foundation::NSArray::from_retained_slice(&objects);
    if !pasteboard.writeObjects(&array) {
        warn!("pasteboard writeObjects failed");
    }
    pasteboard.changeCount() as i64
}

/// A pasteboard item with the text-like representations: plain text, RTF, HTML, and URL.
fn text_pasteboard_item(reps: &Representations) -> Retained<NSPasteboardItem> {
    // SAFETY: reading constants exported by AppKit.
    let (string_t, rtf_t, html_t, url_t) = unsafe {
        (
            NSPasteboardTypeString,
            NSPasteboardTypeRTF,
            NSPasteboardTypeHTML,
            NSPasteboardTypeURL,
        )
    };
    let item = NSPasteboardItem::new();
    if let Some(plain) = &reps.plain {
        item.setString_forType(&NSString::from_str(plain), string_t);
    }
    if let Some(rtf) = &reps.rtf {
        item.setData_forType(&NSData::with_bytes(rtf), rtf_t);
    }
    if let Some(html) = &reps.html {
        item.setData_forType(&NSData::with_bytes(html), html_t);
    }
    if let Some(url) = &reps.url {
        item.setString_forType(&NSString::from_str(url), url_t);
    }
    item
}

/// Starts a native drag of text-like representations from the GPUI view, mirroring GPUI's own
/// file drag: one `NSDraggingItem` around a pasteboard item, sourced by the window. Called when
/// a GPUI drag leaves the panel; returns false when there is no current mouse event to anchor.
pub fn begin_text_drag(window: &Window, reps: &Representations) -> bool {
    let mtm = main_thread();
    let Some(ns_window) = ns_window(window) else {
        return false;
    };
    let Some(view) = ns_window.contentView() else {
        return false;
    };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
        return false;
    };
    let item = text_pasteboard_item(reps);
    let writer = ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&*item);
    let dragging_item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
    let location = event.locationInWindow();
    let frame = NSRect::new(
        NSPoint::new(location.x - 16.0, location.y - 16.0),
        NSSize::new(32.0, 32.0),
    );
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str("doc.text"),
        None,
    );
    // SAFETY: `contents` may be nil or an NSImage.
    unsafe {
        dragging_item.setDraggingFrame_contents(frame, image.as_deref().map(|i| i as &AnyObject))
    };
    let items = objc2_foundation::NSArray::from_retained_slice(&[dragging_item]);
    // SAFETY: the GPUI window class implements NSDraggingSource (GPUI's own file drag uses it
    // as the source); the view, event, and items are live for the duration of the call.
    let session: *mut AnyObject = unsafe {
        msg_send![&*view, beginDraggingSessionWithItems: &*items, event: &*event, source: &*ns_window]
    };
    !session.is_null()
}

/// Posts a left mouse-up at the current pointer position to this process. After a native drag
/// session (which swallows the real mouse-up) this lets GPUI end its in-window drag state.
pub fn post_left_mouse_up_to_self() {
    use objc2_core_graphics::{CGEventType, CGMouseButton};
    let Some(probe) = CGEvent::new(None) else {
        return;
    };
    let location = CGEvent::location(Some(&probe));
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState);
    let event = CGEvent::new_mouse_event(
        source.as_deref(),
        CGEventType::LeftMouseUp,
        location,
        CGMouseButton::Left,
    );
    CGEvent::post_to_pid(std::process::id() as i32, event.as_deref());
}

// ---- accessibility and key injection -------------------------------------------------------

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

/// Whether macOS lets this process post keyboard events (System Settings → Accessibility).
pub fn accessibility_trusted() -> bool {
    // SAFETY: plain C call with no arguments.
    unsafe { AXIsProcessTrusted() }
}

/// Same check, but asks macOS to show its permission prompt when the answer is no.
pub fn accessibility_trusted_with_prompt() -> bool {
    let key = NSString::from_str("AXTrustedCheckOptionPrompt");
    let value = NSNumber::new_bool(true);
    let options = NSDictionary::from_slices::<NSString>(&[&key], &[&*value]);
    // SAFETY: NSDictionary is toll-free bridged to CFDictionary; the dictionary outlives the call.
    unsafe { AXIsProcessTrustedWithOptions(Retained::as_ptr(&options) as *const c_void) }
}

/// Posts Command-V to the session event tap as an explicit modifier sequence: Command down,
/// V down, V up, Command up. macOS dispatches a session-tap event as a key equivalent to
/// whichever window has key focus, so the caller hides the panel first to hand focus back to
/// the target app. Requires Accessibility.
///
/// `CGEventPostToPid` is not an option: apps that dispatch ⌘V through the key window's responder
/// chain (iTerm2) ignore it. The Command key itself goes down around V because such apps track
/// the modifier as a held key; a V event that only carries the Command flag types a "v". No
/// Unicode override is set, which can make the event look like typed text.
pub fn post_cmd_v() {
    use objc2_core_graphics::CGEventTapLocation;
    // Left Command key. Keycode 9 is the ANSI "V" position (`VK_V`).
    const VK_COMMAND: u16 = 0x37;
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState);
    let tap = CGEventTapLocation::SessionEventTap;

    let post = |key: u16, key_down: bool, flags: CGEventFlags| {
        let event = CGEvent::new_keyboard_event(source.as_deref(), key, key_down);
        CGEvent::set_flags(event.as_deref(), flags);
        CGEvent::post(tap, event.as_deref());
    };
    // Command down, V down, V up (all with the Command flag held), Command up.
    post(VK_COMMAND, true, CGEventFlags::MaskCommand);
    post(VK_V, true, CGEventFlags::MaskCommand);
    post(VK_V, false, CGEventFlags::MaskCommand);
    post(VK_COMMAND, false, CGEventFlags::empty());
    debug!("posted ⌘V to the session event tap");
}

/// Whether a keyboard event is a plain Cmd-V on the current layout (by keycode or character).
fn is_cmd_v(event: &CGEvent) -> bool {
    use objc2_core_graphics::CGEventField;
    let flags = CGEvent::flags(Some(event));
    if !flags.contains(CGEventFlags::MaskCommand)
        || flags.contains(CGEventFlags::MaskShift)
        || flags.contains(CGEventFlags::MaskAlternate)
        || flags.contains(CGEventFlags::MaskControl)
    {
        return false;
    }
    let keycode = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
    if keycode == VK_V as i64 {
        return true;
    }
    let mut buf = [0u16; 4];
    let mut len: u64 = 0;
    // SAFETY: the buffer holds up to four UTF-16 units and `len` receives the count written.
    unsafe {
        CGEvent::keyboard_get_unicode_string(
            Some(event),
            buf.len() as u64,
            &mut len,
            buf.as_mut_ptr(),
        );
    }
    len >= 1 && (buf[0] == u16::from(b'v') || buf[0] == u16::from(b'V'))
}

// ---- Cmd-V event tap (Paste Queue) ---------------------------------------------------------

struct TapContext {
    tx: UnboundedSender<()>,
    port: Option<objc2_core_foundation::CFRetained<objc2_core_foundation::CFMachPort>>,
}

/// Listen-only session tap that reports every Cmd-V key-up. Runs its own thread with a
/// `CFRunLoop`, as event taps require; re-enables itself when macOS disables it for slowness.
pub struct CmdVTap {
    run_loop: RunLoopHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// `CFRunLoop` is thread-safe for `stop`, which is all the main thread does with it.
struct RunLoopHandle(*const objc2_core_foundation::CFRunLoop);
// SAFETY: CFRunLoopStop and CFRunLoopWakeUp are documented as safe from any thread, and the
// pointer stays valid while the tap thread (which owns a retain) is alive; `stop` joins it.
unsafe impl Send for RunLoopHandle {}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    kind: objc2_core_graphics::CGEventType,
    event: std::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    use objc2_core_graphics::CGEventType;
    // SAFETY: `user_info` is the Box<TapContext> leaked in `start`, freed only after the tap
    // thread has exited.
    let context = unsafe { &*(user_info as *const TapContext) };
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        if let Some(port) = &context.port {
            CGEvent::tap_enable(port, true);
        }
        return event.as_ptr();
    }
    if kind == CGEventType::KeyUp {
        // SAFETY: CoreGraphics passes a live event for the callback's duration.
        let event_ref = unsafe { event.as_ref() };
        if is_cmd_v(event_ref) {
            let _ = context.tx.unbounded_send(());
        }
    }
    event.as_ptr()
}

impl CmdVTap {
    /// Creates the tap on a new thread. `None` when macOS refuses (no Accessibility).
    pub fn start(tx: UnboundedSender<()>) -> Option<CmdVTap> {
        use objc2_core_foundation::{CFMachPort, CFRunLoop, kCFRunLoopCommonModes};
        use objc2_core_graphics::{
            CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<RunLoopHandle>>();
        let thread = std::thread::Builder::new()
            .name("pasteloop-event-tap".into())
            .spawn(move || {
                let context = Box::into_raw(Box::new(TapContext { tx, port: None }));
                let mask: u64 = 1 << CGEventType::KeyUp.0;
                // SAFETY: the callback matches CGEventTapCallBack and the context pointer stays
                // valid until this thread frees it after the run loop stops.
                let port = unsafe {
                    CGEvent::tap_create(
                        CGEventTapLocation::SessionEventTap,
                        CGEventTapPlacement::HeadInsertEventTap,
                        CGEventTapOptions::ListenOnly,
                        mask,
                        Some(tap_callback),
                        context as *mut c_void,
                    )
                };
                let Some(port) = port else {
                    // SAFETY: nothing else holds the context.
                    drop(unsafe { Box::from_raw(context) });
                    let _ = ready_tx.send(None);
                    return;
                };
                // SAFETY: the callback only reads `port` after this write, from the same thread's
                // run loop, which has not started yet.
                unsafe { (*context).port = Some(port.clone()) };
                let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
                    let _ = ready_tx.send(None);
                    return;
                };
                let Some(run_loop) = CFRunLoop::current() else {
                    let _ = ready_tx.send(None);
                    return;
                };
                // SAFETY: reading a constant exported by CoreFoundation.
                let mode = unsafe { kCFRunLoopCommonModes };
                run_loop.add_source(Some(&source), mode);
                CGEvent::tap_enable(&port, true);
                let handle =
                    RunLoopHandle(objc2_core_foundation::CFRetained::as_ptr(&run_loop).as_ptr());
                let _ = ready_tx.send(Some(handle));
                CFRunLoop::run();
                CGEvent::tap_enable(&port, false);
                run_loop.remove_source(Some(&source), mode);
                // SAFETY: the run loop has exited, so no callback can run again.
                drop(unsafe { Box::from_raw(context) });
            })
            .ok()?;
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Some(run_loop)) => Some(CmdVTap {
                run_loop,
                thread: Some(thread),
            }),
            _ => {
                debug!("event tap thread did not report a run loop");
                None
            }
        }
    }

    pub fn stop(mut self) {
        // SAFETY: the pointer came from a live CFRunLoop retained by the tap thread; stopping
        // it is thread-safe.
        unsafe {
            let run_loop = &*self.run_loop.0;
            run_loop.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---- compression (Paste app payloads) ------------------------------------------------------

const COMPRESSION_ZLIB: i32 = 0x205;
const COMPRESSION_LZFSE: i32 = 0x801;

/// Decodes a raw LZFSE stream (the `bvx…` block format), used by the Paste app's newer payloads.
pub fn lzfse_decode(src: &[u8]) -> Option<Vec<u8>> {
    compression_decode(src, COMPRESSION_LZFSE)
}

/// Decodes a raw DEFLATE stream (Apple's `COMPRESSION_ZLIB`), used by the Paste app's JSON
/// payloads.
pub fn zlib_decode(src: &[u8]) -> Option<Vec<u8>> {
    compression_decode(src, COMPRESSION_ZLIB)
}

/// Compresses (`encode`) or decompresses everything `input` yields into `output` with LZFSE,
/// streaming in 64 KB chunks so memory stays flat whatever the size.
pub fn lzfse_stream(
    input: &mut dyn std::io::Read,
    output: &mut dyn std::io::Write,
    encode: bool,
) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    #[repr(C)]
    struct CompressionStream {
        dst_ptr: *mut u8,
        dst_size: usize,
        src_ptr: *const u8,
        src_size: usize,
        state: *mut c_void,
    }
    const STATUS_OK: i32 = 0;
    const STATUS_END: i32 = 1;
    const FLAG_FINALIZE: i32 = 1;
    // SAFETY: the libcompression streaming API, declared with its C signatures.
    #[link(name = "compression")]
    unsafe extern "C" {
        fn compression_stream_init(
            stream: *mut CompressionStream,
            operation: i32,
            algorithm: i32,
        ) -> i32;
        fn compression_stream_process(stream: *mut CompressionStream, flags: i32) -> i32;
        fn compression_stream_destroy(stream: *mut CompressionStream) -> i32;
    }
    let mut stream = CompressionStream {
        dst_ptr: std::ptr::null_mut(),
        dst_size: 0,
        src_ptr: std::ptr::null(),
        src_size: 0,
        state: std::ptr::null_mut(),
    };
    let operation = if encode { 0 } else { 1 };
    // SAFETY: `stream` is a valid, exclusively borrowed struct for init to fill in.
    if unsafe { compression_stream_init(&mut stream, operation, COMPRESSION_LZFSE) } != STATUS_OK {
        return Err(Error::other("compression_stream_init failed"));
    }
    let mut src = vec![0u8; 64 * 1024];
    let mut dst = vec![0u8; 64 * 1024];
    let mut eof = false;
    let result = loop {
        if stream.src_size == 0 && !eof {
            let n = match input.read(&mut src) {
                Ok(n) => n,
                Err(err) => break Err(err),
            };
            eof = n == 0;
            stream.src_ptr = src.as_ptr();
            stream.src_size = n;
        }
        stream.dst_ptr = dst.as_mut_ptr();
        stream.dst_size = dst.len();
        let flags = if eof { FLAG_FINALIZE } else { 0 };
        // SAFETY: src_ptr/src_size describe unread bytes of `src` and dst_ptr/dst_size the whole
        // of `dst`; both buffers outlive the call and are not reallocated in this loop.
        let status = unsafe { compression_stream_process(&mut stream, flags) };
        let produced = dst.len() - stream.dst_size;
        if let Err(err) = output.write_all(&dst[..produced]) {
            break Err(err);
        }
        match status {
            STATUS_END => break Ok(()),
            STATUS_OK if eof && produced == 0 && stream.src_size == 0 => {
                break Err(Error::new(
                    ErrorKind::UnexpectedEof,
                    "truncated LZFSE stream",
                ));
            }
            STATUS_OK => {}
            _ => break Err(Error::new(ErrorKind::InvalidData, "LZFSE stream error")),
        }
    };
    // SAFETY: `stream` was initialized above and is destroyed exactly once.
    unsafe { compression_stream_destroy(&mut stream) };
    result
}

/// Decompresses `src` with an Apple Compression algorithm into a right-sized buffer.
fn compression_decode(src: &[u8], algorithm: i32) -> Option<Vec<u8>> {
    // SAFETY: `compression_decode_buffer` from libcompression; declared with its C signature.
    #[link(name = "compression")]
    unsafe extern "C" {
        fn compression_decode_buffer(
            dst: *mut u8,
            dst_size: usize,
            src: *const u8,
            src_size: usize,
            scratch: *mut c_void,
            algorithm: i32,
        ) -> usize;
    }
    if src.is_empty() {
        return None;
    }
    let mut cap = 256 * 1024;
    loop {
        let mut dst = vec![0u8; cap];
        // SAFETY: dst and src are valid buffers of the given lengths; a null scratch tells the
        // framework to allocate its own.
        let n = unsafe {
            compression_decode_buffer(
                dst.as_mut_ptr(),
                dst.len(),
                src.as_ptr(),
                src.len(),
                std::ptr::null_mut(),
                algorithm,
            )
        };
        if n == 0 {
            return None;
        }
        if n < cap || cap >= 64 * 1024 * 1024 {
            dst.truncate(n);
            return Some(dst);
        }
        cap *= 4; // buffer may have been too small; grow and retry.
    }
}

// ---- images --------------------------------------------------------------------------------

// AppKit and Vision hand back autoreleased objects (decoded bitmaps, PNG data). These helpers run
// on background threads in long loops (capture, the Paste import, OCR) where no pool drains, so
// each drains its own.

/// Decodes any image format ImageIO understands (TIFF, HEIC, ...) and re-encodes it as PNG.
/// Safe to call off the main thread: `NSBitmapImageRep` does not draw.
pub fn image_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    objc2::rc::autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)?;
        png_bytes(&rep)
    })
}

/// Pixel dimensions via ImageIO, for formats the `image` crate cannot parse (HEIC).
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    objc2::rc::autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)?;
        let (w, h) = (rep.pixelsWide(), rep.pixelsHigh());
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    })
}

// ---- status item ---------------------------------------------------------------------------

/// What the menu-bar item asks the app to do. Delivered through a channel into a GPUI task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusCommand {
    TogglePanel,
    ToggleQueue,
    PauseFor(Option<Duration>),
    Resume,
    OpenSettings,
    EraseHistory,
    About,
    Quit,
    ScreenParametersChanged,
}

const TAG_OPEN: isize = 1;
const TAG_QUEUE: isize = 2;
const TAG_PAUSE_5: isize = 10;
const TAG_PAUSE_15: isize = 11;
const TAG_PAUSE_30: isize = 12;
const TAG_PAUSE_60: isize = 13;
const TAG_PAUSE_FOREVER: isize = 14;
const TAG_RESUME: isize = 15;
const TAG_SETTINGS: isize = 20;
const TAG_ERASE: isize = 21;
const TAG_ABOUT: isize = 22;
const TAG_QUIT: isize = 30;

struct TargetIvars {
    tx: UnboundedSender<StatusCommand>,
    status_item: std::cell::RefCell<Option<Retained<NSStatusItem>>>,
    menu: std::cell::RefCell<Option<Retained<NSMenu>>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and StatusTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PasteLoopStatusTarget"]
    #[ivars = TargetIvars]
    struct StatusTarget;

    impl StatusTarget {
        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _sender: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            let right_click = app.currentEvent().is_some_and(|event| {
                let kind = event.r#type();
                kind == NSEventType::RightMouseUp
                    || kind == NSEventType::RightMouseDown
                    || event.modifierFlags().contains(NSEventModifierFlags::Control)
            });
            if right_click {
                self.pop_up_menu();
            } else {
                let _ = self.ivars().tx.unbounded_send(StatusCommand::TogglePanel);
            }
        }

        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            // SAFETY: only NSMenuItems created below are wired to this selector.
            let tag: isize = unsafe { msg_send![sender, tag] };
            let command = match tag {
                TAG_OPEN => StatusCommand::TogglePanel,
                TAG_QUEUE => StatusCommand::ToggleQueue,
                TAG_PAUSE_5 => StatusCommand::PauseFor(Some(Duration::from_secs(5 * 60))),
                TAG_PAUSE_15 => StatusCommand::PauseFor(Some(Duration::from_secs(15 * 60))),
                TAG_PAUSE_30 => StatusCommand::PauseFor(Some(Duration::from_secs(30 * 60))),
                TAG_PAUSE_60 => StatusCommand::PauseFor(Some(Duration::from_secs(60 * 60))),
                TAG_PAUSE_FOREVER => StatusCommand::PauseFor(None),
                TAG_RESUME => StatusCommand::Resume,
                TAG_SETTINGS => StatusCommand::OpenSettings,
                TAG_ERASE => StatusCommand::EraseHistory,
                TAG_ABOUT => StatusCommand::About,
                TAG_QUIT => StatusCommand::Quit,
                _ => return,
            };
            let _ = self.ivars().tx.unbounded_send(command);
        }

        #[unsafe(method(screenParametersChanged:))]
        fn screen_parameters_changed(&self, _notification: Option<&AnyObject>) {
            let _ = self
                .ivars()
                .tx
                .unbounded_send(StatusCommand::ScreenParametersChanged);
        }
    }
);

impl StatusTarget {
    fn pop_up_menu(&self) {
        let (Some(item), Some(menu)) = (
            self.ivars().status_item.borrow().clone(),
            self.ivars().menu.borrow().clone(),
        ) else {
            return;
        };
        // Attaching the menu and clicking the button shows it under the item; detaching keeps
        // left clicks routed to our action.
        item.setMenu(Some(&menu));
        if let Some(button) = item.button(self.mtm()) {
            // SAFETY: performClick takes an optional sender; nil is fine.
            unsafe { button.performClick(None) };
        }
        item.setMenu(None);
    }
}

/// The menu-bar item. Dropping it removes the item.
pub struct StatusItem {
    item: Retained<NSStatusItem>,
    target: Retained<StatusTarget>,
    pause_item: Retained<NSMenuItem>,
    resume_item: Retained<NSMenuItem>,
}

impl StatusItem {
    pub fn install(tx: UnboundedSender<StatusCommand>, shortcuts: &StatusShortcuts) -> StatusItem {
        let mtm = main_thread();
        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(NSVariableStatusItemLength);
        let target = StatusTarget::alloc(mtm).set_ivars(TargetIvars {
            tx,
            status_item: std::cell::RefCell::new(None),
            menu: std::cell::RefCell::new(None),
        });
        // SAFETY: NSObject's init on an allocated, ivar-initialized instance.
        let target: Retained<StatusTarget> = unsafe { msg_send![super(target), init] };
        if let Some(button) = item.button(mtm) {
            if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str("doc.on.clipboard"),
                Some(&NSString::from_str("PasteLoop")),
            ) {
                image.setTemplate(true);
                button.setImage(Some(&image));
            } else {
                button.setTitle(&NSString::from_str("P"));
            }
            // SAFETY: the target outlives the button (both live in this struct); the selector
            // is defined on StatusTarget above.
            unsafe {
                button.setTarget(Some(&target));
                button.setAction(Some(sel!(statusClicked:)));
            }
            button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
        }
        let (menu, pause_item, resume_item) = build_menu(mtm, &target, shortcuts);
        *target.ivars().status_item.borrow_mut() = Some(item.clone());
        *target.ivars().menu.borrow_mut() = Some(menu);

        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: the target defines `screenParametersChanged:`; the notification name is a
        // constant exported by AppKit; the observer is removed in Drop.
        unsafe {
            center.addObserver_selector_name_object(
                &target,
                sel!(screenParametersChanged:),
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
            );
        }
        StatusItem {
            item,
            target,
            pause_item,
            resume_item,
        }
    }

    /// Greys the icon out while capture is paused and swaps the Pause/Resume entries.
    pub fn set_paused(&self, paused: bool) {
        if let Some(button) = self.item.button(main_thread()) {
            button.setAppearsDisabled(paused);
        }
        self.pause_item.setHidden(paused);
        self.resume_item.setHidden(!paused);
    }
}

impl Drop for StatusItem {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        // SAFETY: the observer was added in `install`.
        unsafe { center.removeObserver(&self.target) };
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}

/// Shortcut labels shown next to the menu entries (display only; the app has no menu bar).
pub struct StatusShortcuts {
    pub open: String,
    pub queue: String,
}

fn menu_item(
    mtm: MainThreadMarker,
    target: &StatusTarget,
    title: &str,
    tag: isize,
    key: &str,
    mask: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    // SAFETY: the selector is defined on StatusTarget.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(sel!(menuAction:)),
            &NSString::from_str(key),
        )
    };
    if !key.is_empty() {
        item.setKeyEquivalentModifierMask(mask);
    }
    item.setTag(tag);
    // SAFETY: the target outlives the menu.
    unsafe { item.setTarget(Some(target)) };
    item
}

/// Converts a keystroke like "shift-cmd-v" into a menu key equivalent and modifier mask.
fn key_equivalent(keystroke: &str) -> (String, NSEventModifierFlags) {
    let mut mask = NSEventModifierFlags::empty();
    let mut key = String::new();
    for token in keystroke.split('-') {
        match token {
            "cmd" | "command" | "super" => mask |= NSEventModifierFlags::Command,
            "shift" => mask |= NSEventModifierFlags::Shift,
            "alt" | "option" => mask |= NSEventModifierFlags::Option,
            "ctrl" | "control" => mask |= NSEventModifierFlags::Control,
            other => key = other.to_string(),
        }
    }
    (key, mask)
}

fn build_menu(
    mtm: MainThreadMarker,
    target: &StatusTarget,
    shortcuts: &StatusShortcuts,
) -> (Retained<NSMenu>, Retained<NSMenuItem>, Retained<NSMenuItem>) {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("PasteLoop"));
    menu.setAutoenablesItems(false);
    let (open_key, open_mask) = key_equivalent(&shortcuts.open);
    menu.addItem(&menu_item(
        mtm,
        target,
        "Open PasteLoop",
        TAG_OPEN,
        &open_key,
        open_mask,
    ));
    let (queue_key, queue_mask) = key_equivalent(&shortcuts.queue);
    menu.addItem(&menu_item(
        mtm,
        target,
        "Paste Queue",
        TAG_QUEUE,
        &queue_key,
        queue_mask,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let pause_item = menu_item(
        mtm,
        target,
        "Pause Capture",
        0,
        "t",
        NSEventModifierFlags::Command,
    );
    let pause_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Pause"));
    pause_menu.setAutoenablesItems(false);
    for (title, tag) in [
        ("For 5 Minutes", TAG_PAUSE_5),
        ("For 15 Minutes", TAG_PAUSE_15),
        ("For 30 Minutes", TAG_PAUSE_30),
        ("For 1 Hour", TAG_PAUSE_60),
        ("Until Resumed", TAG_PAUSE_FOREVER),
    ] {
        pause_menu.addItem(&menu_item(
            mtm,
            target,
            title,
            tag,
            "",
            NSEventModifierFlags::empty(),
        ));
    }
    pause_item.setSubmenu(Some(&pause_menu));
    menu.addItem(&pause_item);
    let resume_item = menu_item(
        mtm,
        target,
        "Resume Capture",
        TAG_RESUME,
        "t",
        NSEventModifierFlags::Command,
    );
    resume_item.setHidden(true);
    menu.addItem(&resume_item);
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    menu.addItem(&menu_item(
        mtm,
        target,
        "Settings…",
        TAG_SETTINGS,
        ",",
        NSEventModifierFlags::Command,
    ));
    menu.addItem(&menu_item(
        mtm,
        target,
        "Erase History…",
        TAG_ERASE,
        "",
        NSEventModifierFlags::empty(),
    ));
    menu.addItem(&menu_item(
        mtm,
        target,
        "About PasteLoop",
        TAG_ABOUT,
        "",
        NSEventModifierFlags::empty(),
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(
        mtm,
        target,
        "Quit PasteLoop",
        TAG_QUIT,
        "q",
        NSEventModifierFlags::Command,
    ));
    (menu, pause_item, resume_item)
}

// ---- Vision OCR, link metadata, Quick Look -------------------------------------------------

/// Recognizes text in an image with Vision (accurate level, language correction). Thread-safe;
/// called from the background executor. Returns the lines joined with newlines, or `None`.
pub fn recognize_text(image_bytes: &[u8]) -> Option<String> {
    objc2::rc::autoreleasepool(|_| recognize_text_in_pool(image_bytes))
}

fn recognize_text_in_pool(image_bytes: &[u8]) -> Option<String> {
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };
    let data = NSData::with_bytes(image_bytes);
    let options = NSDictionary::new();
    let handler = VNImageRequestHandler::initWithData_options(
        VNImageRequestHandler::alloc(),
        &data,
        &options,
    );
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    let base: &VNRequest = &request;
    let requests = objc2_foundation::NSArray::from_slice(&[base]);
    if let Err(err) = handler.performRequests_error(&requests) {
        debug!("vision request failed: {}", err.localizedDescription());
        return None;
    }
    let results = request.results()?;
    let mut lines = Vec::new();
    for observation in results.iter() {
        let candidates = observation.topCandidates(1);
        if let Some(best) = candidates.iter().next() {
            let text = best.string().to_string();
            if !text.trim().is_empty() {
                lines.push(text);
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// What LinkPresentation found for a URL.
#[derive(Clone, Debug, Default)]
pub struct LinkMetadata {
    pub title: Option<String>,
    pub image_png: Option<Vec<u8>>,
}

/// Fetches a link's title and preview image with `LPMetadataProvider` (Safari's rules) and
/// sends the result on `tx`. Must be called on the main thread; the completion handlers run
/// on a background queue. The provider stays alive through the block it retains; that small
/// cycle is dropped when the fetch completes.
pub fn fetch_link_metadata(
    item_id: i64,
    url: &str,
    timeout_secs: f64,
    tx: UnboundedSender<(i64, LinkMetadata)>,
) {
    use objc2_link_presentation::{LPLinkMetadata, LPMetadataProvider};
    let Some(ns_url) = NSURL::URLWithString(&NSString::from_str(url)) else {
        let _ = tx.unbounded_send((item_id, LinkMetadata::default()));
        return;
    };
    // SAFETY: LPMetadataProvider has a plain designated initializer.
    let provider = unsafe { LPMetadataProvider::new() };
    // SAFETY: the timeout is a plain number.
    unsafe { provider.setTimeout(timeout_secs) };
    let completion = block2::RcBlock::new(
        move |metadata: *mut LPLinkMetadata, error: *mut objc2_foundation::NSError| {
            if metadata.is_null() {
                if !error.is_null() {
                    // SAFETY: AppKit passes a live NSError when non-null.
                    let err = unsafe { &*error };
                    debug!("link metadata failed: {}", err.localizedDescription());
                }
                let _ = tx.unbounded_send((item_id, LinkMetadata::default()));
                return;
            }
            // SAFETY: LinkPresentation passes a live LPLinkMetadata when non-null.
            let metadata = unsafe { &*metadata };
            // SAFETY: plain property getters.
            let (title, provider) = unsafe { (metadata.title(), metadata.imageProvider()) };
            let title = title.map(|t| t.to_string());
            let Some(image_provider) = provider else {
                let _ = tx.unbounded_send((
                    item_id,
                    LinkMetadata {
                        title,
                        image_png: None,
                    },
                ));
                return;
            };
            let tx = tx.clone();
            let title_for_image = title.clone();
            let image_done = block2::RcBlock::new(
                move |data: *mut NSData, _error: *mut objc2_foundation::NSError| {
                    let png = if data.is_null() {
                        None
                    } else {
                        // SAFETY: Foundation passes live NSData when non-null.
                        let bytes = unsafe { &*data }.to_vec();
                        if bytes.starts_with(b"\x89PNG") {
                            Some(bytes)
                        } else {
                            image_to_png(&bytes)
                        }
                    };
                    let _ = tx.unbounded_send((
                        item_id,
                        LinkMetadata {
                            title: title_for_image.clone(),
                            image_png: png,
                        },
                    ));
                },
            );
            // SAFETY: the type identifier is a valid UTI and the block outlives the call
            // (Foundation retains it).
            let _progress = unsafe {
                image_provider.loadDataRepresentationForTypeIdentifier_completionHandler(
                    &NSString::from_str("public.png"),
                    &image_done,
                )
            };
        },
    );
    // SAFETY: the URL and block are live; LinkPresentation retains both for the fetch.
    unsafe { provider.startFetchingMetadataForURL_completionHandler(&ns_url, &completion) };
    // The provider must outlive the request; parking it in the block's captures would need a
    // cycle, so keep it in a main-thread list pruned on the next fetch.
    PENDING_PROVIDERS.with(|list| {
        let mut list = list.borrow_mut();
        if list.len() >= 8 {
            list.remove(0);
        }
        list.push(provider);
    });
}

thread_local! {
    static PENDING_PROVIDERS: std::cell::RefCell<Vec<Retained<objc2_link_presentation::LPMetadataProvider>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

// ---- launch at login -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginItemStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
    Unavailable,
}

/// `SMAppService.mainApp` status. Only meaningful when running from an `.app` bundle.
pub fn login_item_status() -> LoginItemStatus {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    if !running_from_bundle() {
        return LoginItemStatus::Unavailable;
    }
    // SAFETY: `mainAppService` and `status` have no preconditions beyond running in a bundle.
    let status = unsafe { SMAppService::mainAppService().status() };
    match status {
        SMAppServiceStatus::Enabled => LoginItemStatus::Enabled,
        SMAppServiceStatus::RequiresApproval => LoginItemStatus::RequiresApproval,
        SMAppServiceStatus::NotFound => LoginItemStatus::NotFound,
        _ => LoginItemStatus::NotRegistered,
    }
}

pub fn set_launch_at_login(enabled: bool) -> Result<(), String> {
    use objc2_service_management::SMAppService;
    if !running_from_bundle() {
        return Err("launch at login needs the .app bundle (run `just bundle`)".into());
    }
    // SAFETY: registering the main app service has no preconditions beyond running in a bundle.
    let result = unsafe {
        let service = SMAppService::mainAppService();
        if enabled {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    result.map_err(|err| err.localizedDescription().to_string())
}

pub fn running_from_bundle() -> bool {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

/// Starts a helper that reopens this app once the current process has exited; the caller
/// quits right after. macOS decides at launch whether a process may post keystrokes, so an
/// Accessibility grant made while PasteLoop runs takes effect only after a relaunch. The helper
/// waits for this process to go away so the new one can register the same hotkeys.
pub fn spawn_relauncher() -> std::io::Result<()> {
    use std::ffi::OsString;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let launch: Vec<OsString> = if running_from_bundle() {
        // exe -> MacOS -> Contents -> PasteLoop.app
        let bundle = exe
            .ancestors()
            .nth(3)
            .ok_or_else(|| std::io::Error::other("no bundle path"))?;
        vec!["/usr/bin/open".into(), "-n".into(), bundle.into()]
    } else {
        vec![exe.clone().into()]
    };
    // `-n` asks Launch Services for a fresh instance even if it still lists the old one.
    let script = format!(
        "while kill -0 {} 2>/dev/null; do sleep 0.05; done; sleep 0.2; exec \"$@\"",
        std::process::id()
    );
    Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("pasteloop-relaunch")
        .args(&launch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

pub fn log_environment() {
    debug!(
        "bundle={} accessibility={} reduce_motion={}",
        running_from_bundle(),
        accessibility_trusted(),
        reduce_motion()
    );
}

// ---- iCloud Drive files and background threads ---------------------------------------------

/// Asks iCloud Drive to download a file (or a folder's contents) that may only be a placeholder
/// on this Mac. Returns immediately; reads of the file wait for the download.
pub fn start_icloud_download(path: &std::path::Path) -> bool {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    objc2_foundation::NSFileManager::defaultManager()
        .startDownloadingUbiquitousItemAtURL_error(&url)
        .is_ok()
}

/// Whether iCloud Drive has finished uploading the file: `None` when the file is not in iCloud
/// Drive or its state is unknown.
pub fn icloud_uploaded(path: &std::path::Path) -> Option<bool> {
    use objc2_foundation::{NSArray, NSURLUbiquitousItemIsUploadedKey};
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    // SAFETY: an extern static NSString provided by Foundation.
    let key = unsafe { NSURLUbiquitousItemIsUploadedKey };
    let values = url
        .resourceValuesForKeys_error(&NSArray::from_slice(&[key]))
        .ok()?;
    let value = values.objectForKey(key)?;
    value.downcast_ref::<NSNumber>().map(|n| n.boolValue())
}

/// Lowers the calling thread to the utility QoS and lets it read iCloud Drive files that are
/// only placeholders on this Mac (the read downloads them). For long background jobs.
pub fn become_background_file_worker() {
    const QOS_CLASS_UTILITY: u32 = 0x11;
    const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: i32 = 3;
    const IOPOL_SCOPE_THREAD: i32 = 1;
    const IOPOL_MATERIALIZE_DATALESS_FILES_ON: i32 = 2;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
    }
    // SAFETY: both calls only change attributes of the calling thread.
    let (qos, io) = unsafe {
        (
            pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0),
            setiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_THREAD,
                IOPOL_MATERIALIZE_DATALESS_FILES_ON,
            ),
        )
    };
    if qos != 0 || io != 0 {
        warn!("background worker setup: qos={qos} iopolicy={io}");
    }
}

/// This Mac's network name without `.local`, to label its backup folder.
pub fn host_name() -> String {
    let name = objc2_foundation::NSProcessInfo::processInfo()
        .hostName()
        .to_string();
    name.trim_end_matches(".local").to_string()
}
