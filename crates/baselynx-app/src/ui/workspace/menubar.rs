//! The menu bar. A menu opens when the pointer rests on its title, and the next one takes
//! over as the pointer moves along the bar. A menu opened by hovering closes when the pointer
//! leaves both the bar and the menu; one opened by a click stays until a choice, Esc or a
//! click elsewhere, and takes the keyboard.
//!
//! Items are dispatched from the active tab, so tab commands reach it (D1), and an item is
//! enabled only when its shortcut would do something right now (D2).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::{Selectable as _, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::Workspace;
use crate::actions::*;
use crate::ui::tokens;

/// Element ids and titles, in order.
pub(super) const MENUS: [(&str, &str); 5] = [
    ("menu-file", "File"),
    ("menu-edit", "Edit"),
    ("menu-view", "View"),
    ("menu-query", "Query"),
    ("menu-table", "Table"),
];

/// How long the pointer may be away before a hovered menu closes, so crossing the gap
/// between title and menu does not close it.
const LEAVE_GRACE: Duration = Duration::from_millis(300);

#[derive(Default)]
pub(super) struct MenuBar {
    open: Option<OpenMenu>,
    over_bar: bool,
    /// Where the open menu was painted: the pointer inside it keeps a hovered menu open.
    popup_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The open menu's title. The menu hangs below it; its own reported origin is not where
    /// it is drawn (it is placed later), so only its size is used.
    title_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The menu closed by the mouse-down that is now reaching its title: that click closes
    /// it rather than opening it again.
    last_dismiss: Option<(usize, Instant)>,
    /// A mouse-down on a title just now: the menu's own "clicked outside" that follows it is
    /// not a reason to close.
    title_down: Option<(usize, Instant)>,
    _close: Option<Task<()>>,
}

struct OpenMenu {
    ix: usize,
    menu: Entity<PopupMenu>,
    /// Opened by a click: stays open when the pointer leaves.
    pinned: bool,
    _sub: Subscription,
}

