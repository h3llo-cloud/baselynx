//! Query console.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use std::cell::RefCell;
use std::rc::Rc;

use baselynx_core::exec::{self, ExecEvent};
use baselynx_core::export::ExportSource;
use baselynx_core::model::{ConnectionColor, TabState};
use baselynx_core::session::{self, Session};
use baselynx_core::sql;
use baselynx_core::sql::classify;
use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{Editor, EditorState, InputEvent, Rope, RopeExt as _};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::actions::*;
use crate::app_state::{ConnLabel, ConnTarget, ExportJob, TabEvent, TabStatus};
use crate::format;
use crate::ui::export_dialog;
use crate::ui::grid::{self as gridmod, GridDelegate, GridMode, Jump};
use crate::ui::record::{RecordField, RecordPanel};
use crate::ui::sql_complete::{Source, SqlCompletion};
use crate::ui::tokens;
use crate::ui::workspace::MenuState;

pub const ROW_CAP: usize = 10_000;
/// Results kept from one run, one per statement that returned rows.
const MAX_RESULTS: usize = 20;

/// A statement to run and where it starts in the editor, to place errors.
type Stmt = (String, usize);

/// The result of one statement.
struct ResultSet {
    title: SharedString,
    grid: Entity<TableState<GridDelegate>>,
    rows: u64,
    truncated: bool,
    _sub: Subscription,
}

#[derive(Clone)]
struct Message {
    text: SharedString,
    error: bool,
}

/// How a run treats writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunMode {
    Normal,
    /// The missing-WHERE warning was answered with "Run anyway".
    Confirmed,
    /// "Run outside a transaction": autocommit for this run.
    Outside,
}

/// The transaction baselynx opened for writes.
#[derive(Clone, Debug, Default)]
struct TxState {
    statements: usize,
    rows: u64,
    verb: &'static str,
}

impl TxState {
    /// "1 204 rows updated" or "3 statements · 1 310 rows changed".
    fn summary(&self) -> String {
        if self.statements <= 1 {
            let verb = if self.verb.is_empty() { "changed" } else { self.verb };
            format!("{} {verb}", format::rows(self.rows))
        } else {
            format!("{} statements · {} changed", self.statements, format::rows(self.rows))
        }
    }
}

struct WhereWarning {
    text: SharedString,
    stmts: Vec<Stmt>,
}

