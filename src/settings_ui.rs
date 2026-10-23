//! The settings window: General, Appearance, Shortcuts, Privacy, Integrations, Migration, and
//! About. A normal window opened from a task; edits are saved immediately and applied by the app
//! through `SettingsEvent`.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, SharedString, Stateful, Window, div, prelude::*,
    px,
};
use tracing::{info, warn};

use crate::cards::Theme;
use crate::macos::{self, LoginItemStatus};
use crate::settings::{DEFAULT_PANEL_HEIGHT, Modifier, PasteMode, Settings};
use crate::state::AppState;
use crate::store::Retention;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    General,
    Appearance,
    Shortcuts,
    Privacy,
    Integrations,
    Migration,
    About,
}

impl Section {
    const ALL: [Section; 7] = [
        Section::General,
        Section::Appearance,
        Section::Shortcuts,
        Section::Privacy,
        Section::Integrations,
        Section::Migration,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::Shortcuts => "Shortcuts",
            Section::Privacy => "Privacy",
            Section::Integrations => "Integrations",
            Section::Migration => "Migration",
            Section::About => "About",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeyField {
    Activation,
    Queue,
    NextCollection,
    PrevCollection,
}

/// What the app must do after a change the view cannot apply itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    /// Settings were saved; keymap-affecting fields may have changed.
    Changed,
    /// The activation or queue hotkey changed and must be re-registered.
    HotkeysChanged,
    /// The user confirmed erasing history.
    EraseHistory,
    /// Back up to iCloud Drive at the next check.
    BackUpNow,
    /// The Settings window gained (`true`) or lost (`false`) focus; the live bar follows it.
    BarPreview(bool),
}

pub struct SettingsView {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    section: Section,
    theme: Theme,
    recording: Option<HotkeyField>,
    confirm_erase: bool,
    retention_pending: Option<(Retention, i64)>,
    login_status: LoginItemStatus,
    login_error: Option<String>,
    /// Status line for the "Migrate from Paste" action.
    migrate_status: Option<String>,
    migrating: bool,
    /// Active text-size drag: (start pointer x in window px, scale at drag start).
    text_slider_drag: Option<(f32, f32)>,
    /// Store counts for the About section, read when it opens rather than on every render.
    about_stats: crate::store::Stats,
    /// Editable plain-text list of ignored bundle ids (one per line).
    ignore_input: Entity<crate::text_input::TextInput>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let theme = Theme::for_window(window);
        let ignore_input = cx.new(|cx| {
            let mut input = crate::text_input::TextInput::new(cx, "com.example.app", true);
            input.colors = theme.input_colors();
            let joined = state.read(cx).settings.ignored_apps.join("\n");
            input.set_text(joined, cx);
            input
        });
        let mut subscriptions = vec![cx.observe(&state, |_, _, cx| cx.notify())];
        subscriptions.push(cx.subscribe(&ignore_input, Self::on_ignore_event));
        subscriptions.push(cx.observe_window_activation(window, |_, window, cx| {
            cx.emit(SettingsEvent::BarPreview(window.is_window_active()));
        }));
        SettingsView {
            state,
            focus_handle: cx.focus_handle(),
            section: Section::General,
            theme,
            recording: None,
            confirm_erase: false,
            retention_pending: None,
            login_status: macos::login_item_status(),
            login_error: None,
            migrate_status: None,
            migrating: false,
            text_slider_drag: None,
            about_stats: Default::default(),
            ignore_input,
            _subscriptions: subscriptions,
        }
    }

    fn on_ignore_event(
        &mut self,
        _: Entity<crate::text_input::TextInput>,
        event: &crate::text_input::InputEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::text_input::InputEvent;
        match event {
            // Plain Enter adds a new line in this list rather than submitting.
            InputEvent::Submit => {
                let mut text = self.ignore_input.read(cx).text().to_string();
                text.push('\n');
                self.ignore_input.update(cx, |i, cx| i.set_text(text, cx));
            }
            InputEvent::Changed => {
                let text = self.ignore_input.read(cx).text().to_string();
                let apps: Vec<String> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                self.edit(cx, |s| s.ignored_apps = apps);
            }
            _ => {}
        }
    }

