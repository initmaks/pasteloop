//! Paste-out: builds the pasteboard representations for one or many items (plain or rich),
//! decides how a multi-selection is delivered, and drives the write, hide, and Cmd-V injection.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{AsyncApp, Entity};
use tracing::{info, warn};

use crate::macos::{self, NativeWindow};
use crate::settings::{PasteMode, cache_dir};
use crate::state::AppState;
use crate::store::{Item, Kind, Store};

/// Everything written to the general pasteboard for one paste.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Representations {
    pub plain: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub html: Option<Vec<u8>>,
    pub url: Option<String>,
    pub image_png: Option<Vec<u8>>,
    /// A temp file for apps that only accept file drops (images as files).
    pub image_file: Option<PathBuf>,
    pub file_paths: Vec<String>,
}

impl Representations {
    pub fn is_empty(&self) -> bool {
        self.plain.is_none()
            && self.rtf.is_none()
            && self.html.is_none()
            && self.url.is_none()
            && self.image_png.is_none()
            && self.file_paths.is_empty()
    }
}

/// How the selection is delivered: one pasteboard write, or one per item with a delay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    One(Representations),
    Sequence(Vec<Representations>),
}

/// Delay between writes of a mixed multi-selection.
pub const SEQUENCE_GAP: Duration = Duration::from_millis(80);

/// Time given to WindowServer to hand key focus back to the target app after the panel hides,
/// before the injected ⌘V is posted. One run-loop turn is enough; this leaves margin.
const FOCUS_SETTLE: Duration = Duration::from_millis(45);

/// What a paste writes. The default writes every item as it was captured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Form {
    /// Text-like items without formatting: no RTF, HTML, or URL type.
    pub plain_text: bool,
    /// Images and files as their paths, one per line (an image as a PNG in the cache).
    pub paths: bool,
}

impl Form {
    /// Return pastes text in the user's default style and images and files as themselves;
    /// the alternate modifier switches text to the other style and pastes paths.
    pub fn for_paste(plain_by_default: bool, alternate: bool) -> Form {
        Form {
            plain_text: plain_by_default != alternate,
            paths: alternate,
        }
    }
}

/// Representations for one item in the given form.
pub fn representations(
    item: &Item,
    store: &Store,
    form: Form,
    images_as_files: bool,
) -> Representations {
    let mut reps = Representations::default();
    match item.kind {
        Kind::Text => {
            reps.plain = store
                .full_text(item)
                .ok()
                .flatten()
                .or_else(|| item.text.clone());
            if !form.plain_text
                && let Some(blob) = &item.rich_blob
                && let Ok(bytes) = std::fs::read(store.blob_path(blob))
            {
                match item.rich_type.as_deref() {
                    Some("public.rtf") => reps.rtf = Some(bytes),
                    Some("public.html") => reps.html = Some(bytes),
                    _ => {}
                }
            }
        }
        Kind::Link => {
            reps.plain = item.text.clone();
            if !form.plain_text {
                reps.url = item.text.clone();
            }
        }
        Kind::Color => {
            reps.plain = item.text.clone();
        }
        Kind::Image => {
            if let Some(blob) = &item.image_blob
                && let Ok(bytes) = std::fs::read(store.blob_path(blob))
            {
                let png = if bytes.starts_with(b"\x89PNG") {
                    Some(bytes)
                } else {
                    macos::image_to_png(&bytes)
                };
                if let Some(png) = png {
                    if form.paths {
                        reps.plain = write_temp_png(blob, &png).map(|p| p.display().to_string());
                    } else {
                        if images_as_files {
                            reps.image_file = write_temp_png(blob, &png);
                        }
                        reps.image_png = Some(png);
                    }
                }
            }
        }
        Kind::File if form.paths => reps.plain = Some(item.files.join("\n")),
        Kind::File => {
            reps.file_paths = item.files.clone();
        }
    }
    reps
}

fn write_temp_png(blob: &str, png: &[u8]) -> Option<PathBuf> {
    let dir = cache_dir().join("paste");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.png", &blob[..blob.len().min(16)]));
    std::fs::write(&path, png).ok()?;
    Some(path)
}

