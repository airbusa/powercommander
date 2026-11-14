pub mod about;
pub mod activity;
pub mod addons;
pub mod archive;
pub mod associations;
pub mod attributes;
mod bundled;
pub mod checksum;
pub mod clipboard;
pub mod cloud;
pub mod codepage;
pub mod command;
pub mod compare;
pub mod connections;
pub mod convert;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod desktop;
pub mod devices;
pub mod ebook;
pub mod dates;
pub mod edit;
pub mod elevate;
#[cfg(target_os = "macos")]
pub mod finder;
pub mod ftp;
pub mod heif;
pub mod html;
pub mod icons;
pub mod keymap;
pub mod links;
pub mod media;
pub mod menu;
pub mod mounts;
pub mod ops;
pub mod origin;
pub mod packing;
pub mod pdf;
pub mod properties;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod toolkit_style;
pub mod undo;
pub mod remote;
pub mod rename;
pub mod resume;
pub mod search;
pub mod secrets;
pub mod session;
pub mod settings;
#[cfg(windows)]
pub mod shell_icon;
#[cfg(windows)]
pub mod shell_menu;
pub mod sidebar;
pub mod sftp;
pub mod space;
pub mod tags;
pub mod text_edit;
pub mod text_find;
pub mod split;
pub mod theme;
pub mod vfs;
pub mod viewer;

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::session::{SideState, TabState};
use crate::vfs::{Fs, Local, Remote};
use std::sync::Arc;

pub const PARENT: &str = "..";

#[derive(Clone, Debug)]
pub struct Entry {
    /// Not always UTF-8.
    pub name: OsString,
    /// Symbolic links to folders are folders.
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Dot files on Linux and macOS, the hidden attribute on Windows.
    pub hidden: bool,
    /// The system attribute; Windows only.
    pub system: bool,
    /// As the Attr column shows them: `rwxr-xr-x`, or `rahs` on Windows.
    pub attributes: String,
    /// macOS: its Finder tags' colours.
    pub tags: Vec<u8>,
    /// A cloud file that is only online: reading it downloads it.
    pub online: bool,
}

/// Which entries a panel lists besides the usual ones.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Filter {
    pub hidden: bool,
    pub system: bool,
}

impl Filter {
    fn shows(self, entry: &Entry) -> bool {
        (self.hidden || !entry.hidden) && (self.system || !entry.system)
    }
}

/// The Show menu's file filter, as Total Commander's: the files a panel
/// lists, its folders always (but with Only Selected Files); it stays as the
/// panel moves, Only Selected Files to the next folder.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum FileFilter {
    #[default]
    All,
    /// Windows' by their extension; elsewhere those that may run.
    Programs,
    /// Masks as Select Group's ("*.txt *.md", or with ';'); after '|', those left out.
    Masks(String),
    /// The names marked when chosen.
    Selected(HashSet<OsString>),
    /// Quick filter: the names with this in them, any case; folders too.
    Quick(String),
}

/// Programs' extensions on Windows.
const PROGRAMS: [&str; 6] = ["exe", "com", "bat", "cmd", "pif", "msi"];

impl FileFilter {
    fn shows(&self, entry: &Entry) -> bool {
        if entry.is_parent() {
            return true;
        }
        match self {
            FileFilter::All => true,
            FileFilter::Selected(names) => names.contains(&entry.name),
            FileFilter::Quick(text) => entry.name.to_string_lossy().to_lowercase().contains(&text.to_lowercase()),
            _ if entry.is_dir && !entry.is_app() => true,
            FileFilter::Programs if cfg!(windows) => PROGRAMS.contains(&extension(entry).as_str()),
            FileFilter::Programs => entry.is_app() || entry.attributes.contains(['x', 's']),
            FileFilter::Masks(masks) => {
                let (shown, left_out) = masks.split_once('|').unwrap_or((masks, ""));
                let name = entry.name.to_string_lossy();
                let shown = shown.trim();
                (shown.is_empty() || associations::matches(shown, &name)) && !associations::matches(left_out, &name)
            }
        }
    }

