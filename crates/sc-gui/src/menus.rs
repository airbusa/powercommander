use gpui_kit::component::GlobalState;
use gpui_kit::component::menu::PopupMenu;
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::{
    Action, AnyWindowHandle, App, Context, DummyKeyboardMapper, Global, KeyBinding,
    KeyBindingContextPredicate, Menu, MenuItem, SharedString, Unbind, Window, WindowId,
};
#[cfg(target_os = "macos")]
use gpui_kit::{SystemMenuType, actions};
use sc_core::keymap::KeySettings;
use sc_core::menu::{Command, Item, MENUS};
use sc_core::theme::Appearance;

#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
pub struct Run(pub Command);

#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
pub struct SetAppearance(pub Appearance);

/// A tab of the active side.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
pub struct SelectTab(pub usize);

/// A place of the active tab's history, by its index there.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
pub struct GoToHistory(pub usize);

/// One of the active side's recently closed tabs.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
pub struct ReopenTab(pub usize);

/// Go > Workspaces: one, by its place in the list.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
pub struct OpenWorkspace(pub usize);

/// macOS greys out a menu item only when no one handles its action.
#[derive(Action, Clone, PartialEq)]
#[action(namespace = sc, no_json)]
struct Unavailable;

#[cfg(target_os = "macos")]
actions!(sc, [HideApp, HideOtherApps, ShowAllApps, MinimizeWindow, ZoomWindow]);

/// macOS: the application menu has these, not the window's menus; Configuration, left empty, goes.
#[cfg(target_os = "macos")]
const IN_APP_MENU: [Command; 4] = [Command::About, Command::Settings, Command::Shortcuts, Command::Quit];

/// The commands' keys. Keys bound `before` are unbound first: the new
/// bindings come after that, so they win.
pub fn bind(keys: &KeySettings, before: Option<&KeySettings>, cx: &mut App) {
    let binding = |key: &str, action: Box<dyn Action>, context: &str| {
        let context = Some(Rc::new(KeyBindingContextPredicate::parse(context).unwrap()));
        KeyBinding::load(key, action, context, false, None, &DummyKeyboardMapper).ok()
    };
    // The terminal takes every other key, and keeps its own (Ctrl+Shift+T opens a tab there).
    let contexts = |command: Command, key: &str| match command {
        Command::TerminalPanel if !crate::terminal::own_key(key) => &[crate::CONTEXT, "Terminal"][..],
        _ => &[crate::CONTEXT][..],
    };
    let mut unbind = Vec::new();
    for (key, command) in before.into_iter().flat_map(KeySettings::bindings) {
        for context in contexts(command, &key) {
            unbind.extend(binding(&key, Box::new(Unbind(Run::name_for_type().into())), context));
        }
    }
    cx.bind_keys(unbind);
    // A key the settings file got wrong is left out.
    let mut bound = keys.bindings();
    // GPUI Kit shows a command's last key, macOS's menu bar its first: there
    // the shortcut goes before the alternate.
    if cfg!(target_os = "macos") {
        for keys in bound.chunk_by_mut(|a, b| a.1 == b.1) {
            keys.reverse();
        }
    }
    let mut bindings = Vec::new();
    for (key, command) in bound {
        for context in contexts(command, &key) {
            bindings.extend(binding(&key, Box::new(Run(command)), context));
        }
    }
    cx.bind_keys(bindings);
    // The menu bar shows the keys: the next window to render puts it up again.
    #[cfg(target_os = "macos")]
    cx.set_global(Installed(None));
}

pub fn init(cx: &mut App) {
    cx.on_action(|Run(command): &Run, cx| {
        if *command == Command::Quit {
            cx.quit()
        }
    });
    #[cfg(target_os = "macos")]
    {
        cx.on_action(|_: &HideApp, cx| cx.hide());
        cx.on_action(|_: &HideOtherApps, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAllApps, cx| cx.unhide_other_apps());
        cx.on_action(|_: &MinimizeWindow, cx| on_active_window(cx, Window::minimize_window));
        cx.on_action(|_: &ZoomWindow, cx| on_active_window(cx, Window::zoom_window));
        cx.bind_keys([
            KeyBinding::new("cmd-h", HideApp, None),
            KeyBinding::new("alt-cmd-h", HideOtherApps, None),
            KeyBinding::new("cmd-m", MinimizeWindow, None),
        ]);
    }
    cx.on_window_closed(|cx, closed| {
        cx.default_global::<Lent>().0.remove(&closed);
        #[cfg(target_os = "macos")]
        if cx.windows().is_empty() {
            grey_out(cx);
        }
    })
    .detach();
}

