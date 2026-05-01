//! pdfium-render wrapper. Two responsibilities:
//!   1. Locate libpdfium.so at startup (vendored next to the binary,
//!      or pointed at via $TERMPDF_PDFIUM).
//!   2. Render a page index to a `DynamicImage` at a target size that
//!      matches the terminal's pixel grid.

use std::env;
use std::path::Path;

use anyhow::Result;
use image::DynamicImage;
use pdfium_render::prelude::*;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;

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

    for p in [
        "/home/aman/termpdf-rs/vendor/lib/libpdfium.so",
        "/home/aman/termpdf-rs/vendor/libpdfium.so",
        "/usr/lib64/libpdfium.so",
        "/usr/lib/libpdfium.so",
    ] {
        if Path::new(p).exists() {
            return Ok(p.to_string());
        }
    }
    anyhow::bail!("libpdfium.so not found; run setup.sh or set TERMPDF_PDFIUM");
}

/// Render page `page_idx` of `document` into a `DynamicImage` sized to
/// fit `area` (terminal cells). `zoom` of 1.0 means "fit"; >1 zooms in.
///
/// We multiply the cell area by `picker.font_size()` to get pixel
/// dimensions, since one terminal cell is several pixels tall/wide
/// — without that, kitty graphics renders at one pixel per cell and
/// the page is illegible.
pub fn render_page(
    document: &PdfDocument<'_>,
    page_idx: usize,
    area: Rect,
    picker: &Picker,
    zoom: f32,
) -> Result<DynamicImage> {
    let pages = document.pages();
    // pdfium-render 0.9 widened `PdfPageIndex` from `u16` to `i32` to
    // mirror pdfium's C API. Keep arithmetic in i32 so the bound check
    // and the `pages.get(idx)` call line up without a redundant cast.
    let total = pages.len() as i32;
    if total <= 0 {
        anyhow::bail!("PDF has zero pages");
    }
    let idx = (page_idx as i32).clamp(0, total - 1);
    let page = pages.get(idx)?;

    let (cell_w, cell_h) = picker.font_size();
    let pixel_w = (area.width as i32) * (cell_w as i32);
    let pixel_h = (area.height as i32) * (cell_h as i32);
    let zoomed_w = ((pixel_w as f32) * zoom).max(64.0) as i32;
    let zoomed_h = ((pixel_h as f32) * zoom).max(64.0) as i32;

    let config = PdfRenderConfig::new()
        .set_target_width(zoomed_w)
        .set_maximum_height(zoomed_h);

    let bitmap = page.render_with_config(&config)?;
    Ok(bitmap.as_image()?)
}
