//! Table tab.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use baselynx_core::catalog::{self, RelationInfo};
use baselynx_core::edit::{self, CellValue};
use baselynx_core::export::ExportSource;
use baselynx_core::model::{ConnectionColor, SortDir, TabState};
use baselynx_core::session::{self, Session};
use baselynx_core::table::{self, DEFAULT_PAGE_SIZE, PAGE_SIZES, PageRequest};
use baselynx_core::{CoreError, exec};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::actions::*;
use crate::app_state::{ConnLabel, ConnPhase, ConnTarget, ExportJob, TabEvent, TabStatus};
use crate::format;
use crate::ui::export_dialog::{self, ExportScope};
use crate::ui::grid::{
    self as gridmod, CellEditor, ColumnSelection, GridDelegate, GridMode, Jump, apply_edit, cell_menu,
};
use crate::ui::record::RecordPanel;
use crate::ui::review::CommitReview;
use crate::ui::tokens;
use crate::ui::workspace::MenuState;

mod filter_assist;
mod links;

/// A menu item's effect on the view.
type ViewFn = Rc<dyn Fn(&mut TableView, &mut Window, &mut Context<TableView>)>;

pub const REFRESH_CHOICES: [Option<u32>; 6] = [None, Some(1), Some(5), Some(10), Some(30), Some(60)];

#[derive(Clone, Copy)]
pub enum RowFormat {
    Insert,
    Json,
    Tsv,
}

/// A loaded page, kept to tell what the next reload changed.
struct PageSnapshot {
    view: (Option<String>, Option<(String, SortDir)>, u64, u32),
    columns: Vec<String>,
    rows: Vec<baselynx_core::exec::Row>,
}

