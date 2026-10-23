//! Automatic backup of the library to iCloud Drive, and restore from it on a new Mac or after
//! reinstalling macOS.
//!
//! Each library backs up into its own folder under `iCloud Drive/PasteLoop/`:
//! - `history.sqlite.lzfse`: the database without deleted items and with an empty search index
//!   (a restore rebuilds it), compressed with LZFSE. Built locally and moved in, so iCloud only
//!   ever sees a complete file.
//! - `blobs/`: the content-addressed blob files. They never change once written, so iCloud
//!   uploads each one once; files no snapshot uses any more are pruned after a backup.
//! - `settings.json`, and `manifest.json` describing the backup.
//!
//! The live database never lives in iCloud Drive: syncing a file SQLite is writing corrupts it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::macos;
use crate::store::{MergeReport, Store, now_ms};

const SNAPSHOT: &str = "history.sqlite.lzfse";
const MANIFEST: &str = "manifest.json";
const SETTINGS: &str = "settings.json";
const FORMAT: u32 = 1;

/// Back up once nothing has changed for this long.
const QUIET: Duration = Duration::from_secs(30 * 60);
/// After a deletion, sooner, so deleted items leave iCloud soon.
const QUIET_AFTER_DELETE: Duration = Duration::from_secs(5 * 60);
/// While changes keep coming, at least this often.
const MAX_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Held while a backup or restore runs; a quit-time backup waits for a scheduled one.
static BUSY: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A library younger than this restores a much bigger backup it finds (a new Mac, a reinstall);
/// an older one never does, so two Macs in use never fold their histories together.
const RESTORE_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// The id of the library this backup belongs to.
    pub library: String,
    pub device: String,
    pub app_version: String,
    pub items: i64,
    pub collections: i64,
    pub blobs: usize,
    pub created_at: i64,
}

#[derive(Debug)]
pub struct BackupReport {
    pub manifest: Manifest,
    pub folder: PathBuf,
    pub copied_blobs: usize,
    pub pruned_blobs: usize,
    pub snapshot_bytes: u64,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct RestoreReport {
    pub merged: MergeReport,
    pub from: Manifest,
    pub settings: Option<crate::settings::Settings>,
    pub missing_blobs: usize,
}

/// Where backups go: `PASTELOOP_BACKUP_DIR`, else `iCloud Drive/PasteLoop`. `None` for an
/// instance on another data directory (development, tests), which must not touch the real one.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PASTELOOP_BACKUP_DIR") {
        return Some(PathBuf::from(dir));
    }
    if std::env::var_os("PASTELOOP_DATA_DIR").is_some() {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/Mobile Documents/com~apple~CloudDocs/PasteLoop"))
}

/// Whether backups can be written under `root`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Ready,
    /// The folder holding `root` does not exist: iCloud Drive is turned off.
    Off,
    /// macOS refused access; the user has to allow it in System Settings.
    Denied,
}

/// Creates `root` if needed. The first write into iCloud Drive is what makes macOS ask the user
/// for access, so this tries the write instead of only looking. The folder holding `root` is
/// never created: when iCloud Drive is off, a stand-in folder there would sync nowhere.
pub fn access(root: &Path) -> Access {
    use std::io::ErrorKind;
    let classify = |err: std::io::Error| match err.kind() {
        ErrorKind::PermissionDenied => Access::Denied,
        _ => Access::Off,
    };
    if let Some(parent) = root.parent()
        && let Err(err) = fs::metadata(parent)
    {
        return classify(err);
    }
    match fs::create_dir(root) {
        Ok(()) => Access::Ready,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => match fs::read_dir(root) {
            Ok(_) => Access::Ready,
            Err(err) => classify(err),
        },
        Err(err) => classify(err),
    }
}

/// Whether a backup is due. `since_change` is the time since the latest change not yet backed
/// up (`None` when there is none); `deleted` says whether the changes removed items. A library
/// that was never backed up is backed up at once.
pub fn due(since_backup: Option<Duration>, since_change: Option<Duration>, deleted: bool) -> bool {
    let Some(quiet) = since_change else {
        return false;
    };
    let wait = if deleted { QUIET_AFTER_DELETE } else { QUIET };
    since_backup.is_none_or(|d| d >= MAX_INTERVAL) || quiet >= wait
}