/// One write when every item is text-like (joined with newlines) or every item is a file
/// (all URLs at once); otherwise a sequence, one item per write.
pub fn plan(items: &[Item], store: &Store, form: Form, images_as_files: bool) -> Plan {
    let all_textual = items
        .iter()
        .all(|i| matches!(i.kind, Kind::Text | Kind::Link | Kind::Color));
    let all_files = items.iter().all(|i| i.kind == Kind::File);
    if items.len() <= 1 {
        let reps = items
            .first()
            .map(|i| representations(i, store, form, images_as_files))
            .unwrap_or_default();
        return Plan::One(reps);
    }
    if all_textual {
        // Formatting cannot be joined, so several text items always paste as plain text.
        let plain = Form {
            plain_text: true,
            paths: false,
        };
        let parts: Vec<String> = items
            .iter()
            .filter_map(|i| representations(i, store, plain, false).plain)
            .collect();
        return Plan::One(Representations {
            plain: Some(parts.join("\n")),
            ..Default::default()
        });
    }
    if all_files {
        let paths: Vec<String> = items.iter().flat_map(|i| i.files.clone()).collect();
        return Plan::One(if form.paths {
            Representations {
                plain: Some(paths.join("\n")),
                ..Default::default()
            }
        } else {
            Representations {
                file_paths: paths,
                ..Default::default()
            }
        });
    }
    Plan::Sequence(
        items
            .iter()
            .map(|i| representations(i, store, form, images_as_files))
            .filter(|r| !r.is_empty())
            .collect(),
    )
}

/// Outcome reported back to the panel for the toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pasted,
    CopiedOnly,
    NeedsAccessibility,
    /// Trusted now, but not when this process started: macOS drops the keystrokes until
    /// the app is relaunched.
    NeedsRelaunch,
}

/// Whether a paste can be injected into the target app, and the outcome to report.
///
/// Injection needs "paste to active app", Accessibility trust, and a known target. Trust
/// must also have been there when this process started: macOS decides at launch whether a
/// process may post keystrokes, and a grant made later is honored only after a relaunch
/// (`AXIsProcessTrusted` says yes, but the WindowServer silently drops the Cmd-V).
pub fn decide(
    mode: PasteMode,
    trusted: bool,
    trusted_at_launch: bool,
    has_target: bool,
) -> (bool, Outcome) {
    if mode != PasteMode::ActiveApp {
        return (false, Outcome::CopiedOnly);
    }
    if !trusted {
        return (false, Outcome::NeedsAccessibility);
    }
    if !trusted_at_launch {
        return (false, Outcome::NeedsRelaunch);
    }
    if !has_target {
        return (false, Outcome::CopiedOnly);
    }
    (true, Outcome::Pasted)
}

