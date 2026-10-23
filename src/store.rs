//! SQLite storage: schema and migrations, item and collection queries, FTS5 search, the retention
//! sweep, and the content-addressed blob store on disk.
//!
//! One `Store` is shared by the whole process behind an `Arc`. Writes are serialized through a
//! single connection guarded by a mutex; reads check a connection out of a small pool so the
//! panel's queries never wait on a capture in progress (WAL mode makes that safe). The CLI opens
//! the same file with the same code.
//!
//! Item ids follow time: a larger id was created later, except for items moved to the front by
//! a recopy or a paste, which carry `last_used_at`. Search relies on this (see
//! `build_search_sql`), so history imported from elsewhere is inserted below the existing ids.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: i64 = 5;
/// Rows per page for history, collections, and search results.
pub const PAGE_SIZE: usize = 200;
/// Plain text longer than this is kept in full as a blob; `items.text` holds this prefix.
pub const TEXT_INLINE_LIMIT: usize = 64 * 1024;
/// Queries shorter than the trigram minimum scan this many recent rows with LIKE instead.
const SHORT_QUERY_SCAN_ROWS: usize = 5_000;
const FTS_MIN_TERM_LEN: usize = 3;
/// Extra FTS candidates fetched beyond `offset + limit`; filters are applied inside the scan,
/// so the slack only covers rows removed by the moved-set union and soft deletes.
const FTS_CANDIDATE_SLACK: usize = 100;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Link,
    Image,
    File,
    Color,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::Text, Kind::Link, Kind::Image, Kind::File, Kind::Color];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Link => "link",
            Kind::Image => "image",
            Kind::File => "file",
            Kind::Color => "color",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "text" => Some(Kind::Text),
            "link" => Some(Kind::Link),
            "image" => Some(Kind::Image),
            "file" => Some(Kind::File),
            "color" => Some(Kind::Color),
            _ => None,
        }
    }

    /// The label shown in filter chips and the settings UI.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Text => "Text",
            Kind::Link => "Link",
            Kind::Image => "Image",
            Kind::File => "File",
            Kind::Color => "Color",
        }
    }
}

/// One clipboard item as stored. `text` is the plain text, URL, or `#RRGGBB`; the blob columns
/// name files in the blob store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub uuid: String,
    pub kind: Kind,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub source_bundle_id: Option<String>,
    pub source_app_name: Option<String>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub text_blob: Option<String>,
    pub rich_blob: Option<String>,
    pub rich_type: Option<String>,
    pub image_blob: Option<String>,
    pub image_w: Option<i64>,
    pub image_h: Option<i64>,
    pub image_truncated: bool,
    pub files: Vec<String>,
    pub size_bytes: i64,
    pub content_hash: String,
    pub ocr_text: Option<String>,
    pub link_title: Option<String>,
    pub link_domain: Option<String>,
    pub link_image_blob: Option<String>,
    pub device_id: String,
    pub deleted_at: Option<i64>,
    /// Collections this item belongs to, ascending by collection id.
    pub collection_ids: Vec<i64>,
}

impl Item {
    /// The key of the item's cached PNG in the thumbnail directory, if it has one: the image
    /// blob, the link preview image, or (for files) the content hash of the Finder icon.
    pub fn thumb_key(&self) -> Option<&str> {
        match self.kind {
            Kind::Image => self.image_blob.as_deref(),
            Kind::Link => self.link_image_blob.as_deref(),
            Kind::File => Some(&self.content_hash),
            Kind::Text | Kind::Color => None,
        }
    }

    /// The title shown on the card: the user's name if set, else a kind-specific fallback.
    pub fn display_title(&self) -> String {
        if let Some(title) = self.title.as_deref().filter(|t| !t.trim().is_empty()) {
            return title.to_string();
        }
        match self.kind {
            Kind::Text => {
                let first = self
                    .text
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                truncate_chars(first, 80)
            }
            Kind::Link => self
                .link_domain
                .clone()
                .or_else(|| self.text.as_deref().and_then(url_host))
                .unwrap_or_else(|| self.text.clone().unwrap_or_default()),
            Kind::Image => match (self.image_w, self.image_h) {
                (Some(w), Some(h)) => format!("{w}×{h}"),
                _ => "Image".to_string(),
            },
            Kind::File => self
                .files
                .iter()
                .map(|p| file_name(p))
                .collect::<Vec<_>>()
                .join(", "),
            Kind::Color => self.text.clone().unwrap_or_default(),
        }
    }
}

/// The host of a URL, used as a link card's domain.
pub fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_string)
}

/// The last path component, or the whole path when it has none.
pub fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Everything the capture engine hands to `insert`. Blob bytes are stored by `insert` so callers
/// never touch the blob directory.
#[derive(Clone, Debug, Default)]
pub struct NewItem {
    pub kind: Option<Kind>,
    pub text: Option<String>,
    pub rich: Option<(Vec<u8>, &'static str)>,
    pub image: Option<NewImage>,
    pub files: Vec<String>,
    pub source_bundle_id: Option<String>,
    pub source_app_name: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewImage {
    pub png_or_original: Vec<u8>,
    pub mime: &'static str,
    pub width: i64,
    pub height: i64,
    pub truncated: bool,
}

impl NewItem {
    pub fn text(text: impl Into<String>) -> Self {
        NewItem {
            kind: Some(Kind::Text),
            text: Some(text.into()),
            ..Default::default()
        }
    }

    /// blake3 over the canonical bytes: plain text for text-like kinds, image bytes for images,
    /// sorted paths for files.
    pub fn content_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        match self.kind {
            Some(Kind::Image) => {
                if let Some(image) = &self.image {
                    hasher.update(&image.png_or_original);
                }
            }
            Some(Kind::File) => {
                let mut paths = self.files.clone();
                paths.sort();
                for p in paths {
                    hasher.update(p.as_bytes());
                    hasher.update(b"\0");
                }
            }
            _ => {
                hasher.update(self.text.as_deref().unwrap_or("").as_bytes());
            }
        }
        hasher.finalize().to_hex().to_string()
    }

    pub fn size_bytes(&self) -> i64 {
        let text = self.text.as_ref().map_or(0, |t| t.len());
        let rich = self.rich.as_ref().map_or(0, |(b, _)| b.len());
        let image = self.image.as_ref().map_or(0, |i| i.png_or_original.len());
        let files: usize = self.files.iter().map(|f| f.len()).sum();
        (text + rich + image + files) as i64
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Collection {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub color: String,
    pub position: i64,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: Option<String>,
    pub last_seen_at: i64,
}

/// A cursor for keyset pagination: the last row of the previous page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub created_at: i64,
    pub id: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Day,
    Week,
    #[default]
    Month,
    Year,
    Forever,
}

impl Retention {
    pub const ALL: [Retention; 5] = [
        Retention::Day,
        Retention::Week,
        Retention::Month,
        Retention::Year,
        Retention::Forever,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Retention::Day => "1 day",
            Retention::Week => "1 week",
            Retention::Month => "1 month",
            Retention::Year => "1 year",
            Retention::Forever => "Forever",
        }
    }

    /// Items created before this instant are past retention. `None` means keep forever.
    pub fn cutoff_ms(self, now: i64) -> Option<i64> {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        match self {
            Retention::Day => Some(now - DAY),
            Retention::Week => Some(now - 7 * DAY),
            Retention::Month => Some(now - 30 * DAY),
            Retention::Year => Some(now - 365 * DAY),
            Retention::Forever => None,
        }
    }
}

/// Filters and terms for one search. Built by the panel from the field text and chips.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchQuery {
    pub terms: Vec<String>,
    pub kinds: Vec<Kind>,
    pub apps: Vec<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub collections: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub expired_items: usize,
    pub purged_deleted: usize,
    pub orphan_blobs: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub items: i64,
    pub deleted: i64,
    pub collections: i64,
    pub blobs: i64,
    pub blob_bytes: i64,
}

/// What a snapshot for a backup holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub items: i64,
    pub collections: i64,
    pub blob_ids: Vec<String>,
}

/// What merging a backup's snapshot added.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub items: usize,
    pub collections: usize,
}

pub struct Store {
    dir: PathBuf,
    db_path: PathBuf,
    write: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    device_id: String,
}

const ITEM_COLUMNS: &str = "i.id, i.uuid, i.kind, i.created_at, i.updated_at, i.last_used_at, \
    i.source_bundle_id, i.source_app_name, i.title, i.text, i.text_blob, i.rich_blob, i.rich_type, \
    i.image_blob, i.image_w, i.image_h, i.image_truncated, i.files_json, i.size_bytes, \
    i.content_hash, i.ocr_text, i.link_title, i.link_domain, i.link_image_blob, i.device_id, \
    i.deleted_at, \
    (SELECT group_concat(collection_id) FROM (SELECT collection_id FROM collection_items \
        WHERE item_id = i.id ORDER BY collection_id)) AS collection_ids";

