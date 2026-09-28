//! `pasteloop` CLI subcommands over the same store the app uses (SQLite in WAL mode; the app
//! notices external writes through `data_version`).

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};

use crate::settings;
use crate::store::{Kind, NewItem, PAGE_SIZE, SearchQuery, Store};

pub const USAGE: &str = "\
pasteloop — clipboard manager

Usage:
  pasteloop                      run the app (menu bar)
  pasteloop search <query> [--kind text|link|image|file|color] [--app <bundle id>] [--limit n]
  pasteloop get <id>             print one item as JSON (full text, blob paths)
  pasteloop recent [n]           print the newest n items (default 20)
  pasteloop add --text <text> | --file <path>
  pasteloop export --json <path> write history and collections as JSON
  pasteloop stats                item, collection, and blob counts
  pasteloop ocr <image>          print the text Vision recognizes in an image
  pasteloop seed [n]             insert n synthetic text items (performance checks)
  pasteloop import-paste [--db <path>] [--limit n] [--dry-run]
                              migrate text, link, and color history from the Paste app";

fn open_store() -> Result<Store> {
    Store::open(&settings::data_dir()).context("open store")
}

/// The compact JSON shape for lists: enough to pick an item, cheap to read.
fn summary(item: &crate::store::Item) -> serde_json::Value {
    let text = item.text.as_deref().unwrap_or("");
    serde_json::json!({
        "id": item.id,
        "kind": item.kind.as_str(),
        "title": item.display_title(),
        "text": crate::store::truncate_chars(text, 200),
        "app": item.source_app_name,
        "created_at": item.created_at,
        "collection_ids": item.collection_ids,
        "files": item.files,
    })
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

pub fn run_cli(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = &args[1..];
    match sub {
        "search" => {
            let mut query = SearchQuery {
                terms: rest
                    .iter()
                    .take_while(|a| !a.starts_with("--"))
                    .flat_map(|a| a.split_whitespace())
                    .map(str::to_string)
                    .collect(),
                ..Default::default()
            };
            if let Some(kind) = flag(rest, "--kind") {
                query
                    .kinds
                    .push(Kind::parse(kind).ok_or_else(|| anyhow!("unknown kind {kind}"))?);
            }
            if let Some(app) = flag(rest, "--app") {
                query.apps.push(app.to_string());
            }
            let limit: usize = flag(rest, "--limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(50);
            let store = open_store()?;
            let items = store.search(&query, 0, limit.min(PAGE_SIZE))?;
            let out: Vec<_> = items.iter().map(summary).collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        "get" => {
            let id: i64 = rest
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow!("usage: pasteloop get <id>"))?;
            let store = open_store()?;
            let item = store.get(id)?.ok_or_else(|| anyhow!("no item {id}"))?;
            let mut value = serde_json::to_value(&item)?;
            if let Some(full) = store.full_text(&item)? {
                value["text"] = serde_json::Value::String(full);
            }
            let blob_paths: Vec<String> =
                [&item.image_blob, &item.rich_blob, &item.link_image_blob]
                    .into_iter()
                    .flatten()
                    .map(|blob| store.blob_path(blob).display().to_string())
                    .collect();
            if !blob_paths.is_empty() {
                value["blob_paths"] = serde_json::json!(blob_paths);
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        "recent" => {
            let n: usize = rest.first().and_then(|s| s.parse().ok()).unwrap_or(20);
            let store = open_store()?;
            let items = store.history(None, n.min(PAGE_SIZE))?;
            let out: Vec<_> = items.iter().map(summary).collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        "add" => {
            let store = open_store()?;
            let new = if let Some(text) = flag(rest, "--text") {
                NewItem::text(text)
            } else if let Some(path) = flag(rest, "--file") {
                let abs = std::fs::canonicalize(path).with_context(|| format!("resolve {path}"))?;
                NewItem {
                    kind: Some(Kind::File),
                    files: vec![abs.display().to_string()],
                    ..Default::default()
                }
            } else {
                return Err(anyhow!(
                    "usage: pasteloop add --text <text> | --file <path>"
                ));
            };
            let (item, was_new) = store.insert(&NewItem {
                source_app_name: Some("pasteloop CLI".into()),
                ..new
            })?;
            println!("{} id={} new={}", item.kind.as_str(), item.id, was_new);
        }
        "export" => {
            let path: PathBuf = flag(rest, "--json")
                .map(PathBuf::from)
                .ok_or_else(|| anyhow!("usage: pasteloop export --json <path>"))?;
            let store = open_store()?;
            let json = store.export_json()?;
            std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
            println!("wrote {}", path.display());
        }
        "stats" => {
            let store = open_store()?;
            println!("{}", serde_json::to_string_pretty(&store.stats()?)?);
        }
        "ocr" => {
            let path = rest
                .first()
                .ok_or_else(|| anyhow!("usage: pasteloop ocr <image>"))?;
            let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
            match crate::macos::recognize_text(&bytes) {
                Some(text) => println!("{text}"),
                None => println!("(no text recognized)"),
            }
        }
        "seed" => {
            // Development helper for the performance checks: n synthetic text items.
            let n: usize = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10_000);
            let store = open_store()?;
            let words = [
                "alpha",
                "bravo",
                "charlie",
                "delta",
                "echo",
                "foxtrot",
                "golf",
                "hotel",
                "rust",
                "clipboard",
                "keyboard",
                "search",
            ];
            store.write(|conn| {
                let tx = conn.transaction()?;
                let now = crate::store::now_ms();
                for i in 0..n {
                    let text: Vec<&str> = (0..6).map(|k| words[(i * 7 + k * 3) % words.len()]).collect();
                    let text = format!("{} {i}", text.join(" "));
                    // Oldest first, so rowid order matches creation order like real captures.
                    tx.execute(
                        "INSERT INTO items (uuid, kind, created_at, updated_at, source_bundle_id, \
                         source_app_name, text, size_bytes, content_hash, device_id) \
                         VALUES (?1, 'text', ?2, ?2, 'com.apple.TextEdit', 'TextEdit', ?3, ?4, ?5, 'seed')",
                        rusqlite::params![
                            format!("seed-{i}-{now}"),
                            now - ((n - i) as i64) * 60_000,
                            text,
                            text.len() as i64,
                            format!("seed{now:x}{i:048x}"),
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })?;
            println!("seeded {n} items");
        }
        "import-paste" => {
            let db = flag(rest, "--db")
                .map(PathBuf::from)
                .or_else(crate::import::default_paste_db)
                .ok_or_else(|| {
                    anyhow!("Paste database not found; pass --db <path to db.sqlite>")
                })?;
            let limit = flag(rest, "--limit").and_then(|n| n.parse().ok());
            let dry_run = rest.iter().any(|a| a == "--dry-run");
            let store = open_store()?;
            eprintln!("importing from {}…", db.display());
            let stats = crate::import::import_from_paste(&store, &db, limit, dry_run)?;
            println!(
                "{} {} item(s), skipped {}, created {} collection(s)",
                if dry_run { "would import" } else { "imported" },
                stats.imported,
                stats.skipped,
                stats.collections_created
            );
        }
        _ => return Err(anyhow!("{USAGE}")),
    }
    Ok(())
}
