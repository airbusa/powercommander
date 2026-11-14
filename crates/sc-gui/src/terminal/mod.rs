//! The terminal under the panels: the shell on alacritty_terminal, drawn as
//! Zed's terminal draws it (crates/terminal_view, GPL-3.0-or-later).

mod keys;
mod mouse;

use std::borrow::Cow;
use std::io;
use std::path::Path;
use std::sync::Arc;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::menu::ContextMenuExt;
use gpui_kit::*;

/// The shell has ended.
pub struct Exited;


actions!(terminal, [SendTab, SendBackTab, Interrupt, CopySelection, Paste, NewTerminalTab, CloseTerminalTab, NextTerminalTab, PreviousTerminalTab]);

/// The terminal's own tab keys, Konsole's and GNOME Terminal's (Terminal.app's on macOS).
#[cfg(not(target_os = "macos"))]
const TAB_KEYS: [&str; 4] = ["ctrl-shift-t", "ctrl-shift-w", "ctrl-pagedown", "ctrl-pageup"];
#[cfg(target_os = "macos")]
const TAB_KEYS: [&str; 4] = ["cmd-t", "cmd-w", "cmd-shift-]", "cmd-shift-["];

/// Keys the terminal keeps, whatever the app's commands are bound to.
pub fn own_key(key: &str) -> bool {
    let key = key.replace("secondary-", if cfg!(target_os = "macos") { "cmd-" } else { "ctrl-" });
    TAB_KEYS.contains(&key.as_str()) || ["tab", "shift-tab", "ctrl-c", "cmd-c", "ctrl-shift-c", "ctrl-shift-v", "cmd-v", "ctrl-insert", "shift-insert"].contains(&key.as_str())
}

/// Keys GPUI Kit's root takes (Tab moves the focus, Ctrl+C copies) go to the shell here.
pub fn bind_keys(cx: &mut App) {
    let context = Some("Terminal");
    let [new, close, next, previous] = TAB_KEYS;
    cx.bind_keys([
        KeyBinding::new("tab", SendTab, context),
        KeyBinding::new("shift-tab", SendBackTab, context),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Interrupt, context),
        // A terminal's copy and paste keys; the last of each is the one the right-click menu shows.
        KeyBinding::new("ctrl-insert", CopySelection, context),
        KeyBinding::new("shift-insert", Paste, context),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-c", CopySelection, context),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-v", Paste, context),
        // Windows Terminal's (Ctrl+C copies what is selected: `interrupt`).
        #[cfg(windows)]
        KeyBinding::new("ctrl-v", Paste, context),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", CopySelection, context),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-v", Paste, context),
        KeyBinding::new(new, NewTerminalTab, context),
        KeyBinding::new(close, CloseTerminalTab, context),
        KeyBinding::new(next, NextTerminalTab, context),
        KeyBinding::new(previous, PreviousTerminalTab, context),
    ]);
}