impl Workspace {
    fn build_menu(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> Entity<PopupMenu> {
        let pref = tokens::theme_pref(cx);
        let tab = self.active.and_then(|i| self.tabs.get(i)).cloned();
        let context = tab
            .as_ref()
            .map(|t| t.action_focus(cx))
            .unwrap_or_else(|| self.focus_handle.clone());
        let st = tab.as_ref().map(|t| t.menu_state(cx)).unwrap_or_default();
        let n_tabs = self.tabs.len();
        let sidebar_visible = self.sidebar_visible;
        let zoom = tokens::zoom(cx);
        PopupMenu::build(window, cx, move |m, _, _| {
            let m = m.action_context(context.clone());
            match ix {
                0 => m
                    .menu("New console", Box::new(NewConsole))
                    .menu("Add connection…", Box::new(AddConnection))
                    .separator()
                    .menu("Open config file…", Box::new(OpenConfig))
                    .menu("Edit config file", Box::new(EditConfig))
                    .separator()
                    .menu("New project…", Box::new(NewProject))
                    .separator()
                    .menu_with_disabled("Export…", Box::new(ExportData), !st.can_export)
                    .menu_with_disabled("Close tab", Box::new(CloseTab), n_tabs == 0)
                    .separator()
                    .menu("Quit", Box::new(Quit)),
                1 => m
                    .menu_with_disabled("Undo edit", Box::new(UndoEdit), !st.can_undo)
                    .menu_with_disabled("Redo edit", Box::new(RedoEdit), !st.can_redo)
                    .separator()
                    .menu_with_disabled("Copy value", Box::new(CopyCell), !st.cell)
                    .menu_with_disabled("Edit cell", Box::new(BeginEdit), !st.editable_cell)
                    .menu_with_disabled("Set NULL", Box::new(SetNull), !st.editable_cell)
                    .menu_with_disabled("Revert cell", Box::new(RevertCell), !st.editable_cell)
                    .separator()
                    .menu_with_disabled("Add row", Box::new(AddRow), !st.editable)
                    .menu_with_disabled("Delete or restore row", Box::new(DeleteRow), !st.row_selected)
                    .separator()
                    .menu_with_disabled("Commit changes…", Box::new(CommitChanges), !st.pending)
                    .menu_with_disabled("Discard changes…", Box::new(DiscardChanges), !st.pending),
                2 => m
                    .menu("Go to table or command…", Box::new(OpenPalette))
                    .menu("Focus sidebar", Box::new(ToggleSidebar))
                    .menu("Filter tables…", Box::new(FocusSidebarFilter))
                    .menu(
                        if sidebar_visible {
                            "Hide sidebar"
                        } else {
                            "Show sidebar"
                        },
                        Box::new(ShowHideSidebar),
                    )
                    .menu_with_disabled("Next tab", Box::new(NextTab), n_tabs < 2)
                    .menu_with_disabled("Previous tab", Box::new(PrevTab), n_tabs < 2)
                    .separator()
                    .menu("Backups", Box::new(ShowBackups))
                    .separator()
                    .menu_with_disabled(
                        "Zoom in",
                        Box::new(ZoomIn),
                        zoom >= tokens::ZOOM_STEPS[tokens::ZOOM_STEPS.len() - 1] - 0.001,
                    )
                    .menu_with_disabled("Zoom out", Box::new(ZoomOut), zoom <= tokens::ZOOM_STEPS[0] + 0.001)
                    .menu_with_disabled("Reset zoom", Box::new(ResetZoom), (zoom - 1.0).abs() < 0.001)
                    .separator()
                    .menu_with_check("Light theme", pref == tokens::ThemePref::Light, Box::new(ThemeLight))
                    .menu_with_check("Dark theme", pref == tokens::ThemePref::Dark, Box::new(ThemeDark))
                    .menu_with_check(
                        "Theme follows the system",
                        pref == tokens::ThemePref::System,
                        Box::new(ThemeSystem),
                    ),
                3 => {
                    let can_run = st.console && st.connected && !st.running;
                    m.menu_with_disabled("Run statement", Box::new(RunStatement), !can_run)
                        .menu_with_disabled("Run all", Box::new(RunAll), !can_run)
                        .menu_with_disabled("Stop", Box::new(StopQuery), !st.running)
                }
                _ => m
                    .menu_with_disabled("Refresh", Box::new(Refresh), !st.table)
                    .menu_with_disabled("Filter…", Box::new(FocusFilter), !st.table)
                    .menu_with_disabled("Filter by this value", Box::new(FilterByValue), !st.filter_value)
                    .menu_with_disabled("Exclude this value", Box::new(ExcludeValue), !st.filter_value)
                    .separator()
                    .menu_with_disabled("Previous page", Box::new(PrevPage), !st.prev_page)
                    .menu_with_disabled("Next page", Box::new(NextPage), !st.next_page)
                    .menu_with_disabled("Count rows", Box::new(CountRows), !st.can_count),
            }
        })
    }

    fn open_menu(&mut self, ix: usize, pinned: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.menubar.open.as_mut()
            && open.ix == ix
        {
            if pinned && !open.pinned {
                open.pinned = true;
                open.menu.read(cx).focus_handle(cx).focus(window, cx);
            }
            return;
        }
        let menu = self.build_menu(ix, window, cx);
        let sub = cx.subscribe_in(&menu, window, move |this, _, _: &DismissEvent, _, cx| {
            let own_title = this
                .menubar
                .title_down
                .is_some_and(|(t, at)| t == ix && at.elapsed() < Duration::from_millis(250));
            if own_title {
                return;
            }
            if let Some(open) = this.menubar.open.take_if(|o| o.ix == ix) {
                // A click on the title of a menu opened by a click closes it; the same click
                // on a menu that only opened under the pointer pins it open instead.
                if open.pinned {
                    this.menubar.last_dismiss = Some((ix, Instant::now()));
                }
                cx.notify();
            }
        });
        // A menu opened by a click takes the keyboard (arrows, Enter, Esc); one opened by
        // passing over leaves the focus where the user was typing.
        if pinned {
            menu.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.menubar.open = Some(OpenMenu {
            ix,
            menu,
            pinned,
            _sub: sub,
        });
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menubar.popup_bounds.set(None);
        if self.menubar.open.take().is_some() {
            cx.notify();
        }
    }

    fn menu_title_down(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.menubar.title_down = Some((ix, Instant::now()));
        // The mouse-down that closed this menu (it counts as a click outside it) closes it.
        if let Some((d, at)) = self.menubar.last_dismiss.take()
            && d == ix
            && at.elapsed() < Duration::from_millis(250)
        {
            return;
        }
        match &self.menubar.open {
            Some(o) if o.ix == ix && o.pinned => self.close_menu(cx),
            _ => self.open_menu(ix, true, window, cx),
        }
    }

    fn menu_hover(&mut self, over_bar: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.menubar.over_bar = over_bar;
        if over_bar || self.menubar._close.is_some() || !self.menubar.open.as_ref().is_some_and(|o| !o.pinned) {
            return;
        }
        // Left the bar with a hovered menu open: watch the pointer until it is in neither
        // the bar nor the menu for a moment. The menu handles its own mouse moves, so its
        // painted bounds are checked rather than a hover event.
        self.menubar._close = Some(cx.spawn_in(window, async move |this, cx| {
            let mut away_since: Option<Instant> = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let done = this
                    .update_in(cx, |this, window, cx| {
                        if !this.menubar.open.as_ref().is_some_and(|o| !o.pinned) {
                            return true;
                        }
                        let menu_area = this
                            .menubar
                            .title_bounds
                            .get()
                            .zip(this.menubar.popup_bounds.get())
                            .map(|(t, p)| Bounds::new(point(t.left(), t.bottom()), p.size));
                        let inside =
                            this.menubar.over_bar || menu_area.is_some_and(|b| b.contains(&window.mouse_position()));
                        if inside {
                            away_since = None;
                            return false;
                        }
                        let since = *away_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= LEAVE_GRACE {
                            this.close_menu(cx);
                            return true;
                        }
                        false
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
            let _ = this.update(cx, |this, _| this.menubar._close = None);
        }));
    }

    /// The menu bar: every command in one place, with its shortcut (people forget Ctrl+K).
    pub(super) fn render_menus(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open_ix = self.menubar.open.as_ref().map(|o| o.ix);
        let mut bar = h_flex()
            .id("menu-bar")
            .gap_0p5()
            .on_hover(cx.listener(|this, hovered: &bool, window, cx| this.menu_hover(*hovered, window, cx)));
        for (ix, (id, title)) in MENUS.into_iter().enumerate() {
            let is_open = open_ix == Some(ix);
            let menu = self
                .menubar
                .open
                .as_ref()
                .filter(|o| o.ix == ix)
                .map(|o| o.menu.clone());
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("{id}-slot")))
                    .relative()
                    .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                        if *hovered {
                            // Resting on a title opens it, or moves an open menu to it.
                            let pinned = this.menubar.open.as_ref().is_some_and(|o| o.pinned);
                            this.open_menu(ix, pinned, window, cx);
                        }
                    }))
                    .child(
                        Button::new(id)
                            .label(title)
                            .small()
                            .ghost()
                            .selected(is_open)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    // Not a window drag: the bar sits in the title row.
                                    window.prevent_default();
                                    cx.stop_propagation();
                                    this.menu_title_down(ix, window, cx);
                                }),
                            ),
                    )
                    .when(is_open, |d| {
                        let cell = self.menubar.title_bounds.clone();
                        d.child(
                            canvas(|_, _, _| {}, move |bounds, _, _, _| cell.set(Some(bounds)))
                                .absolute()
                                .size_full(),
                        )
                    })
                    .when_some(menu, |d, menu| {
                        d.child(
                            div().absolute().top_full().left_0().child(
                                deferred(anchored().snap_to_window_with_margin(px(8.)).child(
                                    div().id("menu-popup").relative().occlude().pt_1().child(menu).child({
                                        let cell = self.menubar.popup_bounds.clone();
                                        canvas(|_, _, _| {}, move |bounds, _, _, _| cell.set(Some(bounds)))
                                            .absolute()
                                            .size_full()
                                    }),
                                ))
                                .with_priority(2),
                            ),
                        )
                    }),
            );
        }
        bar
    }
}