    fn settings(&self, cx: &App) -> Settings {
        self.state.read(cx).settings.clone()
    }

    /// Applies a change to the live settings, saves, and tells the app.
    fn edit(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Settings)) {
        self.state.update(cx, |s, cx| {
            f(&mut s.settings);
            s.save_settings();
            cx.notify();
        });
        cx.emit(SettingsEvent::Changed);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.recording else {
            return;
        };
        cx.stop_propagation();
        let ks = &event.keystroke;
        if ks.key == "escape" {
            self.recording = None;
            cx.notify();
            return;
        }
        let m = &ks.modifiers;
        let key = ks.key.to_lowercase();
        let is_modifier_only = matches!(
            key.as_str(),
            "shift" | "control" | "alt" | "platform" | "cmd" | "ctrl" | "function" | "fn"
        );
        if is_modifier_only {
            return;
        }
        let global = matches!(field, HotkeyField::Activation | HotkeyField::Queue);
        if global && !(m.platform || m.control || m.alt) {
            // A global hotkey without Cmd, Ctrl, or Option would swallow ordinary typing.
            return;
        }
        let mut tokens = Vec::new();
        if m.control {
            tokens.push("ctrl");
        }
        if m.alt {
            tokens.push("alt");
        }
        if m.shift {
            tokens.push("shift");
        }
        if m.platform {
            tokens.push("cmd");
        }
        tokens.push(key.as_str());
        let combo = tokens.join("-");
        info!("hotkey {field:?} set to {combo}");
        self.recording = None;
        self.edit(cx, |s| match field {
            HotkeyField::Activation => s.activation_hotkey = combo.clone(),
            HotkeyField::Queue => s.queue_hotkey = combo.clone(),
            HotkeyField::NextCollection => s.next_collection_key = combo.clone(),
            HotkeyField::PrevCollection => s.previous_collection_key = combo.clone(),
        });
        if global {
            cx.emit(SettingsEvent::HotkeysChanged);
        }
    }

    // ---- widgets -------------------------------------------------------------------------------

    fn row(&self, label: &str, control: impl IntoElement) -> impl IntoElement {
        let theme = self.theme.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .py(px(6.))
            .border_b_1()
            .border_color(theme.separator)
            .child(
                // Grows to fill and shrinks (min_w 0) so the control on the right is never
                // pushed off the edge.
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(13.))
                    .child(SharedString::from(label.to_string())),
            )
            .child(div().flex_shrink_0().child(control))
    }

    fn toggle(
        &self,
        id: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        on_change: impl Fn(&mut Self, bool, &mut Context<Self>) + 'static,
    ) -> Stateful<gpui::Div> {
        let theme = self.theme.clone();
        div()
            .id(id)
            .w(px(38.))
            .h(px(22.))
            .rounded_full()
            .p(px(2.))
            .cursor_pointer()
            .bg(if on { theme.accent } else { theme.separator })
            .flex()
            .when(on, |d| d.justify_end())
            .child(
                div()
                    .size(px(18.))
                    .rounded_full()
                    .bg(gpui::white())
                    .shadow_sm(),
            )
            .on_click(cx.listener(move |this, _, _, cx| on_change(this, !on, cx)))
    }

    fn button(
        &self,
        id: gpui::ElementId,
        label: &str,
        primary: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<gpui::Div> {
        crate::cards::button(id, label, primary, &self.theme)
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
    }

    fn segmented<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: Vec<(T, String)>,
        current: T,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, T, &mut Context<Self>) + 'static + Clone,
    ) -> impl IntoElement {
        let theme = self.theme.clone();
        let mut row = div()
            .flex()
            .flex_row()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(theme.field);
        for (ix, (value, label)) in options.into_iter().enumerate() {
            let active = value == current;
            let on_pick = on_pick.clone();
            row = row.child(
                div()
                    .id((id, ix))
                    .px(px(10.))
                    .h(px(22.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(active, |d| {
                        d.bg(theme.menu)
                            .shadow_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                    })
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, _, cx| on_pick(this, value, cx))),
            );
        }
        row
    }

    fn hotkey_button(
        &self,
        field: HotkeyField,
        value: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme.clone();
        let recording = self.recording == Some(field);
        let id = match field {
            HotkeyField::Activation => "hk-activation",
            HotkeyField::Queue => "hk-queue",
            HotkeyField::NextCollection => "hk-next",
            HotkeyField::PrevCollection => "hk-prev",
        };
        div()
            .id(id)
            .min_w(px(120.))
            .px(px(10.))
            .h(px(26.))
            .rounded(px(7.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .font_family("Menlo")
            .text_size(px(12.))
            .bg(if recording {
                theme.accent.opacity(0.25)
            } else {
                theme.field
            })
            .border_1()
            .border_color(if recording {
                theme.accent
            } else {
                gpui::transparent_black()
            })
            .child(SharedString::from(if recording {
                "Press keys…".to_string()
            } else {
                pretty_keystroke(value)
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.recording = Some(field);
                window.focus(&this.focus_handle, cx);
                cx.notify();
            }))
    }

    // ---- sections ------------------------------------------------------------------------------

    fn render_general(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.settings(cx);
        let theme = self.theme.clone();
        let mut col = div().flex().flex_col().gap(px(8.));
        col = col.child(self.row(
            "Activate PasteLoop",
            self.hotkey_button(HotkeyField::Activation, &s.activation_hotkey, cx),
        ));
        col = col.child(self.row(
            "Paste items",
            self.segmented(
                "paste-mode",
                vec![
                    (PasteMode::ActiveApp, "To active app".into()),
                    (PasteMode::Clipboard, "To clipboard".into()),
                ],
                s.paste_mode,
                cx,
                |this, mode, cx| this.edit(cx, |s| s.paste_mode = mode),
            ),
        ));
        if s.paste_mode == PasteMode::ActiveApp {
            col = col.child(self.row(
                "Ask for permission when pasting needs it",
                self.toggle(
                    "ask-accessibility",
                    s.ask_for_accessibility,
                    cx,
                    |this, on, cx| this.edit(cx, |s| s.ask_for_accessibility = on),
                ),
            ));
        }
        col = col.child(self.row(
            "Paste text",
            self.segmented(
                "paste-text",
                vec![
                    (true, "Without formatting".into()),
                    (false, "With formatting".into()),
                ],
                s.paste_plain_text,
                cx,
                |this, plain, cx| this.edit(cx, |s| s.paste_plain_text = plain),
            ),
        ));
        col = col.child(
            self.row(
                "Keep history",
                self.segmented(
                    "retention",
                    Retention::ALL
                        .iter()
                        .map(|r| (*r, r.label().to_string()))
                        .collect(),
                    s.retention,
                    cx,
                    |this, retention, cx| this.change_retention(retention, cx),
                ),
            ),
        );
        col = col.child(self.row(
            "Back up to iCloud Drive",
            self.toggle(
                "icloud-backup",
                s.icloud_backup == Some(true),
                cx,
                |this, on, cx| this.edit(cx, |s| s.icloud_backup = Some(on)),
            ),
        ));
        {
            let backup = &self.state.read(cx).backup;
            let summary = backup.summary(crate::store::now_ms());
            let busy = matches!(
                backup.status,
                crate::state::BackupStatus::Running | crate::state::BackupStatus::Restoring
            );
            let on = s.icloud_backup == Some(true);
            col = col.child(
                div()
                    .pb(px(4.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .text_size(px(11.))
                            .text_color(theme.text_secondary)
                            .child(summary)
                            .when(on, |d| {
                                d.child("A new Mac, or this one after reinstalling macOS, restores it automatically.")
                            }),
                    )
                    .when(on && !busy, |d| {
                        d.child(self.button("backup-now".into(), "Back Up Now", false, cx, |_, _, cx| {
                            cx.emit(SettingsEvent::BackUpNow)
                        }))
                    }),
            );
        }
        if let Some((retention, count)) = self.retention_pending {
            col = col.child(
                div()
                    .py(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(format!(
                        "Keeping history for {} deletes {count} older item(s) now. This cannot be undone.",
                        retention.label()
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.))
                            .child(self.button("retention-cancel".into(), "Cancel", false, cx, |this, _, cx| {
                                this.retention_pending = None;
                                cx.notify();
                            }))
                            .child(self.button("retention-apply".into(), "Delete and Apply", true, cx, move |this, _, cx| {
                                this.retention_pending = None;
                                this.edit(cx, |s| s.retention = retention);
                            })),
                    ),
            );
        }
        col = col.child(self.row(
            "Erase history",
            if self.confirm_erase {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .child(self.button(
                        "erase-cancel".into(),
                        "Cancel",
                        false,
                        cx,
                        |this, _, cx| {
                            this.confirm_erase = false;
                            cx.notify();
                        },
                    ))
                    .child(self.button(
                        "erase-confirm".into(),
                        "Erase Everything Not in a Collection",
                        true,
                        cx,
                        |this, _, cx| {
                            this.confirm_erase = false;
                            cx.emit(SettingsEvent::EraseHistory);
                            cx.notify();
                        },
                    ))
                    .into_any_element()
            } else {
                self.button(
                    "erase".into(),
                    "Erase History…",
                    false,
                    cx,
                    |this, _, cx| {
                        this.confirm_erase = true;
                        cx.notify();
                    },
                )
                .into_any_element()
            },
        ));
        let login_on = self.login_status == LoginItemStatus::Enabled;
        let login_available = macos::running_from_bundle();
        col = col.child(
            self.row(
                "Open at login",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .when(!login_available, |d| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.text_secondary)
                                .child("Needs the .app bundle"),
                        )
                    })
                    .when_some(self.login_error.clone(), |d, err| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.text_secondary)
                                .child(err),
                        )
                    })
                    .when(
                        self.login_status == LoginItemStatus::RequiresApproval,
                        |d| {
                            d.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.text_secondary)
                                    .child("Approve in System Settings → Login Items"),
                            )
                        },
                    )
                    .child(self.toggle("login", login_on, cx, |this, on, cx| {
                        match macos::set_launch_at_login(on) {
                            Ok(()) => this.login_error = None,
                            Err(err) => this.login_error = Some(err),
                        }
                        this.login_status = macos::login_item_status();
                        this.edit(cx, |s| s.launch_at_login = Some(on));
                    })),
            ),
        );
        col = col.child(
            self.row(
                "Panel height",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.text_secondary)
                            .child(format!("{:.0} pt", s.panel_height)),
                    )
                    .child(self.button(
                        "height-reset".into(),
                        "Reset",
                        false,
                        cx,
                        |this, _, cx| {
                            this.edit(cx, |s| s.panel_height = DEFAULT_PANEL_HEIGHT);
                        },
                    )),
            ),
        );
        col.into_any_element()
    }

    fn change_retention(&mut self, retention: Retention, cx: &mut Context<Self>) {
        let current = self.settings(cx).retention;
        if retention == current {
            return;
        }
        let shorter =
            retention.cutoff_ms(0).unwrap_or(i64::MIN) > current.cutoff_ms(0).unwrap_or(i64::MIN);
        let count = if shorter {
            self.state.read(cx).count_expired_by(retention)
        } else {
            0
        };
        if count > 0 {
            self.retention_pending = Some((retention, count));
            cx.notify();
        } else {
            self.edit(cx, |s| s.retention = retention);
        }
    }

    fn render_shortcuts(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.settings(cx);
        let theme = self.theme.clone();
        let modifiers: Vec<(Modifier, String)> = Modifier::ALL
            .iter()
            .map(|m| (*m, m.label().to_string()))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(self.row("Activate PasteLoop", self.hotkey_button(HotkeyField::Activation, &s.activation_hotkey, cx)))
            .child(self.row("Activate Paste Queue", self.hotkey_button(HotkeyField::Queue, &s.queue_hotkey, cx)))
            .child(self.row("Next collection", self.hotkey_button(HotkeyField::NextCollection, &s.next_collection_key, cx)))
            .child(self.row("Previous collection", self.hotkey_button(HotkeyField::PrevCollection, &s.previous_collection_key, cx)))
            .child(self.row(
                "Quick Paste modifier",
                self.segmented("quick-mod", modifiers.clone(), s.quick_paste_modifier, cx, |this, m, cx| {
                    this.edit(cx, |s| s.quick_paste_modifier = m)
                }),
            ))
            .child(self.row(
                if s.paste_plain_text { "Paste formatted modifier" } else { "Paste plain modifier" },
                self.segmented("alternate-mod", modifiers, s.alternate_modifier, cx, |this, m, cx| {
                    this.edit(cx, |s| s.alternate_modifier = m)
                }),
            ))
            .child(
                div()
                    .pt(px(10.))
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.text_secondary)
                            .child("Click a shortcut and press the new keys. Fixed keys: Return pastes, Esc hides, ⌘F searches, ⌘N new item, ⌘T pause, ⌘, settings, ⌘Q quit."),
                    )
                    .child(self.button("reset-shortcuts".into(), "Reset to Defaults", false, cx, |this, _, cx| {
                        let defaults = Settings::default();
                        this.edit(cx, move |s| {
                            s.activation_hotkey = defaults.activation_hotkey;
                            s.queue_hotkey = defaults.queue_hotkey;
                            s.next_collection_key = defaults.next_collection_key;
                            s.previous_collection_key = defaults.previous_collection_key;
                            s.quick_paste_modifier = defaults.quick_paste_modifier;
                            s.alternate_modifier = defaults.alternate_modifier;
                        });
                        cx.emit(SettingsEvent::HotkeysChanged);
                    })),
            )
            .into_any_element()
    }

    fn render_privacy(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.settings(cx);
        let mut col = div().flex().flex_col().gap(px(8.));
        col = col.child(self.row(
            "Ignore confidential content (password managers)",
            self.toggle("concealed", s.skip_concealed, cx, |this, on, cx| {
                this.edit(cx, |s| s.skip_concealed = on)
            }),
        ));
        col = col.child(self.row(
            "Ignore transient content",
            self.toggle("transient", s.skip_transient, cx, |this, on, cx| {
                this.edit(cx, |s| s.skip_transient = on)
            }),
        ));
        col = col.child(self.row(
            "Show panel during screen sharing",
            self.toggle(
                "sharing",
                !s.hide_from_screen_sharing,
                cx,
                |this, on, cx| this.edit(cx, |s| s.hide_from_screen_sharing = !on),
            ),
        ));
        let theme = self.theme.clone();
        col = col.child(
            div()
                .pt(px(12.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Ignore applications"),
                        )
                        .child(self.button(
                            "reset-ignore".into(),
                            "Reset to password managers",
                            false,
                            cx,
                            |this, _, cx| {
                                let defaults = crate::settings::default_ignored_apps();
                                this.ignore_input
                                    .update(cx, |i, cx| i.set_text(defaults.join("\n"), cx));
                                this.edit(cx, |s| s.ignored_apps = defaults);
                            },
                        )),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_secondary)
                        .child(
                            "One bundle identifier per line. Copies made in these apps are never saved.",
                        ),
                )
                .child(
                    div()
                        .id("ignore-box")
                        // Grows with the list; the settings page scrolls when it gets long.
                        .min_h(px(160.))
                        .p(px(10.))
                        .rounded(px(8.))
                        .bg(theme.field)
                        .font_family("Menlo")
                        .text_size(px(12.))
                        .text_color(theme.text)
                        .child(self.ignore_input.clone()),
                ),
        );
        col.into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.settings(cx);
        let rows = s.bar_rows();
        let dec = self.button("rows-dec".into(), "−", false, cx, {
            let h = s.panel_height_for_rows(rows.saturating_sub(1).max(1));
            move |this, _, cx| this.edit(cx, |s| s.panel_height = h)
        });
        let inc = self.button("rows-inc".into(), "+", false, cx, {
            let h = s.panel_height_for_rows(rows + 1);
            move |this, _, cx| this.edit(cx, |s| s.panel_height = h)
        });
        let rows_control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(dec)
            .child(
                div()
                    .min_w(px(64.))
                    .flex()
                    .justify_center()
                    .text_size(px(13.))
                    .child(SharedString::from(format!(
                        "{rows} row{}",
                        if rows == 1 { "" } else { "s" }
                    ))),
            )
            .child(inc);
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                self.stepper_row("Card width", s.card_width, "pt", 10.0, cx, |this, v, cx| {
                    this.edit(cx, |s| s.card_width = Settings::clamp_card_width(v))
                }),
            )
            .child(self.stepper_row(
                "Card height",
                s.card_max_height,
                "pt",
                10.0,
                cx,
                |this, v, cx| {
                    this.edit(cx, |s| {
                        // Keep the row count; the bar grows or shrinks to fit the new height.
                        let rows = s.bar_rows();
                        s.card_max_height = Settings::clamp_card_max_height(v);
                        s.panel_height = s.panel_height_for_rows(rows);
                    })
                },
            ))
            .child(self.row("Rows in the bar", rows_control))
            .child(self.text_size_slider(cx))
            .child(
                div()
                    .pt(px(8.))
                    .text_size(px(12.))
                    .text_color(self.theme.text_secondary)
                    .child(
                        "The bar stays open while you are here, so changes show on it live. Drag \
                         its top edge to resize; it snaps to whole rows when you let go.",
                    ),
            )
            .into_any_element()
    }

    /// An Apple-style text-size slider: small "A" and large "A" ends that step, and a knob you
    /// drag along the track. The track has a fixed width, so the drag maps pointer movement to a
    /// scale change without needing element bounds.
    fn text_size_slider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::{MAX_TEXT_SCALE, MIN_TEXT_SCALE};
        const TRACK_W: f32 = 180.0;
        const KNOB: f32 = 18.0;
        const STEP: f32 = 0.05;
        let theme = self.theme.clone();
        let scale = self.settings(cx).text_scale;
        let range = MAX_TEXT_SCALE - MIN_TEXT_SCALE;
        let frac = ((scale - MIN_TEXT_SCALE) / range).clamp(0.0, 1.0);
        let knob_x = frac * (TRACK_W - KNOB);

        let dim = theme.text_secondary;
        let full = theme.text;
        let end_a = |id: &'static str, size: f32, delta: f32, cx: &mut Context<Self>| {
            div()
                .id(id)
                .w(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_size(px(size))
                .text_color(dim)
                .hover(move |s| s.text_color(full))
                .child("A")
                .on_click(cx.listener(move |this, _, _, cx| {
                    let next = Settings::clamp_text_scale(this.settings(cx).text_scale + delta);
                    this.edit(cx, |s| s.text_scale = next);
                }))
        };
        let small_a = end_a("text-smaller", 11.0, -STEP, cx);
        let large_a = end_a("text-larger", 20.0, STEP, cx);

        let knob = div()
            .absolute()
            .top(px(0.))
            .left(px(knob_x))
            .size(px(KNOB))
            .rounded_full()
            .bg(if theme.dark {
                gpui::rgb(0xF5F5F7).into()
            } else {
                gpui::white()
            })
            .border_1()
            .border_color(theme.separator)
            .shadow_sm();

        let track = div()
            .id("text-size-track")
            .relative()
            .w(px(TRACK_W))
            .h(px(KNOB))
            .flex()
            .items_center()
            .cursor_pointer()
            // The rail, centered in the track's height.
            .child(
                div()
                    .absolute()
                    .top(px(KNOB / 2.0 - 2.0))
                    .left(px(0.))
                    .w(px(TRACK_W))
                    .h(px(4.))
                    .rounded_full()
                    .bg(theme.field),
            )
            // Filled portion up to the knob.
            .child(
                div()
                    .absolute()
                    .top(px(KNOB / 2.0 - 2.0))
                    .left(px(0.))
                    .w(px((knob_x + KNOB / 2.0).max(0.0)))
                    .h(px(4.))
                    .rounded_full()
                    .bg(theme.accent),
            )
            .child(knob)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.text_slider_drag =
                        Some((f32::from(ev.position.x), this.settings(cx).text_scale));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                let Some((start_x, start_scale)) = this.text_slider_drag else {
                    return;
                };
                let dx = f32::from(ev.position.x) - start_x;
                let next =
                    Settings::clamp_text_scale(start_scale + (dx / (TRACK_W - KNOB)) * range);
                if (next - this.settings(cx).text_scale).abs() > f32::EPSILON {
                    this.edit(cx, |s| s.text_scale = next);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.text_slider_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            );

        let percent = (scale * 100.0).round() as i32;
        let control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(small_a)
            .child(track)
            .child(large_a)
            .child(
                div()
                    .min_w(px(44.))
                    .flex()
                    .justify_end()
                    .text_size(px(12.))
                    .text_color(self.theme.text_secondary)
                    .child(SharedString::from(format!("{percent}%"))),
            );
        self.row("Text size", control)
    }

    /// A `label   [−  value  +]` row that steps a numeric setting.
    fn stepper_row(
        &self,
        label: &'static str,
        value: f32,
        unit: &str,
        step: f32,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, f32, &mut Context<Self>) + 'static + Clone,
    ) -> impl IntoElement {
        let dec = self.button(format!("{label}-dec").into(), "−", false, cx, {
            let a = apply.clone();
            move |this, _, cx| a(this, value - step, cx)
        });
        let inc = self.button(format!("{label}-inc").into(), "+", false, cx, {
            let a = apply.clone();
            move |this, _, cx| a(this, value + step, cx)
        });
        let readout = div()
            .min_w(px(64.))
            .flex()
            .justify_center()
            .text_size(px(13.))
            .child(SharedString::from(format!("{value:.0} {unit}")));
        self.row(
            label,
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(dec)
                .child(readout)
                .child(inc),
        )
    }

    fn render_migration(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme.clone();
        let found = crate::import::default_paste_db().is_some();
        let label = if self.migrating {
            "Migrating…".to_string()
        } else {
            "Migrate from Paste".to_string()
        };
        let mut button = div()
            .id("migrate-paste")
            .px(px(12.))
            .h(px(28.))
            .rounded(px(7.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .bg(theme.field);
        if found && !self.migrating {
            button = button
                .cursor_pointer()
                .hover(|s| s.bg(theme.accent).text_color(theme.on_accent))
                .on_click(cx.listener(Self::start_migration));
        } else {
            button = button.text_color(theme.text_secondary);
        }
        let status = self.migrate_status.clone().unwrap_or_else(|| {
            if found {
                "Imports your clipboard history and collections from the Paste app.".to_string()
            } else {
                "The Paste app was not found on this Mac.".to_string()
            }
        });
        div()
            .pt(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(13.))
                    .child("Migrate from Paste"),
            )
            .child(button.child(label))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(status)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.text_secondary)
                    .child(
                        "Items keep their original dates, so set “Keep history” to Forever first \
                         if you want to keep older ones.",
                    ),
            )
            .into_any_element()
    }

    fn start_migration(&mut self, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.migrating {
            return;
        }
        let Some(db) = crate::import::default_paste_db() else {
            self.migrate_status = Some("The Paste app was not found on this Mac.".into());
            cx.notify();
            return;
        };
        let store = std::sync::Arc::clone(&self.state.read(cx).store);
        self.migrating = true;
        self.migrate_status = Some("Migrating… this can take a minute for a large history.".into());
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::import::import_from_paste(&store, &db, None, false) })
                .await;
            let message = match &result {
                Ok(s) => format!(
                    "Imported {} item(s) and {} collection(s), added {} file preview(s); skipped {}.",
                    s.imported, s.collections_created, s.previews_added, s.skipped
                ),
                Err(err) => format!("Migration failed: {err}"),
            };
            cx.update(|cx| {
                if let Some(view) = view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.migrating = false;
                        view.migrate_status = Some(message);
                        cx.notify();
                    });
                }
                state.update(cx, |s, cx| {
                    s.refresh_collections();
                    s.reload_list();
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn render_integrations(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = self.settings(cx);
        let theme = self.theme.clone();
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "pasteloop".into());
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(self.row(
                "Link previews (fetches titles and images over the network)",
                self.toggle("network", s.link_previews_network, cx, |this, on, cx| {
                    this.edit(cx, |s| s.link_previews_network = on)
                }),
            ))
            .child(self.row(
                "Paste and drag images as files",
                self.toggle("images-files", s.images_as_files, cx, |this, on, cx| {
                    this.edit(cx, |s| s.images_as_files = on)
                }),
            ))
            .child(
                div()
                    .pt(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Command line"),
                    )
                    .child(
                        div()
                            .text_color(theme.text_secondary)
                            .child("Add the binary to your PATH or call it by its full path:"),
                    )
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_size(px(11.))
                            .child(SharedString::from(exe)),
                    )
                    .child(div().text_color(theme.text_secondary).child(
                        "pasteloop recent, pasteloop search <query>, pasteloop get <id>, pasteloop add --text, pasteloop export --json",
                    )),
            )
            .into_any_element()
    }

    fn render_about(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme.clone();
        let stats = &self.about_stats;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(12.))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("PasteLoop"),
            )
            .child(format!("Version {}", env!("CARGO_PKG_VERSION")))
            .child(
                div()
                    .text_color(theme.text_secondary)
                    .child("A native clipboard manager for macOS. MIT License."),
            )
            .child(format!(
                "{} items · {} collections · {:.1} MB of blobs",
                stats.items,
                stats.collections,
                stats.blob_bytes as f64 / 1_048_576.0
            ))
            .child(
                div()
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(format!(
                        "Data: {}",
                        crate::settings::data_dir().display()
                    ))),
            )
            .child(
                div()
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(format!(
                        "Log: {}/pasteloop.log",
                        crate::settings::log_dir().display()
                    ))),
            )
            .child(
                self.button("github".into(), "Source on GitHub", false, cx, |_, _, _| {
                    macos::open_url("https://github.com/initmaks/pasteloop");
                }),
            )
            .into_any_element()
    }
}

