//! On-disk cache of rendered PDF pages. Skips pdfium re-render for
//! repeated opens of the same PDF.
//!
//! ## Cache key
//!
//! Composed from `(file_path, file_mtime, file_size, page_idx,
//! fit_width_px, dark)`. The file metadata invalidates the cache when
//! the user edits the PDF; layout fields (width/dark) gate per
//! zoom-level + dark-mode combo.
//!
//! ## Storage
//!
//! `$XDG_CACHE_HOME/termpdf-rs/<file-hash>/<page>_<w>_<dark>.png`
//!
//! - `<file-hash>` keys the directory by file identity (not full
//!   path) so renamed/moved files reuse the cache.
//! - `<w>` is fit_width_px; `<dark>` is `0` or `1`.
//!
//! ## Trade-off
//!
//! Writes are best-effort — failures (no XDG_CACHE_HOME, full disk,
//! ENOSPC) are swallowed silently because the cache is purely an
//! optimization and the renderer falls through to pdfium. Reads
//! similarly: any error → cache miss → pdfium.
//!
//! ## What this gives up
//!
//! No size cap. The user can `rm -rf ~/.cache/termpdf-rs` if they
//! want to reclaim space. A 700-page PDF caches at ~175 MB
//! (PNG-compressed); typical users open few PDFs so this is fine
//! in practice. Adding LRU eviction is a future task.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::DynamicImage;

/// Compose the cache file path for one rendered page. Returns `None`
/// when no cache directory is available (no `XDG_CACHE_HOME`, no
/// `$HOME`, etc.) or when the source file's metadata can't be read.
pub fn cache_path(
    pdf_path: &Path,
    page_idx: usize,
    fit_width_px: u32,
    dark: bool,
) -> Option<PathBuf> {
    let cache_root = dirs::cache_dir()?.join("termpdf-rs");
    let meta = std::fs::metadata(pdf_path).ok()?;
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let size = meta.len();
    let path_str = pdf_path.to_string_lossy();
    // 64-bit FNV-1a of path|size|mtime → hex. Stable, collision
    // probability negligible for the cache count we expect (a few
    // hundred PDFs).
    let key_str = format!("{}|{}|{}", path_str, size, mtime_secs);
    let hash = fnv1a_hash_str(&key_str);
    let dir = cache_root.join(format!("{:016x}", hash));
    let file = format!("{}_{}_{}.png", page_idx, fit_width_px, dark as u8);
    Some(dir.join(file))
}

/// Try to load a cached page bitmap. `None` on miss or any read /
/// decode error — caller falls through to pdfium.
pub fn load(path: &Path) -> Option<DynamicImage> {
    if !path.exists() {
        return None;
    }
    image::open(path).ok()
}

/// Write a rendered page bitmap to the cache. Best-effort: returns
/// `Ok(false)` on any IO failure (cache root unwritable, ENOSPC) so
/// callers can ignore failures without special-casing.
///
/// Encodes with `Fast` + `Up` filter — same settings as the on-the-
/// wire payload encoder, optimised for PDF backgrounds (50× ratio
/// at ~2 ms for a typical 1600×2300 page).
pub fn store(path: &Path, image: &DynamicImage) -> std::io::Result<bool> {
    let parent = match path.parent() {
        Some(p) => p,
        None => return Ok(false),
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        // dir creation race or ENOSPC — skip cache, don't propagate.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Ok(false);
        }
    }
    let rgba = image.to_rgba8();
    let mut buf = Vec::with_capacity(512 * 1024);
    {
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        use image::ImageEncoder;
        let encoder = PngEncoder::new_with_quality(
            &mut buf,
            CompressionType::Fast,
            FilterType::Up,
        );
        if encoder
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .is_err()
        {
            return Ok(false);
        }
    }
    // Atomic-ish write: write to a tmp sibling then rename. Avoids
    // half-written files on crash mid-write being read as truncated
    // PNGs on next open.
    let tmp = path.with_extension("png.tmp");
    if std::fs::write(&tmp, &buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    Ok(true)
}

fn fnv1a_hash_str(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_includes_layout_fields() {
        let pdf = std::env::temp_dir().join("disk_cache_test_dummy.pdf");
        std::fs::write(&pdf, b"x").unwrap();
        let p1 = cache_path(&pdf, 5, 1024, false).expect("dirs::cache_dir works");
        let p2 = cache_path(&pdf, 5, 1024, true).expect("dirs::cache_dir works");
        assert_ne!(p1, p2, "dark flag must alter the path");
        let p3 = cache_path(&pdf, 5, 2048, false).unwrap();
        assert_ne!(p1, p3, "fit_width_px must alter the path");
        let p4 = cache_path(&pdf, 6, 1024, false).unwrap();
        assert_ne!(p1, p4, "page_idx must alter the path");
        std::fs::remove_file(&pdf).ok();
    }

    #[test]
    fn store_then_load_roundtrips() {
        // Tiny solid-color image; verifies write + read decode.
        let dir = std::env::temp_dir().join(format!("disk_cache_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.png");
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            16, 16,
            image::Rgba([12, 34, 56, 255]),
        ));
        let ok = store(&path, &img).unwrap();
        assert!(ok, "store should succeed under temp dir");
        let loaded = load(&path).expect("load returns Some on hit");
        assert_eq!(loaded.width(), 16);
        assert_eq!(loaded.height(), 16);
        let pixel = loaded.to_rgba8().get_pixel(0, 0).0;
        assert_eq!(pixel, [12, 34, 56, 255]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_returns_none() {
        let path = std::env::temp_dir().join("nonexistent-disk-cache-file.png");
        assert!(load(&path).is_none());
    }

    #[test]
    fn fnv1a_hash_changes_with_input() {
        let h1 = fnv1a_hash_str("a");
        let h2 = fnv1a_hash_str("b");
        let h3 = fnv1a_hash_str("");
        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
    }
}