/// Writes a backup of `store` into its folder under `root`, creating the folder on first use.
pub fn back_up(store: &Store, settings_path: &Path, root: &Path) -> Result<BackupReport> {
    let _busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let folder = own_folder(store, root)?;
    fs::create_dir_all(folder.join("blobs"))
        .with_context(|| format!("create {}", folder.display()))?;
    let work = work_dir(store)?;

    let db = work.join("snapshot.sqlite");
    let info = store.snapshot_into(&db)?;
    let packed = work.join(SNAPSHOT);
    {
        let mut input = fs::File::open(&db)?;
        let mut output = std::io::BufWriter::new(fs::File::create(&packed)?);
        macos::lzfse_stream(&mut input, &mut output, true)?;
        std::io::Write::flush(&mut output)?;
    }
    let snapshot_bytes = fs::metadata(&packed)?.len();
    let _ = fs::remove_file(&db);

    // Blobs before the snapshot that refers to them; the snapshot replaces the old one in one
    // rename (the work folder is on the same volume as iCloud Drive).
    let in_backup = list_blobs(&folder.join("blobs"))?;
    let mut copied = 0;
    for id in info.blob_ids.iter().filter(|id| !in_backup.contains(*id)) {
        let from = store.blob_path(id);
        if !from.exists() {
            tracing::warn!("backup: blob {id} is missing locally; skipped");
            continue;
        }
        copy_atomic(&from, &blob_path(&folder, id))?;
        copied += 1;
    }
    fs::rename(&packed, folder.join(SNAPSHOT))
        .or_else(|_| fs::copy(&packed, folder.join(SNAPSHOT)).map(drop))?;
    let _ = fs::remove_file(&packed);
    if settings_path.exists() {
        copy_atomic(settings_path, &folder.join(SETTINGS))?;
    }
    let manifest = Manifest {
        format: FORMAT,
        library: store.library_id().to_string(),
        device: macos::host_name(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        items: info.items,
        collections: info.collections,
        blobs: info.blob_ids.len(),
        created_at: now_ms(),
    };
    crate::store::write_atomic(
        &folder.join(MANIFEST),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;

    let keep: HashSet<&str> = info.blob_ids.iter().map(String::as_str).collect();
    let mut pruned = 0;
    for id in in_backup.iter().filter(|id| !keep.contains(id.as_str())) {
        if fs::remove_file(blob_path(&folder, id)).is_ok() {
            pruned += 1;
        }
    }
    Ok(BackupReport {
        manifest,
        folder,
        copied_blobs: copied,
        pruned_blobs: pruned,
        snapshot_bytes,
        elapsed: started.elapsed(),
    })
}

/// The newest backup under `root` from another library, when this library is new and much
/// smaller than it: what a new Mac or a fresh macOS install should restore.
pub fn restore_candidate(store: &Store, root: &Path) -> Result<Option<(PathBuf, Manifest)>> {
    if store.meta("backup_restored_from")?.is_some() {
        return Ok(None);
    }
    let created = store.library_created_ms().unwrap_or(0);
    if now_ms() - created > RESTORE_WINDOW_MS {
        return Ok(None);
    }
    let live = store.count_items()?;
    let newest = backups(root)?
        .into_iter()
        .filter(|(_, m)| m.library != store.library_id() && m.items > live.max(1) * 2)
        .max_by_key(|(_, m)| m.created_at);
    Ok(newest)
}

/// Every readable backup under `root`.
pub fn backups(root: &Path) -> Result<Vec<(PathBuf, Manifest)>> {
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let folder = entry.path();
        let Ok(bytes) = read_capped(&folder.join(MANIFEST), MAX_MANIFEST) else {
            continue;
        };
        match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(m) if m.format == FORMAT => found.push((folder, m)),
            Ok(m) => tracing::warn!("backup {} has format {}", folder.display(), m.format),
            Err(err) => tracing::warn!("backup {}: bad manifest: {err}", folder.display()),
        }
    }
    Ok(found)
}