/// Windows that work for another, as Find Files for the main one: while one
/// is active, the menu bar shows the other's menus and their commands go to
/// the other, as a Mac app's panels leave the menus to its main window.
#[derive(Default)]
struct Lent(HashMap<WindowId, AnyWindowHandle>);

impl Global for Lent {}

/// `window` works for `owner` (see `Lent`).
pub fn lend(window: &Window, owner: AnyWindowHandle, cx: &mut App) {
    cx.default_global::<Lent>().0.insert(window.window_handle().window_id(), owner);
}

/// The window the active one works for, if it works for one.
pub fn owner_of_active(cx: &App) -> Option<AnyWindowHandle> {
    let active = cx.active_window()?;
    cx.try_global::<Lent>()?.0.get(&active.window_id()).copied()
}

/// macOS: no window is left to act on; the menu bar stays as it was, greyed
/// out but for hiding and quitting.
#[cfg(target_os = "macos")]
fn grey_out(cx: &mut App) {
    use gpui_kit::OwnedMenuItem;
    fn items(owned: Vec<OwnedMenuItem>) -> Vec<MenuItem> {
        let stay: [&dyn Action; 4] = [&HideApp, &HideOtherApps, &ShowAllApps, &Run(Command::Quit)];
        let each = owned.into_iter().map(|entry| match entry {
            OwnedMenuItem::Separator => MenuItem::Separator,
            OwnedMenuItem::Submenu(menu) => MenuItem::submenu(Menu::new(menu.name).items(items(menu.items))),
            OwnedMenuItem::SystemMenu(menu) => MenuItem::os_submenu(menu.name, menu.menu_type),
            OwnedMenuItem::Action { name, action, os_action, checked, .. } => {
                let kept = stay.iter().any(|stays| stays.partial_eq(&*action));
                MenuItem::Action {
                    name: name.into(),
                    action: if kept { action } else { Box::new(Unavailable) },
                    os_action,
                    checked,
                    disabled: !kept,
                }
            }
        });
        each.collect()
    }
    let Some(menus) = cx.get_menus() else {
        return;
    };
    cx.set_menus(menus.into_iter().map(|menu| Menu::new(menu.name).items(items(menu.items))));
    // The next window to open puts its own up.
    cx.set_global(Installed(None));
}

/// The action reaches the app while its window is being updated: after that.
#[cfg(target_os = "macos")]
fn on_active_window(cx: &mut App, act: fn(&Window)) {
    if let Some(handle) = cx.active_window() {
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, _| act(window));
        });
    }
}

/// The Show menu's last custom filter, the name of the filter item after
/// Programs, as Total Commander's; "*.*" before one.
static LAST_FILTER: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn set_last_filter(masks: &str) {
    *LAST_FILTER.lock().unwrap_or_else(|err| err.into_inner()) = masks.to_owned();
}

pub fn last_filter() -> String {
    let masks = LAST_FILTER.lock().unwrap_or_else(|err| err.into_inner()).clone();
    if masks.is_empty() { "*.*".to_owned() } else { masks }
}

/// Go > Workspaces' names, and which is open.
static WORKSPACES: std::sync::Mutex<(Vec<String>, usize)> = std::sync::Mutex::new((Vec::new(), 0));

/// Call `set` after.
pub fn set_workspaces(workspaces: &sc_core::session::Workspaces) {
    let names = workspaces.list.iter().map(|workspace| workspace.name.clone()).collect();
    *WORKSPACES.lock().unwrap_or_else(|err| err.into_inner()) = (names, workspaces.current);
}