    /// After the path, as Total Commander's `c:\docs\*.txt`.
    pub fn label(&self) -> Option<String> {
        match self {
            FileFilter::All => None,
            FileFilter::Programs if cfg!(windows) => Some(PROGRAMS.map(|ext| format!("*.{ext}")).join(" ")),
            FileFilter::Programs => Some("programs".into()),
            FileFilter::Masks(masks) => Some(masks.clone()),
            FileFilter::Selected(_) => Some("selected files".into()),
            FileFilter::Quick(text) => Some(format!("*{text}*")),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Extension,
    Modified,
    Size,
    Attributes,
    /// As the folder lists them.
    Unsorted,
}

/// How a panel lays its entries out, as Total Commander's Show menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    /// A row each, with its columns.
    #[default]
    Full,
    /// Names only, in columns side by side.
    Brief,
    /// Pictures' thumbnails, others' icons, in a grid.
    Thumbnails,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Sort {
    /// Newest and largest first, names A to Z.
    pub fn by(key: SortKey) -> Self {
        Self {
            key,
            descending: matches!(key, SortKey::Modified | SortKey::Size),
        }
    }

    /// `..` first, then folders by name (as Total Commander by default) or
    /// as the files are (`set_folders_like_files`), then files; macOS's
    /// applications among the files, as Finder has them.
    fn compare(self, a: &Entry, b: &Entry) -> Ordering {
        self.compare_folders(a, b, FOLDERS_LIKE_FILES.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn compare_folders(self, a: &Entry, b: &Entry, like_files: bool) -> Ordering {
        let folder = |entry: &Entry| entry.is_dir && !entry.is_app();
        let parent = b.is_parent().cmp(&a.is_parent());
        let dirs = parent.then(folder(b).cmp(&folder(a)));
        if (folder(a) || folder(b)) && !like_files {
            return dirs.then_with(|| by_name(a, b));
        }
        let files = match self.key {
            SortKey::Name => by_name(a, b),
            SortKey::Extension => extension(a).cmp(&extension(b)).then_with(|| by_name(a, b)),
            SortKey::Modified => a.modified.cmp(&b.modified).then_with(|| by_name(a, b)),
            SortKey::Size => a.size.cmp(&b.size).then_with(|| by_name(a, b)),
            SortKey::Attributes => a.attributes.cmp(&b.attributes).then_with(|| by_name(a, b)),
            SortKey::Unsorted => Ordering::Equal,
        };
        dirs.then(if self.descending {
            files.reverse()
        } else {
            files
        })
    }
}

static FOLDERS_LIKE_FILES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Folders sorted by the files' column, also by time (Settings > Display);
/// else always by name. Tabs' `resort` puts the lists in the new order.
pub fn set_folders_like_files(on: bool) {
    FOLDERS_LIKE_FILES.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn extension(entry: &Entry) -> String {
    Path::new(&entry.name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

impl Entry {
    pub fn is_parent(&self) -> bool {
        self.name == PARENT
    }

    pub fn display_name(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }

    /// macOS: an application, a folder named `….app` the system runs.
    pub fn is_app(&self) -> bool {
        let extension = Path::new(&self.name).extension();
        cfg!(target_os = "macos") && self.is_dir && extension.is_some_and(|e| e.eq_ignore_ascii_case("app"))
    }

    /// It shows its own icon, as the system's file manager draws it: macOS's
    /// applications, Windows' programs and shortcuts, Linux's launchers.
    pub fn own_icon(&self) -> bool {
        let extension = Path::new(&self.name).extension().map(|e| e.to_ascii_lowercase());
        let one_of = |list: &[&str]| extension.as_ref().is_some_and(|e| list.iter().any(|x| e == x));
        match () {
            _ if cfg!(target_os = "macos") => self.is_app(),
            _ if cfg!(windows) => !self.is_dir && one_of(&["exe", "lnk", "ico", "url", "cpl", "msc", "scr"]),
            _ => !self.is_dir && one_of(&["desktop"]),
        }
    }

    /// For the Name and Ext columns; folders keep their name whole.
    pub fn name_and_extension(&self) -> (String, String) {
        let name = self.display_name();
        match name.rfind('.') {
            Some(dot) if dot > 0 && (!self.is_dir || self.is_app()) => {
                (name[..dot].to_owned(), name[dot + 1..].to_owned())
            }
            _ => (name, String::new()),
        }
    }

    pub fn size_text(&self) -> String {
        if self.is_app() {
            return "<APP>".into();
        }
        if self.is_dir {
            return "<DIR>".into();
        }
        grouped(self.size)
    }

    pub fn modified_text(&self) -> String {
        self.modified.map(dates::date_time).unwrap_or_default()
    }
}

/// `1.5 MB`, for speeds and progress.
pub fn human_size(bytes: u64) -> String {
    const K: u64 = 1024;
    match bytes {
        0..K => format!("{bytes} B"),
        K..1_048_576 => format!("{:.1} KB", bytes as f64 / K as f64),
        1_048_576..1_073_741_824 => format!("{:.1} MB", bytes as f64 / 1_048_576.),
        1_073_741_824..1_099_511_627_776 => format!("{:.2} GB", bytes as f64 / 1_073_741_824.),
        _ => format!("{:.2} TB", bytes as f64 / 1_099_511_627_776.),
    }
}

/// `time` in the local zone, as `format` says.
pub fn local_time(time: SystemTime, format: &str) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format(format)
        .to_string()
}

/// Where a typed path leads: the panel's own file system, or this computer
/// when it shows a server's.
#[derive(Debug, PartialEq)]
pub enum Typed {
    Here(PathBuf),
    Local(PathBuf),
}

fn typed_on_server(url: &str, folder: &Path, typed: &str) -> Typed {
    if let Some(path) = typed.strip_prefix(url) {
        return Typed::Here(PathBuf::from(path));
    }
    match Path::new(typed).is_absolute() || typed.starts_with('~') {
        true => Typed::Local(command::resolve(typed, Path::new("/"))),
        false => Typed::Here(folder.join(typed)),
    }
}

#[derive(Clone)]
pub struct Panel {
    /// The folder, or the archive `archive` browses; on `remote`'s server if any.
    path: PathBuf,
    archive: Option<Archive>,
    remote: Option<Arc<dyn Remote>>,
    entries: Vec<Entry>,
    cursor: usize,
    /// Names of the marked entries: what F5, F6 and F8 work on.
    marked: HashSet<OsString>,
    /// Folders' sizes Calculate Occupied Space found, until the panel moves.
    measured: HashMap<OsString, u64>,
    /// Moves from a locked tab open a new tab.
    locked: bool,
    /// The tab's own name.
    name: Option<String>,
    filter: Filter,
    file_filter: FileFilter,
    sort: Sort,
    view: View,
    /// Search results fed to the panel, or Branch View's files, listed by their path from `path`.
    found: Option<Vec<PathBuf>>,
    /// What the path bar calls them.
    found_label: &'static str,
    /// Where the tab has been, for Back and Forward.
    pub history: History,
}

/// How many places a tab's history keeps.
const HISTORY: usize = 20;

/// The places a tab has been, as the path bar says them, and the one it is
/// at: Back and Forward go along them.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct History {
    pub places: Vec<String>,
    pub at: usize,
}

impl History {
    /// A move from `before` to `after`; none for where there is no going
    /// back to, as inside an archive. A new place drops those ahead.
    pub fn moved(&mut self, before: Option<String>, after: Option<String>) {
        if self.places.is_empty()
            && let Some(before) = before
        {
            self.visit(before);
        }
        if let Some(after) = after {
            self.visit(after);
        }
    }

    fn visit(&mut self, place: String) {
        if self.places.get(self.at) == Some(&place) {
            return;
        }
        self.places.truncate(self.at + 1);
        self.places.push(place);
        if self.places.len() > HISTORY {
            self.places.remove(0);
        }
        self.at = self.places.len() - 1;
    }

    /// Back (-1) or Forward (1): the place there, now the one it is at.
    pub fn step(&mut self, by: isize) -> Option<String> {
        self.jump(self.at.checked_add_signed(by)?)
    }

    /// Alt+Down's pick: the place at `to`, now the one it is at.
    pub fn jump(&mut self, to: usize) -> Option<String> {
        let place = self.places.get(to)?.clone();
        self.at = to;
        Some(place)
    }
}

#[derive(Clone)]
struct Archive {
    items: Vec<archive::Item>,
    dir: PathBuf,
    /// Where the archive is on the server; `path` is a copy taken from there.
    origin: Option<PathBuf>,
}

impl Panel {
    pub fn new(path: impl Into<PathBuf>, filter: Filter) -> io::Result<Self> {
        let mut panel = Self {
            path: path.into(),
            archive: None,
            remote: None,
            entries: Vec::new(),
            cursor: 0,
            marked: HashSet::new(),
            measured: HashMap::new(),
            locked: false,
            name: None,
            filter,
            file_filter: FileFilter::All,
            sort: Sort::default(),
            view: View::Full,
            found: None,
            found_label: "Search results:",
            history: History::default(),
        };
        panel.reload()?;
        Ok(panel)
    }

    pub fn file_filter(&self) -> &FileFilter {
        &self.file_filter
    }

    /// Lists the folder again by `filter`; Only Selected Files takes the marked names.
    pub fn set_file_filter(&mut self, filter: FileFilter) -> io::Result<()> {
        self.file_filter = match filter {
            FileFilter::Selected(_) => FileFilter::Selected(self.marked.clone()),
            filter => filter,
        };
        self.reload()
    }

    /// Where `state` was, or the nearest folder above it that still exists.
    pub fn restore(state: &TabState, filter: Filter) -> io::Result<Self> {
        if state.url.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "a server's tab",
            ));
        }
        let exact = match &state.archive_dir {
            Some(dir) => Self::new(state.path.parent().unwrap_or(&state.path), filter).and_then(
                |mut panel| {
                    panel.enter_archive(state.path.clone(), None, &archive::Watch::default())?;
                    panel.show_archive_dir(dir.clone(), None);
                    Ok(panel)
                },
            ),
            None => Self::new(&state.path, filter),
        };
        let mut panel = exact.or_else(|err| {
            let above = state.path.ancestors().skip(1);
            above
                .into_iter()
                .find_map(|dir| Self::new(dir, filter).ok())
                .ok_or(err)
        })?;
        panel.locked = state.locked;
        panel.name = state.name.clone();
        panel.view = state.view;
        panel.history = state.history.clone();
        Ok(panel)
    }

    pub fn state(&self) -> TabState {
        TabState {
            path: self.path.clone(),
            archive_dir: self.archive.as_ref().map(|archive| archive.dir.clone()),
            locked: self.locked,
            name: self.name.clone(),
            url: self.remote.as_ref().map(|remote| remote.url()),
            view: self.view,
            history: self.history.clone(),
        }
    }

    /// Where it is, for the history: as the path bar says it, which it takes
    /// back; none inside an archive or for search results.
    fn place(&self) -> Option<String> {
        (self.archive.is_none() && self.found.is_none()).then(|| self.location())
    }

    /// The path as the title bar shows it: `sftp://user@host/dir` on a
    /// server, `smb://nas/media` where the desktop mounted it.
    pub fn location(&self) -> String {
        let path = self.path();
        match &self.remote {
            Some(remote) => format!("{}{}", remote.url(), path.display()),
            None => mounts::url_of(&path).unwrap_or_else(|| path.display().to_string()),
        }
    }

    /// Where a path typed for this panel leads. On a server: the server's
    /// path after its address, or relative; else this computer's.
    pub fn typed_location(&self, typed: &str) -> Typed {
        match &self.remote {
            Some(remote) => typed_on_server(&remote.url(), self.folder(), typed),
            None => Typed::Here(command::resolve(typed, self.folder())),
        }
    }

    pub fn remote(&self) -> Option<&Arc<dyn Remote>> {
        self.remote.as_ref()
    }

    /// What copies read from and write to here.
    pub fn fs(&self) -> Arc<dyn Fs> {
        match &self.remote {
            Some(remote) => remote.clone(),
            None => Arc::new(Local),
        }
    }

    /// Shows `path` on `remote`'s server.
    pub fn go_remote(&mut self, remote: Arc<dyn Remote>, path: PathBuf) -> io::Result<()> {
        let entries = remote.entries(&path)?;
        self.remote = Some(remote);
        self.archive = None;
        self.found = None;
        self.marked.clear();
        self.measured.clear();
        self.path = path;
        self.show(entries, None);
        Ok(())
    }

    /// Leaves the server for the local `path`.
    pub fn disconnect(&mut self, path: PathBuf) -> io::Result<()> {
        let entries = read_dir(&path)?;
        self.remote = None;
        self.archive = None;
        self.found = None;
        self.marked.clear();
        self.measured.clear();
        self.path = path;
        self.show(entries, None);
        Ok(())
    }

    pub fn path(&self) -> PathBuf {
        match &self.archive {
            Some(archive) if archive.dir.as_os_str().is_empty() => self.archive_path().to_owned(),
            Some(archive) => self.archive_path().join(&archive.dir),
            None => self.path.clone(),
        }
    }

    /// The archive browsed, where it is: on the server for one taken from there.
    fn archive_path(&self) -> &Path {
        let origin = self
            .archive
            .as_ref()
            .and_then(|archive| archive.origin.as_deref());
        origin.unwrap_or(&self.path)
    }

    /// The tab's own name, or its folder's.
    pub fn name(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        if self.found.is_some() {
            return "Search results".into();
        }
        let path = self.path();
        match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned(),
            None => path.display().to_string(),
        }
    }

    /// As its tab shows it; locked tabs start with `*`.
    pub fn title(&self) -> String {
        match self.locked {
            true => format!("*{}", self.name()),
            false => self.name(),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn move_cursor(&mut self, rows: isize) {
        self.set_cursor(self.cursor.saturating_add_signed(rows));
    }

    /// Quick search, as typing in a panel: the first entry from `from`, round
    /// to the top, whose name starts with `typed`, any case; never `..`.
    pub fn quick_find(&self, typed: &str, from: usize) -> Option<usize> {
        let typed = typed.to_lowercase();
        let count = self.entries.len();
        (from.min(count)..count).chain(0..from.min(count)).find(|&ix| {
            let entry = &self.entries[ix];
            !entry.is_parent() && entry.display_name().to_lowercase().starts_with(&typed)
        })
    }

    pub fn set_cursor(&mut self, ix: usize) {
        self.cursor = ix.min(self.entries.len().saturating_sub(1));
    }

    /// False on a file that is no archive.
    pub fn open(&mut self) -> io::Result<bool> {
        self.open_watched(&archive::Watch::default())
    }

    /// As `open`; an archive's listing reports to `watch` and stops by it.
    pub fn open_watched(&mut self, watch: &archive::Watch) -> io::Result<bool> {
        let Some(entry) = self.entries.get(self.cursor) else {
            return Ok(false);
        };
        let (name, is_dir) = (entry.name.clone(), entry.is_dir);
        if entry.is_parent() {
            self.up()?;
        } else if let Some(archive) = &self.archive {
            if !is_dir {
                return Ok(false);
            }
            self.show_archive_dir(archive.dir.join(name), None);
        } else if is_dir {
            self.go_to(self.path.join(name), None)?;
        } else if archive::is_archive(&name) {
            let path = self.path.join(name);
            match &self.remote {
                // shortcut: no progress while it downloads; show one if big archives make it feel stuck.
                Some(remote) => self.enter_archive(vfs::fetch(&**remote, &path)?, Some(path), watch)?,
                None => self.enter_archive(path, None, watch)?,
            }
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    /// Cursor lands on the folder or archive it leaves.
    pub fn up(&mut self) -> io::Result<()> {
        // From search results: their folder.
        if self.found.is_some() {
            return self.go_to(self.path.clone(), None);
        }
        if let Some(archive) = &self.archive
            && let Some(parent) = archive.dir.parent()
        {
            let left = archive.dir.file_name().map(OsString::from);
            self.show_archive_dir(parent.to_owned(), left);
            return Ok(());
        }
        let here = self.archive_path();
        let Some(parent) = here.parent() else {
            return Ok(());
        };
        let left = here.file_name().map(OsString::from);
        self.go_to(parent.to_owned(), left)
    }

    /// Reads the folder or archive again; the cursor stays on its entry.
    pub fn reload(&mut self) -> io::Result<()> {
        let select = self.entries.get(self.cursor).map(|e| e.name.clone());
        if self.found.is_some() {
            let entries = self.found_entries();
            self.show(entries, select);
            return Ok(());
        }
        let Some(archive) = &self.archive else {
            return self.go_to(self.path.clone(), select);
        };
        let (dir, origin) = (archive.dir.clone(), archive.origin.clone());
        let items = archive::list(&self.path)?;
        self.archive = Some(Archive {
            items,
            dir: PathBuf::new(),
            origin,
        });
        self.show_archive_dir(dir, select);
        Ok(())
    }

    fn enter_archive(&mut self, path: PathBuf, origin: Option<PathBuf>, watch: &archive::Watch) -> io::Result<()> {
        let items = archive::list_watched(&path, watch)?;
        self.moved();
        self.found = None;
        self.path = path;
        self.archive = Some(Archive {
            items,
            dir: PathBuf::new(),
            origin,
        });
        self.show_archive_dir(PathBuf::new(), None);
        Ok(())
    }

    // Read first: a failed move leaves the panel as it was.
    pub fn sort(&self) -> Sort {
        self.sort
    }

    pub fn view(&self) -> View {
        self.view
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
    }

    /// The cursor stays on its entry.
    pub fn set_sort(&mut self, sort: Sort) {
        self.sort = sort;
        let select = self.current_entry().map(|e| e.name.clone());
        let entries = std::mem::take(&mut self.entries);
        self.show(entries, select);
    }

    pub fn in_archive(&self) -> bool {
        self.archive.is_some()
    }

    /// Inside an archive, the file at the cursor.
    pub fn archive_member(&self) -> Option<archive::Member> {
        self.archive_member_of(self.current_entry()?)
    }

    fn archive_member_of(&self, entry: &Entry) -> Option<archive::Member> {
        let archive = self.archive.as_ref()?;
        let entry = Some(entry).filter(|entry| !entry.is_dir)?;
        let path = archive.dir.join(&entry.name);
        let item = archive.items.iter().find(|item| item.path == path)?;
        Some(archive::Member {
            archive: self.path.clone(),
            listed: item.listed.clone(),
            name: entry.name.clone(),
            inside: item.path.clone(),
        })
    }

    /// An archive taken from a server: the server, and where it is there.
    pub fn archive_origin(&self) -> Option<(Arc<dyn Remote>, PathBuf)> {
        let origin = self.archive.as_ref()?.origin.clone()?;
        Some((self.remote.clone()?, origin))
    }

    /// The file at the cursor, for viewers and editors.
    pub fn origin(&self) -> Option<origin::Origin> {
        self.origin_of(self.current_entry()?)
    }

    /// An entry's file, as `origin` the cursor's.
    pub fn origin_of(&self, entry: &Entry) -> Option<origin::Origin> {
        let entry = Some(entry).filter(|entry| !entry.is_dir)?;
        if self.archive.is_some() {
            let member = self.archive_member_of(entry)?;
            return Some(origin::Origin::Archive(member, self.archive_origin()));
        }
        let path = self.folder().join(&entry.name);
        Some(match self.remote {
            Some(_) => origin::Origin::Server(self.fs(), path),
            None => origin::Origin::Local(path),
        })
    }

    /// The folder's files, and which is at the cursor: the viewer's Next and Previous.
    pub fn files_around(&self) -> (Vec<PathBuf>, usize) {
        let files = self.entries.iter().filter(|entry| !entry.is_dir);
        let files: Vec<PathBuf> = files.map(|entry| self.folder().join(&entry.name)).collect();
        let at = self
            .current_entry()
            .map(|entry| self.folder().join(&entry.name));
        let at = files.iter().position(|file| Some(file) == at.as_ref());
        (files, at.unwrap_or(0))
    }

    /// Inside an archive: the chosen entries taken out with their folders
    /// into a temporary folder, for F5 to copy from there. Blocks.
    pub fn extract_chosen(&self) -> io::Result<Vec<PathBuf>> {
        let names: Vec<OsString> = self.chosen().iter().map(|e| e.name.clone()).collect();
        self.extract(&names)
    }

    /// Inside an archive: these entries taken out, as `extract_chosen`.
    pub fn extract(&self, names: &[OsString]) -> io::Result<Vec<PathBuf>> {
        let Some(archive) = &self.archive else {
            return Err(io::Error::other("not in an archive"));
        };
        let paths: Vec<PathBuf> = names.iter().map(|name| archive.dir.join(name)).collect();
        let listed = paths.iter().filter_map(|path| {
            let item = archive.items.iter().find(|item| item.path == *path)?;
            Some(item.listed.as_str())
        });
        let listed: Vec<&str> = listed.collect();
        let dir = archive::fresh_scratch()?;
        archive::extract(&self.path, &listed, &dir, true)?;
        Ok(paths.iter().map(|path| dir.join(path)).collect())
    }

    /// A file's size, or a folder's once measured.
    pub fn measured_size(&self, entry: &Entry) -> Option<u64> {
        match entry.is_dir {
            true => self.measured.get(&entry.name).copied(),
            false => Some(entry.size),
        }
    }

    /// The Size column: a measured folder's size in place of `<DIR>`.
    pub fn size_text(&self, entry: &Entry) -> String {
        match (entry.is_dir, self.measured.get(&entry.name)) {
            (true, Some(&bytes)) => grouped(bytes),
            _ => entry.size_text(),
        }
    }

    /// Calculate Occupied Space found `bytes` in the folder `name`.
    pub fn set_measured(&mut self, name: OsString, bytes: u64) {
        self.measured.insert(name, bytes);
    }

    pub fn current_entry(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// Quick View's subject: the file or folder at the cursor, this folder on
    /// `..`, and whether it is a folder; inside an archive, its path there.
    pub fn quick_view(&self) -> Option<(PathBuf, bool)> {
        let entry = self.current_entry()?;
        Some(match entry.is_parent() {
            true => (self.path(), true),
            false => (self.path().join(&entry.name), entry.is_dir),
        })
    }

    pub fn is_marked(&self, entry: &Entry) -> bool {
        self.marked.contains(&entry.name)
    }

    /// Insert and Space: the entry at the cursor, then the next one.
    pub fn toggle_mark(&mut self) {
        if let Some(entry) = self.current_entry().filter(|e| !e.is_parent()) {
            let name = entry.name.clone();
            if !self.marked.remove(&name) {
                self.marked.insert(name);
            }
        }
        self.move_cursor(1);
    }

    /// All but `..`, or none.
    pub fn mark_all(&mut self, marked: bool) {
        self.marked.clear();
        if marked {
            let names = self.entries.iter().filter(|e| !e.is_parent());
            self.marked.extend(names.map(|e| e.name.clone()));
        }
    }

    pub fn invert_marks(&mut self) {
        let names = self.entries.iter().filter(|e| !e.is_parent());
        let inverted = names.filter(|e| !self.marked.contains(&e.name));
        self.marked = inverted.map(|e| e.name.clone()).collect();
    }

    /// Select or Unselect Group: entries matching `masks` (`*.txt *.md`);
    /// a mask ending in `/` or `\\` is for folders.
    pub fn mark_group(&mut self, masks: &str, mark: bool) {
        let (mut files, mut dirs) = (Vec::new(), Vec::new());
        for mask in masks.split([' ', ';']).filter(|m| !m.is_empty()) {
            match mask.strip_suffix(['/', '\\']) {
                Some(dir) => dirs.push(dir),
                None => files.push(mask),
            }
        }
        let (files, dirs) = (files.join(" "), dirs.join(" "));
        for entry in self.entries.iter().filter(|e| !e.is_parent()) {
            let masks = if entry.is_dir { &dirs } else { &files };
            if associations::matches(masks, &entry.name.to_string_lossy()) {
                match mark {
                    true => self.marked.insert(entry.name.clone()),
                    false => self.marked.remove(&entry.name),
                };
            }
        }
    }

    /// Select All With Same Extension: the cursor's file's.
    pub fn mark_same_extension(&mut self) {
        let Some(cursor) = self.current_entry().filter(|e| !e.is_dir) else {
            return;
        };
        let wanted = extension(cursor);
        let same = self
            .entries
            .iter()
            .filter(|e| !e.is_dir && extension(e) == wanted);
        let names: Vec<OsString> = same.map(|e| e.name.clone()).collect();
        self.marked.extend(names);
    }

    /// The marked entries' names, for Save Selection.
    pub fn marked_names(&self) -> Vec<OsString> {
        let marked = self.entries.iter().filter(|e| self.is_marked(e));
        marked.map(|e| e.name.clone()).collect()
    }

    /// Restore Selection: those of `names` that are here.
    pub fn mark_names(&mut self, names: &[OsString]) {
        let here: HashSet<&OsString> = self.entries.iter().map(|e| &e.name).collect();
        let found: Vec<OsString> = names.iter().filter(|n| here.contains(n)).cloned().collect();
        self.marked.clear();
        self.marked.extend(found);
    }

    /// The chosen entries for the clipboard, a line each: the name or the
    /// path, and with details its size, date and attributes, tab apart.
    pub fn names_text(&self, path: bool, details: bool) -> String {
        let mut text = String::new();
        for entry in self.chosen() {
            match path {
                true => text += &self.folder().join(&entry.name).to_string_lossy(),
                false => text += &entry.name.to_string_lossy(),
            }
            if details {
                let size = self.size_text(entry);
                text += &format!("\t{size}\t{}\t{}", entry.modified_text(), entry.attributes);
            }
            text += if cfg!(windows) { "\r\n" } else { "\n" };
        }
        text
    }

    /// Compare Directories: marks here the files the other panel lacks or
    /// has older, and there the other way round; with `hide_same`, files of
    /// the same size and time leave both lists. Gives the files marked.
    pub fn compare_with(&mut self, other: &mut Panel, hide_same: bool) -> usize {
        let files =
            |panel: &Panel| -> std::collections::HashMap<OsString, (u64, Option<SystemTime>)> {
                let files = panel.entries.iter().filter(|e| !e.is_dir);
                files
                    .map(|e| (e.name.clone(), (e.size, e.modified)))
                    .collect()
            };
        let (mine, theirs) = (files(self), files(other));
        // Within two seconds is the same time: FAT keeps even seconds.
        let newer = |a: Option<SystemTime>, b: Option<SystemTime>| match (a, b) {
            (Some(a), Some(b)) => a.duration_since(b).is_ok_and(|d| d.as_secs() >= 2),
            _ => false,
        };
        let same = |a: &(u64, Option<SystemTime>), b: &(u64, Option<SystemTime>)| {
            a.0 == b.0 && !newer(a.1, b.1) && !newer(b.1, a.1)
        };
        let pick =
            |own: &std::collections::HashMap<OsString, (u64, Option<SystemTime>)>,
             other: &std::collections::HashMap<OsString, (u64, Option<SystemTime>)>| {
                own.iter()
                    .filter(|(name, meta)| match other.get(*name) {
                        None => true,
                        Some(theirs) => {
                            newer(meta.1, theirs.1)
                                || (!same(meta, theirs) && !newer(theirs.1, meta.1))
                        }
                    })
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>()
            };
        let (here, there) = (pick(&mine, &theirs), pick(&theirs, &mine));
        let count = here.len() + there.len();
        self.marked = here.into_iter().collect();
        other.marked = there.into_iter().collect();
        if hide_same {
            let hidden: HashSet<OsString> = mine
                .iter()
                .filter(|(name, meta)| theirs.get(*name).is_some_and(|t| same(meta, t)))
                .map(|(name, _)| name.clone())
                .collect();
            for panel in [&mut *self, &mut *other] {
                panel
                    .entries
                    .retain(|e| e.is_dir || !hidden.contains(&e.name));
                panel.cursor = panel.cursor.min(panel.entries.len().saturating_sub(1));
            }
        }
        count
    }

    /// What a drag from `row` takes: the marked entries when it is one of
    /// them, else it alone; never `..`.
    pub fn dragged(&self, row: usize) -> Vec<&Entry> {
        match self.entries.get(row).filter(|entry| !entry.is_parent()) {
            Some(entry) if self.is_marked(entry) => self.chosen(),
            Some(entry) => vec![entry],
            None => Vec::new(),
        }
    }

    /// What an operation works on, as in Total Commander: the marked
    /// entries, or else the one at the cursor.
    pub fn chosen(&self) -> Vec<&Entry> {
        let marked: Vec<&Entry> = self.entries.iter().filter(|e| self.is_marked(e)).collect();
        match marked.is_empty() {
            true => self
                .current_entry()
                .filter(|e| !e.is_parent())
                .into_iter()
                .collect(),
            false => marked,
        }
    }

    /// `name` may be a path; the cursor goes to its first folder here.
    pub fn new_folder(&mut self, name: &str) -> io::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no name given"));
        }
        if self.archive.is_some() {
            let (archive, dir) = self.archive_place()?;
            let made = archive::new_folder(&archive, &dir, name);
            activity::note(format!("New folder \"{name}\" in {}", archive.display()), &made);
            made?;
            return self.reload_at(name);
        }
        self.on_disk()?;
        let target = self.path.join(name);
        let fs = self.fs();
        if fs.metadata(&target).is_ok() {
            let err = format!("{name} exists already");
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, err));
        }
        let made = vfs::create_all(&*fs, &target);
        activity::note(format!("New folder {}", target.display()), &made);
        made?;
        let first = Path::new(name)
            .components()
            .next()
            .map(|c| c.as_os_str().to_owned());
        self.go_to(self.path.clone(), first)
    }

    /// A new file here, empty or a copy of `template`; the cursor on it.
    pub fn new_file(&mut self, name: &str, template: Option<&Path>) -> io::Result<()> {
        self.on_disk()?;
        let name = name.trim();
        if name.is_empty() || name.contains(['/', std::path::MAIN_SEPARATOR]) || name == "." || name == PARENT {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name}: not a name")));
        }
        let target = self.path.join(name);
        let fs = self.fs();
        if fs.metadata(&target).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{name} exists already")));
        }
        // A new file's times are now's, not the template's.
        let mut file = fs.create(&target)?;
        if let Some(template) = template {
            io::copy(&mut fs::File::open(template)?, &mut file)?;
        }
        drop(file);
        self.go_to(self.path.clone(), Some(OsString::from(name)))
    }

