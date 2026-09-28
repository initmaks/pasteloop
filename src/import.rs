//! One-time migration from the Paste app (`com.wiheads.paste-setapp`). Paste keeps its clips in
//! a Core Data SQLite store. This opens it read-only, decodes each clip's pasteboard payload,
//! rebuilds the item through the same classifier the poller uses, and inserts it with its
//! timestamp, custom title, source app, and collection.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use tracing::{info, warn};

use crate::clipboard::{
    self, Snapshot, UTI_FILE_URL, UTI_HTML, UTI_JPEG, UTI_PNG, UTI_RTF, UTI_TIFF, UTI_UTF8,
};
use crate::macos::FrontApp;
use crate::store::Store;

/// Seconds between the Unix epoch (1970) and the Core Data reference date (2001).
const CORE_DATA_EPOCH: f64 = 978_307_200.0;

#[derive(Debug, Default, Clone, Copy)]
pub struct ImportStats {
    pub imported: usize,
    pub skipped: usize,
    pub collections_created: usize,
}

/// The Paste database for this user, if the app's data is present.
pub fn default_paste_db() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path =
        Path::new(&home).join("Library/Application Support/com.wiheads.paste-setapp/db.sqlite");
    path.exists().then_some(path)
}

/// Imports up to `limit` most-recent clips from Paste at `db` into `store`. With `dry_run`, it
/// parses and counts but writes nothing.
pub fn import_from_paste(
    store: &Store,
    db: &Path,
    limit: Option<usize>,
    dry_run: bool,
) -> Result<ImportStats> {
    let uri = format!("file:{}?immutable=1", db.display());
    let conn = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open Paste database at {}", db.display()))?;

    // Lists: Z_PK -> (name, is_history). Paste's default history list (ZRAWTYPE = 1) is not a
    // collection; named lists are.
    let mut lists: HashMap<i64, (String, bool)> = HashMap::new();
    {
        let mut q = conn.prepare("SELECT Z_PK, ZNAME, ZRAWTYPE FROM ZLISTENTITY")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for row in rows {
            let (pk, name, raw) = row?;
            lists.insert(pk, (name, raw == Some(1)));
        }
    }

    let mut q = conn.prepare(
        "SELECT i.ZTITLE, i.ZCREATEDAT, i.ZLIST, d.ZRAWPASTEBOARDITEMS, \
         a.ZBUNDLEIDENTIFIER, a.ZNAME \
         FROM ZITEMENTITY i JOIN ZITEMDATAENTITY d ON d.ZITEM = i.Z_PK \
         LEFT JOIN ZAPPLICATIONENTITY a ON a.Z_PK = i.ZSOURCEAPPLICATION \
         ORDER BY i.ZCREATEDAT DESC",
    )?;
    let mut rows = q.query([])?;

    let mut stats = ImportStats::default();
    let mut collection_ids: HashMap<i64, i64> = HashMap::new();
    let palette = crate::cards::COLLECTION_PALETTE;
    let mut taken = 0usize;

    while let Some(row) = rows.next()? {
        if limit.is_some_and(|l| taken >= l) {
            break;
        }
        let title: Option<String> = row.get(0)?;
        let created: Option<f64> = row.get(1)?;
        let list: Option<i64> = row.get(2)?;
        let blob: Vec<u8> = row.get(3)?;
        let bundle: Option<String> = row.get(4)?;
        let app_name: Option<String> = row.get(5)?;

        let Some(snap) = parse_pasteboard(&blob) else {
            stats.skipped += 1;
            continue;
        };
        let source = FrontApp {
            pid: 0,
            bundle_id: bundle,
            name: app_name,
        };
        let mut new = match clipboard::build_item(snap, Some(&source)) {
            Ok(item) => item,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        // Paste's title is usually the auto-generated text preview; only carry it over when it is
        // a real custom name (differs from the item's own text), so cards are not all "named".
        if let Some(t) = title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            let redundant = new.text.as_deref().map(str::trim) == Some(t);
            if !redundant {
                new.title = Some(t.to_string());
            }
        }
        // A clip without a date has no place in the timeline.
        let Some(created_ms) = created.map(|secs| ((secs + CORE_DATA_EPOCH) * 1000.0) as i64)
        else {
            stats.skipped += 1;
            continue;
        };
        taken += 1;
        if dry_run {
            stats.imported += 1;
            continue;
        }

        let (item, was_new) = match store.insert_imported(&new, created_ms) {
            Ok(inserted) => inserted,
            Err(err) => {
                warn!("import: insert failed: {err:#}");
                stats.skipped += 1;
                continue;
            }
        };
        if !was_new {
            // Already in PasteLoop: keep that copy's time, but still honor the collection.
            stats.skipped += 1;
        }
        if let Some(list_pk) = list
            && let Some((name, is_history)) = lists.get(&list_pk)
            && !is_history
            && !name.trim().is_empty()
        {
            let collection = match collection_ids.get(&list_pk) {
                Some(id) => *id,
                None => {
                    let color = palette[collection_ids.len() % palette.len()];
                    let pb = store.create_collection(name, color)?;
                    collection_ids.insert(list_pk, pb.id);
                    stats.collections_created += 1;
                    pb.id
                }
            };
            if let Err(err) = store.add_to_collection(item.id, collection) {
                warn!(
                    "import: adding item {} to its collection failed: {err:#}",
                    item.id
                );
            }
        }
        if was_new {
            stats.imported += 1;
        }
    }
    info!(
        "import from Paste: imported={} skipped={} collections={}",
        stats.imported, stats.skipped, stats.collections_created
    );
    Ok(stats)
}

