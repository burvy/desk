//! desk-gui: a window onto a desk workspace. It reads the same files the
//! CLI does and writes through desk-core, so it's only ever a view. If no
//! supervisor is running, it runs one in a background thread.

// A window, not a console program: no terminal pops up behind it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent_form;
mod app;
mod home;
mod snapshot;

use desk_core::Workspace;
use desk_core::supervisor::{self, Supervisor};
use eframe::egui::{self, Ui};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

fn main() -> eframe::Result {
    // A path argument, else the workspace around the current folder, else the last one used.
    let ws = match std::env::args().nth(1) {
        Some(p) => Workspace::open(Path::new(&p)).ok(),
        None => Workspace::find(&std::env::current_dir().unwrap_or_default()).ok(),
    }
    .or_else(|| home::recent().iter().find_map(|p| Workspace::open(p).ok()));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("desk")
            .with_icon(icon())
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "desk",
        options,
        Box::new(move |cc| {
            symbol_fallback(&cc.egui_ctx);
            let mut root = Root { home: home::Home::default(), open: None };
            if let Some(ws) = ws {
                root.open(&cc.egui_ctx, ws);
            }
            Ok(Box::new(root))
        }),
    )
}

/// The start screen, or an open workspace.
struct Root {
    home: home::Home,
    open: Option<app::App>,
}

impl Root {
    fn open(&mut self, ctx: &egui::Context, ws: Workspace) {
        home::remember(&ws.root);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("desk — {}", ws.root.display())));
        self.open = Some(app::App::new(ws));
    }
}

impl eframe::App for Root {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        if let Some(app) = &mut self.open {
            app.ui(ui);
            if app.close_requested {
                self.open = None; // drops its supervisor, which stops the agents
                self.home = home::Home::default();
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title("desk".into()));
            }
        } else if let Some(ws) = self.home.ui(ui) {
            self.open(&ui.ctx().clone(), ws);
        }
    }
}

/// egui's bundled fonts lack symbols like ● ⚙ →; borrow the system's symbol font.
fn symbol_fallback(ctx: &egui::Context) {
    use egui::{FontData, FontFamily, FontInsert, FontPriority, InsertFontFamily};
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

/// The window icon, drawn here rather than shipped as an image: a desk
/// (a top and two legs) on a blue tile.
fn icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let (cx, cy) = ((x as i32 - 31).abs(), (y as i32 - 31).abs());
            // A rounded square: outside the corners' circles stays transparent.
            let (dx, dy) = ((cx - 20).max(0), (cy - 20).max(0));
            if dx * dx + dy * dy > 11 * 11 {
                continue;
            }
            let top = (22..30).contains(&y) && (12..52).contains(&x);
            let leg = (30..46).contains(&y) && ((16..22).contains(&x) || (42..48).contains(&x));
            let px = if top || leg { [245, 245, 240, 255] } else { [70, 115, 200, 255] };
            rgba[(y * N + x) * 4..][..4].copy_from_slice(&px);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

/// Runs a command without flashing a console window (we're a window app).
pub fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
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
