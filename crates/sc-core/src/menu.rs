//! The menus, the button bar and the commands; their keys are the key map's.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Command {
    // Files
    /// The newest copy, move, rename or move to the trash undone (`undo`).
    Undo,
    ChangeAttributes,
    Pack,
    UnpackSpecificFiles,
    TestArchives,
    CompareByContent,
    AssociateWith,
    InternalAssociations,
    Properties,
    CalculateOccupiedSpace,
    /// Every folder shown gets its size in the Size column.
    AllFolderSizes,
    /// Links to the chosen in the other panel's folder (`links`).
    CreateLink,
    MultiRenameTool,
    SplitFile,
    CombineFiles,
    CreateChecksumFiles,
    VerifyChecksums,
    Quit,
    // Mark
    SelectGroup,
    UnselectGroup,
    SelectAll,
    UnselectAll,
    InvertSelection,
    SelectAllWithSameExtension,
    SaveSelection,
    RestoreSelection,
    SaveSelectionToFile,
    LoadSelectionFromFile,
    CopyNamesToClipboard,
    CopyNamesWithPathToClipboard,
    CopyWithAllDetails,
    CopyWithPathAndDetails,
    CopyToClipboard,
    CutToClipboard,
    PasteFromClipboard,
    CompareDirectories,
    MarkNewerHideSame,
    // Commands
    /// Gone (Places' bookmarks and the Folders tree do it); kept so old settings files load.
    CdTree,
    Search,
    SearchInSeparateProcess,
    VolumeLabel,
    SystemInformation,
    SynchronizeDirs,
    /// Places' bookmarks in the sidebar: Total Commander's Directory Hotlist.
    #[serde(alias = "DirectoryHotlist")]
    Bookmarks,
    /// The folder shown, into Bookmarks.
    BookmarkFolder,
    /// Bookmarks to add, remove, rename and put in order.
    ManageBookmarks,
    GoBack,
    GoForward,
    /// Alt+Down: the tab's last places, to pick one.
    FolderHistory,
    /// This computer's home folder; the server's on a server.
    GoHome,
    OpenTerminal,
    BranchView,
    /// Branch View of the marked folders only.
    BranchViewSelected,
    /// Gone (Desktop is a first bookmark); kept so old settings files load.
    OpenDesktopFolder,
    BackgroundTransferManager,
    /// What was done to files, and how each ended (`activity`).
    ActivityLog,
    SwapPanels,
    TargetEqualsSource,
    /// The other panel makes the active one's moves too, as Krusader's.
    SyncBrowsing,
    // Show
    Brief,
    Full,
    /// Shows or hides the sidebar's tab.
    #[serde(alias = "Tree")]
    SidebarTree,
    #[serde(alias = "SidebarFavourites")]
    SidebarConnections,
    SidebarPlaces,
    ToggleSidebar,
    ThumbnailView,
    /// Dot files, and Windows' hidden and system files, shown or not.
    HiddenFiles,
    QuickViewPanel,
    /// The built-in terminal under the panels, shown or hidden; its shell lives on.
    TerminalPanel,
    VerticalArrangement,
    DuplicateTab,
    AllFiles,
    Programs,
    AllWithExtension,
    CustomFilter,
    /// Ctrl+I: what is typed hides the names without it, until Escape or another folder.
    QuickFilter,
    OnlySelectedFiles,
    SortByName,
    SortByExtension,
    SortByTime,
    SortBySize,
    Unsorted,
    ReversedOrder,
    RereadSource,
    // Configuration
    Settings,
    Shortcuts,
    // Help
    Website,
    About,
    // Button bar
    View,
    Edit,
    EditExternally,
    ToggleMark,
    OperationQueue,
    /// Gone with the Net menu; kept so old settings files load.
    ConnectToServer,
    Disconnect,
    Copy,
    Move,
    NewFolder,
    Delete,
    DeletePermanently,
    // Tab menu
    RenameTab,
    LockTab,
    CopyTabToOtherPanel,
    CloseTab,
    CloseAllTabs,
    CloseDuplicateTabs,
    ListTabs,
    NextTab,
    PreviousTab,
    // Keys only, as Total Commander's
    SwapPanelsAndTabs,
    /// Ctrl+Left and Ctrl+Right: the other panel's folder, or the one at its cursor, in that panel.
    OpenInLeftPanel,
    OpenInRightPanel,
    GoToRoot,
    OpenInNewTab,
    OpenInOtherNewTab,
    PathToCommandLine,
    NameToCommandLine,
    PathAndNameToCommandLine,
    RenameInPlace,
    NewTextFile,
}