impl Store {
    /// Opens (or creates) the store in `dir`, which becomes `0700`. Runs migrations.
    pub fn open(dir: &Path) -> Result<Store> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).ok();
        for sub in ["blobs", "thumbs"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        let db_path = dir.join("pasteloop.sqlite");
        let write = open_connection(&db_path)?;
        let device_id = migrate(&write)?;
        Ok(Store {
            dir: dir.to_path_buf(),
            db_path,
            write: Mutex::new(write),
            readers: Mutex::new(Vec::new()),
            device_id,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The cached PNG for `key`: a blob id for image thumbnails, a content hash for file icons.
    pub fn thumb_path(&self, key: &str) -> PathBuf {
        thumb_path_in(&self.dir, key)
    }

    pub fn blob_path(&self, id: &str) -> PathBuf {
        blob_path_in(&self.dir, id)
    }

    /// Runs `f` with the write connection. Serialized process-wide.
    pub fn write<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let mut conn = self
            .write
            .lock()
            .map_err(|_| anyhow!("write lock poisoned"))?;
        f(&mut conn)
    }

    /// Runs `f` with a read-only connection from the pool.
    pub fn read<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let conn = {
            let mut pool = self
                .readers
                .lock()
                .map_err(|_| anyhow!("reader pool poisoned"))?;
            pool.pop()
        };
        let conn = match conn {
            Some(c) => c,
            None => open_connection(&self.db_path)?,
        };
        let result = f(&conn);
        if let Ok(mut pool) = self.readers.lock()
            && pool.len() < 2
        {
            pool.push(conn);
        }
        result
    }

    /// Drops SQLite's page caches on idle connections (they refill on the next queries).
    pub fn release_memory(&self) {
        if let Ok(pool) = self.readers.lock() {
            for conn in pool.iter() {
                let _ = conn.execute_batch("PRAGMA shrink_memory;");
            }
        }
        if let Ok(conn) = self.write.try_lock() {
            let _ = conn.execute_batch("PRAGMA shrink_memory;");
        }
    }

    /// SQLite's data version as seen by the write connection. It changes only when another
    /// process commits (the CLI), never for this process's own writes. `None` while a write is in
    /// progress, so the caller never waits for one.
    pub fn data_version(&self) -> Option<i64> {
        let conn = self.write.try_lock().ok()?;
        conn.query_row("PRAGMA data_version", [], |r| r.get(0)).ok()
    }

    // ---- items -------------------------------------------------------------------------------

    /// Inserts a new item, or, when a live item with the same content hash exists, moves that
    /// item to the front. Returns the resulting row and whether it was new.
    pub fn insert(&self, new: &NewItem) -> Result<(Item, bool)> {
        self.insert_at(new, None)
    }

    /// Inserts history from another app at its original time, below every existing id (call it
    /// newest first). A row newer than the oldest in-order item cannot keep id order down there,
    /// so it is marked moved, which search already handles. A live duplicate is left where it is.
    pub fn insert_imported(&self, new: &NewItem, created_at: i64) -> Result<(Item, bool)> {
        self.insert_at(new, Some(created_at))
    }

    fn insert_at(&self, new: &NewItem, imported_at: Option<i64>) -> Result<(Item, bool)> {
        let kind = new
            .kind
            .ok_or_else(|| anyhow!("item kind not classified"))?;
        let hash = new.content_hash();
        let now = now_ms();
        self.write(|conn| {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM items WHERE content_hash = ?1 AND deleted_at IS NULL",
                    params![hash],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                if imported_at.is_some() {
                    let item = load_item(conn, id)?.ok_or_else(|| anyhow!("item vanished"))?;
                    return Ok((item, false));
                }
                conn.execute(
                    "UPDATE items SET created_at = ?1, updated_at = ?1, last_used_at = ?1, \
                     source_bundle_id = COALESCE(?2, source_bundle_id), \
                     source_app_name = COALESCE(?3, source_app_name) WHERE id = ?4",
                    params![now, new.source_bundle_id, new.source_app_name, id],
                )?;
                let item = load_item(conn, id)?.ok_or_else(|| anyhow!("item vanished"))?;
                return Ok((item, false));
            }

            let tx = conn.transaction()?;
            let (text, text_blob) = match new.text.as_deref() {
                Some(t) if t.len() > TEXT_INLINE_LIMIT => {
                    let id = put_blob(&tx, &self.dir, t.as_bytes(), "text/plain")?;
                    (
                        Some(prefix_at_char_boundary(t, TEXT_INLINE_LIMIT)),
                        Some(id),
                    )
                }
                Some(t) => (Some(t.to_string()), None),
                None => (None, None),
            };
            let (rich_blob, rich_type) = match &new.rich {
                Some((bytes, uti)) => (Some(put_blob(&tx, &self.dir, bytes, uti)?), Some(*uti)),
                None => (None, None),
            };
            let image_blob = match &new.image {
                Some(img) => Some(put_blob(&tx, &self.dir, &img.png_or_original, img.mime)?),
                None => None,
            };
            let files_json = if new.files.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&new.files)?)
            };
            let link_domain = match kind {
                Kind::Link => new.text.as_deref().and_then(url_host),
                _ => None,
            };
            // NULL lets SQLite pick the next id at the top; imports go below the lowest one.
            let (id, moved_at): (Option<i64>, Option<i64>) = match imported_at {
                Some(at) => {
                    let (id, oldest): (i64, Option<i64>) = tx.query_row(
                        "SELECT (SELECT COALESCE(min(id), 1) - 1 FROM items), \
                         (SELECT min(created_at) FROM items \
                          WHERE last_used_at IS NULL AND deleted_at IS NULL)",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    (Some(id), oldest.filter(|oldest| at > *oldest).map(|_| at))
                }
                None => (None, None),
            };
            tx.execute(
                "INSERT INTO items (id, uuid, kind, created_at, updated_at, last_used_at, \
                 source_bundle_id, source_app_name, title, text, text_blob, rich_blob, rich_type, \
                 image_blob, image_w, image_h, image_truncated, files_json, size_bytes, \
                 content_hash, link_domain, device_id) \
                 VALUES (?20, ?1, ?2, ?3, ?3, ?21, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
                 ?15, ?16, ?17, ?18, ?19)",
                params![
                    uuid::Uuid::now_v7().to_string(),
                    kind.as_str(),
                    imported_at.unwrap_or(now),
                    new.source_bundle_id,
                    new.source_app_name,
                    new.title,
                    text,
                    text_blob,
                    rich_blob,
                    rich_type,
                    image_blob,
                    new.image.as_ref().map(|i| i.width),
                    new.image.as_ref().map(|i| i.height),
                    new.image.as_ref().is_some_and(|i| i.truncated) as i64,
                    files_json,
                    new.size_bytes(),
                    hash,
                    link_domain,
                    self.device_id,
                    id,
                    moved_at,
                ],
            )?;
            let id = tx.last_insert_rowid();
            // Keep the apps table current for filter chips and suggestions; the icon job fills
            // in the icon and tint later.
            if let Some(bundle) = &new.source_bundle_id {
                tx.execute(
                    "INSERT INTO apps (bundle_id, name, last_seen_at) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(bundle_id) DO UPDATE SET name = COALESCE(excluded.name, name), \
                     last_seen_at = excluded.last_seen_at",
                    params![bundle, new.source_app_name, now],
                )?;
            }
            tx.commit()?;
            let item = load_item(conn, id)?.ok_or_else(|| anyhow!("inserted item missing"))?;
            Ok((item, true))
        })
    }

    pub fn get(&self, id: i64) -> Result<Option<Item>> {
        self.read(|c| load_item(c, id))
    }

    /// The full plain text of an item, reading the overflow blob when the text was long.
    pub fn full_text(&self, item: &Item) -> Result<Option<String>> {
        match &item.text_blob {
            Some(blob) => {
                let bytes = fs::read(self.blob_path(blob))?;
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
            None => Ok(item.text.clone()),
        }
    }

    /// Newest first. `after` is the last row of the previous page.
    pub fn history(&self, after: Option<Cursor>, limit: usize) -> Result<Vec<Item>> {
        self.read(|c| {
            let (created_at, id) = after.map_or((i64::MAX, i64::MAX), |c| (c.created_at, c.id));
            let sql = format!(
                "SELECT {ITEM_COLUMNS} FROM items i WHERE i.deleted_at IS NULL \
                 AND (i.created_at < ?1 OR (i.created_at = ?1 AND i.id < ?2)) \
                 ORDER BY i.created_at DESC, i.id DESC LIMIT ?3"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![created_at, id, limit as i64], item_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn count_items(&self) -> Result<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM items WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )?)
        })
    }

    pub fn count_collection_items(&self, collection_id: i64) -> Result<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM collection_items p JOIN items i ON i.id = p.item_id \
                 WHERE p.collection_id = ?1 AND i.deleted_at IS NULL",
                params![collection_id],
                |r| r.get(0),
            )?)
        })
    }

    /// Items in a collection in manual order (most recently added first unless reordered).
    pub fn collection_items(
        &self,
        collection_id: i64,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Item>> {
        self.read(|c| {
            let sql = format!(
                "SELECT {ITEM_COLUMNS} FROM items i JOIN collection_items p ON p.item_id = i.id \
                 WHERE p.collection_id = ?1 AND i.deleted_at IS NULL \
                 ORDER BY p.position ASC, p.added_at DESC LIMIT ?2 OFFSET ?3"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(
                params![collection_id, limit as i64, offset as i64],
                item_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Search results, newest first, `offset` rows in. See `build_search_sql` for the design.
    pub fn search(&self, query: &SearchQuery, offset: usize, limit: usize) -> Result<Vec<Item>> {
        let (sql, binds) = build_search_sql(query, offset, limit);
        self.read(|c| {
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(binds.iter()), item_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn rename(&self, id: i64, title: Option<&str>) -> Result<()> {
        let title = title.map(str::trim).filter(|t| !t.is_empty());
        self.write(|c| {
            c.execute(
                "UPDATE items SET title = ?1, updated_at = ?2 WHERE id = ?3",
                params![title, now_ms(), id],
            )?;
            Ok(())
        })
    }

    /// Replaces the plain text of a text item. Rich representations are dropped because the
    /// edit was plain; the hash and FTS row follow the new text.
    pub fn update_text(&self, id: i64, text: &str) -> Result<Item> {
        let hash = NewItem::text(text).content_hash();
        self.write(|c| {
            let tx = c.transaction()?;
            let old: (Option<String>, Option<String>, Option<String>) = tx.query_row(
                "SELECT rich_blob, text_blob, kind FROM items WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if old.2.as_deref() != Some("text") {
                return Err(anyhow!("only text items can be edited"));
            }
            let (inline, text_blob) = if text.len() > TEXT_INLINE_LIMIT {
                let blob = put_blob(&tx, &self.dir, text.as_bytes(), "text/plain")?;
                (prefix_at_char_boundary(text, TEXT_INLINE_LIMIT), Some(blob))
            } else {
                (text.to_string(), None)
            };
            // A live row with the same text would violate the unique index. The edited item
            // absorbs it: collection memberships move over, then hard_delete removes the duplicate
            // with its blobs.
            let duplicates = collect_ids(
                &tx,
                "SELECT id FROM items WHERE content_hash = ?1 AND deleted_at IS NULL AND id != ?2",
                params![hash, id],
            )?;
            for dup in &duplicates {
                tx.execute(
                    "INSERT OR IGNORE INTO collection_items (collection_id, item_id, position, added_at) \
                     SELECT collection_id, ?1, position, added_at FROM collection_items WHERE item_id = ?2",
                    params![id, dup],
                )?;
            }
            hard_delete(&tx, &self.dir, &duplicates)?;
            tx.execute(
                "UPDATE items SET text = ?1, text_blob = ?2, rich_blob = NULL, rich_type = NULL, \
                 content_hash = ?3, size_bytes = ?4, updated_at = ?5 WHERE id = ?6",
                params![inline, text_blob, hash, text.len() as i64, now_ms(), id],
            )?;
            for blob in [old.0, old.1].into_iter().flatten() {
                release_blob(&tx, &blob)?;
            }
            tx.commit()?;
            load_item(c, id)?.ok_or_else(|| anyhow!("item vanished"))
        })
    }

    pub fn set_ocr_text(&self, id: i64, ocr: &str) -> Result<()> {
        self.write(|c| {
            c.execute(
                "UPDATE items SET ocr_text = ?1 WHERE id = ?2",
                params![ocr, id],
            )?;
            Ok(())
        })
    }

    pub fn set_link_metadata(
        &self,
        id: i64,
        title: Option<&str>,
        image_png: Option<&[u8]>,
    ) -> Result<()> {
        self.write(|c| {
            let tx = c.transaction()?;
            let blob = match image_png {
                Some(bytes) => Some(put_blob(&tx, &self.dir, bytes, "image/png")?),
                None => None,
            };
            let updated = tx.execute(
                "UPDATE items SET link_title = COALESCE(?1, link_title), \
                 link_image_blob = COALESCE(?2, link_image_blob) WHERE id = ?3 AND deleted_at IS NULL",
                params![title, blob, id],
            )?;
            if updated == 0
                && let Some(blob) = &blob
            {
                // The item vanished meanwhile; the sweep removes the file once unreferenced.
                release_blob(&tx, blob)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Marks the item as pasted just now and moves it to the front of history.
    pub fn touch_used(&self, id: i64) -> Result<()> {
        self.write(|c| {
            let now = now_ms();
            c.execute(
                "UPDATE items SET last_used_at = ?1, created_at = ?1, updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            Ok(())
        })
    }

    /// Soft delete. The rows stay (for undo) until the next sweep.
    pub fn delete(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.write(|c| {
            let now = now_ms();
            let mut n = 0;
            for id in ids {
                n += c.execute(
                    "UPDATE items SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                    params![now, id],
                )?;
            }
            Ok(n)
        })
    }

    /// Undo of `delete`. An item whose content was recopied in the meantime stays deleted
    /// because two live rows may not share a hash; the count tells the caller how many came back.
    pub fn restore(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.write(|c| {
            let mut n = 0;
            for id in ids {
                n += c.execute(
                    "UPDATE items SET deleted_at = NULL WHERE id = ?1 AND deleted_at IS NOT NULL \
                     AND NOT EXISTS (SELECT 1 FROM items o WHERE o.content_hash = items.content_hash \
                     AND o.deleted_at IS NULL AND o.id != items.id)",
                    params![id],
                )?;
            }
            Ok(n)
        })
    }

    /// Hard-deletes every item that is in no collection. Collections and their items stay.
    pub fn erase_history(&self) -> Result<usize> {
        self.write(|c| {
            let tx = c.transaction()?;
            let ids = collect_ids(
                &tx,
                "SELECT id FROM items WHERE NOT EXISTS \
                 (SELECT 1 FROM collection_items p WHERE p.item_id = items.id)",
                [],
            )?;
            let n = hard_delete(&tx, &self.dir, &ids)?;
            tx.commit()?;
            Ok(n)
        })
    }

    /// Number of live items in no collection older than `cutoff_ms`, for the confirmation dialog.
    pub fn count_expired(&self, cutoff_ms: i64) -> Result<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM items WHERE deleted_at IS NULL AND created_at < ?1 \
                 AND NOT EXISTS (SELECT 1 FROM collection_items p WHERE p.item_id = items.id)",
                params![cutoff_ms],
                |r| r.get(0),
            )?)
        })
    }

    /// Retention sweep: removes expired items that are in no collection, purges soft-deleted rows older than an
    /// hour, and deletes blob files nothing references any more.
    pub fn sweep(&self, retention: Retention) -> Result<SweepReport> {
        let now = now_ms();
        let mut report = SweepReport::default();
        self.write(|c| {
            let tx = c.transaction()?;
            if let Some(cutoff) = retention.cutoff_ms(now) {
                let ids = collect_ids(
                    &tx,
                    "SELECT id FROM items WHERE deleted_at IS NULL AND created_at < ?1 \
                     AND NOT EXISTS (SELECT 1 FROM collection_items p WHERE p.item_id = items.id)",
                    params![cutoff],
                )?;
                report.expired_items = hard_delete(&tx, &self.dir, &ids)?;
            }
            let purge_before = now - 60 * 60 * 1000;
            let ids = collect_ids(
                &tx,
                "SELECT id FROM items WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
                params![purge_before],
            )?;
            report.purged_deleted = hard_delete(&tx, &self.dir, &ids)?;
            report.orphan_blobs = delete_orphan_blobs(&tx, &self.dir)?;
            tx.commit()?;
            Ok(())
        })?;
        Ok(report)
    }

    pub fn stats(&self) -> Result<Stats> {
        self.read(|c| {
            let items = c.query_row(
                "SELECT count(*) FROM items WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let deleted = c.query_row(
                "SELECT count(*) FROM items WHERE deleted_at IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            let collections = c.query_row("SELECT count(*) FROM collections", [], |r| r.get(0))?;
            let (blobs, blob_bytes) = c.query_row(
                "SELECT count(*), COALESCE(sum(size_bytes), 0) FROM blobs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(Stats {
                items,
                deleted,
                collections,
                blobs,
                blob_bytes,
            })
        })
    }

    // ---- backup --------------------------------------------------------------------------------

    /// The id this library was created with. It names the library's backup and tags its items.
    pub fn library_id(&self) -> &str {
        &self.device_id
    }

    /// When this library was created, read from its time-ordered (v7) id.
    pub fn library_created_ms(&self) -> Option<i64> {
        let (secs, nanos) = uuid::Uuid::parse_str(&self.device_id)
            .ok()?
            .get_timestamp()?
            .to_unix();
        Some(secs as i64 * 1000 + nanos as i64 / 1_000_000)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.read(|c| {
            Ok(
                c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                    r.get(0)
                })
                .optional()?,
            )
        })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.write(|c| {
            c.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
    }

    /// A summary that changes whenever anything a backup holds changes: items, their edits,
    /// recognized text and link titles, deletions, collections and their contents.
    /// Also returns the number of live (not deleted) items.
    pub fn fingerprint(&self) -> Result<(String, i64)> {
        self.read(|c| {
            let (items, live): (String, i64) = c.query_row(
                "SELECT count(*) || ':' || count(deleted_at) || ':' || \
                 COALESCE(max(updated_at), 0) || ':' || COALESCE(max(created_at), 0) || ':' || \
                 COALESCE(max(last_used_at), 0) || ':' || COALESCE(max(deleted_at), 0) || ':' || \
                 count(ocr_text) || ':' || count(link_title), count(*) - count(deleted_at) \
                 FROM items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let collections: String = c.query_row(
                "SELECT (SELECT COALESCE(group_concat(id || name || color || position), '') \
                 FROM collections) || ':' || (SELECT count(*) || '/' || \
                 COALESCE(sum(position), 0) FROM collection_items)",
                [],
                |r| r.get(0),
            )?;
            Ok((format!("{items}|{collections}"), live))
        })
    }

    /// Writes a self-contained copy of the database to `dest` for a backup: deleted items are
    /// left out and the search index is emptied (a restore rebuilds it as rows go in), which
    /// makes the copy less than half the size. Reads only; the app keeps writing meanwhile.
    pub fn snapshot_into(&self, dest: &Path) -> Result<SnapshotInfo> {
        let _ = fs::remove_file(dest);
        let dest_str = dest.to_string_lossy().to_string();
        self.read(|c| {
            c.execute("VACUUM INTO ?1", params![dest_str])?;
            Ok(())
        })?;
        let conn = Connection::open(dest)?;
        conn.execute_batch(
            "PRAGMA journal_mode = DELETE; PRAGMA foreign_keys = ON; \
             DELETE FROM items WHERE deleted_at IS NOT NULL; \
             INSERT INTO items_fts(items_fts) VALUES ('delete-all');",
        )?;
        recount_blobs(&conn)?;
        conn.execute_batch(
            "DELETE FROM blobs WHERE refcount <= 0; \
             DELETE FROM meta WHERE key LIKE 'backup_%'; VACUUM;",
        )?;
        let (items, collections) = conn.query_row(
            "SELECT (SELECT count(*) FROM items), (SELECT count(*) FROM collections)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let blob_ids = {
            let mut stmt = conn.prepare("SELECT id FROM blobs ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        Ok(SnapshotInfo {
            items,
            collections,
            blob_ids,
        })
    }

    /// Adds a backup's snapshot to this library. Items already here (the same id, or the same
    /// content) are kept as they are; the rest go below this library's lowest id, since the
    /// backup is normally older, so ids keep following time. A backup item newer than the oldest
    /// local one is marked as moved, which is what lets search find it (see `build_search_sql`).
    /// Collections are matched by id, memberships carried over, and blob rows added; the blob
    /// files themselves are copied separately (`missing_blob_files`).
    pub fn merge_snapshot(&self, snapshot: &Path) -> Result<MergeReport> {
        {
            // A backup is not trusted input: its schema may not run functions, and it must hold
            // plain tables with well-formed names before anything of it is merged.
            let conn = Connection::open(snapshot)?;
            conn.execute_batch("PRAGMA trusted_schema = OFF;")?;
            let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
            if check != "ok" {
                return Err(anyhow!("the backup's database is damaged ({check})"));
            }
            // Bring the snapshot to this build's schema; fails for one from a newer build.
            migrate(&conn)?;
            validate_snapshot(&conn)?;
        }
        let path = snapshot.to_string_lossy().to_string();
        self.write(|conn| {
            conn.execute("ATTACH DATABASE ?1 AS b", params![path])?;
            let result = merge_attached(conn);
            conn.execute_batch("DETACH DATABASE b")?;
            result
        })
    }

    /// Blob ids in use whose files are not on disk yet (after a merge, until they are copied).
    pub fn missing_blob_files(&self) -> Result<Vec<String>> {
        let ids = self.read(|c| {
            let mut stmt = c.prepare("SELECT id FROM blobs WHERE refcount > 0")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
        })?;
        Ok(ids
            .into_iter()
            .filter(|id| !self.blob_path(id).exists())
            .collect())
    }

    // ---- collections ---------------------------------------------------------------------------

    pub fn collections(&self) -> Result<Vec<Collection>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT id, uuid, name, color, position, created_at FROM collections \
                 ORDER BY position ASC, id ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Collection {
                    id: r.get(0)?,
                    uuid: r.get(1)?,
                    name: r.get(2)?,
                    color: r.get(3)?,
                    position: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn create_collection(&self, name: &str, color: &str) -> Result<Collection> {
        self.write(|c| {
            let position: i64 = c.query_row(
                "SELECT COALESCE(max(position), -1) + 1 FROM collections",
                [],
                |r| r.get(0),
            )?;
            let uuid = uuid::Uuid::now_v7().to_string();
            let now = now_ms();
            c.execute(
                "INSERT INTO collections (uuid, name, color, position, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![uuid, name, color, position, now],
            )?;
            Ok(Collection {
                id: c.last_insert_rowid(),
                uuid,
                name: name.to_string(),
                color: color.to_string(),
                position,
                created_at: now,
            })
        })
    }

    pub fn update_collection(
        &self,
        id: i64,
        name: Option<&str>,
        color: Option<&str>,
    ) -> Result<()> {
        self.write(|c| {
            c.execute(
                "UPDATE collections SET name = COALESCE(?1, name), color = COALESCE(?2, color) \
                 WHERE id = ?3",
                params![name, color, id],
            )?;
            Ok(())
        })
    }

    /// Sets the tab order to `ids` (first is leftmost).
    pub fn reorder_collections(&self, ids: &[i64]) -> Result<()> {
        self.write(|c| {
            let tx = c.transaction()?;
            for (position, id) in ids.iter().enumerate() {
                tx.execute(
                    "UPDATE collections SET position = ?1 WHERE id = ?2",
                    params![position as i64, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Deletes the collection; its items stay in history.
    pub fn delete_collection(&self, id: i64) -> Result<()> {
        self.write(|c| {
            c.execute("DELETE FROM collections WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn add_to_collection(&self, item_id: i64, collection_id: i64) -> Result<()> {
        self.write(|c| {
            let position: i64 = c.query_row(
                "SELECT COALESCE(min(position), 1) - 1 FROM collection_items WHERE collection_id = ?1",
                params![collection_id],
                |r| r.get(0),
            )?;
            c.execute(
                "INSERT OR IGNORE INTO collection_items (collection_id, item_id, position, added_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![collection_id, item_id, position, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn remove_from_collection(&self, item_id: i64, collection_id: i64) -> Result<()> {
        self.write(|c| {
            c.execute(
                "DELETE FROM collection_items WHERE collection_id = ?1 AND item_id = ?2",
                params![collection_id, item_id],
            )?;
            Ok(())
        })
    }

    /// Sets the manual order of items inside a collection (first is leftmost).
    pub fn reorder_collection_items(&self, collection_id: i64, item_ids: &[i64]) -> Result<()> {
        self.write(|c| {
            let tx = c.transaction()?;
            for (position, item_id) in item_ids.iter().enumerate() {
                tx.execute(
                    "UPDATE collection_items SET position = ?1 WHERE collection_id = ?2 AND item_id = ?3",
                    params![position as i64, collection_id, item_id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    // ---- apps --------------------------------------------------------------------------------

    pub fn apps(&self) -> Result<Vec<AppInfo>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT bundle_id, name, last_seen_at FROM apps \
                 ORDER BY last_seen_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(AppInfo {
                    bundle_id: r.get(0)?,
                    name: r.get(1)?,
                    last_seen_at: r.get(2)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- export ------------------------------------------------------------------------------

    /// Every live item and collection as JSON. Blob columns are paths relative to the store dir.
    pub fn export_json(&self) -> Result<serde_json::Value> {
        let mut items = Vec::new();
        let mut after = None;
        loop {
            let page = self.history(after, PAGE_SIZE)?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|i| Cursor {
                created_at: i.created_at,
                id: i.id,
            });
            items.extend(page);
        }
        Ok(serde_json::json!({
            "version": SCHEMA_VERSION,
            "device_id": self.device_id,
            "exported_at": now_ms(),
            "collections": self.collections()?,
            "items": items,
        }))
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; \
         PRAGMA busy_timeout = 5000; PRAGMA temp_store = MEMORY;",
    )?;
    Ok(conn)
}

/// Forward-only migrations inside one transaction. Returns the device id.
fn migrate(conn: &Connection) -> Result<String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let version: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    match version {
        0 => conn.execute_batch(&format!(
            "BEGIN; {SCHEMA} \
             INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '{SCHEMA_VERSION}'); \
             COMMIT;"
        ))?,
        4 => conn.execute_batch(UPGRADE_FROM_V4)?,
        SCHEMA_VERSION => {}
        newer if newer > SCHEMA_VERSION => {
            return Err(anyhow!(
                "database schema version {newer} is newer than this build supports ({SCHEMA_VERSION})"
            ));
        }
        older => {
            return Err(anyhow!(
                "database schema version {older} comes from a pre-release build and cannot be upgraded"
            ));
        }
    }
    let device_id: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| {
            r.get(0)
        })
        .optional()?;
    match device_id {
        Some(id) => Ok(id),
        None => {
            let id = uuid::Uuid::now_v7().to_string();
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('device_id', ?1)",
                params![id],
            )?;
            Ok(id)
        }
    }
}

/// The schema, created in one transaction on a new store. Item ids follow creation time (see the
/// module docs). The full-text table indexes the searchable columns with the trigram tokenizer
/// and exposes the columns searches filter on as UNINDEXED columns, read live from `items`, so a
/// filtered search can stop after enough matching rows.
const SCHEMA: &str = "
CREATE TABLE items (
  id            INTEGER PRIMARY KEY,
  uuid          TEXT NOT NULL UNIQUE,
  kind          TEXT NOT NULL CHECK (kind IN ('text','link','image','file','color')),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  last_used_at  INTEGER,
  source_bundle_id TEXT,
  source_app_name  TEXT,
  title         TEXT,
  text          TEXT,
  text_blob     TEXT,
  rich_blob     TEXT,
  rich_type     TEXT,
  image_blob    TEXT,
  image_w       INTEGER, image_h INTEGER,
  image_truncated INTEGER NOT NULL DEFAULT 0,
  files_json    TEXT,
  size_bytes    INTEGER NOT NULL,
  content_hash  TEXT NOT NULL,
  ocr_text      TEXT,
  link_title    TEXT, link_domain TEXT, link_image_blob TEXT,
  device_id     TEXT NOT NULL,
  deleted_at    INTEGER
);
CREATE UNIQUE INDEX items_live_hash ON items(content_hash) WHERE deleted_at IS NULL;
CREATE INDEX items_created ON items(created_at DESC, id DESC) WHERE deleted_at IS NULL;
CREATE INDEX items_kind_created ON items(kind, created_at DESC) WHERE deleted_at IS NULL;
CREATE INDEX items_app_created ON items(source_bundle_id, created_at DESC) WHERE deleted_at IS NULL;
CREATE INDEX items_deleted ON items(deleted_at) WHERE deleted_at IS NOT NULL;
-- Items moved to the front by a recopy or a paste (see `build_search_sql`).
CREATE INDEX items_moved ON items(created_at) WHERE last_used_at IS NOT NULL;

CREATE TABLE collections (
  id INTEGER PRIMARY KEY, uuid TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL, color TEXT NOT NULL, position INTEGER NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE collection_items (
  collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
  item_id     INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL, added_at INTEGER NOT NULL,
  PRIMARY KEY (collection_id, item_id)
);
CREATE INDEX collection_items_item ON collection_items(item_id);

CREATE TABLE apps (
  bundle_id TEXT PRIMARY KEY, name TEXT, last_seen_at INTEGER
);
CREATE TABLE blobs (
  id TEXT PRIMARY KEY,
  size_bytes INTEGER NOT NULL, mime TEXT, created_at INTEGER NOT NULL, refcount INTEGER NOT NULL DEFAULT 0
);
CREATE VIRTUAL TABLE items_fts USING fts5(
  title, text, ocr_text, link_title, source_app_name,
  kind UNINDEXED, source_bundle_id UNINDEXED, created_at UNINDEXED, deleted_at UNINDEXED,
  content='items', content_rowid='id',
  tokenize='trigram case_sensitive 0'
);
CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fts(rowid, title, text, ocr_text, link_title, source_app_name)
  VALUES (new.id, new.title, new.text, new.ocr_text, new.link_title, new.source_app_name);
END;
CREATE TRIGGER items_ad AFTER DELETE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, title, text, ocr_text, link_title, source_app_name)
  VALUES ('delete', old.id, old.title, old.text, old.ocr_text, old.link_title, old.source_app_name);
END;
CREATE TRIGGER items_au AFTER UPDATE OF title, text, ocr_text, link_title, source_app_name ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, title, text, ocr_text, link_title, source_app_name)
  VALUES ('delete', old.id, old.title, old.text, old.ocr_text, old.link_title, old.source_app_name);
  INSERT INTO items_fts(rowid, title, text, ocr_text, link_title, source_app_name)
  VALUES (new.id, new.title, new.text, new.ocr_text, new.link_title, new.source_app_name);
END;
";

/// Pre-release builds (schema 4) called collections "pinboards".
const UPGRADE_FROM_V4: &str = "
BEGIN;
ALTER TABLE pinboards RENAME TO collections;
ALTER TABLE pinboard_items RENAME TO collection_items;
ALTER TABLE collection_items RENAME COLUMN pinboard_id TO collection_id;
DROP INDEX IF EXISTS pinboard_items_item;
CREATE INDEX collection_items_item ON collection_items(item_id);
INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '5');
COMMIT;
";

fn item_from_row(r: &Row) -> rusqlite::Result<Item> {
    let kind_str: String = r.get(2)?;
    let files_json: Option<String> = r.get(17)?;
    let collection_ids: Option<String> = r.get(26)?;
    Ok(Item {
        id: r.get(0)?,
        uuid: r.get(1)?,
        kind: Kind::parse(&kind_str).unwrap_or(Kind::Text),
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
        last_used_at: r.get(5)?,
        source_bundle_id: r.get(6)?,
        source_app_name: r.get(7)?,
        title: r.get(8)?,
        text: r.get(9)?,
        text_blob: r.get(10)?,
        rich_blob: r.get(11)?,
        rich_type: r.get(12)?,
        image_blob: r.get(13)?,
        image_w: r.get(14)?,
        image_h: r.get(15)?,
        image_truncated: r.get::<_, i64>(16)? != 0,
        files: files_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        size_bytes: r.get(18)?,
        content_hash: r.get(19)?,
        ocr_text: r.get(20)?,
        link_title: r.get(21)?,
        link_domain: r.get(22)?,
        link_image_blob: r.get(23)?,
        device_id: r.get(24)?,
        deleted_at: r.get(25)?,
        collection_ids: collection_ids
            .map(|s| s.split(',').filter_map(|p| p.parse().ok()).collect())
            .unwrap_or_default(),
    })
}

fn load_item(conn: &Connection, id: i64) -> Result<Option<Item>> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items i WHERE i.id = ?1");
    let mut stmt = conn.prepare_cached(&sql)?;
    Ok(stmt.query_row(params![id], item_from_row).optional()?)
}

fn collect_ids<P: rusqlite::Params>(conn: &Connection, sql: &str, params: P) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Deletes item rows for good and releases their blobs. Runs inside the caller's transaction.
fn hard_delete(conn: &Connection, dir: &Path, ids: &[i64]) -> Result<usize> {
    let mut n = 0;
    for id in ids {
        let (blobs, kind, hash): (Vec<Option<String>>, String, String) = conn.query_row(
            "SELECT text_blob, rich_blob, image_blob, link_image_blob, kind, content_hash \
             FROM items WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    vec![r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?],
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        n += conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        for blob in blobs.into_iter().flatten() {
            release_blob(conn, &blob)?;
        }
        if kind == "file" {
            // File cards cache their Finder icon under the content hash, outside the blob store.
            let _ = fs::remove_file(thumb_path_in(dir, &hash));
        }
    }
    Ok(n)
}

fn prefix_at_char_boundary(s: &str, max: usize) -> String {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---- blobs ---------------------------------------------------------------------------------

/// Whether `s` is a blake3 hash as this app writes it: 64 lowercase hex digits. Blob ids and
/// content hashes name files on disk, so anything else (from a damaged or crafted backup) must
/// never reach a path.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A blob's file. An id that is not a hash maps to one fixed name inside the blob folder that is
/// never written, so it can neither escape the folder nor match a real blob.
fn blob_path_in(dir: &Path, id: &str) -> PathBuf {
    if !is_hash(id) {
        return dir.join("blobs").join("invalid");
    }
    dir.join("blobs").join(&id[..2]).join(id)
}

/// A cached PNG keyed by a hash, or by a hash plus `-preview` (see `jobs::preview_key`).
fn thumb_path_in(dir: &Path, key: &str) -> PathBuf {
    if !is_hash(key.strip_suffix("-preview").unwrap_or(key)) {
        return dir.join("thumbs").join("invalid.png");
    }
    dir.join("thumbs").join(format!("{key}.png"))
}

/// Writes through a temp file and a rename, so a reader never sees a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Stores `bytes` under their blake3 hash (once) and bumps the refcount.
fn put_blob(conn: &Connection, dir: &Path, bytes: &[u8], mime: &str) -> Result<String> {
    let id = blake3::hash(bytes).to_hex().to_string();
    let path = blob_path_in(dir, &id);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_atomic(&path, bytes)?;
    }
    conn.execute(
        "INSERT INTO blobs (id, size_bytes, mime, created_at, refcount) VALUES (?1, ?2, ?3, ?4, 1) \
         ON CONFLICT(id) DO UPDATE SET refcount = refcount + 1",
        params![id, bytes.len() as i64, mime, now_ms()],
    )?;
    Ok(id)
}

fn release_blob(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE blobs SET refcount = max(refcount - 1, 0) WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

/// Sets every blob's refcount to the number of item columns that reference it.
fn recount_blobs(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE blob_refs AS \
           SELECT text_blob AS id FROM items WHERE text_blob IS NOT NULL \
           UNION ALL SELECT rich_blob FROM items WHERE rich_blob IS NOT NULL \
           UNION ALL SELECT image_blob FROM items WHERE image_blob IS NOT NULL \
           UNION ALL SELECT link_image_blob FROM items WHERE link_image_blob IS NOT NULL; \
         CREATE INDEX temp.blob_refs_id ON blob_refs(id); \
         UPDATE blobs SET refcount = (SELECT count(*) FROM blob_refs r WHERE r.id = blobs.id); \
         DROP TABLE temp.blob_refs;",
    )?;
    Ok(())
}

const ITEM_TABLE_COLUMNS: &str = "uuid, kind, created_at, updated_at, last_used_at, \
    source_bundle_id, source_app_name, title, text, text_blob, rich_blob, rich_type, image_blob, \
    image_w, image_h, image_truncated, files_json, size_bytes, content_hash, ocr_text, link_title, \
    link_domain, link_image_blob, device_id, deleted_at";

/// Refuses a snapshot whose tables are not plain tables or whose blob ids and content hashes are
/// not hashes (they become file names).
fn validate_snapshot(conn: &Connection) -> Result<()> {
    for table in [
        "items",
        "collections",
        "collection_items",
        "apps",
        "blobs",
        "meta",
    ] {
        let kind: Option<String> = conn
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = ?1",
                params![table],
                |r| r.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("table") {
            return Err(anyhow!("the backup's `{table}` is not a table"));
        }
    }
    let names = {
        let mut stmt = conn.prepare(
            "SELECT id FROM blobs UNION ALL SELECT content_hash FROM items \
             UNION ALL SELECT text_blob FROM items WHERE text_blob IS NOT NULL \
             UNION ALL SELECT rich_blob FROM items WHERE rich_blob IS NOT NULL \
             UNION ALL SELECT image_blob FROM items WHERE image_blob IS NOT NULL \
             UNION ALL SELECT link_image_blob FROM items WHERE link_image_blob IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if let Some(bad) = names.iter().find(|n| !is_hash(n)) {
        return Err(anyhow!(
            "the backup names a file {:?}, which is not a hash; not restored",
            truncate_chars(bad, 80)
        ));
    }
    Ok(())
}

/// `merge_snapshot` with the snapshot attached as `b`.
fn merge_attached(conn: &mut Connection) -> Result<MergeReport> {
    let tx = conn.transaction()?;
    let (local_min, local_max, backup_min, backup_max): (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = tx.query_row(
        "SELECT (SELECT min(id) FROM main.items), (SELECT max(id) FROM main.items), \
         (SELECT min(id) FROM b.items), (SELECT max(id) FROM b.items)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let offset = match (local_min, backup_max) {
        (Some(min), Some(max)) if max >= min => min - 1 - max,
        _ => 0,
    };
    tx.execute_batch(
        "CREATE TEMP TABLE item_map (b_id INTEGER PRIMARY KEY, m_id INTEGER NOT NULL); \
         INSERT INTO item_map SELECT bi.id, mi.id FROM b.items bi \
           JOIN main.items mi ON mi.uuid = bi.uuid; \
         INSERT OR IGNORE INTO item_map SELECT bi.id, mi.id FROM b.items bi \
           JOIN main.items mi ON mi.content_hash = bi.content_hash AND mi.deleted_at IS NULL \
           WHERE bi.deleted_at IS NULL;",
    )?;
    let items = tx.execute(
        &format!(
            "INSERT INTO main.items (id, {ITEM_TABLE_COLUMNS}) \
             SELECT id + ?1, {ITEM_TABLE_COLUMNS} FROM b.items \
             WHERE deleted_at IS NULL AND id NOT IN (SELECT b_id FROM item_map) ORDER BY id"
        ),
        params![offset],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO item_map SELECT id, id + ?1 FROM b.items WHERE deleted_at IS NULL",
        params![offset],
    )?;
    if let (Some(_), Some(lo), Some(hi)) = (local_max, backup_min, backup_max) {
        tx.execute(
            "UPDATE main.items SET last_used_at = created_at \
             WHERE id BETWEEN ?1 AND ?2 AND last_used_at IS NULL AND created_at > \
               (SELECT min(created_at) FROM main.items \
                WHERE id > ?2 AND last_used_at IS NULL AND deleted_at IS NULL)",
            params![lo + offset, hi + offset],
        )?;
    }

    let (collection_offset, position_offset): (i64, i64) = tx.query_row(
        "SELECT COALESCE(max(id), 0), COALESCE(max(position) + 1, 0) FROM main.collections",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    tx.execute_batch(
        "CREATE TEMP TABLE collection_map (b_id INTEGER PRIMARY KEY, m_id INTEGER NOT NULL); \
         INSERT INTO collection_map SELECT bc.id, mc.id FROM b.collections bc \
           JOIN main.collections mc ON mc.uuid = bc.uuid;",
    )?;
    let collections = tx.execute(
        "INSERT INTO main.collections (id, uuid, name, color, position, created_at) \
         SELECT id + ?1, uuid, name, color, position + ?2, created_at FROM b.collections \
         WHERE id NOT IN (SELECT b_id FROM collection_map)",
        params![collection_offset, position_offset],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO collection_map SELECT id, id + ?1 FROM b.collections",
        params![collection_offset],
    )?;
    tx.execute_batch(
        "INSERT OR IGNORE INTO main.collection_items (collection_id, item_id, position, added_at) \
           SELECT cm.m_id, im.m_id, bci.position, bci.added_at FROM b.collection_items bci \
           JOIN collection_map cm ON cm.b_id = bci.collection_id \
           JOIN item_map im ON im.b_id = bci.item_id; \
         INSERT OR IGNORE INTO main.apps (bundle_id, name, last_seen_at) \
           SELECT bundle_id, name, last_seen_at FROM b.apps; \
         INSERT OR IGNORE INTO main.blobs (id, size_bytes, mime, created_at, refcount) \
           SELECT id, size_bytes, mime, created_at, 0 FROM b.blobs; \
         DROP TABLE temp.item_map; DROP TABLE temp.collection_map;",
    )?;
    recount_blobs(&tx)?;
    tx.commit()?;
    Ok(MergeReport { items, collections })
}

/// Removes blob rows and files with a zero refcount, plus their thumbnails.
fn delete_orphan_blobs(conn: &Connection, dir: &Path) -> Result<usize> {
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM blobs WHERE refcount <= 0")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for id in &ids {
        let _ = fs::remove_file(blob_path_in(dir, id));
        let _ = fs::remove_file(thumb_path_in(dir, id));
        let _ = fs::remove_file(thumb_path_in(dir, &crate::jobs::preview_key(id)));
        conn.execute("DELETE FROM blobs WHERE id = ?1", params![id])?;
    }
    Ok(ids.len())
}

// ---- search --------------------------------------------------------------------------------

/// Escapes a term for an FTS5 phrase: wrap in double quotes, double any inner quotes.
fn fts_phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// The FTS5 MATCH expression for the long terms of a query: phrases joined with AND, which for
/// the trigram tokenizer means "every term appears as a substring".
pub fn fts_match_expression(terms: &[String]) -> Option<String> {
    let phrases: Vec<String> = terms
        .iter()
        .filter(|t| t.chars().count() >= FTS_MIN_TERM_LEN)
        .map(|t| fts_phrase(t))
        .collect();
    if phrases.is_empty() {
        None
    } else {
        Some(phrases.join(" AND "))
    }
}

fn like_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// The columns a term can match (qualified by `table`, like "i."), as one LIKE group for a bound
/// parameter.
fn like_group(table: &str, param: usize) -> String {
    let columns: Vec<String> = ["title", "text", "ocr_text", "link_title", "source_app_name"]
        .iter()
        .map(|col| format!("{table}{col} LIKE ?{param} ESCAPE '\\'"))
        .collect();
    format!("({})", columns.join(" OR "))
}

/// Builds the paged search statement and its bound values. Ordering is the timeline
/// (`created_at DESC`), never relevance.
///
/// Full-text terms are not allowed to materialize every match: for a common trigram on 100k
/// items that costs tens of milliseconds. Instead FTS5 walks matches in descending rowid order
/// (which it does natively) and stops after `offset + limit + slack` rows. Rowid order is
/// creation order except for items moved to the front by a recopy or a paste, which carry
/// `last_used_at`. A moved item outside that window can only reach the page when the window
/// was cut short and the item is newer than the window's oldest row, so a second candidate set
/// checks exactly those with LIKE. The union is then filtered, sorted by time, and paged.
pub fn build_search_sql(
    query: &SearchQuery,
    offset: usize,
    limit: usize,
) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut clauses: Vec<String> = vec!["i.deleted_at IS NULL".to_string()];
    let mut binds: Vec<Value> = Vec::new();
    let mut ctes: Vec<String> = Vec::new();
    let mut from = "items i".to_string();

    // Filters are rendered twice: inside the FTS candidate scan (so the window is cut after
    // them) and on the final join (for the moved-set candidates). Parameters are shared.
    let mut fts_filters: Vec<String> = vec!["deleted_at IS NULL".to_string()];
    let mut outer_filters: Vec<String> = Vec::new();
    if !query.kinds.is_empty() {
        let placeholders: Vec<String> = query
            .kinds
            .iter()
            .map(|k| {
                binds.push(Value::Text(k.as_str().to_string()));
                format!("?{}", binds.len())
            })
            .collect();
        fts_filters.push(format!("kind IN ({})", placeholders.join(", ")));
        outer_filters.push(format!("i.kind IN ({})", placeholders.join(", ")));
    }
    if !query.apps.is_empty() {
        let placeholders: Vec<String> = query
            .apps
            .iter()
            .map(|a| {
                binds.push(Value::Text(a.clone()));
                format!("?{}", binds.len())
            })
            .collect();
        fts_filters.push(format!("source_bundle_id IN ({})", placeholders.join(", ")));
        outer_filters.push(format!(
            "i.source_bundle_id IN ({})",
            placeholders.join(", ")
        ));
    }
    if let Some(since) = query.since_ms {
        binds.push(Value::Integer(since));
        fts_filters.push(format!("created_at >= ?{}", binds.len()));
        outer_filters.push(format!("i.created_at >= ?{}", binds.len()));
    }
    if let Some(until) = query.until_ms {
        binds.push(Value::Integer(until));
        fts_filters.push(format!("created_at < ?{}", binds.len()));
        outer_filters.push(format!("i.created_at < ?{}", binds.len()));
    }
    if !query.collections.is_empty() {
        let placeholders: Vec<String> = query
            .collections
            .iter()
            .map(|id| {
                binds.push(Value::Integer(*id));
                format!("?{}", binds.len())
            })
            .collect();
        fts_filters.push(format!(
            "rowid IN (SELECT item_id FROM collection_items WHERE collection_id IN ({}))",
            placeholders.join(", ")
        ));
        outer_filters.push(format!(
            "EXISTS (SELECT 1 FROM collection_items p WHERE p.item_id = i.id AND p.collection_id IN ({}))",
            placeholders.join(", ")
        ));
    }

    let long_terms: Vec<&String> = query
        .terms
        .iter()
        .filter(|t| t.chars().count() >= FTS_MIN_TERM_LEN)
        .collect();
    if let Some(expr) = fts_match_expression(&query.terms) {
        binds.push(Value::Text(expr));
        let match_param = binds.len();
        binds.push(Value::Integer(
            (offset + limit + FTS_CANDIDATE_SLACK) as i64,
        ));
        let window_param = binds.len();
        let moved_like: Vec<String> = long_terms
            .iter()
            .map(|term| {
                binds.push(Value::Text(like_pattern(term)));
                like_group("", binds.len())
            })
            .collect();
        ctes.push(format!(
            "fts AS MATERIALIZED (SELECT rowid AS id FROM items_fts WHERE items_fts MATCH \
             ?{match_param} AND {} ORDER BY rowid DESC LIMIT ?{window_param})",
            fts_filters.join(" AND ")
        ));
        ctes.push(format!(
            // Pinned to the partial index: it holds only moved items, but without statistics
            // the planner would scan the whole table.
            "moved AS (SELECT id FROM items INDEXED BY items_moved \
             WHERE last_used_at IS NOT NULL AND deleted_at IS NULL \
             AND (SELECT count(*) FROM fts) >= ?{window_param} \
             AND created_at >= (SELECT min(m.created_at) FROM fts JOIN items m ON m.id = fts.id) \
             AND {})",
            moved_like.join(" AND ")
        ));
        ctes.push("cand AS (SELECT id FROM fts UNION SELECT id FROM moved)".to_string());
        from = "items i JOIN cand ON cand.id = i.id".to_string();
    }
    clauses.extend(outer_filters);
    let short_terms: Vec<&String> = query
        .terms
        .iter()
        .filter(|t| t.chars().count() < FTS_MIN_TERM_LEN && !t.is_empty())
        .collect();
    if !short_terms.is_empty() {
        // Short terms are matched with LIKE while walking items newest first, so a common
        // character fills the page within a few hundred rows; the walk stops at the newest
        // `SHORT_QUERY_SCAN_ROWS` items.
        clauses.push(format!(
            "i.created_at >= COALESCE((SELECT created_at FROM items WHERE deleted_at IS NULL \
             ORDER BY created_at DESC LIMIT 1 OFFSET {}), {})",
            SHORT_QUERY_SCAN_ROWS - 1,
            i64::MIN
        ));
    }
    for term in short_terms {
        binds.push(Value::Text(like_pattern(term)));
        clauses.push(like_group("i.", binds.len()));
    }
    binds.push(Value::Integer(limit as i64));
    let limit_param = binds.len();
    binds.push(Value::Integer(offset as i64));
    let offset_param = binds.len();
    let with = if ctes.is_empty() {
        String::new()
    } else {
        format!("WITH {} ", ctes.join(", "))
    };
    // Pick the page by id first; the wide item columns (and the collection subquery) are only
    // fetched for the rows that survive the sort and limit.
    let sql = format!(
        "{with}SELECT {ITEM_COLUMNS} FROM items i JOIN (SELECT i.id FROM {from} WHERE {} \
         ORDER BY i.created_at DESC, i.id DESC LIMIT ?{limit_param} OFFSET ?{offset_param}) page \
         ON page.id = i.id ORDER BY i.created_at DESC, i.id DESC",
        clauses.join(" AND "),
    );
    (sql, binds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        (dir, store)
    }

    fn text_item(store: &Store, text: &str) -> Item {
        let mut new = NewItem::text(text);
        new.source_bundle_id = Some("com.apple.TextEdit".into());
        new.source_app_name = Some("TextEdit".into());
        store.insert(&new).expect("insert").0
    }

    #[test]
    fn only_hashes_become_file_names() {
        let dir = Path::new("/lib");
        let hash = "a".repeat(64);
        assert!(is_hash(&hash));
        assert_eq!(
            blob_path_in(dir, &hash),
            dir.join("blobs").join("aa").join(&hash)
        );
        assert_eq!(
            thumb_path_in(dir, &format!("{hash}-preview")),
            dir.join("thumbs").join(format!("{hash}-preview.png"))
        );
        for bad in [
            "../../x",
            "",
            "é",
            &"A".repeat(64),
            &format!("{}/", "a".repeat(63)),
        ] {
            assert!(!is_hash(bad), "{bad:?}");
            assert_eq!(blob_path_in(dir, bad), dir.join("blobs").join("invalid"));
            assert_eq!(
                thumb_path_in(dir, bad),
                dir.join("thumbs").join("invalid.png")
            );
        }
    }

    #[test]
    fn schema_creates_and_reopens() {
        let (dir, store) = temp_store();
        assert_eq!(store.count_items().unwrap(), 0);
        let device = store.device_id.clone();
        drop(store);
        let again = Store::open(dir.path()).unwrap();
        assert_eq!(again.device_id, device);
        let mode: String = again
            .read(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode, "wal");
        let perms = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(perms, 0o700);
    }

    #[test]
    fn insert_dedupes_and_moves_to_front() {
        let (_dir, store) = temp_store();
        let a = text_item(&store, "alpha");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = text_item(&store, "beta");
        assert!(b.created_at >= a.created_at);
        let history = store.history(None, 10).unwrap();
        assert_eq!(
            history.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![b.id, a.id]
        );

        std::thread::sleep(std::time::Duration::from_millis(2));
        let (again, was_new) = store.insert(&NewItem::text("alpha")).unwrap();
        assert!(!was_new);
        assert_eq!(again.id, a.id);
        assert_eq!(store.count_items().unwrap(), 2);
        let history = store.history(None, 10).unwrap();
        assert_eq!(history[0].id, a.id, "recopied item is first");
    }

    #[test]
    fn history_pages_by_cursor() {
        let (_dir, store) = temp_store();
        for i in 0..7 {
            text_item(&store, &format!("item {i}"));
        }
        let first = store.history(None, 3).unwrap();
        assert_eq!(first.len(), 3);
        let cursor = Cursor {
            created_at: first[2].created_at,
            id: first[2].id,
        };
        let second = store.history(Some(cursor), 3).unwrap();
        assert_eq!(second.len(), 3);
        assert!(first.iter().all(|f| second.iter().all(|s| s.id != f.id)));
        let cursor = Cursor {
            created_at: second[2].created_at,
            id: second[2].id,
        };
        let third = store.history(Some(cursor), 3).unwrap();
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn long_text_goes_to_blob() {
        let (_dir, store) = temp_store();
        let long = "x".repeat(TEXT_INLINE_LIMIT + 10);
        let item = text_item(&store, &long);
        assert_eq!(item.text.as_ref().unwrap().len(), TEXT_INLINE_LIMIT);
        assert!(item.text_blob.is_some());
        assert_eq!(store.full_text(&item).unwrap().unwrap(), long);
    }

    #[test]
    fn image_and_file_items_store_blobs_and_paths() {
        let (_dir, store) = temp_store();
        let img = NewItem {
            kind: Some(Kind::Image),
            image: Some(NewImage {
                png_or_original: vec![1, 2, 3, 4],
                mime: "image/png",
                width: 10,
                height: 20,
                truncated: false,
            }),
            ..Default::default()
        };
        let (item, _) = store.insert(&img).unwrap();
        assert_eq!(item.display_title(), "10×20");
        let blob = item.image_blob.clone().unwrap();
        assert_eq!(fs::read(store.blob_path(&blob)).unwrap(), vec![1, 2, 3, 4]);

        let files = NewItem {
            kind: Some(Kind::File),
            files: vec!["/tmp/b.txt".into(), "/tmp/a.txt".into()],
            ..Default::default()
        };
        let (item, _) = store.insert(&files).unwrap();
        assert_eq!(item.display_title(), "b.txt, a.txt");
        let swapped = NewItem {
            kind: Some(Kind::File),
            files: vec!["/tmp/a.txt".into(), "/tmp/b.txt".into()],
            ..Default::default()
        };
        assert_eq!(
            swapped.content_hash(),
            files.content_hash(),
            "hash ignores path order"
        );
    }

    #[test]
    fn delete_restore_and_sweep() {
        let (_dir, store) = temp_store();
        let a = text_item(&store, "one");
        let b = text_item(&store, "two");
        assert_eq!(store.delete(&[a.id]).unwrap(), 1);
        assert_eq!(store.count_items().unwrap(), 1);
        assert_eq!(store.restore(&[a.id]).unwrap(), 1);
        assert_eq!(store.count_items().unwrap(), 2);

        // A recopy after deletion blocks restore of the old row.
        store.delete(&[b.id]).unwrap();
        let (b2, was_new) = store.insert(&NewItem::text("two")).unwrap();
        assert!(was_new);
        assert_ne!(b2.id, b.id);
        assert_eq!(store.restore(&[b.id]).unwrap(), 0);

        // Retention: age an item and sweep.
        store
            .write(|c| {
                c.execute(
                    "UPDATE items SET created_at = 0 WHERE id = ?1",
                    params![a.id],
                )?;
                Ok(())
            })
            .unwrap();
        let report = store.sweep(Retention::Day).unwrap();
        assert_eq!(report.expired_items, 1);
        assert!(store.get(a.id).unwrap().is_none());
        assert_eq!(store.sweep(Retention::Forever).unwrap().expired_items, 0);
    }

    #[test]
    fn collection_items_survive_retention_and_erase() {
        let (_dir, store) = temp_store();
        let a = text_item(&store, "keep me");
        let b = text_item(&store, "drop me");
        let board = store.create_collection("Snippets", "blue").unwrap();
        store.add_to_collection(a.id, board.id).unwrap();
        store
            .write(|c| {
                c.execute("UPDATE items SET created_at = 0", [])?;
                Ok(())
            })
            .unwrap();
        let report = store.sweep(Retention::Day).unwrap();
        assert_eq!(report.expired_items, 1);
        assert!(store.get(a.id).unwrap().is_some());
        assert!(store.get(b.id).unwrap().is_none());
        let c = text_item(&store, "gone on erase");
        assert_eq!(store.erase_history().unwrap(), 1);
        assert!(store.get(c.id).unwrap().is_none());
        assert_eq!(
            store.get(a.id).unwrap().unwrap().collection_ids,
            vec![board.id]
        );
        assert_eq!(store.collections().unwrap().len(), 1);
    }

    #[test]
    fn collections_crud_and_order() {
        let (_dir, store) = temp_store();
        let p1 = store.create_collection("One", "red").unwrap();
        let p2 = store.create_collection("Two", "green").unwrap();
        assert!(p1.position < p2.position);
        store.reorder_collections(&[p2.id, p1.id]).unwrap();
        let boards = store.collections().unwrap();
        assert_eq!(boards[0].id, p2.id);
        store.update_collection(p1.id, Some("Uno"), None).unwrap();
        assert_eq!(store.collections().unwrap()[1].name, "Uno");

        let a = text_item(&store, "a");
        let b = text_item(&store, "b");
        store.add_to_collection(a.id, p1.id).unwrap();
        store.add_to_collection(b.id, p1.id).unwrap();
        let items = store.collection_items(p1.id, 0, 10).unwrap();
        assert_eq!(
            items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![b.id, a.id],
            "most recently added first"
        );
        store
            .reorder_collection_items(p1.id, &[a.id, b.id])
            .unwrap();
        let items = store.collection_items(p1.id, 0, 10).unwrap();
        assert_eq!(
            items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![a.id, b.id]
        );
        store.remove_from_collection(a.id, p1.id).unwrap();
        assert_eq!(store.collection_items(p1.id, 0, 10).unwrap().len(), 1);
        store.delete_collection(p1.id).unwrap();
        assert!(store.get(b.id).unwrap().unwrap().collection_ids.is_empty());
        assert_eq!(store.count_items().unwrap(), 2, "items stay in history");
    }

    #[test]
    fn search_terms_and_filters() {
        let (_dir, store) = temp_store();
        text_item(&store, "The quick brown fox");
        text_item(&store, "jumps over the lazy dog");
        let mut link = NewItem::text("https://example.com/path");
        link.kind = Some(Kind::Link);
        link.source_bundle_id = Some("com.apple.Safari".into());
        link.source_app_name = Some("Safari".into());
        store.insert(&link).unwrap();

        let q = SearchQuery {
            terms: vec!["QUICK".into()],
            ..Default::default()
        };
        let hits = store.search(&q, 0, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].text.as_ref().unwrap().contains("quick"),
            "case-insensitive trigram"
        );

        let q = SearchQuery {
            terms: vec!["the".into(), "dog".into()],
            ..Default::default()
        };
        assert_eq!(store.search(&q, 0, 10).unwrap().len(), 1);

        let q = SearchQuery {
            terms: vec!["ox".into()],
            ..Default::default()
        };
        assert_eq!(
            store.search(&q, 0, 10).unwrap().len(),
            1,
            "short term via LIKE"
        );

        let q = SearchQuery {
            kinds: vec![Kind::Link],
            ..Default::default()
        };
        assert_eq!(store.search(&q, 0, 10).unwrap().len(), 1);

        let q = SearchQuery {
            apps: vec!["com.apple.Safari".into()],
            ..Default::default()
        };
        assert_eq!(store.search(&q, 0, 10).unwrap().len(), 1);

        let q = SearchQuery {
            terms: vec!["safari".into()],
            ..Default::default()
        };
        assert_eq!(
            store.search(&q, 0, 10).unwrap().len(),
            1,
            "matches source app name"
        );

        let q = SearchQuery {
            since_ms: Some(now_ms() + 10_000),
            ..Default::default()
        };
        assert_eq!(store.search(&q, 0, 10).unwrap().len(), 0);

        let board = store.create_collection("B", "red").unwrap();
        let first = store.history(None, 1).unwrap()[0].clone();
        store.add_to_collection(first.id, board.id).unwrap();
        let q = SearchQuery {
            collections: vec![board.id],
            ..Default::default()
        };
        assert_eq!(store.search(&q, 0, 10).unwrap()[0].id, first.id);
    }

    #[test]
    fn search_applies_every_filter_and_both_term_kinds() {
        let (_dir, store) = temp_store();
        let board = store.create_collection("Work", "blue").unwrap();
        let add = |text: &str, kind: Kind, app: &str, in_collection: bool| {
            let (item, _) = store
                .insert(&NewItem {
                    kind: Some(kind),
                    text: Some(text.into()),
                    source_bundle_id: Some(app.into()),
                    source_app_name: Some(app.into()),
                    ..Default::default()
                })
                .unwrap();
            if in_collection {
                store.add_to_collection(item.id, board.id).unwrap();
            }
            item
        };
        let hit = add("quarterly xy report", Kind::Text, "com.notes", true);
        add("quarterly xy draft", Kind::Text, "com.mail", true); // other app
        add("quarterly xy memo", Kind::Text, "com.notes", false); // not in the collection
        add("quarterly summary", Kind::Text, "com.notes", true); // no short term
        add("quarterly xy link", Kind::Link, "com.notes", true); // other kind
        add("annual xy report", Kind::Text, "com.notes", true); // no long term

        let query = SearchQuery {
            terms: vec!["quarterly".into(), "xy".into()],
            kinds: vec![Kind::Text],
            apps: vec!["com.notes".into()],
            since_ms: Some(0),
            until_ms: Some(now_ms() + 60_000),
            collections: vec![board.id],
        };
        let found: Vec<i64> = store
            .search(&query, 0, 10)
            .unwrap()
            .iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(found, vec![hit.id]);

        // Filters alone, without terms.
        let links = SearchQuery {
            kinds: vec![Kind::Link],
            ..Default::default()
        };
        assert_eq!(store.search(&links, 0, 10).unwrap().len(), 1);
        // A quote inside a term is matched literally.
        add("say \"hi\" there", Kind::Text, "com.notes", false);
        let quoted = SearchQuery {
            terms: vec!["\"hi\"".into()],
            ..Default::default()
        };
        assert_eq!(store.search(&quoted, 0, 10).unwrap().len(), 1);
    }

    #[test]
    fn filtered_search_does_not_drop_matches_beyond_the_window() {
        let (_dir, store) = temp_store();
        // Many text matches, then a few older link matches: the links must still be found.
        let mut link_ids = Vec::new();
        for i in 0..3 {
            let mut link = NewItem::text(format!("https://needle{i}.example.com/"));
            link.kind = Some(Kind::Link);
            link_ids.push(store.insert(&link).unwrap().0.id);
        }
        for i in 0..(FTS_CANDIDATE_SLACK + 250) {
            text_item(&store, &format!("needle text {i}"));
        }
        let q = SearchQuery {
            terms: vec!["needle".into()],
            kinds: vec![Kind::Link],
            ..Default::default()
        };
        let hits = store.search(&q, 0, 10).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|h| link_ids.contains(&h.id)));
    }

    #[test]
    fn editing_into_a_duplicate_merges_collections_and_frees_blobs() {
        let (_dir, store) = temp_store();
        let board = store.create_collection("B", "red").unwrap();
        let mut rich = NewItem::text("target");
        rich.rich = Some((b"{\\rtf1 target}".to_vec(), "public.rtf"));
        let target = store.insert(&rich).unwrap().0;
        store.add_to_collection(target.id, board.id).unwrap();
        let rich_blob = target.rich_blob.clone().unwrap();
        let draft = text_item(&store, "draft");
        let edited = store.update_text(draft.id, "target").unwrap();
        assert_eq!(edited.id, draft.id);
        assert!(
            store.get(target.id).unwrap().is_none(),
            "duplicate row is gone"
        );
        assert_eq!(
            edited.collection_ids,
            vec![board.id],
            "membership moved over"
        );
        let refcount: i64 = store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT refcount FROM blobs WHERE id = ?1",
                    params![rich_blob],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(refcount, 0, "the duplicate's rich blob was released");
    }

    #[test]
    fn search_finds_items_moved_to_front_beyond_the_fts_window() {
        let (_dir, store) = temp_store();
        let oldest = text_item(&store, "needle 0");
        for i in 1..(FTS_CANDIDATE_SLACK + 400) {
            text_item(&store, &format!("needle {i}"));
        }
        let q = SearchQuery {
            terms: vec!["needle".into()],
            ..Default::default()
        };
        let first = store.search(&q, 0, 5).unwrap();
        assert_eq!(first.len(), 5);
        assert_ne!(first[0].id, oldest.id);
        // Recopying the oldest item moves it to the front; its rowid is far outside the window.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (again, was_new) = store.insert(&NewItem::text("needle 0")).unwrap();
        assert!(!was_new);
        assert_eq!(again.id, oldest.id);
        let first = store.search(&q, 0, 5).unwrap();
        assert_eq!(first[0].id, oldest.id, "moved item leads the results");
        // Paging by offset stays consistent and does not repeat rows.
        let page1 = store.search(&q, 0, 200).unwrap();
        let page2 = store.search(&q, 200, 200).unwrap();
        assert_eq!(page1.len(), 200);
        assert!(page1.iter().all(|a| page2.iter().all(|b| a.id != b.id)));
    }

    #[test]
    fn edit_rename_and_touch() {
        let (_dir, store) = temp_store();
        let a = text_item(&store, "draft");
        store.rename(a.id, Some("  My title ")).unwrap();
        assert_eq!(
            store.get(a.id).unwrap().unwrap().title.as_deref(),
            Some("My title")
        );
        store.rename(a.id, Some("   ")).unwrap();
        assert_eq!(store.get(a.id).unwrap().unwrap().title, None);

        let edited = store.update_text(a.id, "final").unwrap();
        assert_eq!(edited.text.as_deref(), Some("final"));
        assert_eq!(edited.content_hash, NewItem::text("final").content_hash());
        let q = SearchQuery {
            terms: vec!["final".into()],
            ..Default::default()
        };
        assert_eq!(
            store.search(&q, 0, 10).unwrap().len(),
            1,
            "fts follows edits"
        );

        let b = text_item(&store, "other");
        assert_eq!(store.history(None, 5).unwrap()[0].id, b.id);
        std::thread::sleep(std::time::Duration::from_millis(2));
        store.touch_used(a.id).unwrap();
        let top = store.history(None, 5).unwrap()[0].clone();
        assert_eq!(top.id, a.id);
        assert!(top.last_used_at.is_some());
    }

    #[test]
    fn blobs_are_refcounted_and_swept() {
        let (_dir, store) = temp_store();
        let img = NewItem {
            kind: Some(Kind::Image),
            image: Some(NewImage {
                png_or_original: vec![9; 100],
                mime: "image/png",
                width: 1,
                height: 1,
                truncated: false,
            }),
            ..Default::default()
        };
        let (item, _) = store.insert(&img).unwrap();
        let blob = item.image_blob.clone().unwrap();
        store
            .set_link_metadata(item.id, None, Some(&[9; 100]))
            .unwrap();
        let refcount: i64 = store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT refcount FROM blobs WHERE id = ?1",
                    params![blob],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(refcount, 2, "same bytes shared by two references");
        store.delete(&[item.id]).unwrap();
        store
            .write(|c| {
                c.execute("UPDATE items SET deleted_at = 0", [])?;
                Ok(())
            })
            .unwrap();
        let report = store.sweep(Retention::Forever).unwrap();
        assert_eq!(report.purged_deleted, 1);
        assert_eq!(report.orphan_blobs, 1);
        assert!(!store.blob_path(&blob).exists());
    }

    #[test]
    fn apps_and_export() {
        let (_dir, store) = temp_store();
        text_item(&store, "s1");
        text_item(&store, "s2");
        store.release_memory();
        let apps = store.apps().unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name.as_deref(), Some("TextEdit"));
        let json = store.export_json().unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn data_version_ignores_own_writes() {
        let (dir, store) = temp_store();
        let before = store.data_version().unwrap();
        text_item(&store, "ours");
        assert_eq!(store.data_version(), Some(before));
        // Another process (the CLI) writing the same file does change it.
        let other = Store::open(dir.path()).unwrap();
        other.insert(&NewItem::text("theirs")).unwrap();
        assert_ne!(store.data_version(), Some(before));
    }

    #[test]
    fn imported_items_go_below_existing_ids_and_keep_their_time() {
        let (_dir, store) = temp_store();
        let current = text_item(&store, "copied in PasteLoop");
        let shared = text_item(&store, "copied in both");
        // Imports arrive newest first.
        let (newer, new1) = store
            .insert_imported(&NewItem::text("old b"), 2_000)
            .unwrap();
        let (older, new2) = store
            .insert_imported(&NewItem::text("old a"), 1_000)
            .unwrap();
        assert!(new1 && new2);
        assert!(older.id < newer.id && newer.id < current.id);
        assert_eq!((older.created_at, newer.created_at), (1_000, 2_000));
        // A duplicate stays where it is.
        let (dup, was_new) = store
            .insert_imported(&NewItem::text("copied in both"), 500)
            .unwrap();
        assert!(!was_new);
        assert_eq!((dup.id, dup.created_at), (shared.id, shared.created_at));
        let history: Vec<i64> = store
            .history(None, 10)
            .unwrap()
            .iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(history, vec![shared.id, current.id, newer.id, older.id]);
    }

    #[test]
    fn imports_newer_than_existing_history_stay_searchable_in_order() {
        let (_dir, store) = temp_store();
        let old = store.insert(&NewItem::text("needle current")).unwrap().0;
        store
            .write(|c| {
                c.execute(
                    "UPDATE items SET created_at = 1000 WHERE id = ?1",
                    params![old.id],
                )?;
                Ok(())
            })
            .unwrap();
        for i in 0..(FTS_CANDIDATE_SLACK + 400) {
            text_item(&store, &format!("needle filler {i}"));
        }
        // Imported, newer than `old` but placed below it: it must still lead a time-ordered page.
        let (imported, _) = store
            .insert_imported(&NewItem::text("needle imported"), now_ms() + 60_000)
            .unwrap();
        assert!(imported.id < old.id);
        let q = SearchQuery {
            terms: vec!["needle".into()],
            ..Default::default()
        };
        assert_eq!(store.search(&q, 0, 5).unwrap()[0].id, imported.id);
        // One older than everything stays in order without the mark.
        let (ancient, _) = store
            .insert_imported(&NewItem::text("needle ancient"), 10)
            .unwrap();
        assert!(ancient.last_used_at.is_none());
    }

    #[test]
    fn opens_a_pre_release_store_and_renames_its_pinboards() {
        let (dir, store) = temp_store();
        let item = text_item(&store, "kept");
        let board = store.create_collection("Work", "blue").unwrap();
        store.add_to_collection(item.id, board.id).unwrap();
        // Back to schema 4, as pre-release builds left it.
        store
            .write(|c| {
                c.execute_batch(
                    "ALTER TABLE collection_items RENAME COLUMN collection_id TO pinboard_id;
                     ALTER TABLE collection_items RENAME TO pinboard_items;
                     ALTER TABLE collections RENAME TO pinboards;
                     DROP INDEX collection_items_item;
                     CREATE INDEX pinboard_items_item ON pinboard_items(item_id);
                     UPDATE meta SET value = '4' WHERE key = 'schema_version';",
                )?;
                Ok(())
            })
            .unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.collections().unwrap()[0].name, "Work");
        let items = store.collection_items(board.id, 0, 10).unwrap();
        assert_eq!(
            items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![item.id]
        );
        assert_eq!(items[0].collection_ids, vec![board.id]);
    }
}
