//! The one application state entity: settings, pause state, the active list and its selection,
//! search state, undo, and the paste target. Views observe it and hold no domain state.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::macos::FrontApp;
use crate::settings::Settings;
use crate::store::{
    AppInfo, Collection, Cursor, Item, Kind, PAGE_SIZE, Retention, SearchQuery, Store, now_ms,
};

/// Max remembered deletions for Cmd-Z.
const UNDO_DEPTH: usize = 20;
pub const TOAST_DURATION: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    History,
    Collection(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseState {
    Until(Instant),
    Indefinite,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The card that keyboard navigation moves from.
    pub cursor: Option<usize>,
    /// Where a Shift-extension started.
    pub anchor: Option<usize>,
    pub set: BTreeSet<usize>,
}

impl Selection {
    pub fn single(ix: usize) -> Selection {
        Selection {
            cursor: Some(ix),
            anchor: Some(ix),
            set: BTreeSet::from([ix]),
        }
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.set.contains(&ix)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateRange {
    Today,
    Yesterday,
    Last7Days,
    Last30Days,
}

impl DateRange {
    pub const ALL: [DateRange; 4] = [
        DateRange::Today,
        DateRange::Yesterday,
        DateRange::Last7Days,
        DateRange::Last30Days,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DateRange::Today => "Today",
            DateRange::Yesterday => "Yesterday",
            DateRange::Last7Days => "Last 7 days",
            DateRange::Last30Days => "Last 30 days",
        }
    }

    /// `(since, until)` in unix ms, computed in the local time zone.
    pub fn bounds(self, now: chrono::DateTime<chrono::Local>) -> (Option<i64>, Option<i64>) {
        // Calendar days, not multiples of 24 h, so a daylight-saving change does not shift them.
        let midnight = |days_ago: u64| {
            let date = now.date_naive() - chrono::Days::new(days_ago);
            date.and_time(chrono::NaiveTime::MIN)
                .and_local_timezone(chrono::Local)
                .earliest()
                .map_or(now.timestamp_millis(), |t| t.timestamp_millis())
        };
        match self {
            DateRange::Today => (Some(midnight(0)), None),
            DateRange::Yesterday => (Some(midnight(1)), Some(midnight(0))),
            DateRange::Last7Days => (Some(midnight(6)), None),
            DateRange::Last30Days => (Some(midnight(29)), None),
        }
    }
}

/// A filter shown as a chip inside the search field.
#[derive(Clone, Debug, PartialEq)]
pub enum Chip {
    Kind(Kind),
    App { bundle_id: String, name: String },
    Date(DateRange),
    Collection { id: i64, name: String },
}

impl Chip {
    pub fn label(&self) -> String {
        match self {
            Chip::Kind(k) => k.label().to_string(),
            Chip::App { name, .. } => name.clone(),
            Chip::Date(d) => d.label().to_string(),
            Chip::Collection { name, .. } => name.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub active: bool,
    pub text: String,
    pub chips: Vec<Chip>,
    pub query: SearchQuery,
    /// The list the results replaced, so Cmd-G and Escape can return to it.
    pub home: Option<Tab>,
}

impl SearchState {
    /// The terms the user typed, split on whitespace.
    pub fn terms(&self) -> Vec<String> {
        self.text
            .split_whitespace()
            .map(|t| t.to_string())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.chips.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub until: Instant,
}

/// The Paste Queue queue, in capture order; the head depends on `newest_first`.
#[derive(Default)]
pub struct PasteQueue {
    pub active: bool,
    pub newest_first: bool,
    pub tap_available: bool,
    pub items: Vec<Item>,
    /// The item currently on the pasteboard, so the head is not rewritten needlessly.
    pub loaded_id: Option<i64>,
    /// Cmd-V key-ups before this instant are ours (injected) and must not advance the queue.
    pub suppress_taps_until: Option<Instant>,
}

impl PasteQueue {
    /// The queue in paste order.
    pub fn ordered_items(&self) -> Vec<Item> {
        let mut items = self.items.clone();
        if self.newest_first {
            items.reverse();
        }
        items
    }

    pub fn head(&self) -> Option<&Item> {
        if self.newest_first {
            self.items.last()
        } else {
            self.items.first()
        }
    }

    pub fn push(&mut self, item: Item) {
        if !self.items.iter().any(|i| i.id == item.id) {
            self.items.push(item);
        }
    }

    pub fn pop_head(&mut self) -> Option<Item> {
        if self.items.is_empty() {
            None
        } else if self.newest_first {
            self.items.pop()
        } else {
            Some(self.items.remove(0))
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.loaded_id = None;
    }
}

/// How long a dismissed panel keeps its place. Reopening within this window continues the last
/// browse (tab, search, card); after it, or once an item was used or something new was copied,
/// the panel opens on the newest card.
pub const RESUME_WINDOW: Duration = Duration::from_secs(60);

pub struct AppState {
    pub store: Arc<Store>,
    pub settings: Settings,
    pub pause: Option<PauseState>,
    /// The pasteboard change count of our last own write, which the poller must ignore.
    pub own_change_count: Option<i64>,
    pub collections: Vec<Collection>,
    pub tab: Tab,
    /// The loaded rows of the current list. Shared so a frame can hold them without copying.
    pub items: Arc<Vec<Item>>,
    pub has_more: bool,
    /// Rows in the whole current list, for the position counter; `None` for a search, whose
    /// size is only known as far as it is loaded.
    pub total: Option<usize>,
    pub selection: Selection,
    pub search: SearchState,
    pub target_app: Option<FrontApp>,
    pub panel_visible: bool,
    /// Set when the panel is dismissed without using an item; see `RESUME_WINDOW`.
    pub resume_until: Option<Instant>,
    pub apps: Arc<HashMap<String, AppInfo>>,
    pub undo: Vec<Vec<i64>>,
    pub toast: Option<Toast>,
    /// Set after the first Accessibility explanation so the sheet shows once per launch.
    pub accessibility_prompted: bool,
    /// Trust as measured at startup. macOS honors a grant made while the app runs only
    /// after a relaunch, so injection needs this too (see `paste::decide`).
    pub accessibility_at_launch: bool,
    /// Set after the relaunch explanation so the sheet shows once per launch.
    pub relaunch_prompted: bool,
    /// Item the user asked to jump to (Cmd-G) with a temporary highlight.
    pub highlight_item: Option<(i64, Instant)>,
    /// The item shown in the floating preview window above the panel (Space on a card).
    pub preview: Option<Item>,
    /// True while the Settings window is open, so the bar stays visible for a live preview.
    pub settings_open: bool,
    pub queue: PasteQueue,
    pub backup: BackupState,
}

/// Where the iCloud Drive backup stands, for Settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum BackupStatus {
    #[default]
    Off,
    /// iCloud Drive is off on this Mac, or this instance runs on a test data directory.
    Unavailable,
    Idle,
    Running,
    Restoring,
    Failed(String),
}

/// The backup's status and the change tracking that decides when the next one is due.
#[derive(Debug, Default)]
pub struct BackupState {
    pub status: BackupStatus,
    /// When the last backup finished (ms) and how many items it held.
    pub last_at: Option<i64>,
    pub last_items: i64,
    /// The last snapshot, and whether iCloud Drive has uploaded it (`None` when unknown).
    pub snapshot: Option<std::path::PathBuf>,
    pub uploaded: Option<bool>,
    /// Set by "Back Up Now" and by turning backup on: run at the next check.
    pub requested: bool,
    /// The library fingerprint at the last backup, and as last seen.
    pub backed_up: Option<String>,
    pub seen: Option<(String, i64)>,
    /// When the latest change not yet backed up was seen, and whether the changes removed items.
    pub changed_at: Option<Instant>,
    pub deleted: bool,
}

impl BackupState {
    /// Records the current fingerprint and live item count; returns whether a backup is due.
    pub fn observe(&mut self, fingerprint: String, live: i64, now: Instant) -> bool {
        if self.backed_up.as_deref() == Some(fingerprint.as_str()) {
            self.changed_at = None;
            self.deleted = false;
        } else if self.seen.as_ref().map(|(f, _)| f) != Some(&fingerprint) {
            self.changed_at = Some(now);
            if self.seen.as_ref().is_some_and(|(_, before)| live < *before) {
                self.deleted = true;
            }
        }
        self.seen = Some((fingerprint, live));
        let since_backup = self
            .last_at
            .map(|at| Duration::from_millis((now_ms() - at).max(0) as u64));
        self.requested
            || crate::backup::due(
                since_backup,
                self.changed_at.map(|t| now.duration_since(t)),
                self.deleted,
            )
    }

    /// The line Settings shows under the switch.
    pub fn summary(&self, now: i64) -> String {
        match &self.status {
            BackupStatus::Off => "Off. Your history stays on this Mac.".into(),
            BackupStatus::Unavailable => "iCloud Drive is off on this Mac.".into(),
            BackupStatus::Running => "Backing up…".into(),
            BackupStatus::Restoring => "Restoring from iCloud Drive…".into(),
            BackupStatus::Failed(err) => format!("Last backup failed: {err}"),
            BackupStatus::Idle => match self.last_at {
                None => "Waiting for the first backup…".into(),
                Some(at) => {
                    let upload = match self.uploaded {
                        Some(true) => " · in iCloud",
                        Some(false) => " · uploading",
                        None => "",
                    };
                    format!(
                        "Last backup: {} · {} items{upload}",
                        crate::cards::relative_time(at, now),
                        self.last_items
                    )
                }
            },
        }
    }
}

impl AppState {
    pub fn new(store: Arc<Store>, settings: Settings) -> AppState {
        let backup = BackupState {
            last_at: store
                .meta("backup_at")
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok()),
            last_items: store
                .meta("backup_items")
                .ok()
                .flatten()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            backed_up: store.meta("backup_stamp").ok().flatten(),
            ..Default::default()
        };
        let mut state = AppState {
            store,
            settings,
            pause: None,
            own_change_count: None,
            collections: Vec::new(),
            tab: Tab::History,
            items: Arc::default(),
            has_more: false,
            total: None,
            selection: Selection::default(),
            search: SearchState::default(),
            target_app: None,
            panel_visible: false,
            resume_until: None,
            apps: Arc::default(),
            undo: Vec::new(),
            toast: None,
            accessibility_prompted: false,
            accessibility_at_launch: false,
            relaunch_prompted: false,
            highlight_item: None,
            preview: None,
            settings_open: false,
            queue: PasteQueue::default(),
            backup,
        };
        state.refresh_collections();
        state.refresh_apps();
        state.reload_list();
        state
    }

    // ---- capture state -------------------------------------------------------------------------

    pub fn is_paused(&self) -> bool {
        match self.pause {
            None => false,
            Some(PauseState::Indefinite) => true,
            Some(PauseState::Until(t)) => Instant::now() < t,
        }
    }

    pub fn pause_for(&mut self, duration: Option<Duration>) {
        self.pause = Some(match duration {
            Some(d) => PauseState::Until(Instant::now() + d),
            None => PauseState::Indefinite,
        });
        info!("capture paused ({duration:?})");
    }

    pub fn resume(&mut self) {
        self.pause = None;
        info!("capture resumed");
    }

    /// Drops an expired timed pause. Returns true when the pause state changed.
    pub fn expire_pause(&mut self) -> bool {
        if let Some(PauseState::Until(t)) = self.pause
            && Instant::now() >= t
        {
            self.pause = None;
            info!("timed pause elapsed; capture resumed");
            return true;
        }
        false
    }

    // ---- lists ---------------------------------------------------------------------------------

    pub fn refresh_collections(&mut self) {
        match self.store.collections() {
            Ok(boards) => self.collections = boards,
            Err(err) => warn!("collections query failed: {err:#}"),
        }
        if let Tab::Collection(id) = self.tab
            && !self.collections.iter().any(|p| p.id == id)
        {
            // The active collection is gone: fall back to history, with its items.
            self.tab = Tab::History;
            self.items = Arc::default();
            self.selection = Selection::default();
            self.reload_list();
            self.select_first();
        }
    }

    pub fn refresh_apps(&mut self) {
        match self.store.apps() {
            Ok(apps) => {
                self.apps = Arc::new(apps.into_iter().map(|a| (a.bundle_id.clone(), a)).collect());
            }
            Err(err) => warn!("apps query failed: {err:#}"),
        }
    }

    /// Re-queries the current list from the store, keeping at least as many rows as were
    /// loaded and preserving the selection by item id.
    pub fn reload_list(&mut self) {
        let anchor = self.selection_anchor();
        let wanted = self.items.len().max(PAGE_SIZE);
        let started = Instant::now();
        let result = if self.search.active {
            self.store.search(&self.search.query, 0, wanted)
        } else {
            match self.tab {
                Tab::History => self.store.history(None, wanted),
                Tab::Collection(id) => self.store.collection_items(id, 0, wanted),
            }
        };
        self.total = if self.search.active {
            None
        } else {
            let count = match self.tab {
                Tab::History => self.store.count_items(),
                Tab::Collection(id) => self.store.count_collection_items(id),
            };
            count.ok().map(|n| n as usize)
        };
        match result {
            Ok(items) => {
                self.has_more = items.len() >= wanted;
                self.items = Arc::new(items);
                debug!(
                    "list reloaded: {} rows in {:?} (search={})",
                    self.items.len(),
                    started.elapsed(),
                    self.search.active
                );
            }
            Err(err) => warn!("list query failed: {err:#}"),
        }
        self.restore_selection(anchor);
    }

    /// Loads the next page when the strip is scrolled near its end.
    pub fn load_more(&mut self) {
        if !self.has_more || self.items.is_empty() {
            return;
        }
        let last = &self.items[self.items.len() - 1];
        let cursor = Cursor {
            created_at: last.created_at,
            id: last.id,
        };
        let result = if self.search.active {
            self.store
                .search(&self.search.query, self.items.len(), PAGE_SIZE)
        } else {
            match self.tab {
                Tab::History => self.store.history(Some(cursor), PAGE_SIZE),
                Tab::Collection(id) => self.store.collection_items(id, self.items.len(), PAGE_SIZE),
            }
        };
        match result {
            Ok(page) => {
                self.has_more = page.len() >= PAGE_SIZE;
                Arc::make_mut(&mut self.items).extend(page);
            }
            Err(err) => {
                // Stop asking; callers loop on `has_more`.
                self.has_more = false;
                warn!("page query failed: {err:#}");
            }
        }
    }

    /// The selected ids and the cursor's id, so a re-query can find them again.
    /// Loads pages until `done` holds or the list is exhausted (with a cap of 500 pages).
    pub fn load_until(&mut self, done: impl Fn(&AppState) -> bool) {
        for _ in 0..500 {
            if done(self) || !self.has_more {
                return;
            }
            self.load_more();
        }
    }

    fn selection_anchor(&self) -> Option<(Vec<i64>, Option<i64>)> {
        let cursor = self.selection.cursor?;
        let ids = self
            .selection
            .set
            .iter()
            .filter_map(|ix| self.items.get(*ix).map(|i| i.id))
            .collect();
        Some((ids, self.items.get(cursor).map(|i| i.id)))
    }

    fn restore_selection(&mut self, anchor: Option<(Vec<i64>, Option<i64>)>) {
        let Some((ids, cursor_id)) = anchor else {
            self.selection = Selection::default();
            return;
        };
        let index: HashMap<i64, usize> = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| (item.id, ix))
            .collect();
        let set: BTreeSet<usize> = ids.iter().filter_map(|id| index.get(id).copied()).collect();
        let cursor = cursor_id
            .and_then(|id| index.get(&id).copied())
            .or_else(|| set.first().copied());
        self.selection = match cursor {
            Some(c) if !set.is_empty() => Selection {
                cursor: Some(c),
                anchor: Some(c),
                set,
            },
            Some(c) => Selection::single(c),
            None => Selection::default(),
        };
    }

    /// The tab cycle used by the hotkey and Cmd-Left/Right: history, then each collection.
    pub fn tabs(&self) -> Vec<Tab> {
        std::iter::once(Tab::History)
            .chain(self.collections.iter().map(|p| Tab::Collection(p.id)))
            .collect()
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if self.tab == tab && !self.search.active {
            return;
        }
        self.tab = tab;
        self.search = SearchState::default();
        self.items = Arc::default();
        self.selection = Selection::default();
        self.reload_list();
        self.select_first();
    }

    pub fn cycle_tab(&mut self, delta: isize) {
        let tabs = self.tabs();
        let current = tabs.iter().position(|t| *t == self.tab).unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(tabs.len() as isize) as usize;
        self.set_tab(tabs[next]);
    }

    pub fn collection(&self, id: i64) -> Option<&Collection> {
        self.collections.iter().find(|p| p.id == id)
    }

    /// Decides where a show starts: inside the resume window the last browse continues,
    /// otherwise history, no search, newest card. Returns whether the browse was kept.
    pub fn begin_show(&mut self) -> bool {
        let resume = self
            .resume_until
            .take()
            .is_some_and(|until| Instant::now() < until);
        if resume {
            if self.selection.cursor.is_none() {
                self.select_first();
            }
        } else {
            self.set_tab(Tab::History);
            // A deep scroll last time loaded thousands of rows; start from one page again.
            if self.items.len() > PAGE_SIZE {
                self.items = Arc::new(self.items[..PAGE_SIZE].to_vec());
                self.has_more = true;
            }
            self.select_first();
        }
        resume
    }

    // ---- selection -----------------------------------------------------------------------------

    pub fn select_first(&mut self) {
        if self.items.is_empty() {
            self.selection = Selection::default();
        } else {
            self.selection = Selection::single(0);
        }
    }

    pub fn select_last(&mut self) {
        if let Some(last) = self.items.len().checked_sub(1) {
            self.selection = Selection::single(last);
        }
    }

    pub fn select(&mut self, ix: usize) {
        if ix < self.items.len() {
            self.selection = Selection::single(ix);
        }
    }

    pub fn toggle_select(&mut self, ix: usize) {
        if ix >= self.items.len() {
            return;
        }
        if self.selection.set.contains(&ix) {
            self.selection.set.remove(&ix);
            self.selection.cursor = self.selection.set.iter().next_back().copied();
        } else {
            self.selection.set.insert(ix);
            self.selection.cursor = Some(ix);
            self.selection.anchor = Some(ix);
        }
    }

    pub fn extend_to(&mut self, ix: usize) {
        if ix >= self.items.len() {
            return;
        }
        let anchor = self
            .selection
            .anchor
            .or(self.selection.cursor)
            .unwrap_or(ix);
        let (lo, hi) = if anchor <= ix {
            (anchor, ix)
        } else {
            (ix, anchor)
        };
        self.selection = Selection {
            cursor: Some(ix),
            anchor: Some(anchor),
            set: (lo..=hi).collect(),
        };
    }

    /// Left/Right (and Shift variants). Returns the new cursor for scroll-into-view.
    pub fn move_cursor(&mut self, delta: isize, extend: bool) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let cursor = match self.selection.cursor {
            Some(c) => c,
            None => {
                self.select_first();
                return Some(0);
            }
        };
        let next = (cursor as isize + delta).clamp(0, self.items.len() as isize - 1) as usize;
        if extend {
            self.extend_to(next);
        } else {
            self.selection = Selection::single(next);
        }
        Some(next)
    }

    pub fn select_all(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selection = Selection {
            cursor: self.selection.cursor.or(Some(0)),
            anchor: Some(0),
            set: (0..self.items.len()).collect(),
        };
    }

    pub fn selected_items(&self) -> Vec<Item> {
        self.selection
            .set
            .iter()
            .filter_map(|ix| self.items.get(*ix).cloned())
            .collect()
    }

    pub fn cursor_item(&self) -> Option<&Item> {
        self.selection.cursor.and_then(|ix| self.items.get(ix))
    }

    // ---- search --------------------------------------------------------------------------------

    /// Recomputes the query from text and chips and re-queries. Empty search returns home.
    pub fn apply_search(&mut self) {
        if self.search.is_empty() {
            if self.search.active {
                let home = self.search.home.take().unwrap_or(self.tab);
                self.search = SearchState::default();
                self.tab = home;
                self.items = Arc::default();
                self.reload_list();
                self.select_first();
            }
            return;
        }
        if !self.search.active {
            self.search.home = Some(self.tab);
            self.search.active = true;
        }
        let now = chrono::Local::now();
        let mut query = SearchQuery {
            terms: self.search.terms(),
            ..Default::default()
        };
        for chip in &self.search.chips {
            match chip {
                Chip::Kind(k) => query.kinds.push(*k),
                Chip::App { bundle_id, .. } => query.apps.push(bundle_id.clone()),
                Chip::Date(range) => {
                    let (since, until) = range.bounds(now);
                    query.since_ms = since;
                    query.until_ms = until;
                }
                Chip::Collection { id, .. } => query.collections.push(*id),
            }
        }
        self.search.query = query;
        self.items = Arc::default();
        self.selection = Selection::default();
        self.reload_list();
        self.select_first();
    }

    pub fn set_search_text(&mut self, text: String) {
        if self.search.text == text {
            return;
        }
        self.search.text = text;
        self.apply_search();
    }

    pub fn add_chip(&mut self, chip: Chip) {
        if !self.search.chips.contains(&chip) {
            // Only one date range at a time.
            if matches!(chip, Chip::Date(_)) {
                self.search.chips.retain(|c| !matches!(c, Chip::Date(_)));
            }
            self.search.chips.push(chip);
        }
        self.apply_search();
    }

    pub fn remove_chip(&mut self, index: usize) {
        if index < self.search.chips.len() {
            self.search.chips.remove(index);
            self.apply_search();
        }
    }

    pub fn clear_search(&mut self) {
        self.search.text.clear();
        self.search.chips.clear();
        self.apply_search();
    }

    /// Chip suggestions for the last typed token: kinds, app names at word boundaries, dates.
    pub fn suggest_chip(&self) -> Option<Chip> {
        let token = self.search.text.split_whitespace().last()?.to_lowercase();
        if token.len() < 2 {
            return None;
        }
        match token.as_str() {
            "today" => return Some(Chip::Date(DateRange::Today)),
            "yesterday" => return Some(Chip::Date(DateRange::Yesterday)),
            _ => {}
        }
        for kind in Kind::ALL {
            if kind.label().to_lowercase() == token || kind.as_str() == token {
                return Some(Chip::Kind(kind));
            }
        }
        // The most recently seen app with a word starting with the token.
        let starts_word = |name: &str| {
            name.split(|c: char| !c.is_alphanumeric())
                .any(|word| !word.is_empty() && word.to_lowercase().starts_with(&token))
        };
        let app = self
            .apps
            .values()
            .filter(|a| a.name.as_deref().is_some_and(starts_word))
            .max_by_key(|a| a.last_seen_at)?;
        Some(Chip::App {
            bundle_id: app.bundle_id.clone(),
            name: app.name.clone()?,
        })
    }

    /// Accepts the suggested chip: the token becomes the chip and leaves the text.
    pub fn accept_suggestion(&mut self) -> bool {
        let Some(chip) = self.suggest_chip() else {
            return false;
        };
        let mut words: Vec<&str> = self.search.text.split_whitespace().collect();
        words.pop();
        let rest = words.join(" ");
        self.search.text = if rest.is_empty() {
            rest
        } else {
            format!("{rest} ")
        };
        self.add_chip(chip);
        true
    }

    // ---- mutations -----------------------------------------------------------------------------

    /// Called after the capture engine stored an item. Re-queries when the item belongs in the
    /// visible list; otherwise nothing changes on screen.
    pub fn on_item_captured(&mut self, item: &Item, was_new: bool) {
        // A copy made while the panel is away is what the next paste is most likely for.
        if !self.panel_visible {
            self.resume_until = None;
        }
        if let Some(bundle) = &item.source_bundle_id
            && !self.apps.contains_key(bundle)
        {
            self.refresh_apps();
        }
        let affects_list = match (self.search.active, self.tab) {
            (true, _) => true,
            (false, Tab::History) => true,
            (false, Tab::Collection(id)) => item.collection_ids.contains(&id),
        };
        if affects_list {
            // New captures land in front; keep the keyboard cursor on the same item unless the
            // user is at the very front, where following the newest card is what they expect.
            let follow = self.selection.cursor == Some(0) || self.selection.cursor.is_none();
            self.reload_list();
            if follow {
                self.select_first();
            }
        }
        debug!(
            "captured {:?} item id={} new={}",
            item.kind, item.id, was_new
        );
    }

    /// Soft-deletes the selection and remembers it for undo. Returns the deleted ids.
    pub fn delete_selected(&mut self) -> Vec<i64> {
        let ids: Vec<i64> = self.selected_items().iter().map(|i| i.id).collect();
        if ids.is_empty() {
            return ids;
        }
        match self.store.delete(&ids) {
            Ok(n) => info!("deleted {n} item(s)"),
            Err(err) => {
                warn!("delete failed: {err:#}");
                return Vec::new();
            }
        }
        self.undo.push(ids.clone());
        if self.undo.len() > UNDO_DEPTH {
            self.undo.remove(0);
        }
        let first = self.selection.set.iter().next().copied().unwrap_or(0);
        self.selection = Selection::default();
        self.reload_list();
        if !self.items.is_empty() {
            self.select(first.min(self.items.len() - 1));
        }
        ids
    }

    pub fn undo_delete(&mut self) -> usize {
        let Some(ids) = self.undo.pop() else {
            return 0;
        };
        let restored = self.store.restore(&ids).unwrap_or_else(|err| {
            warn!("undo failed: {err:#}");
            0
        });
        self.reload_list();
        if let Some(first) = ids.first()
            && let Some(ix) = self.items.iter().position(|i| i.id == *first)
        {
            self.select(ix);
        }
        restored
    }

    pub fn erase_history(&mut self) {
        match self.store.erase_history() {
            Ok(n) => info!("erased {n} items"),
            Err(err) => {
                warn!("erase failed: {err:#}");
                return;
            }
        }
        self.undo.clear();
        self.items = Arc::default();
        self.reload_list();
        self.selection = Selection::default();
    }

    pub fn sweep(&mut self) {
        let retention = self.settings.retention;
        match self.store.sweep(retention) {
            Ok(report) => {
                if report.expired_items > 0 || report.purged_deleted > 0 || report.orphan_blobs > 0
                {
                    info!(
                        "sweep: expired={} purged={} orphan_blobs={}",
                        report.expired_items, report.purged_deleted, report.orphan_blobs
                    );
                    self.reload_list();
                }
            }
            Err(err) => warn!("sweep failed: {err:#}"),
        }
    }

    pub fn count_expired_by(&self, retention: Retention) -> i64 {
        retention
            .cutoff_ms(now_ms())
            .and_then(|cutoff| self.store.count_expired(cutoff).ok())
            .unwrap_or(0)
    }

    pub fn show_toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            until: Instant::now() + TOAST_DURATION,
        });
    }

    pub fn save_settings(&self) {
        if let Err(err) = self.settings.save(&crate::settings::settings_path()) {
            warn!("could not save settings: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewItem;

    fn state_with(n: usize) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path()).unwrap());
        for i in 0..n {
            store.insert(&NewItem::text(format!("item {i}"))).unwrap();
        }
        let state = AppState::new(store, Settings::default());
        (dir, state)
    }

    #[test]
    fn selection_moves_and_extends() {
        let (_d, mut s) = state_with(5);
        s.select_first();
        assert_eq!(s.selection.cursor, Some(0));
        s.move_cursor(1, false);
        s.move_cursor(1, false);
        assert_eq!(s.selection, Selection::single(2));
        s.move_cursor(2, true);
        assert_eq!(s.selection.set, BTreeSet::from([2, 3, 4]));
        s.move_cursor(-3, true);
        assert_eq!(s.selection.set, BTreeSet::from([1, 2]));
        s.move_cursor(100, false);
        assert_eq!(s.selection.cursor, Some(4));
        s.select_all();
        assert_eq!(s.selection.set.len(), 5);
        s.toggle_select(0);
        assert_eq!(s.selection.set.len(), 4);
        s.select_last();
        assert_eq!(s.selection, Selection::single(4));
    }

    #[test]
    fn capture_keeps_cursor_unless_at_front() {
        let (_d, mut s) = state_with(3);
        s.select(1);
        let cursor_id = s.items[1].id;
        let (item, new) = s.store.insert(&NewItem::text("fresh")).unwrap();
        s.on_item_captured(&item, new);
        assert_eq!(s.items.len(), 4);
        assert_eq!(s.items[s.selection.cursor.unwrap()].id, cursor_id);
        s.select_first();
        let (item, new) = s.store.insert(&NewItem::text("fresher")).unwrap();
        s.on_item_captured(&item, new);
        assert_eq!(s.items[0].id, item.id);
        assert_eq!(s.selection.cursor, Some(0));
    }

    #[test]
    fn show_resumes_only_a_recent_untouched_browse() {
        let (_d, mut s) = state_with(4);
        let board = s.store.create_collection("B", "blue").unwrap();
        s.refresh_collections();

        // Dismissed a moment ago: the same card again.
        s.select(2);
        s.resume_until = Some(Instant::now() + RESUME_WINDOW);
        assert!(s.begin_show());
        assert_eq!(s.selection.cursor, Some(2));

        // After a paste there is no window: history, newest card, even from a collection.
        s.set_tab(Tab::Collection(board.id));
        assert!(!s.begin_show());
        assert_eq!(s.tab, Tab::History);
        assert_eq!(s.selection.cursor, Some(0));

        // A copy while hidden ends the window and the new item is selected.
        s.select(2);
        s.resume_until = Some(Instant::now() + RESUME_WINDOW);
        let (item, new) = s.store.insert(&NewItem::text("fresh")).unwrap();
        s.on_item_captured(&item, new);
        assert!(!s.begin_show());
        assert_eq!(s.items[0].id, item.id);
        assert_eq!(s.selection.cursor, Some(0));

        // An expired window starts fresh, and searches are cleared.
        s.set_search_text("item".into());
        s.select(1);
        s.resume_until = Instant::now().checked_sub(Duration::from_millis(1));
        assert!(!s.begin_show());
        assert!(!s.search.active);
        assert_eq!(s.selection.cursor, Some(0));
    }

    #[test]
    fn fresh_show_trims_a_deep_scroll_to_one_page() {
        let (_d, mut s) = state_with(PAGE_SIZE + 50);
        s.load_until(|_| false);
        assert_eq!(s.items.len(), PAGE_SIZE + 50);
        s.select_last();
        assert!(!s.begin_show());
        assert_eq!(s.items.len(), PAGE_SIZE);
        assert!(s.has_more);
        assert_eq!(s.selection.cursor, Some(0));
    }

    #[test]
    fn delete_and_undo() {
        let (_d, mut s) = state_with(3);
        s.select(1);
        s.move_cursor(1, true);
        let ids = s.delete_selected();
        assert_eq!(ids.len(), 2);
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.selection.cursor, Some(0));
        assert_eq!(s.undo_delete(), 2);
        assert_eq!(s.items.len(), 3);
        assert_eq!(s.undo_delete(), 0);
    }

    #[test]
    fn search_and_chips_return_home() {
        let (_d, mut s) = state_with(3);
        s.store
            .insert(&NewItem {
                kind: Some(Kind::Link),
                text: Some("https://zed.dev".into()),
                source_bundle_id: Some("com.apple.Safari".into()),
                source_app_name: Some("Safari".into()),
                ..Default::default()
            })
            .unwrap();
        s.reload_list();
        s.refresh_apps();
        s.set_search_text("zed".into());
        assert!(s.search.active);
        assert_eq!(s.items.len(), 1);
        s.set_search_text("".into());
        assert!(!s.search.active);
        assert_eq!(s.items.len(), 4);

        s.set_search_text("saf".into());
        assert_eq!(
            s.suggest_chip(),
            Some(Chip::App {
                bundle_id: "com.apple.Safari".into(),
                name: "Safari".into()
            })
        );
        assert!(s.accept_suggestion());
        assert_eq!(s.search.text, "");
        assert_eq!(s.search.chips.len(), 1);
        assert_eq!(s.items.len(), 1);
        s.set_search_text("link".into());
        assert_eq!(s.suggest_chip(), Some(Chip::Kind(Kind::Link)));
        s.set_search_text("today".into());
        assert_eq!(s.suggest_chip(), Some(Chip::Date(DateRange::Today)));
        s.add_chip(Chip::Date(DateRange::Yesterday));
        s.add_chip(Chip::Date(DateRange::Today));
        assert_eq!(
            s.search
                .chips
                .iter()
                .filter(|c| matches!(c, Chip::Date(_)))
                .count(),
            1
        );
        s.clear_search();
        assert!(!s.search.active);
        assert_eq!(s.tab, Tab::History);
    }

    #[test]
    fn tabs_cycle_through_collections() {
        let (_d, mut s) = state_with(2);
        let board = s.store.create_collection("B", "blue").unwrap();
        s.refresh_collections();
        assert_eq!(s.tabs(), vec![Tab::History, Tab::Collection(board.id)]);
        s.cycle_tab(1);
        assert_eq!(s.tab, Tab::Collection(board.id));
        assert!(s.items.is_empty());
        s.cycle_tab(1);
        assert_eq!(s.tab, Tab::History);
        s.cycle_tab(-1);
        assert_eq!(s.tab, Tab::Collection(board.id));
        s.store.delete_collection(board.id).unwrap();
        s.refresh_collections();
        assert_eq!(s.tab, Tab::History);
    }

    #[test]
    fn pause_expires() {
        let (_d, mut s) = state_with(0);
        assert!(!s.is_paused());
        s.pause_for(Some(Duration::from_millis(1)));
        assert!(s.is_paused());
        std::thread::sleep(Duration::from_millis(3));
        assert!(!s.is_paused());
        assert!(s.expire_pause());
        s.pause_for(None);
        assert!(s.is_paused());
        s.resume();
        assert!(!s.is_paused());
    }

    #[test]
    fn date_ranges_start_at_local_midnight() {
        use chrono::{Local, NaiveDate, TimeZone, Timelike};
        let local = |ms: i64| Local.timestamp_millis_opt(ms).unwrap();
        // 2026-03-09 and 2026-11-02 follow daylight-saving changes in the US; any day works
        // elsewhere.
        for day in [(2026, 3, 9), (2026, 11, 2), (2026, 7, 15)] {
            let date = NaiveDate::from_ymd_opt(day.0, day.1, day.2).unwrap();
            let now = date
                .and_hms_opt(15, 30, 0)
                .unwrap()
                .and_local_timezone(Local)
                .unwrap();
            let (today, open) = DateRange::Today.bounds(now);
            assert_eq!(open, None);
            assert_eq!(local(today.unwrap()).date_naive(), date);
            assert_eq!(local(today.unwrap()).hour(), 0);

            let (since, until) = DateRange::Yesterday.bounds(now);
            assert_eq!(until, today);
            assert_eq!(local(since.unwrap()).date_naive(), date.pred_opt().unwrap());
            assert_eq!(local(since.unwrap()).hour(), 0);

            let (week, _) = DateRange::Last7Days.bounds(now);
            assert_eq!(
                local(week.unwrap()).date_naive(),
                date - chrono::Days::new(6)
            );
        }
    }
}
