use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::settings;

/// Both sides' tabs and the window's layout, kept between runs.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// The tabs; the open workspace's.
    #[serde(flatten)]
    pub open: OpenTabs,
    /// Widths of the Name, Ext, Size, Date and Attr columns; Name 0 fills the rest.
    pub columns: Vec<f32>,
    /// The left panel's share of the width.
    pub split: f32,
    /// The sidebar's tab; none while it is hidden.
    pub sidebar: Option<crate::sidebar::SidebarTab>,
    pub sidebar_width: f32,
    /// The panels one above the other.
    pub vertical: bool,
    /// The terminal's height in pixels; 0 before it was dragged.
    pub terminal_height: f32,
}

/// Both panels' tabs and the terminal's: what a workspace keeps.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenTabs {
    pub sides: Vec<SideState>,
    /// The side the keys go to.
    pub active: usize,
    /// The terminal was shown: it opens again, as Krusader's.
    pub terminal: bool,
    /// The terminal's tabs, kept also while it is hidden.
    pub terminals: Vec<TerminalState>,
    pub terminal_tab: usize,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SideState {
    pub tabs: Vec<TabState>,
    pub active: usize,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TabState {
    /// The folder, or the archive.
    pub path: PathBuf,
    /// The folder inside the archive.
    pub archive_dir: Option<PathBuf>,
    pub locked: bool,
    pub name: Option<String>,
    /// A server's tab: `path` is on it. Restored as a local tab, not reconnected.
    pub url: Option<String>,
    pub view: crate::View,
    pub history: crate::History,
}

/// A terminal tab: a shell in its folder, or ssh.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalState {
    /// None for ssh.
    pub dir: Option<PathBuf>,
    /// What runs instead of the shell, with its arguments.
    pub program: Option<(String, Vec<String>)>,
}

/// The window's tabs; the terminal's are its own.
pub const WINDOW: &str = "session.json";
pub const TERMINAL: &str = "session-terminal.json";
pub const WORKSPACES: &str = "workspaces.json";

impl Session {
    pub fn load() -> io::Result<Self> {
        Self::load_from(WINDOW)
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(WINDOW)
    }

    pub fn load_from(file: &str) -> io::Result<Self> {
        settings::read_json(file)
    }

    pub fn save_to(&self, file: &str) -> io::Result<()> {
        settings::write_json(file, self)
    }
}

/// Go > Workspaces: named sets of open tabs; changes go to the open one.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspaces {
    pub list: Vec<Workspace>,
    /// The open one; none before the first is made.
    pub current: Option<usize>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub name: String,
    #[serde(flatten)]
    pub open: OpenTabs,
}

impl Workspaces {
    pub fn load() -> io::Result<Self> {
        let mut workspaces: Self = settings::read_json(WORKSPACES)?;
        workspaces.current = workspaces.current.filter(|&ix| ix < workspaces.list.len());
        Ok(workspaces)
    }

    pub fn save(&self) -> io::Result<()> {
        settings::write_json(WORKSPACES, self)
    }

    /// Why `name` cannot be workspace `ix`'s (none: a new one's).
    pub fn name_error(&self, name: &str, ix: Option<usize>) -> Option<&'static str> {
        if name.trim().is_empty() {
            return Some("A workspace needs a name.");
        }
        let taken = self.list.iter().enumerate().any(|(at, other)| Some(at) != ix && other.name == name.trim());
        taken.then_some("Another workspace has this name.")
    }

    /// The open tabs kept as a new workspace, which becomes the open one.
    pub fn add(&mut self, name: &str, open: OpenTabs) {
        self.list.push(Workspace { name: name.trim().to_owned(), open });
        self.current = Some(self.list.len() - 1);
    }

    /// Gone; the open one gone, none is open.
    pub fn remove(&mut self, ix: usize) {
        if ix >= self.list.len() {
            return;
        }
        self.list.remove(ix);
        self.current = match self.current {
            Some(current) if current == ix => None,
            Some(current) if current > ix => Some(current - 1),
            current => current,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_are_named_once_and_removed_in_place() {
        let mut workspaces = Workspaces::default();
        workspaces.add(" Site ", OpenTabs::default());
        workspaces.add("Book", OpenTabs::default());
        assert_eq!(workspaces.list[0].name, "Site");
        assert_eq!(workspaces.current, Some(1));
        assert!(workspaces.name_error("site", None).is_none());
        assert!(workspaces.name_error("Site ", None).is_some());
        assert!(workspaces.name_error("Site", Some(0)).is_none());
        assert!(workspaces.name_error("  ", Some(0)).is_some());
        workspaces.remove(0);
        assert_eq!(workspaces.current, Some(0));
        workspaces.remove(0);
        assert_eq!(workspaces.current, None);
    }

    #[test]
    fn a_session_saved_before_workspaces_reads() {
        let old = r#"{"sides":[{"tabs":[{"path":"/x"}],"active":0}],"active":1,"split":0.4,"terminal":true}"#;
        let session: Session = serde_json::from_str(old).unwrap();
        assert_eq!(session.open.sides[0].tabs[0].path, PathBuf::from("/x"));
        assert_eq!((session.open.active, session.open.terminal, session.split), (1, true, 0.4));
        assert!(session.open.terminals.is_empty());
    }
}
