//! The tab strip's context menu (right-click a tab), closing tabs in bulk with one
//! confirmation, reopening closed tabs, and middle-click to close.

use super::*;

/// Closed tabs kept for "Reopen closed tab", newest last.
pub(super) const CLOSED_KEPT: usize = 20;

impl Workspace {
    /// Closes the tabs with these ids. Tabs with uncommitted changes are listed in one
    /// confirmation for the lot; without any, they close at once. `then` is activated
    /// afterwards when it is still open (the tab the menu was opened on).
    pub(super) fn close_tabs(&mut self, ids: Vec<Uuid>, then: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let dirty: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| ids.contains(&t.id(cx)) && t.is_dirty(cx))
            .map(|t| t.title(cx))
            .collect();
        if dirty.is_empty() {
            self.close_tabs_now(&ids, then, window, cx);
            return;
        }
        let weak = cx.weak_entity();
        let title = match ids.len() {
            1 => format!("Close {} with uncommitted changes?", dirty[0]),
            n => format!("Close {n} tabs?"),
        };
        let description = match dirty.len() {
            _ if ids.len() == 1 => "The changes are discarded.".to_string(),
            1 => format!("{} has uncommitted changes; they are discarded.", dirty[0]),
            n => format!("{n} of them have uncommitted changes ({}); they are discarded.", dirty.join(", ")),
        };
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (weak, ids) = (weak.clone(), ids.clone());
            alert
                .title(title.clone())
                .description(description.clone())
                .confirm()
                .ok_text("Discard and close")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| this.close_tabs_now(&ids, then, window, cx));
                    true
                })
        });
    }

    fn close_tabs_now(&mut self, ids: &[Uuid], then: Option<Uuid>, window: &mut Window, cx: &mut Context<Self>) {
        for id in ids {
            if let Some(ix) = self.tabs.iter().position(|t| t.id(cx) == *id) {
                self.close_tab_now(ix, cx);
            }
        }
        if let Some(ix) = then.and_then(|id| self.tabs.iter().position(|t| t.id(cx) == id)) {
            self.active = Some(ix);
        }
        match self.active {
            Some(a) => self.activate(a, window, cx),
            None => self.focus_handle.focus(window, cx),
        }
    }

    /// Ctrl+Shift+T: the last closed tab comes back where tabs open, with its filter, sort and
    /// page size (a table) or its text (a console). A table already open again is activated.
    pub(super) fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(st) = self.closed.pop() else { return };
        if let TabState::Table { connection, schema, table, .. } = &st
            && let Some(ix) = self.tabs.iter().position(|t| match t {
                TabView::Table(v) => {
                    let v = v.read(cx);
                    v.conn.id == *connection && v.schema == *schema && v.table == *table
                }
                _ => false,
            })
        {
            self.activate(ix, window, cx);
            return;
        }
        let connection = match &st {
            TabState::Table { connection, .. } | TabState::Console { connection, .. } => *connection,
        };
        // A tab of a connection removed since cannot come back; try the one before it.
        if self.project.connection(connection).is_none() {
            self.reopen_closed_tab(window, cx);
            return;
        }
        let tab = self.make_tab(st, window, cx);
        self.tabs.push(tab);
        let ix = self.tabs.len() - 1;
        self.activate(ix, window, cx);
        self.schedule_save(cx);
    }

    /// The sidebar shows the tab's table (its connection and schema expanded), or for a
    /// console its connection.
    fn reveal_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix).cloned() else { return };
        self.sidebar_visible = true;
        let conn = tab.connection(cx);
        let TabView::Table(v) = &tab else {
            self.side_reveal(conn, cx);
            return;
        };
        let (schema, table) = {
            let v = v.read(cx);
            (v.schema.clone(), v.table.clone())
        };
        let Some(rt) = self.conns.get_mut(&conn) else { return };
        rt.expanded = true;
        rt.expanded_schemas.insert(schema.clone());
        let si = rt.schemas.iter().position(|s| s.name == schema);
        let ri = rt.relations.get(&schema).and_then(|r| r.iter().position(|r| r.name == table));
        self.side_reveal(conn, cx);
        let target = match (si, ri) {
            (Some(si), Some(ri)) => Some(sidebar::Row::Rel(conn, si, ri)),
            (Some(si), None) => Some(sidebar::Row::Schema(conn, si)),
            _ => None,
        };
        if let Some(ix) = target.and_then(|t| self.side.rows.iter().position(|r| *r == t)) {
            self.side_select(ix, cx);
        }
    }
}

