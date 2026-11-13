//! The sidebar tree: connections grouped by cluster, a filter across every
//! connected database, full keyboard use, one state dot per connection, row estimates.
//!
//! Rows are rebuilt every frame as small index records and drawn with `uniform_list`, so
//! only the visible ones are laid out, however many relations the databases hold.

use std::cell::Cell;

use baselynx_core::model::ConnectionColor;
use gpui_kit::component::input::{Input, InputEvent, InputState};

use super::*;

/// One line of the tree. Every row has the same height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Row {
    /// A cluster group header: index into `SideState::groups`.
    Group(usize),
    Conn(Uuid),
    ConnError(Uuid),
    /// Connection, index into its schemas.
    Schema(Uuid, usize),
    Loading(Uuid),
    Empty(Uuid),
    /// Connection, schema index, index into that schema's relations.
    Rel(Uuid, usize, usize),
    /// A filter result: connection, index into its `all_relations`.
    Match(Uuid, usize),
    NoMatches,
}

pub(super) struct Group {
    pub name: String,
    pub conns: Vec<Uuid>,
    pub color: Option<ConnectionColor>,
}

/// Cached search labels, rebuilt when the connected catalogs change.
struct SearchIndex {
    generation: u64,
    labels: Arc<Vec<(String, u8)>>,
    targets: Arc<Vec<(Uuid, usize)>>,
}

pub(super) struct SideState {
    pub filter: Entity<InputState>,
    query: String,
    matches: Vec<(Uuid, usize)>,
    revision: u64,
    search: Option<Task<()>>,
    index: Option<SearchIndex>,
    /// Bumped whenever a connection's relation list changes.
    pub generation: u64,
    pub selected: Option<usize>,
    pub scroll: UniformListScrollHandle,
    pub collapsed: HashSet<String>,
    pub rows: Rc<Vec<Row>>,
    pub groups: Rc<Vec<Group>>,
    pub menu: Option<(Entity<PopupMenu>, Point<Pixels>, Subscription)>,
    selected_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

const MAX_MATCHES: usize = 500;

impl SideState {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> (Self, Subscription) {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables  Ctrl+Shift+K"));
        let sub = cx.subscribe_in(&filter, window, |this, input, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change => {
                let q = input.read(cx).value().to_string();
                this.side_search(q, cx);
            }
            InputEvent::PressEnter { .. } => this.side_open_selected(window, cx),
            _ => {}
        });
        (
            Self {
                filter,
                query: String::new(),
                matches: Vec::new(),
                revision: 0,
                search: None,
                index: None,
                generation: 0,
                selected: None,
                scroll: UniformListScrollHandle::new(),
                collapsed: HashSet::new(),
                rows: Rc::new(Vec::new()),
                groups: Rc::new(Vec::new()),
                menu: None,
                selected_bounds: Rc::new(Cell::new(None)),
            },
            sub,
        )
    }

    pub fn filtering(&self) -> bool {
        !self.query.trim().is_empty()
    }
}

/// `1.2M`, `340`: a row estimate in a few characters.
pub fn compact_count(n: u64) -> String {
    fn one(v: f64, unit: &str) -> String {
        if v < 9.95 {
            format!("{v:.1}{unit}")
        } else {
            format!("{v:.0}{unit}")
        }
    }
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => one(n as f64 / 1e3, "K"),
        1_000_000..=999_999_999 => one(n as f64 / 1e6, "M"),
        _ => one(n as f64 / 1e9, "B"),
    }
}

/// The name shown for a connection inside its cluster group: `orders-db` for
/// `shop-prod · orders-db`.
pub fn short_name(c: &ConnectionConfig, grouped: bool) -> &str {
    match (&c.kube, grouped) {
        (Some(k), true) => c
            .name
            .strip_prefix(&k.cluster)
            .and_then(|r| r.strip_prefix(" · "))
            .unwrap_or(&c.name),
        _ => &c.name,
    }
}

impl Workspace {
    // ------------------------------------------------------------------ model

