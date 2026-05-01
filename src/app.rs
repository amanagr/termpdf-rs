use std::path::{Path, PathBuf};

use anyhow::Result;
use pdfium_render::prelude::PdfDocument;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::highlight::HighlightStore;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Command,
    Visual,
    Search,
}

/// The cache key for `image_proto`. We rebuild the rendered image only
/// when something visible changes — page number, dark toggle, area
/// resize, zoom. Without this, every keypress would re-render a full
/// 1500×1900 pixmap and stall on big PDFs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderKey {
    pub page: usize,
    pub dark: bool,
    pub area_w: u16,
    pub area_h: u16,
    pub zoom_milli: u32,
}

pub struct App<'doc> {
    pub document: PdfDocument<'doc>,
    pub path: PathBuf,
    pub page: usize,
    pub page_count: usize,

    pub dark: bool,
    pub mode: Mode,
    pub pending: String,    // numeric prefix typed in normal mode
    pub cmd_buffer: String, // text typed after `:` or `/`
    pub status: String,     // ephemeral status-line message
    pub show_help: bool,
    pub zoom: f32,

    pub picker: Picker,
    pub image_proto: Option<StatefulProtocol>,
    pub last_render_key: Option<RenderKey>,

    pub highlights: HighlightStore,
    pub should_quit: bool,
}

impl<'doc> App<'doc> {
    pub fn new(
        document: PdfDocument<'doc>,
        path: &Path,
        page: usize,
        dark: bool,
        picker: Picker,
    ) -> Result<Self> {
        let page_count = document.pages().len() as usize;
        let highlights = HighlightStore::load(path)?;
        Ok(Self {
            document,
            path: path.to_path_buf(),
            page: page.min(page_count.saturating_sub(1).max(0)),
            page_count,
            dark,
            mode: Mode::Normal,
            pending: String::new(),
            cmd_buffer: String::new(),
            status: String::new(),
            show_help: false,
            zoom: 1.0,
            picker,
            image_proto: None,
            last_render_key: None,
            highlights,
            should_quit: false,
        })
    }

    pub fn invalidate(&mut self) {
        self.last_render_key = None;
    }

    pub fn goto_page(&mut self, page: usize) {
        let new = page.min(self.page_count.saturating_sub(1));
        if new != self.page {
            self.page = new;
            self.invalidate();
        }
    }

    pub fn next_page(&mut self, count: usize) {
        let target = self.page + count.max(1);
        self.goto_page(target);
    }
    pub fn prev_page(&mut self, count: usize) {
        let target = self.page.saturating_sub(count.max(1));
        self.goto_page(target);
    }
    pub fn first_page(&mut self) {
        self.goto_page(0);
    }
    pub fn last_page(&mut self) {
        self.goto_page(self.page_count.saturating_sub(1));
    }

    pub fn toggle_dark(&mut self) {
        self.dark = !self.dark;
        self.invalidate();
    }

    pub fn persist_highlights(&self) -> Result<()> {
        self.highlights.save(&self.path)
    }
}
