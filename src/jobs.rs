//! Background work that must not block capture or rendering: thumbnails, OCR, and app icon
//! tints. The functions are synchronous and run on GPUI's background executor; callers notify
//! the UI when they finish.

use std::io::Cursor;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use tracing::debug;

use crate::store::{Item, Kind, Store};

/// Long edge of card thumbnails in pixels.
pub const THUMB_LONG_EDGE: u32 = 400;
/// Long edge of the preview window's copy of an image: sharp at its size on a Retina screen,
/// and a few megabytes decoded where a 5K screenshot is sixty.
pub const PREVIEW_LONG_EDGE: u32 = 1600;

/// The thumbnail for an image item (or a link preview image), created on first request.
/// Returns the PNG path, or `None` when the item has no image.
pub fn ensure_thumbnail(store: &Store, item: &Item) -> Result<Option<PathBuf>> {
    let blob = match item.kind {
        Kind::Image => item.image_blob.as_deref(),
        Kind::Link => item.link_image_blob.as_deref(),
        _ => None,
    };
    match blob {
        Some(blob) => ensure_scaled(store, blob, blob, THUMB_LONG_EDGE).map(Some),
        None => Ok(None),
    }
}

/// The copy of an image item shown in the preview window, created on first request.
pub fn ensure_preview_image(store: &Store, item: &Item) -> Result<Option<PathBuf>> {
    match item.image_blob.as_deref() {
        Some(blob) => ensure_scaled(store, blob, &preview_key(blob), PREVIEW_LONG_EDGE).map(Some),
        None => Ok(None),
    }
}

/// The thumbnail-directory key of an image blob's preview-size copy.
pub fn preview_key(blob: &str) -> String {
    format!("{blob}-preview")
}

fn ensure_scaled(store: &Store, blob: &str, key: &str, long_edge: u32) -> Result<PathBuf> {
    let path = store.thumb_path(key);
    if path.exists() {
        return Ok(path);
    }
    let bytes =
        std::fs::read(store.blob_path(blob)).with_context(|| format!("read blob {blob}"))?;
    crate::store::write_atomic(&path, &thumbnail_png(&bytes, long_edge)?)?;
    debug!(
        "{long_edge} px copy written for blob {}",
        &blob[..8.min(blob.len())]
    );
    Ok(path)
}

/// Stores an already-rendered PNG (a file icon) under `key` in the thumbnail cache.
pub fn write_thumbnail(store: &Store, key: &str, png: &[u8]) -> Result<PathBuf> {
    let path = store.thumb_path(key);
    if !path.exists() {
        crate::store::write_atomic(&path, png)?;
    }
    Ok(path)
}

/// Decodes any supported image and re-encodes a PNG whose long edge is at most `long_edge`.
/// Formats the `image` crate cannot read (HEIC) go through ImageIO first.
pub fn thumbnail_png(bytes: &[u8], long_edge: u32) -> Result<Vec<u8>> {
    let decoded = match image::load_from_memory(bytes) {
        Ok(img) => img,
        Err(_) => {
            let png =
                crate::macos::image_to_png(bytes).context("ImageIO could not decode image")?;
            image::load_from_memory(&png)?
        }
    };
    let small = if decoded.width() > long_edge || decoded.height() > long_edge {
        decoded.thumbnail(long_edge, long_edge)
    } else {
        decoded
    };
    let mut out = Vec::new();
    small.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

/// Minimum side (px) for OCR; smaller images are icons and glyphs, not text.
pub const OCR_MIN_SIDE: i64 = 64;

/// Runs Vision on an image item and stores the recognized text for search. Returns the text.
pub fn ocr_item(store: &Store, item: &Item) -> Result<Option<String>> {
    if item.kind != Kind::Image
        || item.image_w.unwrap_or(0) < OCR_MIN_SIDE
        || item.image_h.unwrap_or(0) < OCR_MIN_SIDE
    {
        return Ok(None);
    }
    let Some(blob) = &item.image_blob else {
        return Ok(None);
    };
    let bytes =
        std::fs::read(store.blob_path(blob)).with_context(|| format!("read blob {blob}"))?;
    let text = crate::macos::recognize_text(&bytes);
    if let Some(text) = &text {
        store.set_ocr_text(item.id, text)?;
        debug!("ocr: {} chars for item {}", text.len(), item.id);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba(rgba))
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn thumbnail_downscales_long_edge() {
        let big = png(1000, 500, [10, 20, 30, 255]);
        let thumb = thumbnail_png(&big, 400).unwrap();
        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 200));
        let small = png(50, 50, [1, 2, 3, 255]);
        let thumb = thumbnail_png(&small, 400).unwrap();
        assert_eq!(image::load_from_memory(&thumb).unwrap().width(), 50);
    }

    #[test]
    fn ensure_thumbnail_creates_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let new = crate::store::NewItem {
            kind: Some(Kind::Image),
            image: Some(crate::store::NewImage {
                png_or_original: png(800, 100, [0, 0, 255, 255]),
                mime: "image/png",
                width: 800,
                height: 100,
                truncated: false,
            }),
            ..Default::default()
        };
        let (item, _) = store.insert(&new).unwrap();
        let path = ensure_thumbnail(&store, &item).unwrap().unwrap();
        assert!(path.exists());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let again = ensure_thumbnail(&store, &item).unwrap().unwrap();
        assert_eq!(again, path);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let text = store
            .insert(&crate::store::NewItem::text("no image"))
            .unwrap()
            .0;
        assert_eq!(ensure_thumbnail(&store, &text).unwrap(), None);
    }
}