    fn side_groups(&self) -> Vec<Group> {
        let grouped = self.project.connections.iter().any(|c| c.kube.is_some());
        if !grouped {
            return vec![Group {
                name: String::new(),
                conns: self.project.connections.iter().map(|c| c.id).collect(),
                color: None,
            }];
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut other = Vec::new();
        for c in &self.project.connections {
            match &c.kube {
                Some(k) => match groups.iter_mut().find(|g| g.name == k.cluster) {
                    Some(g) => g.conns.push(c.id),
                    None => groups.push(Group {
                        name: k.cluster.clone(),
                        conns: vec![c.id],
                        color: None,
                    }),
                },
                None => other.push(c.id),
            }
        }
        if !other.is_empty() {
            groups.push(Group {
                name: "Other".into(),
                conns: other,
                color: None,
            });
        }
        // A group shows a colour when all its connections share one.
        for g in &mut groups {
            let colors: HashSet<ConnectionColor> = g
                .conns
                .iter()
                .filter_map(|id| self.project.connection(*id).map(|c| c.color))
                .collect();
            if colors.len() == 1 {
                g.color = colors.into_iter().next().filter(|c| *c != ConnectionColor::None);
            }
        }
        groups
    }

    pub(super) fn rebuild_side_rows(&mut self) {
        let groups = self.side_groups();
        let mut rows = Vec::new();
        if self.side.filtering() {
            let mut order: Vec<Uuid> = Vec::new();
            for (id, _) in &self.side.matches {
                if !order.contains(id) {
                    order.push(*id);
                }
            }
            for id in order {
                rows.push(Row::Conn(id));
                rows.extend(
                    self.side
                        .matches
                        .iter()
                        .filter(|(c, _)| *c == id)
                        .map(|(c, i)| Row::Match(*c, *i)),
                );
            }
            if rows.is_empty() && self.side.search.is_none() {
                rows.push(Row::NoMatches);
            }
        } else {
            let headers = groups.len() > 1 || groups.first().is_some_and(|g| !g.name.is_empty());
            for (gi, g) in groups.iter().enumerate() {
                if headers {
                    rows.push(Row::Group(gi));
                    if self.side.collapsed.contains(&g.name) {
                        continue;
                    }
                }
                for id in &g.conns {
                    self.push_conn_rows(*id, &mut rows);
                }
            }
        }
        self.side.rows = Rc::new(rows);
        self.side.groups = Rc::new(groups);
        if let Some(sel) = self.side.selected
            && sel >= self.side.rows.len()
        {
            self.side.selected = self.side.rows.len().checked_sub(1);
        }
    }

    fn push_conn_rows(&self, id: Uuid, rows: &mut Vec<Row>) {
        rows.push(Row::Conn(id));
        let Some(rt) = self.conns.get(&id) else { return };
        if !rt.expanded {
            return;
        }
        if let ConnStatus::Failed(_) = &rt.status {
            rows.push(Row::ConnError(id));
            return;
        }
        for (si, s) in rt.schemas.iter().enumerate() {
            rows.push(Row::Schema(id, si));
            if rt.expanded_schemas.contains(&s.name) {
                match rt.relations.get(&s.name) {
                    Some(rels) if rels.is_empty() => rows.push(Row::Empty(id)),
                    Some(rels) => rows.extend((0..rels.len()).map(|ri| Row::Rel(id, si, ri))),
                    None => rows.push(Row::Loading(id)),
                }
            }
        }
    }

    // ------------------------------------------------------------------ filter

    fn side_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.side.query = query.clone();
        self.side.revision += 1;
        let revision = self.side.revision;
        self.side.selected = None;
        if query.trim().is_empty() {
            self.side.matches.clear();
            self.side.search = None;
            cx.notify();
            return;
        }
        if self.side.index.as_ref().map(|i| i.generation) != Some(self.side.generation) {
            let mut labels = Vec::new();
            let mut targets = Vec::new();
            for c in &self.project.connections {
                if let Some(rt) = self.conns.get(&c.id) {
                    for (i, r) in rt.all_relations.iter().enumerate() {
                        labels.push((format!("{}.{}", r.schema, r.name), 0u8));
                        targets.push((c.id, i));
                    }
                }
            }
            self.side.index = Some(SearchIndex {
                generation: self.side.generation,
                labels: Arc::new(labels),
                targets: Arc::new(targets),
            });
        }
        let Some(index) = self.side.index.as_ref() else { return };
        let (labels, targets) = (index.labels.clone(), index.targets.clone());
        let ranked = cx.background_spawn(async move { crate::palette_rank::rank(&labels, &query, MAX_MATCHES) });
        self.side.search = Some(cx.spawn(async move |this, cx| {
            let found = ranked.await;
            let _ = this.update(cx, |this, cx| {
                if this.side.revision == revision {
                    this.side.matches = found.into_iter().map(|i| targets[i]).collect();
                    this.side.search = None;
                    // The first table is selected, so Enter opens it.
                    this.rebuild_side_rows();
                    this.side.selected = this.side.rows.iter().position(|r| matches!(r, Row::Match(..)));
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn clear_side_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.side.filter.update(cx, |i, cx| i.set_value("", window, cx));
        self.side_search(String::new(), cx);
        self.sidebar_focus.focus(window, cx);
    }

    pub(super) fn focus_side_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.side.filter.update(cx, |i, cx| i.focus(window, cx));
        cx.notify();
    }

    /// Typing while the tree has focus starts filtering.
    pub(super) fn side_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sidebar_focus.is_focused(window) {
            return;
        }
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        let Some(ch) = ev
            .keystroke
            .key_char
            .clone()
            .filter(|c| !c.chars().any(char::is_control) && !c.trim().is_empty())
        else {
            return;
        };
        cx.stop_propagation();
        self.side.filter.update(cx, |i, cx| {
            i.set_value(ch.clone(), window, cx);
            i.focus(window, cx);
            let end = ch.len();
            i.set_selected_range(end..end, cx);
        });
        self.side_search(ch, cx);
    }

    // ------------------------------------------------------------------ keyboard

    pub(super) fn side_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.side.rows.len() {
            self.side.selected = Some(ix);
            self.side.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            cx.notify();
        }
    }

    pub(super) fn side_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.side.rows.len();
        if n == 0 {
            return;
        }
        let next = match self.side.selected {
            None => 0,
            Some(i) => (i as isize + delta).clamp(0, n as isize - 1) as usize,
        };
        self.side_select(next, cx);
    }

    /// The row's parent: a schema's connection, a relation's schema, a connection's group.
    fn side_parent(&self, ix: usize) -> Option<usize> {
        let rows = &self.side.rows;
        let row = *rows.get(ix)?;
        let is_parent = |r: &Row| match row {
            Row::Conn(_) => matches!(r, Row::Group(_)),
            Row::Schema(id, _) | Row::ConnError(id) => matches!(r, Row::Conn(c) if *c == id),
            Row::Rel(id, si, _) => matches!(r, Row::Schema(c, s) if *c == id && *s == si),
            Row::Loading(id) | Row::Empty(id) => matches!(r, Row::Schema(c, _) if *c == id),
            Row::Match(id, _) => matches!(r, Row::Conn(c) if *c == id),
            _ => false,
        };
        rows[..ix].iter().rposition(is_parent)
    }

    pub(super) fn side_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.side.selected else {
            self.side_move(1, cx);
            return;
        };
        let Some(row) = self.side.rows.get(ix).copied() else {
            return;
        };
        let expanded = self.side_is_expanded(row);
        match (row, expanded) {
            (Row::Group(_) | Row::Conn(_) | Row::Schema(..), Some(false)) => self.side_activate(row, false, window, cx),
            (Row::Group(_) | Row::Conn(_) | Row::Schema(..), Some(true)) => self.side_move(1, cx),
            _ => {}
        }
    }

    pub(super) fn side_left(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.side.selected else { return };
        let Some(row) = self.side.rows.get(ix).copied() else {
            return;
        };
        if self.side_is_expanded(row) == Some(true) {
            self.side_activate(row, false, window, cx);
        } else if let Some(p) = self.side_parent(ix) {
            self.side_select(p, cx);
        }
    }

    /// `Some(expanded)` for rows that open and close.
    fn side_is_expanded(&self, row: Row) -> Option<bool> {
        match row {
            Row::Group(gi) => self.side.groups.get(gi).map(|g| !self.side.collapsed.contains(&g.name)),
            Row::Conn(id) if !self.side.filtering() => self.conns.get(&id).map(|r| r.expanded),
            Row::Schema(id, si) => self
                .conns
                .get(&id)
                .and_then(|rt| rt.schemas.get(si).map(|s| rt.expanded_schemas.contains(&s.name))),
            _ => None,
        }
    }

    pub(super) fn side_open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.side.selected.and_then(|i| self.side.rows.get(i).copied()) else {
            return;
        };
        self.side_activate(row, true, window, cx);
    }

