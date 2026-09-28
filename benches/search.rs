//! Search latency on a synthetic 100k-item store: queries of 1, 3, and 8 characters, with
//! and without filters. Target: under 10 ms per query (the search budget in CONTRIBUTING.md).

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use pasteloop::store::{Kind, SearchQuery, Store};
use rusqlite::params;

const ITEMS: usize = 100_000;

fn synthetic_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("open");
    let words = [
        "alpha",
        "bravo",
        "charlie",
        "delta",
        "echo",
        "foxtrot",
        "golf",
        "hotel",
        "india",
        "juliet",
        "kilo",
        "lima",
        "mike",
        "november",
        "oscar",
        "papa",
        "quebec",
        "romeo",
        "sierra",
        "tango",
        "uniform",
        "victor",
        "whiskey",
        "xray",
        "yankee",
        "zulu",
        "rust",
        "clipboard",
        "manager",
        "collection",
        "metal",
        "renderer",
        "keyboard",
        "search",
    ];
    let apps = [
        ("com.apple.Safari", "Safari"),
        ("com.apple.TextEdit", "TextEdit"),
        ("com.googlecode.iterm2", "iTerm2"),
        ("com.microsoft.VSCode", "Code"),
        ("com.apple.finder", "Finder"),
    ];
    store
        .write(|conn| {
            let tx = conn.transaction()?;
            let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut next = || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed
            };
            for i in 0..ITEMS {
                let n = 4 + (next() % 12) as usize;
                let text: Vec<&str> = (0..n)
                    .map(|_| words[(next() % words.len() as u64) as usize])
                    .collect();
                let text = format!("{} {}", text.join(" "), i);
                let ocr = if i % 5 == 0 {
                    Some(format!("screenshot text {} {}", words[i % words.len()], i))
                } else {
                    None
                };
                let (bundle, name) = apps[i % apps.len()];
                let kind = if i % 7 == 0 { Kind::Link } else { Kind::Text };
                let created = 1_700_000_000_000i64 + (i as i64) * 1_000;
                tx.execute(
                    "INSERT INTO items (uuid, kind, created_at, updated_at, source_bundle_id, \
                     source_app_name, text, size_bytes, content_hash, ocr_text, device_id) \
                     VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'bench')",
                    params![
                        format!("bench-{i}"),
                        kind.as_str(),
                        created,
                        bundle,
                        name,
                        text,
                        text.len() as i64,
                        format!("{i:064x}"),
                        ocr,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .expect("seed");
    (dir, store)
}

fn bench_search(c: &mut Criterion) {
    let (_dir, store) = synthetic_store();
    let mut group = c.benchmark_group("search_100k");
    group.measurement_time(Duration::from_secs(5));
    let cases: Vec<(&str, SearchQuery)> = vec![
        (
            "1 char",
            SearchQuery {
                terms: vec!["r".into()],
                ..Default::default()
            },
        ),
        (
            "3 chars",
            SearchQuery {
                terms: vec!["rus".into()],
                ..Default::default()
            },
        ),
        (
            "8 chars",
            SearchQuery {
                terms: vec!["renderer".into()],
                ..Default::default()
            },
        ),
        (
            "3 chars + kind",
            SearchQuery {
                terms: vec!["rus".into()],
                kinds: vec![Kind::Link],
                ..Default::default()
            },
        ),
        (
            "8 chars + app + date",
            SearchQuery {
                terms: vec!["keyboard".into()],
                apps: vec!["com.apple.Safari".into()],
                since_ms: Some(1_700_000_000_000 + 50_000_000),
                ..Default::default()
            },
        ),
        (
            "kind only",
            SearchQuery {
                kinds: vec![Kind::Link],
                ..Default::default()
            },
        ),
    ];
    for (name, query) in &cases {
        group.bench_function(*name, |b| {
            b.iter(|| store.search(query, 0, 200).expect("search"))
        });
    }
    group.bench_function("history page", |b| {
        b.iter(|| store.history(None, 200).expect("history"))
    });
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
