//! The sidebar: its sections as an accordion, one open and filling it;
//! this computer's folders as a tree that follows the active panel, the
//! servers by protocol, and Places: bookmarks, drives and devices.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::Scrollbar;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use sc_core::connections::{self, Connection, Connections};
use sc_core::devices::{self, Device, Volume, VolumeKind};
use sc_core::menu::Command;
use sc_core::remote::{Address, Scheme};
use sc_core::sidebar::{self, FolderTree, SidebarTab};
use sc_core::{icons, theme};

use crate::drag_drop::Dragged;
use crate::{Commander, connections_dialog};

pub(crate) const WIDTH: f32 = 260.;
/// How long it slides in or out, shown or hidden.
const SLIDE: Duration = Duration::from_millis(200);
const ROW: Pixels = px(22.);
const SECTION: Pixels = px(30.);

pub(crate) struct Sidebar {
    /// The section open; none while hidden.
    pub(crate) tab: Option<SidebarTab>,
    /// The section last open, which Show Sidebar opens.
    pub(crate) last: SidebarTab,
    pub(crate) width: f32,
    /// Times shown or hidden since the start, each a slide of its own.
    slides: usize,
    /// Hidden, the section it slides out with.
    closing: Option<SidebarTab>,
    /// Section switches so far.
    switches: usize,
    /// The section sliding shut.
    leaving: Option<SidebarTab>,
    /// How much of the window colour it paints over the desktop; none: solid.
    tint: Option<f32>,
    /// The desktop through it blurred, else sharp.
    blur: bool,
    tree: FolderTree,
    tree_scroll: UniformListScrollHandle,
    /// The active panel's folder, where the tree last went.
    followed: Option<PathBuf>,
    servers: Vec<Connection>,
    hosts: Vec<String>,
    closed: Vec<Scheme>,
    selected: Option<(Scheme, usize)>,
    /// KDE Connect's paired devices.
    devices: Vec<Device>,
    volumes: Vec<Volume>,
    /// The folders of the devices mounted, by id.
    device_folders: HashMap<String, Vec<(PathBuf, String)>>,
    /// When each phone was last connected again, its files having stopped answering.
    reconnected: HashMap<String, std::time::Instant>,
}

/// Where a mounted device's files are: KDE Connect mounts it at
/// `$XDG_RUNTIME_DIR/<id>`, its folders below; none while not mounted.
fn device_roots(sidebar: &Sidebar, id: &str) -> Vec<PathBuf> {
    let Some(folders) = sidebar.device_folders.get(id) else {
        return Vec::new();
    };
    let id = std::ffi::OsStr::new(id);
    let root = |path: &PathBuf| path.ancestors().find(|at| at.file_name() == Some(id)).unwrap_or(path).to_owned();
    folders.iter().map(|(path, _)| root(path)).collect()
}

/// The "Opening…" note of a device being mounted.
struct Mounting;

/// Dragging the sidebar's edge.
pub(crate) struct SidebarDrag;

impl Sidebar {
    pub(crate) fn new(tab: Option<SidebarTab>, width: f32, hidden: bool, opacity: u8, blur: bool) -> Self {
        Sidebar {
            tab,
            last: tab.unwrap_or(SidebarTab::Tree),
            width: if width > 0. { width } else { WIDTH },
            slides: 0,
            closing: None,
            switches: 0,
            leaving: None,
            tint: theme::backdrop_tint(opacity),
            blur,
            tree: FolderTree::new(hidden),
            tree_scroll: UniformListScrollHandle::new(),
            followed: None,
            servers: Connections::load().unwrap_or_default().saved,
            hosts: connections::ssh_hosts(),
            closed: Vec::new(),
            selected: None,
            devices: Vec::new(),
            volumes: Vec::new(),
            device_folders: HashMap::new(),
            reconnected: HashMap::new(),
        }
    }

    /// Show > Sidebar's check marks.
    pub(crate) fn shows(&self, command: Command) -> bool {
        match command {
            Command::SidebarTree => self.tab == Some(SidebarTab::Tree),
            Command::SidebarConnections => self.tab == Some(SidebarTab::Connections),
            Command::SidebarPlaces => self.tab == Some(SidebarTab::Places),
            Command::ToggleSidebar => self.tab.is_some(),
            _ => false,
        }
    }

    /// Reads the open folders again; `hidden` shows dot folders.
    pub(crate) fn refresh(&mut self, hidden: bool) {
        self.tree.refresh(hidden);
    }

    pub(crate) fn reload_servers(&mut self) {
        self.servers = Connections::load().unwrap_or_default().saved;
        self.hosts = connections::ssh_hosts();
    }
}