/// Writes the plan to the pasteboard and delivers Cmd-V to the target app, or falls back to a
/// clipboard-only paste. Runs as a task so window ordering happens outside GPUI borrows.
pub async fn perform(
    plan: Plan,
    item_ids: Vec<i64>,
    state: Entity<AppState>,
    native: NativeWindow,
    cx: &mut AsyncApp,
) -> Outcome {
    let (mode, target, trusted, trusted_at_launch, store) = cx.update(|cx| {
        let s = state.read(cx);
        (
            s.settings.paste_mode,
            s.target_app.clone(),
            macos::accessibility_trusted(),
            s.accessibility_at_launch,
            Arc::clone(&s.store),
        )
    });
    let (inject, outcome) = decide(mode, trusted, trusted_at_launch, target.is_some());

    let mut reps: Vec<Representations> = match plan {
        Plan::One(r) => vec![r],
        Plan::Sequence(v) => v,
    };
    if reps.iter().all(|r| r.is_empty()) {
        warn!("paste: nothing to write for the selection");
        return Outcome::CopiedOnly;
    }
    if !inject {
        // Without injection a sequence makes no sense: leave the first item on the pasteboard.
        reps.truncate(1);
    }
    // Logged as recorded, so a copy-only outcome still shows which app was in front.
    let wanted = target.as_ref().map(|a| (a.pid, a.name.clone()));
    let target = target.filter(|_| inject);
    // Our own Cmd-V must not advance an active Paste Queue.
    cx.update(|cx| {
        state.update(cx, |s, _| {
            if s.queue.active {
                s.queue.suppress_taps_until =
                    Some(std::time::Instant::now() + Duration::from_millis(400));
            }
        });
    });
    if let Some(app) = &target {
        // The panel never activates us, but a settings window might have; then the target must
        // be brought back before it can receive the keystroke.
        let front = macos::frontmost_app();
        if front.as_ref().map(|f| f.pid) != Some(app.pid) {
            macos::activate_pid(app.pid);
            // Activation lands on a later run-loop turn; give it a few frames.
            for _ in 0..10 {
                cx.background_executor()
                    .timer(Duration::from_millis(15))
                    .await;
                if macos::pid_is_active(app.pid) {
                    break;
                }
            }
            if !macos::pid_is_active(app.pid) {
                warn!("target pid {} did not activate in time", app.pid);
            }
        }
    }
    if inject {
        // Hide the panel before injecting. A ⌘V posted to the session tap is delivered to
        // whichever window has key focus; the panel held key focus for its search field and
        // selection, so it must resign it first and hand focus back to the target app.
        native.order_out();
        // Let WindowServer settle key focus onto the target before the keystroke arrives.
        cx.background_executor().timer(FOCUS_SETTLE).await;
    }
    for (i, r) in reps.iter().enumerate() {
        if i > 0 {
            cx.background_executor().timer(SEQUENCE_GAP).await;
        }
        let count = macos::write_pasteboard(r);
        cx.update(|cx| {
            state.update(cx, |s, _| s.own_change_count = Some(count));
        });
        if inject {
            macos::post_cmd_v();
        }
    }
    info!(
        "paste: writes={} inject={} outcome={:?} target={:?}",
        reps.len(),
        inject,
        outcome,
        wanted
    );
    // The pasteboard now holds this item; an active queue puts its head back after a beat.
    crate::queue::after_own_write(state.clone(), cx);
    // Mark the items used, which also moves them to the front of history.
    let touched = cx.background_executor().spawn(async move {
        for id in item_ids {
            if let Err(err) = store.touch_used(id) {
                warn!("touch_used({id}) failed: {err:#}");
            }
        }
    });
    touched.await;
    cx.update(|cx| {
        state.update(cx, |s, cx| {
            if outcome == Outcome::Pasted {
                s.panel_visible = false;
            }
            s.reload_list();
            cx.notify();
        });
    });
    outcome
}