    /// Renames an entry here or on a server (`name` may be a path, as in
    /// search results); the cursor on it then.
    pub fn rename_entry(&mut self, name: &OsStr, to: &str) -> io::Result<()> {
        let to = to.trim();
        if to.is_empty() || to.contains(['/', std::path::MAIN_SEPARATOR]) || to == "." || to == PARENT {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{to}: not a name")));
        }
        if self.archive.is_some() {
            let (archive, dir) = self.archive_place()?;
            let renamed = archive::rename(&archive, &dir.join(name), &dir.join(to));
            activity::note(format!("Rename \"{}\" to \"{to}\" in {}", Path::new(name).display(), archive.display()), &renamed);
            renamed?;
            return self.reload_at(to);
        }
        self.on_disk()?;
        let from = self.path.join(name);
        let target = from.with_file_name(to);
        let fs = self.fs();
        // Only its case changed, it is there already: on macOS and Windows, itself.
        let itself = from.file_name().is_some_and(|old| old.to_string_lossy().eq_ignore_ascii_case(to));
        if !itself && fs.metadata(&target).is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{to} exists already")));
        }
        let renamed = fs.rename(&from, &target);
        activity::note(format!("Rename {} to \"{to}\"", from.display()), &renamed);
        renamed?;
        let title = format!("Rename \"{}\" to \"{to}\"", from.file_name().unwrap_or_default().to_string_lossy());
        undo::remember(title, undo::Step::Moved { from: fs.clone(), to: fs.clone(), moves: vec![(from.clone(), target.clone())] });
        let shown = Path::new(name).with_file_name(to).into_os_string();
        match &mut self.found {
            Some(found) => {
                found.iter_mut().filter(|path| **path == from).for_each(|path| *path = target.clone());
                let entries = self.found_entries();
                self.show(entries, Some(shown));
                Ok(())
            }
            None => self.go_to(self.path.clone(), Some(shown)),
        }
    }

    /// The chosen entries, to the trash or for good; stops at the first error.
    pub fn delete(&mut self, to_trash: bool) -> io::Result<()> {
        // Inside an archive, 7-Zip's: no trash there.
        if self.archive.is_some() {
            let (archive, dir) = self.archive_place()?;
            let paths: Vec<PathBuf> = self.chosen().iter().map(|e| dir.join(&e.name)).collect();
            let deleted = archive::delete(&archive, &paths);
            activity::note(format!("Delete {} item(s) from {}", paths.len(), archive.display()), &deleted);
            deleted?;
            let cursor = self.cursor;
            self.reload()?;
            self.set_cursor(cursor);
            return Ok(());
        }
        self.on_disk()?;
        let paths: Vec<PathBuf> = self
            .chosen()
            .iter()
            .map(|e| self.path.join(&e.name))
            .collect();
        let at = std::time::SystemTime::now();
        let mut trashed = Vec::new();
        let deleted = paths.iter().try_for_each(|path| {
            if let Some(remote) = &self.remote {
                // A server has no trash.
                vfs::remove_all(&**remote, path)
            } else if to_trash {
                trash::delete(path).map_err(io::Error::other)?;
                trashed.push(path.clone());
                Ok(())
            } else if fs::symlink_metadata(path)?.is_dir() {
                fs::remove_dir_all(path)
            } else {
                // A link goes, never what it points to.
                fs::remove_file(path)
            }
        });
        let what = match (paths.as_slice(), self.remote.is_some() || !to_trash) {
            ([one], true) => format!("Delete {}", one.display()),
            ([one], false) => format!("Move {} to the trash", one.display()),
            (many, true) => format!("Delete {} items in {}", many.len(), self.path.display()),
            (many, false) => format!("Move {} items in {} to the trash", many.len(), self.path.display()),
        };
        activity::note(what, &deleted);
        if !trashed.is_empty() {
            let title = match trashed.as_slice() {
                [one] => format!("Move \"{}\" to the trash", one.file_name().unwrap_or_default().to_string_lossy()),
                many => format!("Move {} items to the trash", many.len()),
            };
            undo::remember(title, undo::Step::Trashed { paths: trashed, at });
        }
        let cursor = self.cursor;
        self.go_to(self.path.clone(), None)?;
        self.set_cursor(cursor);
        deleted
    }

    /// The archive shown and its folder inside, where 7-Zip can change it:
    /// this computer's 7z, zip and tar archives.
    pub fn archive_place(&self) -> io::Result<(PathBuf, PathBuf)> {
        let Some(archive) = &self.archive else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "not inside an archive"));
        };
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        if archive.origin.is_some() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, format!("{name}: archives on servers cannot be changed yet")));
        }
        if !archive::updatable(&self.path) {
            let text = format!("{name}: 7-Zip changes 7z, zip and tar archives in place, not this one");
            return Err(io::Error::new(io::ErrorKind::Unsupported, text));
        }
        Ok((self.path.clone(), archive.dir.clone()))
    }

    /// Lists the archive again, the cursor on `name`.
    fn reload_at(&mut self, name: &str) -> io::Result<()> {
        self.reload()?;
        if let Some(at) = self.entries.iter().position(|e| e.name == name) {
            self.set_cursor(at);
        }
        Ok(())
    }

    fn on_disk(&self) -> io::Result<()> {
        match self.archive {
            Some(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "not inside archives yet",
            )),
            None => Ok(()),
        }
    }

    pub fn go_to_folder(&mut self, path: PathBuf) -> io::Result<()> {
        self.go_to(path, None)
    }

    /// Where synchronized browsing follows it: its folder, with the server's
    /// address on one; none in an archive or search results.
    pub fn spot(&self) -> Option<(Option<String>, PathBuf)> {
        let server = self.remote.as_ref().map(|remote| remote.url());
        (self.archive.is_none() && self.found.is_none()).then(|| (server, self.path.clone()))
    }

    /// Synchronized browsing: the other panel's move from `from` to `to` made here too.
    pub fn follow(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        let (target, left) = followed(from, to, &self.path)
            .ok_or_else(|| io::Error::other("the other panel went to another drive"))?;
        // Named, as it is not where the panel is.
        let named = |err: io::Error| io::Error::new(err.kind(), format!("{}: {err}", target.display()));
        self.go_to(target.clone(), left).map_err(named)
    }

    /// The top of what it shows: the archive's, the server's, the drive's or `/`.
    pub fn go_to_root(&mut self) -> io::Result<()> {
        if self.archive.is_some() {
            self.show_archive_dir(PathBuf::new(), None);
            return Ok(());
        }
        let root = self.folder().ancestors().last().unwrap_or(Path::new("/")).to_owned();
        self.go_to(root, None)
    }

    /// Its folder, the cursor on it; a file inside an archive, the archive's folder.
    pub fn go_to_file(&mut self, path: &Path, member: Option<&Path>) -> io::Result<()> {
        let parent = path.parent().unwrap_or(path).to_owned();
        let Some(member) = member else {
            return self.go_to(parent, path.file_name().map(OsString::from));
        };
        self.go_to(parent, None)?;
        self.enter_archive(path.to_owned(), None, &archive::Watch::default())?;
        let dir = member.parent().unwrap_or(Path::new("")).to_owned();
        self.show_archive_dir(dir, member.file_name().map(OsString::from));
        Ok(())
    }

    /// Feed to listbox: the files a search found, by their path from the
    /// folder they are all in; `..` leaves for that folder.
    pub fn show_found(&mut self, paths: Vec<PathBuf>) {
        let Some(first) = paths.first() else {
            return;
        };
        let mut root = first.parent().unwrap_or(first).to_owned();
        for path in &paths {
            while !path.starts_with(&root) && root.pop() {}
        }
        self.show_listed(root, paths, "Search results:");
    }

    /// Branch View (Total Commander's Ctrl+B): every file under this folder,
    /// or under the marked folders, as one list by their paths from it; `..`
    /// leaves it, and so does asking again. Hidden ones as the panel shows them.
    pub fn toggle_branch(&mut self, marked_only: bool) -> io::Result<()> {
        if self.found.is_some() {
            return self.go_to(self.path.clone(), None);
        }
        if self.remote.is_some() || self.archive.is_some() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Branch View lists this computer's folders"));
        }
        let roots: Vec<PathBuf> = match marked_only {
            true => self.chosen().into_iter().filter(|entry| entry.is_dir && !entry.is_parent()).map(|entry| self.path.join(&entry.name)).collect(),
            false => vec![self.path.clone()],
        };
        let mut files = Vec::new();
        for root in &roots {
            files_under(root, self.filter, &mut files);
        }
        self.show_listed(self.path.clone(), files, "Branch view:");
        Ok(())
    }

    /// The path bar's word for a list of found files.
    pub fn found_label(&self) -> Option<&'static str> {
        self.found.is_some().then_some(self.found_label)
    }

    fn show_listed(&mut self, root: PathBuf, paths: Vec<PathBuf>, label: &'static str) {
        self.found_label = label;
        self.remote = None;
        self.archive = None;
        self.marked.clear();
        self.measured.clear();
        self.path = root;
        self.found = Some(paths);
        let entries = self.found_entries();
        self.show(entries, None);
    }

    /// Showing search results.
    pub fn is_found(&self) -> bool {
        self.found.is_some()
    }

    /// The found that are still there.
    fn found_entries(&self) -> Vec<Entry> {
        let mut entries = vec![parent_entry(None)];
        for path in self.found.iter().flatten() {
            let Ok(meta) = fs::metadata(path).or_else(|_| fs::symlink_metadata(path)) else {
                continue;
            };
            let (hidden, system, attributes) = attributes(path.file_name().unwrap_or_default(), Some(&meta));
            entries.push(Entry {
                name: path.strip_prefix(&self.path).unwrap_or(path).as_os_str().to_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
                modified: meta.modified().ok(),
                hidden,
                system,
                attributes,
                tags: tags::colours(path),
                online: online_only(&meta),
            });
        }
        entries
    }

    /// The folder, local or on the server: inside an archive, the archive's.
    pub fn folder(&self) -> &Path {
        match self.archive {
            Some(_) => {
                let archive = self.archive_path();
                archive.parent().unwrap_or(archive)
            }
            None => &self.path,
        }
    }

    /// As Total Commander's status bar: marked / all.
    /// Measured folders count with their contents.
    pub fn status_text(&self) -> String {
        let listed = self.entries.iter().filter(|e| !e.is_parent());
        let (dirs, files): (Vec<&Entry>, Vec<&Entry>) = listed.partition(|e| e.is_dir);
        let marked = |list: &[&Entry]| list.iter().filter(|e| self.is_marked(e)).count();
        let all = || files.iter().chain(&dirs);
        let bytes = |list: &mut dyn Iterator<Item = &&Entry>| {
            list.filter_map(|e| self.measured_size(e)).sum::<u64>() / 1024
        };
        format!(
            "{} k / {} k in {} / {} file(s), {} / {} dir(s)",
            grouped(bytes(&mut all().filter(|e| self.is_marked(e)))),
            grouped(bytes(&mut all())),
            marked(&files),
            files.len(),
            marked(&dirs),
            dirs.len()
        )
    }

    fn go_to(&mut self, path: PathBuf, select: Option<OsString>) -> io::Result<()> {
        let entries = match &self.remote {
            Some(remote) => remote.entries(&path)?,
            None => read_dir(&path)?,
        };
        if path != self.path || self.archive.is_some() || self.found.is_some() {
            self.moved();
        }
        self.archive = None;
        self.found = None;
        self.path = path;
        self.show(entries, select);
        Ok(())
    }

    fn show_archive_dir(&mut self, dir: PathBuf, select: Option<OsString>) {
        let archive = self.archive.as_mut().expect("in an archive");
        let entries = archive::entries(&archive.items, &dir);
        if archive.dir != dir {
            self.moved();
        }
        let archive = self.archive.as_mut().expect("in an archive");
        archive.dir = dir;
        self.show(entries, select);
    }

    /// Another folder: no marks, no sizes measured, Only Selected Files over.
    fn moved(&mut self) {
        self.marked.clear();
        self.measured.clear();
        if matches!(self.file_filter, FileFilter::Selected(_) | FileFilter::Quick(_)) {
            self.file_filter = FileFilter::All;
        }
    }

    fn show(&mut self, mut entries: Vec<Entry>, select: Option<OsString>) {
        // What a search found shows, hidden or not.
        entries.retain(|entry| self.found.is_some() || (self.filter.shows(entry) && self.file_filter.shows(entry)));
        // Marks stay on what is still there.
        let names: HashSet<&OsString> = entries.iter().map(|e| &e.name).collect();
        self.marked.retain(|name| names.contains(name));
        entries.sort_by(|a, b| self.sort.compare(a, b));
        self.cursor = select
            .and_then(|name| entries.iter().position(|e| e.name == name))
            .unwrap_or(0);
        self.entries = entries;
    }

    /// Neither locked nor named.
    /// To show elsewhere, another tab or the other panel: unlocked, unnamed, nothing marked.
    pub fn copy_elsewhere(&self) -> Self {
        let mut copy = self.plain_copy();
        copy.marked.clear();
        copy
    }

    fn plain_copy(&self) -> Self {
        Self {
            locked: false,
            name: None,
            ..self.clone()
        }
    }
}

