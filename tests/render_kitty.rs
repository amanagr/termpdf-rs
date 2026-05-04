//! Per-page kitty placement smoke test.
//!
//! Like `render_pty.rs` but runs `--protocol kitty` so the binary
//! exercises the new `kitty_pages` draw path: per-page transmits,
//! unicode-placeholder cells, PNG `f=100` payloads. We can't
//! visually verify the image renders correctly inside a fake pty,
//! but we can assert the protocol bytes the binary writes — that's
//! enough to catch regressions in escape format, transmit absence,
//! or full-on crashes on the kitty path.
//!
//! Separate file (and therefore separate test binary) because
//! pdfium-render's `Pdfium::bind_to_library` is process-global; the
//! existing `render_pty.rs` test already binds it and we'd collide.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn binary_path() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for rel in ["target/debug/termpdf", "target/release/termpdf"] {
        let p = root.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn pdfium_or_skip() -> Option<pdfium_render::prelude::Pdfium> {
    let candidates = [
        std::env::var("TERMPDF_PDFIUM").ok(),
        Some(format!(
            "{}/vendor/lib/libpdfium.so",
            env!("CARGO_MANIFEST_DIR")
        )),
        Some(format!(
            "{}/vendor/libpdfium.so",
            env!("CARGO_MANIFEST_DIR")
        )),
        Some("/usr/lib64/libpdfium.so".into()),
        Some("/usr/lib/libpdfium.so".into()),
    ];
    let lib = candidates
        .into_iter()
        .flatten()
        .find(|p| std::path::Path::new(p).exists());
    let lib = match lib {
        Some(p) => p,
        None => {
            eprintln!("skipping: libpdfium not found");
            return None;
        }
    };
    let bindings = pdfium_render::prelude::Pdfium::bind_to_library(&lib).ok()?;
    Some(pdfium_render::prelude::Pdfium::new(bindings))
}

fn make_test_pdf(pdfium: &pdfium_render::prelude::Pdfium) -> PathBuf {
    use pdfium_render::prelude::*;
    let mut path = std::env::temp_dir();
    path.push(format!("termpdf-render-kitty-{}.pdf", std::process::id()));
    let mut doc = pdfium.create_new_pdf().expect("create pdf");
    // Make 3 pages so we exercise per-page IDs (not just page 0).
    for i in 0..3 {
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .expect("create page");
        let font = doc.fonts_mut().helvetica();
        let text = format!("page {}", i + 1);
        let mut object =
            PdfPageTextObject::new(&doc, text, font, PdfPoints::new(14.0)).expect("text");
        object
            .translate(PdfPoints::new(50.0), PdfPoints::new(700.0))
            .expect("translate");
        page.objects_mut().add_text_object(object).expect("add");
    }
    doc.save_to_file(&path).expect("save");
    path
}

fn spawn_in_pty(
    pdf: &std::path::Path,
) -> (
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn Write + Send>,
    mpsc::Receiver<Vec<u8>>,
) {
    let bin = binary_path().expect("binary built");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 800,
            pixel_height: 600,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(bin);
    cmd.arg(pdf);
    // Force the kitty path. We're not running inside a real kitty
    // terminal so the image won't actually display, but the binary
    // will still emit the protocol bytes — which is what we assert on.
    cmd.arg("--protocol");
    cmd.arg("kitty");
    cmd.env("TERM", "xterm-kitty");
    cmd.env_remove("TMUX");
    cmd.env(
        "TERMPDF_PDFIUM",
        format!("{}/vendor/lib/libpdfium.so", env!("CARGO_MANIFEST_DIR")),
    );
    cmd.env("TERMPDF_CELL_PX", "8x16");
    // Sandbox the disk cache to a per-test temp dir so test runs
    // don't pollute the user's real ~/.cache/termpdf-rs.
    let tmp_cache =
        std::env::temp_dir().join(format!("termpdf-test-cache-kitty-{}", std::process::id()));
    cmd.env("XDG_CACHE_HOME", &tmp_cache);

    let child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = pair.master.take_writer().expect("writer");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    (child, writer, rx)
}

fn drain_until(
    rx: &mpsc::Receiver<Vec<u8>>,
    buf: &mut Vec<u8>,
    total: Duration,
    mut predicate: impl FnMut(&[u8]) -> bool,
) -> bool {
    let deadline = Instant::now() + total;
    if predicate(buf) {
        return true;
    }
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(chunk) => {
                buf.extend_from_slice(&chunk);
                if predicate(buf) {
                    return true;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if predicate(buf) {
                    return true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    predicate(buf)
}

fn twoway_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn dump_printable(bytes: &[u8], n: usize) -> String {
    let take = bytes.len().min(n);
    bytes[..take]
        .iter()
        .map(|b| {
            if (0x20..=0x7e).contains(b) || *b == b'\n' {
                *b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Smoke test: open a 3-page PDF in --protocol kitty, wait for the
/// status line to appear, then look for the kitty graphics transmit
/// escape sequence (\x1b_G). Asserts that:
///   * the binary doesn't crash on the kitty path,
///   * `f=100` (PNG) is the on-the-wire format (we shipped this),
///   * `U=1` (unicode-placeholder mode) is in the transmit,
///   * the placeholder character `\u{10EEEE}` (UTF-8: F4 8E BB AE)
///     is in the buffer,
///   * `:q` cleanly exits.
#[test]
fn binary_kitty_path_emits_transmit_and_placeholders() {
    let Some(pdfium) = pdfium_or_skip() else {
        return;
    };
    let pdf = make_test_pdf(&pdfium);

    let (mut child, mut writer, rx) = spawn_in_pty(&pdf);

    let mut buf = Vec::new();
    let painted = drain_until(&rx, &mut buf, Duration::from_secs(8), |b| {
        twoway_contains(b, b"1/3")
    });
    assert!(
        painted,
        "status line `1/3` never appeared after opening PDF.\nLast 1KB:\n{}",
        dump_printable(&buf[buf.len().saturating_sub(1024)..], 1024)
    );

    // Drain a bit more to catch the kitty transmit (sent in the same
    // term.draw cycle as the status line, but the chunked DCS may
    // arrive in a later read).
    drain_until(&rx, &mut buf, Duration::from_millis(800), |_| false);

    assert!(
        twoway_contains(&buf, b"\x1b_G"),
        "no kitty graphics escape (\\x1b_G) seen — kitty draw path didn't emit a transmit"
    );
    assert!(
        twoway_contains(&buf, b"f=100"),
        "kitty transmit didn't include f=100 (PNG); should have switched off raw RGBA"
    );
    assert!(
        twoway_contains(&buf, b"U=1"),
        "kitty transmit missing U=1 (unicode placeholder mode)"
    );
    // U+10EEEE in UTF-8 = F4 8E BB AE
    assert!(
        twoway_contains(&buf, b"\xf4\x8e\xbb\xae"),
        "no \\u{{10EEEE}} placeholder character in output — placement loop didn't run"
    );

    // Quit cleanly.
    writer.write_all(b":q\r").ok();
    writer.flush().ok();
    let exit_deadline = Instant::now() + Duration::from_secs(4);
    let mut clean_exit = false;
    while Instant::now() < exit_deadline {
        if let Ok(Some(_)) = child.try_wait() {
            clean_exit = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !clean_exit {
        let _ = child.kill();
    }
    assert!(
        clean_exit,
        "binary did not exit after `:q\\r` on kitty path"
    );
}