pub enum Item {
    Command {
        label: &'static str,
        command: Command,
    },
    Submenu {
        label: &'static str,
        items: &'static [Item],
    },
    /// System, Light and Dark.
    Appearance {
        label: &'static str,
    },
    /// A submenu of the side's recently closed tabs.
    ClosedTabs {
        label: &'static str,
    },
    Separator,
}

pub struct Menu {
    pub label: &'static str,
    pub items: &'static [Item],
}

const fn cmd(label: &'static str, command: Command) -> Item {
    Item::Command { label, command }
}

const SEP: Item = Item::Separator;

use Command::*;

/// Total Commander's menus in its order, Go added; long lists in
/// submenus, every command that works on one.
pub const MENUS: &[Menu] = &[
    Menu {
        label: "Files",
        items: &[
            cmd("Undo", Undo),
            SEP,
            cmd("View", View),
            cmd("Edit", Edit),
            cmd("Edit With External Editor", EditExternally),
            cmd("New File...", NewTextFile),
            cmd("New Folder...", NewFolder),
            SEP,
            cmd("Copy...", Copy),
            cmd("Move...", Move),
            cmd("Rename", RenameInPlace),
            cmd("Create Link...", CreateLink),
            cmd("Delete", Delete),
            cmd("Delete Permanently", DeletePermanently),
            SEP,
            cmd("Properties...", Properties),
            cmd("Change Attributes...", ChangeAttributes),
            cmd("Calculate Occupied Space...", CalculateOccupiedSpace),
            cmd("Calculate All Folder Sizes", AllFolderSizes),
            cmd("Multi-Rename Tool...", MultiRenameTool),
            cmd("Compare By Content...", CompareByContent),
            SEP,
            Item::Submenu {
                label: "Archive",
                items: &[
                    cmd("Pack...", Pack),
                    cmd("Unpack Specific Files...", UnpackSpecificFiles),
                    cmd("Test Archive(s)", TestArchives),
                ],
            },
            Item::Submenu {
                label: "Checksums",
                items: &[
                    cmd("Create Checksum File(s) (CRC32, MD5, SHA1, SHA256, SHA512)...", CreateChecksumFiles),
                    cmd("Verify Checksums (from checksum files)", VerifyChecksums),
                ],
            },
            Item::Submenu {
                label: "Split and Combine",
                items: &[cmd("Split File...", SplitFile), cmd("Combine Files...", CombineFiles)],
            },
            Item::Submenu {
                label: "Associations",
                items: &[
                    cmd("Associate With...", AssociateWith),
                    cmd("Internal Associations (Spark Commander Only)...", InternalAssociations),
                ],
            },
            SEP,
            cmd("Quit", Quit),
        ],
    },
    Menu {
        label: "Mark",
        items: &[
            cmd("Mark / Unmark", ToggleMark),
            cmd("Select Group...", SelectGroup),
            cmd("Unselect Group...", UnselectGroup),
            cmd("Select All", SelectAll),
            cmd("Unselect All", UnselectAll),
            cmd("Invert Selection", InvertSelection),
            cmd("Select All With Same Extension", SelectAllWithSameExtension),
            Item::Submenu {
                label: "Selection",
                items: &[
                    cmd("Save Selection", SaveSelection),
                    cmd("Restore Selection", RestoreSelection),
                    cmd("Save Selection To File", SaveSelectionToFile),
                    cmd("Load Selection From File", LoadSelectionFromFile),
                ],
            },
            SEP,
            cmd("Copy Files", CopyToClipboard),
            cmd("Cut Files", CutToClipboard),
            cmd("Paste Files", PasteFromClipboard),
            Item::Submenu {
                label: "Copy Names",
                items: &[
                    cmd("Copy Selected Names To Clipboard", CopyNamesToClipboard),
                    cmd("Copy Names With Path To Clipboard", CopyNamesWithPathToClipboard),
                    cmd("Copy To Clipboard With All Details", CopyWithAllDetails),
                    cmd("Copy To Clipboard With Path+Details", CopyWithPathAndDetails),
                ],
            },
            SEP,
            Item::Submenu {
                label: "Compare Directories",
                items: &[
                    cmd("Mark Newer", CompareDirectories),
                    cmd("Mark Newer, Hide Same Files", MarkNewerHideSame),
                ],
            },
        ],
    },
    Menu {
        label: "Go",
        items: &[
            cmd("Back", GoBack),
            cmd("Forward", GoForward),
            cmd("Folder History...", FolderHistory),
            cmd("Home", GoHome),
            cmd("Root Folder", GoToRoot),
            SEP,
            cmd("Bookmarks", Bookmarks),
            cmd("Bookmark This Folder", BookmarkFolder),
            cmd("Manage Bookmarks...", ManageBookmarks),
        ],
    },
    Menu {
        label: "Commands",
        items: &[
            cmd("Search...", Search),
            cmd("Search in Separate Window...", SearchInSeparateProcess),
            cmd("Synchronize Dirs...", SynchronizeDirs),
            SEP,
            cmd("Operation Queue...", OperationQueue),
            cmd("Activity Log...", ActivityLog),
            SEP,
            cmd("Open Terminal Here", OpenTerminal),
            cmd("Branch View (With Subdirs)", BranchView),
            cmd("Branch View of Selected Folders", BranchViewSelected),
            SEP,
            cmd("Swap Panels", SwapPanels),
            cmd("Swap Panels With Their Tabs", SwapPanelsAndTabs),
            cmd("Target = Source", TargetEqualsSource),
            cmd("Synchronized Browsing", SyncBrowsing),
        ],
    },
    Menu {
        label: "Show",
        items: &[
            cmd("Brief", Brief),
            cmd("Full", Full),
            cmd("Thumbnail View", ThumbnailView),
            SEP,
            cmd("Hidden Files", HiddenFiles),
            SEP,
            Item::Submenu {
                label: "Sidebar",
                items: &[
                    cmd("Show Sidebar", ToggleSidebar),
                    SEP,
                    cmd("Folders", SidebarTree),
                    cmd("Connections", SidebarConnections),
                    cmd("Places", SidebarPlaces),
                ],
            },
            cmd("Quick View Panel", QuickViewPanel),
            cmd("Terminal", TerminalPanel),
            cmd("Vertical Arrangement", VerticalArrangement),
            SEP,
            Item::Submenu {
                label: "Filter",
                items: &[
                    cmd("Quick Filter", QuickFilter),
                    SEP,
                    cmd("All Files", AllFiles),
                    cmd("Programs", Programs),
                    cmd("*.*", AllWithExtension),
                    cmd("Custom...", CustomFilter),
                    cmd("Only Selected Files", OnlySelectedFiles),
                ],
            },
            Item::Submenu {
                label: "Sort",
                items: &[
                    cmd("Name", SortByName),
                    cmd("Extension", SortByExtension),
                    cmd("Time", SortByTime),
                    cmd("Size", SortBySize),
                    cmd("Unsorted", Unsorted),
                    SEP,
                    cmd("Reversed Order", ReversedOrder),
                ],
            },
            Item::Submenu {
                label: "Tabs",
                items: &[
                    cmd("New Tab", DuplicateTab),
                    cmd("Open Folder in New Tab", OpenInNewTab),
                    cmd("Close Tab", CloseTab),
                    cmd("Close All Tabs", CloseAllTabs),
                    cmd("Close Duplicate Tabs", CloseDuplicateTabs),
                    SEP,
                    cmd("Next Tab", NextTab),
                    cmd("Previous Tab", PreviousTab),
                    cmd("List of Tabs", ListTabs),
                ],
            },
            SEP,
            cmd("Reread Source", RereadSource),
        ],
    },
    Menu {
        label: "Configuration",
        items: &[
            cmd("Settings...", Settings),
            cmd("Shortcuts...", Shortcuts),
        ],
    },
    Menu {
        label: "Help",
        items: &[cmd("Spark Commander Website", Website), SEP, cmd("About Spark Commander", About)],
    },
];