const CLOSED_TABS: usize = 10;

/// A side's folder tabs; there is always one.
pub struct Tabs {
    panels: Vec<Panel>,
    active: usize,
    /// Most recent first.
    closed: Vec<Panel>,
}

impl Tabs {
    pub fn new(panel: Panel) -> Self {
        Self {
            panels: vec![panel],
            active: 0,
            closed: Vec::new(),
        }
    }

    /// Tabs that cannot be opened any more are left out; with none, `fallback`.
    pub fn restore(state: &SideState, filter: Filter, fallback: impl FnOnce() -> Panel) -> Self {
        let mut panels: Vec<Panel> = state
            .tabs
            .iter()
            .filter_map(|tab| Panel::restore(tab, filter).ok())
            .collect();
        if panels.is_empty() {
            panels.push(fallback());
        }
        Self {
            active: state.active.min(panels.len() - 1),
            panels,
            closed: Vec::new(),
        }
    }

    pub fn state(&self) -> SideState {
        SideState {
            tabs: self.panels.iter().map(Panel::state).collect(),
            active: self.active,
        }
    }

    pub fn panels(&self) -> &[Panel] {
        &self.panels
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn current(&self) -> &Panel {
        &self.panels[self.active]
    }

    pub fn current_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }

    pub fn closed(&self) -> &[Panel] {
        &self.closed
    }