impl Commander {
    /// Opens `which`; the open one slides shut.
    fn open_section(&mut self, which: SidebarTab, cx: &mut Context<Self>) {
        let Some(was) = self.sidebar.tab.filter(|&was| was != which) else {
            return;
        };
        self.sidebar.tab = Some(which);
        self.sidebar.last = which;
        self.sidebar.switches += 1;
        self.sidebar.leaving = Some(was);
        let switch = self.sidebar.switches;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SLIDE).await;
            let _ = this.update(cx, |this, cx| {
                if this.sidebar.switches == switch && this.sidebar.leaving.take().is_some() {
                    cx.notify();
                }
            });
        })
        .detach();
        self.reload_menus(cx);
        cx.notify();
    }

    /// Show > Sidebar: that tab, or hidden when it shows it already.
    pub(crate) fn toggle_sidebar(&mut self, tab: SidebarTab, window: &mut Window, cx: &mut Context<Self>) {
        let was = self.sidebar.tab;
        if was.is_some_and(|was| was != tab) {
            return self.open_section(tab, cx);
        }
        self.sidebar.tab = (was != Some(tab)).then_some(tab);
        self.sidebar.last = tab;
        // Shown or hidden, it slides; hidden, it is drawn until out.
        if was.is_some() != self.sidebar.tab.is_some() {
            self.sidebar.slides += 1;
            self.sidebar.closing = was.filter(|_| self.sidebar.tab.is_none());
            let slide = self.sidebar.slides;
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(SLIDE).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.sidebar.slides == slide && this.sidebar.closing.take().is_some() {
                        this.set_backdrop(window, cx);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        self.set_backdrop(window, cx);
        self.reload_menus(cx);
        cx.notify();
    }

    /// The sidebar's and the terminal's opacity over the desktop, blurred or
    /// not: in the settings, or tried in their dialog.
    pub(crate) fn show_backdrop(&mut self, look: &sc_core::settings::Settings, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.tint = theme::backdrop_tint(look.sidebar_opacity.0);
        self.sidebar.blur = look.sidebar_blur.0;
        self.terminal_look = (theme::backdrop_tint(look.terminal_opacity.0), look.terminal_blur.0);
        self.set_backdrop(window, cx);
        cx.notify();
    }

    /// The desktop shows through, blurred if wished, only while the sidebar
    /// or the terminal is there: a see-through window loses subpixel text.
    pub(crate) fn set_backdrop(&self, window: &mut Window, cx: &mut App) {
        // Sliding out too.
        let sidebar = self.sidebar.tab.or(self.sidebar.closing).is_some() && self.sidebar.tint.is_some();
        let terminal = self.terminal_shown && self.terminal_look.0.is_some();
        // shortcut: the window has one backdrop: blurred when either part shown asks for it.
        let blur = sidebar && self.sidebar.blur || terminal && self.terminal_look.1;
        let see_through = sidebar || terminal;
        window.set_background_appearance(match see_through {
            false => WindowBackgroundAppearance::Opaque,
            true if !blur => WindowBackgroundAppearance::Transparent,
            true => WindowBackgroundAppearance::Blurred,
        });
        window.set_see_through(see_through, cx);
    }

    /// The tree opens down to the active panel's folder, once that changed.
    fn follow_active(&mut self) {
        let panel = self.panels[self.active].tabs.current();
        let here = panel.remote().is_none().then(|| panel.folder().to_owned());
        if here == self.sidebar.followed {
            return;
        }
        if let Some(path) = &here
            && self.sidebar.tree.reveal(path)
            && let Some(ix) = self.sidebar.tree.rows().iter().position(|row| row.path == path)
        {
            self.sidebar.tree_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        self.sidebar.followed = here;
    }

    /// The active panel shows `path`, leaving its server for it; an error names `path`.
    fn open_folder(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let side = &mut self.panels[self.active];
        let remote = side.tabs.current().remote().is_some();
        let before = side.tabs.current().spot();
        let went = side.tabs.navigate(|panel| match remote {
            true => panel.disconnect(path.clone()),
            false => panel.go_to_folder(path.clone()),
        });
        side.scroll_to_cursor();
        match went {
            Ok(()) => self.follow(self.active, before, window, cx),
            Err(err) => window.push_notification(Notification::error(format!("{}: {err}", path.display())), cx),
        }
        cx.notify();
    }

    /// The saved servers as on disk, changed, saved and shown.
    fn change_servers(&mut self, change: impl FnOnce(&mut Vec<Connection>), window: &mut Window, cx: &mut Context<Self>) {
        let mut all = Connections::load().unwrap_or_default();
        change(&mut all.saved);
        if let Err(err) = all.save() {
            window.push_notification(Notification::error(format!("Could not save the connections: {err}")), cx);
        }
        self.sidebar.reload_servers();
        cx.notify();
    }

    /// New (none), edit, or duplicate a saved server.
    fn edit_server(&mut self, ix: Option<usize>, duplicate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut connection = ix.and_then(|ix| self.sidebar.servers.get(ix).cloned()).unwrap_or_default();
        if duplicate {
            connection.name = format!("{} (copy)", connection.name);
        }
        let replace = ix.filter(|_| !duplicate);
        let this = cx.entity().downgrade();
        connections_dialog::open_edit(connection, window, cx, move |connection, window, cx| {
            let _ = this.update(cx, |this, cx| {
                let change = |saved: &mut Vec<Connection>| match replace.filter(|&ix| ix < saved.len()) {
                    Some(ix) => saved[ix] = connection,
                    None => saved.push(connection),
                };
                this.change_servers(change, window, cx)
            });
        });
    }

    fn delete_server(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let delete = |saved: &mut Vec<Connection>| {
            if ix < saved.len() {
                let gone = saved.remove(ix);
                connections::forget_password(saved, &gone);
            }
        };
        self.sidebar.selected = None;
        self.change_servers(delete, window, cx);
    }

    pub(crate) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.sidebar.tab.or(self.sidebar.closing)?;
        if tab == SidebarTab::Tree {
            self.follow_active();
        }
        let background = self.colors.window.opacity(self.sidebar.tint.unwrap_or(1.));
        let sections = [
            (SidebarTab::Tree, "tree.svg", "Folders"),
            (SidebarTab::Connections, "folder-server.svg", "Connections"),
            (SidebarTab::Places, "folder-favicon.svg", "Places"),
        ];
        let mut accordion = v_flex().h_full().flex_1().min_w_0();
        for (at, (which, icon, name)) in sections.into_iter().enumerate() {
            let open = tab == which;
            let button = (which == SidebarTab::Connections).then(|| self.new_connection_button(cx));
            let theme = cx.theme();
            let chevron = match open {
                true => IconName::ChevronDown,
                false => IconName::ChevronRight,
            };
            // A click opens it; the open one stays: one is always open.
            let header = h_flex()
                .id(name)
                .flex_none()
                .h(SECTION)
                .px_2()
                .gap_2()
                .cursor_pointer()
                .when(at > 0, |el| el.border_t_1().border_color(theme.border))
                .hover(|style| style.bg(theme.foreground.opacity(0.06)))
                .child(img(format!("material/{icon}")).size(px(16.)).flex_none())
                .child(div().flex_1().font_medium().child(name))
                .children(button)
                .child(Icon::new(chevron).xsmall().text_color(theme.muted_foreground))
                .on_click(cx.listener(move |this, _, _, cx| this.open_section(which, cx)));
            accordion = accordion.child(header);
            let leaving = self.sidebar.leaving == Some(which) && self.sidebar.tab.is_some();
            if open || leaving {
                let content = match which {
                    SidebarTab::Tree => self.render_tree(cx).into_any_element(),
                    SidebarTab::Connections => self.render_connections(cx).into_any_element(),
                    SidebarTab::Places => self.render_places(cx).into_any_element(),
                };
                let content = v_flex().flex_1().min_h_0().overflow_hidden().pb_1().child(content);
                accordion = match self.sidebar.leaving.is_some() {
                    true => {
                        let id = (if open { "sidebar-opening" } else { "sidebar-closing" }, self.sidebar.switches);
                        let slide = Animation::new(SLIDE).with_easing(ease_in_out);
                        accordion.child(content.with_animation(id, slide, move |mut el, delta| {
                            el.style().flex_grow = Some(if open { delta } else { 1. - delta });
                            el
                        }))
                    }
                    false => accordion.child(content),
                };
            }
        }
        let theme = cx.theme();
        let width = self.sidebar.width;
        let drawer = h_flex()
            .h_full()
            .w(px(width))
            .bg(background)
            .border_r_1()
            .border_color(theme.border)
            .child(accordion);
        // Sliding, its edge narrows or widens; the drawer is clipped, not reflowed.
        let sidebar = div()
            .relative()
            .h_full()
            .flex_none()
            .w(px(width))
            .child(div().size_full().overflow_hidden().child(drawer))
            .child(
                div()
                    .id("sidebar-edge")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right(px(-3.))
                    .w(px(6.))
                    .occlude()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_drag(SidebarDrag, |_, _, _, cx| cx.new(|_| EmptyView)),
            );
        // As the window opens, it is there already.
        if self.sidebar.slides == 0 {
            return Some(sidebar.into_any_element());
        }
        let out = self.sidebar.tab.is_none();
        let slide = Animation::new(SLIDE).with_easing(ease_in_out);
        let sliding = sidebar.with_animation(("sidebar-slide", self.sidebar.slides), slide, move |el, delta| {
            let shown = match out {
                true => 1. - delta,
                false => delta,
            };
            el.w(px(width * shown))
        });
        Some(sliding.into_any_element())
    }

    /// The sidebar's width, from where its edge is dragged in `row`.
    pub(crate) fn drag_sidebar(&mut self, row: Bounds<Pixels>, x: Pixels, cx: &mut Context<Self>) {
        self.sidebar.width = f32::from(x - row.left()).clamp(140., 640.);
        cx.notify();
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let count = self.sidebar.tree.rows().len();
        let list = uniform_list(
            "sidebar-tree",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                let rows = this.sidebar.tree.rows();
                range.map(|ix| this.render_tree_row(ix, rows[ix], cx)).collect()
            }),
        )
        .track_scroll(&self.sidebar.tree_scroll)
        .size_full()
        .pr(px(12.));
        let rows = div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(list)
            .child(div().absolute().inset_0().child(Scrollbar::vertical(&self.sidebar.tree_scroll)));
        v_flex().size_full().child(rows)
    }

    fn render_tree_row(&self, ix: usize, row: sidebar::Row, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let selected = self.sidebar.followed.as_deref() == Some(row.path);
        let light = !theme.mode.is_dark();
        let icon = match (row.depth, row.path == Path::new("/")) {
            (0, true) => "folder-root.svg",
            (0, false) => "folder-home.svg",
            _ => icons::material().icon(row.name, true, light),
        };
        let icon = img(format!("material/{icon}")).size(px(16.)).flex_none();
        let path = row.path.to_owned();
        let toggled = path.clone();
        let chevron = match row.expanded {
            true => IconName::ChevronDown,
            false => IconName::ChevronRight,
        };
        h_flex()
            .id(("tree-row", ix))
            .w_full()
            .h(ROW)
            .pl(px(4. + 14. * row.depth as f32))
            .pr_1()
            .gap_1()
            .rounded(theme.radius)
            .when(selected, |el| el.bg(self.colors.sidebar_selection))
            .when(!selected, |el| el.hover(|style| style.bg(theme.foreground.opacity(0.06))))
            .child(
                div()
                    .id(("tree-toggle", ix))
                    .size(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(!row.leaf, |el| {
                        el.child(Icon::new(chevron).xsmall().text_color(theme.muted_foreground))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.sidebar.tree.toggle(&toggled);
                                    cx.notify();
                                }),
                            )
                    }),
            )
            .child(icon)
            .child(div().flex_1().min_w_0().truncate().child(row.name.to_owned()))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                match event.click_count() {
                    2 => this.sidebar.tree.toggle(&path),
                    _ => this.open_folder(path.clone(), window, cx),
                }
                cx.notify();
            }))
    }

    /// Asks KDE Connect for its devices now and every few seconds after, for
    /// as long as the window is there.
    pub(crate) fn watch_devices(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let found = cx.background_executor().spawn(async { (devices::paired(), devices::volumes()) });
                let (paired, volumes) = found.await;
                if this.update(cx, |this, cx| this.set_devices(paired, volumes, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        })
        .detach();
    }

    fn set_devices(&mut self, devices: Vec<Device>, volumes: Vec<Volume>, cx: &mut Context<Self>) {
        if devices == self.sidebar.devices && volumes == self.sidebar.volumes {
            return;
        }
        // KDE Connect unmounts what it can no longer reach.
        let browsable = |id: &String| devices.iter().any(|device| device.id == *id && device.browsable);
        self.sidebar.device_folders.retain(|id, _| browsable(id));
        self.sidebar.devices = devices;
        self.sidebar.volumes = volumes;
        cx.notify();
    }

    /// Shows the volume, mounting it first if it is not.
    fn open_volume(&mut self, volume: Volume, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = volume.path.clone() {
            return self.open_folder(path, window, cx);
        }
        let name = volume.name.clone();
        let note = Notification::info(format!("Mounting {name}...")).id1::<Mounting>(name.clone()).autohide(false);
        window.push_notification(note, cx);
        let mount = cx.background_executor().spawn(async move { devices::mount_volume(&volume).map(|path| (volume, path)) });
        cx.spawn_in(window, async move |this, cx| {
            let mounted = mount.await;
            let _ = this.update_in(cx, |this, window, cx| {
                window.remove_notification1::<Mounting>(name.clone(), cx);
                match mounted {
                    Ok((volume, path)) => {
                        let listed = this.sidebar.volumes.iter_mut().find(|listed| **listed == volume);
                        if let Some(listed) = listed {
                            listed.path = Some(path.clone());
                        }
                        this.open_folder(path, window, cx);
                    }
                    Err(err) => window.push_notification(Notification::error(format!("{name}: {err}")).autohide(false), cx),
                }
            });
        })
        .detach();
    }

    /// Panels in it go home first, so nothing holds it.
    fn unmount_volume(&mut self, volume: Volume, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mount) = volume.path.clone() else {
            return;
        };
        let home = std::env::home_dir().unwrap_or_else(|| "/".into());
        for ix in 0..2 {
            let panel = self.panels[ix].tabs.current();
            if panel.remote().is_none() && panel.folder().starts_with(&mount) {
                let home = home.clone();
                self.navigate_on(ix, false, move |panel| panel.go_to_folder(home), window, cx);
            }
        }
        let unmount = cx.background_executor().spawn(async move { devices::unmount_volume(&volume).map(|()| volume) });
        cx.spawn_in(window, async move |this, cx| {
            let unmounted = unmount.await;
            let _ = this.update_in(cx, |this, window, cx| match unmounted {
                Ok(volume) => {
                    if let Some(listed) = this.sidebar.volumes.iter_mut().find(|listed| **listed == volume) {
                        listed.path = None;
                    }
                    cx.notify();
                }
                Err(err) => window.push_notification(Notification::error(format!("Could not unmount: {err}")), cx),
            });
        })
        .detach();
    }

    fn render_volume(&self, ix: usize, volume: &Volume, inside: Option<&Path>, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let selected = volume.path.is_some() && volume.path.as_deref() == inside;
        let icon = match volume.kind {
            VolumeKind::System | VolumeKind::Drive => Icon::new(IconName::HardDrive),
            VolumeKind::Removable => Icon::new(Lucide::Usb),
            VolumeKind::Cloud => Icon::new(Lucide::Cloud),
            VolumeKind::Network => Icon::new(IconName::Network),
            VolumeKind::Phone => Icon::new(Lucide::Smartphone),
        };
        let detail = if volume.path.is_none() { "not mounted" } else { "" };
        let (opened, menu_volume) = (volume.clone(), volume.clone());
        let commander = cx.entity().downgrade();
        h_flex()
            .id(("volume", ix))
            .h(ROW + px(2.))
            .px_1()
            .gap_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(selected, |el| el.bg(self.colors.sidebar_selection))
            .when(!selected, |el| el.hover(|style| style.bg(theme.foreground.opacity(0.06))))
            .child(icon.small().text_color(theme.muted_foreground))
            .child(div().flex_none().max_w(relative(0.7)).truncate().child(volume.name.clone()))
            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(detail))
            .on_click(cx.listener(move |this, _, window, cx| this.open_volume(opened.clone(), window, cx)))
            .context_menu(move |menu, _, _| {
                let act = |label: &'static str, unmount: bool| {
                    let (commander, volume) = (commander.clone(), menu_volume.clone());
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let volume = volume.clone();
                        let _ = commander.update(cx, |this, cx| match unmount {
                            true => this.unmount_volume(volume, window, cx),
                            false => this.open_volume(volume, window, cx),
                        });
                    })
                };
                let mounted = menu_volume.path.is_some() && menu_volume.udisks.is_some();
                let menu = menu.item(act("Open", false));
                match mounted {
                    true => menu.item(act("Unmount", true)),
                    false => menu,
                }
            })
    }

    /// Mounts the device in the background, afresh where its mount went stale,
    /// then shows `folder`, else its widest one.
    fn open_device(&mut self, device: &Device, folder: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let (id, name) = (device.id.clone(), device.name.clone());
        // Mounted, it is quick.
        if !self.sidebar.device_folders.contains_key(&id) {
            let note = Notification::info(format!("Opening {name}...")).id1::<Mounting>(id.clone()).autohide(false);
            window.push_notification(note, cx);
        }
        let mount = {
            let id = id.clone();
            cx.background_executor().spawn(async move { devices::mount(&id) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let mounted = mount.await;
            let _ = this.update_in(cx, |this, window, cx| {
                window.remove_notification1::<Mounting>(id.clone(), cx);
                match mounted {
                    Ok(folders) => {
                        let shown = folder.unwrap_or_else(|| folders[0].0.clone());
                        this.sidebar.device_folders.insert(id, folders);
                        this.open_folder(shown, window, cx);
                    }
                    Err(err) => {
                        let note = Notification::error(format!("{name}: {err}")).autohide(false);
                        window.push_notification(note, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Mounted phones whose files stopped answering, connected again (KDE
    /// Connect's app stops serving them when the phone sleeps, the mount here
    /// staying): what showed them reads them again. Once in 30 s each; a
    /// phone that still does not answer is said to.
    pub(crate) fn reconnect_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mounted: Vec<(Device, Vec<PathBuf>)> =
            self.sidebar.devices.iter().map(|device| (device.clone(), device_roots(&self.sidebar, &device.id))).filter(|(_, roots)| !roots.is_empty()).collect();
        // A dead mount may hang as well as fail: looked at in the background.
        let dead = cx.background_executor().spawn(async move {
            let lost = |root: &PathBuf| std::fs::metadata(root).err().is_some_and(|err| lost_device(&err));
            mounted.into_iter().filter(|(_, roots)| roots.iter().any(lost)).map(|(device, _)| device).collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let dead = dead.await;
            let _ = this.update_in(cx, |this, window, cx| dead.into_iter().for_each(|device| this.reconnect_device(device, window, cx)));
        })
        .detach();
    }

    fn reconnect_device(&mut self, device: Device, window: &mut Window, cx: &mut Context<Self>) {
        let (id, name) = (device.id.clone(), device.name.clone());
        if self.sidebar.reconnected.get(&id).is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            let text = format!("{name} does not answer: unlock it and open KDE Connect on it, then read the folder again (F2).");
            return window.push_notification(Notification::error(text).id1::<Mounting>(id), cx);
        }
        self.sidebar.reconnected.insert(id.clone(), std::time::Instant::now());
        let note = Notification::info(format!("{name} does not answer: connecting again...")).id1::<Mounting>(id.clone()).autohide(false);
        window.push_notification(note, cx);
        let mount = cx.background_executor().spawn({
            let id = id.clone();
            async move {
                // Mounted, it would not mount again.
                let _ = devices::unmount(&id);
                devices::mount(&id)
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let mounted = mount.await;
            let _ = this.update_in(cx, |this, window, cx| {
                window.remove_notification1::<Mounting>(id.clone(), cx);
                match mounted {
                    Ok(folders) => {
                        this.sidebar.device_folders.insert(id.clone(), folders);
                        let roots = device_roots(&this.sidebar, &id);
                        for ix in 0..2 {
                            let panel = this.panels[ix].tabs.current();
                            if panel.remote().is_none() && roots.iter().any(|root| panel.folder().starts_with(root)) {
                                this.navigate_on(ix, false, sc_core::Panel::reload, window, cx);
                            }
                        }
                        this.reload_quick_view();
                        cx.notify();
                    }
                    Err(err) => {
                        let text = format!("{name} does not answer ({err}): unlock it and open KDE Connect on it.");
                        window.push_notification(Notification::error(text).autohide(false), cx);
                    }
                }
            });
        })
        .detach();
    }

    /// The mounted device whose files `path` is in.
    pub(crate) fn device_of(&self, path: &Path) -> Option<Device> {
        let holds = |device: &&Device| device_roots(&self.sidebar, &device.id).iter().any(|root| path.starts_with(root));
        self.sidebar.devices.iter().find(holds).cloned()
    }

    /// The path bar's ×, or Disconnect, on a device: panels in its files go
    /// home first, so nothing holds them; it stays paired.
    pub(crate) fn unmount_device(&mut self, device: &Device, window: &mut Window, cx: &mut Context<Self>) {
        let roots = device_roots(&self.sidebar, &device.id);
        if roots.is_empty() {
            return;
        }
        let home = std::env::home_dir().unwrap_or_else(|| "/".into());
        for ix in 0..2 {
            let panel = self.panels[ix].tabs.current();
            if panel.remote().is_none() && roots.iter().any(|root| panel.folder().starts_with(root)) {
                let home = home.clone();
                self.navigate_on(ix, false, move |panel| panel.go_to_folder(home), window, cx);
            }
        }
        let (id, name) = (device.id.clone(), device.name.clone());
        let unmount = cx.background_executor().spawn({
            let id = id.clone();
            async move { devices::unmount(&id) }
        });
        cx.spawn_in(window, async move |this, cx| {
            let unmounted = unmount.await;
            let _ = this.update_in(cx, |this, window, cx| match unmounted {
                Ok(()) => {
                    this.sidebar.device_folders.remove(&id);
                    cx.notify();
                }
                Err(err) => window.push_notification(Notification::error(format!("{name}: could not unmount: {err}")), cx),
            });
        })
        .detach();
    }

    fn render_places(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let panel = self.panels[self.active].tabs.current();
        let here = panel.remote().is_none().then(|| panel.folder().to_owned());
        let mut list = v_flex().id("places").flex_1().min_h_0().overflow_y_scroll().px_1();
        list = list.child(self.render_bookmarks(here.as_deref(), cx));
        // The deepest volume the active panel is in.
        let inside: Option<PathBuf> = here.as_ref().and_then(|here| {
            let mounted = self.sidebar.volumes.iter().filter_map(|volume| volume.path.as_ref());
            mounted.filter(|path| here.starts_with(path)).max_by_key(|path| path.components().count()).cloned()
        });
        let of = |kinds: &[VolumeKind]| -> Vec<(usize, Volume)> {
            let volumes = self.sidebar.volumes.iter().cloned().enumerate();
            volumes.filter(|(_, volume)| kinds.contains(&volume.kind)).collect()
        };
        let drives = of(&[VolumeKind::System, VolumeKind::Drive, VolumeKind::Removable]);
        let (phones, network) = (of(&[VolumeKind::Phone]), of(&[VolumeKind::Network]));
        let cloud = of(&[VolumeKind::Cloud]);
        if !drives.is_empty() {
            list = list.child(section("Drives", cx));
        }
        for (ix, volume) in &drives {
            list = list.child(self.render_volume(*ix, volume, inside.as_deref(), cx));
        }
        if !cloud.is_empty() {
            list = list.child(section("Cloud", cx));
        }
        for (ix, volume) in &cloud {
            list = list.child(self.render_volume(*ix, volume, inside.as_deref(), cx));
        }
        if !phones.is_empty() || !self.sidebar.devices.is_empty() {
            list = list.child(section("Devices", cx));
        }
        for (ix, volume) in &phones {
            list = list.child(self.render_volume(*ix, volume, inside.as_deref(), cx));
        }
        for device in &self.sidebar.devices {
            let theme = cx.theme();
            let icon = match device.kind.as_str() {
                "tablet" => Lucide::Tablet,
                "laptop" => Lucide::Laptop,
                "desktop" => Lucide::Monitor,
                "tv" => Lucide::Tv,
                _ => Lucide::Smartphone,
            };
            let detail = match (device.reachable, device.browsable) {
                (false, _) => "not connected",
                (true, false) => "shares no files",
                (true, true) => "",
            };
            let opened = device.clone();
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("device-{}", device.id)))
                    .h(ROW + px(2.))
                    .px_1()
                    .gap_1p5()
                    .rounded(theme.radius)
                    .map(|el| match device.browsable {
                        true => el
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.foreground.opacity(0.06)))
                            .on_click(cx.listener(move |this, _, window, cx| this.open_device(&opened, None, window, cx))),
                        false => el.text_color(theme.muted_foreground),
                    })
                    .child(Icon::new(icon).small().text_color(theme.muted_foreground))
                    .child(div().flex_none().max_w(relative(0.7)).truncate().child(device.name.clone()))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(detail)),
            );
            let Some(folders) = self.sidebar.device_folders.get(&device.id) else {
                continue;
            };
            // The deepest of its folders the active panel is in.
            let inside = here.as_ref().and_then(|here| {
                let holding = folders.iter().filter(|(path, _)| here.starts_with(path));
                holding.max_by_key(|(path, _)| path.components().count()).map(|(path, _)| path.clone())
            });
            for (ix, (path, name)) in folders.iter().enumerate() {
                let theme = cx.theme();
                let selected = inside.as_ref() == Some(path);
                let icon = icons::material().icon(name, true, !theme.mode.is_dark());
                let (path, opened) = (path.clone(), device.clone());
                list = list.child(
                    h_flex()
                        .id(SharedString::from(format!("device-{}-{ix}", device.id)))
                        .h(ROW)
                        .pl(px(26.))
                        .pr_1()
                        .gap_1()
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .when(selected, |el| el.bg(self.colors.sidebar_selection))
                        .when(!selected, |el| el.hover(|style| style.bg(theme.foreground.opacity(0.06))))
                        .child(img(format!("material/{icon}")).size(px(16.)).flex_none())
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_device(&opened, Some(path.clone()), window, cx)
                        })),
                );
            }
        }
        if !network.is_empty() {
            list = list.child(section("Network", cx));
        }
        for (ix, volume) in &network {
            list = list.child(self.render_volume(*ix, volume, inside.as_deref(), cx));
        }
        v_flex().size_full().child(list)
    }

    /// Places > Bookmarks, shown also when empty: where folders are dropped.
    fn render_bookmarks(&self, here: Option<&Path>, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let target = theme.accent.opacity(0.35);
        let manage = Button::new("bookmarks-manage")
            .ghost()
            .xsmall()
            .icon(IconName::Ellipsis)
            .tooltip("Manage Bookmarks...")
            .on_click(cx.listener(|this, _, window, cx| this.manage_bookmarks(window, cx)));
        let header = h_flex()
            .child(section("Bookmarks", cx).flex_1())
            .child(div().invisible().group_hover("bookmarks", |style| style.visible()).child(manage));
        let mut block = v_flex()
            .id("bookmarks")
            .group("bookmarks")
            .rounded(theme.radius)
            .drag_over::<Dragged>(move |style, _, _, _| style.bg(target))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(target))
            .on_drop(cx.listener(|this, dragged: &Dragged, window, cx| {
                let folders = this.dragged_folders(*dragged);
                this.add_bookmarks(folders, window, cx)
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_bookmarks(paths.paths().to_vec(), window, cx)
            }))
            .child(header);
        if self.settings.bookmarks.is_empty() {
            let hint = div().px_1().pb_1().text_xs().text_color(theme.muted_foreground);
            block = block.child(hint.child("Drop folders here."));
        }
        let home = std::env::home_dir();
        for (ix, path) in self.settings.bookmarks.iter().enumerate() {
            let theme = cx.theme();
            let folder = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
            let name = self.settings.bookmark_name(path);
            // Home's would be /home: nothing to tell.
            let parent = path.parent().filter(|_| home.as_deref() != Some(path.as_path())).map(|parent| tilde(parent, home.as_deref()));
            let selected = here == Some(path.as_path());
            let icon = icons::material().icon(&folder, true, !theme.mode.is_dark());
            let (opened, commander) = (path.clone(), cx.entity().downgrade());
            block = block.child(
                h_flex()
                    .id(("bookmark", ix))
                    .h(ROW + px(2.))
                    .px_1()
                    .gap_1p5()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected, |el| el.bg(self.colors.sidebar_selection))
                    .when(!selected, |el| el.hover(|style| style.bg(theme.foreground.opacity(0.06))))
                    .child(img(format!("material/{icon}")).size(px(16.)).flex_none())
                    .child(div().flex_none().max_w(relative(0.6)).truncate().child(name))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).children(parent))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_folder(opened.clone(), window, cx)))
                    .context_menu(move |menu, _, _| {
                        let act = |label: &'static str, run: fn(&mut Commander, usize, &mut Window, &mut Context<Commander>)| {
                            let commander = commander.clone();
                            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                let _ = commander.update(cx, |this, cx| run(this, ix, window, cx));
                            })
                        };
                        menu.item(act("Rename...", Commander::rename_bookmark))
                            .item(act("Move Up", |this, ix, window, cx| this.move_bookmark(ix, ix.wrapping_sub(1), window, cx)))
                            .item(act("Move Down", |this, ix, window, cx| this.move_bookmark(ix, ix + 1, window, cx)))
                            .separator()
                            .item(act("Copy Name", |this, ix, _, cx| {
                                let name = this.settings.bookmarks.get(ix).map(|path| this.settings.bookmark_name(path));
                                cx.write_to_clipboard(ClipboardItem::new_string(name.unwrap_or_default()));
                            }))
                            .item(act("Copy Path", |this, ix, _, cx| {
                                let path = this.settings.bookmarks.get(ix).map(|path| path.display().to_string());
                                cx.write_to_clipboard(ClipboardItem::new_string(path.unwrap_or_default()));
                            }))
                            .separator()
                            .item(act("Remove from Bookmarks", Commander::remove_bookmark))
                            .item(act("Manage Bookmarks...", |this, _, window, cx| this.manage_bookmarks(window, cx)))
                    }),
            );
        }
        block
    }

    /// Go > Bookmark This Folder (Ctrl+D, as a browser's): the active panel's folder.
    pub(crate) fn bookmark_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panel = self.panels[self.active].tabs.current();
        let folder = panel.folder().to_owned();
        if panel.remote().is_some() || panel.in_archive() || panel.is_found() || !sc_core::settings::bookmarkable(&folder) {
            return window.push_notification(Notification::warning(ONLY_LOCAL), cx);
        }
        let name = folder.file_name().map_or_else(|| folder.display().to_string(), |name| name.to_string_lossy().into_owned());
        let note = match self.settings.bookmarks.contains(&folder) {
            true => format!("{name} is in Bookmarks already."),
            false => format!("Bookmarked: {name}"),
        };
        self.add_bookmarks(vec![folder], window, cx);
        window.push_notification(Notification::info(note), cx);
    }

    /// A bookmark's own name; an empty one gives it its folder's again.
    pub(crate) fn rename_bookmark(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.settings.bookmarks.get(ix).cloned() else {
            return;
        };
        let name = self.settings.bookmark_name(&path);
        let commander = cx.entity().downgrade();
        crate::name_dialog::open("Rename bookmark", "Name (empty: the folder's):", name, window, cx, move |name, window, cx| {
            let _ = commander.update(cx, |this, cx| {
                match name.trim() {
                    "" => this.settings.bookmark_names.remove(&path),
                    name => this.settings.bookmark_names.insert(path.clone(), name.to_owned()),
                };
                this.save_settings(window, cx);
                cx.notify();
            });
        });
    }

    /// The `ix`th bookmark to place `to` in the list.
    pub(crate) fn move_bookmark(&mut self, ix: usize, to: usize, window: &mut Window, cx: &mut Context<Self>) {
        let bookmarks = &mut self.settings.bookmarks;
        if ix == to || ix >= bookmarks.len() || to >= bookmarks.len() {
            return;
        }
        let path = bookmarks.remove(ix);
        bookmarks.insert(to, path);
        self.save_settings(window, cx);
        cx.notify();
    }

    pub(crate) fn remove_bookmark(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.settings.bookmarks.len() {
            let gone = self.settings.bookmarks.remove(ix);
            self.settings.bookmark_names.remove(&gone);
            self.save_settings(window, cx);
            cx.notify();
        }
    }

    /// Bookmarks A to Z.
    pub(crate) fn sort_bookmarks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.sort_bookmarks();
        self.save_settings(window, cx);
        cx.notify();
    }

    /// A panel's dragged folders, this computer's only.
    pub(crate) fn dragged_folders(&self, dragged: Dragged) -> Vec<PathBuf> {
        let panel = self.panels[dragged.side].tabs.current();
        if panel.remote().is_some() || panel.in_archive() {
            return Vec::new();
        }
        let entries = panel.dragged(dragged.row);
        entries.iter().filter(|e| e.is_dir && !e.is_parent()).map(|e| panel.folder().join(&e.name)).collect()
    }

    /// This computer's folders among `paths` bookmarked, once each; files and servers' folders are not.
    pub(crate) fn add_bookmarks(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let count = paths.len();
        let folders: Vec<PathBuf> = paths.into_iter().filter(|path| sc_core::settings::bookmarkable(path)).collect();
        if folders.len() < count {
            window.push_notification(Notification::warning(ONLY_LOCAL), cx);
        }
        let bookmarks = &mut self.settings.bookmarks;
        let before = bookmarks.len();
        for folder in folders {
            if !bookmarks.contains(&folder) {
                bookmarks.push(folder);
            }
        }
        if bookmarks.len() != before {
            self.save_settings(window, cx);
        }
        cx.notify();
    }

    /// Go > Bookmarks: the sidebar on Places.
    pub(crate) fn show_bookmarks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.tab != Some(SidebarTab::Places) {
            self.toggle_sidebar(SidebarTab::Places, window, cx);
        }
    }

    /// On the Connections header: a new one.
    fn new_connection_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("server-new")
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip("New connection...")
            .on_click(cx.listener(|this, _, window, cx| this.edit_server(None, false, window, cx)))
    }

    fn render_connections(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let groups = sidebar::servers(&self.sidebar.servers, &self.sidebar.hosts);
        let mut list = v_flex().id("connections").flex_1().min_h_0().overflow_y_scroll().px_1();
        if groups.is_empty() {
            list = list.child(
                div()
                    .p_2()
                    .text_color(theme.muted_foreground)
                    .child("No connections yet: + adds one."),
            );
        }
        for (scheme, servers) in groups {
            let theme = cx.theme();
            let open = !self.sidebar.closed.contains(&scheme);
            let chevron = match open {
                true => IconName::ChevronDown,
                false => IconName::ChevronRight,
            };
            list = list.child(
                h_flex()
                    .id(("server-group", scheme as usize))
                    .h(ROW)
                    .px_1()
                    .gap_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.foreground.opacity(0.06)))
                    .child(Icon::new(chevron).xsmall().text_color(theme.muted_foreground))
                    .child(div().flex_1().font_semibold().child(scheme.name().to_uppercase()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(servers.len().to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let closed = &mut this.sidebar.closed;
                        match closed.iter().position(|&s| s == scheme) {
                            Some(at) => drop(closed.remove(at)),
                            None => closed.push(scheme),
                        }
                        cx.notify();
                    })),
            );
            if !open {
                continue;
            }
            for (ix, server) in servers.into_iter().enumerate() {
                list = list.child(self.render_server(scheme, ix, server, cx));
            }
        }
        v_flex().size_full().pt_1().child(list)
    }

    fn render_server(&self, scheme: Scheme, ix: usize, server: sidebar::Server, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let selected = self.sidebar.selected == Some((scheme, ix));
        let commander = cx.entity().downgrade();
        let (address, saved, name) = (server.address.clone(), server.saved, server.name.clone());
        h_flex()
            .id((scheme.name(), ix))
            .h(ROW + px(2.))
            .pl(px(22.))
            .pr_1()
            .gap_1p5()
            .rounded(theme.radius)
            .when(selected, |el| el.bg(self.colors.sidebar_selection))
            .when(!selected, |el| el.hover(|style| style.bg(theme.foreground.opacity(0.06))))
            .child(Icon::new(Lucide::Server).xsmall().text_color(theme.muted_foreground))
            .child(div().flex_none().max_w(relative(0.6)).truncate().child(server.name))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(server.detail),
            )
            .on_click(cx.listener({
                let address = address.clone();
                move |this, event: &ClickEvent, window, cx| {
                    this.sidebar.selected = Some((scheme, ix));
                    if event.click_count() == 2 {
                        this.connect(this.active, address.clone(), window, cx);
                    }
                    cx.notify();
                }
            }))
            .context_menu(move |menu, _, _| server_menu(menu, commander.clone(), address.clone(), name.clone(), saved))
    }
}