/// Call `AppMenuBar::reload` after.
/// The menu bar; `checked` commands show a check mark.
pub fn set(chosen: Appearance, checked: &dyn Fn(Command) -> bool, cx: &mut App) {
    let menus = main_menus(chosen, checked).into_iter().map(Menu::owned).collect();
    // AppMenuBar draws these, except on macOS: there `show` gives them to the system.
    GlobalState::global_mut(cx).set_app_menus(menus);
}

/// The main window's menus; `checked` commands show a check mark.
pub fn main_menus(chosen: Appearance, checked: &dyn Fn(Command) -> bool) -> Vec<Menu> {
    let menus = MENUS.iter().map(|menu| Menu::new(menu.label).items(items(menu.items, chosen, checked)));
    menus.filter(|menu| !menu.items.is_empty()).collect()
}

fn items(list: &[Item], chosen: Appearance, checked: &dyn Fn(Command) -> bool) -> Vec<MenuItem> {
    let all = list.iter().filter_map(|entry| match *entry {
        Item::Separator => Some(MenuItem::separator()),
        Item::Submenu { label, items: sub } => Some(MenuItem::submenu(
            Menu::new(label).items(items(sub, chosen, checked)),
        )),
        #[cfg(target_os = "macos")]
        Item::Command { command, .. } if IN_APP_MENU.contains(&command) => None,
        Item::Command { command: Command::AllWithExtension, .. } => {
            Some(item(last_filter().into(), checked(Command::AllWithExtension), Box::new(Run(Command::AllWithExtension)), false))
        }
        Item::Command { label, command, .. } => {
            Some(item(label.into(), checked(command), Box::new(Run(command)), false))
        }
        // Xfce's, MATE's and Cinnamon's look is their theme's: no other to choose.
        Item::Appearance { .. } if sc_core::theme::desktop_dark().is_some() => None,
        Item::Appearance { label } => {
            let each = [
                ("As the system", Appearance::System),
                ("Light", Appearance::Light),
                ("Dark", Appearance::Dark),
            ]
            .map(|(name, appearance)| {
                MenuItem::action(name, SetAppearance(appearance)).checked(chosen == appearance)
            });
            Some(MenuItem::submenu(Menu::new(label).items(each)))
        }
        Item::ClosedTabs { .. } => None,
        Item::Workspaces { label, items: rest } => {
            let (names, current) = WORKSPACES.lock().unwrap_or_else(|err| err.into_inner()).clone();
            let each = names.into_iter().enumerate();
            let each = each.map(|(ix, name)| item(name.into(), current == ix, Box::new(OpenWorkspace(ix)), false));
            let all = each.chain(items(rest, chosen, checked));
            Some(MenuItem::submenu(Menu::new(label).items(all)))
        }
    });
    // What the application menu took may leave a separator last.
    let mut items: Vec<MenuItem> = all.collect();
    while matches!(items.last(), Some(MenuItem::Separator)) {
        items.pop();
    }
    items
}

/// A menu bar item; macOS writes "…" for "...".
fn item(label: SharedString, checked: bool, action: Box<dyn Action>, disabled: bool) -> MenuItem {
    let name = match cfg!(target_os = "macos") {
        true => label.replace("...", "…").into(),
        false => label,
    };
    MenuItem::Action {
        name,
        action: if disabled { Box::new(Unavailable) } else { action },
        os_action: None,
        checked,
        disabled,
    }
}

/// What a window's menus are built into: GPUI Kit's popups, as the menu
/// bar in the window shows them, or the items of macOS's menu bar.
pub trait Entries: Sized {
    fn entry(self, label: impl Into<SharedString>, checked: bool, action: Box<dyn Action>, disabled: bool) -> Self;
    fn separator(self) -> Self;
}

impl Entries for PopupMenu {
    fn entry(self, label: impl Into<SharedString>, checked: bool, action: Box<dyn Action>, disabled: bool) -> Self {
        self.menu_with_check_and_disabled(label, checked, action, disabled)
    }

    fn separator(self) -> Self {
        PopupMenu::separator(self)
    }
}