    /// What a click (or Enter, with `open`) does on a row: toggle, connect, open a table.
    pub(super) fn side_activate(&mut self, row: Row, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        match row {
            Row::Group(gi) => {
                if let Some(g) = self.side.groups.get(gi)
                    && !self.side.collapsed.remove(&g.name)
                {
                    self.side.collapsed.insert(g.name.clone());
                }
            }
            Row::Conn(id) => {
                let status = self.conns.get(&id).map(|r| match r.status {
                    ConnStatus::Connected { .. } => 0,
                    ConnStatus::Failed(_) => 1,
                    ConnStatus::Connecting => 2,
                    ConnStatus::Disconnected => 3,
                });
                match status {
                    Some(0 | 1) => {
                        if let Some(rt) = self.conns.get_mut(&id) {
                            rt.expanded = !rt.expanded;
                        }
                    }
                    Some(3) => self.connect(id, window, cx),
                    _ => {}
                }
            }
            Row::Schema(id, si) => {
                let mut load = None;
                if let Some(rt) = self.conns.get_mut(&id)
                    && let Some(name) = rt.schemas.get(si).map(|s| s.name.clone())
                    && !rt.expanded_schemas.remove(&name)
                {
                    rt.expanded_schemas.insert(name.clone());
                    if !rt.relations.contains_key(&name) {
                        load = Some(name);
                    }
                }
                if let Some(name) = load {
                    self.load_relations(id, name, window, cx);
                }
            }
            Row::Rel(..) | Row::Match(..) if open => {
                if let Some((id, schema, name)) = self.side_relation(row) {
                    self.open_table(id, &schema, &name, window, cx);
                }
            }
            Row::ConnError(id) if open => {
                if let Some(rt) = self.conns.get_mut(&id) {
                    rt.status = ConnStatus::Disconnected;
                }
                // The error row goes away; the selection stays on its connection.
                if let Some(ix) = self.side.rows.iter().position(|r| *r == Row::Conn(id)) {
                    self.side.selected = Some(ix);
                }
                self.connect(id, window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    fn side_relation(&self, row: Row) -> Option<(Uuid, String, String)> {
        let r = match row {
            Row::Rel(id, si, ri) => {
                let rt = self.conns.get(&id)?;
                let schema = rt.schemas.get(si)?;
                rt.relations.get(&schema.name)?.get(ri)?
            }
            Row::Match(id, i) => self.conns.get(&id)?.all_relations.get(i)?,
            _ => return None,
        };
        let id = match row {
            Row::Rel(id, ..) | Row::Match(id, _) => id,
            _ => return None,
        };
        Some((id, r.schema.clone(), r.name.clone()))
    }

    /// The context menu key: the selected row's menu, at the row.
    pub(super) fn side_menu_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.side.selected.and_then(|i| self.side.rows.get(i).copied()) else {
            return;
        };
        let weak = cx.weak_entity();
        let menu = match row {
            Row::Conn(id) => {
                let ro = self.project.connection(id).is_some_and(|c| c.read_only);
                let restore = self.project.connection(id).is_some_and(|c| c.allow_restore);
                PopupMenu::build(window, cx, move |m, _, _| conn_menu(m, weak.clone(), id, ro, restore))
            }
            Row::Rel(..) | Row::Match(..) => {
                let Some((id, schema, name)) = self.side_relation(row) else {
                    return;
                };
                PopupMenu::build(window, cx, move |m, _, _| rel_menu(m, weak.clone(), id, &schema, &name))
            }
            _ => return,
        };
        let at = self
            .side
            .selected_bounds
            .get()
            .map(|b| point(b.left() + px(24.), b.bottom()))
            .unwrap_or_else(|| window.mouse_position());
        menu.read(cx).focus_handle(cx).focus(window, cx);
        let sub = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.side.menu = None;
            this.sidebar_focus.focus(window, cx);
            cx.notify();
        });
        self.side.menu = Some((menu, at, sub));
        cx.notify();
    }

    /// Selects and scrolls to a connection row.
    pub(super) fn side_reveal(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(g) = self.side_groups().iter().find(|g| g.conns.contains(&id)) {
            self.side.collapsed.remove(&g.name);
        }
        self.rebuild_side_rows();
        if let Some(ix) = self.side.rows.iter().position(|r| *r == Row::Conn(id)) {
            self.side_select(ix, cx);
        }
    }

    // ------------------------------------------------------------------ drawing

    fn render_side_item(&self, ix: usize, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.side.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        let secondary = tokens::secondary_text(cx);
        let selected = self.side.selected == Some(ix);
        let grouped = self.side.groups.len() > 1 || self.side.groups.first().is_some_and(|g| !g.name.is_empty());
        let filtering = self.side.filtering();
        let indent = if grouped && !filtering { 1 } else { 0 };
        let bounds_cell = self.side.selected_bounds.clone();
        let base = h_flex()
            .id(("side", ix))
            .relative()
            .h_7()
            .pr_2()
            .gap_1()
            .w_full()
            .text_sm()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .when(selected, |d| {
                d.bg(if focused {
                    cx.theme().list_active
                } else {
                    cx.theme().list_active.opacity(0.45)
                })
                .child(
                    canvas(move |b, _, _| bounds_cell.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
            })
            .when(!selected, |d| d.hover(|d| d.bg(cx.theme().sidebar_accent)))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.side.selected = Some(ix);
                this.sidebar_focus.focus(window, cx);
                let open = ev.click_count() >= 2;
                match row {
                    // One click toggles; a double click opens a table or connects.
                    Row::Rel(..) | Row::Match(..) => {
                        if open {
                            this.side_activate(row, true, window, cx);
                        }
                    }
                    Row::Conn(id) => {
                        let connected = matches!(
                            this.conns.get(&id).map(|r| &r.status),
                            Some(ConnStatus::Connected { .. } | ConnStatus::Failed(_))
                        );
                        if connected || !open {
                            this.side_activate(row, false, window, cx);
                        } else {
                            this.connect(id, window, cx);
                        }
                    }
                    _ => this.side_activate(row, false, window, cx),
                }
                cx.notify();
            }));
        let pad = |level: usize| px(8. + 14. * level as f32);
        let chevron = |open: bool| {
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .xsmall()
            .text_color(secondary)
        };
        let env_bar = |color: Option<Hsla>| {
            div()
                .absolute()
                .left_0()
                .top_1()
                .bottom_1()
                .w(px(2.))
                .rounded_full()
                .when_some(color, |d, c| d.bg(c))
        };
        match row {
            Row::Group(gi) => {
                let Some(g) = self.side.groups.get(gi) else {
                    return div().into_any_element();
                };
                let open = !self.side.collapsed.contains(&g.name);
                let (ok, total) = (
                    g.conns
                        .iter()
                        .filter(|id| {
                            matches!(
                                self.conns.get(id).map(|r| &r.status),
                                Some(ConnStatus::Connected { .. })
                            )
                        })
                        .count(),
                    g.conns.len(),
                );
                base.pl(pad(0))
                    .child(env_bar(g.color.and_then(|c| tokens::connection(c, cx))))
                    .child(chevron(open))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(secondary)
                            .child(g.name.to_uppercase()),
                    )
                    .child(div().text_xs().text_color(secondary).child(format!("{ok}/{total}")))
                    .into_any_element()
            }
            Row::Conn(id) => {
                let Some(c) = self.project.connection(id) else {
                    return div().into_any_element();
                };
                let rt = self.conns.get(&id);
                let expanded = rt.is_some_and(|r| r.expanded) && !filtering;
                let (state_color, state_label) = match rt.map(|r| &r.status) {
                    Some(ConnStatus::Connected { .. }) => (Some(cx.theme().success), "connected"),
                    Some(ConnStatus::Connecting) => (Some(cx.theme().warning), "connecting"),
                    Some(ConnStatus::Failed(_)) => (Some(cx.theme().danger), "failed"),
                    _ => (None, "disconnected"),
                };
                let name = if filtering {
                    c.name.clone()
                } else {
                    short_name(c, grouped).to_string()
                };
                let read_only = c.read_only;
                let mut tip = match &c.kube {
                    Some(k) => format!("{} · {state_label} · {}", c.name, k.describe()),
                    None => format!("{} · {state_label} · {}", c.name, c.describe()),
                };
                if let Some(ConnStatus::Failed(e)) = rt.map(|r| &r.status) {
                    tip.push('\n');
                    tip.push_str(e);
                }
                let tip: SharedString = tip.into();
                let weak = cx.weak_entity();
                base.pl(pad(indent))
                    .child(env_bar(tokens::connection(c.color, cx)))
                    .when(!filtering, |d| d.child(chevron(expanded)))
                    .child(div().flex_1().truncate().font_weight(FontWeight::MEDIUM).child(name))
                    .when(read_only, |d| {
                        d.child(
                            Icon::new(gpui_kit::assets::IconName::Lock)
                                .xsmall()
                                .text_color(secondary),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .size_2()
                            .rounded_full()
                            .when_some(state_color, |d, col| d.bg(col))
                            .when(state_color.is_none(), |d| d.border_1().border_color(secondary)),
                    )
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .context_menu(move |menu: PopupMenu, _, cx| {
                        let (ro, restore) = weak
                            .upgrade()
                            .and_then(|w| {
                                w.read(cx)
                                    .project
                                    .connection(id)
                                    .map(|c| (c.read_only, c.allow_restore))
                            })
                            .unwrap_or((false, false));
                        conn_menu(menu, weak.clone(), id, ro, restore)
                    })
                    .into_any_element()
            }
            Row::ConnError(id) => {
                let error = match self.conns.get(&id).map(|r| &r.status) {
                    Some(ConnStatus::Failed(e)) => e.clone(),
                    _ => String::new(),
                };
                let tip: SharedString = error.clone().into();
                base.pl(pad(indent + 1))
                    .child(
                        div()
                            .id(("side-err-text", ix))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(tokens::error_text(cx))
                            .child(error)
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                            }),
                    )
                    .child(
                        Button::new(("retry", ix))
                            .label("Retry")
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(rt) = this.conns.get_mut(&id) {
                                    rt.status = ConnStatus::Disconnected;
                                }
                                this.connect(id, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("edit", ix))
                            .label("Edit")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(c) = this.project.connection(id).cloned() {
                                    this.open_connection_form(c, false, window, cx);
                                }
                            })),
                    )
                    .into_any_element()
            }
            Row::Schema(id, si) => {
                let Some(rt) = self.conns.get(&id) else {
                    return div().into_any_element();
                };
                let Some(s) = rt.schemas.get(si) else {
                    return div().into_any_element();
                };
                let open = rt.expanded_schemas.contains(&s.name);
                base.pl(pad(indent + 1))
                    .child(chevron(open))
                    .child(
                        div()
                            .truncate()
                            .when(s.system, |d| d.text_color(secondary))
                            .child(s.name.clone()),
                    )
                    .into_any_element()
            }
            Row::Loading(_) => base
                .pl(pad(indent + 3))
                .text_color(secondary)
                .child("Loading…")
                .into_any_element(),
            Row::Empty(_) => base
                .pl(pad(indent + 3))
                .text_color(secondary)
                .child("No tables")
                .into_any_element(),
            Row::NoMatches => base
                .pl(pad(0))
                .text_color(secondary)
                .child("No table matches")
                .into_any_element(),
            Row::Rel(..) | Row::Match(..) => {
                let rel = match row {
                    Row::Rel(id, si, ri) => self
                        .conns
                        .get(&id)
                        .and_then(|rt| rt.schemas.get(si).and_then(|s| rt.relations.get(&s.name)?.get(ri))),
                    Row::Match(id, i) => self.conns.get(&id).and_then(|rt| rt.all_relations.get(i)),
                    _ => None,
                };
                let Some(r) = rel else {
                    return div().into_any_element();
                };
                let level = if filtering { 1 } else { indent + 2 };
                let label = if filtering {
                    format!("{}.{}", r.schema, r.name)
                } else {
                    r.name.clone()
                };
                let estimate = r.estimate.filter(|_| r.kind.is_table()).map(compact_count);
                let Some((id, schema, name)) = self.side_relation(row) else {
                    return div().into_any_element();
                };
                let weak = cx.weak_entity();
                base.pl(pad(level))
                    .child(
                        Icon::new(match r.kind {
                            RelKind::View | RelKind::MaterializedView => gpui_kit::assets::IconName::Eye,
                            _ => gpui_kit::assets::IconName::Table,
                        })
                        .xsmall()
                        .text_color(secondary),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .when_some(estimate, |d, e| {
                        d.child(div().flex_shrink_0().text_xs().text_color(secondary).child(e))
                    })
                    .context_menu(move |menu: PopupMenu, _, _| rel_menu(menu, weak.clone(), id, &schema, &name))
                    .into_any_element()
            }
        }
    }

    pub(super) fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebuild_side_rows();
        let empty = self.project.connections.is_empty();
        let focused = self.sidebar_focus.is_focused(window);
        let body: AnyElement = if empty {
            v_flex()
                .p_4()
                .gap_2()
                .items_center()
                .child(
                    Button::new("first-connection")
                        .primary()
                        .w_full()
                        .icon(IconName::Plus)
                        .label("Add a connection")
                        .on_click(cx.listener(|this, _, window, cx| this.add_connection(&AddConnection, window, cx))),
                )
                .child(
                    Button::new("first-import")
                        .outline()
                        .w_full()
                        .icon(gpui_kit::assets::IconName::Database)
                        .label("Open a baselynx.yaml")
                        .on_click(cx.listener(|this, _, window, cx| this.open_config(&OpenConfig, window, cx))),
                )
                .child(
                    div()
                        .pt_1()
                        .text_sm()
                        .text_color(tokens::secondary_text(cx))
                        .child("or paste a postgres:// URL here"),
                )
                .into_any_element()
        } else {
            let n = self.side.rows.len();
            uniform_list(
                "side-rows",
                n,
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                    let focused = this.sidebar_focus.is_focused(window);
                    range
                        .map(|ix| this.render_side_item(ix, focused, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.side.scroll)
            .size_full()
            .p_1()
            .into_any_element()
        };
        let _ = focused;
        v_flex()
            .id("sidebar")
            .key_context(menu_paste::CONTEXT)
            .track_focus(&self.sidebar_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.side.filter.read(cx).focus_handle(cx).is_focused(window) {
                        this.sidebar_focus.focus(window, cx)
                    }
                }),
            )
            .on_key_down(cx.listener(Self::side_key_down))
            .on_action(cx.listener(|this, _: &menu_paste::PasteUrl, window, cx| this.paste_url(window, cx)))
            .on_action(cx.listener(|this, _: &SideUp, _, cx| this.side_move(-1, cx)))
            .on_action(cx.listener(|this, _: &SideDown, _, cx| this.side_move(1, cx)))
            .on_action(cx.listener(|this, _: &SideLeft, window, cx| this.side_left(window, cx)))
            .on_action(cx.listener(|this, _: &SideRight, window, cx| this.side_right(window, cx)))
            .on_action(cx.listener(|this, _: &SideOpen, window, cx| this.side_open_selected(window, cx)))
            .on_action(cx.listener(|this, _: &SideMenu, window, cx| this.side_menu_key(window, cx)))
            .on_action(cx.listener(|this, _: &SideFilterEscape, window, cx| this.clear_side_filter(window, cx)))
            .size_full()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .h_9()
                    .px_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Connections"))
                    .child(
                        h_flex()
                            .child(
                                Button::new("open-config")
                                    .icon(gpui_kit::assets::IconName::Database)
                                    .xsmall()
                                    .ghost()
                                    .tooltip("Open a config file (baselynx.yaml)")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.open_config(&OpenConfig, window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("add-connection")
                                    .icon(IconName::Plus)
                                    .xsmall()
                                    .ghost()
                                    .tooltip("Add connection")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_connection(&AddConnection, window, cx)
                                    })),
                            ),
                    ),
            )
            .when(!empty, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_1p5()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(Input::new(&self.side.filter).small().cleanable(true)),
                )
            })
            .child(div().flex_1().min_h_0().child(body))
            .when_some(self.side.menu.as_ref(), |this, (menu, position, _)| {
                this.child(
                    deferred(
                        anchored()
                            .position(*position)
                            .snap_to_window_with_margin(px(8.))
                            .child(menu.clone()),
                    )
                    .with_priority(1),
                )
            })
    }
}

