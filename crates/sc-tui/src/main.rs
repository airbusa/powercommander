//! Spark Commander in the terminal: the window's panels, keys and core,
//! drawn as Midnight Commander does.

mod dialog;
mod input;
mod keys;
mod picture;
mod ui;
mod viewer;

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sc_core::keymap;
use sc_core::menu::Command;
use sc_core::origin::{self, Upload};
use sc_core::session::{self, Session};
use sc_core::settings::Settings;
use sc_core::viewer::Searches;
use sc_core::{Panel, Sort, SortKey, Tabs, archive, command, edit};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{Clear, ClearType, SetTitle};
use ratatui_image::picker::Picker;

use crate::dialog::{Answer, Form};
use crate::input::Input;
use crate::ui::Places;
use crate::viewer::Viewer;

pub struct Side {
    pub tabs: Tabs,
    /// The first entry in sight.
    pub top: usize,
}

pub struct Message {
    pub text: String,
    pub error: bool,
}

/// What work done in the background does to the app, back in its loop.
pub type Done = Box<dyn FnOnce(&mut App) + Send>;

/// What a dialog's answer is for.
pub enum Purpose {
    Find,
    /// Select (true) or Unselect Group.
    Group(bool),
}

/// Programs that take the terminal over until they end; the loop runs them.
enum Foreground {
    /// A command line for the shell, and its folder.
    Shell(String, PathBuf),
    /// The file, and for a copy how saves go back.
    Editor(PathBuf, Option<Upload>),
}