/// `path` with `~` for the home folder.
pub(crate) fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(inside) if inside.as_os_str().is_empty() => "~".to_owned(),
        Some(inside) => format!("~/{}", inside.display()),
        None => path.display().to_string(),
    }
}

const ONLY_LOCAL: &str = "Only this computer's folders can be bookmarked.";

/// A tab's title line; buttons go after it.
/// A group's name in a tab: Drives, Phones, Network.
fn section(title: &'static str, cx: &App) -> Div {
    let label = div().text_xs().font_semibold().text_color(cx.theme().muted_foreground);
    label.px_1().pt_2().pb_0p5().child(title)
}

/// A server's right-click menu; the saved ones can be changed.
fn server_menu(
    menu: gpui_kit::component::menu::PopupMenu,
    commander: WeakEntity<Commander>,
    address: Address,
    name: String,
    saved: Option<usize>,
) -> gpui_kit::component::menu::PopupMenu {
    let act = |label: &'static str, run: fn(&mut Commander, &mut Window, &mut Context<Commander>)| {
        let commander = commander.clone();
        PopupMenuItem::new(label).on_click(move |_, window, cx| {
            let _ = commander.update(cx, |this, cx| run(this, window, cx));
        })
    };
    let copy = |label: &'static str, text: String| PopupMenuItem::new(label).on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())));
    // Its address as typed to connect: no password is kept in it.
    let (copy_name, copy_path) = (copy("Copy Name", name), copy("Copy Path", address.url()));
    let (commander_for_connect, ssh) = (commander.clone(), address.clone());
    let sftp = address.scheme == Scheme::Sftp;
    let connect = PopupMenuItem::new("Connect").on_click(move |_, window, cx| {
        let _ = commander_for_connect.update(cx, |this, cx| this.connect(this.active, address.clone(), window, cx));
    });
    let mut menu = menu.item(connect);
    if sftp {
        let commander = commander.clone();
        menu = menu.item(PopupMenuItem::new("Open in Terminal").on_click(move |_, window, cx| {
            let _ = commander.update(cx, |this, cx| this.ssh_in_terminal(&ssh, window, cx));
        }));
    }
    if let Some(ix) = saved {
        let edit = |label: &'static str, run: fn(&mut Commander, usize, &mut Window, &mut Context<Commander>)| {
            let commander = commander.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = commander.update(cx, |this, cx| run(this, ix, window, cx));
            })
        };
        menu = menu
            .item(edit("Edit...", |this, ix, window, cx| this.edit_server(Some(ix), false, window, cx)))
            .item(edit("Duplicate", |this, ix, window, cx| this.edit_server(Some(ix), true, window, cx)))
            .item(edit("Delete", |this, ix, window, cx| this.delete_server(ix, window, cx)));
    }
    menu.separator()
        .item(copy_name)
        .item(copy_path)
        .separator()
        .item(act("New connection...", |this, window, cx| this.edit_server(None, false, window, cx)))
        .item(act("Disconnect", |this, window, cx| this.disconnect(window, cx)))
}

/// An error of a mount whose connection is gone (sshfs's to a phone).
pub(crate) fn lost_device(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    return matches!(err.raw_os_error(), Some(libc::EIO | libc::ENOTCONN));
    #[cfg(not(unix))]
    {
        let _ = err;
        false
    }
}