/// The connection row's menu, for the right click and the context menu key.
pub(super) fn conn_menu(
    menu: PopupMenu,
    weak: WeakEntity<Workspace>,
    id: Uuid,
    read_only: bool,
    allow_restore: bool,
) -> PopupMenu {
    let item = |label: &str, f: fn(&mut Workspace, Uuid, &mut Window, &mut Context<Workspace>)| {
        let w = weak.clone();
        PopupMenuItem::new(label.to_string()).on_click(move |_, window, cx| {
            let _ = w.update(cx, |this, cx| f(this, id, window, cx));
        })
    };
    menu.item(item("Connect", |this, id, window, cx| this.connect(id, window, cx)))
        .item(item("Disconnect", |this, id, _, cx| this.disconnect(id, cx)))
        .item(item("New console", |this, id, window, cx| {
            this.open_console(id, None, window, cx)
        }))
        .item(item("Refresh", |this, id, window, cx| {
            this.refresh_connection(id, window, cx)
        }))
        .separator()
        // Look without touching (3.8).
        .item(
            item("Read-write (primary)", |this, id, window, cx| {
                this.set_read_only(id, false, window, cx)
            })
            .checked(!read_only),
        )
        .item(
            item("Read-only (replica)", |this, id, window, cx| {
                this.set_read_only(id, true, window, cx)
            })
            .checked(read_only),
        )
        .when(!read_only, |m| {
            m.item(item("Open read-only", |this, id, window, cx| {
                this.open_read_only(id, window, cx)
            }))
        })
        .separator()
        .item(item("Back up now", |this, id, window, cx| {
            this.back_up_now(id, window, cx)
        }))
        // Only where the config file allows it (and only into an empty database).
        .item(
            item("Restore from backup…", |this, id, window, cx| {
                this.restore_backup(id, window, cx)
            })
            .disabled(!allow_restore),
        )
        .separator()
        .item(item("Edit…", |this, id, window, cx| {
            if let Some(c) = this.project.connection(id).cloned() {
                this.open_connection_form(c, false, window, cx);
            }
        }))
        .item(item("Remove…", |this, id, window, cx| {
            this.remove_connection(id, window, cx)
        }))
}

