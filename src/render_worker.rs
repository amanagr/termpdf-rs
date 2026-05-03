//! Background page-render worker — currently a no-op stub.
//!
//! Original intent: spawn a thread that owns its own pdfium binding +
//! `PdfDocument` so prefetch rasterisation could happen off the main
//! thread, keeping steady scrolling smooth.
//!
//! Why it's a stub today: `pdfium-render`'s `Pdfium::bind_to_library`
//! is process-global — the second call returns
//! `PdfiumLibraryBindingsAlreadyInitialized`. And `PdfDocument` borrows
//! the `Pdfium`, so the main thread's binding can't simply be moved or
//! reborrowed across threads. There's no upstream API to clone or share
//! the existing binding either.
//!
//! Possible futures:
//!   - `Arc<Mutex<PdfDocument>>` — works but serializes pdfium calls,
//!     which defeats the parallelism we wanted.
//!   - Upstream pdfium-render exposing a shared-bindings API.
//!   - Switch to a different PDF renderer with thread-friendly bindings
//!     (mupdf, poppler).
//!
//! `spawn` returns `None`; `request`/`drain` exist as no-ops so call
//! sites stay one-line and don't have to special-case the stub.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use image::DynamicImage;

#[derive(Debug, Clone, Copy)]
pub struct RenderReq {
    pub page: usize,
    pub target_width_px: u32,
    pub dark: bool,
}

#[derive(Debug)]
pub struct RenderResp {
    pub req: RenderReq,
    pub image: anyhow::Result<DynamicImage>,
}

#[allow(dead_code)]
pub struct RenderWorker {
    pub req_tx: Sender<RenderReq>,
    pub resp_rx: Receiver<RenderResp>,
    handle: Option<JoinHandle<()>>,
}

impl RenderWorker {
    /// Returns `None` — see module docs. Call sites already handle the
    /// `None` case by falling back to synchronous rendering.
    pub fn spawn(_lib_path: String, _pdf_path: PathBuf) -> Option<Self> {
        None
    }

    pub fn request(&self, _req: RenderReq) -> bool {
        false
    }

    pub fn drain(&self) -> Vec<RenderResp> {
        Vec::new()
    }
}