/// Builds a [`Snapshot`] from a Paste pasteboard payload, or `None` when it cannot be read.
///
/// The payload is a one-byte prefix, then the pasteboard data. Prefix 0x01 is inline; 0x02 is a
/// reference to another clip, which is not resolved. Inline data is a binary plist (raw or LZFSE,
/// magic `bvx`) or zlib-compressed JSON with base64 values, both shaped
/// `[{ types, dataByType: { uti: <bytes> } }]`.
fn parse_pasteboard(blob: &[u8]) -> Option<Snapshot> {
    if blob.first() != Some(&1) {
        return None;
    }
    let body = &blob[1..];
    let data_by_type = if body.starts_with(b"bplist00") {
        plist_data_by_type(body)?
    } else if body.starts_with(b"bvx") {
        plist_data_by_type(&crate::macos::lzfse_decode(body)?)?
    } else {
        let decoded = crate::macos::zlib_decode(body)?;
        if decoded.starts_with(b"bplist00") {
            plist_data_by_type(&decoded)?
        } else {
            json_data_by_type(&decoded)?
        }
    };
    Some(snapshot_from(data_by_type))
}

/// `dataByType` (uti -> bytes) from a binary-plist pasteboard payload.
fn plist_data_by_type(bytes: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    let value = plist::Value::from_reader(Cursor::new(bytes)).ok()?;
    let dict = value
        .as_array()?
        .first()?
        .as_dictionary()?
        .get("dataByType")?
        .as_dictionary()?;
    Some(
        dict.iter()
            .filter_map(|(k, v)| v.as_data().map(|b| (k.clone(), b.to_vec())))
            .collect(),
    )
}

/// `dataByType` (uti -> bytes) from a JSON pasteboard payload (values are base64).
fn json_data_by_type(bytes: &[u8]) -> Option<HashMap<String, Vec<u8>>> {
    use base64::Engine;
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let dict = value.as_array()?.first()?.get("dataByType")?.as_object()?;
    let mut out = HashMap::new();
    for (uti, val) in dict {
        if let Some(s) = val.as_str()
            && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(s)
        {
            out.insert(uti.clone(), decoded);
        }
    }
    Some(out)
}

fn snapshot_from(data_by_type: HashMap<String, Vec<u8>>) -> Snapshot {
    let mut snap = Snapshot {
        change_count: 0,
        types: Vec::with_capacity(data_by_type.len()),
        file_paths: Vec::new(),
        image: None,
        text: None,
        rtf: None,
        html: None,
    };
    for (uti, bytes) in data_by_type {
        match uti.as_str() {
            UTI_UTF8 => snap.text = Some(String::from_utf8_lossy(&bytes).into_owned()),
            UTI_RTF => snap.rtf = Some(bytes),
            UTI_HTML => snap.html = Some(bytes),
            UTI_PNG => snap.image = Some((bytes, UTI_PNG)),
            UTI_JPEG if snap.image.is_none() => snap.image = Some((bytes, UTI_JPEG)),
            UTI_TIFF if snap.image.is_none() => snap.image = Some((bytes, UTI_TIFF)),
            UTI_FILE_URL => snap.file_paths.extend(file_url_to_path(&bytes)),
            "public.url" if snap.text.is_none() => {
                snap.text = Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            _ => {}
        }
        snap.types.push(uti);
    }
    snap
}

fn file_url_to_path(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    url::Url::parse(s.trim())
        .ok()?
        .to_file_path()
        .ok()?
        .to_str()
        .map(str::to_string)
}