/// A table row's menu.
pub(super) fn rel_menu(menu: PopupMenu, weak: WeakEntity<Workspace>, id: Uuid, schema: &str, name: &str) -> PopupMenu {
    let (w1, w2) = (weak.clone(), weak);
    let (s1, n1, s2, n2) = (
        schema.to_string(),
        name.to_string(),
        schema.to_string(),
        name.to_string(),
    );
    menu.item(PopupMenuItem::new("Open").on_click(move |_, window, cx| {
        let _ = w1.update(cx, |this, cx| this.open_table(id, &s1, &n1, window, cx));
    }))
    .item(PopupMenuItem::new("Open in console").on_click(move |_, window, cx| {
        let _ = w2.update(cx, |this, cx| {
            let d = this
                .project
                .connection(id)
                .map(|c| c.engine.dialect())
                .unwrap_or_default();
            let sql = format!("SELECT * FROM {} LIMIT 100;", d.qualified(&s2, &n2));
            this.open_console(id, Some(sql), window, cx)
        });
    }))
}

#[cfg(test)]
mod tests {
    use super::compact_count;

    #[test]
    fn compact_counts() {
        assert_eq!(compact_count(340), "340");
        assert_eq!(compact_count(1_234), "1.2K");
        assert_eq!(compact_count(56_000), "56K");
        assert_eq!(compact_count(1_200_000), "1.2M");
        assert_eq!(compact_count(3_400_000_000), "3.4B");
    }
}