impl Entries for Vec<MenuItem> {
    fn entry(mut self, label: impl Into<SharedString>, checked: bool, action: Box<dyn Action>, disabled: bool) -> Self {
        self.push(item(label.into(), checked, action, disabled));
        self
    }

    fn separator(mut self) -> Self {
        self.push(MenuItem::separator());
        self
    }
}

/// What macOS's menu bar shows, so windows that render again with the same
/// menus leave it alone; `None` puts it up again.
#[cfg(target_os = "macos")]
struct Installed(Option<u64>);

#[cfg(target_os = "macos")]
impl Global for Installed {}

/// macOS: the active window's menus on the system's menu bar, between the
/// application menu and Window. Each window calls this as it renders.
#[cfg(target_os = "macos")]
pub fn show(window: &Window, menus: Vec<Menu>, cx: &mut App) {
    use std::hash::{DefaultHasher, Hash, Hasher};
    fn hash_items(items: &[MenuItem], state: &mut DefaultHasher) {
        for item in items {
            match item {
                MenuItem::Separator | MenuItem::SystemMenu(_) => 0.hash(state),
                MenuItem::Submenu(menu) => {
                    menu.name.hash(state);
                    hash_items(&menu.items, state);
                }
                MenuItem::Action { name, action, checked, disabled, .. } => {
                    (name, action.name(), checked, disabled).hash(state)
                }
            }
        }
    }
    // Its own while active, and while a window that works for it is.
    let lent = owner_of_active(cx).is_some_and(|owner| owner.window_id() == window.window_handle().window_id());
    if !window.is_window_active() && !lent {
        return;
    }
    let mut state = DefaultHasher::new();
    for menu in &menus {
        menu.name.hash(&mut state);
        hash_items(&menu.items, &mut state);
    }
    let hash = Some(state.finish());
    if cx.try_global::<Installed>().is_some_and(|installed| installed.0 == hash) {
        return;
    }
    cx.set_global(Installed(hash));
    let app = Menu::new("Spark Commander").items([
        item("About Spark Commander".into(), false, Box::new(Run(Command::About)), false),
        MenuItem::separator(),
        item("Settings...".into(), false, Box::new(Run(Command::Settings)), false),
        item("Shortcuts...".into(), false, Box::new(Run(Command::Shortcuts)), false),
        MenuItem::separator(),
        MenuItem::os_submenu("Services", SystemMenuType::Services),
        MenuItem::separator(),
        MenuItem::action("Hide Spark Commander", HideApp),
        MenuItem::action("Hide Others", HideOtherApps),
        MenuItem::action("Show All", ShowAllApps),
        MenuItem::separator(),
        MenuItem::action("Quit Spark Commander", Run(Command::Quit)),
    ]);
    // macOS lists the windows under these.
    let windows = Menu::new("Window").items([
        MenuItem::action("Minimize", MinimizeWindow),
        MenuItem::action("Zoom", ZoomWindow),
    ]);
    cx.set_menus(std::iter::once(app).chain(menus).chain([windows]));
}

/// `checked` puts a check mark on a command; `closed` lists the recently closed tabs.
pub fn context_menu(
    mut menu: PopupMenu,
    items: &[Item],
    checked: impl Fn(Command) -> bool,
    closed: Vec<SharedString>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    for item in items {
        menu = match *item {
            Item::Command { label, command, .. } => {
                menu.menu_with_check(label, checked(command), Box::new(Run(command)))
            }
            Item::Separator => menu.separator(),
            Item::ClosedTabs { label } => {
                let closed = closed.clone();
                menu.submenu(label, window, cx, move |mut menu, _, _| {
                    if closed.is_empty() {
                        return menu.menu_with_check_and_disabled(
                            "(none)",
                            false,
                            Box::new(ReopenTab(0)),
                            true,
                        );
                    }
                    for (ix, path) in closed.iter().enumerate() {
                        menu = menu.menu(path.clone(), Box::new(ReopenTab(ix)));
                    }
                    menu
                })
            }
            // shortcut: no submenus or appearances in context menus yet; add them when one has them.
            Item::Submenu { .. } | Item::Appearance { .. } | Item::Workspaces { .. } => menu,
        };
    }
    menu
}
