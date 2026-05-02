//! pdfium-render wrapper. Three responsibilities:
//!   1. Locate libpdfium.so at startup (vendored next to the binary,
//!      or pointed at via $TERMPDF_PDFIUM).
//!   2. Cheap per-page metrics (width/height in PDF points) used by
//!      the continuous-scroll layout.
//!   3. Render a single page at a target *width* — height falls out
//!      of the page aspect ratio. The continuous renderer pre-knows
//!      the height from `page_metrics()`, so no `maximum_height`
//!      cap is needed.

use std::env;
use std::path::Path;

use anyhow::Result;
use image::DynamicImage;
use pdfium_render::prelude::*;

pub fn bindings(lib_path: &str) -> Result<Box<dyn PdfiumLibraryBindings>> {
    Ok(Pdfium::bind_to_library(lib_path)?)
}

/// Search known locations for `libpdfium.so`:
///   1. `$TERMPDF_PDFIUM` env var (explicit override)
///   2. `<exe-dir>/{,../,../../}vendor/{,lib/}libpdfium.so`
///      — pdfium-binaries extracts to `vendor/lib/`; some installs flatten
///        the lib into `vendor/` directly. Cover both.
///   3. Fixed dev path + system locations.
pub fn find_libpdfium() -> Result<String> {
    if let Ok(p) = env::var("TERMPDF_PDFIUM") {
        return Ok(p);
    }

    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            for rel in [
                "vendor/lib/libpdfium.so",
                "vendor/libpdfium.so",
                "../vendor/lib/libpdfium.so",
                "../vendor/libpdfium.so",
                "../../vendor/lib/libpdfium.so",
                "../../vendor/libpdfium.so",
            ] {
                let cand = dir.join(rel);
                if cand.exists() {
                    return Ok(cand.to_string_lossy().into_owned());
                }
            }
        }
    }

    for p in ["/usr/lib64/libpdfium.so", "/usr/lib/libpdfium.so"] {
        if Path::new(p).exists() {
            return Ok(p.to_string());
        }
    }
    anyhow::bail!("libpdfium.so not found; run setup.sh or set TERMPDF_PDFIUM");
}

/// Per-page natural size in PDF points. Cheap to query — no
/// rasterisation, just the page tree metadata. Read once at load.
#[derive(Debug, Clone, Copy)]
pub struct PageMetrics {
    pub width_pts: f32,
    pub height_pts: f32,
}

pub fn page_metrics(document: &PdfDocument<'_>) -> Result<Vec<PageMetrics>> {
    let pages = document.pages();
    let total = pages.len() as i32;
    let mut out = Vec::with_capacity(total.max(0) as usize);
    for i in 0..total {
        let p = pages.get(i)?;
        out.push(PageMetrics {
            width_pts: p.width().value,
            height_pts: p.height().value,
        });
    }
    Ok(out)
}

/// Render `page_idx` at exactly `target_width_px` pixels wide. The
/// height comes from the page's aspect ratio and matches what
/// `PageLayout::build` predicted from `PageMetrics`.
pub fn render_page_at_width(
    document: &PdfDocument<'_>,
    page_idx: usize,
    target_width_px: u32,
) -> Result<DynamicImage> {
    let pages = document.pages();
    let total = pages.len() as i32;
    if total <= 0 {
        anyhow::bail!("PDF has zero pages");
    }
    let idx = (page_idx as i32).clamp(0, total - 1);
    let page = pages.get(idx)?;
    let config = PdfRenderConfig::new().set_target_width(target_width_px.max(1) as i32);
    let bitmap = page.render_with_config(&config)?;
    Ok(bitmap.as_image()?)
}
