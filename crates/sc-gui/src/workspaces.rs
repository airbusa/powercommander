//! Go > Workspaces: named sets of both panels' tabs and the terminal's. What
//! changes goes into the open one; there is nothing to save.

use std::mem;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme, Disableable, WindowExt, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use sc_core::Tabs;
use sc_core::session::OpenTabs;

use crate::terminal_pane::TerminalTab;
use crate::{Commander, menus, name_dialog, restore_side};

/// A workspace left this run: its tabs as they were, its shells running on.
pub(crate) struct Parked {
    tabs: [Tabs; 2],
    active: usize,
    terminals: Vec<TerminalTab>,
    terminal_tab: usize,
    terminal_shown: bool,
}

/// The one opened in place of the deleted open one.
fn neighbour(ix: usize) -> usize {
    if ix == 0 { 1 } else { ix - 1 }
}

impl Commander {
    /// Both panels' tabs and the terminal's, as kept.
    pub(crate) fn open_tabs(&self) -> OpenTabs {
        OpenTabs {
            sides: self.panels.iter().map(|side| side.tabs.state()).collect(),
            active: self.active,
            terminal: self.terminal_shown,
            terminals: self.terminals.iter().map(TerminalTab::state).collect(),
            terminal_tab: self.terminal_tab,
        }
    }

    /// Into the menu and the file.
    fn workspaces_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        menus::set_workspaces(&self.workspaces);
        self.reload_menus(cx);
        if let Err(err) = self.workspaces.save() {
            window.push_notification(Notification::error(format!("Could not save the workspaces: {err}")), cx);
        }
        cx.notify();
    }

    /// Go > New Workspace: the open tabs go on under a name; the workspace
    /// left keeps them as they are now.
    pub(crate) fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let free = (1..).map(|n| format!("Workspace {n}")).find(|name| self.workspaces.name_error(name, None).is_none());
        let commander = cx.entity().downgrade();
        name_dialog::open("New Workspace", "Name:", free.unwrap_or_default(), window, cx, move |name, window, cx| {
            let _ = commander.update(cx, |this, cx| {
                if let Some(problem) = this.workspaces.name_error(&name, None) {
                    return window.push_notification(Notification::error(problem), cx);
                }
                if let Some(current) = this.workspaces.current {
                    this.workspaces.list[current].open = this.open_tabs();
                }
                this.workspaces.add(&name, this.open_tabs());
                this.parked.push(None);
                this.workspaces_changed(window, cx);
            });
        });
    }

    /// Go > Workspaces > one: the open tabs and shells put aside as they are,
    /// its own back as they were left, or as saved.
    pub(crate) fn open_workspace(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.workspaces.list.len() || self.workspaces.current == Some(ix) {
            return;
        }
        let fresh = self.parked[ix].is_none();
        let next = self.parked[ix].take().unwrap_or_else(|| {
            let saved = &self.workspaces.list[ix].open;
            Parked {
                tabs: [0, 1].map(|side| restore_side(saved.sides.get(side), self.settings.filter())),
                active: saved.active,
                terminals: Vec::new(),
                terminal_tab: 0,
                terminal_shown: false,
            }
        });
        let left_open = self.open_tabs();
        let Parked { tabs: [left, right], active, terminals, terminal_tab, terminal_shown } = next;
        let left = Parked {
            tabs: [mem::replace(&mut self.panels[0].tabs, left), mem::replace(&mut self.panels[1].tabs, right)],
            active: mem::replace(&mut self.active, active.min(1)),
            terminals: mem::replace(&mut self.terminals, terminals),
            terminal_tab: mem::replace(&mut self.terminal_tab, terminal_tab),
            terminal_shown: mem::replace(&mut self.terminal_shown, terminal_shown),
        };
        if let Some(current) = self.workspaces.current {
            self.workspaces.list[current].open = left_open;
            self.parked[current] = Some(left);
        }
        self.workspaces.current = Some(ix);
        self.switches += 1;
        (self.typing, self.path_edit, self.renaming, self.panel_list) = (None, None, None, None);
        for side in &self.panels {
            side.scroll_to_cursor();
        }
        self.reread_local(cx);
        if fresh {
            let saved = &self.workspaces.list[ix].open;
            let (terminals, tab, shown) = (saved.terminals.clone(), saved.terminal_tab, saved.terminal);
            self.open_terminals(&terminals, tab, shown, window, cx);
        } else {
            // The left workspace's shell may have had the keys.
            cx.defer_in(window, |this, window, cx| window.focus(&this.focus, cx));
            self.terminal_changed(window, cx);
        }
        self.workspaces_changed(window, cx);
    }

    fn rename_workspace(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspaces.list.get(ix).map(|workspace| workspace.name.clone()) else {
            return;
        };
        let commander = cx.entity().downgrade();
        name_dialog::open("Rename Workspace", "Name:", name, window, cx, move |name, window, cx| {
            let _ = commander.update(cx, |this, cx| {
                if let Some(problem) = this.workspaces.name_error(&name, Some(ix)) {
                    return window.push_notification(Notification::error(problem), cx);
                }
                if let Some(workspace) = this.workspaces.list.get_mut(ix) {
                    workspace.name = name.trim().to_owned();
                }
                this.workspaces_changed(window, cx);
            });
        });
    }

    /// Asked first: its shells end with it.
    fn confirm_delete_workspace(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspaces.list.get(ix) else {
            return;
        };
        let open = self.workspaces.current == Some(ix);
        let shells = match open {
            true => self.terminals.len(),
            false => self.parked[ix].as_ref().map_or(0, |parked| parked.terminals.len()),
        };
        let mut text = format!("Delete the workspace \"{}\"? ", workspace.name);
        text += match (open, self.workspaces.list.len()) {
            (true, 1) => "Its tabs stay open, as no workspace's.",
            (true, _) => "Its tabs close",
            (false, _) => "Its tabs are forgotten",
        };
        if !(open && self.workspaces.list.len() == 1) {
            if shells > 0 {
                text += ", and the shells in its terminal tabs end";
            }
            match open {
                true => text += &format!("; \"{}\" opens instead.", self.workspaces.list[neighbour(ix)].name),
                false => text += ".",
            }
        }
        let commander = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let commander = commander.clone();
            alert.title("Delete Workspace").description(text.clone()).ok_text("Delete").show_cancel(true).on_ok(move |_, window, cx| {
                let _ = commander.update(cx, |this, cx| this.delete_workspace(ix, window, cx));
                true
            })
        });
    }

    /// The open one: another opens first, unless it is the last, whose tabs stay.
    fn delete_workspace(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.workspaces.list.len() {
            return;
        }
        if self.workspaces.current == Some(ix) && self.workspaces.list.len() > 1 {
            self.open_workspace(neighbour(ix), window, cx);
        }
        self.workspaces.remove(ix);
        self.parked.remove(ix);
        self.workspaces_changed(window, cx);
    }

    pub(crate) fn manage_workspaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let commander = cx.entity();
        let view = cx.new(|cx| WorkspacesDialog {
            _changed: cx.observe(&commander, |_, _, cx| cx.notify()),
            commander: commander.downgrade(),
            selected: self.workspaces.current,
        });
        window.open_dialog(cx, move |dialog, _, _| dialog.title("Workspaces").width(px(560.)).child(view.clone()));
    }
}