/// "shift-cmd-v" → "⇧⌘V"
pub fn pretty_keystroke(keystroke: &str) -> String {
    let mut out = String::new();
    let mut key = String::new();
    for token in keystroke.split('-') {
        match token {
            "ctrl" | "control" => out.push('⌃'),
            "alt" | "option" => out.push('⌥'),
            "shift" => out.push('⇧'),
            "cmd" | "command" | "super" => out.push('⌘'),
            other => key = other.to_string(),
        }
    }
    let key = match key.as_str() {
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "space" => "Space".to_string(),
        "enter" => "↩".to_string(),
        "escape" => "⎋".to_string(),
        "tab" => "⇥".to_string(),
        other => other.to_uppercase(),
    };
    out.push_str(&key);
    out
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.theme = Theme::for_window(window);
        let colors = self.theme.input_colors();
        if self.ignore_input.read(cx).colors != colors {
            self.ignore_input.update(cx, |i, _| i.colors = colors);
        }
        let theme = self.theme.clone();
        let mut sidebar = div()
            .w(px(168.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(12.))
            .bg(theme.field.opacity(0.5));
        for section in Section::ALL {
            let active = self.section == section;
            sidebar = sidebar.child(
                div()
                    .id(section.label())
                    .px(px(10.))
                    .h(px(28.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(14.))
                    .when(active, |d| d.bg(theme.accent).text_color(theme.on_accent))
                    .when(!active, |d| d.hover(|s| s.bg(theme.field)))
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        this.recording = None;
                        if section == Section::About {
                            this.about_stats =
                                this.state.read(cx).store.stats().unwrap_or_default();
                        }
                        cx.notify();
                    })),
            );
        }
        let content = match self.section {
            Section::General => self.render_general(cx),
            Section::Appearance => self.render_appearance(cx),
            Section::Shortcuts => self.render_shortcuts(cx),
            Section::Privacy => self.render_privacy(cx),
            Section::Integrations => self.render_integrations(cx),
            Section::Migration => self.render_migration(cx),
            Section::About => self.render_about(cx),
        };
        div()
            .id("settings-root")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_row()
            .bg(theme.menu)
            .text_color(theme.text)
            .font_family(".SystemUIFont")
            .text_size(px(13.))
            .child(sidebar)
            .child(
                // min_w(0) lets the content shrink to the window width; otherwise one long line
                // forces it wider and clips every row's right side.
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_y_scroll()
                    .p(px(22.))
                    .child(content),
            )
    }
}

/// Logs a saved settings change.
pub fn log_change(settings: &Settings) {
    info!(
        "settings saved: hotkey={} queue={} retention={:?} paste={:?} plain_text={}",
        settings.activation_hotkey,
        settings.queue_hotkey,
        settings.retention,
        settings.paste_mode,
        settings.paste_plain_text
    );
    if settings.ignored_apps.len() > 50 {
        warn!(
            "ignore list is unusually long ({})",
            settings.ignored_apps.len()
        );
    }
}