/// Merges the backup in `folder` into `store`, copies its blob files, and adopts the folder so
/// later backups of this library continue there. Settings are returned for the caller to apply.
pub fn restore(
    store: &Store,
    root: &Path,
    folder: &Path,
    manifest: &Manifest,
) -> Result<RestoreReport> {
    let _busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    macos::start_icloud_download(folder);
    let work = work_dir(store)?;
    let db = work.join("restore.sqlite");
    {
        let mut input = std::io::BufReader::new(
            fs::File::open(folder.join(SNAPSHOT)).context("open the backup's snapshot")?,
        );
        let mut output = Capped {
            inner: std::io::BufWriter::new(fs::File::create(&db)?),
            left: MAX_SNAPSHOT,
        };
        let unpacked = macos::lzfse_stream(&mut input, &mut output, false)
            .and_then(|()| std::io::Write::flush(&mut output));
        if let Err(err) = unpacked {
            let _ = fs::remove_file(&db);
            return Err(err).context("unpack the backup's snapshot");
        }
    }
    let merged = store.merge_snapshot(&db);
    let _ = fs::remove_file(&db);
    let merged = merged?;

    let name = folder
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("backup folder has no name"))?;
    if let Some(own) = store.meta("backup_folder")?
        && own != name
    {
        // This library's own backup is a subset of the merged one now.
        let _ = fs::remove_dir_all(root.join(own));
    }
    store.set_meta("backup_folder", name)?;
    store.set_meta("backup_restored_from", &manifest.library)?;

    let settings = read_capped(&folder.join(SETTINGS), MAX_MANIFEST)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let missing_blobs = fetch_missing_blobs(store, folder)?;
    Ok(RestoreReport {
        merged,
        from: manifest.clone(),
        settings,
        missing_blobs,
    })
}

/// Copies blob files this library uses but lacks from the backup in `folder`; returns how many
/// are still missing (not yet downloaded, or gone).
pub fn fetch_missing_blobs(store: &Store, folder: &Path) -> Result<usize> {
    let missing = store.missing_blob_files()?;
    let mut still = 0;
    for id in &missing {
        let from = blob_path(folder, id);
        if copy_atomic(&from, &store.blob_path(id)).is_err() {
            still += 1;
        }
    }
    Ok(still)
}

/// This library's backup folder under `root`: the one it has used before, or a new one named
/// after this Mac.
pub fn own_folder(store: &Store, root: &Path) -> Result<PathBuf> {
    if let Some(name) = store.meta("backup_folder")? {
        return Ok(root.join(name));
    }
    let id = store.library_id();
    let short = &id[id.len().saturating_sub(8)..];
    let device: String = macos::host_name()
        .chars()
        .map(|c| if matches!(c, '/' | ':') { '-' } else { c })
        .collect();
    let name = format!("{device} ({short})");
    store.set_meta("backup_folder", &name)?;
    Ok(root.join(name))
}

/// A folder next to the database for building and unpacking snapshots: local disk, and the same
/// volume as iCloud Drive, so a finished snapshot moves into the backup with a rename.
fn work_dir(store: &Store) -> Result<PathBuf> {
    let dir = store.dir().join("backup-work");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A blob's file in a backup folder; ids that are not hashes never become paths (see
/// `store::is_hash`).
fn blob_path(folder: &Path, id: &str) -> PathBuf {
    if !crate::store::is_hash(id) {
        return folder.join("blobs").join("invalid");
    }
    folder.join("blobs").join(&id[..2]).join(id)
}

fn list_blobs(dir: &Path) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    let Ok(prefixes) = fs::read_dir(dir) else {
        return Ok(ids);
    };
    for prefix in prefixes.flatten() {
        if let Ok(files) = fs::read_dir(prefix.path()) {
            for file in files.flatten() {
                if let Some(name) = file.file_name().to_str()
                    && crate::store::is_hash(name)
                {
                    ids.insert(name.to_string());
                }
            }
        }
    }
    Ok(ids)
}

/// Copies through a hidden temp file next to `to` and a rename, so a reader (or iCloud) never
/// sees a partial file.
fn copy_atomic(from: &Path, to: &Path) -> Result<()> {
    let parent = to
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", to.display()))?;
    fs::create_dir_all(parent)?;
    let name = to.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(".{name}.tmp"));
    fs::copy(from, &tmp).with_context(|| format!("copy {}", from.display()))?;
    fs::rename(&tmp, to)?;
    Ok(())
}