/// "Left 2 tabs, right 1 tab, terminal 3 tabs (hidden)".
fn summary(open: &OpenTabs) -> String {
    let tabs = |n: usize| if n == 1 { "1 tab".to_owned() } else { format!("{n} tabs") };
    let side = |ix: usize| open.sides.get(ix).map_or(1, |side| side.tabs.len().max(1));
    let mut text = format!("Left {}, right {}", tabs(side(0)), tabs(side(1)));
    if !open.terminals.is_empty() {
        text += &format!(", terminal {}", tabs(open.terminals.len()));
        if !open.terminal {
            text += " (hidden)";
        }
    }
    text
}

struct WorkspacesDialog {
    commander: WeakEntity<Commander>,
    selected: Option<usize>,
    _changed: Subscription,
}

impl Render for WorkspacesDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(commander) = self.commander.upgrade() else {
            return div();
        };
        let this = commander.read(cx);
        let current = this.workspaces.current;
        let rows: Vec<(String, String)> = (this.workspaces.list.iter().enumerate())
            .map(|(ix, workspace)| {
                // The open one's as it is now.
                let text = match current == Some(ix) {
                    true => summary(&this.open_tabs()),
                    false => summary(&workspace.open),
                };
                (workspace.name.clone(), text)
            })
            .collect();
        let selected = self.selected.filter(|&ix| ix < rows.len());
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mut list = v_flex().id("workspace-list").h(px(240.)).overflow_y_scroll().p_1().border_1().border_color(theme.border).rounded(theme.radius);
        if rows.is_empty() {
            list = list.child(div().p_2().text_sm().text_color(muted).child("No workspaces: Go > New Workspace names the open tabs."));
        }
        for (ix, (name, text)) in rows.into_iter().enumerate() {
            let theme = cx.theme();
            let open = current == Some(ix);
            let faint = if selected == Some(ix) { theme.foreground.opacity(0.7) } else { muted };
            list = list.child(
                v_flex()
                    .id(("workspace-row", ix))
                    .flex_none()
                    .px_1p5()
                    .py_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(selected == Some(ix), |el| el.bg(theme.list_active))
                    .when(selected != Some(ix), |el| el.hover(|style| style.bg(theme.list_hover)))
                    .child(h_flex().gap_2().child(div().truncate().child(name)).when(open, |el| el.child(div().text_xs().text_color(faint).child("open"))))
                    .child(div().text_xs().text_color(faint).truncate().child(text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(ix);
                        cx.notify();
                    })),
            );
        }
        let on_selected = |button: Button, run: fn(&mut Commander, usize, &mut Window, &mut Context<Commander>)| {
            button.outline().disabled(selected.is_none()).on_click(cx.listener(move |this, _, window, cx| {
                if let Some(ix) = this.selected {
                    let _ = this.commander.update(cx, |commander, cx| run(commander, ix, window, cx));
                }
            }))
        };
        let buttons = h_flex()
            .gap_2()
            .child(on_selected(Button::new("workspaces-rename").label("Rename..."), Commander::rename_workspace))
            .child(on_selected(Button::new("workspaces-delete").label("Delete..."), Commander::confirm_delete_workspace))
            .child(div().flex_1())
            .child(Button::new("workspaces-close").primary().label("Close").min_w(px(80.)).on_click(|_, window, cx| window.close_dialog(cx)));
        v_flex()
            .w_full()
            .gap_2()
            .text_sm()
            .child(div().text_color(muted).child("Go > Workspaces opens one. What changes in the open one is kept in it."))
            .child(list)
            .child(buttons)
    }
}