pub struct App {
    pub sides: [Side; 2],
    pub active: usize,
    pub settings: Settings,
    pub command: Input,
    pub message: Option<Message>,
    pub places: Places,
    pub viewer: Option<Viewer>,
    pub form: Option<(Form, Purpose)>,
    /// Keys as the terminal names them; Cmd never reaches it, so `secondary` is Ctrl.
    bindings: HashMap<String, Command>,
    /// For double clicks: when, and on which side's row.
    clicked: Option<(Instant, usize, usize)>,
    foreground: Option<Foreground>,
    /// Read at the first Find.
    searches: Option<Searches>,
    /// Save Selection's names, for Restore Selection.
    saved: Vec<std::ffi::OsString>,
    /// Synchronized browsing: the other panel makes the active one's moves too.
    sync_browsing: bool,
    /// How the terminal draws pictures; asked at the start.
    picker: Option<Picker>,
    done: (mpsc::Sender<Done>, mpsc::Receiver<Done>),
    quit: bool,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long the loop waits for a key before it looks at background work.
const TICK: Duration = Duration::from_millis(100);

fn main() -> io::Result<()> {
    sc_core::settings::adopt_old_name();
    let mut problems = Vec::new();
    let settings = Settings::load().unwrap_or_else(|err| {
        problems.push(format!("Could not read the settings: {err}"));
        Settings::default()
    });
    settings.apply_globally();
    let session = Session::load_from(session::TERMINAL).unwrap_or_else(|err| {
        problems.push(format!("Could not read the open tabs: {err}"));
        Session::default()
    });
    std::thread::spawn(archive::remove_stale_scratch);
    let mut app = App::new(
        settings,
        &session,
        std::env::args_os().skip(1).map(PathBuf::from),
    );
    if !problems.is_empty() {
        app.say(problems.join("\n"), true);
    }
    let mut terminal = start();
    app.picker = Some(picture::ask_terminal());
    let ran = app.run(&mut terminal);
    stop();
    app.save_session();
    archive::remove_scratch();
    ran
}

/// Full screen, raw keys, the mouse.
// shortcut: no kitty keyboard protocol, so most terminals send Ctrl+Tab as Tab (Alt+1..9
// pick tabs); turn it on once it is tried in kitty, foot or WezTerm.
fn start() -> DefaultTerminal {
    let terminal = ratatui::init();
    let _ = execute!(
        io::stdout(),
        EnableMouseCapture,
        SetTitle("Spark Commander")
    );
    terminal
}

fn stop() {
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

impl App {
    /// The saved tabs; folders given on the command line open on the left
    /// and right.
    fn new(settings: Settings, session: &Session, folders: impl Iterator<Item = PathBuf>) -> Self {
        let home = std::env::home_dir().unwrap_or_else(|| "/".into());
        let filter = settings.filter();
        let fallback = || {
            Panel::new(&home, filter)
                .or_else(|_| Panel::new("/", filter))
                .expect("list the home folder or /")
        };
        let side = |ix: usize| Side {
            tabs: match session.sides.get(ix) {
                Some(state) => Tabs::restore(state, filter, fallback),
                None => Tabs::new(fallback()),
            },
            top: 0,
        };
        let bindings = (settings.keys.bindings().into_iter())
            .map(|(key, command)| (keymap::normalize(&key, false), command))
            .collect();
        let mut app = App {
            sides: [side(0), side(1)],
            active: session.active.min(1),
            settings,
            command: Input::default(),
            message: None,
            places: Places::default(),
            viewer: None,
            form: None,
            bindings,
            clicked: None,
            foreground: None,
            searches: None,
            saved: Vec::new(),
            sync_browsing: false,
            picker: None,
            done: mpsc::channel(),
            quit: false,
        };
        for (ix, folder) in folders.take(2).enumerate() {
            let folder = command::resolve(
                &folder.to_string_lossy(),
                &std::env::current_dir().unwrap_or_default(),
            );
            app.navigate_on(ix, move |panel| panel.go_to_folder(folder));
        }
        app
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut redraw = true;
        let mut picture = None;
        while !self.quit {
            self.want_picture();
            // Pixels of a picture stay until the screen is cleared.
            let in_sight = self
                .viewer
                .as_ref()
                .and_then(Viewer::picture_in_sight)
                .cloned();
            if in_sight != picture {
                // Not terminal.clear(): it asks where the cursor is, and waits.
                execute!(io::stdout(), Clear(ClearType::All))?;
                terminal.swap_buffers();
                (picture, redraw) = (in_sight, true);
            }
            // Asked before drawing: work that ends while it draws gets one more frame.
            let busy = self.viewer.as_ref().is_some_and(Viewer::busy);
            if redraw {
                terminal.draw(|frame| ui::draw(frame, self))?;
            }
            redraw = busy;
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
                    Event::Mouse(mouse) => self.mouse(mouse),
                    _ => {}
                }
                redraw = true;
            }
            while let Ok(done) = self.done.1.try_recv() {
                done(self);
                redraw = true;
            }
            if let Some(foreground) = self.foreground.take() {
                *terminal = self.run_foreground(foreground);
                redraw = true;
            }
        }
        Ok(())
    }

    /// `work` on a thread of its own; what it gives back runs in the loop.
    fn background(&self, work: impl FnOnce() -> Done + Send + 'static) {
        let sender = self.done.0.clone();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
    }

    fn searches(&mut self) -> &mut Searches {
        self.searches
            .get_or_insert_with(|| Searches::load().unwrap_or_default())
    }