/// Writes the plan without injecting (Cmd-C on the panel); a sequence contributes its first
/// item. Returns the pasteboard change count.
pub fn copy_only(plan: Plan) -> i64 {
    let reps = match plan {
        Plan::One(r) => r,
        Plan::Sequence(v) => v.into_iter().next().unwrap_or_default(),
    };
    macos::write_pasteboard(&reps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewItem, Store};

    const AS_IS: Form = Form {
        plain_text: false,
        paths: false,
    };
    const PLAIN: Form = Form {
        plain_text: true,
        paths: false,
    };
    const PATHS: Form = Form {
        plain_text: true,
        paths: true,
    };

    fn store_with_items() -> (tempfile::TempDir, Store, Vec<Item>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut items = Vec::new();
        let mut rich = NewItem::text("Hello");
        rich.rich = Some((b"{\\rtf1 Hello}".to_vec(), "public.rtf"));
        items.push(store.insert(&rich).unwrap().0);
        let mut link = NewItem::text("https://example.com/a");
        link.kind = Some(Kind::Link);
        items.push(store.insert(&link).unwrap().0);
        let files = NewItem {
            kind: Some(Kind::File),
            files: vec!["/tmp/a.txt".into(), "/tmp/b.txt".into()],
            ..Default::default()
        };
        items.push(store.insert(&files).unwrap().0);
        let mut color = NewItem::text("#ABCDEF");
        color.kind = Some(Kind::Color);
        items.push(store.insert(&color).unwrap().0);
        (dir, store, items)
    }

    #[test]
    fn single_item_representations() {
        let (_d, store, items) = store_with_items();
        let text = representations(&items[0], &store, AS_IS, false);
        assert_eq!(text.plain.as_deref(), Some("Hello"));
        assert!(text.rtf.is_some());
        let plain = representations(&items[0], &store, PLAIN, false);
        assert!(plain.rtf.is_none());
        assert_eq!(plain.plain.as_deref(), Some("Hello"));

        let link = representations(&items[1], &store, AS_IS, false);
        assert_eq!(link.url.as_deref(), Some("https://example.com/a"));
        assert_eq!(link.plain.as_deref(), Some("https://example.com/a"));
        assert!(
            representations(&items[1], &store, PLAIN, false)
                .url
                .is_none()
        );

        let files = representations(&items[2], &store, AS_IS, false);
        assert_eq!(files.file_paths.len(), 2);
        assert!(files.plain.is_none());
        let files_plain = representations(&items[2], &store, PATHS, false);
        assert!(files_plain.file_paths.is_empty());
        assert_eq!(files_plain.plain.as_deref(), Some("/tmp/a.txt\n/tmp/b.txt"));

        let color = representations(&items[3], &store, AS_IS, false);
        assert_eq!(color.plain.as_deref(), Some("#ABCDEF"));
    }

    #[test]
    fn multi_selection_plans() {
        let (_d, store, items) = store_with_items();
        let textual = plan(
            &[items[0].clone(), items[1].clone(), items[3].clone()],
            &store,
            AS_IS,
            false,
        );
        assert_eq!(
            textual,
            Plan::One(Representations {
                plain: Some("Hello\nhttps://example.com/a\n#ABCDEF".into()),
                ..Default::default()
            })
        );
        let files = plan(&[items[2].clone(), items[2].clone()], &store, AS_IS, false);
        match files {
            Plan::One(r) => assert_eq!(r.file_paths.len(), 4),
            _ => panic!("files paste at once"),
        }
        let mixed = plan(&[items[0].clone(), items[2].clone()], &store, AS_IS, false);
        assert!(matches!(mixed, Plan::Sequence(ref v) if v.len() == 2));
        assert_eq!(
            plan(&[], &store, AS_IS, false),
            Plan::One(Representations::default())
        );
    }

    #[test]
    fn injection_requires_trust_at_launch() {
        use crate::settings::PasteMode::{ActiveApp, Clipboard};
        assert_eq!(decide(ActiveApp, true, true, true), (true, Outcome::Pasted));
        assert_eq!(
            decide(ActiveApp, true, true, false),
            (false, Outcome::CopiedOnly)
        );
        assert_eq!(
            decide(ActiveApp, false, false, true),
            (false, Outcome::NeedsAccessibility)
        );
        // Granted while running: macOS drops the keystrokes until the app is relaunched.
        assert_eq!(
            decide(ActiveApp, true, false, true),
            (false, Outcome::NeedsRelaunch)
        );
        assert_eq!(
            decide(Clipboard, true, true, true),
            (false, Outcome::CopiedOnly)
        );
    }

    #[test]
    fn return_follows_the_text_default_and_the_modifier_flips_it() {
        // Plain text by default: Return strips formatting, the modifier keeps it and pastes paths.
        assert_eq!(Form::for_paste(true, false), PLAIN);
        assert!(!Form::for_paste(true, true).plain_text);
        assert!(Form::for_paste(true, true).paths);
        // Formatted by default: the modifier is the plain-text-and-paths paste.
        assert_eq!(Form::for_paste(false, false), AS_IS);
        assert_eq!(Form::for_paste(false, true), PATHS);

        let (_d, store, items) = store_with_items();
        let formatted = representations(&items[0], &store, Form::for_paste(true, true), false);
        assert!(formatted.rtf.is_some());
        let files = representations(&items[2], &store, Form::for_paste(true, false), false);
        assert_eq!(files.file_paths.len(), 2);
    }
}