/// The right-click menu of folder tabs.
pub const TAB_MENU: &[Item] = &[
    cmd("Duplicate this tab", DuplicateTab),
    cmd("Rename tab...", RenameTab),
    cmd("Lock this tab", LockTab),
    SEP,
    cmd("Copy tab to other panel", CopyTabToOtherPanel),
    SEP,
    cmd("Close this tab", CloseTab),
    cmd("Close all tabs", CloseAllTabs),
    cmd("Close duplicate tabs", CloseDuplicateTabs),
    Item::ClosedTabs {
        label: "Recently closed tabs",
    },
    cmd("Show list of all tabs", ListTabs),
];

/// The function key buttons under the panels.
pub const BUTTON_BAR: &[Item] = &[
    cmd("View", View),
    cmd("Edit", Edit),
    cmd("Copy", Copy),
    cmd("Move", Move),
    cmd("New Folder", NewFolder),
    cmd("Delete", Delete),
    cmd("Exit", Quit),
];

/// Commands on no menu or button.
pub const OTHER: &[Item] = &[
    cmd("Disconnect from the server", Disconnect),
    cmd("Show in the left panel", OpenInLeftPanel),
    cmd("Show in the right panel", OpenInRightPanel),
    cmd("Open the folder in a new tab of the other panel", OpenInOtherNewTab),
    cmd("Path to the command line", PathToCommandLine),
    cmd("Name to the command line", NameToCommandLine),
    cmd("Path and name to the command line", PathAndNameToCommandLine),
];

