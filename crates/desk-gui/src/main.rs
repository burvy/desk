//! desk-gui: a window onto a desk workspace. It reads the same files the
//! CLI does and writes through desk-core, so it's only ever a view. If no
//! supervisor is running, it runs one in a background thread.

mod app;
mod snapshot;

use anyhow::Result;
use desk_core::Workspace;
use desk_core::supervisor::{self, Supervisor};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

fn main() -> Result<()> {
    let ws = match std::env::args().nth(1) {
        Some(p) => Workspace::open(std::path::Path::new(&p))?,
        None => Workspace::find(&std::env::current_dir()?)?,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(format!("desk — {}", ws.root.display()))
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "desk",
        options,
        Box::new(move |cc| {
            symbol_fallback(&cc.egui_ctx);
            Ok(Box::new(app::App::new(ws)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// egui's bundled fonts lack symbols like ● ⚙ →; borrow the system's symbol font.
fn symbol_fallback(ctx: &eframe::egui::Context) {
    use eframe::egui::{FontData, FontFamily, FontInsert, FontPriority, InsertFontFamily};
    let candidates = [
        "C:/Windows/Fonts/seguisym.ttf",
        "/System/Library/Fonts/Apple Symbols.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else { return };
    let families = [FontFamily::Proportional, FontFamily::Monospace]
        .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest });
    ctx.add_font(FontInsert {
        name: "symbols".into(),
        data: FontData::from_owned(bytes),
        families: families.into(),
    });
}

/// The supervisor, when this window owns it. Dropping it stops the agents.
pub struct Embedded {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub notes: Arc<Mutex<Vec<String>>>,
}

impl Embedded {
    /// Starts a supervisor unless one is already running (e.g. `desk run` in a terminal).
    pub fn start(ws: &Workspace) -> Option<Self> {
        if supervisor::is_alive(ws) {
            return None;
        }
        let mut sup = Supervisor::start(ws.clone()).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let (s, n) = (stop.clone(), notes.clone());
        let thread = std::thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let new = sup.tick();
                if !new.is_empty() {
                    let time = desk_core::fsutil::now_rfc3339();
                    let mut log = n.lock().unwrap();
                    log.extend(new.into_iter().map(|l| format!("{} {l}", &time[11..19])));
                    let excess = log.len().saturating_sub(500);
                    log.drain(..excess);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            sup.shutdown();
        });
        Some(Embedded { stop, thread: Some(thread), notes })
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