/// "1.2 million", "56 thousand", "340 rows".
fn about(n: u64) -> String {
    match n {
        0..=999 => format::rows(n),
        1_000..=999_999 => format!("{} thousand", (n as f64 / 1e3).round()),
        1_000_000..=999_999_999 => format!("{:.1} million", n as f64 / 1e6),
        _ => format!("{:.1} billion", n as f64 / 1e9),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Result,
    Messages,
}

pub struct ConsoleView {
    pub id: Uuid,
    pub title: String,
    pub conn: ConnLabel,
    target: Option<ConnTarget>,
    session: Option<Arc<Session>>,
    editor: Entity<EditorState>,
    /// The result shown: one of `results`, or an empty grid before the first run.
    grid: Entity<TableState<GridDelegate>>,
    results: Vec<ResultSet>,
    current: usize,
    completion: Rc<RefCell<Source>>,
    messages: Vec<Message>,
    pane: Pane,
    running: Option<Task<()>>,
    run_started: Option<Instant>,
    show_spinner: bool,
    stop_requested: bool,
    has_result: bool,
    result_rows: Option<u64>,
    truncated: bool,
    last_elapsed: Option<Duration>,
    last_error: Option<SharedString>,
    export: Option<ExportJob>,
    /// The selected result row, top to bottom.
    record: Option<Entity<RecordPanel>>,
    /// `None`: guarded on red and amber connections; the toolbar switch overrides it.
    guard_mode: Option<bool>,
    tx: Option<TxState>,
    where_warning: Option<WhereWarning>,
    /// A statement that failed with 25001 inside the transaction.
    outside_offer: Option<Stmt>,
    /// A passing remark for the status bar: "Reconnected to …".
    note: Option<SharedString>,
    alternatives: Option<crate::app_state::Alternatives>,
    focus_handle: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<TabEvent> for ConsoleView {}

impl ConsoleView {
    pub fn new(
        id: Uuid,
        title: String,
        conn: ConnLabel,
        text: String,
        target: Option<ConnTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(text)
        });
        let completion = Rc::new(RefCell::new(Source::default()));
        completion.borrow_mut().dialect = conn.dialect();
        if let Some(t) = &target {
            let mut s = completion.borrow_mut();
            s.relations = t.relations.clone();
            s.catalog = t.catalog.clone();
        }
        editor.update(cx, |e, _| {
            let lsp = e.lsp_mut();
            lsp.completion_provider = Some(Rc::new(SqlCompletion {
                source: completion.clone(),
            }));
            // Tables are ranked by any part of the name, so the match is painted where it is.
            lsp.set_completion_highlighter(Rc::new(gpui_kit::component::input::highlight_contains));
        });
        let grid = Self::new_grid(window, cx);
        let mut subs = Vec::new();
        subs.push(cx.subscribe(&editor, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.emit(TabEvent::StateChanged);
            }
        }));
        subs.push(Self::watch_grid(&grid, cx));
        Self {
            id,
            title,
            conn,
            target,
            session: None,
            editor,
            grid,
            results: Vec::new(),
            current: 0,
            completion,
            messages: Vec::new(),
            pane: Pane::Result,
            running: None,
            run_started: None,
            show_spinner: false,
            stop_requested: false,
            has_result: false,
            result_rows: None,
            truncated: false,
            last_elapsed: None,
            last_error: None,
            export: None,
            record: None,
            guard_mode: None,
            tx: None,
            where_warning: None,
            outside_offer: None,
            note: None,
            alternatives: None,
            focus_handle: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn new_grid(window: &mut Window, cx: &mut Context<Self>) -> Entity<TableState<GridDelegate>> {
        cx.new(|cx| {
            TableState::new(GridDelegate::new(GridMode::Console), window, cx)
                .cell_selectable(true)
                .col_selectable(false)
                .row_selectable(false)
                .row_header(false)
                .sortable(false)
                .col_movable(false)
        })
    }

    fn watch_grid(grid: &Entity<TableState<GridDelegate>>, cx: &mut Context<Self>) -> Subscription {
        cx.subscribe(grid, |this, grid, ev: &TableEvent, cx| match ev {
            TableEvent::RightClickedCell(r, _) => {
                let r = *r;
                grid.update(cx, |g, cx| g.set_right_clicked_row(Some(r), cx));
            }
            TableEvent::SelectCell(..) => {
                let _ = this;
                cx.notify()
            }
            _ => {}
        })
    }

    /// Shows the result of statement `ix` of the last run.
    fn show_result(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.results.get(ix) {
            self.current = ix;
            self.grid = r.grid.clone();
            self.result_rows = Some(r.rows);
            self.truncated = r.truncated;
            self.pane = Pane::Result;
            cx.notify();
        }
    }

    pub fn set_target(&mut self, target: ConnTarget, cx: &mut Context<Self>) {
        let mode_changed = self
            .target
            .as_ref()
            .is_some_and(|t| t.config.read_only != target.config.read_only);
        if mode_changed && self.tx.is_none() && self.running.is_none() {
            self.session = None;
            self.log(
                if target.config.read_only {
                    "Now read-only: the next statement runs in a read-only session."
                } else {
                    "Now read-write: the next statement runs in a read-write session."
                },
                false,
            );
        }
        self.conn = target.label();
        {
            let mut s = self.completion.borrow_mut();
            s.dialect = self.conn.dialect();
            s.relations = target.relations.clone();
            s.catalog = target.catalog.clone();
        }
        self.target = Some(target);
        cx.notify();
    }

    pub fn set_alternatives(&mut self, alt: crate::app_state::Alternatives) {
        self.alternatives = Some(alt);
    }

    /// Moves the console to another connection, keeping its text (3.5).
    pub fn move_to(&mut self, label: ConnLabel, target: Option<ConnTarget>, cx: &mut Context<Self>) {
        if self.tx.is_some() || self.running.is_some() {
            return;
        }
        let from = std::mem::replace(&mut self.conn, label);
        self.session = None;
        self.target = None;
        if let Some(t) = target {
            self.set_target(t, cx);
        }
        self.log(format!("Switched from {} to {}", from.name, self.conn.name), false);
        cx.emit(TabEvent::StateChanged);
        cx.notify();
    }

    /// Tab: take the highlighted completion, or indent when no menu is open.
    fn accept_completion(&mut self, _: &AcceptCompletion, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.editor.read(cx).completion_menu_state().open;
        if open {
            window.dispatch_action(
                Box::new(gpui_kit::component::input::Enter {
                    secondary: false,
                    shift: false,
                }),
                cx,
            );
        } else {
            window.dispatch_action(Box::new(gpui_kit::component::input::IndentInline), cx);
        }
    }

    /// Underlines the spot a server error points at and puts the caret there (3.6).
    fn mark_error(&mut self, stmt: &Stmt, position: u32, message: String, window: &mut Window, cx: &mut Context<Self>) {
        let (text, start) = stmt;
        let at = text
            .char_indices()
            .nth(position.saturating_sub(1) as usize)
            .map(|(b, _)| *start + b)
            .unwrap_or(*start);
        self.editor.update(cx, |e, cx| {
            let value = e.value().to_string();
            if at > value.len() {
                return;
            }
            // The word at the error, at least one character.
            let len = value[at..]
                .char_indices()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                .map(|(i, _)| i)
                .unwrap_or(value.len() - at)
                .max(value[at..].chars().next().map_or(0, char::len_utf8));
            let rope = Rope::from(value.as_str());
            let range = rope.offset_to_position(at)..rope.offset_to_position(at + len);
            if let Some(set) = e.diagnostics_mut() {
                set.clear();
                set.push(Diagnostic::new(range, message).with_severity(DiagnosticSeverity::Error));
            }
            e.set_selected_range(at..at, cx);
            e.focus(window, cx);
            cx.notify();
        });
    }

    fn clear_errors(&mut self, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| {
            if let Some(set) = e.diagnostics_mut()
                && !set.is_empty()
            {
                set.clear();
                cx.notify();
            }
        });
    }

    pub fn has_target(&self) -> bool {
        self.target.is_some()
    }

    pub fn state(&self, cx: &App) -> TabState {
        TabState::Console {
            id: self.id,
            connection: self.conn.id,
            title: self.title.clone(),
            text: self.editor.read(cx).value().to_string(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some()
    }

    /// The zoom changed: lay out the result grid again.
    pub fn relayout(&mut self, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| g.refresh(cx));
        cx.notify();
    }

    fn toggle_record(&mut self, _: &ToggleRecord, _: &mut Window, cx: &mut Context<Self>) {
        if self.record.take().is_none() {
            self.record = Some(cx.new(|_| RecordPanel::new()));
        }
        cx.notify();
    }

    /// The selected result row into the record panel (read-only).
    fn sync_record(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.record.clone() else { return };
        let g = self.grid.read(cx);
        let row = g.selected_cell().map(|(r, _)| r);
        let d = g.delegate();
        let (title, fields): (SharedString, Vec<RecordField>) = match row {
            Some(r) if self.has_result => (
                format!("Row {}", format::count(r as u64 + 1)).into(),
                d.columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| RecordField {
                        col: i,
                        name: c.name.clone(),
                        type_name: c.type_label.clone(),
                        value: d.text(r, i),
                        default: false,
                        changed: false,
                        editable: false,
                        key: false,
                        references: None,
                    })
                    .collect(),
            ),
            _ => ("No row selected".into(), Vec::new()),
        };
        panel.update(cx, |p, cx| p.set_record(title, fields, cx));
    }

    fn jump(&mut self, to: Jump, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| gridmod::jump(g, to, cx));
        cx.notify();
    }

    /// Which menu bar commands apply right now (D2).
    pub fn menu_state(&self, cx: &App) -> MenuState {
        let cell = self.has_result && self.grid.read(cx).selected_cell().is_some_and(|(_, c)| c > 0);
        MenuState {
            console: true,
            cell,
            running: self.running.is_some(),
            connected: self.target.is_some(),
            can_export: self.has_result && self.export.is_none(),
            ..Default::default()
        }
    }

    pub fn editor_handle(&self) -> Entity<EditorState> {
        self.editor.clone()
    }

    pub fn status(&self) -> TabStatus {
        TabStatus {
            rows: self.result_rows.map(|n| {
                let mut s = format::rows(n);
                if self.truncated {
                    s.push_str(" (capped)");
                }
                s.into()
            }),
            timing: self.last_elapsed.map(|d| format::duration(d).into()),
            error: self.last_error.clone(),
            export: self.export.as_ref().map(|e| e.progress()),
            note: self.note.clone(),
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.session.take()
            && self.running.is_some()
        {
            cx.background_spawn(s.cancel()).detach();
        }
        self.running = None;
        cx.notify();
    }

    fn statements_to_run(&self, all: bool, cx: &App) -> Vec<Stmt> {
        let e = self.editor.read(cx);
        let text = e.value().to_string();
        if all {
            return sql::split_in(&text, self.conn.dialect())
                .into_iter()
                .map(|s| (s.text, s.range.start))
                .collect();
        }
        let sel = e.selected_range();
        if sel.start < sel.end
            && sel.end <= text.len()
            && let Some(selected) = text.get(sel.clone())
        {
            return sql::split_in(selected, self.conn.dialect())
                .into_iter()
                .map(|s| (s.text, sel.start + s.range.start))
                .collect();
        }
        let caret = e.cursor().min(text.len());
        sql::statement_at(&text, caret)
            .into_iter()
            .map(|s| (s.text, s.range.start))
            .collect()
    }

    fn run_statement(&mut self, _: &RunStatement, window: &mut Window, cx: &mut Context<Self>) {
        let stmts = self.statements_to_run(false, cx);
        self.run(stmts, window, cx);
    }

    fn run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        let stmts = self.statements_to_run(true, cx);
        self.run(stmts, window, cx);
    }

    fn stop(&mut self, _: &StopQuery, _: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_none() {
            cx.propagate();
            return;
        }
        self.stop_requested = true;
        if let Some(s) = &self.session {
            let fut = s.cancel();
            cx.background_spawn(async move {
                let _ = fut.await;
            })
            .detach();
        }
    }

    /// Whether writes run inside a transaction baselynx opens: by default on
    /// red and amber connections, overridden per tab by the toolbar switch.
    pub fn guarded(&self) -> bool {
        self.guard_mode
            .unwrap_or(matches!(self.conn.color, ConnectionColor::Red | ConnectionColor::Amber))
    }

    /// The open transaction keeps the tab dirty: closing or switching asks first.
    pub fn is_dirty(&self) -> bool {
        self.tx.is_some()
    }

    fn run(&mut self, stmts: Vec<Stmt>, window: &mut Window, cx: &mut Context<Self>) {
        self.run_with(stmts, RunMode::Normal, window, cx);
    }

    fn run_with(&mut self, stmts: Vec<Stmt>, mode: RunMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.running.is_some() || stmts.is_empty() {
            return;
        }
        let Some(target) = self.target.clone() else {
            self.log("Not connected yet.", true);
            cx.notify();
            return;
        };
        let user_tx = stmts.iter().any(|(s, _)| classify::is_transaction_control(s));
        if user_tx && self.tx.is_some() {
            self.log(
                "Commit or roll back the open transaction before running BEGIN, COMMIT or ROLLBACK.",
                true,
            );
            self.pane = Pane::Messages;
            cx.notify();
            return;
        }
        // A script that manages its own transaction is run as written.
        let guard = mode != RunMode::Outside && self.guarded() && !user_tx;
        if guard
            && mode != RunMode::Confirmed
            && let Some((schema, table)) = stmts.iter().find_map(|(s, _)| classify::missing_where(s))
        {
            self.where_warning = Some(WhereWarning {
                text: self.where_text(&schema, &table).into(),
                stmts,
            });
            cx.notify();
            return;
        }
        self.where_warning = None;
        self.outside_offer = None;
        self.clear_errors(cx);
        self.stop_requested = false;
        self.last_error = None;
        self.run_started = Some(Instant::now());
        self.show_spinner = false;
        let existing = self.session.clone().filter(|s| !s.is_closed());
        // A session that died reconnects on its own for the next run (3.8).
        self.note = match (&self.session, &existing) {
            (Some(_), None) if self.tx.is_none() => Some(format!("Reconnected to {}", self.conn.name).into()),
            _ => None,
        };
        if existing.is_none() && self.tx.is_some() {
            // The session holding the transaction is gone, and the transaction with it.
            self.tx = None;
            self.log(
                "The connection was lost; the open transaction was rolled back by the server.",
                true,
            );
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let _ = this.update(cx, |this, cx| {
                if this.running.is_some() {
                    this.show_spinner = true;
                    cx.notify();
                }
            });
        })
        .detach();
        let task = cx.spawn_in(window, async move |this, cx| {
            let session = match existing {
                Some(s) => s,
                None => match session::connect(&target.config, target.password.clone()).await {
                    Ok(s) => {
                        let _ = this.update(cx, |this, _| this.session = Some(s.clone()));
                        s
                    }
                    Err(e) => {
                        let _ = this.update(cx, |this, cx| {
                            this.log(format!("Could not connect: {e}"), true);
                            this.last_error = Some(e.to_string().into());
                            this.finish(cx);
                        });
                        return;
                    }
                },
            };
            let mut first_result = true;
            for item in stmts {
                let stmt = item.0.clone();
                if this.update(cx, |this, _| this.stop_requested).unwrap_or(true) {
                    break;
                }
                let write = guard && classify::is_write(&stmt);
                let open = this.update(cx, |this, _| this.tx.is_some()).unwrap_or(false);
                if write && !open {
                    if let Err(e) = exec::execute_collect(&session, "BEGIN".into(), None).await {
                        let _ = this.update(cx, |this, cx| {
                            this.log(format!("Could not open a transaction: {e}"), true);
                            cx.notify();
                        });
                        break;
                    }
                    let _ = this.update(cx, |this, _| {
                        this.tx = Some(TxState::default());
                    });
                }
                // Inside the transaction each statement gets a savepoint, so a failing
                // statement does not throw away the changes before it.
                let in_tx = write || open;
                if in_tx {
                    let _ = exec::execute_collect(&session, "SAVEPOINT baselynx_statement".into(), None).await;
                }
                let preview = format::statement_preview(&stmt, 80);
                let title: SharedString = format::statement_preview(&stmt, 24).into();
                let verb = classify::write_verb(&stmt);
                let started = Instant::now();
                let mut reported: Option<u64> = None;
                let mut rx = exec::execute(&session, stmt.clone(), Some(ROW_CAP));
                let mut failure: Option<baselynx_core::CoreError> = None;
                let mut affected = 0u64;
                while let Some(ev) = rx.recv().await {
                    let keep_going = this
                        .update_in(cx, |this, window, cx| match ev {
                            Ok(ExecEvent::Columns(cols)) => {
                                // Every statement that returns rows gets its own result (4.9).
                                if std::mem::take(&mut first_result) {
                                    this.results.clear();
                                }
                                if this.results.len() >= MAX_RESULTS {
                                    this.results.remove(0);
                                }
                                let grid = Self::new_grid(window, cx);
                                grid.update(cx, |g, cx| {
                                    g.delegate_mut().set_data(&cols, Vec::new(), None, 0);
                                    g.refresh(cx);
                                });
                                let sub = Self::watch_grid(&grid, cx);
                                this.results.push(ResultSet {
                                    title: title.clone(),
                                    grid,
                                    rows: 0,
                                    truncated: false,
                                    _sub: sub,
                                });
                                this.has_result = true;
                                let last = this.results.len() - 1;
                                this.show_result(last, cx);
                                true
                            }
                            Ok(ExecEvent::Rows(rows)) => {
                                let Some(r) = this.results.last_mut() else { return true };
                                r.grid.update(cx, |g, cx| {
                                    g.delegate_mut().model.append_loaded(rows);
                                    g.refresh(cx);
                                });
                                let n = r.grid.read(cx).delegate().model.loaded_len() as u64;
                                r.rows = n;
                                if this.current + 1 == this.results.len() {
                                    this.result_rows = Some(n);
                                }
                                cx.notify();
                                true
                            }
                            Ok(ExecEvent::Done {
                                count,
                                returned_rows,
                                elapsed,
                                truncated,
                            }) => {
                                let what = if returned_rows {
                                    format::rows(count)
                                } else {
                                    affected = count;
                                    format!("{} affected", format::rows(count))
                                };
                                reported = Some(count);
                                let pending = if in_tx { " · not committed" } else { "" };
                                this.log(
                                    format!("{preview} · {what} · {}{pending}", format::duration(elapsed)),
                                    false,
                                );
                                this.last_elapsed = Some(elapsed);
                                if returned_rows {
                                    if let Some(r) = this.results.last_mut() {
                                        r.truncated = truncated;
                                    }
                                    this.truncated = truncated;
                                }
                                cx.notify();
                                true
                            }
                            Err(e) => {
                                if let Some(pos) = e.db().and_then(|d| d.position) {
                                    let message = e.db().map(|d| d.message.clone()).unwrap_or_default();
                                    this.mark_error(&item, pos, message, window, cx);
                                }
                                let text = if e.is_cancel() {
                                    format!("{preview} · cancelled")
                                } else {
                                    format!("{preview}\n{e}")
                                };
                                this.log(text, true);
                                this.last_error = Some(e.to_string().into());
                                if !e.is_cancel() {
                                    this.pane = Pane::Messages;
                                }
                                failure = Some(e);
                                cx.notify();
                                false
                            }
                        })
                        .unwrap_or(false);
                    if !keep_going {
                        break;
                    }
                }
                let _ = this.update(cx, |_, cx| {
                    cx.emit(TabEvent::Ran {
                        text: stmt.clone(),
                        rows: reported,
                        duration: started.elapsed(),
                    })
                });
                if in_tx {
                    let ok = failure.is_none();
                    let undo = if ok {
                        "RELEASE SAVEPOINT baselynx_statement"
                    } else {
                        "ROLLBACK TO SAVEPOINT baselynx_statement"
                    };
                    let _ = exec::execute_collect(&session, undo.into(), None).await;
                    let empty = this
                        .update(cx, |this, _| {
                            if let Some(tx) = this.tx.as_mut()
                                && ok
                                && write
                            {
                                tx.statements += 1;
                                tx.rows += affected;
                                tx.verb = verb;
                            }
                            this.tx.as_ref().is_some_and(|t| t.statements == 0)
                        })
                        .unwrap_or(false);
                    // A transaction opened for a statement that failed holds nothing: close it.
                    if !ok && empty {
                        let _ = exec::execute_collect(&session, "ROLLBACK".into(), None).await;
                        let _ = this.update(cx, |this, _| this.tx = None);
                    }
                }
                if let Some(e) = failure {
                    // VACUUM, CREATE INDEX CONCURRENTLY …: offered outside the transaction,
                    // never retried silently.
                    if e.cannot_run_in_transaction() {
                        let _ = this.update(cx, |this, cx| {
                            this.outside_offer = Some(item.clone());
                            cx.notify();
                        });
                    }
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| this.finish(cx));
        });
        self.running = Some(task);
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    /// "No WHERE: this touches every row of public.customers (about 1.2 million)".
    fn where_text(&self, schema: &str, table: &str) -> String {
        let found = self.target.as_ref().map(|t| t.relations.clone()).unwrap_or_default();
        let rel = found
            .iter()
            .filter(|r| r.name == table && (schema.is_empty() || r.schema == schema))
            .min_by_key(|r| r.schema != "public");
        let name = match rel {
            Some(r) => format!("{}.{}", r.schema, r.name),
            None if schema.is_empty() => table.to_string(),
            None => format!("{schema}.{table}"),
        };
        match rel.and_then(|r| r.estimate) {
            Some(n) => format!("No WHERE: this touches every row of {name} (about {})", about(n)),
            None => format!("No WHERE: this touches every row of {name}"),
        }
    }

    fn end_transaction(&mut self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(session), Some(tx)) = (self.session.clone(), self.tx.clone()) else {
            self.tx = None;
            cx.notify();
            return;
        };
        if self.running.is_some() {
            return;
        }
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = exec::execute_collect(&session, sql.into(), None).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.tx = None;
                        let what = tx.summary();
                        this.log(
                            if commit {
                                format!("Committed: {what}")
                            } else {
                                format!("Rolled back: {what}")
                            },
                            false,
                        );
                    }
                    Err(e) => {
                        this.log(format!("{sql} failed: {e}"), true);
                        this.pane = Pane::Messages;
                    }
                }
                this.finish(cx);
            });
        });
        self.running = Some(task);
        cx.notify();
    }

    fn finish(&mut self, cx: &mut Context<Self>) {
        self.running = None;
        self.show_spinner = false;
        cx.emit(TabEvent::Updated);
        cx.notify();
    }

    fn log(&mut self, text: impl Into<SharedString>, error: bool) {
        self.messages.push(Message {
            text: text.into(),
            error,
        });
        if self.messages.len() > 500 {
            self.messages.remove(0);
        }
    }

    fn copy_cell(&mut self, _: &CopyCell, _: &mut Window, cx: &mut Context<Self>) {
        let grid = self.grid.read(cx);
        if let Some((r, c)) = grid.selected_cell()
            && let Some(d) = grid.delegate().data_col(c)
        {
            let text = grid.delegate().shown_text(r, d).unwrap_or_default();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_row(&mut self, json: bool, cx: &mut Context<Self>) {
        let g = self.grid.read(cx);
        let Some((row, _)) = g.selected_cell() else { return };
        let cells = g.delegate().row_cells(row);
        let text = if json {
            crate::row_text::json(&cells)
        } else {
            crate::row_text::tsv(&cells)
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn export(&mut self, _: &ExportData, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_result || self.export.is_some() {
            return;
        }
        let mut notes = Vec::new();
        if self.truncated {
            notes.push(SharedString::from(format!(
                "The result was capped at {} rows; only those are exported.",
                format::count(ROW_CAP as u64)
            )));
        }
        let view = cx.entity().downgrade();
        let grid = self.grid.clone();
        export_dialog::open_options(false, notes, window, cx, move |choice, window, cx| {
            let d = grid.read(cx).delegate();
            let columns = d
                .columns
                .iter()
                .map(|c| baselynx_core::exec::ColumnMeta::named(c.name.clone()))
                .collect();
            let rows = d.model.loaded().to_vec();
            export_dialog::run(
                view.clone(),
                ExportSource::Loaded { columns, rows },
                choice.format,
                "result".into(),
                window,
                cx,
                |v: &mut ConsoleView, job| v.export = job,
                |v: &mut ConsoleView, rows| {
                    if let Some(j) = v.export.as_mut() {
                        j.rows = rows;
                    }
                },
            );
        });
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.running.is_some();
        let red = self.conn.color == ConnectionColor::Red;
        h_flex()
            .h_9()
            .px_2()
            .gap_1()
            .flex_shrink_0()
            .bg(tokens::raised(cx))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("run")
                    .icon(IconName::Play)
                    .label("Run")
                    .small()
                    .ghost()
                    .when(red, |b| b.outline().danger())
                    .disabled(running)
                    .tooltip_with_action("Run the statement at the caret", &RunStatement, Some(CONSOLE))
                    .on_click(cx.listener(|this, _, window, cx| this.run_statement(&RunStatement, window, cx))),
            )
            .child(
                Button::new("run-all")
                    .label("Run all")
                    .small()
                    .ghost()
                    .disabled(running)
                    .tooltip_with_action("Run every statement", &RunAll, Some(CONSOLE))
                    .on_click(cx.listener(|this, _, window, cx| this.run_all(&RunAll, window, cx))),
            )
            .child(
                Button::new("stop")
                    .icon(IconName::Square)
                    .label("Stop")
                    .small()
                    .ghost()
                    .disabled(!running)
                    .tooltip_with_action("Cancel the running statement", &StopQuery, Some(CONSOLE))
                    .on_click(cx.listener(|this, _, window, cx| this.stop(&StopQuery, window, cx))),
            )
            .child(
                h_flex()
                    .ml_2()
                    .gap_1()
                    .text_sm()
                    .text_color(tokens::secondary_text(cx))
                    .child({
                        // The connection name moves the console to another connection (3.5).
                        let alt = self.alternatives.clone();
                        let view = cx.entity().downgrade();
                        let own = self.conn.clone();
                        let locked = self.tx.is_some() || running;
                        Button::new("console-conn")
                            .small()
                            .ghost()
                            .dropdown_caret(true)
                            .disabled(locked)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .when_some(tokens::connection(self.conn.color, cx), |d, c| {
                                        d.child(div().size_2().rounded_full().bg(c))
                                    })
                                    .child(self.conn.name.clone()),
                            )
                            .tooltip(if locked {
                                "Finish the run and the transaction to move the console"
                            } else {
                                "Move this console to another connection"
                            })
                            .dropdown_menu(move |menu, _, cx| {
                                let others = alt.as_ref().map(|f| f(cx)).unwrap_or_default();
                                let mut menu =
                                    menu.item(PopupMenuItem::new(own.name.clone()).checked(true).disabled(true));
                                if !others.is_empty() {
                                    menu = menu.separator();
                                }
                                for (label, connected) in others {
                                    let view = view.clone();
                                    let id = label.id;
                                    let text = if connected {
                                        label.name.clone()
                                    } else {
                                        format!("{} (connect)", label.name)
                                    };
                                    menu = menu.item(PopupMenuItem::new(text).on_click(move |_, _, cx| {
                                        let _ = view.update(cx, |_, cx| cx.emit(TabEvent::MoveConsole { conn: id }));
                                    }));
                                }
                                menu
                            })
                    })
                    .when(self.session.as_ref().is_some_and(|s| s.is_pooled()), |this| {
                        this.child(
                            div()
                                .id("pooled")
                                .px_1()
                                .rounded(cx.theme().radius)
                                .border_1()
                                .border_color(cx.theme().border)
                                .text_xs()
                                .child("pooled")
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        "Through a connection pooler: SET, temporary tables and advisory locks \
                                         do not persist between statements.",
                                    )
                                    .build(window, cx)
                                }),
                        )
                    }),
            )
            .child(div().flex_1())
            .child({
                let guarded = self.guarded();
                Button::new("guard-mode")
                    .icon(if guarded {
                        Icon::new(gpui_kit::assets::IconName::Lock)
                    } else {
                        Icon::new(IconName::Play)
                    })
                    .label(if guarded { "Guarded" } else { "Autocommit" })
                    .small()
                    .ghost()
                    .selected(guarded)
                    .disabled(self.tx.is_some() || running)
                    .tooltip(if guarded {
                        "Guarded: writes run in a transaction you commit or roll back. Click for autocommit."
                    } else {
                        "Autocommit: each statement is committed at once. Click to guard writes."
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.guard_mode = Some(!guarded);
                        cx.notify();
                    }))
            })
            .child(
                Button::new("export")
                    .label("Export")
                    .small()
                    .ghost()
                    .disabled(!self.has_result || self.export.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.export(&ExportData, window, cx))),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.pane;
        // One tab per statement that returned rows, titled by its first words (4.9).
        let n = self.results.len().max(1);
        let mut tabs = TabBar::new("console-panes")
            .underline()
            .small()
            .selected_index(if pane == Pane::Result {
                self.current.min(n - 1)
            } else {
                n
            })
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                if *ix < n {
                    this.show_result(*ix, cx);
                } else {
                    this.pane = Pane::Messages;
                }
                cx.notify();
            }));
        if self.results.len() <= 1 {
            tabs = tabs.child(Tab::new().label("Result"));
        } else {
            for r in &self.results {
                tabs = tabs.child(Tab::new().label(r.title.clone()));
            }
        }
        let tabs = tabs.child(Tab::new().label(if self.messages.iter().any(|m| m.error) {
            "Messages •"
        } else {
            "Messages"
        }));
        let body = match pane {
            Pane::Result => {
                if self.has_result {
                    v_flex()
                        .size_full()
                        .when(self.truncated, |this| {
                            this.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .text_sm()
                                    .text_color(tokens::secondary_text(cx))
                                    .child(format!("First {} rows shown", format::count(ROW_CAP as u64))),
                            )
                        })
                        .child(
                            h_flex()
                                .items_start()
                                .flex_1()
                                .min_h_0()
                                .child(
                                    div()
                                        .key_context(GRID)
                                        .on_action(cx.listener(Self::copy_cell))
                                        .on_action(cx.listener(|this, _: &CopyRowJson, _, cx| this.copy_row(true, cx)))
                                        .on_action(cx.listener(|this, _: &CopyRowTsv, _, cx| this.copy_row(false, cx)))
                                        .on_action(
                                            cx.listener(|this, _: &GridRowStart, _, cx| this.jump(Jump::RowStart, cx)),
                                        )
                                        .on_action(
                                            cx.listener(|this, _: &GridRowEnd, _, cx| this.jump(Jump::RowEnd, cx)),
                                        )
                                        .on_action(cx.listener(|this, _: &GridTop, _, cx| this.jump(Jump::Top, cx)))
                                        .on_action(
                                            cx.listener(|this, _: &GridBottom, _, cx| this.jump(Jump::Bottom, cx)),
                                        )
                                        .flex_1()
                                        .min_w_0()
                                        .h_full()
                                        .child(crate::ui::grid::data_table(&self.grid, cx)),
                                )
                                .when_some(self.record.clone(), |d, panel| {
                                    d.child(div().w(tokens::ui(360., cx)).h_full().flex_shrink_0().child(panel))
                                }),
                        )
                        .into_any_element()
                } else {
                    h_flex()
                        .size_full()
                        .justify_center()
                        .text_sm()
                        .text_color(tokens::secondary_text(cx))
                        .child("Ctrl+Enter runs the statement at the caret")
                        .into_any_element()
                }
            }
            Pane::Messages => v_flex()
                .id("messages")
                .size_full()
                .overflow_y_scroll()
                .p_2()
                .gap_1()
                .font_family(cx.theme().mono_font_family.clone())
                .text_sm()
                .children(self.messages.iter().enumerate().map(|(i, m)| {
                    div()
                        .id(("msg", i))
                        .whitespace_normal()
                        .when(m.error, |d| d.text_color(tokens::error_text(cx)))
                        .child(m.text.clone())
                }))
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .child(h_flex().px_2().border_b_1().border_color(cx.theme().border).child(tabs))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl ConsoleView {
    /// The bars between the editor and the result: the missing-WHERE question, the open
    /// transaction, and a statement that must run outside one.
    fn render_bars(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let red = self.conn.color == ConnectionColor::Red;
        let bar = |cx: &App, fill: Hsla, edge: Hsla| {
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .gap_2()
                .text_sm()
                .bg(cx.theme().background)
                .child(div().absolute().inset_0().bg(fill))
                .border_b_1()
                .border_color(edge)
                .relative()
        };
        if let Some(w) = &self.where_warning {
            out.push(
                bar(cx, tokens::changed_fill(cx), tokens::changed(cx).opacity(0.5))
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(tokens::changed(cx)),
                    )
                    .child(div().flex_1().child(w.text.clone()))
                    .child(
                        Button::new("where-run")
                            .label("Run anyway")
                            .small()
                            .when(red, |b| b.danger())
                            .when(!red, |b| b.primary())
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(w) = this.where_warning.take() {
                                    this.run_with(w.stmts, RunMode::Confirmed, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("where-cancel")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.where_warning = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        if let Some(tx) = &self.tx {
            out.push(
                bar(cx, tokens::changed_fill(cx), tokens::changed(cx).opacity(0.5))
                    .child(div().size_2().rounded_full().bg(tokens::changed(cx)))
                    .child(div().flex_1().child(format!("{} · not committed", tx.summary())))
                    .child(
                        Button::new("tx-commit")
                            .label("Commit")
                            .small()
                            .when(red, |b| b.danger())
                            .when(!red, |b| b.primary())
                            .disabled(self.running.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.end_transaction(true, window, cx))),
                    )
                    .child(
                        Button::new("tx-rollback")
                            .label("Roll back")
                            .small()
                            .outline()
                            .disabled(self.running.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.end_transaction(false, window, cx))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(stmt) = &self.outside_offer {
            let stmt = stmt.clone();
            let blocked = self.tx.is_some();
            out.push(
                bar(cx, tokens::removed_fill(cx), tokens::removed(cx).opacity(0.4))
                    .child(div().flex_1().child("This statement cannot run inside a transaction."))
                    .child(
                        Button::new("tx-outside")
                            .label("Run outside a transaction")
                            .small()
                            .outline()
                            .disabled(blocked)
                            .when(blocked, |b| b.tooltip("Commit or roll back the open transaction first"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.outside_offer = None;
                                this.run_with(vec![stmt.clone()], RunMode::Outside, window, cx);
                            })),
                    )
                    .child(
                        Button::new("tx-outside-dismiss")
                            .icon(IconName::Close)
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.outside_offer = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        out
    }
}

impl Focusable for ConsoleView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConsoleView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_record(cx);
        let progress = self.show_spinner;
        v_flex()
            .key_context(CONSOLE)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_statement))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::stop))
            .on_action(cx.listener(Self::export))
            .on_action(cx.listener(Self::toggle_record))
            .on_action(cx.listener(Self::accept_completion))
            .size_full()
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .h(px(2.))
                    .w_full()
                    .flex_shrink_0()
                    .when(progress, |d| d.bg(cx.theme().primary)),
            )
            .child(
                v_resizable("console-split")
                    .child(
                        resizable_panel()
                            .size(px(260.))
                            .child(Editor::new(&self.editor).bordered(false).h_full()),
                    )
                    .child(
                        resizable_panel().child(
                            v_flex()
                                .size_full()
                                .children(self.render_bars(cx))
                                .child(div().flex_1().min_h_0().child(self.render_results(cx))),
                        ),
                    ),
            )
    }
}