/// Names for where the menu gives no context.
const FULL_NAMES: &[(Command, &str)] = &[
    (Brief, "Brief View"),
    (Full, "Full View"),
    (SidebarTree, "Sidebar: Folders"),
    (SidebarConnections, "Sidebar: Connections"),
    (SidebarPlaces, "Sidebar: Places"),
    (ToggleSidebar, "Sidebar: show or hide"),
    (AllFiles, "Show All Files"),
    (Programs, "Show Programs"),
    (AllWithExtension, "Show *.*"),
    (CustomFilter, "Custom Filter"),
    (QuickFilter, "Quick Filter"),
    (HiddenFiles, "Show Hidden Files"),
    (OnlySelectedFiles, "Show Only Selected Files"),
    (SortByName, "Sort by Name"),
    (SortByExtension, "Sort by Extension"),
    (SortByTime, "Sort by Time"),
    (SortBySize, "Sort by Size"),
    (Unsorted, "Sort Unsorted"),
    (RenameInPlace, "Rename in place"),
    (CompareDirectories, "Compare Directories: Mark Newer"),
    (GoBack, "Go Back"),
    (GoForward, "Go Forward"),
    (GoHome, "Go Home"),
    (DuplicateTab, "New Tab (a copy of this one)"),
];

/// Every command once, under its menu's name, for the Shortcuts dialog.
pub fn groups() -> Vec<(&'static str, Vec<(&'static str, Command)>)> {
    fn walk(items: &[Item], seen: &mut Vec<Command>, out: &mut Vec<(&'static str, Command)>) {
        for item in items {
            match *item {
                Item::Command { label, command } if !seen.contains(&command) => {
                    seen.push(command);
                    let full = FULL_NAMES.iter().find(|(c, _)| *c == command);
                    out.push((full.map_or(label.trim_end_matches("..."), |(_, name)| name), command));
                }
                Item::Submenu { items, .. } => walk(items, seen, out),
                _ => {}
            }
        }
    }
    let mut seen = Vec::new();
    let named = MENUS.iter().map(|menu| (menu.label, menu.items));
    let others = [("Tabs", TAB_MENU), ("Button bar", BUTTON_BAR), ("Other", OTHER)];
    let mut groups = Vec::new();
    for (name, items) in named.chain(others) {
        let mut commands = Vec::new();
        walk(items, &mut seen, &mut commands);
        if !commands.is_empty() {
            groups.push((name, commands));
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KeySettings, Preset};

    #[test]
    fn every_command_with_keys_is_listed() {
        let listed: Vec<Command> = groups().into_iter().flat_map(|(_, c)| c).map(|(_, c)| c).collect();
        for preset in Preset::ALL {
            let keys = KeySettings { preset, ..Default::default() };
            for (key, command) in keys.bindings() {
                assert!(listed.contains(&command), "{key}: {command:?} is on no list");
            }
        }
    }
}
