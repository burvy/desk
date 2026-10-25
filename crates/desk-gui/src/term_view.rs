//! Draws a terminal agent's screen and sends it your keys, clicks and
//! scrolling: the agent's CLI, live, inside the window.

use desk_core::terminal::alacritty_terminal::grid::Scroll;
use desk_core::terminal::alacritty_terminal::term::TermMode;
use desk_core::terminal::alacritty_terminal::term::cell::Flags;
use desk_core::terminal::alacritty_terminal::term::color::Colors;
use desk_core::terminal::alacritty_terminal::vte::ansi::{Color, NamedColor};
use desk_core::terminal::{Screen, Terminal};
use eframe::egui::{self, Color32, FontId, Key, Pos2, Rect, Sense, Ui, Vec2};
use std::sync::Arc;

const FONT_SIZE: f32 = 13.0;

/// Draws the terminal in all the space left in `ui`.
pub fn show(ui: &mut Ui, term: &Arc<Terminal>) {
    let font = FontId::monospace(FONT_SIZE);
    let (cell_w, cell_h) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let size = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let cols = (rect.width() / cell_w).floor().max(10.0) as u16;
    let lines = (rect.height() / cell_h).floor().max(2.0) as u16;
    term.resize(cols, lines, cell_w as u16, cell_h as u16);

    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let focused = response.has_focus();
    if focused {
        // Keep Tab, arrows and Escape for the terminal, not egui's focus moves.
        let filter =
            egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true };
        ui.memory_mut(|m| m.set_focus_lock_filter(response.id, filter));
        input(ui, term, rect, cell_w, cell_h);
    }
    if response.hovered() {
        let lines: f32 = ui.input(|i| {
            i.events
                .iter()
                .map(|e| match e {
                    egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta, .. } => delta.y,
                    egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, .. } => {
                        delta.y / cell_h
                    }
                    egui::Event::MouseWheel { delta, .. } => delta.y * lines as f32,
                    _ => 0.0,
                })
                .sum()
        });
        if lines.abs() >= 0.5 {
            scroll(term, lines.round() as i32);
        }
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, BACKGROUND);
    let screen = term.screen.lock();
    draw(&painter, &screen, rect.min, cell_w, cell_h, &font, focused);
    drop(screen);
    if !focused {
        painter.text(
            rect.right_top() + Vec2::new(-6.0, 4.0),
            egui::Align2::RIGHT_TOP,
            "click to type",
            FontId::proportional(11.0),
            Color32::from_gray(120),
        );
    }
    if term.exited() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "the CLI exited; desk restarts it",
            FontId::proportional(14.0),
            Color32::from_gray(180),
        );
    }
}

fn draw(
    painter: &egui::Painter,
    screen: &Screen,
    origin: Pos2,
    cw: f32,
    ch: f32,
    font: &FontId,
    focused: bool,
) {
    let content = screen.renderable_content();
    let offset = content.display_offset as i32;
    let colors = content.colors;
    let cursor = content.cursor.point;
    let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR) && offset == 0;
    for cell in content.display_iter {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = cell.point.line.0 + offset;
        let pos = origin + Vec2::new(cell.point.column.0 as f32 * cw, row as f32 * ch);
        let width = if cell.flags.contains(Flags::WIDE_CHAR) { cw * 2.0 } else { cw };
        let mut fg = color(cell.fg, colors, cell.flags);
        let mut bg = color(cell.bg, colors, Flags::empty());
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let is_cursor = show_cursor && cell.point == cursor;
        if is_cursor && focused {
            std::mem::swap(&mut fg, &mut bg);
            if bg == BACKGROUND {
                bg = FOREGROUND;
                fg = BACKGROUND;
            }
        }
        if bg != BACKGROUND {
            painter.rect_filled(Rect::from_min_size(pos, Vec2::new(width + 0.5, ch + 0.5)), 0.0, bg);
        }
        if is_cursor && !focused {
            painter.rect_stroke(
                Rect::from_min_size(pos, Vec2::new(width, ch)),
                0.0,
                egui::Stroke::new(1.0, FOREGROUND),
                egui::StrokeKind::Inside,
            );
        }
        if cell.c != ' ' && cell.c != '\t' {
            let mut text = cell.c.to_string();
            if let Some(extra) = cell.zerowidth() {
                text.extend(extra);
            }
            painter.text(pos + Vec2::new(width / 2.0, 0.0), egui::Align2::CENTER_TOP, text, font.clone(), fg);
        }
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            let y = pos.y + ch - 1.0;
            painter
                .line_segment([Pos2::new(pos.x, y), Pos2::new(pos.x + width, y)], egui::Stroke::new(1.0, fg));
        }
    }
}

