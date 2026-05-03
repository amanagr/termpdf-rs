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

/// Soft cap on disk-cache size. `evict_to_budget` runs at startup
/// and trims oldest files until under this budget. 512 MB ≈ ~2000
/// pages of typical PDFs — enough room for the user's working set
/// of recent PDFs without unbounded growth on a long-running setup.
const DISK_CACHE_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Returns the per-PDF cache directory. Same hashing scheme as
/// `cache_path` so the search index sits alongside the PNG cache.
pub fn pdf_cache_dir(pdf_path: &Path) -> Option<PathBuf> {
    let cache_root = dirs::cache_dir()?.join("termpdf-rs");
    let meta = std::fs::metadata(pdf_path).ok()?;
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let size = meta.len();
    let key_str = format!("{}|{}|{}", pdf_path.to_string_lossy(), size, mtime_secs);
    let hash = fnv1a_hash_str(&key_str);
    Some(cache_root.join(format!("{:016x}", hash)))
}

/// Compose the cache file path for one rendered page. Returns `None`
/// when no cache directory is available (no `XDG_CACHE_HOME`, no
/// `$HOME`, etc.) or when the source file's metadata can't be read.
pub fn cache_path(
    pdf_path: &Path,
    page_idx: usize,
    fit_width_px: u32,
    dark: bool,
) -> Option<PathBuf> {
    let dir = pdf_cache_dir(pdf_path)?;
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

/// Walk `~/.cache/termpdf-rs/`, sum file sizes; if over `budget`,
/// delete oldest files (by mtime, ascending) until under. Best-effort:
/// any IO error skips that file. Empty per-PDF dirs are also pruned.
///
/// Designed to run once at startup. On a 10 000-file cache the walk
/// + sort is sub-100 ms — small relative to pdfium binding load.
/// Caller should not error on failure.
pub fn evict_to_budget(budget_bytes: u64) -> std::io::Result<()> {
    let cache_root = match dirs::cache_dir() {
        Some(d) => d.join("termpdf-rs"),
        None => return Ok(()),
    };
    if !cache_root.exists() {
        return Ok(());
    }

    // Collect (mtime, size, path) for every cache file. Skip half-
    // written sidecar files (e.g. .png.tmp / .idx.tmp) so a crashed
    // write doesn't fool the sort. Counts both PNG (page bitmaps)
    // and .bin (search index) files toward the budget.
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for pdf_dir in std::fs::read_dir(&cache_root)?.flatten() {
        let pdf_dir_path = pdf_dir.path();
        if !pdf_dir_path.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&pdf_dir_path)?.flatten() {
            let path = file.path();
            let keep = path.extension().is_some_and(|e| e == "png" || e == "bin");
            if !keep {
                continue;
            }
            let meta = match file.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len();
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total += size;
            entries.push((mtime, size, path));
        }
    }

    if total <= budget_bytes {
        return Ok(());
    }

    // Oldest first.
    entries.sort_by_key(|e| e.0);

    let mut to_free = total - budget_bytes;
    for (_mtime, size, path) in entries {
        if to_free == 0 {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            to_free = to_free.saturating_sub(size);
        }
    }

    // Prune now-empty per-PDF dirs so they don't accumulate.
    if let Ok(read) = std::fs::read_dir(&cache_root) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let empty = std::fs::read_dir(&path)
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(false);
                if empty {
                    let _ = std::fs::remove_dir(&path);
                }
            }
        }
    }
    Ok(())
}

/// Convenience wrapper: spawn `evict_to_budget` in a background
/// thread so startup isn't blocked on filesystem walk for users
/// with large caches. Failures are swallowed.
pub fn evict_to_budget_async() {
    std::thread::spawn(|| {
        let _ = evict_to_budget(DISK_CACHE_BUDGET_BYTES);
    });
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

    /// Serialize tests that mutate XDG_CACHE_HOME — cargo runs
    /// tests in parallel by default and concurrent env mutation
    /// would race the lookup inside `evict_to_budget`.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn evict_to_budget_drops_files_when_over_budget() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Stand up a fake cache_root inside a temp dir and point
        // `dirs::cache_dir` at it via XDG_CACHE_HOME.
        let scratch = std::env::temp_dir()
            .join(format!("disk_cache_evict_test_{}", std::process::id()));
        let prev_xdg = std::env::var("XDG_CACHE_HOME").ok();
        // SAFETY: tests in this binary share process env. We restore
        // before assertions to keep this test self-contained.
        unsafe { std::env::set_var("XDG_CACHE_HOME", &scratch); }

        let pdf_dir = scratch.join("termpdf-rs/abcd1234abcd1234");
        std::fs::create_dir_all(&pdf_dir).unwrap();
        // Three 10 KB files = 30 KB total.
        let big = vec![0u8; 10 * 1024];
        for i in 0..3 {
            std::fs::write(pdf_dir.join(format!("{i}_800_0.png")), &big).unwrap();
        }

        // Budget 5 KB forces eviction of all but ~zero files (oldest
        // first; mtime ordering is whatever the filesystem assigned
        // so we don't pin a specific survivor here — we just check
        // the total bytes drop).
        evict_to_budget(5 * 1024).unwrap();

        // Restore env before assertions to avoid leaks on panic.
        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }

        let surviving = std::fs::read_dir(&pdf_dir)
            .map(|it| it.flatten().count())
            .unwrap_or(0);
        // Started with 3 files; budget is below one file's size; at
        // most one should survive (could be 0). Evict logic stops as
        // soon as `to_free` hits 0, so we expect 0 or 1 survivor.
        assert!(
            surviving <= 1,
            "expected ≤1 file to survive 5 KB budget, found {surviving}"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn evict_to_budget_no_op_when_under_budget() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let scratch = std::env::temp_dir()
            .join(format!("disk_cache_under_budget_test_{}", std::process::id()));
        let prev_xdg = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", &scratch); }

        let pdf_dir = scratch.join("termpdf-rs/cafebabecafebabe");
        std::fs::create_dir_all(&pdf_dir).unwrap();
        std::fs::write(pdf_dir.join("0_800_0.png"), vec![0u8; 1024]).unwrap();

        evict_to_budget(10 * 1024 * 1024).unwrap(); // 10 MB budget; well above 1 KB

        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }

        assert!(pdf_dir.join("0_800_0.png").exists(), "file under budget must survive");
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