/// A change of what the tab shows; with pending edits it asks first.
#[derive(Debug, Clone, PartialEq)]
enum Nav {
    Page(u64),
    Filter(Option<String>),
    Sort(Option<(String, SortDir)>),
    PageSize(u32),
    Refresh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Move {
    Down,
    Up,
    Right,
    Stay,
}

pub struct TableView {
    pub id: Uuid,
    pub conn: ConnLabel,
    pub schema: String,
    pub table: String,
    target: Option<ConnTarget>,
    session: Option<Arc<Session>>,
    rel: Option<RelationInfo>,
    filter_input: Entity<InputState>,
    filter: Option<String>,
    sort: Option<(String, SortDir)>,
    page: u64,
    page_size: u32,
    has_next: bool,
    count: Option<u64>,
    counting: Option<Task<()>>,
    refresh_secs: Option<u32>,
    _refresh_loop: Option<Task<()>>,
    grid: Entity<TableState<GridDelegate>>,
    loading: Option<Task<()>>,
    request: u64,
    show_progress: bool,
    loaded_once: bool,
    elapsed: Option<Duration>,
    load_error: Option<CoreError>,
    filter_error: Option<SharedString>,
    connection_lost: bool,
    active: bool,
    reload_when_shown: bool,
    review: Option<Entity<CommitReview>>,
    after_commit: Option<Nav>,
    export: Option<ExportJob>,
    cell_menu: Option<(Entity<PopupMenu>, Point<Pixels>, Subscription)>,
    /// Width of the toolbar in the last frame; labels drop when it is narrow (4.11).
    toolbar_w: Pixels,
    phase: ConnPhase,
    /// A reload after the session died; one attempt (3.8).
    reconnecting: bool,
    note: Option<SharedString>,
    record: Option<Entity<RecordPanel>>,
    record_width: f32,
    assist: filter_assist::FilterAssist,
    /// A selection change we made ourselves, so it does not end the column selection.
    expect_select: Option<(usize, usize)>,
    alternatives: Option<crate::app_state::Alternatives>,
    /// The last page as loaded, to compare the next reload with (3.2).
    snapshot: Option<PageSnapshot>,
    /// When the shown diff was taken: "3 cells changed · 14:02:11".
    diff_at: Option<SharedString>,
    /// The next reload follows a commit: its changes are tinted green (4.13).
    after_commit_tint: bool,
    _diff_clear: Option<Task<()>>,
    child_counts: Option<links::ChildCounts>,
    peek: Option<links::Peek>,
    glance: Option<links::Glance>,
    focus_handle: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<TabEvent> for TableView {}

impl TableView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        conn: ConnLabel,
        target: Option<ConnTarget>,
        schema: String,
        table: String,
        filter: Option<String>,
        sort: Option<(String, SortDir)>,
        page_size: u32,
        refresh_secs: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("WHERE …")
                .default_value(filter.clone().unwrap_or_default())
        });
        let weak = cx.weak_entity();
        let grid = cx.new(|cx| {
            let mut d = GridDelegate::new(GridMode::Table);
            let weak2 = weak.clone();
            let weak3 = weak.clone();
            let weak = weak.clone();
            d.on_sort(Rc::new(move |col, window, cx| {
                let _ = weak.update(cx, |this, cx| this.cycle_sort(col, window, cx));
            }));
            d.on_header_menu(Rc::new(move |col, window, cx| {
                let _ = weak2.update(cx, |this, cx| this.open_header_menu(col, window, cx));
            }));
            d.on_cell_mouse(Rc::new(move |row, col, m, _, cx| {
                weak3
                    .update(cx, |this, cx| this.cell_mouse(row, col, m, cx))
                    .unwrap_or(false)
            }));
            TableState::new(d, window, cx)
                .cell_selectable(true)
                .col_selectable(false)
                .row_selectable(false)
                .row_header(false)
                .sortable(false)
                .col_movable(false)
        });
        let mut subs = Vec::new();
        subs.push(
            cx.subscribe_in(&filter_input, window, |this, _, ev: &InputEvent, _, cx| match ev {
                // Enter is the FilterAccept binding, which also takes a suggestion.
                InputEvent::Change => this.filter_changed(cx),
                InputEvent::Blur => {
                    this.assist.suggest = None;
                    cx.notify();
                }
                _ => {}
            }),
        );
        subs.push(
            cx.subscribe_in(&grid, window, |this, _, ev: &TableEvent, window, cx| match ev {
                TableEvent::DoubleClickedCell(..) => this.begin_edit(None, window, cx),
                // In cell mode the table stops the right click before its own context menu sees it,
                // so the view opens the menu itself.
                TableEvent::RightClickedCell(r, c) => {
                    // The menu's actions work on the selected cell.
                    let (r, c) = (*r, *c);
                    this.grid.update(cx, |g, cx| g.set_selected_cell(r, c, cx));
                    this.open_cell_menu(r, window, cx);
                }
                TableEvent::SelectCell(r, c) => {
                    this.peek = None;
                    // A plain move ends a column selection; our own moves are expected.
                    if this.expect_select.take() != Some((*r, *c)) {
                        this.grid.update(cx, |g, _| g.delegate_mut().multi = None);
                    }
                    cx.notify()
                }
                _ => {}
            }),
        );
        let page_size = if page_size == 0 { DEFAULT_PAGE_SIZE } else { page_size };
        let mut this = Self {
            id,
            conn,
            schema,
            table,
            target,
            session: None,
            rel: None,
            filter_input,
            filter,
            sort,
            page: 0,
            page_size,
            has_next: false,
            count: None,
            counting: None,
            refresh_secs: None,
            _refresh_loop: None,
            grid,
            loading: None,
            request: 0,
            show_progress: false,
            loaded_once: false,
            elapsed: None,
            load_error: None,
            filter_error: None,
            connection_lost: false,
            active: false,
            reload_when_shown: false,
            review: None,
            after_commit: None,
            export: None,
            cell_menu: None,
            toolbar_w: px(10_000.),
            phase: ConnPhase::Connecting,
            reconnecting: false,
            note: None,
            record: None,
            record_width: 360.,
            assist: filter_assist::FilterAssist::new(),
            expect_select: None,
            alternatives: None,
            snapshot: None,
            diff_at: None,
            after_commit_tint: false,
            _diff_clear: None,
            child_counts: None,
            peek: None,
            glance: None,
            focus_handle: cx.focus_handle(),
            _subs: subs,
        };
        this.set_refresh(refresh_secs, window, cx);
        this
    }

    pub fn title(&self) -> String {
        self.table.clone()
    }

    pub fn set_target(&mut self, target: ConnTarget, window: &mut Window, cx: &mut Context<Self>) {
        // The mode changed (3.8): the old session must not be reused.
        let mode_changed = self
            .target
            .as_ref()
            .is_some_and(|t| t.config.read_only != target.config.read_only || t.config.kube != target.config.kube);
        if mode_changed {
            self.session = None;
            self.reload_when_shown = true;
            if self.active && self.loaded_once && !self.is_dirty(cx) {
                self.reload_when_shown = false;
                self.conn = target.label();
                self.target = Some(target);
                self.load(window, cx);
                cx.notify();
                return;
            }
        }
        self.conn = target.label();
        self.target = Some(target);
        if self.active && !self.loaded_once {
            self.load(window, cx);
        }
        cx.notify();
    }

    pub fn has_target(&self) -> bool {
        self.target.is_some()
    }

    /// Shows the table with another filter (following a key, walking the trail).
    pub fn show_filter(&mut self, filter: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter == filter {
            return;
        }
        self.filter_input
            .update(cx, |i, cx| i.set_value(filter.clone().unwrap_or_default(), window, cx));
        self.request_nav(Nav::Filter(filter), window, cx);
    }

    pub fn current_filter(&self) -> Option<String> {
        self.filter.clone()
    }

    /// The connection is in read-only mode (3.8).
    pub fn read_only(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.config.read_only)
    }

    pub fn set_alternatives(&mut self, alt: crate::app_state::Alternatives) {
        self.alternatives = Some(alt);
    }

    pub fn set_phase(&mut self, phase: ConnPhase, cx: &mut Context<Self>) {
        if self.phase != phase {
            self.phase = phase;
            cx.notify();
        }
    }

    pub fn state(&self) -> TabState {
        TabState::Table {
            id: self.id,
            connection: self.conn.id,
            schema: self.schema.clone(),
            table: self.table.clone(),
            filter: self.filter.clone(),
            sort: self.sort.clone(),
            page_size: self.page_size,
            refresh_secs: self.refresh_secs,
        }
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.grid.read(cx).delegate().model.is_dirty()
    }

    pub fn pending(&self, cx: &App) -> usize {
        self.grid.read(cx).delegate().model.pending()
    }

    pub fn is_busy(&self) -> bool {
        self.loading.is_some() || self.export.is_some()
    }

    pub fn status(&self, cx: &App) -> TabStatus {
        let n = self.grid.read(cx).delegate().model.loaded_len() as u64;
        let first = self.page * self.page_size as u64;
        let rows = if !self.loaded_once {
            None
        } else if n == 0 {
            Some("No rows".into())
        } else {
            Some(format!("rows {}–{}", format::count(first + 1), format::count(first + n)).into())
        };
        TabStatus {
            rows,
            timing: self.elapsed.map(|d| format::duration(d).into()),
            error: self
                .filter_error
                .clone()
                .or_else(|| self.load_error.as_ref().map(|e| e.to_string().into())),
            export: self.export.as_ref().map(|e| e.progress()),
            note: self.note.clone(),
        }
    }

    /// The tab became visible or hidden.
    pub fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active && self.target.is_some() && (!self.loaded_once || self.reload_when_shown) {
            self.reload_when_shown = false;
            self.load(window, cx);
        }
    }

    /// The zoom changed: lay out the grid again.
    pub fn relayout(&mut self, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| g.refresh(cx));
        cx.notify();
    }

    /// Rows of the loaded page (tests and the status bar).
    #[cfg(test)]
    pub fn loaded_rows(&self, cx: &App) -> usize {
        self.grid.read(cx).delegate().model.loaded_len()
    }

    /// Zero-based page shown.
    #[cfg(test)]
    pub fn page_number(&self) -> u64 {
        self.page
    }

    /// Selects a data cell (column 0 is the first data column).
    #[cfg(test)]
    pub fn select_cell(&mut self, row: usize, data_col: usize, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| {
            let c = g.delegate().grid_col(data_col).unwrap_or(1);
            g.set_selected_cell(row, c, cx)
        });
        cx.notify();
    }

    /// Drops every pending edit without asking.
    #[cfg(test)]
    pub fn discard_all(&mut self, cx: &mut Context<Self>) {
        self.discard_now(cx);
    }

    fn jump(&mut self, to: Jump, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| gridmod::jump(g, to, cx));
        cx.notify();
    }

    /// Which menu bar commands apply right now (D2).
    pub fn menu_state(&self, cx: &App) -> MenuState {
        let d = self.grid.read(cx).delegate();
        let sel = self.selected(cx);
        let editing = d.editing.is_some();
        let cell = sel.is_some() && !editing;
        let editable_cell = sel.is_some_and(|(r, c)| d.can_edit(r, c)) && !editing;
        let is_new = sel.is_some_and(|(r, _)| d.model.is_new(r));
        MenuState {
            table: true,
            cell,
            editable_cell,
            editable: d.editable && self.loaded_once,
            row_selected: sel.is_some() && d.editable,
            filter_value: cell && !is_new,
            pending: d.model.is_dirty(),
            prev_page: self.page > 0,
            next_page: self.has_next,
            can_count: self.session.is_some() && self.counting.is_none(),
            can_export: self.loaded_once && self.export.is_none() && self.rel.is_some(),
            can_undo: d.model.can_undo() && !editing,
            can_redo: d.model.can_redo() && !editing,
            ..Default::default()
        }
    }

    pub fn grid_focus(&self, cx: &App) -> FocusHandle {
        self.grid.read(cx).focus_handle(cx)
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        let handle = self.grid_focus(cx);
        handle.focus(window, cx);
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.loading = None;
        self._refresh_loop = None;
        if let Some(job) = &self.export {
            job.cancel.cancel();
        }
        self.session = None;
        cx.notify();
    }

    // ------------------------------------------------------------------ loading

    fn page_request(&self) -> PageRequest {
        PageRequest {
            schema: self.schema.clone(),
            table: self.table.clone(),
            filter: self.filter.clone(),
            sort: self.sort.clone(),
            page: self.page,
            page_size: self.page_size,
        }
    }

    /// Reloads the current page; old rows stay until new ones arrive.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target.clone() else {
            return;
        };
        // The session died since the last load (restart, failover, idle cut): a read
        // reconnects once and says so; pending edits never ride on a silent reconnect (3.8).
        if self.session.as_ref().is_some_and(|s| s.is_closed()) {
            if self.is_dirty(cx) {
                self.connection_lost = true;
                cx.notify();
                return;
            }
            self.reconnecting = true;
            self.session = None;
        }
        self.request += 1;
        let request = self.request;
        let existing = self.session.clone().filter(|s| !s.is_closed());
        let rel = self.rel.clone();
        let req = self.page_request();
        self.show_progress = false;
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let _ = this.update(cx, |this, cx| {
                if this.request == request && this.loading.is_some() {
                    this.show_progress = true;
                    cx.notify();
                }
            });
        })
        .detach();
        let (schema, table) = (self.schema.clone(), self.table.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let session = match existing {
                Some(s) => s,
                None => match session::connect(&target.config, target.password.clone()).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = this.update_in(cx, |this, window, cx| this.apply_error(request, e, window, cx));
                        return;
                    }
                },
            };
            let rel = match rel {
                Some(r) => r,
                None => match catalog::describe_relation(&session, &schema, &table).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = this.update_in(cx, |this, window, cx| this.apply_error(request, e, window, cx));
                        return;
                    }
                },
            };
            let result = table::load_page(&session, &rel, req).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.request != request {
                    return;
                }
                this.session = Some(session);
                this.rel = Some(rel.clone());
                match result {
                    Ok(page) => this.apply_page(page, rel, started, window, cx),
                    Err(e) => this.apply_error(request, e, window, cx),
                }
            });
        });
        self.loading = Some(task);
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn apply_page(
        &mut self,
        page: table::Page,
        rel: RelationInfo,
        started: Instant,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Keep the selected cell on the same row when it is still there.
        let keep = self.selected_row_key(cx);
        let offset = self.page * self.page_size as u64;
        let key_cols: Vec<usize> = rel
            .row_key
            .iter()
            .flatten()
            .filter_map(|k| page.columns.iter().position(|c| &c.name == k))
            .collect();
        let new_sel = keep.and_then(|(col, key)| {
            page.rows
                .iter()
                .position(|r| {
                    key_cols
                        .iter()
                        .map(|&i| r.get(i).cloned().flatten())
                        .collect::<Vec<_>>()
                        == key
                })
                .map(|r| (r, col))
        });
        self.has_next = page.has_next;
        let read_only = self.read_only();
        if std::mem::take(&mut self.reconnecting) {
            self.note = Some(format!("Reconnected to {}", self.conn.name).into());
        }
        // Compare with the last load of the same page: same filter, sort, page and size.
        let names: Vec<String> = page.columns.iter().map(|c| c.name.clone()).collect();
        let view_key = (self.filter.clone(), self.sort.clone(), self.page, self.page_size);
        let diff = match (&self.snapshot, rel.row_key.as_deref()) {
            (Some(s), Some(key)) if s.view == view_key => {
                crate::page_diff::diff(&s.columns, &s.rows, &names, &page.rows, key).filter(|d| !d.is_empty())
            }
            _ => None,
        };
        self.snapshot = Some(PageSnapshot {
            view: view_key,
            columns: names,
            rows: page.rows.clone(),
        });
        let committed = std::mem::take(&mut self.after_commit_tint);
        self.diff_at = diff
            .as_ref()
            .map(|_| chrono::Local::now().format("%H:%M:%S").to_string().into());
        if committed && diff.is_some() {
            // The commit tint fades after a moment; a plain refresh tint stays until the next one.
            self._diff_clear = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                let _ = this.update(cx, |this, cx| {
                    this.grid.update(cx, |g, cx| {
                        g.delegate_mut().diff = None;
                        g.refresh(cx);
                    });
                    this.diff_at = None;
                    cx.notify();
                });
            }));
        }
        self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            d.diff = diff;
            d.diff_committed = committed;
            // A read-only connection never edits (3.8).
            d.editable = rel.editable && !read_only;
            d.sort = self
                .sort
                .as_ref()
                .and_then(|(name, dir)| page.columns.iter().position(|c| &c.name == name).map(|i| (i, *dir)));
            d.set_data(&page.columns, page.rows, Some(&rel), offset);
            g.refresh(cx);
            if let Some((r, c)) = new_sel {
                g.set_selected_cell(r, c, cx);
            }
        });
        self.loaded_once = true;
        self.loading = None;
        self.show_progress = false;
        self.load_error = None;
        self.filter_error = None;
        self.connection_lost = false;
        self.elapsed = Some(started.elapsed());
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn apply_error(&mut self, request: u64, err: CoreError, window: &mut Window, cx: &mut Context<Self>) {
        if self.request != request {
            return;
        }
        self.loading = None;
        self.show_progress = false;
        let closed = matches!(err, CoreError::Closed) || self.session.as_ref().is_some_and(|s| s.is_closed());
        // A dead session (restart, failover, idle cut) reconnects once on its own for a
        // read; with pending edits it never does, and the band keeps them (3.8).
        if closed && !self.reconnecting && !self.is_dirty(cx) && self.loaded_once {
            self.reconnecting = true;
            self.session = None;
            self.load(window, cx);
            return;
        }
        self.reconnecting = false;
        if closed {
            self.connection_lost = true;
            self.session = None;
        } else if self.loaded_once && (self.filter.is_some() || matches!(err, CoreError::Invalid(_))) {
            self.filter_error = Some(err.to_string().into());
        } else {
            self.load_error = Some(err);
        }
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn selected_row_key(&self, cx: &App) -> Option<(usize, Vec<Option<String>>)> {
        let g = self.grid.read(cx);
        let (row, col) = g.selected_cell()?;
        let rel = self.rel.as_ref()?;
        let d = g.delegate();
        let key = rel.row_key.as_ref()?;
        let names = d.column_names();
        let values = key
            .iter()
            .map(|k| {
                names
                    .iter()
                    .position(|n| n == k)
                    .and_then(|i| d.model.loaded_value(row, i).map(String::from))
            })
            .collect();
        Some((col, values))
    }

    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session = None;
        self.connection_lost = false;
        self.load(window, cx);
    }

    // ------------------------------------------------------------------ navigation

    fn request_nav(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_dirty(cx) {
            self.ask_pending(nav, window, cx);
        } else {
            self.apply_nav(nav, window, cx);
        }
    }

    fn apply_nav(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        match nav {
            Nav::Page(p) => self.page = p,
            Nav::Filter(f) => {
                if let Some(f) = &f
                    && let Err(e) = table::validate_filter(f)
                {
                    self.filter_error = Some(e.to_string().into());
                    cx.notify();
                    return;
                }
                self.filter = f;
                self.page = 0;
                self.count = None;
                self.filter_error = None;
                cx.emit(TabEvent::StateChanged);
            }
            Nav::Sort(s) => {
                self.sort = s;
                self.page = 0;
                cx.emit(TabEvent::StateChanged);
            }
            Nav::PageSize(n) => {
                self.page_size = n;
                self.page = 0;
                cx.emit(TabEvent::StateChanged);
            }
            Nav::Refresh => {}
        }
        // A discarded edit set must not reappear after the reload.
        self.grid.update(cx, |g, _| g.delegate_mut().editing = None);
        self.load(window, cx);
    }

    /// "Commit / Discard / Cancel" before leaving pending changes.
    fn ask_pending(&mut self, nav: Nav, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.pending(cx);
        let view = cx.entity().downgrade();
        let title = format!(
            "{} uncommitted change{} to {}.{}",
            n,
            if n == 1 { "" } else { "s" },
            self.schema,
            self.table
        );
        window.open_dialog(cx, move |dialog, _, _| {
            let (v1, v2) = (view.clone(), view.clone());
            let (nav1, nav2) = (nav.clone(), nav.clone());
            dialog
                .title(title.clone())
                .child("Commit them first, or discard them and continue.")
                .footer(
                    DialogFooter::new()
                        .gap_2()
                        .child(DialogClose::new().child(Button::new("pending-cancel").label("Cancel").outline()))
                        .child(
                            Button::new("pending-discard")
                                .label(format!("Discard {n} change{}", if n == 1 { "" } else { "s" }))
                                .outline()
                                .danger()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let nav = nav1.clone();
                                    let _ = v1.update(cx, |this, cx| {
                                        this.discard_now(cx);
                                        this.apply_nav(nav, window, cx);
                                    });
                                }),
                        )
                        .child(
                            Button::new("pending-commit")
                                .label(format!("Commit {n}"))
                                .primary()
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let nav = nav2.clone();
                                    let _ = v2.update(cx, |this, cx| {
                                        this.after_commit = Some(nav);
                                        this.open_review(window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn cycle_sort(&mut self, data_col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .grid
            .read(cx)
            .delegate()
            .columns
            .get(data_col)
            .map(|c| c.name.clone())
        else {
            return;
        };
        let next = match &self.sort {
            Some((n, SortDir::Asc)) if *n == name => Some((name, SortDir::Desc)),
            Some((n, SortDir::Desc)) if *n == name => None,
            _ => Some((name, SortDir::Asc)),
        };
        self.request_nav(Nav::Sort(next), window, cx);
    }

    fn set_refresh(&mut self, secs: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_secs = secs;
        self._refresh_loop = secs.map(|s| {
            cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(s as u64)).await;
                    let alive = this
                        .update_in(cx, |this, window, cx| {
                            if !this.active {
                                this.reload_when_shown = true;
                                return;
                            }
                            let editing = this.grid.read(cx).delegate().editing.is_some();
                            // Skip a tick while loading, with pending edits, while editing or reviewing.
                            if this.loading.is_none() && !this.is_dirty(cx) && !editing && this.review.is_none() {
                                this.load(window, cx);
                            }
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
            })
        });
        cx.notify();
    }

    // ------------------------------------------------------------------ editing

    /// The selected cell as (row, data column).
    fn selected(&self, cx: &App) -> Option<(usize, usize)> {
        let g = self.grid.read(cx);
        let (r, c) = g.selected_cell()?;
        g.delegate().data_col(c).map(|d| (r, d))
    }

    fn begin_edit(&mut self, initial: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        // Enter reaches the grid binding after the cell editor's own; never restart an edit.
        if self.grid.read(cx).delegate().editing.is_some() {
            return;
        }
        let Some((row, col)) = self.selected(cx) else { return };
        if !self.grid.read(cx).delegate().can_edit(row, col) {
            return;
        }
        let value = initial.unwrap_or_else(|| self.grid.read(cx).delegate().shown_text(row, col).unwrap_or_default());
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } => this.confirm_edit(Move::Down, window, cx),
            InputEvent::Blur => this.confirm_edit(Move::Stay, window, cx),
            _ => {}
        });
        self._subs.push(sub);
        input.update(cx, |i, cx| {
            i.focus(window, cx);
            let end = i.value().len();
            i.set_selected_range(end..end, cx);
        });
        self.grid.update(cx, |g, cx| {
            g.delegate_mut().editing = Some(CellEditor { row, col, input });
            g.refresh(cx);
        });
        cx.notify();
    }

    fn confirm_edit(&mut self, mv: Move, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.grid.update(cx, |g, _| g.delegate_mut().editing.take()) else {
            return;
        };
        let mut text = ed.input.read(cx).value().to_string();
        if self.grid.read(cx).delegate().is_boolean(ed.col) {
            text = crate::ui::grid::normalize_bool(&text);
        }
        let rows = self.grid.read(cx).delegate().model.row_count();
        let cols = self.grid.read(cx).delegate().display_count();
        let many = self.multi_cells(cx).filter(|c| c.contains(&(ed.row, ed.col)));
        self.grid.update(cx, |g, cx| {
            match many {
                // Typed into one cell of a column selection: every selected cell takes it.
                Some(cells) => {
                    let d = g.delegate_mut();
                    let cells: Vec<_> = cells.into_iter().filter(|&(r, c)| d.can_edit(r, c)).collect();
                    d.model.set_many(&cells, CellValue::Text(text));
                    d.multi = None;
                }
                None => {
                    apply_edit(g.delegate_mut(), ed.row, ed.col, CellValue::Text(text));
                }
            }
            // Moves follow the display order, which may differ from the table's.
            let gc = g.delegate().grid_col(ed.col).unwrap_or(1);
            let (r, c) = match mv {
                Move::Down => ((ed.row + 1).min(rows.saturating_sub(1)), gc),
                Move::Up => (ed.row.saturating_sub(1), gc),
                Move::Right => (ed.row, (gc + 1).min(cols.max(1))),
                Move::Stay => (ed.row, gc),
            };
            g.set_selected_cell(r, c, cx);
            g.refresh(cx);
        });
        if mv != Move::Stay {
            self.focus(window, cx);
        }
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        let had = self.grid.update(cx, |g, cx| {
            let had = g.delegate_mut().editing.take().is_some();
            g.refresh(cx);
            had
        });
        if had {
            self.focus(window, cx);
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn on_begin_edit(&mut self, _: &BeginEdit, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_edit(None, window, cx);
    }

    fn on_confirm_right(&mut self, _: &ConfirmEditRight, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self.grid.read(cx).delegate().editing.as_ref().map(|e| (e.row, e.col));
        let Some((row, col)) = editing else {
            cx.propagate();
            return;
        };
        let d = self.grid.read(cx).delegate();
        let last_col = d.grid_col(col).is_some_and(|g| g >= d.display_count());
        let new_row = d.model.is_new(row);
        self.confirm_edit(Move::Right, window, cx);
        // Tab from the last cell of a new row adds another new row.
        if last_col && new_row {
            self.add_row(&AddRow, window, cx);
        }
    }

    fn on_confirm_up(&mut self, _: &ConfirmEditUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.read(cx).delegate().editing.is_none() {
            cx.propagate();
            return;
        }
        self.confirm_edit(Move::Up, window, cx);
    }

    fn on_confirm_down(&mut self, _: &ConfirmEditDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.read(cx).delegate().editing.is_none() {
            cx.propagate();
            return;
        }
        self.confirm_edit(Move::Down, window, cx);
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.grid.read(cx).delegate().editing.is_some() {
            return;
        }
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        if ev.keystroke.key == "escape" && (self.peek.is_some() || self.glance.is_some()) {
            self.peek = None;
            self.glance = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // Space on a foreign-key cell peeks at the referenced row instead of editing (3.3).
        if ev.keystroke.key == "space"
            && let Some((r, c)) = self.selected(cx)
            && self.grid.read(cx).delegate().columns.get(c).is_some_and(|c| c.fk)
            && self.open_peek(r, c, cx)
        {
            cx.stop_propagation();
            return;
        }
        if let Some(ch) = ev
            .keystroke
            .key_char
            .as_ref()
            .filter(|c| !c.chars().any(char::is_control))
            && self.selected(cx).is_some()
        {
            self.begin_edit(Some(ch.clone()), window, cx);
            cx.stop_propagation();
        }
    }

    fn set_null(&mut self, _: &SetNull, _: &mut Window, cx: &mut Context<Self>) {
        let Some(cells) = self.target_cells(cx) else { return };
        self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            let cells: Vec<_> = cells.into_iter().filter(|&(r, c)| d.can_edit(r, c)).collect();
            d.model.set_many(&cells, CellValue::Null);
            g.refresh(cx);
        });
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn revert_cell(&mut self, _: &RevertCell, _: &mut Window, cx: &mut Context<Self>) {
        let Some(cells) = self.target_cells(cx) else { return };
        self.grid.update(cx, |g, cx| {
            g.delegate_mut().model.revert_many(&cells);
            g.refresh(cx);
        });
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    // ------------------------------------------------------------------ column selection

    /// The column selection's cells when it holds more than one.
    fn multi_cells(&self, cx: &App) -> Option<Vec<(usize, usize)>> {
        self.grid
            .read(cx)
            .delegate()
            .multi
            .as_ref()
            .filter(|m| m.rows.len() > 1)
            .map(|m| m.cells())
    }

    /// What Set NULL, Revert, Copy and Paste act on: the column selection, else the cell.
    fn target_cells(&self, cx: &App) -> Option<Vec<(usize, usize)>> {
        self.multi_cells(cx).or_else(|| self.selected(cx).map(|c| vec![c]))
    }

    /// Moves the grid's cursor without ending the column selection.
    fn select_quietly(&mut self, row: usize, data_col: usize, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| {
            let gc = g.delegate().grid_col(data_col).unwrap_or(1);
            self.expect_select = Some((row, gc));
            g.set_selected_cell(row, gc, cx);
            g.scroll_to_row(row, cx);
            g.refresh(cx);
        });
        cx.notify();
    }

    /// Shift+click selects a range in the column, Ctrl+click adds or removes one cell, Alt+click
    /// follows a foreign key.
    fn cell_mouse(&mut self, row: usize, col: usize, m: Modifiers, cx: &mut Context<Self>) -> bool {
        if self.grid.read(cx).delegate().editing.is_some() {
            return false;
        }
        if m.alt && !m.shift && !m.control && !m.platform {
            if self.grid.read(cx).delegate().columns.get(col).is_some_and(|c| c.fk) {
                self.follow_key(row, col, cx);
                return true;
            }
            return false;
        }
        let current = self.selected(cx);
        let toggle = m.secondary(cx);
        self.grid.update(cx, |g, _| {
            let d = g.delegate_mut();
            let mut sel = match d.multi.take().filter(|s| s.col == col) {
                Some(s) => s,
                None => {
                    // Starts from the selected cell when it is in the same column.
                    let start = current.filter(|&(_, c)| c == col).map(|(r, _)| r).unwrap_or(row);
                    ColumnSelection::new(col, start)
                }
            };
            if toggle {
                sel.toggle(row);
            } else {
                sel.range_to(row);
            }
            d.multi = Some(sel);
        });
        self.select_quietly(row, col, cx);
        true
    }

    /// Shift+Up and Shift+Down grow or shrink the range in the column.
    fn extend_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some((r, c)) = self.selected(cx) else { return };
        let rows = self.grid.read(cx).delegate().model.row_count();
        if rows == 0 {
            return;
        }
        let active = self.grid.update(cx, |g, _| {
            let d = g.delegate_mut();
            let mut sel = d
                .multi
                .take()
                .filter(|s| s.col == c && s.active == r)
                .unwrap_or_else(|| ColumnSelection::new(c, r));
            let active = sel.active.saturating_add_signed(delta).min(rows - 1);
            sel.range_to(active);
            d.multi = Some(sel);
            active
        });
        self.select_quietly(active, c, cx);
    }

    /// Ctrl+V: the clipboard goes into the selected cells, one line per cell.
    fn paste_cells(&mut self, _: &PasteCells, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        let Some((r0, c0)) = self.selected(cx) else { return };
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(&text);
        let lines: Vec<&str> = if text.is_empty() {
            vec![""]
        } else {
            text.lines().collect()
        };
        let many = self.multi_cells(cx);
        let rows = self.grid.read(cx).delegate().model.row_count();
        self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            // Lines fill the selection in order and repeat when there are fewer, as a spreadsheet
            // does; without a selection they go down from the selected cell.
            let pairs: Vec<((usize, usize), &str)> = match &many {
                Some(cells) => cells.iter().copied().zip(lines.iter().copied().cycle()).collect(),
                None => (r0..rows).map(|r| (r, c0)).zip(lines.iter().copied()).collect(),
            };
            let edits: Vec<_> = pairs
                .into_iter()
                .filter(|&((r, c), _)| d.can_edit(r, c))
                .map(|((r, c), v)| ((r, c), crate::ui::grid::pasted_value(d, c, v)))
                .collect();
            // One undo step even when the lines differ.
            d.model.set_each(edits);
            g.refresh(cx);
        });
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn add_row(&mut self, _: &AddRow, window: &mut Window, cx: &mut Context<Self>) {
        if !self.grid.read(cx).delegate().editable {
            return;
        }
        let (row, first) = self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            let row = d.model.add_row();
            // The first column the user must fill, in display order: writable and without a
            // default.
            let order = d.display_order().to_vec();
            let first = order
                .iter()
                .position(|&i| d.columns[i].writable && !d.columns[i].has_default)
                .or_else(|| order.iter().position(|&i| d.columns[i].writable))
                .unwrap_or(0);
            g.refresh(cx);
            g.set_selected_cell(row, first + 1, cx);
            g.scroll_to_row(row, cx);
            (row, first)
        });
        let _ = (row, first);
        self.focus(window, cx);
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn delete_row(&mut self, _: &DeleteRow, _: &mut Window, cx: &mut Context<Self>) {
        let Some((r, _)) = self.selected(cx) else { return };
        if !self.grid.read(cx).delegate().editable {
            return;
        }
        self.grid.update(cx, |g, cx| {
            g.delegate_mut().editing = None;
            g.delegate_mut().model.toggle_delete(r);
            g.refresh(cx);
        });
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn discard_now(&mut self, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            let rows = d.model.loaded().to_vec();
            let n = d.columns.len();
            d.editing = None;
            d.model.reset(rows, n);
            g.refresh(cx);
        });
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn discard(&mut self, _: &DiscardChanges, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.pending(cx);
        if n == 0 {
            return;
        }
        let view = cx.entity().downgrade();
        let title = format!(
            "Discard {n} change{} to {}.{}?",
            if n == 1 { "" } else { "s" },
            self.schema,
            self.table
        );
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert
                .title(title.clone())
                .description("The edits are dropped; nothing is sent to the database.")
                .confirm()
                .ok_text(format!("Discard {n} change{}", if n == 1 { "" } else { "s" }))
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.discard_now(cx));
                    true
                })
        });
    }

    fn open_cell_menu(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let position = window.mouse_position();
        let focus = self.grid_focus(cx);
        let grid = self.grid.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, cx| {
            cell_menu(grid.read(cx).delegate(), row, menu).action_context(focus)
        });
        let handle = menu.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        let sub = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.cell_menu = None;
            this.focus(window, cx);
            cx.notify();
        });
        self.cell_menu = Some((menu, position, sub));
        cx.notify();
    }

    /// Right click on a column header (4.5): sort, filter, hide, pin, copy the name.
    fn open_header_menu(&mut self, data_col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.grid.read(cx).delegate();
        let Some(col) = d.columns.get(data_col).cloned() else {
            return;
        };
        let hidden = d.hidden.len();
        let has_keys = d.columns.iter().any(|c| c.key);
        let pinned = d.pin_keys;
        let sorted = self.sort.as_ref().map(|(n, dir)| (n == &col.name, *dir));
        let position = window.mouse_position();
        let view = cx.entity().downgrade();
        let name = col.name.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            let item = |label: String, f: ViewFn| {
                let view = view.clone();
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let f = f.clone();
                    let _ = view.update(cx, |this, cx| f(this, window, cx));
                })
            };
            let n1 = name.clone();
            let n2 = name.clone();
            let n3 = name.clone();
            let n4 = name.clone();
            let mut menu = menu
                .item(
                    item(
                        "Sort ascending".into(),
                        Rc::new(move |this, w, cx| {
                            this.request_nav(Nav::Sort(Some((n1.clone(), SortDir::Asc))), w, cx)
                        }),
                    )
                    .checked(sorted == Some((true, SortDir::Asc))),
                )
                .item(
                    item(
                        "Sort descending".into(),
                        Rc::new(move |this, w, cx| {
                            this.request_nav(Nav::Sort(Some((n2.clone(), SortDir::Desc))), w, cx)
                        }),
                    )
                    .checked(sorted == Some((true, SortDir::Desc))),
                );
            if sorted.is_some_and(|(this_col, _)| this_col) {
                menu = menu.item(item(
                    "Clear sort".into(),
                    Rc::new(|this, w, cx| this.request_nav(Nav::Sort(None), w, cx)),
                ));
            }
            let n6 = name.clone();
            menu = menu
                .separator()
                .item(item(
                    "Filter by…".into(),
                    Rc::new(move |this, w, cx| this.start_filter_on(&n3, w, cx)),
                ))
                .item(item(
                    "Glance".into(),
                    Rc::new(move |this, w, cx| this.open_glance(&n6, w, cx)),
                ))
                .separator()
                .item(item(
                    "Hide column".into(),
                    Rc::new(move |this, _, cx| this.set_hidden(Some(n4.clone()), cx)),
                ));
            if hidden > 0 {
                menu = menu.item(item(
                    format!("Show all columns ({hidden} hidden)"),
                    Rc::new(|this, _, cx| this.set_hidden(None, cx)),
                ));
            }
            if has_keys {
                menu = menu.item(item(
                    if pinned { "Unpin key columns" } else { "Pin key columns" }.into(),
                    Rc::new(move |this, _, cx| this.set_pin_keys(!pinned, cx)),
                ));
            }
            let n5 = name.clone();
            menu.separator().item(item(
                "Copy name".into(),
                Rc::new(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(n5.clone()))),
            ))
        });
        let handle = menu.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        let sub = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.cell_menu = None;
            // Keyboard focus returns to the grid, so shortcuts keep working.
            if this.glance.is_none() && !this.filter_input.read(cx).focus_handle(cx).is_focused(window) {
                this.focus(window, cx);
            }
            cx.notify();
        });
        self.cell_menu = Some((menu, position, sub));
        cx.notify();
    }

    /// Puts `"column" = ` at the end of the filter and the caret after it.
    fn start_filter_on(&mut self, column: &str, window: &mut Window, cx: &mut Context<Self>) {
        let cond = format!("{} = ", baselynx_core::sql::quote_ident(column));
        let current = self.filter_input.read(cx).value().trim().to_string();
        let text = if current.is_empty() {
            cond
        } else {
            format!("{current} AND {cond}")
        };
        self.filter_input.update(cx, |i, cx| {
            i.set_value(text.clone(), window, cx);
            i.focus(window, cx);
            let end = text.len();
            i.set_selected_range(end..end, cx);
        });
    }

    /// Hides one column, or shows them all again with `None`.
    fn set_hidden(&mut self, column: Option<String>, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            match column {
                Some(c) => {
                    d.hidden.insert(c);
                }
                None => d.hidden.clear(),
            }
            d.editing = None;
            d.rebuild_order();
            g.refresh(cx);
        });
        cx.notify();
    }

    fn set_pin_keys(&mut self, pin: bool, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            d.pin_keys = pin;
            d.editing = None;
            d.rebuild_order();
            g.refresh(cx);
        });
        cx.notify();
    }

    /// Ctrl+Z / Ctrl+Y over the pending edits of this page (4.7).
    fn step_edits(&mut self, back: bool, cx: &mut Context<Self>) {
        let moved = self.grid.update(cx, |g, cx| {
            let d = g.delegate_mut();
            if d.editing.is_some() {
                return false;
            }
            let moved = if back { d.model.undo() } else { d.model.redo() };
            g.refresh(cx);
            moved
        });
        if moved {
            cx.emit(TabEvent::Updated);
            cx.notify();
        }
    }

    fn copy_row(&mut self, format: RowFormat, cx: &mut Context<Self>) {
        let Some((row, _)) = self.selected(cx) else { return };
        let d = self.grid.read(cx).delegate();
        let cells = d.row_cells(row);
        let text = match format {
            RowFormat::Insert => crate::row_text::insert(&self.schema, &self.table, &cells),
            RowFormat::Json => crate::row_text::json(&cells),
            RowFormat::Tsv => crate::row_text::tsv(&cells),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn copy_cell(&mut self, _: &CopyCell, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(cells) = self.target_cells(cx) {
            let d = self.grid.read(cx).delegate();
            // A column selection copies one value per line, as a spreadsheet pastes it.
            let text = cells
                .iter()
                .map(|&(r, c)| d.shown_text(r, c).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn quick_filter(&mut self, exclude: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((r, c)) = self.selected(cx) else { return };
        let d = self.grid.read(cx).delegate();
        if d.model.is_new(r) {
            return;
        }
        let Some(name) = d.columns.get(c).map(|c| c.name.clone()) else {
            return;
        };
        let value = CellValue::from_option(d.model.loaded_value(r, c).map(String::from));
        let cond = table::quick_filter(&name, &value, exclude);
        let filter = table::append_filter(self.filter.as_deref(), &cond);
        self.filter_input
            .update(cx, |i, cx| i.set_value(filter.clone(), window, cx));
        self.request_nav(Nav::Filter(Some(filter)), window, cx);
    }

    // ------------------------------------------------------------------ commit

    fn open_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rel) = self.rel.clone() else { return };
        let d = self.grid.read(cx).delegate();
        if !d.model.is_dirty() {
            return;
        }
        let changes = match d.model.change_set(&rel, &d.column_names()) {
            Ok(c) => c,
            Err(e) => {
                window.push_notification(Notification::error(e).autohide(false), cx);
                return;
            }
        };
        let statements = match edit::preview(&rel, &changes) {
            Ok(s) => s,
            Err(e) => {
                window.push_notification(Notification::error(e.to_string()).autohide(false), cx);
                return;
            }
        };
        let red = self.conn.color == ConnectionColor::Red;
        let review = cx.new(|_| {
            CommitReview::new(
                format!("Commit to {} · {}.{}", self.conn.name, self.schema, self.table),
                changes.summary(),
                statements,
                self.conn.name.clone(),
                red,
            )
        });
        self.review = Some(review.clone());
        let view = cx.entity().downgrade();
        let on_commit = {
            let view = view.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| this.commit_now(window, cx));
            })
        };
        CommitReview::open(review, on_commit, view, window, cx);
    }

    pub fn review_closed(&mut self, cx: &mut Context<Self>) {
        self.review = None;
        self.after_commit = None;
        cx.notify();
    }

    fn commit_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(rel), Some(session), Some(review)) = (self.rel.clone(), self.session.clone(), self.review.clone())
        else {
            return;
        };
        let d = self.grid.read(cx).delegate();
        let Ok(changes) = d.model.change_set(&rel, &d.column_names()) else {
            return;
        };
        review.update(cx, |r, cx| r.set_busy(cx));
        let fut = edit::commit(&session, &rel, changes);
        cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(report) => {
                    this.review = None;
                    window.close_dialog(cx);
                    let mut parts = Vec::new();
                    for (n, one, many) in [
                        (report.updated, "update", "updates"),
                        (report.deleted, "delete", "deletes"),
                        (report.inserted, "insert", "inserts"),
                    ] {
                        if n > 0 {
                            parts.push(format!("{n} {}", if n == 1 { one } else { many }));
                        }
                    }
                    window.push_notification(Notification::success(format!("Committed {}", parts.join(", "))), cx);
                    this.discard_now(cx);
                    this.after_commit_tint = true;
                    let nav = this.after_commit.take().unwrap_or(Nav::Refresh);
                    this.apply_nav(nav, window, cx);
                    this.focus(window, cx);
                }
                Err(e) => {
                    let failing = match &e {
                        CoreError::Commit { index, .. } => Some(index.0),
                        _ => None,
                    };
                    let message = match &e {
                        CoreError::Commit { source, .. } => source.to_string(),
                        other => other.to_string(),
                    };
                    review.update(cx, |r, cx| r.set_error(message, failing, cx));
                }
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------ export, count

    fn export(&mut self, _: &ExportData, window: &mut Window, cx: &mut Context<Self>) {
        if self.export.is_some() || self.rel.is_none() {
            return;
        }
        let mut notes = Vec::new();
        if self.is_dirty(cx) {
            notes.push(SharedString::from(
                "Uncommitted changes are not exported; the file holds what is in the database.",
            ));
        }
        let view = cx.entity().downgrade();
        let grid = self.grid.clone();
        let target = self.target.clone();
        let rel = self.rel.clone();
        let req = self.page_request();
        let name = self.table.clone();
        export_dialog::open_options(true, notes, window, cx, move |choice, window, cx| {
            let d = grid.read(cx).delegate();
            let columns: Vec<exec::ColumnMeta> = d
                .columns
                .iter()
                .map(|c| {
                    let info = rel.as_ref().and_then(|r| r.column(&c.name));
                    exec::ColumnMeta {
                        name: c.name.clone(),
                        type_oid: info.map(|i| i.type_oid),
                        type_name: info.map(|i| i.type_name.clone()),
                    }
                })
                .collect();
            let source = match choice.scope {
                ExportScope::CurrentPage => ExportSource::Loaded {
                    columns,
                    rows: d.model.loaded().to_vec(),
                },
                ExportScope::AllRows => {
                    let (Some(target), Some(rel)) = (target.clone(), rel.clone()) else {
                        return;
                    };
                    let Ok(sql) = table::build_select(&req, rel.row_key.as_deref(), false) else {
                        return;
                    };
                    ExportSource::Query {
                        connection: Box::new(target.config),
                        password: target.password,
                        sql,
                        columns,
                    }
                }
            };
            export_dialog::run(
                view.clone(),
                source,
                choice.format,
                name.clone(),
                window,
                cx,
                |v: &mut TableView, job| v.export = job,
                |v: &mut TableView, rows| {
                    if let Some(j) = v.export.as_mut() {
                        j.rows = rows;
                    }
                },
            );
        });
    }

    fn count_rows(&mut self, _: &CountRows, _: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else { return };
        let fut = table::count_rows(&session, &self.schema, &self.table, self.filter.clone());
        let filter = self.filter.clone();
        self.counting = Some(cx.spawn(async move |this, cx| {
            let result = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.counting = None;
                if this.filter == filter {
                    match result {
                        Ok(n) => this.count = Some(n),
                        Err(e) => this.filter_error = Some(e.to_string().into()),
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    // ------------------------------------------------------------------ rendering

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.grid.read(cx).delegate();
        let editable = d.editable;
        let pending = d.model.pending();
        let red = self.conn.color == ConnectionColor::Red;
        let refresh_label = match self.refresh_secs {
            None => "Off".to_string(),
            Some(s) => format!("{s} s"),
        };
        // Labels drop before buttons do when the tab is narrow; nothing wraps.
        let compact = self.toolbar_w < tokens::ui(780., cx);
        let view = cx.entity().downgrade();
        let current_refresh = self.refresh_secs;
        let weak = cx.entity().downgrade();
        h_flex()
            .relative()
            .h_9()
            .px_2()
            .gap_1()
            .flex_shrink_0()
            .overflow_hidden()
            .bg(tokens::raised(cx))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = weak.update(cx, |v, cx| {
                            if (v.toolbar_w - bounds.size.width).abs() > px(1.) {
                                v.toolbar_w = bounds.size.width;
                                cx.notify();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(self.render_switcher(cx))
            .child(
                div()
                    .key_context(FILTER)
                    .on_action(cx.listener(Self::filter_enter))
                    .on_action(cx.listener(Self::filter_tab))
                    .on_action(cx.listener(Self::filter_up))
                    .on_action(cx.listener(Self::filter_down))
                    .child({
                        let cell = self.assist.bounds.clone();
                        canvas(move |b, _, _| cell.set(Some(b)), |_, _, _, _| {})
                            .absolute()
                            .size_full()
                    })
                    .relative()
                    .on_action(cx.listener(|this, _: &ClearFilter, window, cx| {
                        if this.filter_escape_suggestions(cx) {
                            return;
                        }
                        if this.filter.is_none() && this.filter_input.read(cx).value().is_empty() {
                            this.focus(window, cx);
                            return;
                        }
                        this.filter_input.update(cx, |i, cx| i.set_value("", window, cx));
                        this.request_nav(Nav::Filter(None), window, cx);
                    }))
                    .flex_1()
                    .min_w_40()
                    .child(
                        Input::new(&self.filter_input)
                            .small()
                            .font_family(cx.theme().mono_font_family.clone())
                            .cleanable(true),
                    ),
            )
            .child(
                Button::new("refresh")
                    .icon(IconName::RefreshCw)
                    .small()
                    .ghost()
                    .tooltip_with_action("Refresh", &Refresh, Some(TABLE))
                    .on_click(cx.listener(|this, _, window, cx| this.request_nav(Nav::Refresh, window, cx))),
            )
            .child(
                Button::new("auto-refresh")
                    .icon(gpui_kit::assets::IconName::Clock)
                    .label(refresh_label)
                    .small()
                    .ghost()
                    .when(self.refresh_secs.is_some(), |b| b.selected(true))
                    .tooltip("Auto-refresh")
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu;
                        for choice in REFRESH_CHOICES {
                            let view = view.clone();
                            let label = match choice {
                                None => "Off".to_string(),
                                Some(s) => format!("{s} s"),
                            };
                            menu = menu.item(PopupMenuItem::new(label).checked(current_refresh == choice).on_click(
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.set_refresh(choice, window, cx);
                                        cx.emit(TabEvent::StateChanged);
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(div().w_2())
            .child(
                Button::new("add-row")
                    .icon(IconName::Plus)
                    .when(!compact, |b| b.label("Add row"))
                    .small()
                    .ghost()
                    .disabled(!editable)
                    .tooltip_with_action("Add row", &AddRow, Some(TABLE))
                    .on_click(cx.listener(|this, _, window, cx| this.add_row(&AddRow, window, cx))),
            )
            .child(
                Button::new("delete-row")
                    .icon(IconName::Minus)
                    .when(!compact, |b| b.label("Delete row"))
                    .small()
                    .ghost()
                    .disabled(!editable)
                    .tooltip_with_action("Delete row", &DeleteRow, Some(TABLE))
                    .on_click(cx.listener(|this, _, window, cx| this.delete_row(&DeleteRow, window, cx))),
            )
            .child(
                Button::new("discard")
                    .label(if pending > 0 {
                        format!("Discard {pending}")
                    } else {
                        "Discard".into()
                    })
                    .small()
                    .ghost()
                    .disabled(pending == 0)
                    .on_click(cx.listener(|this, _, window, cx| this.discard(&DiscardChanges, window, cx))),
            )
            .child(
                Button::new("commit")
                    .label(if pending > 0 {
                        format!("Commit {pending}")
                    } else {
                        "Commit".into()
                    })
                    .small()
                    .when(pending > 0 && !red, |b| b.primary())
                    .when(pending > 0 && red, |b| b.danger())
                    .when(pending == 0, |b| b.ghost())
                    .disabled(pending == 0 || !editable)
                    .tooltip_with_action("Review and commit", &CommitChanges, Some(TABLE))
                    .on_click(cx.listener(|this, _, window, cx| this.open_review(window, cx))),
            )
            .child(
                Button::new("export")
                    .icon(gpui_kit::assets::IconName::Download)
                    .when(!compact, |b| b.label("Export"))
                    .small()
                    .ghost()
                    .tooltip("Export to CSV or Excel")
                    .disabled(self.export.is_some() || !self.loaded_once)
                    .on_click(cx.listener(|this, _, window, cx| this.export(&ExportData, window, cx))),
            )
    }

    fn render_pager(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let n = self.grid.read(cx).delegate().model.loaded_len() as u64;
        let first = self.page * self.page_size as u64;
        let range = if !self.loaded_once {
            String::new()
        } else if n == 0 {
            "No rows".to_string()
        } else {
            format!("rows {}–{}", format::count(first + 1), format::count(first + n))
        };
        let total = self.count.map(|c| format!(" of {}", format::count(c)));
        let view = cx.entity().downgrade();
        let size = self.page_size;
        h_flex()
            .h_9()
            .px_2()
            .gap_1()
            .flex_shrink_0()
            .text_sm()
            .bg(tokens::raised(cx))
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("prev")
                    .icon(IconName::ChevronLeft)
                    .small()
                    .ghost()
                    .disabled(self.page == 0)
                    .tooltip_with_action("Previous page", &PrevPage, Some(TABLE))
                    .on_click(cx.listener(|this, _, window, cx| this.prev_page(&PrevPage, window, cx))),
            )
            .child(
                Button::new("next")
                    .icon(IconName::ChevronRight)
                    .small()
                    .ghost()
                    .disabled(!self.has_next)
                    .tooltip_with_action("Next page", &NextPage, Some(TABLE))
                    .on_click(cx.listener(|this, _, window, cx| this.next_page(&NextPage, window, cx))),
            )
            .child(div().px_1().child(range))
            .when_some(self.multi_cells(cx).map(|c| c.len()), |d, n| {
                d.child(
                    div()
                        .px_1()
                        .text_color(cx.theme().primary)
                        .child(format!("{} cells selected", format::count(n as u64))),
                )
            })
            .when_some(self.diff_summary(cx), |d, (text, first)| {
                d.child(
                    div()
                        .id("diff-summary")
                        .px_1()
                        .rounded(cx.theme().radius)
                        .text_color(cx.theme().primary)
                        .cursor_pointer()
                        .hover(|d| d.bg(cx.theme().muted))
                        .child(text)
                        .tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(
                                "Changed since the previous load; click for the first change",
                            )
                            .build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some((r, c)) = first {
                                this.grid.update(cx, |g, cx| {
                                    let gc = g.delegate().grid_col(c).unwrap_or(1);
                                    g.set_selected_cell(r, gc, cx);
                                    g.scroll_to_row(r, cx);
                                    g.scroll_to_col(gc, cx);
                                });
                            }
                        })),
                )
            })
            .when_some(total, |this, t| {
                this.child(div().text_color(tokens::secondary_text(cx)).child(t))
            })
            .child(
                Button::new("count")
                    .label(if self.counting.is_some() {
                        "Counting…"
                    } else {
                        "Count"
                    })
                    .small()
                    .ghost()
                    .disabled(self.counting.is_some() || self.session.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.count_rows(&CountRows, window, cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new("page-size")
                    .label(format!("{} per page", format::count(size as u64)))
                    .small()
                    .ghost()
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu;
                        for s in PAGE_SIZES {
                            let view = view.clone();
                            menu = menu.item(PopupMenuItem::new(format::count(s as u64)).checked(s == size).on_click(
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| this.request_nav(Nav::PageSize(s), window, cx));
                                },
                            ));
                        }
                        menu
                    }),
            )
    }

    /// The connection name as a switcher: the same table on another connection (3.5).
    fn render_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let alt = self.alternatives.clone();
        let view = cx.entity().downgrade();
        let own = self.conn.clone();
        Button::new("conn-switch")
            .small()
            .ghost()
            .dropdown_caret(true)
            .child(
                h_flex()
                    .gap_1()
                    .when_some(tokens::connection(self.conn.color, cx), |d, c| {
                        d.child(div().size_2().rounded_full().bg(c))
                    })
                    .when(self.read_only(), |d| {
                        d.child(Icon::new(gpui_kit::assets::IconName::Lock).xsmall())
                    })
                    .child(
                        div()
                            .max_w(tokens::ui(160., cx))
                            .truncate()
                            .child(self.conn.name.clone()),
                    ),
            )
            .tooltip("The same table on another connection · Shift+click replaces this tab")
            .dropdown_menu(move |menu, _, cx| {
                let others = alt.as_ref().map(|f| f(cx)).unwrap_or_default();
                let mut menu = menu.item(PopupMenuItem::new(own.name.clone()).checked(true).disabled(true));
                if others.is_empty() {
                    return menu.separator().label("No other connected database has this table");
                }
                menu = menu.separator();
                for (label, _) in others {
                    let view = view.clone();
                    let id = label.id;
                    menu = menu.item(PopupMenuItem::new(label.name.clone()).on_click(move |ev, _, cx| {
                        let replace = ev.modifiers().shift;
                        let _ = view.update(cx, |_, cx| cx.emit(TabEvent::OpenElsewhere { conn: id, replace }));
                    }));
                }
                menu
            })
    }

    /// "3 cells changed · 2 new · 1 row gone · 14:02:11" and the first change.
    fn diff_summary(&self, cx: &App) -> Option<(String, Option<(usize, usize)>)> {
        let d = self.grid.read(cx).delegate().diff.as_ref()?;
        let mut parts = Vec::new();
        if !d.changed.is_empty() {
            let n = d.changed.len();
            parts.push(format!(
                "{} cell{} changed",
                format::count(n as u64),
                if n == 1 { "" } else { "s" }
            ));
        }
        if !d.new_rows.is_empty() {
            parts.push(format!("{} new", format::count(d.new_rows.len() as u64)));
        }
        if d.gone > 0 {
            parts.push(format!(
                "{} row{} gone",
                format::count(d.gone as u64),
                if d.gone == 1 { "" } else { "s" }
            ));
        }
        if let Some(at) = &self.diff_at {
            parts.push(at.to_string());
        }
        Some((parts.join(" · "), d.first()))
    }

    fn prev_page(&mut self, _: &PrevPage, window: &mut Window, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.request_nav(Nav::Page(self.page - 1), window, cx);
        }
    }

    fn next_page(&mut self, _: &NextPage, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_next {
            self.request_nav(Nav::Page(self.page + 1), window, cx);
        }
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.target.is_none() {
            let (text, button): (SharedString, Option<&'static str>) = match &self.phase {
                ConnPhase::Connecting => ("Connecting…".into(), None),
                ConnPhase::Disconnected => (format!("Not connected to {}", self.conn.name).into(), Some("Connect")),
                ConnPhase::Failed(e) => (e.clone(), Some("Retry")),
            };
            let failed = matches!(self.phase, ConnPhase::Failed(_));
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .child(
                    div()
                        .max_w(px(640.))
                        .text_center()
                        .text_color(if failed {
                            tokens::error_text(cx)
                        } else {
                            tokens::secondary_text(cx)
                        })
                        .child(text),
                )
                .when_some(button, |d, label| {
                    d.child(
                        Button::new("tab-connect")
                            .label(label)
                            .small()
                            .outline()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(TabEvent::Connect))),
                    )
                })
                .into_any_element();
        }
        if let Some(err) = &self.load_error {
            let db = err.db().cloned();
            return v_flex()
                .size_full()
                .p_4()
                .gap_2()
                .child(
                    div()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(tokens::error_text(cx))
                        .child(
                            db.as_ref()
                                .map(|d| d.message.clone())
                                .unwrap_or_else(|| err.to_string()),
                        ),
                )
                .when_some(db.as_ref().map(|d| d.code.clone()), |this, code| {
                    this.child(div().text_sm().text_color(tokens::secondary_text(cx)).child(code))
                })
                .when_some(db.and_then(|d| d.hint), |this, hint| {
                    this.child(div().text_sm().child(format!("Hint: {hint}")))
                })
                .child(
                    h_flex().child(
                        Button::new("retry")
                            .label("Retry")
                            .small()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.load_error = None;
                                this.load(window, cx);
                            })),
                    ),
                )
                .into_any_element();
        }
        let d = self.grid.read(cx).delegate();
        let empty = self.loaded_once && d.model.row_count() == 0;
        let grid = div()
            .key_context(GRID)
            .on_action(cx.listener(Self::on_begin_edit))
            .on_action(cx.listener(Self::copy_cell))
            .on_action(cx.listener(|this, _: &GridRowStart, _, cx| this.jump(Jump::RowStart, cx)))
            .on_action(cx.listener(|this, _: &GridRowEnd, _, cx| this.jump(Jump::RowEnd, cx)))
            .on_action(cx.listener(|this, _: &GridTop, _, cx| this.jump(Jump::Top, cx)))
            .on_action(cx.listener(|this, _: &GridBottom, _, cx| this.jump(Jump::Bottom, cx)))
            .on_action(cx.listener(Self::on_follow_key))
            .on_action(cx.listener(|this, _: &UndoEdit, _, cx| this.step_edits(true, cx)))
            .on_action(cx.listener(|this, _: &RedoEdit, _, cx| this.step_edits(false, cx)))
            .on_action(cx.listener(|this, _: &CopyRowInsert, _, cx| this.copy_row(RowFormat::Insert, cx)))
            .on_action(cx.listener(|this, _: &CopyRowJson, _, cx| this.copy_row(RowFormat::Json, cx)))
            .on_action(cx.listener(|this, _: &CopyRowTsv, _, cx| this.copy_row(RowFormat::Tsv, cx)))
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .on_action(cx.listener(|this, _: &ExtendUp, _, cx| this.extend_by(-1, cx)))
            .on_action(cx.listener(|this, _: &ExtendDown, _, cx| this.extend_by(1, cx)))
            .on_action(cx.listener(Self::paste_cells))
            .child(crate::ui::grid::data_table(&self.grid, cx));
        let mut body = div().relative().size_full().child(grid);
        if empty {
            let filtered = self.filter.is_some();
            let editable = d.editable;
            body = body.child(
                v_flex()
                    .absolute()
                    .top_10()
                    .left_0()
                    .right_0()
                    .items_center()
                    .gap_2()
                    .text_color(tokens::secondary_text(cx))
                    .child(if filtered {
                        "No rows match the filter"
                    } else {
                        "This table is empty"
                    })
                    .when(filtered, |this| {
                        this.child(
                            Button::new("clear-filter")
                                .label("Clear filter")
                                .small()
                                .outline()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.filter_input.update(cx, |i, cx| i.set_value("", window, cx));
                                    this.request_nav(Nav::Filter(None), window, cx);
                                })),
                        )
                    })
                    .when(!filtered && editable, |this| {
                        this.child(
                            Button::new("empty-add")
                                .label("Add row")
                                .small()
                                .outline()
                                .on_click(cx.listener(|this, _, window, cx| this.add_row(&AddRow, window, cx))),
                        )
                    }),
            );
        }
        if let Some(card) = self.render_peek(cx) {
            body = body.child(card);
        }
        // The record panel replaces the preview strip of the first version.
        let Some(panel) = self.record.clone() else {
            return body.into_any_element();
        };
        let width = self.record_width;
        h_resizable("record-split")
            .on_resize(cx.listener(
                |_, state: &Entity<gpui_kit::component::resizable::ResizableState>, _, cx| {
                    if let Some(w) = state.read(cx).sizes().get(1) {
                        cx.emit(TabEvent::RecordWidth(f32::from(*w)));
                    }
                },
            ))
            .child(resizable_panel().child(body))
            .child(
                resizable_panel()
                    .size(px(width))
                    .size_range(px(240.)..px(900.))
                    .child(panel),
            )
            .into_any_element()
    }
}