/// The largest unpacked snapshot a restore accepts; a 110k-item library is 120 MB.
const MAX_SNAPSHOT: u64 = 4 << 30;
/// The largest manifest or settings file read from a backup.
const MAX_MANIFEST: u64 = 1 << 20;

/// Reads a small file from a backup, refusing one larger than `max` bytes.
fn read_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(max + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::other("file too large"));
    }
    Ok(bytes)
}

/// A writer that fails once more than `left` bytes went through it: a crafted snapshot cannot
/// unpack into a disk-filling file.
struct Capped<W> {
    inner: W,
    left: u64,
}

impl<W: std::io::Write> std::io::Write for Capped<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() as u64 > self.left {
            return Err(std::io::Error::other(
                "the backup's snapshot unpacks too large",
            ));
        }
        let n = self.inner.write(buf)?;
        self.left -= n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Runs `job` on its own low-priority thread that may read iCloud placeholders (downloading
/// them), and hands back its result.
pub fn run_worker<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> futures::channel::oneshot::Receiver<T> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("pasteloop-backup".into())
        .spawn(move || {
            macos::become_background_file_worker();
            let _ = tx.send(job());
        });
    if let Err(err) = spawned {
        tracing::warn!("backup thread failed to start: {err}");
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Kind, NewItem, SearchQuery};

    fn store_in(dir: &Path) -> Store {
        Store::open(dir).unwrap()
    }

    fn texts(store: &Store) -> Vec<String> {
        store
            .history(None, 1000)
            .unwrap()
            .into_iter()
            .filter_map(|i| i.text)
            .collect()
    }

    fn image(seed: u8) -> NewItem {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([seed, 0, 0, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        NewItem {
            kind: Some(Kind::Image),
            image: Some(crate::store::NewImage {
                png_or_original: png,
                mime: "image/png",
                width: 2,
                height: 2,
                truncated: false,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn access_creates_the_folder_but_never_a_missing_icloud_drive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("PasteLoop");
        assert_eq!(access(&root), Access::Ready);
        assert!(root.is_dir());
        assert_eq!(access(&root), Access::Ready);
        let off = tmp.path().join("no-icloud/PasteLoop");
        assert_eq!(access(&off), Access::Off);
        assert!(!tmp.path().join("no-icloud").exists());
    }

    #[test]
    fn backup_is_due_after_quiet_sooner_after_deletes_and_at_most_six_hours_apart() {
        let min = |m: u64| Some(Duration::from_secs(m * 60));
        assert!(!due(min(10), None, false), "nothing changed");
        assert!(!due(min(60), min(10), false));
        assert!(due(min(60), min(30), false));
        assert!(due(min(60), min(5), true));
        assert!(due(min(6 * 60), min(1), false), "busy for six hours");
        assert!(due(None, min(0), false), "never backed up: at once");
    }

    #[test]
    fn backup_then_restore_on_a_new_mac_brings_everything_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("icloud");
        let old = store_in(&tmp.path().join("old"));
        for text in ["alpha one", "bravo two", "charlie three"] {
            old.insert(&NewItem::text(text)).unwrap();
        }
        let (pic, _) = old.insert(&image(7)).unwrap();
        let (gone, _) = old.insert(&NewItem::text("deleted secret")).unwrap();
        old.delete(&[gone.id]).unwrap();
        let work = old.create_collection("Work", "blue").unwrap();
        let (kept, _) = old.insert(&NewItem::text("in a collection")).unwrap();
        old.add_to_collection(kept.id, work.id).unwrap();
        let settings = tmp.path().join("settings.json");
        fs::write(&settings, r#"{"card_width": 300}"#).unwrap();

        let report = back_up(&old, &settings, &root).unwrap();
        assert_eq!(
            report.manifest.items, 5,
            "the deleted item is not backed up"
        );
        assert_eq!(report.copied_blobs, 1);

        // A new Mac copies one thing before iCloud Drive catches up, then finds the backup.
        let new = store_in(&tmp.path().join("new"));
        new.insert(&NewItem::text("first copy on the new mac"))
            .unwrap();
        let (folder, manifest) = restore_candidate(&new, &root).unwrap().unwrap();
        let restored = restore(&new, &root, &folder, &manifest).unwrap();
        assert_eq!(restored.merged.items, 5);
        assert_eq!(restored.merged.collections, 1);
        assert_eq!(restored.missing_blobs, 0);
        assert_eq!(restored.settings.unwrap().card_width, 300.0);

        assert_eq!(
            texts(&new),
            vec![
                "first copy on the new mac",
                "in a collection",
                "charlie three",
                "bravo two",
                "alpha one"
            ]
        );
        let find = |term: &str| {
            let query = SearchQuery {
                terms: vec![term.into()],
                ..Default::default()
            };
            new.search(&query, 0, 10).unwrap().len()
        };
        assert_eq!(find("bravo"), 1, "the search index is rebuilt");
        assert_eq!(find("first copy"), 1);
        assert_eq!(find("secret"), 0);
        let image = new
            .history(None, 10)
            .unwrap()
            .into_iter()
            .find(|i| i.uuid == pic.uuid)
            .unwrap();
        assert!(new.blob_path(image.image_blob.as_deref().unwrap()).exists());
        let collection = new.collections().unwrap();
        assert_eq!(collection.len(), 1);
        assert_eq!(new.count_collection_items(collection[0].id).unwrap(), 1);

        // The new Mac continues the same backup folder and does not restore twice.
        assert_eq!(own_folder(&new, &root).unwrap(), folder);
        assert!(restore_candidate(&new, &root).unwrap().is_none());
    }

    #[test]
    fn restoring_twice_or_onto_the_same_items_adds_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("icloud");
        let old = store_in(&tmp.path().join("old"));
        for i in 0..4 {
            old.insert(&NewItem::text(format!("note {i}"))).unwrap();
        }
        back_up(&old, &tmp.path().join("none.json"), &root).unwrap();
        let (folder, manifest) = backups(&root).unwrap().remove(0);

        let new = store_in(&tmp.path().join("new"));
        new.insert(&NewItem::text("note 2")).unwrap(); // the same content, copied again
        let first = restore(&new, &root, &folder, &manifest).unwrap();
        assert_eq!(first.merged.items, 3);
        let second = restore(&new, &root, &folder, &manifest).unwrap();
        assert_eq!(second.merged.items, 0);
        assert_eq!(new.count_items().unwrap(), 4);
    }

    #[test]
    fn a_later_backup_prunes_blobs_of_deleted_items() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("icloud");
        let store = store_in(&tmp.path().join("lib"));
        let (a, _) = store.insert(&image(1)).unwrap();
        store.insert(&image(2)).unwrap();
        let none = tmp.path().join("none.json");
        let first = back_up(&store, &none, &root).unwrap();
        assert_eq!((first.copied_blobs, first.pruned_blobs), (2, 0));

        store.delete(&[a.id]).unwrap();
        let second = back_up(&store, &none, &root).unwrap();
        assert_eq!((second.copied_blobs, second.pruned_blobs), (0, 1));
        assert_eq!(list_blobs(&first.folder.join("blobs")).unwrap().len(), 1);
    }

    #[test]
    fn a_library_does_not_restore_its_own_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("icloud");
        let store = store_in(&tmp.path().join("lib"));
        for i in 0..3 {
            store.insert(&NewItem::text(format!("x{i}"))).unwrap();
        }
        back_up(&store, &tmp.path().join("none.json"), &root).unwrap();
        assert!(restore_candidate(&store, &root).unwrap().is_none());
    }

    #[test]
    fn a_library_in_use_for_weeks_never_restores_another_macs_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("icloud");
        let other = store_in(&tmp.path().join("other"));
        for i in 0..5 {
            other.insert(&NewItem::text(format!("y{i}"))).unwrap();
        }
        back_up(&other, &tmp.path().join("none.json"), &root).unwrap();

        let dir = tmp.path().join("old");
        let a_month_ago = (now_ms() / 1000 - 30 * 24 * 3600) as u64;
        let old_id =
            uuid::Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, a_month_ago, 0));
        store_in(&dir)
            .set_meta("device_id", &old_id.to_string())
            .unwrap();
        let old = store_in(&dir);
        assert!(restore_candidate(&old, &root).unwrap().is_none());

        let fresh = store_in(&tmp.path().join("fresh"));
        assert!(restore_candidate(&fresh, &root).unwrap().is_some());
    }

    /// Rewrites a backup's snapshot with `sql`, the way someone with write access to iCloud
    /// Drive could.
    fn tamper(folder: &Path, sql: &str) {
        let packed = fs::read(folder.join(SNAPSHOT)).unwrap();
        let mut db = Vec::new();
        macos::lzfse_stream(&mut packed.as_slice(), &mut db, false).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sqlite");
        fs::write(&path, db).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let mut repacked = Vec::new();
        macos::lzfse_stream(
            &mut fs::read(&path).unwrap().as_slice(),
            &mut repacked,
            true,
        )
        .unwrap();
        fs::write(folder.join(SNAPSHOT), repacked).unwrap();
    }

    /// A backup of a few items and one image, plus a fresh library to restore it into.
    fn hostile_setup(tmp: &Path) -> (PathBuf, PathBuf, Manifest, Store) {
        let root = tmp.join("icloud");
        let old = store_in(&tmp.join("old"));
        old.insert(&NewItem::text("hello")).unwrap();
        old.insert(&image(3)).unwrap();
        back_up(&old, &tmp.join("none.json"), &root).unwrap();
        let (folder, manifest) = backups(&root).unwrap().remove(0);
        (root, folder, manifest, store_in(&tmp.join("new")))
    }

    #[test]
    fn a_backup_naming_a_path_instead_of_a_blob_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, folder, manifest, new) = hostile_setup(tmp.path());
        // The blob id `../escaped` reads `<iCloud folder>/escaped`, which anyone who can write to
        // iCloud Drive controls, and writes `<library>/../escaped`, outside the library. One
        // more `../` on a real Mac reaches `~/Library/LaunchAgents`.
        let escape = "../escaped";
        fs::write(root.join("escaped"), "payload").unwrap();
        tamper(
            &folder,
            &format!(
                "UPDATE blobs SET id = '{escape}'; UPDATE items SET image_blob = '{escape}' \
                 WHERE image_blob IS NOT NULL;"
            ),
        );
        let err = restore(&new, &root, &folder, &manifest).unwrap_err();
        assert!(format!("{err:#}").contains("not a hash"), "{err:#}");
        assert_eq!(new.count_items().unwrap(), 0, "nothing is merged");
        assert!(
            !tmp.path().join("escaped").exists(),
            "the payload stayed in iCloud Drive"
        );
    }

    #[test]
    fn a_backup_with_a_crafted_content_hash_is_refused_without_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, folder, manifest, new) = hostile_setup(tmp.path());
        // Not ASCII: slicing the first two bytes of this as a name would panic.
        tamper(
            &folder,
            "UPDATE items SET content_hash = 'éé' || substr(content_hash, 5);",
        );
        assert!(restore(&new, &root, &folder, &manifest).is_err());
        assert_eq!(new.count_items().unwrap(), 0);
    }

    #[test]
    fn a_backup_whose_table_is_a_view_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, folder, manifest, new) = hostile_setup(tmp.path());
        tamper(
            &folder,
            "ALTER TABLE apps RENAME TO apps_real; CREATE VIEW apps AS SELECT * FROM apps_real;",
        );
        let err = restore(&new, &root, &folder, &manifest).unwrap_err();
        assert!(format!("{err:#}").contains("not a table"), "{err:#}");
    }

    #[test]
    fn a_snapshot_that_unpacks_too_large_is_cut_off() {
        let mut out = Capped {
            inner: Vec::new(),
            left: 10,
        };
        assert!(std::io::Write::write_all(&mut out, b"12345").is_ok());
        assert!(std::io::Write::write_all(&mut out, b"123456").is_err());
        assert_eq!(out.inner, b"12345");
    }

    #[test]
    fn lzfse_round_trip() {
        let data: Vec<u8> = (0..300_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        let mut packed = Vec::new();
        macos::lzfse_stream(&mut data.as_slice(), &mut packed, true).unwrap();
        assert!(packed.len() < data.len() / 4);
        let mut unpacked = Vec::new();
        macos::lzfse_stream(&mut packed.as_slice(), &mut unpacked, false).unwrap();
        assert_eq!(unpacked, data);
        let mut truncated = Vec::new();
        assert!(
            macos::lzfse_stream(&mut &packed[..packed.len() / 2], &mut truncated, false).is_err()
        );
    }
}
