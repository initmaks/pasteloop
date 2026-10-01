//! Paste Queue: an ad-hoc ordered queue of items. While it is active every capture appends to
//! the queue, the pasteboard always holds the queue head, and each Cmd-V in any app (seen by a
//! listen-only event tap) pops the head and loads the next one. A small floating window shows
//! the queue with a direction toggle.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, AsyncApp, Bounds, Context, Entity, FocusHandle, Focusable, Global,
    KeyBinding, SharedString, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, actions, div, point, prelude::*, px, size, svg,
};
use tracing::{info, warn};

use crate::cards::{Theme, kind_icon};
use crate::macos::{self, NativeWindow};
use crate::paste;
use crate::state::AppState;
use crate::store::Item;

actions!(queue, [CloseQueue, PasteNext, ToggleDirection]);

pub const KEY_CONTEXT: &str = "Queue";
const WIDTH: f64 = 280.0;
const HEIGHT: f64 = 200.0;
/// Delay between the user's Cmd-V key-up and loading the next item, so the target app has
/// read the pasteboard first.
const ADVANCE_DELAY: Duration = Duration::from_millis(60);

pub fn key_bindings() -> Vec<KeyBinding> {
    let c = Some(KEY_CONTEXT);
    vec![
        KeyBinding::new("escape", CloseQueue, c),
        KeyBinding::new("enter", PasteNext, c),
        KeyBinding::new("cmd-d", ToggleDirection, c),
    ]
}

/// The queue window handle, created on first use and kept alive.
pub struct QueueWindow(pub WindowHandle<QueueView>);
impl Global for QueueWindow {}

/// The listen-only Cmd-V tap; the global exists only while the queue is active.
pub struct QueueTap(macos::CmdVTap);
impl Global for QueueTap {}

pub struct QueueView {
    state: Entity<AppState>,
    native: NativeWindow,
    focus_handle: FocusHandle,
    theme: Theme,
    _subscription: gpui::Subscription,
}

impl QueueView {
    fn new(
        state: Entity<AppState>,
        native: NativeWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        QueueView {
            state,
            native,
            focus_handle: cx.focus_handle(),
            theme: Theme::for_window(window),
            _subscription: subscription,
        }
    }

    fn close_queue(&mut self, _: &CloseQueue, _: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.spawn(async move |_, cx| deactivate(state, cx).await)
            .detach();
    }

    fn paste_next(&mut self, _: &PasteNext, _: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.spawn(async move |_, cx| advance(state, cx).await)
            .detach();
    }

    fn toggle_direction(&mut self, _: &ToggleDirection, _: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.queue.newest_first = !s.queue.newest_first;
            cx.notify();
        });
        let state = self.state.clone();
        cx.spawn(async move |_, cx| load_head(state, cx).await)
            .detach();
    }

    fn button(
        &self,
        id: &'static str,
        label: &str,
        primary: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        crate::cards::button(id, label, primary, &self.theme)
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    pub fn native(&self) -> &NativeWindow {
        &self.native
    }
}

