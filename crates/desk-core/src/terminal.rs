//! An agent's CLI running interactively in a pseudo-terminal, the way you'd
//! run it yourself. desk types its messages in; the GUI draws the screen and
//! lets you type too. This is for CLIs whose headless mode doesn't work, and
//! for watching or steering a session live.
//!
//! Behind the `terminal` feature: only desk-gui hosts terminals.

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, Term, TermMode, test::TermSize};
use alacritty_terminal::tty;
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

pub use alacritty_terminal;

pub type Screen = Term<Listener>;

/// Called whenever any terminal has new output: the GUI repaints.
static ON_OUTPUT: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub fn on_output(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_OUTPUT.set(Box::new(f));
}

/// Receives the emulator's events.
#[derive(Clone)]
pub struct Listener(Arc<Shared>);

struct Shared {
    exited: AtomicBool,
    /// For replies the program asks the terminal for (cursor position...).
    sender: OnceLock<EventLoopSender>,
    size: Mutex<WindowSize>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.reply(text.into_bytes()),
            Event::TextAreaSizeRequest(format) => {
                let size = *self.0.size.lock().unwrap();
                self.reply(format(size).into_bytes());
            }
            Event::ChildExit(_) | Event::Exit => self.0.exited.store(true, Ordering::SeqCst),
            _ => {}
        }
        if let Some(f) = ON_OUTPUT.get() {
            f();
        }
    }
}

impl Listener {
    fn reply(&self, bytes: Vec<u8>) {
        if let Some(tx) = self.0.sender.get() {
            let _ = tx.send(Msg::Input(bytes.into()));
        }
    }
}

pub struct Terminal {
    pub screen: Arc<FairMutex<Screen>>,
    shared: Arc<Shared>,
    pid: Option<u32>,
    pub started: Instant,
}

const COLS: u16 = 120;
const LINES: u16 = 36;

impl Terminal {
    /// Starts `program` in a new pseudo-terminal in `dir`.
    pub fn spawn(program: &Path, args: &[String], dir: &Path, env: &[(String, String)]) -> Result<Self> {
        let size = WindowSize { num_lines: LINES, num_cols: COLS, cell_width: 8, cell_height: 16 };
        // A ConPTY can't start a .cmd (npm's shims) directly; cmd.exe can.
        let batch = program
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
        let shell = if cfg!(windows) && batch {
            let mut a = vec!["/c".to_string(), program.display().to_string()];
            a.extend(args.iter().cloned());
            tty::Shell::new("cmd.exe".into(), a)
        } else {
            tty::Shell::new(program.display().to_string(), args.to_vec())
        };
        let options = tty::Options {
            shell: Some(shell),
            working_directory: Some(dir.to_path_buf()),
            env: env.iter().cloned().collect(),
            ..tty::Options::default()
        };
        let pty = tty::new(&options, size, 0)?;
        #[cfg(windows)]
        let pid = pty.child_watcher().pid().map(|p| p.get());
        #[cfg(unix)]
        let pid = Some(pty.child().id());
        if let Some(pid) = pid {
            crate::process::adopt(pid);
        }

        let shared = Arc::new(Shared {
            exited: AtomicBool::new(false),
            sender: OnceLock::new(),
            size: Mutex::new(size),
        });
        let listener = Listener(shared.clone());
        let config = term::Config { scrolling_history: 5000, ..term::Config::default() };
        let screen = Term::new(config, &TermSize::new(COLS as usize, LINES as usize), listener.clone());
        let screen = Arc::new(FairMutex::new(screen));
        let event_loop = EventLoop::new(screen.clone(), listener, pty, false, false)?;
        let _ = shared.sender.set(event_loop.channel());
        event_loop.spawn();
        Ok(Terminal { screen, shared, pid, started: Instant::now() })
    }

    /// Bytes as if typed.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        if let Some(tx) = self.shared.sender.get() {
            let _ = tx.send(Msg::Input(bytes.into().into()));
        }
    }

    /// Text as if pasted: one block, newlines kept, when the program supports
    /// bracketed paste. Otherwise newlines would submit early, so they become spaces.
    pub fn paste(&self, text: &str) {
        if self.screen.lock().mode().contains(TermMode::BRACKETED_PASTE) {
            let body = text.replace("\r\n", "\r").replace('\n', "\r");
            self.write(format!("\x1b[200~{body}\x1b[201~"));
        } else {
            self.write(text.replace(['\r', '\n'], " "));
        }
    }

    pub fn resize(&self, cols: u16, lines: u16, cell_width: u16, cell_height: u16) {
        let size = WindowSize { num_lines: lines.max(2), num_cols: cols.max(10), cell_width, cell_height };
        let mut old = self.shared.size.lock().unwrap();
        if (old.num_cols, old.num_lines) == (size.num_cols, size.num_lines) {
            return;
        }
        *old = size;
        self.screen.lock().resize(TermSize::new(size.num_cols as usize, size.num_lines as usize));
        if let Some(tx) = self.shared.sender.get() {
            let _ = tx.send(Msg::Resize(size));
        }
    }

    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::SeqCst)
    }

    /// The visible screen as text, one line per row, trailing spaces trimmed.
    pub fn text(&self) -> String {
        let screen = self.screen.lock();
        let grid = screen.grid();
        let mut out = String::new();
        for line in 0..grid.screen_lines() {
            let row = &grid[Line(line as i32)];
            let text: String = (0..grid.columns()).map(|c| row[Column(c)].c).collect();
            out.push_str(text.trim_end());
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    /// Ends the program and everything it started.
    pub fn shutdown(&self) {
        if let Some(tx) = self.shared.sender.get() {
            let _ = tx.send(Msg::Shutdown);
        }
        if let Some(pid) = self.pid {
            crate::process::kill_tree(pid);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}
