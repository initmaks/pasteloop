//! One-time migration from the Paste app. Paste keeps its clips in a Core Data SQLite store
//! (`db.sqlite`), with payloads too large for a row in files beside it. This opens the store
//! read-only, decodes each clip's pasteboard payload, rebuilds the item through the same
//! classifier the poller uses, and inserts it with its timestamp, custom title, source app, and
//! collection. Clips already in the library (same content) are left as they are, so running it
//! again only adds what is missing.

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

/// Where Paste keeps its store, relative to the home folder: the Mac App Store edition (in its
/// sandbox container), then the Setapp edition.
const PASTE_DBS: [&str; 2] = [
    "Library/Containers/com.wiheads.paste/Data/Library/Application Support/Paste/db.sqlite",
    "Library/Application Support/com.wiheads.paste-setapp/db.sqlite",
];

/// The largest decoded payload read for one clip; the live capture keeps nothing larger than
/// `clipboard::MAX_TOTAL_BYTES` either.
const MAX_PAYLOAD: usize = 256 * 1024 * 1024;

/// The Paste database for this user, if the app's data is present.
pub fn default_paste_db() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    PASTE_DBS
        .iter()
        .map(|rel| Path::new(&home).join(rel))
        .find(|p| p.exists())
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

    // Core Data keeps large payloads in files next to the store.
    let external = db
        .parent()
        .unwrap_or(Path::new("."))
        .join(".db_SUPPORT/_EXTERNAL_DATA");
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

        let Some(snap) = parse_pasteboard(&blob, &external) else {
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
/// The column is Core Data's external-storage format: a one-byte prefix, then either the data
/// inline (0x01) or the name of a file in `external` holding it (0x02, a UUID). The data is a
/// binary plist (raw or LZFSE, magic `bvx`) or raw-DEFLATE-compressed plist or JSON with base64
/// values, all shaped `[{ types, dataByType: { uti: <bytes> } }]`.
fn parse_pasteboard(blob: &[u8], external: &Path) -> Option<Snapshot> {
    let stored;
    let body = match blob.split_first()? {
        (1, inline) => inline,
        (2, name) => {
            stored = read_external(external, name)?;
            &stored[..]
        }
        _ => return None,
    };
    let data_by_type = if body.starts_with(b"bplist00") {
        plist_data_by_type(body)?
    } else if body.starts_with(b"bvx") {
        plist_data_by_type(&crate::macos::lzfse_decode(body, MAX_PAYLOAD)?)?
    } else {
        let decoded = crate::macos::zlib_decode(body, MAX_PAYLOAD)?;
        if decoded.starts_with(b"bplist00") {
            plist_data_by_type(&decoded)?
        } else {
            json_data_by_type(&decoded)?
        }
    };
    Some(snapshot_from(data_by_type))
}

/// The bytes of an externally stored payload. Its name comes from the store, so only a UUID is
/// accepted as a file name, and the file is size-capped.
fn read_external(dir: &Path, name: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let name = std::str::from_utf8(name).ok()?.trim_end_matches('\0');
    if !is_uuid(name) {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(dir.join(name))
        .ok()?
        .take(MAX_PAYLOAD as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_PAYLOAD).then_some(bytes)
}

/// `8-4-4-4-12` hex digits, as Core Data names its external files.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A Paste payload as a binary plist: `[{ dataByType: { uti: bytes } }]`.
    fn payload(uti: &str, bytes: &[u8]) -> Vec<u8> {
        let mut data = plist::Dictionary::new();
        data.insert(uti.into(), plist::Value::Data(bytes.to_vec()));
        let mut item = plist::Dictionary::new();
        item.insert("dataByType".into(), plist::Value::Dictionary(data));
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &vec![plist::Value::Dictionary(item)]).unwrap();
        out
    }

    #[test]
    fn inline_and_external_payloads_both_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut inline = vec![1u8];
        inline.extend(payload(UTI_UTF8, b"inline text"));
        let snap = parse_pasteboard(&inline, dir.path()).unwrap();
        assert_eq!(snap.text.as_deref(), Some("inline text"));

        let name = "9BDC0A4E-A5E5-4C36-A922-8DAAB77EE3B3";
        std::fs::write(dir.path().join(name), payload(UTI_UTF8, b"stored outside")).unwrap();
        let mut pointer = vec![2u8];
        pointer.extend(name.as_bytes());
        pointer.push(0);
        let snap = parse_pasteboard(&pointer, dir.path()).unwrap();
        assert_eq!(snap.text.as_deref(), Some("stored outside"));

        let mut missing = vec![2u8];
        missing.extend(b"00000000-0000-0000-0000-000000000000");
        assert!(parse_pasteboard(&missing, dir.path()).is_none());
        assert!(parse_pasteboard(&[3, 1, 2], dir.path()).is_none());
        assert!(parse_pasteboard(&[], dir.path()).is_none());
    }

    #[test]
    fn an_external_name_must_be_a_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("external");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(tmp.path().join("secret"), payload(UTI_UTF8, b"outside")).unwrap();
        for name in [
            "../secret",
            "/etc/hosts",
            "9BDC0A4E-A5E5-4C36-A922-8DAAB77EE3B",
            "9BDC0A4E_A5E5-4C36-A922-8DAAB77EE3B3",
        ] {
            let mut pointer = vec![2u8];
            pointer.extend(name.as_bytes());
            assert!(parse_pasteboard(&pointer, &dir).is_none(), "{name}");
        }
        assert!(is_uuid("9bdc0a4e-a5e5-4c36-a922-8daab77ee3b3"));
    }

    #[test]
    fn decoding_stops_at_the_size_ceiling() {
        // 300 KB of zeros compresses to almost nothing; a 64 KB ceiling refuses it.
        let zeros = vec![0u8; 300 * 1024];
        let mut packed = Vec::new();
        crate::macos::lzfse_stream(&mut zeros.as_slice(), &mut packed, true).unwrap();
        assert_eq!(crate::macos::lzfse_decode(&packed, 1 << 20).unwrap(), zeros);
        assert!(crate::macos::lzfse_decode(&packed, 64 * 1024).is_none());
    }
}