impl Focusable for QueueView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueueView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.theme = Theme::for_window(window);
        let theme = self.theme.clone();
        let (ordered, newest_first, tap_available) = {
            let s = self.state.read(cx);
            (
                s.queue.ordered_items(),
                s.queue.newest_first,
                s.queue.tap_available,
            )
        };
        let mut root = div()
            .id("queue-root")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_queue))
            .on_action(cx.listener(Self::paste_next))
            .on_action(cx.listener(Self::toggle_direction))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.surface)
            .text_color(theme.text)
            .font_family(crate::cards::MONO)
            .text_size(px(12.))
            .p(px(10.))
            .gap(px(8.));

        root = root.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Paste Queue"),
                )
                .child(self.button(
                    "direction",
                    if newest_first {
                        "Newest first"
                    } else {
                        "Oldest first"
                    },
                    false,
                    cx,
                    |this, window, cx| this.toggle_direction(&ToggleDirection, window, cx),
                )),
        );

        if ordered.is_empty() {
            root = root.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .text_color(theme.text_secondary)
                    .child("The queue is empty. Copy items to queue them; each ⌘V pastes the next one.")
                    .child(self.button("done", "Done", true, cx, |this, window, cx| {
                        this.close_queue(&CloseQueue, window, cx)
                    })),
            );
        } else {
            let mut list = div()
                .id("queue-list")
                .flex_1()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(2.));
            for (ix, item) in ordered.iter().enumerate() {
                let head = ix == 0;
                let title = crate::store::truncate_chars(&item.display_title(), 34);
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .px(px(6.))
                        .h(px(22.))
                        .rounded(px(6.))
                        .when(head, |d| d.bg(theme.accent.opacity(0.2)))
                        .child(
                            div()
                                .w(px(16.))
                                .text_color(theme.text_secondary)
                                .child(format!("{}", ix + 1)),
                        )
                        .child(
                            svg()
                                .path(kind_icon(item.kind))
                                .size(px(11.))
                                .text_color(theme.text_secondary)
                                .flex_shrink_0(),
                        )
                        .child(div().truncate().child(SharedString::from(title))),
                );
            }
            root = root.child(list);
            let mut footer = div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(6.));
            if tap_available {
                footer = footer.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.text_secondary)
                        .child("⌘V in any app pastes the next item"),
                );
            } else {
                footer = footer
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_secondary)
                            .child("Grant Accessibility to advance automatically"),
                    )
                    .child(self.button(
                        "paste-next",
                        "Paste next",
                        true,
                        cx,
                        |this, window, cx| this.paste_next(&PasteNext, window, cx),
                    ));
            }
            footer = footer.child(self.button("close", "Done", false, cx, |this, window, cx| {
                this.close_queue(&CloseQueue, window, cx)
            }));
            root = root.child(footer);
        }
        root
    }
}

/// Shift-Cmd-C: activates the queue, or deactivates it when it is already up.
pub async fn toggle(state: Entity<AppState>, cx: &mut AsyncApp) {
    let active = cx.update(|cx| state.read(cx).queue.active);
    if active {
        deactivate(state, cx).await;
    } else {
        activate(state, cx).await;
    }
}

async fn activate(state: Entity<AppState>, cx: &mut AsyncApp) {
    let handle = ensure_window(state.clone(), cx);
    let Some(handle) = handle else {
        return;
    };
    let trusted = macos::accessibility_trusted();
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.queue.clear();
            s.queue.active = true;
            s.queue.tap_available = trusted;
            cx.notify();
        });
    });
    if trusted {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        match macos::CmdVTap::start(tx) {
            Some(tap) => {
                cx.update(|cx| cx.set_global(QueueTap(tap)));
                let state_for_tap = state.clone();
                cx.spawn(async move |cx| {
                    use futures::StreamExt;
                    while rx.next().await.is_some() {
                        let (active, suppressed) = cx.update(|cx| {
                            let s = &state_for_tap.read(cx).queue;
                            let suppressed = s
                                .suppress_taps_until
                                .is_some_and(|t| std::time::Instant::now() < t);
                            (s.active, suppressed)
                        });
                        if !active {
                            break;
                        }
                        if suppressed {
                            continue;
                        }
                        advance(state_for_tap.clone(), cx).await;
                    }
                })
                .detach();
            }
            None => {
                warn!("event tap could not be created; queue runs in manual mode");
                cx.update(|cx| {
                    state.update(cx, |s, cx| {
                        s.queue.tap_available = false;
                        cx.notify();
                    });
                });
            }
        }
    }
    let Ok((native, frame)) = cx.update(|cx| {
        handle.update(cx, |view, _, _| {
            (
                view.native.clone(),
                macos::queue_frame_under_mouse(WIDTH, HEIGHT),
            )
        })
    }) else {
        return;
    };
    native.show_at(frame);
    cx.update(|cx| {
        handle
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle, cx);
                cx.notify();
            })
            .ok();
    });
    info!("paste queue activated (tap={trusted})");
}

