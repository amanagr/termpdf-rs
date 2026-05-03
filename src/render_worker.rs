//! Background page-render worker.
//!
//! Owns its own pdfium binding + document handle so the main thread
//! never blocks on rasterisation during steady scrolling. The worker
//! receives prefetch requests on a channel and emits completed
//! bitmaps; the main thread drains them at the top of every render
//! cycle and inserts into `App::page_cache`.
//!
//! Scope is intentionally narrow: only **prefetch** goes through the
//! worker. Visible-page rendering on a cold cache (cold start, big
//! page jumps) stays synchronous so the UI shows the new page in the
//! same frame the user asked for it. Steady scrolling with a warm
//! prefetch buffer becomes ~free.
//!
//! Why a second pdfium instance and not a shared one: `PdfDocument`
//! borrows `Pdfium`, which is a lifetime-tied borrow that can't cross
//! threads. Loading the doc twice is cheap (the underlying file
//! cache is shared at the OS level) and lets the worker work
//! concurrently with the main thread without lock contention.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use image::DynamicImage;
use pdfium_render::prelude::Pdfium;

use crate::dark;
use crate::pdf;

/// One render request. The worker matches the (page, target_width,
/// dark) tuple in its reply so the main thread can correlate the
/// response with what's still relevant — e.g. discard a stale render
/// after the user zoomed.
#[derive(Debug, Clone, Copy)]
pub struct RenderReq {
    pub page: usize,
    pub target_width_px: u32,
    pub dark: bool,
}

/// Completed render. `Result` lets the worker propagate per-page
/// failures without killing itself; the main thread inserts only on
/// `Ok` and surfaces a status hint on `Err`.
#[derive(Debug)]
pub struct RenderResp {
    pub req: RenderReq,
    pub image: anyhow::Result<DynamicImage>,
}

pub struct RenderWorker {
    pub req_tx: Sender<RenderReq>,
    pub resp_rx: Receiver<RenderResp>,
    /// Held so the worker thread is joined cleanly when `App` drops.
    /// Sender is dropped first, the worker exits on `recv()` returning
    /// `Err`, and the JoinHandle's drop-on-drop completes the join.
    #[allow(dead_code)]
    handle: Option<JoinHandle<()>>,
}

impl RenderWorker {
    /// Spawn a worker that owns its own pdfium binding loaded from
    /// `lib_path` and a fresh `PdfDocument` opened from `pdf_path`.
    /// Returns `None` if the worker setup fails (e.g. pdfium dlopen
    /// or document load fails); callers can fall back to synchronous
    /// rendering instead of crashing.
    pub fn spawn(lib_path: String, pdf_path: PathBuf) -> Option<Self> {
        let (req_tx, req_rx) = mpsc::channel::<RenderReq>();
        let (resp_tx, resp_rx) = mpsc::channel::<RenderResp>();

        let handle = thread::Builder::new()
            .name("termpdf-render".into())
            .spawn(move || {
                let bindings = match pdf::bindings(&lib_path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("render-worker: dlopen failed: {e:#}");
                        return;
                    }
                };
                let pdfium = Pdfium::new(bindings);
                let document = match pdfium.load_pdf_from_file(&pdf_path, None) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("render-worker: pdf load failed: {e:#}");
                        return;
                    }
                };
                while let Ok(req) = req_rx.recv() {
                    let img = pdf::render_page_at_width(&document, req.page, req.target_width_px)
                        .map(|img| {
                            if req.dark {
                                DynamicImage::ImageRgba8(dark::invert_luminance(&img))
                            } else {
                                img
                            }
                        });
                    // Send may fail if the receiver dropped (App
                    // shutting down); just stop processing.
                    if resp_tx
                        .send(RenderResp { req, image: img })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .ok()?;

        Some(Self {
            req_tx,
            resp_rx,
            handle: Some(handle),
        })
    }

    /// Try to send a prefetch request. Returns `false` if the channel
    /// disconnected (worker died) — caller should fall back to sync
    /// render or skip silently.
    pub fn request(&self, req: RenderReq) -> bool {
        self.req_tx.send(req).is_ok()
    }

    /// Drain all completed responses without blocking. Returns them
    /// in arrival order; main thread inserts each into `page_cache`.
    pub fn drain(&self) -> Vec<RenderResp> {
        let mut out = Vec::new();
        loop {
            match self.resp_rx.try_recv() {
                Ok(r) => out.push(r),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}
