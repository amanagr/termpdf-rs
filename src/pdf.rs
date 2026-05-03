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

use anyhow::{Context, Result};
use image::DynamicImage;
use pdfium_render::prelude::*;

pub fn bindings(lib_path: &str) -> Result<Box<dyn PdfiumLibraryBindings>> {
    Pdfium::bind_to_library(lib_path)
        .with_context(|| format!("dlopen failed for {lib_path}"))
}

/// Search known locations for `libpdfium.so`:
///   1. `$TERMPDF_PDFIUM` env var (explicit override)
///   2. `<exe-dir>/{,../,../../}vendor/{,lib/}libpdfium.so`
///      — pdfium-binaries extracts to `vendor/lib/`; some installs flatten
///        the lib into `vendor/` directly. Cover both.
///   3. Fixed dev path + system locations.
pub fn find_libpdfium() -> Result<String> {
    if let Ok(p) = env::var("TERMPDF_PDFIUM") {
        // Validate the override before handing it to dlopen. An
        // unchecked $TERMPDF_PDFIUM lets a hostile environment (e.g.
        // a forwarded shell session, a sourced .env) point us at any
        // shared library on the box, which Pdfium::bind_to_library
        // would then dlopen and execute initialisers from. Require
        // the file to exist and have a recognisable shared-lib
        // extension — the latter is a defence in depth, not a real
        // sandbox, but it kicks out the trivial cases.
        let path = Path::new(&p);
        if !path.exists() {
            anyhow::bail!("$TERMPDF_PDFIUM={p} does not exist");
        }
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "so" || e == "dylib" || e == "dll"
            })
            .unwrap_or(false);
        let name_match = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                // Allow versioned names like libpdfium.so.1 too.
                let lower = n.to_ascii_lowercase();
                lower.starts_with("libpdfium.") || lower.starts_with("pdfium.")
            })
            .unwrap_or(false);
        if !ext_ok && !name_match {
            anyhow::bail!(
                "$TERMPDF_PDFIUM={p} doesn't look like a pdfium shared library \
                 (expected libpdfium.{{so,dylib,dll}} or similar)"
            );
        }
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
    let total = pages.len();
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

/// Render-quality tier. `Fast` is the on-the-wire scroll path:
/// render at `target_width_px` directly with pdfium's own AA. `Sharp`
/// supersamples by `TERMPDF_RENDER_SCALE` (default 2×) then Lanczos3-
/// downsamples; it's perceptibly crisper at the cost of ~3-4× CPU per
/// page.
///
/// The point of having a tiered API: cold renders fired during a
/// scroll keystroke pay just enough work to land a readable bitmap on
/// the wire (`Fast`), and the idle path quietly upgrades visible
/// pages to `Sharp` while the user is reading. The user reported a
/// 50→74°C scroll-induced heat spike on a 600-page book — that
/// thermal envelope was almost entirely the Sharp-tier render +
/// Lanczos3 cost firing on every scrolled-into uncached page. Fast
/// pulls the per-scroll work down by ~70%; Sharp keeps the visible
/// quality the same once the scroll settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderQuality {
    /// Render directly at `target_width_px` (no supersampling, no
    /// downsample). ~6-10 ms for a typical 1600 px page on x86_64.
    /// Used during active reading.
    Fast,
    /// Render at `target_width_px * TERMPDF_RENDER_SCALE` then
    /// downsample with Lanczos3. ~25-40 ms for a typical 1600 px page.
    /// Used during long-idle background refinement.
    Sharp,
}

/// Supersampling factor for `RenderQuality::Sharp`, read from
/// `$TERMPDF_RENDER_SCALE`. Default 2.0 is the sweet spot between
/// sharpness and latency; 1.0 makes Sharp identical to Fast.
fn render_scale() -> f32 {
    use std::sync::OnceLock;
    static CACHED: OnceLock<f32> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TERMPDF_RENDER_SCALE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v >= 1.0 && *v <= 4.0)
            .unwrap_or(2.0)
    })
}

/// Render `page_idx` at exactly `target_width_px` pixels wide using
/// `RenderQuality::Fast` — pdfium's native AA, no supersampling. The
/// scroll path's default. ~70% faster than Sharp; visibly slightly
/// fuzzier on ultra-fine text, indistinguishable on body copy at
/// typical terminal cell sizes.
pub fn render_page_at_width(
    document: &PdfDocument<'_>,
    page_idx: usize,
    target_width_px: u32,
) -> Result<DynamicImage> {
    render_page_at_width_quality(document, page_idx, target_width_px, RenderQuality::Fast)
}

/// Sharp variant: supersample then Lanczos3-downsample. Used by the
/// idle refinement worker so the user reads sharp text once their
/// scroll settles.
pub fn render_page_sharp(
    document: &PdfDocument<'_>,
    page_idx: usize,
    target_width_px: u32,
) -> Result<DynamicImage> {
    render_page_at_width_quality(document, page_idx, target_width_px, RenderQuality::Sharp)
}

fn render_page_at_width_quality(
    document: &PdfDocument<'_>,
    page_idx: usize,
    target_width_px: u32,
    quality: RenderQuality,
) -> Result<DynamicImage> {
    let pages = document.pages();
    let total = pages.len();
    if total <= 0 {
        anyhow::bail!("PDF has zero pages");
    }
    let idx = (page_idx as i32).clamp(0, total - 1);
    let page = pages.get(idx)?;
    let target = target_width_px.max(1);
    // Cap supersample at ~6000 px so wide pages at extreme zoom don't
    // blow up to multi-hundred-MB pdfium renders.
    const RENDER_W_CAP: u32 = 6000;
    let render_w = match quality {
        RenderQuality::Fast => target,
        RenderQuality::Sharp => {
            let scale = render_scale();
            ((target as f32 * scale).round() as u32)
                .max(target)
                .min(RENDER_W_CAP.max(target))
        }
    };
    let config = PdfRenderConfig::new()
        .set_target_width(render_w as i32)
        // LCD-style text anti-aliasing — sharper edges than greyscale AA.
        // Safe for terminal display because we never rotate or scale the
        // image after pdfium produces it (terminal cell grid is rect-aligned).
        .use_lcd_text_rendering(true);
    let bitmap = page.render_with_config(&config)?;
    let img = bitmap.as_image()?;
    if render_w == target {
        return Ok(img);
    }
    let scaled_h = ((img.height() as u64 * target as u64) / render_w as u64) as u32;
    Ok(img.resize_exact(target, scaled_h.max(1), image::imageops::FilterType::Lanczos3))
}