impl Focusable for TableView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TableView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_record(cx);
        let read_only = if self.read_only() {
            Some("Read-only connection")
        } else {
            self.rel.as_ref().and_then(|r| r.read_only_reason())
        };
        let filter_error = self.filter_error.clone();
        let lost = self.connection_lost;
        v_flex()
            .key_context(TABLE)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Refresh, window, cx| this.request_nav(Nav::Refresh, window, cx)))
            .on_action(cx.listener(Self::add_row))
            .on_action(cx.listener(Self::delete_row))
            .on_action(cx.listener(|this, _: &CommitChanges, window, cx| this.open_review(window, cx)))
            .on_action(cx.listener(Self::discard))
            .on_action(cx.listener(Self::prev_page))
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(|this, _: &FocusFilter, window, cx| {
                this.filter_input.update(cx, |i, cx| i.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &FilterByValue, window, cx| this.quick_filter(false, window, cx)))
            .on_action(cx.listener(|this, _: &ExcludeValue, window, cx| this.quick_filter(true, window, cx)))
            .on_action(cx.listener(Self::set_null))
            .on_action(cx.listener(Self::revert_cell))
            .on_action(cx.listener(Self::export))
            .on_action(cx.listener(Self::count_rows))
            .on_action(cx.listener(Self::on_confirm_right))
            .on_action(cx.listener(Self::on_confirm_up))
            .on_action(cx.listener(Self::on_confirm_down))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(cx.listener(Self::toggle_record))
            .size_full()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .h(px(2.))
                    .w_full()
                    .flex_shrink_0()
                    .when(self.show_progress, |d| d.bg(cx.theme().primary)),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_body(cx))
                    .when_some(filter_error, |this, err| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .bg(cx.theme().background)
                                .child(
                                    div()
                                        .px_3()
                                        .py_1()
                                        .bg(tokens::removed_fill(cx))
                                        .border_b_1()
                                        .border_color(tokens::removed(cx).opacity(0.4))
                                        .text_sm()
                                        .font_family(cx.theme().mono_font_family.clone())
                                        .text_color(tokens::error_text(cx))
                                        .child(err),
                                ),
                        )
                    })
                    .when(lost, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .bg(cx.theme().background)
                                .child(
                                    h_flex()
                                        .px_3()
                                        .py_1()
                                        .gap_2()
                                        .bg(tokens::removed_fill(cx))
                                        .border_b_1()
                                        .border_color(tokens::removed(cx).opacity(0.4))
                                        .text_sm()
                                        .child("Connection lost")
                                        .child(
                                            Button::new("reconnect").label("Reconnect").xsmall().outline().on_click(
                                                cx.listener(|this, _, window, cx| this.reconnect(window, cx)),
                                            ),
                                        ),
                                ),
                        )
                    })
                    .when_some(read_only.filter(|_| self.loaded_once), |this, reason| {
                        this.child(
                            div()
                                .absolute()
                                .bottom_2()
                                .right_3()
                                .px_2()
                                .py_1()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().popover)
                                .border_1()
                                .border_color(cx.theme().border)
                                .text_xs()
                                .text_color(tokens::secondary_text(cx))
                                .child(reason),
                        )
                    }),
            )
            .child(self.render_pager(cx))
            .children(self.render_filter_suggestions(cx))
            .children(self.render_glance(cx))
            .when_some(self.cell_menu.as_ref(), |this, (menu, position, _)| {
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