pub async fn deactivate(state: Entity<AppState>, cx: &mut AsyncApp) {
    let tap = cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.queue.active = false;
            s.queue.clear();
            cx.notify();
        });
        cx.has_global::<QueueTap>()
            .then(|| cx.remove_global::<QueueTap>())
    });
    if let Some(QueueTap(tap)) = tap {
        tap.stop();
    }
    let native = cx.update(|cx| {
        cx.try_global::<QueueWindow>()
            .map(|w| w.0)
            .and_then(|h| h.update(cx, |view, _, _| view.native.clone()).ok())
    });
    if let Some(native) = native {
        native.order_out();
    }
    info!("paste queue deactivated");
}

/// Called by the capture poller for every new item while the queue is active.
pub async fn on_capture(item: Item, state: Entity<AppState>, cx: &mut AsyncApp) {
    let queued = cx.update(|cx| {
        state.update(cx, |s, cx| {
            if !s.queue.active {
                return false;
            }
            s.queue.push(item);
            // The user's copy just replaced the pasteboard; the head must be written again.
            s.queue.loaded_id = None;
            cx.notify();
            true
        })
    });
    if queued {
        load_head(state, cx).await;
    }
}

/// After the panel wrote the pasteboard itself (paste or Cmd-C): give the target app time to
/// read it, then restore the queue head so the queue invariant holds.
pub fn after_own_write(state: Entity<AppState>, cx: &mut AsyncApp) {
    let active = cx.update(|cx| state.read(cx).queue.active);
    if !active {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(ADVANCE_DELAY).await;
        cx.update(|cx| {
            state.update(cx, |s, _| s.queue.loaded_id = None);
        });
        load_head(state, cx).await;
    })
    .detach();
}

/// Writes the queue head to the pasteboard unless it is already there.
async fn load_head(state: Entity<AppState>, cx: &mut AsyncApp) {
    let (head, store, images_as_files, form) = cx.update(|cx| {
        let s = state.read(cx);
        (
            s.queue
                .head()
                .filter(|h| s.queue.loaded_id != Some(h.id))
                .cloned(),
            Arc::clone(&s.store),
            s.settings.images_as_files,
            paste::Form::for_paste(s.settings.paste_plain_text, false),
        )
    });
    let Some(head) = head else {
        return;
    };
    let reps = paste::representations(&head, &store, form, images_as_files);
    let count = macos::write_pasteboard(&reps);
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            s.own_change_count = Some(count);
            s.queue.loaded_id = Some(head.id);
            cx.notify();
        });
    });
}

/// After a Cmd-V (or "Paste next"): drop the head and load the next one.
pub async fn advance(state: Entity<AppState>, cx: &mut AsyncApp) {
    cx.background_executor().timer(ADVANCE_DELAY).await;
    let popped = cx.update(|cx| {
        state.update(cx, |s, cx| {
            let popped = s.queue.pop_head();
            s.queue.loaded_id = None;
            cx.notify();
            popped
        })
    });
    if let Some(item) = popped {
        info!("queue advanced past item {}", item.id);
        load_head(state, cx).await;
    }
}

/// Creates the queue window once, from a task, and returns its handle.
fn ensure_window(state: Entity<AppState>, cx: &mut AsyncApp) -> Option<WindowHandle<QueueView>> {
    cx.update(|cx| {
        if let Some(handle) = cx.try_global::<QueueWindow>().map(|w| w.0) {
            return Some(handle);
        }
        let hide = state.read(cx).settings.hide_from_screen_sharing;
        let result = cx.open_window(
            WindowOptions {
                kind: WindowKind::PopUp,
                titlebar: None,
                focus: false,
                show: false,
                is_movable: true,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Blurred,
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(WIDTH as f32), px(HEIGHT as f32)),
                })),
                ..Default::default()
            },
            {
                let state = state.clone();
                move |window, cx| {
                    let native = NativeWindow::of(window).expect("queue window");
                    native.configure_panel(hide);
                    cx.new(|cx| QueueView::new(state, native, window, cx))
                }
            },
        );
        match result {
            Ok(handle) => {
                cx.set_global(QueueWindow(handle));
                Some(handle)
            }
            Err(err) => {
                warn!("queue window failed: {err:#}");
                None
            }
        }
    })
}