/// What a menu item does to the workspace.
type Act = Box<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// The menu for tab `ix`, built when it opens.
pub(super) fn tab_menu(menu: PopupMenu, weak: WeakEntity<Workspace>, ix: usize, cx: &App) -> PopupMenu {
    let Some(ws) = weak.upgrade() else { return menu };
    let ws = ws.read(cx);
    let Some(tab) = ws.tabs.get(ix) else { return menu };
    let id = tab.id(cx);
    let conn = tab.connection(cx);
    let ids: Vec<Uuid> = ws.tabs.iter().map(|t| t.id(cx)).collect();
    let others: Vec<Uuid> = ids.iter().copied().filter(|t| *t != id).collect();
    let right: Vec<Uuid> = ids[ix + 1..].to_vec();
    let unchanged: Vec<Uuid> = ws.tabs.iter().filter(|t| !t.is_dirty(cx)).map(|t| t.id(cx)).collect();
    let same_conn: Vec<Uuid> = ws.tabs.iter().filter(|t| t.connection(cx) == conn).map(|t| t.id(cx)).collect();
    let conn_name = ws.project.connection(conn).map(|c| c.name.clone()).unwrap_or_default();
    let dialect = ws.project.connection(conn).map(|c| c.engine.dialect()).unwrap_or_default();
    let can_reopen = !ws.closed.is_empty();
    let table = match tab {
        TabView::Table(v) => {
            let v = v.read(cx);
            Some((v.schema.clone(), v.table.clone()))
        }
        TabView::Console(_) => None,
    };
    let console_text = match tab {
        TabView::Console(v) => match v.read(cx).state(cx) {
            TabState::Console { text, .. } => Some(text),
            _ => None,
        },
        TabView::Table(_) => None,
    };

    // Each item updates the workspace; `close` builds one that closes a set of tabs.
    let on = |f: Act| {
        let w = weak.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = w.update(cx, |this, cx| f(this, window, cx));
        }
    };
    let close = |label: String, set: Vec<Uuid>, then: Option<Uuid>| {
        let n = set.len();
        PopupMenuItem::new(label)
            .disabled(n == 0)
            .on_click(on(Box::new(move |this, window, cx| this.close_tabs(set.clone(), then, window, cx))))
    };

    let mut menu = menu
        .item(close("Close".into(), vec![id], None).action(Box::new(CloseTab)))
        .item(close("Close others".into(), others, Some(id)))
        .item(close("Close tabs to the right".into(), right, Some(id)))
        .item(close("Close all".into(), ids, None))
        .item(close("Close unchanged tabs".into(), unchanged, Some(id)))
        .item(close(format!("Close all tabs of {conn_name}"), same_conn, None))
        .separator()
        .item(
            PopupMenuItem::new("Reopen closed tab")
                .disabled(!can_reopen)
                .action(Box::new(ReopenClosedTab))
                .on_click(on(Box::new(|this, window, cx| this.reopen_closed_tab(window, cx)))),
        )
        .separator();
    if let Some((schema, name)) = table {
        let sql = format!("SELECT * FROM {} LIMIT 100;", dialect.qualified(&schema, &name));
        let qualified = dialect.qualified(&schema, &name);
        menu = menu
            .item(PopupMenuItem::new("Open in console").on_click(on(Box::new(move |this, window, cx| {
                this.open_console(conn, Some(sql.clone()), window, cx)
            }))))
            .item(PopupMenuItem::new("Copy table name").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(qualified.clone()));
            }))
            .item(PopupMenuItem::new("Reveal in sidebar").on_click(on(Box::new(move |this, _, cx| {
                if let Some(ix) = this.tabs.iter().position(|t| t.id(cx) == id) {
                    this.reveal_tab(ix, cx);
                }
            }))));
    } else {
        menu = menu
            .item(PopupMenuItem::new("Duplicate console").on_click(on(Box::new(move |this, window, cx| {
                this.open_console(conn, console_text.clone(), window, cx)
            }))))
            .item(PopupMenuItem::new("Reveal connection in sidebar").on_click(on(Box::new(move |this, _, cx| {
                if let Some(ix) = this.tabs.iter().position(|t| t.id(cx) == id) {
                    this.reveal_tab(ix, cx);
                }
            }))));
    }
    menu
}