#[derive(Clone)]
struct Listener(UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

/// Columns and lines shown.
#[derive(Clone, Copy, PartialEq)]
struct Cells {
    columns: usize,
    lines: usize,
}

impl Dimensions for Cells {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    pty: EventLoopSender,
    pub focus: FocusHandle,
    cells: Cells,
    /// A cell's size, and where the first one is on screen.
    cell: Size<Pixels>,
    origin: Point<Pixels>,
    /// The theme's, for what a program asks of the colours.
    foreground: Hsla,
    background: Hsla,
    dark: bool,
    /// The wheel's pixels not yet a whole line.
    wheel: f32,
    /// The cell a move was last reported at.
    reported: Option<(usize, usize)>,
    /// What the shell calls itself ("user@host: ~/src").
    pub title: Option<String>,
    /// The pty and the shell's process: whether the shell waits at its prompt.
    #[cfg(unix)]
    shell: (i32, i32),
    /// The shell's process group, as it first stood in front: under macOS's
    /// login it is not the started process's.
    #[cfg(unix)]
    shell_group: Option<i32>,
    _events: Task<()>,
    _focus: [Subscription; 2],
}

impl EventEmitter<Exited> for TerminalView {}

impl Drop for TerminalView {
    fn drop(&mut self) {
        // The loop ends, and its pty hangs up on the shell.
        let _ = self.pty.send(Msg::Shutdown);
    }
}

impl TerminalView {
    /// The user's shell in `dir`, or `program`.
    pub fn open(dir: &Path, program: Option<(String, Vec<String>)>, window: &mut Window, cx: &mut App) -> io::Result<Entity<Self>> {
        let cells = Cells { columns: 80, lines: 24 };
        let (sender, mut events) = unbounded();
        let listener = Listener(sender);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &cells, listener.clone())));
        let env = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];
        let options = tty::Options {
            shell: program.map(|(program, args)| tty::Shell::new(program, args)),
            working_directory: Some(dir.to_owned()),
            #[cfg(windows)]
            escape_args: true,
            env: env.map(|(name, value)| (name.to_owned(), value.to_owned())).into(),
            ..Default::default()
        };
        let cell = size(px(8.), px(16.));
        let pty = tty::new(&options, window_size(cells, cell), 0)?;
        #[cfg(unix)]
        let shell = (std::os::fd::AsRawFd::as_raw_fd(pty.file()), pty.child().id() as i32);
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let pty = event_loop.channel();
        event_loop.spawn();
        Ok(cx.new(|cx| {
            let focus = cx.focus_handle();
            let events = cx.spawn(async move |this, cx| {
                while let Some(event) = events.next().await {
                    if this.update(cx, |this: &mut Self, cx| this.on_event(event, cx)).is_err() {
                        break;
                    }
                }
            });
            let focus_in = cx.on_focus_in(&focus, window, |this, _, cx| this.focused(true, cx));
            let focus_out = cx.on_focus_out(&focus, window, |this, _, _, cx| this.focused(false, cx));
            Self {
                term,
                pty,
                focus,
                cells,
                cell,
                origin: Point::default(),
                foreground: Hsla::default(),
                background: Hsla::default(),
                dark: true,
                wheel: 0.,
                reported: None,
                title: None,
                #[cfg(unix)]
                shell,
                #[cfg(unix)]
                shell_group: None,
                _events: events,
                _focus: [focus_in, focus_out],
            }
        }))
    }

    /// Sends `text` as typed, as the command line's commands go.
    pub fn send(&mut self, text: &str) {
        self.input(text.as_bytes().to_vec());
    }

    /// The shell waits at its prompt, nothing running in front of it; none
    /// where that cannot be told (Windows).
    pub fn idle(&self) -> Option<bool> {
        #[cfg(unix)]
        // SAFETY: a plain query of the pty, which the event loop keeps open.
        return Some(unsafe { libc::tcgetpgrp(self.shell.0) } == self.shell_group.unwrap_or(self.shell.1));
        #[cfg(not(unix))]
        None
    }

    fn on_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            TermEvent::Wakeup => {
                // The shell's group, as it first stands in front with output.
                #[cfg(unix)]
                if self.shell_group.is_none() {
                    // SAFETY: a plain query of the pty, which the event loop keeps open.
                    let group = unsafe { libc::tcgetpgrp(self.shell.0) };
                    self.shell_group = (group > 0).then_some(group);
                }
                cx.notify();
            }
            TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => cx.notify(),
            TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
            TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
                self.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(index, format) => {
                let set = self.term.lock().colors()[index];
                let rgb = set.unwrap_or_else(|| self.default_rgb(index));
                self.write(format(rgb).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => self.write(format(window_size(self.cells, self.cell)).into_bytes()),
            TermEvent::Exit | TermEvent::ChildExit(_) => cx.emit(Exited),
            TermEvent::Title(title) => {
                self.title = Some(title).filter(|title| !title.trim().is_empty());
                cx.notify();
            }
            TermEvent::ResetTitle => {
                self.title = None;
                cx.notify();
            }
            TermEvent::Bell => {}
        }
    }

    fn default_rgb(&self, index: usize) -> Rgb {
        let color = match index {
            0..=255 => rgb(ansi(index as u8, self.dark)).into(),
            i if i == NamedColor::Background as usize => self.background,
            _ => self.foreground,
        };
        let color = Rgba::from(color);
        let byte = |part: f32| (part * 255.).round() as u8;
        Rgb { r: byte(color.r), g: byte(color.g), b: byte(color.b) }
    }

    fn write(&self, bytes: Vec<u8>) {
        let _ = self.pty.send(Msg::Input(Cow::Owned(bytes)));
    }

    /// What the user types: the view goes back to the bottom, the selection away.
    fn input(&mut self, bytes: Vec<u8>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
    }

    fn focused(&mut self, on: bool, cx: &mut Context<Self>) {
        let mut term = self.term.lock();
        term.is_focused = on;
        if term.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write(if on { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
        }
        cx.notify();
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// Ctrl+C: on Windows a selection is copied, as Windows Terminal does.
    fn interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.term.lock().selection.as_ref().is_some_and(|selection| !selection.is_empty());
        match cfg!(windows) && selected {
            true => {
                self.copy(cx);
                self.term.lock().selection = None;
                cx.notify();
            }
            false => self.input(b"\x03".to_vec()),
        }
    }

    fn copy(&self, cx: &mut App) {
        if let Some(text) = self.term.lock().selection_to_string().filter(|text| !text.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste_clipboard(&mut self, cx: &mut App) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste(&text);
        }
    }

    fn paste(&mut self, text: &str) {
        let text = match self.mode().contains(TermMode::BRACKETED_PASTE) {
            true => format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")),
            false => text.replace("\r\n", "\r").replace('\n', "\r"),
        };
        self.input(text.into_bytes());
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let (key, held) = (keystroke.key.to_ascii_lowercase(), keystroke.modifiers);
        match key.as_str() {
            "pageup" | "pagedown" if held.shift && !self.mode().contains(TermMode::ALT_SCREEN) => {
                self.term.lock().scroll_display(if key == "pageup" { Scroll::PageUp } else { Scroll::PageDown });
                cx.notify();
            }
            _ => {
                // AltGr's characters come as text.
                if event.prefer_character_input && keystroke.key_char.is_some() {
                    return;
                }
                let Some(bytes) = keys::to_esc_str(keystroke, self.mode(), false) else {
                    return;
                };
                self.input(bytes.into_owned().into_bytes());
            }
        }
        cx.stop_propagation();
    }

    /// The screen's column and line at `position`, and which half of the cell.
    fn cell_at(&self, position: Point<Pixels>) -> (usize, usize, Side) {
        let x = ((position.x - self.origin.x) / self.cell.width).max(0.);
        let y = ((position.y - self.origin.y) / self.cell.height).max(0.);
        let column = (x as usize).min(self.cells.columns - 1);
        let line = (y as usize).min(self.cells.lines - 1);
        (column, line, if x.fract() > 0.5 { Side::Right } else { Side::Left })
    }

    /// The grid's point for the screen's `line`, scrolled back or not.
    fn grid_point(term: &Term<Listener>, column: usize, line: usize) -> GridPoint {
        GridPoint::new(Line(line as i32 - term.grid().display_offset() as i32), Column(column))
    }

    /// Reported to the program when it asked; Shift keeps the mouse ours.
    fn report(&mut self, event: mouse::Event, position: Point<Pixels>, modifiers: Modifiers) -> bool {
        let (column, line, _) = self.cell_at(position);
        if modifiers.shift {
            return false;
        }
        match mouse::report(event, column, line, modifiers, self.mode()) {
            Some(bytes) => {
                self.write(bytes);
                true
            }
            None => false,
        }
    }

    /// Reported to a program that takes the mouse (mc, htop), the menu does not open.
    fn right_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Right {
            return;
        }
        window.focus(&self.focus, cx);
        if self.report(mouse::Event::Button(event.button, true), event.position, event.modifiers) {
            cx.stop_propagation();
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if self.report(mouse::Event::Button(event.button, true), event.position, event.modifiers) {
            return;
        }
        match event.button {
            MouseButton::Left => {
                let (column, line, side) = self.cell_at(event.position);
                let kind = match event.click_count {
                    2 => SelectionType::Semantic,
                    3.. => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                let mut term = self.term.lock();
                let point = Self::grid_point(&term, column, line);
                term.selection = Some(Selection::new(kind, point, side));
                drop(term);
                cx.notify();
            }
            // X11's: the middle button pastes what was last selected anywhere.
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            MouseButton::Middle => {
                if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                    self.paste(&text);
                }
            }
            _ => {}
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.report(mouse::Event::Button(event.button, false), event.position, event.modifiers) {
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if event.button == MouseButton::Left
            && let Some(text) = self.term.lock().selection_to_string().filter(|text| !text.is_empty())
        {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
        let _ = cx;
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (column, line, side) = self.cell_at(event.position);
        if self.reported != Some((column, line)) && self.report(mouse::Event::Move(event.pressed_button), event.position, event.modifiers) {
            self.reported = Some((column, line));
            return;
        }
        if event.pressed_button == Some(MouseButton::Left) {
            let mut term = self.term.lock();
            let point = Self::grid_point(&term, column, line);
            if let Some(selection) = &mut term.selection {
                selection.update(point, side);
                drop(term);
                cx.notify();
            }
        }
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => {
                self.wheel += delta.y / self.cell.height;
                let whole = self.wheel.trunc();
                self.wheel -= whole;
                whole
            }
        } as i32;
        if lines == 0 {
            return;
        }
        let mode = self.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !event.modifiers.shift {
            for _ in 0..lines.abs() {
                self.report(mouse::Event::Wheel(lines > 0), event.position, event.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            // A pager's or an editor's own scrolling: arrow keys.
            let arrow: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
            self.write(arrow.repeat(lines.unsigned_abs() as usize));
        } else {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    /// Where the grid is and how big, from the space it got.
    fn place(&mut self, origin: Point<Pixels>, cell: Size<Pixels>, cells: Cells, colors: (Hsla, Hsla, bool)) {
        (self.origin, self.cell) = (origin, cell);
        (self.foreground, self.background, self.dark) = colors;
        if cells != self.cells {
            self.cells = cells;
            self.term.lock().resize(cells);
            let _ = self.pty.send(Msg::Resize(window_size(cells, cell)));
        }
    }
}

fn window_size(cells: Cells, cell: Size<Pixels>) -> WindowSize {
    WindowSize {
        num_lines: cells.lines as u16,
        num_cols: cells.columns as u16,
        cell_width: f32::from(cell.width) as u16,
        cell_height: f32::from(cell.height) as u16,
    }
}

/// The 256 colours: VS Code's terminal's first 16, then xterm's cube and greys.
fn ansi(index: u8, dark: bool) -> u32 {
    const DARK: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea,
        0xd670d6, 0x29b8db, 0xe5e5e5,
    ];
    const LIGHT: [u32; 16] = [
        0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, 0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5,
        0xbc05bc, 0x0598bc, 0xa5a5a5,
    ];
    match index {
        0..16 => (if dark { DARK } else { LIGHT })[index as usize],
        16..232 => {
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
            let i = index - 16;
            level(i / 36) << 16 | level(i / 6 % 6) << 8 | level(i % 6)
        }
        _ => {
            let grey = 8 + 10 * (index - 232) as u32;
            grey << 16 | grey << 8 | grey
        }
    }
}

/// What the grid's cells come to on screen.
struct Layout {
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Option<(Bounds<Pixels>, CursorShape, Option<ShapedLine>)>,
    cursor_color: Hsla,
    line_height: Pixels,
    focused: bool,
}

/// A run of cells in one style.
struct Run {
    line: usize,
    column: usize,
    cells: usize,
    text: String,
    style: TextRun,
    /// A wide character's: nothing joins it.
    alone: bool,
}

impl Run {
    fn same(&self, style: &TextRun) -> bool {
        (self.style.font == style.font && self.style.color == style.color)
            && (self.style.underline == style.underline && self.style.strikethrough == style.strikethrough)
    }
}

fn prepaint(view: &Entity<TerminalView>, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) -> Layout {
    let theme = cx.theme();
    let (fore, back, selection, dark) = (theme.foreground, theme.background, theme.selection, theme.mode.is_dark());
    let (base, font_size) = (font(theme.mono_font_family.clone()), theme.mono_font_size);
    let text = window.text_system().clone();
    let width = text.advance(text.resolve_font(&base), font_size, 'm').map_or(font_size * 0.6, |advance| advance.width);
    let cell = size(width, (font_size * 1.3).round());
    let cells = Cells {
        columns: ((bounds.size.width / cell.width) as usize).max(2),
        lines: ((bounds.size.height / cell.height) as usize).max(1),
    };
    view.update(cx, |view, _| view.place(bounds.origin, cell, cells, (fore, back, dark)));
    let term = view.read(cx).term.clone();
    // The window's own focus: a tab switched to has its focus-in heard only later.
    let focused = view.read(cx).focus.is_focused(window);
    let term = term.lock();
    let content = term.renderable_content();
    let set = content.colors;
    let color = |color: Color| -> Hsla {
        let index = match color {
            Color::Spec(spec) => return rgb((spec.r as u32) << 16 | (spec.g as u32) << 8 | spec.b as u32).into(),
            Color::Named(named) => named as usize,
            Color::Indexed(index) => index as usize,
        };
        if let Some(spec) = set[index] {
            return rgb((spec.r as u32) << 16 | (spec.g as u32) << 8 | spec.b as u32).into();
        }
        let dim_black = NamedColor::DimBlack as usize;
        match index {
            0..=255 => rgb(ansi(index as u8, dark)).into(),
            i if i == NamedColor::Background as usize => back,
            i if (dim_black..dim_black + 8).contains(&i) => Hsla::from(rgb(ansi((i - dim_black) as u8, dark))).opacity(0.7),
            i if i == NamedColor::DimForeground as usize => fore.opacity(0.7),
            _ => fore,
        }
    };
    let at = |line: usize, column: usize| point(bounds.origin.x + cell.width * column as f32, bounds.origin.y + cell.height * line as f32);
    let offset = content.display_offset as i32;
    let mut layout = Layout { rects: Vec::new(), lines: Vec::new(), cursor: None, cursor_color: fore, line_height: cell.height, focused };
    let mut runs: Vec<Run> = Vec::new();
    for indexed in content.display_iter {
        let Ok(line) = usize::try_from(indexed.point.line.0 + offset) else {
            continue;
        };
        let column = indexed.point.column.0;
        let flags = indexed.cell.flags;
        let (mut fg, mut bg) = (indexed.cell.fg, indexed.cell.bg);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let wide = flags.contains(Flags::WIDE_CHAR);
        let mut fill = (bg != Color::Named(NamedColor::Background)).then(|| color(bg));
        if content.selection.is_some_and(|range| range.contains(indexed.point)) {
            fill = Some(selection);
        }
        if let Some(fill) = fill {
            let width = cell.width * if wide { 2. } else { 1. };
            match layout.rects.last_mut() {
                // The cell before, in the same colour: one rectangle.
                Some((last, same)) if *same == fill && last.origin.y == at(line, 0).y && last.right() == at(line, column).x => last.size.width += width,
                _ => layout.rects.push((Bounds::new(at(line, column), size(width, cell.height)), fill)),
            }
        }
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let mut foreground = color(fg);
        if flags.contains(Flags::DIM) {
            foreground = foreground.opacity(0.7);
        }
        let style = TextRun {
            len: 0,
            font: Font {
                weight: if flags.contains(Flags::BOLD) { FontWeight::BOLD } else { base.weight },
                style: if flags.contains(Flags::ITALIC) { FontStyle::Italic } else { FontStyle::Normal },
                ..base.clone()
            },
            color: foreground,
            background_color: None,
            underline: flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
                color: Some(foreground),
                thickness: px(1.),
                wavy: flags.contains(Flags::UNDERCURL),
            }),
            strikethrough: flags.contains(Flags::STRIKEOUT).then(|| StrikethroughStyle { color: Some(foreground), thickness: px(1.) }),
        };
        let c = if flags.contains(Flags::HIDDEN) { ' ' } else { indexed.cell.c };
        match runs.last_mut() {
            // A wide character runs alone: each glyph takes one cell's width.
            Some(run) if !wide && !run.alone && run.line == line && run.column + run.cells == column && run.same(&style) => {
                run.text.push(c);
                run.cells += 1;
            }
            _ => runs.push(Run { line, column, cells: 1, text: c.to_string(), style, alone: wide }),
        }
        if let Some(zero_width) = indexed.cell.zerowidth() {
            runs.last_mut().into_iter().for_each(|run| run.text.extend(zero_width));
        }
    }
    for mut run in runs {
        let decorated = run.style.underline.is_some() || run.style.strikethrough.is_some();
        if run.text.trim().is_empty() && !decorated {
            continue;
        }
        run.style.len = run.text.len();
        let shaped = text.shape_line(run.text.into(), font_size, &[run.style], Some(cell.width));
        layout.lines.push((at(run.line, run.column), shaped));
    }
    let cursor = content.cursor;
    if let Ok(line) = usize::try_from(cursor.point.line.0 + offset)
        && cursor.shape != CursorShape::Hidden
        && line < cells.lines
    {
        let under = &term.grid()[cursor.point];
        let wide = if under.flags.contains(Flags::WIDE_CHAR) { 2. } else { 1. };
        let bounds = Bounds::new(at(line, cursor.point.column.0), size(cell.width * wide, cell.height));
        // On a block, the character under it in the background's colour.
        let under = (cursor.shape == CursorShape::Block && focused && under.c != ' ').then(|| {
            let run = TextRun { len: under.c.len_utf8(), font: base.clone(), color: back, background_color: None, underline: None, strikethrough: None };
            text.shape_line(under.c.to_string().into(), font_size, &[run], Some(cell.width))
        });
        layout.cursor = Some((bounds, cursor.shape, under));
    }
    layout
}

fn paint(layout: Layout, view: Entity<TerminalView>, focus: &FocusHandle, window: &mut Window, cx: &mut App) {
    for (bounds, color) in layout.rects {
        window.paint_quad(fill(bounds, color));
    }
    for (origin, line) in layout.lines {
        let _ = line.paint(origin, layout.line_height, TextAlign::Left, None, window, cx);
    }
    let mut caret = Bounds::default();
    if let Some((bounds, shape, under)) = layout.cursor {
        caret = bounds;
        let color = layout.cursor_color;
        match shape {
            _ if !layout.focused => window.paint_quad(outline(bounds, color, BorderStyle::Solid)),
            CursorShape::Beam => window.paint_quad(fill(Bounds::new(bounds.origin, size(px(2.), bounds.size.height)), color)),
            CursorShape::Underline => {
                let origin = point(bounds.origin.x, bounds.bottom() - px(2.));
                window.paint_quad(fill(Bounds::new(origin, size(bounds.size.width, px(2.))), color))
            }
            CursorShape::HollowBlock => window.paint_quad(outline(bounds, color, BorderStyle::Solid)),
            _ => {
                window.paint_quad(fill(bounds, color));
                if let Some(under) = under {
                    let _ = under.paint(bounds.origin, layout.line_height, TextAlign::Left, None, window, cx);
                }
            }
        }
    }
    window.handle_input(focus, Typing { view, caret }, cx);
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (view, focus) = (cx.entity(), self.focus.clone());
        let (painted, menu_view, menu_focus) = (view.clone(), view.downgrade(), focus.clone());
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .px_1()
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &SendTab, _, _| this.input(b"\t".to_vec())))
            .on_action(cx.listener(|this, _: &SendBackTab, _, _| this.input(b"\x1b[Z".to_vec())))
            .on_action(cx.listener(Self::interrupt))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste_clipboard(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .capture_any_mouse_down(cx.listener(Self::right_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_scroll_wheel(cx.listener(Self::scroll_wheel))
            .child(
                canvas(move |bounds, window, cx| prepaint(&view, bounds, window, cx), move |_, layout, window, cx| paint(layout, painted, &focus, window, cx))
                    .size_full(),
            )
            .context_menu(move |menu, _, cx| {
                let selected = menu_view.read_with(cx, |this, _| this.term.lock().selection.as_ref().is_some_and(|selection| !selection.is_empty()));
                menu.action_context(menu_focus.clone())
                    .menu_with_disabled("Copy", Box::new(CopySelection), !selected.unwrap_or(false))
                    .menu("Paste", Box::new(Paste))
            })
    }
}

/// Typed text, an input method's too, goes to the shell.
struct Typing {
    view: Entity<TerminalView>,
    caret: Bounds<Pixels>,
}

impl InputHandler for Typing {
    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut App) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&mut self, _: &mut Window, _: &mut App) -> Option<std::ops::Range<usize>> {
        None
    }

    fn text_for_range(&mut self, _: std::ops::Range<usize>, _: &mut Option<std::ops::Range<usize>>, _: &mut Window, _: &mut App) -> Option<String> {
        None
    }

    fn replace_text_in_range(&mut self, _: Option<std::ops::Range<usize>>, text: &str, _: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, _| view.input(text.as_bytes().to_vec()));
    }

    // shortcut: an input method's text being composed is not shown, only what it gives.
    fn replace_and_mark_text_in_range(&mut self, _: Option<std::ops::Range<usize>>, _: &str, _: Option<std::ops::Range<usize>>, _: &mut Window, _: &mut App) {}

    fn unmark_text(&mut self, _: &mut Window, _: &mut App) {}

    fn bounds_for_range(&mut self, _: std::ops::Range<usize>, _: &mut Window, _: &mut App) -> Option<Bounds<Pixels>> {
        Some(self.caret)
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut App) -> Option<usize> {
        None
    }

    // Held keys repeat, as a terminal's should.
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::ansi;

    #[test]
    fn colours_256() {
        assert_eq!(ansi(1, true), 0xcd3131);
        assert_eq!(ansi(16, true), 0x000000);
        assert_eq!(ansi(231, true), 0xffffff);
        assert_eq!(ansi(196, true), 0xff0000);
        assert_eq!(ansi(232, true), 0x080808);
        assert_eq!(ansi(255, true), 0xeeeeee);
    }
}