    pub fn duplicate(&mut self) {
        let copy = self.current().plain_copy();
        self.add(copy);
    }

    /// A new last tab, which becomes current.
    pub fn add(&mut self, panel: Panel) {
        self.panels.push(panel);
        self.active = self.panels.len() - 1;
    }

    pub fn select(&mut self, ix: usize) {
        if ix < self.panels.len() {
            self.active = ix;
        }
    }

    /// The next (or with -1 the previous) tab, round from the end.
    pub fn step(&mut self, by: isize) {
        let count = self.panels.len() as isize;
        self.active = (self.active as isize + by).rem_euclid(count) as usize;
    }

    /// Moves in the current tab; from a locked tab the move opens a new tab.
    pub fn navigate(&mut self, go: impl FnOnce(&mut Panel) -> io::Result<()>) -> io::Result<()> {
        let before = self.current().place();
        if !self.current().locked {
            go(self.current_mut())?;
            let panel = self.current_mut();
            let after = panel.place();
            panel.history.moved(before, after);
            return Ok(());
        }
        let mut moved = self.current().plain_copy();
        go(&mut moved)?;
        if moved.path() != self.current().path() {
            let after = moved.place();
            moved.history.moved(before, after);
            self.add(moved);
        }
        Ok(())
    }

    /// A move made on a copy of tab `ix` elsewhere, as for a server: the
    /// tab takes it, or a locked one gets a new tab beside it.
    pub fn navigated(&mut self, ix: usize, mut moved: Panel) {
        let Some(panel) = self.panels.get(ix) else {
            return;
        };
        // The tab's history goes on, also where the other panel's folder came.
        moved.history = panel.history.clone();
        let (before, after) = (panel.place(), moved.place());
        moved.history.moved(before, after);
        if !panel.locked {
            self.panels[ix] = moved;
        } else if moved.path() != panel.path() || moved.location() != panel.location() {
            self.add(moved.plain_copy());
        }
    }

    /// Tabs showing `folder` read it again.
    pub fn reload_folder(&mut self, folder: &Path) {
        for panel in &mut self.panels {
            if panel.archive.is_none() && panel.remote.is_none() && panel.path == folder {
                // shortcut: a tab that cannot read its folder keeps its old list.
                let _ = panel.reload();
            }
        }
    }

    /// Every tab, closed ones too, lists again by `filter`.
    /// Every tab in the sort order of now, as after `set_folders_like_files`.
    pub fn resort(&mut self) {
        for panel in self.panels.iter_mut().chain(&mut self.closed) {
            panel.set_sort(panel.sort);
        }
    }

    pub fn set_filter(&mut self, filter: Filter) {
        for panel in self.panels.iter_mut().chain(&mut self.closed) {
            panel.filter = filter;
            // shortcut: a tab whose folder is gone keeps its old list until it moves.
            let _ = panel.reload();
        }
    }

    pub fn toggle_lock(&mut self) {
        let panel = self.current_mut();
        panel.locked = !panel.locked;
    }

    /// An empty name gives the tab its folder's name again.
    pub fn rename(&mut self, ix: usize, name: &str) {
        if let Some(panel) = self.panels.get_mut(ix) {
            let name = name.trim();
            panel.name = (!name.is_empty()).then(|| name.to_owned());
        }
    }

    /// The last tab stays.
    pub fn close(&mut self) {
        if self.panels.len() > 1 {
            let panel = self.panels.remove(self.active);
            self.remember(panel);
            self.active = self.active.min(self.panels.len() - 1);
        }
    }