fn input(ui: &Ui, term: &Terminal, rect: Rect, cw: f32, ch: f32) {
    let mode = *term.screen.lock().mode();
    let events = ui.input(|i| i.events.clone());
    for event in events {
        match event {
            egui::Event::Text(t) => term.write(t.into_bytes()),
            egui::Event::Paste(t) => term.paste(&t),
            // Ctrl+C and Ctrl+X reach us as copy/cut; in a terminal they're keys.
            egui::Event::Copy => term.write(vec![0x03]),
            egui::Event::Cut => term.write(vec![0x18]),
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                if let Some(bytes) = key_bytes(key, modifiers, mode.contains(TermMode::APP_CURSOR)) {
                    term.write(bytes);
                }
            }
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, .. }
                if mode.contains(TermMode::SGR_MOUSE) && mode.intersects(TermMode::MOUSE_MODE) =>
            {
                let col = ((pos.x - rect.min.x) / cw).max(0.0) as usize + 1;
                let row = ((pos.y - rect.min.y) / ch).max(0.0) as usize + 1;
                term.write(format!("\x1b[<0;{col};{row}{}", if pressed { 'M' } else { 'm' }));
            }
            _ => {}
        }
    }
}

/// Scroll: the program's own scrolling if it asked for the mouse, arrow keys
/// on a full-screen app, else the terminal's scrollback.
fn scroll(term: &Terminal, lines: i32) {
    let mode = *term.screen.lock().mode();
    if mode.intersects(TermMode::MOUSE_MODE) && mode.contains(TermMode::SGR_MOUSE) {
        let button = if lines > 0 { 64 } else { 65 };
        for _ in 0..lines.abs().min(5) {
            term.write(format!("\x1b[<{button};1;1M"));
        }
    } else if mode.contains(TermMode::ALT_SCREEN) {
        let key = if lines > 0 { "\x1b[A" } else { "\x1b[B" };
        term.write(key.repeat(lines.unsigned_abs().min(5) as usize));
    } else {
        term.screen.lock().scroll_display(Scroll::Delta(lines));
    }
}

fn key_bytes(key: Key, m: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: char| {
        if m.shift || m.ctrl || m.alt {
            let n = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
            format!("\x1b[1;{n}{c}")
        } else if app_cursor {
            format!("\x1bO{c}")
        } else {
            format!("\x1b[{c}")
        }
    };
    let s = match key {
        Key::Enter if m.shift => "\x1b\r".to_string(), // a newline in most agent TUIs
        Key::Enter => "\r".into(),
        Key::Tab if m.shift => "\x1b[Z".into(),
        Key::Tab => "\t".into(),
        Key::Backspace if m.ctrl => "\x17".into(),
        Key::Backspace => "\x7f".into(),
        Key::Escape => "\x1b".into(),
        Key::ArrowUp => arrow('A'),
        Key::ArrowDown => arrow('B'),
        Key::ArrowRight => arrow('C'),
        Key::ArrowLeft => arrow('D'),
        Key::Home => "\x1b[H".into(),
        Key::End => "\x1b[F".into(),
        Key::PageUp => "\x1b[5~".into(),
        Key::PageDown => "\x1b[6~".into(),
        Key::Delete => "\x1b[3~".into(),
        Key::Insert => "\x1b[2~".into(),
        // Ctrl+letter → its control code. (Plain letters arrive as Text.)
        k if m.ctrl && !m.alt => {
            let name = k.name();
            let c = name.chars().next().filter(|c| name.len() == 1 && c.is_ascii_alphabetic())?;
            return Some(vec![(c.to_ascii_uppercase() as u8) - b'A' + 1]);
        }
        _ => return None,
    };
    Some(s.into_bytes())
}

const FOREGROUND: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xd8);
const BACKGROUND: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);

/// The 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan, white, then bright.
const ANSI: [u32; 16] = [
    0x181818, 0xd0605a, 0x90b45a, 0xe0b860, 0x6a9fd5, 0xb47fc0, 0x6cc0b8, 0xd0d0d0, 0x6b6b6b, 0xe87870,
    0xaad078, 0xf4d488, 0x8ab8f0, 0xcc98d8, 0x90dcd0, 0xf8f8f8,
];

fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

fn color(c: Color, colors: &Colors, flags: Flags) -> Color32 {
    let base = match c {
        Color::Spec(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => match colors[i as usize] {
            Some(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
            None => indexed(i),
        },
        Color::Named(n) => match colors[n] {
            Some(rgb) => Color32::from_rgb(rgb.r, rgb.g, rgb.b),
            None => named(n, flags),
        },
    };
    if flags.intersects(Flags::DIM | Flags::DIM_BOLD) { base.gamma_multiply(0.65) } else { base }
}

fn named(n: NamedColor, flags: Flags) -> Color32 {
    use NamedColor::*;
    let bold = flags.contains(Flags::BOLD);
    match n {
        Foreground | BrightForeground => FOREGROUND,
        Background => BACKGROUND,
        Cursor => FOREGROUND,
        DimForeground => FOREGROUND.gamma_multiply(0.65),
        other => {
            let i = other as usize;
            match i {
                0..=7 => rgb(ANSI[i + if bold { 8 } else { 0 }]),
                8..=15 => rgb(ANSI[i]),
                // Dim variants come after the 16 + foreground/background/cursor slots.
                _ => rgb(ANSI[(i.saturating_sub(259)) % 8]).gamma_multiply(0.65),
            }
        }
    }
}

/// xterm's 256 colors: 16 ANSI, a 6×6×6 cube, then 24 greys.
fn indexed(i: u8) -> Color32 {
    match i {
        0..=15 => rgb(ANSI[i as usize]),
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => Color32::from_gray(8 + (i - 232) * 10),
    }
}