    fn save_session(&self) {
        let session = Session {
            sides: self.sides.iter().map(|side| side.tabs.state()).collect(),
            active: self.active,
            ..Session::default()
        };
        if let Err(err) = session.save_to(session::TERMINAL) {
            eprintln!("Could not save the open tabs: {err}");
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some(Message {
            text: text.into(),
            error,
        });
    }

    fn panel(&self) -> &Panel {
        self.sides[self.active].tabs.current()
    }

    fn panel_mut(&mut self) -> &mut Panel {
        self.sides[self.active].tabs.current_mut()
    }

    /// Rows a page moves: the file rows in sight.
    fn page(&self) -> isize {
        (self.places.rows[self.active].height as isize - 1).max(1)
    }

    fn navigate(&mut self, go: impl FnOnce(&mut Panel) -> io::Result<()>) {
        self.navigate_on(self.active, go);
    }

    fn navigate_on(&mut self, ix: usize, go: impl FnOnce(&mut Panel) -> io::Result<()>) {
        let tabs = &mut self.sides[ix].tabs;
        let before = tabs.current().spot();
        if let Err(err) = tabs.navigate(go) {
            let text = format!("{}: {err}", tabs.current().location());
            return self.say(text, true);
        }
        // Synchronized browsing: the other panel makes the move too.
        let after = self.sides[ix].tabs.current().spot();
        if self.sync_browsing
            && ix == self.active
            && self.sides[1 - ix].tabs.current().spot().is_some()
            && let (Some((_, from)), Some((_, to))) = (before, after)
            && from != to
        {
            let other = &mut self.sides[1 - ix].tabs;
            if let Err(err) = other.navigate(|panel| panel.follow(&from, &to)) {
                self.sync_browsing = false;
                let text = format!("{err}. Synchronized browsing is off.");
                self.say(text, true);
            }
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if self.message.take().is_some() {
            return;
        }
        if let Some((form, _)) = self.form.as_mut() {
            let answer = form.key(&key);
            if answer != Answer::Open
                && let Some((form, purpose)) = self.form.take()
                && answer == Answer::Ok
            {
                match purpose {
                    Purpose::Find => self.find_answered(&form),
                    Purpose::Group(mark) => self.panel_mut().mark_group(form.value(0), mark),
                }
            }
            return;
        }
        if self.viewer.is_some() {
            return self.viewer_key(key);
        }
        let typing = !self.command.is_empty();
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        match key.code {
            KeyCode::Tab if plain => return self.active = 1 - self.active,
            KeyCode::Up if plain => return self.panel_mut().move_cursor(-1),
            KeyCode::Down if plain => return self.panel_mut().move_cursor(1),
            KeyCode::PageUp if plain => return self.move_page(-1),
            KeyCode::PageDown if plain => return self.move_page(1),
            KeyCode::Home if plain && !typing => return self.panel_mut().set_cursor(0),
            KeyCode::End if plain && !typing => return self.panel_mut().set_cursor(usize::MAX),
            KeyCode::Enter if plain && typing => return self.run_line(),
            KeyCode::Enter if plain => return self.navigate(|panel| panel.open().map(drop)),
            KeyCode::Backspace if plain && !typing => return self.navigate(Panel::up),
            KeyCode::PageUp if key.modifiers == KeyModifiers::CONTROL => {
                return self.navigate(Panel::up);
            }
            KeyCode::Esc => return self.command.clear(),
            KeyCode::F(10) if plain => return self.quit = true,
            KeyCode::F(9) if plain => return self.not_yet("The menus"),
            // As in Midnight Commander, with nothing typed: groups and inverting.
            KeyCode::Char('+') if !typing && plain => return self.command(Command::SelectGroup),
            KeyCode::Char('-') if !typing && plain => return self.command(Command::UnselectGroup),
            KeyCode::Char('*') if !typing && plain => {
                return self.command(Command::InvertSelection);
            }
            // Alt+1 to Alt+9: that tab.
            KeyCode::Char(c @ '1'..='9') if key.modifiers == KeyModifiers::ALT => {
                let tabs = &mut self.sides[self.active].tabs;
                return tabs.select(c as usize - '1' as usize);
            }
            _ => {}
        }
        // The line's own editing keys come first while there is a line.
        let editing = matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Backspace
                | KeyCode::Delete
        );
        if typing && editing && self.command.handle(&key) {
            return;
        }
        let space = key.code == KeyCode::Char(' ');
        if let Some(name) = keys::name(&key)
            && (!space || !typing)
            && let Some(&command) = self.bindings.get(name.as_str())
        {
            return self.command(command);
        }
        self.command.handle(&key);
    }

    fn move_page(&mut self, direction: isize) {
        let rows = self.page() * direction;
        self.panel_mut().move_cursor(rows);
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if self.message.is_some() {
            if let MouseEventKind::Down(_) = mouse.kind {
                self.message = None;
            }
            return;
        }
        if self.form.is_some() {
            return;
        }
        let at = ratatui::layout::Position::new(mouse.column, mouse.row);
        // A click on the key bar presses that key.
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && self.places.keys.contains(at)
        {
            let slot = (mouse.column - self.places.keys.x) / (self.places.keys.width / 10).max(1);
            let key = KeyCode::F(slot.min(9) as u8 + 1);
            return self.key(KeyEvent::new(key, KeyModifiers::NONE));
        }
        if self.viewer.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.viewer_scroll(-3),
                MouseEventKind::ScrollDown => self.viewer_scroll(3),
                _ => {}
            }
            return;
        }
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            for ix in 0..2 {
                if let Some(tab) = self.places.tabs[ix].iter().position(|r| r.contains(at)) {
                    self.active = ix;
                    return self.sides[ix].tabs.select(tab);
                }
            }
        }
        let side = (0..2).find(|&ix| self.places.rows[ix].contains(at));
        match (mouse.kind, side) {
            (MouseEventKind::ScrollUp, Some(ix)) => {
                self.sides[ix].tabs.current_mut().move_cursor(-3)
            }
            (MouseEventKind::ScrollDown, Some(ix)) => {
                self.sides[ix].tabs.current_mut().move_cursor(3)
            }
            (MouseEventKind::Down(MouseButton::Left), Some(ix)) => {
                self.active = ix;
                let row = self.sides[ix].top + (mouse.row - self.places.rows[ix].y) as usize;
                if row >= self.panel().entries().len() {
                    return;
                }
                self.panel_mut().set_cursor(row);
                let double = self.clicked.is_some_and(|(when, side, at)| {
                    side == ix && at == row && when.elapsed() < DOUBLE_CLICK
                });
                self.clicked = Some((Instant::now(), ix, row));
                if double {
                    self.clicked = None;
                    self.navigate(|panel| panel.open().map(drop));
                }
            }
            _ => {}
        }
    }

    fn command(&mut self, command: Command) {
        let side = self.active;
        match command {
            Command::Quit => self.quit = true,
            Command::ToggleMark => self.panel_mut().toggle_mark(),
            Command::SelectAll => self.panel_mut().mark_all(true),
            Command::UnselectAll => self.panel_mut().mark_all(false),
            Command::InvertSelection => self.panel_mut().invert_marks(),
            Command::SortByName => self.sort(SortKey::Name),
            Command::SortByExtension => self.sort(SortKey::Extension),
            Command::SortByTime => self.sort(SortKey::Modified),
            Command::SortBySize => self.sort(SortKey::Size),
            Command::Unsorted => self.sort(SortKey::Unsorted),
            Command::ReversedOrder => {
                let sort = self.panel().sort();
                self.panel_mut().set_sort(Sort {
                    descending: !sort.descending,
                    ..sort
                });
            }
            Command::RereadSource => self.navigate(Panel::reload),
            Command::SyncBrowsing => {
                self.sync_browsing = !self.sync_browsing;
                let on = if self.sync_browsing { "on" } else { "off" };
                self.say(format!("Synchronized browsing is {on}."), false);
            }
            Command::SelectGroup | Command::UnselectGroup => {
                let mark = command == Command::SelectGroup;
                let title = if mark {
                    "Select group"
                } else {
                    "Unselect group"
                };
                let form =
                    Form::new(title).text("Files (*.txt *.md; folders end in /):", "*", Vec::new());
                self.form = Some((form, Purpose::Group(mark)));
            }
            Command::SelectAllWithSameExtension => self.panel_mut().mark_same_extension(),
            Command::SaveSelection => self.saved = self.panel().marked_names(),
            Command::RestoreSelection => {
                let saved = std::mem::take(&mut self.saved);
                self.panel_mut().mark_names(&saved);
                self.saved = saved;
            }
            Command::CompareDirectories | Command::MarkNewerHideSame => {
                let hide = command == Command::MarkNewerHideSame;
                let [left, right] = &mut self.sides;
                let count = left
                    .tabs
                    .current_mut()
                    .compare_with(right.tabs.current_mut(), hide);
                let text = match count {
                    0 => "The folders have the same files.".to_string(),
                    n => format!("{n} file(s) marked: newer than, or missing on, the other side."),
                };
                self.say(text, false);
            }
            // This computer's folders only: scmd opens no servers.
            Command::GoBack | Command::GoForward => {
                let by = if command == Command::GoBack { -1 } else { 1 };
                if let Some(place) = self.sides[side].tabs.current_mut().history.step(by) {
                    self.navigate(move |panel| panel.go_to_folder(PathBuf::from(place)));
                }
            }
            Command::BranchView | Command::BranchViewSelected => {
                let selected = command == Command::BranchViewSelected;
                self.navigate(move |panel| panel.toggle_branch(selected));
            }
            Command::GoHome => {
                let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"));
                self.navigate(move |panel| panel.go_to_folder(home));
            }
            Command::DuplicateTab => self.sides[side].tabs.duplicate(),
            Command::LockTab => self.sides[side].tabs.toggle_lock(),
            Command::CloseTab => self.sides[side].tabs.close(),
            Command::NextTab => self.sides[side].tabs.step(1),
            Command::PreviousTab => self.sides[side].tabs.step(-1),
            Command::CloseAllTabs => self.sides[side].tabs.close_all(),
            Command::CloseDuplicateTabs => self.sides[side].tabs.close_duplicates(),
            Command::View => self.view(),
            Command::Edit => self.edit(),
            Command::EditExternally => self.edit_externally(),
            Command::Copy
            | Command::Move
            | Command::NewFolder
            | Command::Delete
            | Command::DeletePermanently
            | Command::CopyToClipboard
            | Command::CutToClipboard
            | Command::PasteFromClipboard => self.not_yet("File operations"),
            Command::ConnectToServer | Command::Disconnect => self.not_yet("Servers"),
            _ => self.not_yet("This command"),
        }
    }

    /// The same key on the same column turns the order around, as a
    /// column title click does in the window.
    fn sort(&mut self, key: SortKey) {
        let sort = self.panel().sort();
        let sort = match sort.key == key {
            true => Sort {
                descending: !sort.descending,
                ..sort
            },
            false => Sort::by(key),
        };
        self.panel_mut().set_sort(sort);
    }

    // shortcut: the commands the terminal lacks say so; they come step by step.
    fn not_yet(&mut self, what: &str) {
        self.say(format!("{what}: not in the terminal version yet."), false);
    }

    /// `cd` moves the panel; anything else goes to the shell.
    fn run_line(&mut self) {
        let line = self.command.text().trim().to_owned();
        self.command.clear();
        let folder = self.panel().folder().to_owned();
        if let Some(target) = command::change_dir(&line, &folder) {
            return self.navigate(move |panel| panel.go_to_folder(target));
        }
        if self.panel().remote().is_some() {
            return self.say("Commands run on this computer, not on the server.", true);
        }
        self.foreground = Some(Foreground::Shell(line, folder));
    }

    /// The terminal is the program's until it ends.
    fn run_foreground(&mut self, foreground: Foreground) -> DefaultTerminal {
        stop();
        let folder = match foreground {
            Foreground::Shell(line, folder) => {
                println!("{}> {line}", folder.display());
                let ran = command::shell(&line).current_dir(&folder).status();
                if let Err(err) = ran {
                    println!("{err}");
                }
                print!("\nPress Enter to return to Spark Commander...");
                let _ = io::stdout().flush();
                let _ = io::stdin().read_line(&mut String::new());
                folder
            }
            Foreground::Editor(file, upload) => {
                let before = origin::modified(&file).ok().flatten();
                if let Err(err) = edit::edit_in_terminal(&file) {
                    self.say(format!("{}: {err}", file.display()), true);
                }
                if let Some(upload) = upload {
                    self.send_back(file.clone(), before, upload);
                }
                file.parent().map(Path::to_owned).unwrap_or_default()
            }
        };
        for side in &mut self.sides {
            side.tabs.reload_folder(&folder);
        }
        start()
    }

    /// A changed copy goes back to its server or archive, in the background.
    fn send_back(&self, file: PathBuf, before: Option<std::time::SystemTime>, upload: Upload) {
        self.background(move || {
            let sent = origin::send_back(&file, before, &upload);
            Box::new(move |app: &mut App| match sent {
                Some(Ok(text)) => app.say(text, false),
                Some(Err(err)) => app.say(format!("Could not save it back: {err}"), true),
                None => {}
            })
        });
    }

    /// F3: a server's or an archive's file is copied out first.
    fn view(&mut self) {
        let panel = self.panel();
        let Some(origin) = panel.origin() else {
            return;
        };
        let settings = self.settings.viewer.clone();
        if origin.is_local() {
            let (files, at) = panel.files_around();
            match Viewer::open(files, at, settings, None) {
                Ok(viewer) => self.viewer = Some(viewer),
                Err(err) => self.say(format!("{}: {err}", origin.name()), true),
            }
            return;
        }
        let name = origin.name();
        self.say(format!("Copying {name}..."), false);
        self.background(move || {
            let opened = origin
                .view()
                .and_then(|file| Viewer::open(vec![file], 0, settings, Some(name.clone())));
            Box::new(move |app: &mut App| {
                app.message = None;
                match opened {
                    Ok(viewer) => app.viewer = Some(viewer),
                    Err(err) => app.say(format!("{name}: {err}"), true),
                }
            })
        });
    }

    /// F4 in the terminal's editor; a server's or an archive's file is
    /// copied out first, and what the editor saves goes back.
    fn edit(&mut self) {
        let Some(origin) = self.panel().origin() else {
            return;
        };
        if let origin::Origin::Local(path) = origin {
            self.foreground = Some(Foreground::Editor(path, None));
            return;
        }
        let name = origin.name();
        self.say(format!("Copying {name}..."), false);
        self.background(move || {
            let prepared = origin.edit();
            Box::new(move |app: &mut App| {
                app.message = None;
                match prepared {
                    Ok((file, upload)) => app.foreground = Some(Foreground::Editor(file, upload)),
                    Err(err) => app.say(format!("{name}: {err}"), true),
                }
            })
        });
    }

    /// The external editor of Settings, a window of its own; a copy goes
    /// back once it ends.
    fn edit_externally(&mut self) {
        let Some(origin) = self.panel().origin() else {
            return;
        };
        let (line, name) = (self.settings.editor.external.clone(), origin.name());
        self.background(move || {
            let edited = origin.edit().and_then(|(file, upload)| {
                let Some(upload) = upload else {
                    return edit::edit_externally(&line, &file).map(|()| None);
                };
                let before = origin::modified(&file)?;
                edit::start_editor(&line, &file)?.wait()?;
                Ok(origin::send_back(&file, before, &upload))
            });
            Box::new(move |app: &mut App| match edited {
                Ok(Some(Ok(text))) => app.say(text, false),
                Ok(Some(Err(err))) => app.say(format!("Could not save it back: {err}"), true),
                Ok(None) => {}
                Err(err) => app.say(format!("{name}: {err}"), true),
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn draws_both_panels() {
        let root = std::env::temp_dir().join(format!("sc-tui-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("notes.txt"), "hello").unwrap();
        let folders = [root.clone(), root.join("sub")].into_iter();
        let mut app = App::new(Settings::default(), &Session::default(), folders);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            screen.contains("/sub") && screen.contains("notes.txt"),
            "{screen}"
        );
        assert!(screen.contains("UP--DIR"));
        assert!(screen.contains("Mkdir") && screen.contains("Quit"));

        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Char(' ')));
        assert!(
            app.panel()
                .entries()
                .iter()
                .any(|e| app.panel().is_marked(e)),
            "space marks"
        );
        for c in "cd sub".chars() {
            app.key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.command.text(),
            "cd sub",
            "typing goes to the command line"
        );
        app.key(key(KeyCode::Enter));
        assert_eq!(app.panel().path(), root.join("sub"));
        app.key(key(KeyCode::Backspace));
        assert_eq!(app.panel().path(), root, "back up, the line being empty");
        app.key(key(KeyCode::Char('l')));
        app.key(key(KeyCode::Enter));
        assert!(
            matches!(app.foreground.take(), Some(Foreground::Shell(line, at)) if line == "l" && at == root),
            "for the shell"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn other_panel_follows_with_synchronized_browsing() {
        let root = std::env::temp_dir().join(format!("sc-tui-sync-{}", std::process::id()));
        for dir in ["a/x", "a/only", "b/x"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let folders = [root.join("a"), root.join("b")].into_iter();
        let mut app = App::new(Settings::default(), &Session::default(), folders);
        app.key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::ALT | KeyModifiers::SHIFT));
        assert!(app.sync_browsing, "Alt+Shift+Y");
        let other = |app: &App| app.sides[1].tabs.current().path();
        app.navigate(|panel| panel.go_to_folder(root.join("a/x")));
        assert_eq!(other(&app), root.join("b/x"));
        app.navigate(Panel::up);
        assert_eq!(other(&app), root.join("b"));
        app.navigate(|panel| panel.go_to_folder(root.join("a/only")));
        assert_eq!(other(&app), root.join("b"));
        assert!(!app.sync_browsing, "off where the other panel cannot follow");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn changes_tabs_by_keys_and_clicks() {
        let root = std::env::temp_dir().join(format!("sc-tui-tabs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let folders = [root.clone(), root.clone()].into_iter();
        let mut app = App::new(Settings::default(), &Session::default(), folders);
        app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.sides[0].tabs.active(), 1, "a second tab");
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        app.key(KeyEvent::new(KeyCode::Left, alt_shift));
        assert_eq!(app.sides[0].tabs.active(), 0, "Alt+Shift+Left");
        app.key(KeyEvent::new(KeyCode::Right, alt_shift));
        assert_eq!(app.sides[0].tabs.active(), 1, "Alt+Shift+Right");
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let first = app.places.tabs[0][0];
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: first.x + 1,
            row: first.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.sides[0].tabs.active(), 0, "a click on the first tab");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn selects_groups_as_midnight_commander() {
        let root = std::env::temp_dir().join(format!("sc-tui-group-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for name in ["a.txt", "b.md", "c.txt"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let mut app = App::new(
            Settings::default(),
            &Session::default(),
            [root.clone()].into_iter(),
        );
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.key(key(KeyCode::Char('+')));
        assert!(
            app.form.is_some(),
            "+ with nothing typed asks for the group"
        );
        app.key(key(KeyCode::Backspace));
        for c in "*.txt".chars() {
            app.key(key(KeyCode::Char(c)));
        }
        app.key(key(KeyCode::Enter));
        let marked: Vec<String> = app
            .panel()
            .marked_names()
            .iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert_eq!(marked, ["a.txt", "c.txt"]);
        app.key(key(KeyCode::Char('*')));
        assert_eq!(app.panel().marked_names(), ["b.md"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