    /// All but the current and the locked tabs.
    pub fn close_all(&mut self) {
        self.close_where(|panel| !panel.locked);
    }

    /// Tabs on a folder that a kept tab shows too; current and locked tabs stay.
    pub fn close_duplicates(&mut self) {
        let mut shown: HashSet<PathBuf> = self
            .panels
            .iter()
            .enumerate()
            .filter(|(ix, panel)| *ix == self.active || panel.locked)
            .map(|(_, panel)| panel.path())
            .collect();
        self.close_where(|panel| !panel.locked && !shown.insert(panel.path()));
    }

    /// In tab order; never the current tab.
    fn close_where(&mut self, mut close: impl FnMut(&Panel) -> bool) {
        let mut closed = Vec::new();
        let active = self.active;
        for (ix, panel) in std::mem::take(&mut self.panels).into_iter().enumerate() {
            if ix != active && close(&panel) {
                closed.push(panel);
            } else {
                if ix == active {
                    self.active = self.panels.len();
                }
                self.panels.push(panel);
            }
        }
        for panel in closed {
            self.remember(panel);
        }
    }

    fn remember(&mut self, panel: Panel) {
        self.closed.insert(0, panel);
        self.closed.truncate(CLOSED_TABS);
    }

    /// A recently closed tab again, its folder read anew.
    pub fn reopen(&mut self, ix: usize) -> io::Result<()> {
        if ix < self.closed.len() {
            let mut panel = self.closed.remove(ix);
            panel.reload()?;
            self.add(panel);
        }
        Ok(())
    }
}

/// Save Selection To File: a name a line.
/// Synchronized browsing, as Krusader's: where a panel at `here` goes when the
/// other moved from `from` to `to`: as many folders up (no higher than its
/// root), then down by the same names. Only up, the folder it left gets the
/// cursor, as Up's. None for a move to another drive.
fn followed(from: &Path, to: &Path, here: &Path) -> Option<(PathBuf, Option<OsString>)> {
    let same = from.components().zip(to.components()).take_while(|(a, b)| a == b).count();
    if same == 0 {
        return None;
    }
    let up = from.components().count() - same;
    let above: Vec<&Path> = here.ancestors().collect();
    let top = above[up.min(above.len() - 1)];
    let down: PathBuf = to.components().skip(same).collect();
    if !down.as_os_str().is_empty() {
        return Some((top.join(down), None));
    }
    let left = up.min(above.len() - 1).checked_sub(1).and_then(|ix| above[ix].file_name());
    Some((top.to_owned(), left.map(OsString::from)))
}

pub fn save_selection(path: &Path, names: &[OsString]) -> io::Result<()> {
    let lines: Vec<String> = names
        .iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    fs::write(path, lines.join("\n") + "\n")
}

/// Load Selection From File: the names it lists.
pub fn load_selection(path: &Path) -> io::Result<Vec<OsString>> {
    let text = fs::read_to_string(path)?;
    let names = text.lines().map(|line| line.trim_end_matches('\r'));
    Ok(names
        .filter(|line| !line.is_empty())
        .map(OsString::from)
        .collect())
}

/// Unsorted, `..` first but in the root.
fn read_dir(path: &Path) -> io::Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = fs::read_dir(path)?
        // Skip unreadable entries, not the whole folder.
        .filter_map(Result::ok)
        .map(|item| {
            // Follow links; a broken link stays itself.
            let meta = fs::metadata(item.path()).or_else(|_| item.metadata()).ok();
            let name = item.file_name();
            let (hidden, system, attributes) = attributes(&name, meta.as_ref());
            Entry {
                tags: tags::colours(&item.path()),
                online: meta.as_ref().is_some_and(online_only),
                name,
                is_dir: meta.as_ref().is_some_and(fs::Metadata::is_dir),
                size: meta.as_ref().map_or(0, fs::Metadata::len),
                modified: meta.and_then(|m| m.modified().ok()),
                hidden,
                system,
                attributes,
            }
        })
        .collect();
    if path.parent().is_some() {
        let modified = fs::metadata(path.join(PARENT))
            .and_then(|m| m.modified())
            .ok();
        entries.insert(0, parent_entry(modified));
    }
    Ok(entries)
}

/// Thousands apart: `1 234 567`.
pub fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut text = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            text.push(' ');
        }
        text.push(digit);
    }
    text
}

fn parent_entry(modified: Option<SystemTime>) -> Entry {
    Entry {
        name: PARENT.into(),
        is_dir: true,
        size: 0,
        modified,
        hidden: false,
        system: false,
        attributes: String::new(),
        tags: Vec::new(),
        online: false,
    }
}

/// A cloud file whose data is not here (OneDrive's, iCloud's…): reading it downloads it.
pub fn online_only(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const OFFLINE: u32 = 0x1000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
        !meta.is_dir() && meta.file_attributes() & (OFFLINE | RECALL_ON_DATA_ACCESS) != 0
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        const SF_DATALESS: u32 = 0x4000_0000;
        !meta.is_dir() && meta.st_flags() & SF_DATALESS != 0
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = meta;
        false
    }
}

/// Hidden, system, and the Attr column's text.
#[cfg(windows)]
fn attributes(_: &OsStr, meta: Option<&fs::Metadata>) -> (bool, bool, String) {
    use std::os::windows::fs::MetadataExt;
    const READ_ONLY: u32 = 0x1;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    const ARCHIVE: u32 = 0x20;
    let bits = meta.map_or(0, MetadataExt::file_attributes);
    let flag = |bit, letter| if bits & bit != 0 { letter } else { '-' };
    let text = [
        flag(READ_ONLY, 'r'),
        flag(ARCHIVE, 'a'),
        flag(HIDDEN, 'h'),
        flag(SYSTEM, 's'),
    ];
    (
        bits & HIDDEN != 0,
        bits & SYSTEM != 0,
        text.iter().collect(),
    )
}

/// The files under `dir`, as `filter` shows hidden and system ones; links to folders are not followed.
fn files_under(dir: &Path, filter: Filter, out: &mut Vec<PathBuf>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let meta = entry.metadata().ok();
        let (hidden, system, _) = attributes(&entry.file_name(), meta.as_ref());
        if hidden && !filter.hidden || system && !filter.system {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files_under(&entry.path(), filter, out),
            Ok(_) => out.push(entry.path()),
            Err(_) => {}
        }
    }
}

/// Hidden, system, and the Attr column's text.
#[cfg(unix)]
fn attributes(name: &OsStr, meta: Option<&fs::Metadata>) -> (bool, bool, String) {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.map_or(0, |meta| meta.permissions().mode());
    let text = (0..9)
        .map(|i| match mode & (0o400 >> i) {
            0 => '-',
            _ => b"rwxrwxrwx"[i] as char,
        })
        .collect();
    (dot_file(name), false, text)
}

pub(crate) fn dot_file(name: &OsStr) -> bool {
    cfg!(not(windows)) && name.as_encoded_bytes().first() == Some(&b'.')
}

fn by_name(a: &Entry, b: &Entry) -> Ordering {
    let key = |e: &Entry| e.name.to_string_lossy().to_lowercase();
    key(a).cmp(&key(b)).then_with(|| a.name.cmp(&b.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_other_panel() {
        let at = |from: &str, to: &str, here: &str| followed(Path::new(from), Path::new(to), Path::new(here));
        let some = |path: &str, left: Option<&str>| Some((PathBuf::from(path), left.map(OsString::from)));
        assert_eq!(at("/a/b", "/a/b/c", "/x/y"), some("/x/y/c", None));
        assert_eq!(at("/a/b/c", "/a/b", "/x/y/c"), some("/x/y", Some("c")));
        assert_eq!(at("/a/b/c", "/a/d", "/x/y/c"), some("/x/d", None));
        // No higher than its root.
        assert_eq!(at("/a/b/c", "/", "/x"), some("/", Some("x")));
        assert_eq!(at("/a", "/", "/"), some("/", None));
        assert_eq!(at("a/b", "c/d", "/x"), None);
    }

    #[test]
    fn shows_search_results() {
        let root = std::env::temp_dir().join(format!("sc-core-found-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/deep")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/deep/x.txt"), "x").unwrap();
        fs::write(root.join("b/.y.txt"), "yy").unwrap();
        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        panel.show_found(vec![root.join("a/deep/x.txt"), root.join("b/.y.txt"), root.join("a/deep")]);
        let names: Vec<_> = panel.entries().iter().map(Entry::display_name).collect();
        // Hidden or not; the folder first, as folders go.
        assert_eq!(names, ["..", "a/deep", "a/deep/x.txt", "b/.y.txt"]);
        assert_eq!((panel.path(), panel.name()), (root.clone(), "Search results".to_owned()));
        assert_eq!(panel.origin_of(&panel.entries()[2]).unwrap().name(), root.join("a/deep/x.txt").display().to_string());
        fs::remove_file(root.join("b/.y.txt")).unwrap();
        panel.reload().unwrap();
        assert_eq!(panel.entries().len(), 3, "a file gone goes from the list");
        panel.set_cursor(1);
        assert!(panel.open().unwrap());
        assert_eq!(panel.path(), root.join("a/deep"));
        assert!(!panel.is_found());
        panel.show_found(vec![root.join("a/deep/x.txt")]);
        assert_eq!(panel.path(), root.join("a/deep"), "one file: its folder");
        panel.up().unwrap();
        assert!(!panel.is_found());
        assert_eq!(panel.path(), root.join("a/deep"), "`..` leaves for the folder");
        panel.go_to_file(&root.join("b"), None).unwrap();
        assert_eq!(panel.current_entry().unwrap().name, "b");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renames() {
        let root = std::env::temp_dir().join(format!("sc-core-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(root.join("sub/b.txt"), "b").unwrap();
        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        panel.rename_entry(OsStr::new("a.txt"), " c.txt ").unwrap();
        assert!(root.join("c.txt").is_file() && !root.join("a.txt").exists());
        assert_eq!(panel.current_entry().unwrap().name, "c.txt", "the cursor follows");
        assert!(panel.rename_entry(OsStr::new("c.txt"), "sub").is_err(), "taken");
        assert!(panel.rename_entry(OsStr::new("c.txt"), "x/y").is_err(), "not a name");
        assert!(panel.rename_entry(OsStr::new("c.txt"), "..").is_err(), "not a name");
        panel.rename_entry(OsStr::new("c.txt"), "C.txt").unwrap();
        panel.rename_entry(OsStr::new("C.txt"), "c.txt").unwrap();
        panel.show_found(vec![root.join("c.txt"), root.join("sub/b.txt")]);
        panel.rename_entry(OsStr::new("sub/b.txt"), "d.txt").unwrap();
        assert!(root.join("sub/d.txt").is_file());
        let names: Vec<_> = panel.entries().iter().map(Entry::display_name).collect();
        assert_eq!(names, ["..", "c.txt", "sub/d.txt"]);
        assert_eq!(panel.current_entry().unwrap().display_name(), "sub/d.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn navigates() {
        let root = std::env::temp_dir().join(format!("sc-core-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("b-dir/inner")).unwrap();
        fs::create_dir(root.join("A-dir")).unwrap();
        fs::write(root.join("a-file"), "12345").unwrap();

        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        let names: Vec<_> = panel.entries().iter().map(Entry::display_name).collect();
        assert_eq!(names, ["..", "A-dir", "b-dir", "a-file"]);
        assert_eq!(panel.entries()[3].size_text(), "5");
        assert_eq!(
            panel.status_text(),
            "0 k / 0 k in 0 / 1 file(s), 0 / 2 dir(s)"
        );

        panel.set_cursor(99);
        assert_eq!(panel.cursor(), 3);
        assert!(!panel.open().unwrap(), "a file is not entered");

        panel.move_cursor(-1);
        assert!(panel.open().unwrap());
        assert_eq!(panel.path(), root.join("b-dir"));

        panel.set_cursor(0);
        assert!(panel.open().unwrap(), "`..` goes up");
        assert_eq!(panel.path(), root);
        assert_eq!(panel.cursor(), 2, "on the folder it left");

        fs::remove_dir_all(&root).unwrap();
        panel.move_cursor(-1);
        assert!(panel.open().is_err());
        assert_eq!(panel.path(), root, "stays where it was");
    }

    #[test]
    fn drags_the_marked_or_the_one() {
        let root = std::env::temp_dir().join(format!("pc-drag-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let mut panel = Panel::new(root.clone(), Filter::default()).unwrap();
        let names = |entries: Vec<&Entry>| -> Vec<String> {
            entries.iter().map(|e| e.display_name()).collect()
        };
        assert!(panel.dragged(0).is_empty(), "never ..");
        assert_eq!(names(panel.dragged(2)), ["b"]);
        panel.mark_names(&["a".into(), "c".into()]);
        assert_eq!(names(panel.dragged(1)), ["a", "c"], "a marked one takes all marked");
        assert_eq!(names(panel.dragged(2)), ["b"], "an unmarked one goes alone");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn typed_paths_on_a_server() {
        let (url, folder) = ("sftp://me@host", Path::new("/home/me"));
        let typed = |text| typed_on_server(url, folder, text);
        assert_eq!(typed("sftp://me@host/srv"), Typed::Here("/srv".into()));
        assert_eq!(typed("docs"), Typed::Here("/home/me/docs".into()));
        assert_eq!(typed("/home/me"), Typed::Local("/home/me".into()));
        let home = std::env::home_dir().unwrap();
        assert_eq!(typed("~/x"), Typed::Local(home.join("x")));
    }

    #[test]
    fn marks_groups_and_compares_folders() {
        let root = std::env::temp_dir().join(format!("pc-mark-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for side in ["l", "r"] {
            fs::create_dir_all(root.join(side).join("docs")).unwrap();
        }
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        let write = |path: &str, text: &str, time: SystemTime| {
            fs::write(root.join(path), text).unwrap();
            fs::File::options()
                .write(true)
                .open(root.join(path))
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        write("l/a.txt", "same", old);
        write("r/a.txt", "same", old);
        write("l/b.txt", "newer", SystemTime::now());
        write("r/b.txt", "older", old);
        write("l/c.md", "only left", old);
        write("r/d.md", "only right", old);
        let mut left = Panel::new(root.join("l"), Filter::default()).unwrap();
        let mut right = Panel::new(root.join("r"), Filter::default()).unwrap();
        left.mark_group("*.TXT docs/", true);
        let names = |panel: &Panel| {
            let mut names: Vec<String> = panel
                .marked_names()
                .iter()
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&left), ["a.txt", "b.txt", "docs"]);
        left.mark_group("a.*", false);
        assert_eq!(names(&left), ["b.txt", "docs"]);
        let saved = left.marked_names();
        left.mark_all(false);
        left.mark_names(&saved);
        assert_eq!(names(&left), ["b.txt", "docs"]);
        save_selection(&root.join("sel.txt"), &left.marked_names()).unwrap();
        assert_eq!(load_selection(&root.join("sel.txt")).unwrap().len(), 2);
        let line = if cfg!(windows) { "\r\n" } else { "\n" };
        assert_eq!(
            left.names_text(false, false),
            format!("docs{line}b.txt{line}"),
            "folders first"
        );
        assert_eq!(left.compare_with(&mut right, true), 3);
        assert_eq!(names(&left), ["b.txt", "c.md"]);
        assert_eq!(names(&right), ["d.md"]);
        assert!(
            !left.entries().iter().any(|e| e.name == "a.txt"),
            "the same file is hidden"
        );
        left.set_cursor(
            left.entries()
                .iter()
                .position(|e| e.name == "c.md")
                .unwrap(),
        );
        left.mark_all(false);
        left.mark_same_extension();
        assert_eq!(names(&left), ["c.md"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn opens_tabs() {
        let mut tabs = Tabs::new(Panel::new("/", Filter::default()).unwrap());
        assert_eq!(tabs.current().title(), "/");
        tabs.current_mut().set_cursor(1);
        tabs.duplicate();
        assert_eq!((tabs.panels().len(), tabs.active()), (2, 1));
        assert_eq!(tabs.current().cursor(), 1, "a copy of the current tab");
        tabs.current_mut().open().unwrap();
        assert_ne!(tabs.current().path(), tabs.panels()[0].path());
        tabs.select(5);
        assert_eq!(tabs.active(), 1);
        tabs.select(0);
        assert_eq!(tabs.current().path(), Path::new("/"));
        tabs.step(-1);
        assert_eq!(tabs.active(), 1, "round from the first to the last");
        tabs.step(1);
        assert_eq!(tabs.active(), 0);
    }

    #[test]
    fn locks_and_closes_tabs() {
        let root = std::env::temp_dir().join(format!("pc-tabs-test-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).unwrap();
        let mut tabs = Tabs::new(Panel::new(&root, Filter::default()).unwrap());
        tabs.toggle_lock();
        assert!(tabs.current().title().starts_with("*pc-tabs-test-"));
        tabs.current_mut().set_cursor(1);
        tabs.navigate(|panel| panel.open().map(drop)).unwrap();
        assert_eq!(
            (tabs.panels().len(), tabs.active()),
            (2, 1),
            "moved in a new tab"
        );
        assert_eq!(tabs.panels()[0].path(), root, "the locked tab stayed");
        assert_eq!(tabs.current().path(), root.join("sub"));
        assert!(!tabs.current().is_locked());

        tabs.duplicate();
        tabs.duplicate();
        tabs.select(2);
        tabs.close_all();
        assert_eq!(
            (tabs.panels().len(), tabs.active()),
            (2, 1),
            "the locked and the current"
        );
        tabs.close();
        tabs.close();
        assert_eq!(tabs.panels().len(), 1, "the last stays");
        assert!(tabs.current().is_locked());

        assert_eq!(tabs.closed().len(), 3, "the closed tabs, most recent first");
        tabs.reopen(0).unwrap();
        assert_eq!((tabs.panels().len(), tabs.active()), (2, 1));
        tabs.duplicate();
        tabs.select(0);
        tabs.close_duplicates();
        assert_eq!(tabs.panels().len(), 2, "one of the two on the same folder");

        tabs.rename(0, " Work ");
        assert_eq!(tabs.panels()[0].title(), "*Work");
        let restored = Tabs::restore(&tabs.state(), Filter::default(), || unreachable!());
        assert_eq!(restored.panels()[0].title(), "*Work");
        assert_eq!(restored.panels()[1].path(), tabs.panels()[1].path());
        tabs.rename(0, "");
        assert!(tabs.panels()[0].title().starts_with("*pc-tabs-test-"));

        fs::remove_dir_all(root.join("sub")).unwrap();
        let gone = Tabs::restore(&tabs.state(), Filter::default(), || unreachable!());
        assert_eq!(gone.panels()[1].path(), root, "the nearest folder left");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg(not(windows))]
    fn hides_dot_files() {
        let root = std::env::temp_dir().join(format!("pc-filter-test-{}", std::process::id()));
        fs::create_dir_all(root.join(".hidden-dir")).unwrap();
        fs::write(root.join("shown"), "").unwrap();
        let mut tabs = Tabs::new(Panel::new(&root, Filter::default()).unwrap());
        let names = |tabs: &Tabs| -> Vec<String> {
            tabs.current()
                .entries()
                .iter()
                .map(Entry::display_name)
                .collect()
        };
        assert_eq!(names(&tabs), ["..", "shown"]);
        tabs.set_filter(Filter {
            hidden: true,
            system: false,
        });
        assert_eq!(names(&tabs), ["..", ".hidden-dir", "shown"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sorts_creates_and_deletes() {
        let root = std::env::temp_dir().join(format!("pc-files-test-{}", std::process::id()));
        fs::create_dir_all(root.join("z-dir")).unwrap();
        fs::write(root.join("b.txt"), "12345").unwrap();
        fs::write(root.join("a.rs"), "1").unwrap();
        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        let names = |panel: &Panel| -> Vec<String> {
            panel.entries().iter().map(Entry::display_name).collect()
        };
        assert_eq!(names(&panel), ["..", "z-dir", "a.rs", "b.txt"]);
        panel.set_cursor(2);
        panel.set_sort(Sort::by(SortKey::Size));
        assert_eq!(
            names(&panel),
            ["..", "z-dir", "b.txt", "a.rs"],
            "largest first"
        );
        assert_eq!(
            panel.current_entry().unwrap().display_name(),
            "a.rs",
            "cursor kept"
        );
        panel.set_sort(Sort {
            key: SortKey::Extension,
            descending: true,
        });
        assert_eq!(names(&panel), ["..", "z-dir", "b.txt", "a.rs"]);

        panel.new_folder("new/inner").unwrap();
        assert_eq!(panel.current_entry().unwrap().display_name(), "new");
        assert!(root.join("new/inner").is_dir());
        assert!(panel.new_folder("new").is_err(), "exists already");
        panel.delete(false).unwrap();
        assert!(!root.join("new").exists());
        assert_eq!(names(&panel), ["..", "z-dir", "b.txt", "a.rs"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn marks_what_operations_work_on() {
        let root = std::env::temp_dir().join(format!("pc-marks-test-{}", std::process::id()));
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("a.txt"), vec![0; 2048]).unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        let chosen = |panel: &Panel| -> Vec<String> {
            panel.chosen().iter().map(|e| e.display_name()).collect()
        };
        assert!(chosen(&panel).is_empty(), "not ..");
        panel.toggle_mark();
        assert_eq!(panel.cursor(), 1, "not .. but the cursor moves on");
        panel.set_cursor(2);
        assert_eq!(chosen(&panel), ["a.txt"], "the cursor's, none marked");
        panel.toggle_mark();
        panel.toggle_mark();
        assert_eq!(chosen(&panel), ["a.txt", "b.txt"]);
        assert_eq!(
            panel.status_text(),
            "2 k / 2 k in 2 / 2 file(s), 0 / 1 dir(s)"
        );
        panel.reload().unwrap();
        assert_eq!(chosen(&panel).len(), 2, "kept on reload");
        panel.invert_marks();
        assert_eq!(chosen(&panel), ["dir"]);
        panel.mark_all(true);
        assert_eq!(chosen(&panel).len(), 3);
        panel.delete(false).unwrap();
        assert_eq!(panel.entries().len(), 1, "only .. left");
        fs::create_dir(root.join("again")).unwrap();
        panel.reload().unwrap();
        panel.mark_all(true);
        panel.set_cursor(1);
        panel.open().unwrap();
        assert!(chosen(&panel).is_empty(), "a new folder, no marks");
        fs::remove_dir_all(&root).unwrap();
    }

    /// Ctrl+B: every file under the folder by its path from it; hidden ones as shown; again, the folder.
    #[test]
    fn shows_a_branch() {
        let root = std::env::temp_dir().join(format!("pc-branch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for file in ["a/b/c.txt", "a/d.txt", "e.txt", "f/g.txt", ".hid/h.txt"] {
            fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            fs::write(root.join(file), "x").unwrap();
        }
        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        let names = |panel: &Panel| {
            let mut names: Vec<String> = panel.entries().iter().skip(1).map(|e| e.name.to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        panel.toggle_branch(false).unwrap();
        assert_eq!(panel.found_label(), Some("Branch view:"));
        assert_eq!(panel.path(), root, "the folder it is of");
        assert_eq!(names(&panel), ["a/b/c.txt", "a/d.txt", "e.txt", "f/g.txt"]);
        panel.toggle_branch(false).unwrap();
        assert_eq!(panel.found_label(), None);
        assert!(names(&panel).contains(&"a".to_owned()), "the folder again");
        // Only the marked folders'.
        let at = panel.entries().iter().position(|e| e.name == "f").unwrap();
        panel.set_cursor(at);
        panel.toggle_mark();
        panel.toggle_branch(true).unwrap();
        assert_eq!(names(&panel), ["f/g.txt"]);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Back and Forward as a browser's: a new place drops those ahead; 20 kept.
    #[test]
    fn keeps_where_tabs_have_been() {
        let mut history = History::default();
        let place = |n: usize| Some(format!("/{n}"));
        history.moved(place(0), place(1));
        history.moved(place(1), place(2));
        assert_eq!(history.step(-1).as_deref(), Some("/1"));
        assert_eq!(history.step(-1).as_deref(), Some("/0"));
        assert_eq!(history.step(-1), None, "nothing before the first");
        assert_eq!(history.step(1).as_deref(), Some("/1"));
        // Arrived where Back went: nothing new.
        history.moved(place(2), place(1));
        assert_eq!(history.places.len(), 3);
        history.moved(place(1), place(9));
        assert_eq!(history.places, ["/0", "/1", "/9"], "the way ahead is dropped");
        assert_eq!(history.step(1), None);
        history.moved(None, None);
        for n in 10..40 {
            history.moved(place(n - 1), place(n));
        }
        assert_eq!(history.places.len(), HISTORY);
        assert_eq!((history.places[0].as_str(), history.at), ("/20", HISTORY - 1));

        let root = std::env::temp_dir().join(format!("pc-history-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        let mut tabs = Tabs::new(Panel::new(&root, Filter::default()).unwrap());
        tabs.navigate(|panel| panel.go_to_folder(root.join("a"))).unwrap();
        tabs.navigate(|panel| panel.go_to_folder(root.join("a/b"))).unwrap();
        let places = |tabs: &Tabs| tabs.current().history.places.clone();
        let shown = |path: PathBuf| path.display().to_string();
        assert_eq!(places(&tabs), [shown(root.clone()), shown(root.join("a")), shown(root.join("a/b"))]);
        // A move made elsewhere, as for a server, keeps the tab's history.
        let mut copy = tabs.current().clone();
        copy.history = History::default();
        copy.go_to_folder(root.clone()).unwrap();
        tabs.navigated(tabs.active(), copy);
        assert_eq!(places(&tabs).len(), 4);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn filters_files_as_the_show_menu() {
        let root = std::env::temp_dir().join(format!("pc-show-filter-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        for name in ["a.txt", "b.md", "c.txt.bak", "run.exe", "run.sh"] {
            fs::write(root.join(name), "").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut panel = Panel::new(&root, Filter::default()).unwrap();
        let shown = |panel: &mut Panel, filter: FileFilter| {
            panel.set_file_filter(filter).unwrap();
            panel.entries().iter().map(Entry::display_name).collect::<Vec<_>>()
        };
        assert_eq!(shown(&mut panel, FileFilter::Masks("*.txt *.md".into())), ["..", "docs", "a.txt", "b.md"], "folders stay");
        assert_eq!(shown(&mut panel, FileFilter::Masks("*.txt* | *.bak".into())), ["..", "docs", "a.txt"]);
        let programs = shown(&mut panel, FileFilter::Programs);
        assert_eq!(programs[2..], [if cfg!(windows) { "run.exe" } else { "run.sh" }]);
        panel.go_to_folder(root.join("docs")).unwrap();
        panel.up().unwrap();
        assert_eq!(panel.entries().len(), 3, "the filter stays as the panel moves");
        let all = shown(&mut panel, FileFilter::All);
        assert_eq!(all.len(), 7);
        panel.mark_group("a.txt docs/", true);
        assert_eq!(shown(&mut panel, FileFilter::Selected(HashSet::new())), ["..", "docs", "a.txt"]);
        panel.go_to_folder(root.join("docs")).unwrap();
        assert_eq!(panel.file_filter(), &FileFilter::All, "only selected ends in another folder");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finds_what_is_typed() {
        let root = std::env::temp_dir().join(format!("pc-quick-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Bundles")).unwrap();
        for name in ["base.txt", "base.history", "Stored.md"] {
            fs::write(root.join(name), "").unwrap();
        }
        let panel = Panel::new(&root, Filter::default()).unwrap();
        let name = |ix: Option<usize>| ix.map(|ix| panel.entries()[ix].display_name());
        // `..`, Bundles, base.history, base.txt, Stored.md
        assert_eq!(name(panel.quick_find("b", 0)), Some("Bundles".into()));
        assert_eq!(name(panel.quick_find("b", 2)), Some("base.history".into()), "from the cursor on");
        assert_eq!(name(panel.quick_find("BASE.T", 2)), Some("base.txt".into()), "any case");
        assert_eq!(name(panel.quick_find("s", 4)), Some("Stored.md".into()));
        assert_eq!(name(panel.quick_find("bu", 3)), Some("Bundles".into()), "round to the top");
        assert_eq!(panel.quick_find(".", 0), None, "never ..");
        assert_eq!(panel.quick_find("x", 0), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sorts_folders_by_name_or_as_files() {
        let entry = |name: &str, is_dir, age: u64| Entry {
            name: name.into(),
            is_dir,
            size: 0,
            modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(age)),
            hidden: false,
            system: false,
            attributes: String::new(),
            tags: Vec::new(),
            online: false,
        };
        let mut entries = vec![entry("old.txt", false, 1), entry("b", true, 9), entry("a", true, 1), entry("new.txt", false, 9)];
        let order = |entries: &mut Vec<Entry>, like_files| {
            entries.sort_by(|a, b| Sort::by(SortKey::Modified).compare_folders(a, b, like_files));
            entries.iter().map(Entry::display_name).collect::<Vec<_>>()
        };
        assert_eq!(order(&mut entries, false), ["a", "b", "new.txt", "old.txt"]);
        assert_eq!(order(&mut entries, true), ["b", "a", "new.txt", "old.txt"], "newest first, folders still first");
    }

    #[test]
    fn splits_names_and_shows_attributes() {
        let entry = |name: &str, is_dir| Entry {
            name: name.into(),
            is_dir,
            size: 0,
            modified: None,
            hidden: false,
            system: false,
            attributes: String::new(),
            tags: Vec::new(),
            online: false,
        };
        let parts = |name, is_dir| entry(name, is_dir).name_and_extension();
        assert_eq!(
            parts("backup.tar.gz", false),
            ("backup.tar".into(), "gz".into())
        );
        assert_eq!(parts(".bashrc", false), (".bashrc".into(), String::new()));
        assert_eq!(
            parts("v1.2", true),
            ("v1.2".into(), String::new()),
            "folders whole"
        );
        assert_eq!(
            crate::archive::tests::attributes_of(" -rw-r--r--"),
            "rw-r--r--"
        );
        assert_eq!(crate::archive::tests::attributes_of("RA"), "ra--");
        #[cfg(unix)]
        {
            let root = std::env::temp_dir().join(format!("pc-attr-test-{}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            let file = root.join("f");
            fs::write(&file, "").unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
            let panel = Panel::new(&root, Filter::default()).unwrap();
            assert_eq!(panel.entries()[1].attributes, "rw-r-----");
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn shows_sizes_for_people() {
        let sizes = [1023, 1536, 5 << 20, 3 << 30].map(human_size);
        assert_eq!(sizes, ["1023 B", "1.5 KB", "5.0 MB", "3.00 GB"]);
    }

    #[test]
    fn separates_thousands() {
        let entry = |size| Entry {
            name: "f".into(),
            is_dir: false,
            size,
            modified: None,
            hidden: false,
            system: false,
            attributes: String::new(),
            tags: Vec::new(),
            online: false,
        };
        assert_eq!(entry(1234567).size_text(), "1 234 567");
        assert_eq!(entry(123456).size_text(), "123 456");
        assert_eq!(entry(0).size_text(), "0");
    }
}
