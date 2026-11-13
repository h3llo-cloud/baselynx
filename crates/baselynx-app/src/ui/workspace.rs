//! The window: title bar with the project switcher, sidebar tree, tabs, status bar.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use baselynx_core::catalog::{self, RelKind, RelationRef, SchemaInfo};
use baselynx_core::config;
use baselynx_core::model::{ConnectionConfig, Project, TabState, parse_connection_url};
use baselynx_core::session::{self, Session};
use baselynx_core::store::{SecretStore, Store};
use baselynx_core::table::DEFAULT_PAGE_SIZE;
use baselynx_core::{APP_NAME, runtime};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogClose, DialogFooter};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
#[allow(unused_imports)]
use gpui_kit::component::{Disableable as _, Selectable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use crate::actions::*;
use crate::app_state::{ConnTarget, TabEvent, TabStatus};
use crate::format;
use crate::ui::connection_form::{ConnectionForm, open_text_prompt};
use crate::ui::console::ConsoleView;
use crate::ui::palette::{Palette, PaletteItem};
use crate::ui::table_view::TableView;
use crate::ui::tokens;

mod backups;
mod backups_view;
mod config_editor;
mod menubar;
mod sidebar;
mod tab_menu;

use sidebar::SideState;

type ActionFactory = fn() -> Box<dyn Action>;

pub enum ConnStatus {
    Disconnected,
    Connecting,
    Connected { session: Arc<Session> },
    Failed(String),
}

pub struct ConnRuntime {
    pub status: ConnStatus,
    /// Kept in memory until the project is closed.
    pub password: Option<String>,
    pub schemas: Vec<SchemaInfo>,
    pub relations: HashMap<String, Vec<RelationRef>>,
    pub all_relations: Arc<Vec<RelationRef>>,
    pub expanded: bool,
    pub expanded_schemas: HashSet<String>,
    pub loading_schemas: HashSet<String>,
}

impl Default for ConnRuntime {
    fn default() -> Self {
        Self {
            status: ConnStatus::Disconnected,
            password: None,
            schemas: Vec::new(),
            relations: HashMap::new(),
            all_relations: Arc::new(Vec::new()),
            expanded: false,
            expanded_schemas: HashSet::new(),
            loading_schemas: HashSet::new(),
        }
    }
}

#[derive(Clone)]
pub enum TabView {
    Console(Entity<ConsoleView>),
    Table(Entity<TableView>),
}

impl TabView {
    pub fn id(&self, cx: &App) -> Uuid {
        match self {
            TabView::Console(v) => v.read(cx).id,
            TabView::Table(v) => v.read(cx).id,
        }
    }

    pub fn connection(&self, cx: &App) -> Uuid {
        match self {
            TabView::Console(v) => v.read(cx).conn.id,
            TabView::Table(v) => v.read(cx).conn.id,
        }
    }

    fn title(&self, cx: &App) -> String {
        match self {
            TabView::Console(v) => v.read(cx).title.clone(),
            TabView::Table(v) => v.read(cx).title(),
        }
    }

    fn tooltip(&self, cx: &App) -> String {
        match self {
            TabView::Console(v) => format!("{} / {}", v.read(cx).conn.name, v.read(cx).title),
            TabView::Table(v) => {
                let v = v.read(cx);
                format!("{} / {}.{}", v.conn.name, v.schema, v.table)
            }
        }
    }

    pub fn state(&self, cx: &App) -> TabState {
        match self {
            TabView::Console(v) => v.read(cx).state(cx),
            TabView::Table(v) => v.read(cx).state(),
        }
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        match self {
            TabView::Console(v) => v.read(cx).is_dirty(),
            TabView::Table(v) => v.read(cx).is_dirty(cx),
        }
    }

    fn is_busy(&self, cx: &App) -> bool {
        match self {
            TabView::Console(v) => v.read(cx).is_busy(),
            TabView::Table(v) => v.read(cx).is_busy(),
        }
    }

    fn status(&self, cx: &App) -> TabStatus {
        match self {
            TabView::Console(v) => v.read(cx).status(),
            TabView::Table(v) => v.read(cx).status(cx),
        }
    }

    fn has_target(&self, cx: &App) -> bool {
        match self {
            TabView::Console(v) => v.read(cx).has_target(),
            TabView::Table(v) => v.read(cx).has_target(),
        }
    }

    fn set_target(&self, target: ConnTarget, window: &mut Window, cx: &mut App) {
        match self {
            TabView::Console(v) => v.update(cx, |v, cx| v.set_target(target, cx)),
            TabView::Table(v) => v.update(cx, |v, cx| v.set_target(target, window, cx)),
        }
    }

    fn set_active(&self, active: bool, window: &mut Window, cx: &mut App) {
        if let TabView::Table(v) = self {
            v.update(cx, |v, cx| v.set_active(active, window, cx));
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        match self {
            TabView::Console(v) => {
                let editor = v.read(cx).editor_handle();
                editor.update(cx, |e, cx| e.focus(window, cx));
            }
            TabView::Table(v) => {
                let handle = v.read(cx).grid_focus(cx);
                handle.focus(window, cx);
            }
        }
    }

    /// The element menu bar commands are dispatched from: the grid of a table tab, the
    /// editor of a console.
    fn action_focus(&self, cx: &App) -> FocusHandle {
        match self {
            TabView::Console(v) => v.read(cx).editor_handle().read(cx).focus_handle(cx),
            TabView::Table(v) => v.read(cx).grid_focus(cx),
        }
    }

    fn menu_state(&self, cx: &App) -> MenuState {
        match self {
            TabView::Console(v) => v.read(cx).menu_state(cx),
            TabView::Table(v) => v.read(cx).menu_state(cx),
        }
    }

    fn relayout(&self, cx: &mut App) {
        match self {
            TabView::Console(v) => v.update(cx, |v, cx| v.relayout(cx)),
            TabView::Table(v) => v.update(cx, |v, cx| v.relayout(cx)),
        }
    }

    fn close(&self, cx: &mut App) {
        match self {
            TabView::Console(v) => v.update(cx, |v, cx| v.close(cx)),
            TabView::Table(v) => v.update(cx, |v, cx| v.close(cx)),
        }
    }

    fn element(&self) -> AnyView {
        match self {
            TabView::Console(v) => v.clone().into(),
            TabView::Table(v) => v.clone().into(),
        }
    }
}

/// What the active tab can do now; drives the enabled state of menu bar items.
#[derive(Debug, Default, Clone, Copy)]
pub struct MenuState {
    pub console: bool,
    pub table: bool,
    /// A data cell is selected and no edit is open.
    pub cell: bool,
    pub editable_cell: bool,
    pub editable: bool,
    pub row_selected: bool,
    pub filter_value: bool,
    pub pending: bool,
    pub prev_page: bool,
    pub next_page: bool,
    pub can_count: bool,
    pub can_export: bool,
    pub running: bool,
    pub connected: bool,
    pub can_undo: bool,
    pub can_redo: bool,
}

pub struct Workspace {
    store: Store,
    secrets: Arc<dyn SecretStore>,
    pub project: Project,
    projects: Vec<Project>,
    conns: HashMap<Uuid, ConnRuntime>,
    pub tabs: Vec<TabView>,
    pub active: Option<usize>,
    sidebar_visible: bool,
    recent: Vec<(Uuid, String, String)>,
    console_counter: usize,
    save_task: Option<Task<()>>,
    focus_handle: FocusHandle,
    sidebar_focus: FocusHandle,
    tab_subs: HashMap<Uuid, Subscription>,
    /// Tabs closed in this project, newest last, for "Reopen closed tab".
    closed: Vec<TabState>,
    /// Connections started together by `connect_all`, for the status bar summary (D10).
    connect_batch: Option<ConnectBatch>,
    side: SideState,
    /// Tables travelled through by following keys, for Alt+[ and Alt+] (3.3).
    trail: Vec<Loc>,
    trail_pos: usize,
    /// Statements run in this project's consoles, newest last (4.10).
    history: Vec<baselynx_core::history::HistoryEntry>,
    _history_save: Option<Task<()>>,
    backups: backups::BackupQueue,
    menubar: menubar::MenuBar,
    /// Passwords from the config file, by connection.
    config_passwords: HashMap<Uuid, String>,
    _subs: Vec<Subscription>,
}

/// Reads a config file and makes its project match it: the project opened from the file
/// before, one that already holds its databases, or a new one. Saved before returning.
fn open_config_project(store: &Store, path: &std::path::Path) -> baselynx_core::Result<(Project, config::SyncReport)> {
    let file = config::load(path)?;
    let (projects, _) = store.list_projects();
    let mut project = config::find_project(&projects, &file)
        .cloned()
        .unwrap_or_else(|| Project::new(file.project.clone()));
    let report = config::sync(&mut project, &file);
    store.save_project(&project)?;
    Ok((project, report))
}

/// A table and filter on the back/forward trail.
#[derive(Clone, Debug, PartialEq)]
struct Loc {
    conn: Uuid,
    schema: String,
    table: String,
    filter: Option<String>,
}

struct ConnectBatch {
    ids: Vec<Uuid>,
    /// Set once every connection has finished and none failed; the summary then fades.
    hide: Option<Task<()>>,
    hidden: bool,
}

impl Workspace {
    /// `config` is a `baselynx.yaml` found next to the binary: its project opens instead of the
    /// last one, and its due backups start.
    pub fn new(
        store: Store,
        secrets: Arc<dyn SecretStore>,
        config: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (mut project, unreadable) = store
            .open_startup_project()
            .unwrap_or_else(|e| (Project::new(format!("Default ({e})")), Vec::new()));
        let mut config_error = None;
        let mut config_report = None;
        if let Some(path) = &config {
            match open_config_project(&store, path) {
                Ok((p, report)) => {
                    project = p;
                    config_report = Some(report);
                }
                Err(e) => config_error = Some(e.to_string()),
            }
        }
        let mut subs = Vec::new();
        subs.push(cx.observe_window_appearance(window, |_, window, cx| {
            tokens::sync_with_system(window, cx);
        }));
        {
            let weak = cx.weak_entity();
            window.on_window_should_close(cx, move |_, cx| {
                let _ = weak.update(cx, |this, cx| this.save_now(cx));
                true
            });
        }
        let (side, side_sub) = SideState::new(window, cx);
        subs.push(side_sub);
        let this = Self {
            store,
            secrets,
            project: Project::new(""),
            projects: Vec::new(),
            conns: HashMap::new(),
            tabs: Vec::new(),
            active: None,
            sidebar_visible: true,
            recent: Vec::new(),
            console_counter: 0,
            save_task: None,
            focus_handle: cx.focus_handle(),
            sidebar_focus: cx.focus_handle(),
            tab_subs: HashMap::new(),
            closed: Vec::new(),
            connect_batch: None,
            side,
            trail: Vec::new(),
            trail_pos: 0,
            history: Vec::new(),
            _history_save: None,
            backups: Default::default(),
            menubar: Default::default(),
            config_passwords: HashMap::new(),
            _subs: subs,
        };
        // The window's overlay layer does not exist yet; anything that can open a dialog
        // or a notification runs after construction.
        cx.defer_in(window, move |this, window, cx| {
            // Passwords first: connecting starts as the project loads.
            if let Some(report) = &config_report {
                this.save_config_passwords(report, cx);
            }
            this.load_project(project, window, cx);
            this.refresh_backup_status(cx);
            if let Some(e) = config_error {
                window.push_notification(Notification::error(e).autohide(false), cx);
            }
            if let Some(report) = config_report {
                if report.added + report.removed > 0 {
                    window.push_notification(
                        Notification::info(format!(
                            "{}: {} added, {} removed",
                            config::FILE_NAME,
                            report.added,
                            report.removed
                        )),
                        cx,
                    );
                }
                // Scheduled backups run once at start-up, when due.
                this.schedule_backups(window, cx);
            }
            for u in unreadable {
                window.push_notification(
                    Notification::warning(format!("Could not read {}: {}", u.path.display(), u.error)).autohide(false),
                    cx,
                );
            }
        });
        this
    }

    // ------------------------------------------------------------------ projects

    fn refresh_project_list(&mut self) {
        self.projects = self.store.list_projects().0;
    }

    fn load_project(&mut self, project: Project, window: &mut Window, cx: &mut Context<Self>) {
        self.project = project;
        self.conns.clear();
        self.tabs.clear();
        self.tab_subs.clear();
        self.closed.clear();
        self.active = None;
        self.recent.clear();
        self.console_counter = 0;
        self.trail.clear();
        self.trail_pos = 0;
        self.history = baselynx_core::history::load(&self.store, self.project.id);
        for c in &self.project.connections {
            // A password written in the config file is known from the start.
            let rt = ConnRuntime {
                password: self.config_passwords.get(&c.id).cloned(),
                ..Default::default()
            };
            self.conns.insert(c.id, rt);
        }
        let states = self.project.tabs.clone();
        let active_id = self.project.active_tab;
        for st in states {
            if self.project.connection(st.connection()).is_none() {
                continue;
            }
            if let TabState::Console { .. } = &st {
                self.console_counter += 1;
            }
            let tab = self.make_tab(st, window, cx);
            self.tabs.push(tab);
        }
        let ix = active_id
            .and_then(|id| self.tabs.iter().position(|t| t.id(cx) == id))
            .or(if self.tabs.is_empty() { None } else { Some(0) });
        let mut state = self.store.load_state();
        state.last_project = Some(self.project.id);
        let _ = self.store.save_state(&state);
        self.refresh_project_list();
        match ix {
            Some(ix) => self.activate(ix, window, cx),
            // Shortcuts need a focused element to dispatch from.
            None => self.focus_handle.focus(window, cx),
        }
        self.connect_all(window, cx);
        cx.notify();
    }

    fn guard_then(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let dirty = self.tabs.iter().any(|t| t.is_dirty(cx) || t.is_busy(cx));
        if !dirty {
            action(self, window, cx);
            return;
        }
        let weak = cx.weak_entity();
        let action = Rc::new(action);
        window.open_dialog(cx, move |dialog, _, _| {
            let weak = weak.clone();
            let action = action.clone();
            dialog
                .title("Uncommitted changes or running queries")
                .child("Some tabs have uncommitted changes or a running query. They are discarded if you continue.")
                .footer(
                    DialogFooter::new()
                        .gap_2()
                        .child(DialogClose::new().child(Button::new("guard-cancel").label("Cancel").outline()))
                        .child(Button::new("guard-go").label("Discard and switch").danger().on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                let action = action.clone();
                                let _ = weak.update(cx, |this, cx| action(this, window, cx));
                            },
                        )),
                )
        });
    }

    fn switch_project(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if id == self.project.id {
            return;
        }
        self.guard_then(window, cx, move |this, window, cx| {
            this.save_now(cx);
            for t in &this.tabs {
                t.close(cx);
            }
            match this.store.load_project(id) {
                Ok(p) => this.load_project(p, window, cx),
                Err(e) => window.push_notification(Notification::error(e.to_string()).autohide(false), cx),
            }
        });
    }

    fn new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        open_text_prompt(
            "New project".into(),
            None,
            String::new(),
            false,
            "Create",
            window,
            cx,
            move |name, window, cx| {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return;
                }
                let _ = weak.update(cx, |this, cx| {
                    let p = Project::new(name);
                    if let Err(e) = this.store.save_project(&p) {
                        window.push_notification(Notification::error(e.to_string()).autohide(false), cx);
                        return;
                    }
                    let id = p.id;
                    this.refresh_project_list();
                    this.switch_project(id, window, cx);
                });
            },
        );
    }

    fn rename_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        open_text_prompt(
            "Rename project".into(),
            None,
            self.project.name.clone(),
            false,
            "Rename",
            window,
            cx,
            move |name, _, cx| {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return;
                }
                let _ = weak.update(cx, |this, cx| {
                    this.project.name = name;
                    this.save_now(cx);
                    this.refresh_project_list();
                    cx.notify();
                });
            },
        );
    }

    fn delete_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        let name = self.project.name.clone();
        let n = self.project.connections.len();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title(format!("Delete project “{name}”?"))
                .description(format!(
                    "Its {n} connection{} and saved passwords are removed. Databases are not touched.",
                    if n == 1 { "" } else { "s" }
                ))
                .confirm()
                .ok_text("Delete project")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        for t in &this.tabs {
                            t.close(cx);
                        }
                        let p = this.project.clone();
                        let store = this.store.clone();
                        let secrets = this.secrets.clone();
                        if let Err(e) = store.delete_project(&p, secrets.as_ref()) {
                            window.push_notification(Notification::error(e.to_string()).autohide(false), cx);
                        }
                        let mut state = store.load_state();
                        state.last_project = None;
                        let _ = store.save_state(&state);
                        let (next, _) = store
                            .open_startup_project()
                            .unwrap_or_else(|e| (Project::new(format!("Default ({e})")), Vec::new()));
                        this.load_project(next, window, cx);
                    });
                    true
                })
        });
    }

    // ------------------------------------------------------------------ appearance

    /// One zoom step in (+1) or out (-1), or back to 100 % (0); every element scales.
    fn zoom(&mut self, delta: i32, cx: &mut Context<Self>) {
        let z = tokens::step_zoom(delta, cx);
        let mut state = self.store.load_state();
        state.zoom = if (z - 1.0).abs() < 0.001 { None } else { Some(z) };
        let _ = self.store.save_state(&state);
        // Grid column widths and row heights are laid out from the zoom.
        for t in self.tabs.clone() {
            t.relayout(cx);
        }
        cx.notify();
    }

    fn set_theme(&mut self, pref: tokens::ThemePref, window: &mut Window, cx: &mut Context<Self>) {
        tokens::set_theme_pref(pref, window, cx);
        let mut state = self.store.load_state();
        state.theme = pref.saved();
        let _ = self.store.save_state(&state);
        for t in self.tabs.clone() {
            t.relayout(cx);
        }
        cx.notify();
    }

    // ------------------------------------------------------------------ saving

    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let _ = this.update(cx, |this, cx| this.save_now(cx));
        }));
    }

    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        self.project.tabs = self.tabs.iter().map(|t| t.state(cx)).collect();
        self.project.active_tab = self.active.and_then(|i| self.tabs.get(i)).map(|t| t.id(cx));
        if let Err(e) = self.store.save_project(&self.project) {
            eprintln!("{APP_NAME}: saving the project failed: {e}");
        }
    }

    // ------------------------------------------------------------------ connections

    fn target(&self, id: Uuid) -> Option<ConnTarget> {
        let config = self.project.connection(id)?.clone();
        let rt = self.conns.get(&id)?;
        match rt.status {
            ConnStatus::Connected { ref session } => Some(ConnTarget {
                config,
                password: rt.password.clone(),
                relations: rt.all_relations.clone(),
                catalog: Some(session.clone()),
            }),
            _ => None,
        }
    }

    /// Connects when the user asks: may prompt for a password, expands the connection.
    pub fn connect(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.connect_mode(id, true, window, cx);
    }

    /// Connects every connection that needs no prompt: Kubernetes secrets, a saved
    /// password, or one typed earlier in this session. Others stay disconnected.
    fn connect_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self.project.connections.iter().map(|c| c.id).collect();
        for id in &ids {
            self.connect_mode(*id, false, window, cx);
        }
        let started: Vec<Uuid> = ids
            .into_iter()
            .filter(|id| matches!(self.conns.get(id).map(|r| &r.status), Some(ConnStatus::Connecting)))
            .collect();
        if started.len() > 1 {
            self.connect_batch = Some(ConnectBatch {
                ids: started,
                hide: None,
                hidden: false,
            });
        }
    }

    /// `(finished, total, connected, failed)` of the current batch.
    fn batch_progress(&self) -> Option<(usize, usize, usize, usize)> {
        let b = self.connect_batch.as_ref().filter(|b| !b.hidden)?;
        let mut done = 0;
        let mut ok = 0;
        let mut failed = 0;
        for id in &b.ids {
            match self.conns.get(id).map(|r| &r.status) {
                Some(ConnStatus::Connecting) => {}
                Some(ConnStatus::Connected { .. }) => {
                    done += 1;
                    ok += 1;
                }
                Some(ConnStatus::Failed(_)) => {
                    done += 1;
                    failed += 1;
                }
                _ => done += 1,
            }
        }
        Some((done, b.ids.len(), ok, failed))
    }

    /// After every connection of the batch has answered without failures, the summary
    /// stays a few seconds and goes.
    fn after_status_change(&mut self, cx: &mut Context<Self>) {
        let Some((done, total, _, failed)) = self.batch_progress() else {
            return;
        };
        let Some(b) = self.connect_batch.as_mut() else { return };
        if done == total && failed == 0 && b.hide.is_none() {
            b.hide = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(8)).await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(b) = this.connect_batch.as_mut() {
                        b.hidden = true;
                    }
                    cx.notify();
                });
            }));
        }
    }

    /// "2 failed" in the status bar: show the failed connections with their errors.
    fn reveal_failed(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        let ids: Vec<Uuid> = self.connect_batch.as_ref().map(|b| b.ids.clone()).unwrap_or_default();
        let mut first = None;
        for id in ids {
            if let Some(rt) = self.conns.get_mut(&id) {
                rt.expanded = matches!(rt.status, ConnStatus::Failed(_));
                if rt.expanded && first.is_none() {
                    first = Some(id);
                }
            }
        }
        if let Some(id) = first {
            self.side_reveal(id, cx);
        }
        cx.notify();
    }

    fn connect_mode(&mut self, id: Uuid, interactive: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.project.connection(id).cloned() else {
            return;
        };
        let rt = self.conns.entry(id).or_default();
        if matches!(rt.status, ConnStatus::Connected { .. } | ConnStatus::Connecting) {
            return;
        }
        // Kubernetes databases read their password from a secret when connecting; a SQLite
        // file has none.
        if config.kube.as_ref().is_some_and(|k| k.credentials.secret.is_some()) || config.engine.is_file() {
            self.connect_with(id, interactive, window, cx);
            return;
        }
        if rt.password.is_none() && config.save_password {
            let secrets = self.secrets.clone();
            rt.status = ConnStatus::Connecting;
            cx.notify();
            let fut = runtime::spawn_blocking(move || secrets.get(id));
            cx.spawn_in(window, async move |this, cx| {
                let saved = fut.await.ok().flatten();
                let _ = this.update_in(cx, |this, window, cx| {
                    if let Some(rt) = this.conns.get_mut(&id) {
                        rt.status = ConnStatus::Disconnected;
                        rt.password = saved.clone();
                    }
                    if saved.is_some() {
                        this.connect_with(id, interactive, window, cx);
                    } else if interactive {
                        this.ask_password(id, window, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        if rt.password.is_some() {
            self.connect_with(id, interactive, window, cx);
        } else if interactive {
            self.ask_password(id, window, cx);
        }
    }

    fn ask_password(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.project.connection(id).cloned() else {
            return;
        };
        let weak = cx.weak_entity();
        open_text_prompt(
            format!("Password for {}", config.name),
            Some(config.describe()),
            String::new(),
            true,
            "Connect",
            window,
            cx,
            move |pw, window, cx| {
                let _ = weak.update(cx, |this, cx| {
                    if let Some(rt) = this.conns.get_mut(&id) {
                        rt.password = Some(pw.clone());
                    }
                    if this.project.connection(id).is_some_and(|c| c.save_password) {
                        let secrets = this.secrets.clone();
                        let pw = pw.clone();
                        cx.background_spawn(runtime::spawn_blocking(move || secrets.set(id, &pw)))
                            .detach();
                    }
                    this.connect_with(id, true, window, cx);
                });
            },
        );
    }

    /// `expand` opens the connection in the sidebar; automatic connects keep it compact.
    fn connect_with(&mut self, id: Uuid, expand: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.project.connection(id).cloned() else {
            return;
        };
        let Some(rt) = self.conns.get_mut(&id) else { return };
        rt.status = ConnStatus::Connecting;
        let password = rt.password.clone();
        cx.notify();
        let fut = session::connect(&config, password);
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let s = fut.await?;
                let schemas = catalog::list_schemas(&s).await?;
                let all = catalog::list_all_relations(&s).await?;
                Ok::<_, baselynx_core::CoreError>((s, schemas, all))
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // The schema a connection works in opens by itself: `public`, SQLite's `main`,
                // or the MySQL database the connection names.
                let home = this.project.connection(id).map(|c| match c.engine {
                    baselynx_core::model::Engine::Postgres => "public".to_string(),
                    baselynx_core::model::Engine::Sqlite => "main".to_string(),
                    baselynx_core::model::Engine::Mysql => c.database.clone(),
                });
                let Some(rt) = this.conns.get_mut(&id) else { return };
                match result {
                    Ok((s, schemas, all)) => {
                        rt.status = ConnStatus::Connected { session: s };
                        rt.expanded |= expand;
                        if let Some(home) = home
                            && schemas.iter().any(|x| x.name == home)
                        {
                            rt.expanded_schemas.insert(home);
                        }
                        rt.schemas = schemas;
                        rt.all_relations = Arc::new(all);
                        this.side.generation += 1;
                        rt.relations.clear();
                        let to_load: Vec<String> = rt.expanded_schemas.iter().cloned().collect();
                        for schema in to_load {
                            this.load_relations(id, schema, window, cx);
                        }
                        this.push_targets(id, window, cx);
                    }
                    Err(e) => {
                        if e.is_auth_failure() {
                            rt.password = None;
                        }
                        rt.status = ConnStatus::Failed(e.to_string());
                        rt.expanded |= expand;
                    }
                }
                this.after_status_change(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn push_targets(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.target(id) else { return };
        for t in self.tabs.clone() {
            if t.connection(cx) == id {
                t.set_target(target.clone(), window, cx);
            }
        }
        if let Some(t) = self.active.and_then(|i| self.tabs.get(i)).cloned() {
            t.set_active(true, window, cx);
        }
    }

    fn disconnect(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self
            .tabs
            .iter()
            .filter(|t| t.connection(cx) == id)
            .map(|t| t.id(cx))
            .collect();
        for tid in ids {
            if let Some(ix) = self.tabs.iter().position(|t| t.id(cx) == tid) {
                self.close_tab_now(ix, cx);
            }
        }
        if let Some(rt) = self.conns.get_mut(&id) {
            rt.status = ConnStatus::Disconnected;
            rt.schemas.clear();
            rt.relations.clear();
            rt.all_relations = Arc::new(Vec::new());
            rt.expanded = false;
        }
        self.side.generation += 1;
        cx.notify();
    }

    fn load_relations(&mut self, id: Uuid, schema: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rt) = self.conns.get_mut(&id) else { return };
        let ConnStatus::Connected { session } = &rt.status else {
            return;
        };
        // A session the server dropped is replaced before it is used.
        if session.is_closed() {
            rt.status = ConnStatus::Disconnected;
            self.connect_with(id, false, window, cx);
            return;
        }
        let fut = catalog::list_relations(session, &schema);
        rt.loading_schemas.insert(schema.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let mut reconnect = false;
                if let Some(rt) = this.conns.get_mut(&id) {
                    rt.loading_schemas.remove(&schema);
                    match result {
                        Ok(rels) => {
                            rt.relations.insert(schema, rels);
                        }
                        // The catalog session died: reconnect once rather than fail.
                        Err(baselynx_core::CoreError::Closed) => {
                            rt.status = ConnStatus::Disconnected;
                            reconnect = true;
                        }
                        Err(e) => rt.status = ConnStatus::Failed(e.to_string()),
                    }
                }
                if reconnect {
                    this.connect_with(id, false, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_connection(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rt) = self.conns.get_mut(&id) else { return };
        let ConnStatus::Connected { session } = &rt.status else {
            return;
        };
        let session = session.clone();
        let expanded: Vec<String> = rt.expanded_schemas.iter().cloned().collect();
        cx.spawn_in(window, async move |this, cx| {
            let schemas = catalog::list_schemas(&session).await;
            let all = catalog::list_all_relations(&session).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(rt) = this.conns.get_mut(&id) {
                    if let Ok(s) = schemas {
                        rt.schemas = s;
                    }
                    if let Ok(a) = all {
                        rt.all_relations = Arc::new(a);
                        this.side.generation += 1;
                    }
                    rt.relations.clear();
                }
                for schema in expanded {
                    this.load_relations(id, schema, window, cx);
                }
                // Tabs get the new relation list (completion, estimates).
                this.push_targets(id, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn add_connection(&mut self, _: &AddConnection, window: &mut Window, cx: &mut Context<Self>) {
        self.open_connection_form(ConnectionConfig::new(""), true, window, cx);
    }

    /// "Open config file…": a `baselynx.yaml` becomes a project (or updates the one opened
    /// from it before), which then opens.
    fn open_config(&mut self, _: &OpenConfig, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                this.guard_then(window, cx, move |this, window, cx| {
                    this.save_now(cx);
                    match open_config_project(&this.store, &path) {
                        Ok((project, report)) => {
                            for t in &this.tabs {
                                t.close(cx);
                            }
                            this.save_config_passwords(&report, cx);
                            let name = project.name.clone();
                            this.load_project(project, window, cx);
                            window.push_notification(
                                Notification::success(format!(
                                    "Opened {name} from {}: {} added, {} updated, {} removed",
                                    path.display(),
                                    report.added,
                                    report.updated,
                                    report.removed
                                )),
                                cx,
                            );
                        }
                        Err(e) => window.push_notification(Notification::error(e.to_string()).autohide(false), cx),
                    }
                });
            });
        })
        .detach();
    }

    /// Passwords written in the config file go to the keyring, never into the project.
    /// Passwords written in the config file stay in memory: the file is where they live.
    fn save_config_passwords(&mut self, report: &config::SyncReport, cx: &mut Context<Self>) {
        for (id, pw) in &report.passwords {
            self.config_passwords.insert(*id, pw.clone());
            if let Some(rt) = self.conns.get_mut(id) {
                rt.password = Some(pw.clone());
            }
        }
        cx.notify();
    }

    /// Flips a connection between read-write and read-only and reconnects it (3.8).
    fn set_read_only(&mut self, id: Uuid, read_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.project.connections.iter_mut().find(|c| c.id == id) else {
            return;
        };
        if c.read_only == read_only {
            return;
        }
        c.read_only = read_only;
        self.save_now(cx);
        // Reconnect in place: tabs stay open and take the new mode with their next session.
        if let Some(rt) = self.conns.get_mut(&id) {
            rt.status = ConnStatus::Disconnected;
        }
        self.connect(id, window, cx);
        cx.notify();
    }

    /// "Open read-only": a read-only twin beside the connection, which keeps its mode.
    fn open_read_only(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = self.project.connection(id).cloned() else {
            return;
        };
        let same = |c: &ConnectionConfig| {
            c.read_only
                && c.host == base.host
                && c.port == base.port
                && c.database == base.database
                && c.user == base.user
                && c.kube == base.kube
        };
        if let Some(twin) = self.project.connections.iter().find(|c| same(c)).map(|c| c.id) {
            self.connect(twin, window, cx);
            self.side_reveal(twin, cx);
            return;
        }
        let mut twin = base.clone();
        twin.id = Uuid::new_v4();
        twin.name = format!("{} (read-only)", base.name);
        twin.read_only = true;
        // The password typed for the original serves the twin for this session only.
        twin.save_password = false;
        let password = self.conns.get(&id).and_then(|r| r.password.clone());
        let at = self
            .project
            .connections
            .iter()
            .position(|c| c.id == id)
            .map_or(self.project.connections.len(), |p| p + 1);
        let twin_id = twin.id;
        self.project.connections.insert(at, twin);
        self.conns.entry(twin_id).or_default().password = password;
        self.save_now(cx);
        self.connect(twin_id, window, cx);
        self.side_reveal(twin_id, cx);
        cx.notify();
    }

    fn open_connection_form(
        &mut self,
        base: ConnectionConfig,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let title = if is_new { "Add connection" } else { "Edit connection" };
        let id = base.id;
        ConnectionForm::open(title, base, None, window, cx, move |r, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                if let Some(pw) = r.password.clone() {
                    if let Some(rt) = this.conns.get_mut(&id) {
                        rt.password = Some(pw.clone());
                    }
                    if r.config.save_password {
                        let secrets = this.secrets.clone();
                        cx.background_spawn(runtime::spawn_blocking(move || secrets.set(id, &pw)))
                            .detach();
                    }
                }
                if !r.config.save_password {
                    let secrets = this.secrets.clone();
                    cx.background_spawn(runtime::spawn_blocking(move || secrets.delete(id)))
                        .detach();
                }
                match this.project.connections.iter_mut().find(|c| c.id == id) {
                    Some(c) => *c = r.config.clone(),
                    None => {
                        this.project.connections.push(r.config.clone());
                        this.conns.entry(id).or_default().password = r.password.clone();
                    }
                }
                // Tabs show the new name and colour.
                if let Some(target) = this.target(id) {
                    for t in this.tabs.clone() {
                        if t.connection(cx) == id {
                            t.set_target(target.clone(), window, cx);
                        }
                    }
                }
                this.save_now(cx);
                if is_new {
                    this.connect(id, window, cx);
                }
                cx.notify();
            });
        });
    }

    fn remove_connection(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.project.connection(id).map(|c| c.name.clone()) else {
            return;
        };
        let weak = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let weak = weak.clone();
            alert
                .title(format!("Remove connection “{name}”?"))
                .description("Its tabs close and its saved password is deleted. The database is not touched.")
                .confirm()
                .ok_text("Remove connection")
                .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        this.disconnect(id, cx);
                        this.project.connections.retain(|c| c.id != id);
                        this.conns.remove(&id);
                        let secrets = this.secrets.clone();
                        cx.background_spawn(runtime::spawn_blocking(move || secrets.delete(id)))
                            .detach();
                        this.save_now(cx);
                        cx.notify();
                    });
                    true
                })
        });
    }

    // ------------------------------------------------------------------ tabs

    fn make_tab(&mut self, st: TabState, window: &mut Window, cx: &mut Context<Self>) -> TabView {
        let conn = st.connection();
        let config = self
            .project
            .connection(conn)
            .cloned()
            .unwrap_or_else(|| ConnectionConfig::new("?"));
        let label = crate::app_state::ConnLabel::of(&config);
        let target = self.target(conn);
        let tab = match st {
            TabState::Console { id, title, text, .. } => {
                TabView::Console(cx.new(|cx| ConsoleView::new(id, title, label, text, target, window, cx)))
            }
            TabState::Table {
                id,
                schema,
                table,
                filter,
                sort,
                page_size,
                refresh_secs,
                ..
            } => TabView::Table(cx.new(|cx| {
                TableView::new(
                    id,
                    label,
                    target,
                    schema,
                    table,
                    filter,
                    sort,
                    page_size,
                    refresh_secs,
                    window,
                    cx,
                )
            })),
        };
        if let (TabView::Table(v), Some(w)) = (&tab, self.project.record_width) {
            v.update(cx, |v, cx| v.set_record_width(w, cx));
        }
        {
            let weak = cx.weak_entity();
            match &tab {
                TabView::Table(v) => {
                    let (own, schema, table) = {
                        let r = v.read(cx);
                        (r.conn.id, r.schema.clone(), r.table.clone())
                    };
                    let alt: crate::app_state::Alternatives = Rc::new(move |cx| {
                        weak.upgrade()
                            .map(|w| w.read(cx).alternatives(own, Some((&schema, &table))))
                            .unwrap_or_default()
                    });
                    v.update(cx, |v, _| v.set_alternatives(alt));
                }
                TabView::Console(v) => {
                    let alt: crate::app_state::Alternatives = Rc::new({
                        let v = v.downgrade();
                        move |cx| {
                            let own = v.upgrade().map(|v| v.read(cx).conn.id).unwrap_or_default();
                            weak.upgrade()
                                .map(|w| w.read(cx).alternatives(own, None))
                                .unwrap_or_default()
                        }
                    });
                    v.update(cx, |v, _| v.set_alternatives(alt));
                }
            }
        }
        if let TabView::Table(v) = &tab {
            let key = {
                let r = v.read(cx);
                format!("{}/{}.{}", r.conn.id, r.schema, r.table)
            };
            let list = self.project.filter_history.get(&key).cloned().unwrap_or_default();
            v.update(cx, |v, _| v.set_filter_history(list));
        }
        let sub = match &tab {
            TabView::Console(v) => cx.subscribe_in(v, window, |this, v, ev: &TabEvent, window, cx| {
                let conn = v.read(cx).conn.id;
                this.on_tab_event(conn, ev, window, cx)
            }),
            TabView::Table(v) => cx.subscribe_in(v, window, |this, v, ev: &TabEvent, window, cx| {
                let conn = v.read(cx).conn.id;
                this.on_tab_event(conn, ev, window, cx)
            }),
        };
        self.tab_subs.insert(tab.id(cx), sub);
        tab
    }

    fn on_tab_event(&mut self, conn: Uuid, ev: &TabEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            TabEvent::StateChanged => self.schedule_save(cx),
            TabEvent::Connect => {
                if let Some(rt) = self.conns.get_mut(&conn)
                    && matches!(rt.status, ConnStatus::Failed(_))
                {
                    rt.status = ConnStatus::Disconnected;
                }
                self.connect(conn, window, cx);
            }
            TabEvent::OpenTable { schema, table, filter } => {
                let to = Loc {
                    conn,
                    schema: schema.clone(),
                    table: table.clone(),
                    filter: filter.clone(),
                };
                self.travel(to, window, cx);
            }
            TabEvent::OpenElsewhere { conn: to, replace } => self.open_elsewhere(*to, *replace, window, cx),
            TabEvent::MoveConsole { conn: to } => self.move_console(*to, window, cx),
            TabEvent::RecordWidth(w) => {
                self.project.record_width = Some(*w);
                for t in &self.tabs {
                    if let TabView::Table(v) = t {
                        v.update(cx, |v, cx| v.set_record_width(*w, cx));
                    }
                }
                self.schedule_save(cx);
            }
            TabEvent::FilterUsed { schema, table, filter } => {
                let list = self
                    .project
                    .filter_history
                    .entry(format!("{conn}/{schema}.{table}"))
                    .or_default();
                list.retain(|f| f != filter);
                list.push(filter.clone());
                if list.len() > 20 {
                    list.remove(0);
                }
                let list = list.clone();
                for t in self.tabs.clone() {
                    if let TabView::Table(v) = t {
                        let same = {
                            let r = v.read(cx);
                            r.conn.id == conn && &r.schema == schema && &r.table == table
                        };
                        if same {
                            v.update(cx, |v, _| v.set_filter_history(list.clone()));
                        }
                    }
                }
                self.schedule_save(cx);
            }
            TabEvent::Ran { text, rows, duration } => {
                let entry = baselynx_core::history::HistoryEntry {
                    text: text.clone(),
                    connection: conn,
                    at: crate::value_fmt::now_epoch(),
                    duration_ms: duration.as_millis() as u64,
                    rows: *rows,
                };
                baselynx_core::history::push(&mut self.history, entry);
                self.save_history_soon(cx);
            }
            TabEvent::Updated => {}
        }
        cx.notify();
    }

    /// Writes the history a moment later, so a script of many statements writes once.
    fn save_history_soon(&mut self, cx: &mut Context<Self>) {
        self._history_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let _ = this.update(cx, |this, _| {
                let (store, id, entries) = (this.store.clone(), this.project.id, this.history.clone());
                std::thread::spawn(move || {
                    if let Err(e) = baselynx_core::history::save(&store, id, &entries) {
                        eprintln!("{APP_NAME}: saving the query history failed: {e}");
                    }
                });
            });
        }));
    }

    /// Other connections for the switcher in a tab's toolbar (3.5): for a table, those
    /// that have the same `schema.table`; for a console, every connection.
    fn alternatives(&self, own: Uuid, table: Option<(&str, &str)>) -> Vec<(crate::app_state::ConnLabel, bool)> {
        self.project
            .connections
            .iter()
            .filter(|c| c.id != own)
            .filter_map(|c| {
                let rt = self.conns.get(&c.id);
                let connected = matches!(rt.map(|r| &r.status), Some(ConnStatus::Connected { .. }));
                let has = match table {
                    None => true,
                    Some((s, t)) => {
                        connected && rt.is_some_and(|r| r.all_relations.iter().any(|x| x.schema == s && x.name == t))
                    }
                };
                has.then(|| (crate::app_state::ConnLabel::of(c), connected))
            })
            .collect()
    }

    fn open_elsewhere(&mut self, to: Uuid, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.active else { return };
        let Some(TabView::Table(v)) = self.tabs.get(ix).cloned() else {
            return;
        };
        let TabState::Table {
            schema,
            table,
            filter,
            sort,
            page_size,
            refresh_secs,
            ..
        } = v.read(cx).state()
        else {
            return;
        };
        // Already open there: show it with this filter.
        if let Some(j) = self.tabs.iter().position(|t| match t {
            TabView::Table(o) => {
                let o = o.read(cx);
                o.conn.id == to && o.schema == schema && o.table == table
            }
            _ => false,
        }) {
            self.activate(j, window, cx);
            if let TabView::Table(o) = self.tabs[j].clone() {
                o.update(cx, |o, cx| o.show_filter(filter, window, cx));
            }
            return;
        }
        let st = TabState::Table {
            id: Uuid::new_v4(),
            connection: to,
            schema,
            table,
            filter,
            sort,
            page_size,
            refresh_secs,
        };
        let tab = self.make_tab(st, window, cx);
        // Shift+click replaces the tab, unless it holds edits.
        if replace && !v.read(cx).is_dirty(cx) {
            self.close_tab_now(ix, cx);
            let at = ix.min(self.tabs.len());
            self.tabs.insert(at, tab);
            self.activate(at, window, cx);
        } else {
            self.tabs.push(tab);
            let at = self.tabs.len() - 1;
            self.activate(at, window, cx);
        }
        self.schedule_save(cx);
    }

    fn move_console(&mut self, to: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TabView::Console(v)) = self.active.and_then(|i| self.tabs.get(i)).cloned() else {
            return;
        };
        let Some(config) = self.project.connection(to).cloned() else {
            return;
        };
        let label = crate::app_state::ConnLabel::of(&config);
        let target = self.target(to);
        v.update(cx, |v, cx| v.move_to(label, target, cx));
        if self.target(to).is_none() {
            self.connect(to, window, cx);
        }
        self.schedule_save(cx);
        cx.notify();
    }

    /// Where the active table tab stands, for the trail.
    fn here(&self, cx: &App) -> Option<Loc> {
        match self.active.and_then(|i| self.tabs.get(i))? {
            TabView::Table(v) => {
                let v = v.read(cx);
                Some(Loc {
                    conn: v.conn.id,
                    schema: v.schema.clone(),
                    table: v.table.clone(),
                    filter: v.current_filter(),
                })
            }
            TabView::Console(_) => None,
        }
    }

    /// Follows a key to another table, leaving a trail to walk back.
    fn travel(&mut self, to: Loc, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(here) = self.here(cx) {
            if self.trail.get(self.trail_pos) != Some(&here) {
                self.trail.truncate(self.trail_pos + 1);
                if self.trail.last() != Some(&here) {
                    self.trail.push(here);
                }
            } else {
                self.trail.truncate(self.trail_pos + 1);
            }
        }
        self.trail.push(to.clone());
        self.trail_pos = self.trail.len() - 1;
        self.go_to(&to, window, cx);
    }

    fn walk_trail(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let next = if back {
            self.trail_pos.checked_sub(1)
        } else {
            Some(self.trail_pos + 1).filter(|p| *p < self.trail.len())
        };
        let Some(pos) = next else { return };
        self.trail_pos = pos;
        let loc = self.trail[pos].clone();
        self.go_to(&loc, window, cx);
    }

    fn go_to(&mut self, loc: &Loc, window: &mut Window, cx: &mut Context<Self>) {
        self.open_table_with(loc.conn, &loc.schema, &loc.table, Some(loc.filter.clone()), window, cx);
    }

    /// Tabs without a session show where their connection stands.
    fn sync_tab_phases(&self, cx: &mut App) {
        for t in &self.tabs {
            if let TabView::Table(v) = t {
                if v.read(cx).has_target() {
                    continue;
                }
                let conn = v.read(cx).conn.id;
                let phase = match self.conns.get(&conn).map(|r| &r.status) {
                    Some(ConnStatus::Connecting) | Some(ConnStatus::Connected { .. }) => {
                        crate::app_state::ConnPhase::Connecting
                    }
                    Some(ConnStatus::Failed(e)) => crate::app_state::ConnPhase::Failed(e.clone().into()),
                    _ => crate::app_state::ConnPhase::Disconnected,
                };
                v.update(cx, |v, cx| v.set_phase(phase, cx));
            }
        }
    }

    pub fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if let Some(old) = self.active.and_then(|i| self.tabs.get(i)).cloned()
            && self.active != Some(ix)
        {
            old.set_active(false, window, cx);
        }
        self.active = Some(ix);
        let tab = self.tabs[ix].clone();
        // Focus first: connecting may open the password prompt, which must keep focus.
        tab.focus(window, cx);
        let conn = tab.connection(cx);
        if !tab.has_target(cx) {
            match self.conns.get(&conn).map(|r| &r.status) {
                Some(ConnStatus::Connected { .. }) => {
                    if let Some(t) = self.target(conn) {
                        tab.set_target(t, window, cx);
                    }
                }
                Some(ConnStatus::Disconnected) => self.connect(conn, window, cx),
                _ => {}
            }
        }
        tab.set_active(true, window, cx);
        self.schedule_save(cx);
        cx.notify();
    }

    fn open_table(&mut self, conn: Uuid, schema: &str, table: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_table_with(conn, schema, table, None, window, cx);
    }

    /// Opens a table, or activates its tab; with `Some(filter)` the tab shows that filter.
    fn open_table_with(
        &mut self,
        conn: Uuid,
        schema: &str,
        table: &str,
        filter: Option<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.tabs.iter().position(|t| match t {
            TabView::Table(v) => {
                let v = v.read(cx);
                v.conn.id == conn && v.schema == schema && v.table == table
            }
            _ => false,
        }) {
            self.activate(ix, window, cx);
            if let (Some(f), TabView::Table(v)) = (filter, self.tabs[ix].clone()) {
                v.update(cx, |v, cx| v.show_filter(f, window, cx));
            }
            return;
        }
        self.recent
            .retain(|(c, s, t)| !(*c == conn && s == schema && t == table));
        self.recent.insert(0, (conn, schema.to_string(), table.to_string()));
        self.recent.truncate(10);
        let st = TabState::Table {
            id: Uuid::new_v4(),
            connection: conn,
            schema: schema.into(),
            table: table.into(),
            filter: filter.flatten(),
            sort: None,
            page_size: DEFAULT_PAGE_SIZE,
            refresh_secs: None,
        };
        let tab = self.make_tab(st, window, cx);
        self.tabs.push(tab);
        let ix = self.tabs.len() - 1;
        self.activate(ix, window, cx);
        self.schedule_save(cx);
    }

    fn open_console(&mut self, conn: Uuid, text: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.console_counter += 1;
        let st = TabState::Console {
            id: Uuid::new_v4(),
            connection: conn,
            title: format!("console {}", self.console_counter),
            text: text.unwrap_or_default(),
        };
        let tab = self.make_tab(st, window, cx);
        self.tabs.push(tab);
        let ix = self.tabs.len() - 1;
        self.activate(ix, window, cx);
        self.schedule_save(cx);
    }

    /// The connection of the current tab, or the only / first connected one.
    fn current_connection(&self, cx: &App) -> Option<Uuid> {
        if let Some(t) = self.active.and_then(|i| self.tabs.get(i)) {
            return Some(t.connection(cx));
        }
        self.project
            .connections
            .iter()
            .find(|c| {
                matches!(
                    self.conns.get(&c.id).map(|r| &r.status),
                    Some(ConnStatus::Connected { .. })
                )
            })
            .or(self.project.connections.first())
            .map(|c| c.id)
    }

    fn new_console(&mut self, _: &NewConsole, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.current_connection(cx) else {
            self.add_connection(&AddConnection, window, cx);
            return;
        };
        self.open_console(conn, None, window, cx);
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix).cloned() else { return };
        if tab.is_dirty(cx) {
            let weak = cx.weak_entity();
            let title = format!("Close {} with uncommitted changes?", tab.title(cx));
            window.open_alert_dialog(cx, move |alert, _, _| {
                let weak = weak.clone();
                alert
                    .title(title.clone())
                    .description("The changes are discarded.")
                    .confirm()
                    .ok_text("Discard and close")
                    .ok_variant(gpui_kit::component::button::ButtonVariant::Danger)
                    .on_ok(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.close_tab_now(ix, cx);
                            match this.active {
                                Some(a) => this.activate(a, window, cx),
                                None => this.focus_handle.focus(window, cx),
                            }
                        });
                        true
                    })
            });
            return;
        }
        self.close_tab_now(ix, cx);
        if let Some(a) = self.active {
            self.activate(a, window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
    }

    fn close_tab_now(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ix);
        self.closed.push(tab.state(cx));
        if self.closed.len() > tab_menu::CLOSED_KEPT {
            self.closed.remove(0);
        }
        tab.close(cx);
        self.tab_subs.remove(&tab.id(cx));
        self.active = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(a) if a > ix => Some(a - 1),
            Some(a) if a == ix => Some(a.min(self.tabs.len() - 1)),
            other => other,
        };
        self.schedule_save(cx);
        cx.notify();
    }

    fn activate_nth(&mut self, n: usize, window: &mut Window, cx: &mut Context<Self>) {
        if n < self.tabs.len() {
            self.activate(n, window, cx);
        }
    }

    // ------------------------------------------------------------------ palette

    fn open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        // A table found in several connections is listed once (3.5): Enter opens the most
        // recently used connection, Tab cycles.
        let mut grouped: Vec<(String, String, RelKind, Vec<crate::ui::palette::RelTarget>)> = Vec::new();
        let mut at: HashMap<(String, String), usize> = HashMap::new();
        for c in &self.project.connections {
            if let Some(rt) = self.conns.get(&c.id) {
                for r in rt.all_relations.iter() {
                    let target = crate::ui::palette::RelTarget {
                        conn: c.id,
                        conn_name: c.name.clone(),
                        color: c.color,
                        key: r.primary_key.clone(),
                    };
                    match at.get(&(r.schema.clone(), r.name.clone())) {
                        Some(&i) => grouped[i].3.push(target),
                        None => {
                            at.insert((r.schema.clone(), r.name.clone()), grouped.len());
                            grouped.push((r.schema.clone(), r.name.clone(), r.kind, vec![target]));
                        }
                    }
                }
            }
        }
        let mut entries = Vec::new();
        for (schema, name, kind, targets) in grouped {
            let first = self
                .recent
                .iter()
                .find_map(|(c, s, t)| (*s == schema && *t == name).then_some(*c))
                .and_then(|c| targets.iter().position(|t| t.conn == c))
                .unwrap_or(0);
            let t = targets[first].clone();
            entries.push(PaletteItem::Relation {
                conn: t.conn,
                conn_name: t.conn_name,
                color: t.color,
                schema,
                name,
                kind,
                key: t.key,
                targets: Rc::new(targets),
            });
        }
        for (index, t) in self.tabs.iter().enumerate() {
            entries.push(PaletteItem::Tab {
                index,
                title: t.tooltip(cx),
            });
        }
        let commands: [(&str, ActionFactory); 22] = [
            ("New console", || Box::new(NewConsole)),
            ("Add connection", || Box::new(AddConnection)),
            ("Open config file", || Box::new(OpenConfig)),
            ("Edit config file", || Box::new(EditConfig)),
            ("Backups", || Box::new(ShowBackups)),
            ("Refresh", || Box::new(Refresh)),
            ("Add row", || Box::new(AddRow)),
            ("Delete row", || Box::new(DeleteRow)),
            ("Commit changes", || Box::new(CommitChanges)),
            ("Discard changes", || Box::new(DiscardChanges)),
            ("Export", || Box::new(ExportData)),
            ("Toggle sidebar", || Box::new(ToggleSidebar)),
            ("Reopen closed tab", || Box::new(ReopenClosedTab)),
            ("Close tab", || Box::new(CloseTab)),
            ("New project", || Box::new(NewProject)),
            ("Count rows", || Box::new(CountRows)),
            ("Zoom in", || Box::new(ZoomIn)),
            ("Zoom out", || Box::new(ZoomOut)),
            ("Reset zoom", || Box::new(ResetZoom)),
            ("Light theme", || Box::new(ThemeLight)),
            ("Dark theme", || Box::new(ThemeDark)),
            ("Theme follows the system", || Box::new(ThemeSystem)),
        ];
        let first_command = entries.len();
        for (label, action) in commands {
            entries.push(PaletteItem::Command {
                label: label.into(),
                action,
            });
        }
        // Statements run before, newest first: `>` searches them (4.10).
        let first_query = entries.len();
        for e in self.history.iter().rev() {
            let conn_name = self
                .project
                .connection(e.connection)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            entries.push(PaletteItem::Query {
                conn: e.connection,
                conn_name,
                text: e.text.clone(),
            });
        }
        let history_end = entries.len();
        let queries_shown = (history_end - first_query).min(5);
        for p in &self.projects {
            if p.id != self.project.id {
                entries.push(PaletteItem::Project {
                    id: p.id,
                    name: p.name.clone(),
                });
            }
        }
        for c in &self.project.connections {
            if !matches!(
                self.conns.get(&c.id).map(|r| &r.status),
                Some(ConnStatus::Connected { .. })
            ) {
                entries.push(PaletteItem::Connect {
                    conn: c.id,
                    name: c.name.clone(),
                });
            }
        }
        // Empty query: ten most recent tables, then commands.
        let mut default_order: Vec<usize> = self
            .recent
            .iter()
            .filter_map(|(c, s, t)| {
                entries.iter().position(|e| {
                    let _ = c;
                    matches!(e, PaletteItem::Relation { schema, name, .. } if schema == s && name == t)
                })
            })
            .collect();
        default_order.extend(first_command..first_query);
        // The latest queries follow the commands, under "Recent queries".
        default_order.extend(first_query..first_query + queries_shown);
        default_order.extend(history_end..entries.len());
        let weak = cx.weak_entity();
        Palette::open(
            entries,
            default_order,
            Rc::new(move |item, secondary, window, cx| {
                let _ = weak.update(cx, |this, cx| this.pick(item, secondary, window, cx));
            }),
            window,
            cx,
        );
    }

    fn pick(&mut self, item: PaletteItem, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            PaletteItem::Relation { conn, schema, name, .. } => {
                if secondary {
                    let d = self
                        .project
                        .connection(conn)
                        .map(|c| c.engine.dialect())
                        .unwrap_or_default();
                    let sql = format!("SELECT * FROM {} LIMIT 100;", d.qualified(&schema, &name));
                    self.open_console(conn, Some(sql), window, cx);
                } else {
                    self.open_table(conn, &schema, &name, window, cx);
                }
            }
            PaletteItem::Row {
                conn,
                schema,
                name,
                key,
                values,
                ..
            } => {
                let values: Vec<Option<String>> = values.into_iter().map(Some).collect();
                let d = self
                    .project
                    .connection(conn)
                    .map(|c| c.engine.dialect())
                    .unwrap_or_default();
                let filter = baselynx_core::table::key_filter(&key, &values, d);
                let to = Loc {
                    conn,
                    schema,
                    table: name,
                    filter,
                };
                self.travel(to, window, cx);
            }
            PaletteItem::Query { conn, text, .. } => {
                let conn = if self.project.connection(conn).is_some() {
                    Some(conn)
                } else {
                    self.current_connection(cx)
                };
                if let Some(conn) = conn {
                    self.open_console(conn, Some(text), window, cx);
                }
            }
            PaletteItem::Tab { index, .. } => self.activate(index, window, cx),
            PaletteItem::Command { action, .. } => {
                let action = action();
                // Commands are dispatched from the focused element; with no tab open, the workspace.
                match self.active.and_then(|i| self.tabs.get(i)).cloned() {
                    Some(t) => t.focus(window, cx),
                    None => self.focus_handle.focus(window, cx),
                }
                window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
            }
            PaletteItem::Connect { conn, .. } => self.connect(conn, window, cx),
            PaletteItem::Project { id, .. } => self.switch_project(id, window, cx),
        }
    }

    // ------------------------------------------------------------------ rendering

    /// A `postgres://` URL pasted while the sidebar has focus opens the form filled in.
    fn paste_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        let Some(url) = parse_connection_url(&text) else { return };
        let mut base = ConnectionConfig::new("");
        if let Some(h) = url.host.clone() {
            base.host = h;
        }
        if let Some(p) = url.port {
            base.port = p;
        }
        if let Some(d) = url.database.clone() {
            base.database = d;
        }
        if let Some(u) = url.user.clone() {
            base.user = u;
        }
        if let Some(m) = url.ssl_mode {
            base.ssl_mode = m;
        }
        base.name = format!("{} @ {}", base.database, base.host);
        let weak = cx.weak_entity();
        let id = base.id;
        let password = url.password.clone();
        ConnectionForm::open("Add connection", base, password, window, cx, move |r, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                if let (Some(pw), true) = (r.password.clone(), r.config.save_password) {
                    let secrets = this.secrets.clone();
                    cx.background_spawn(runtime::spawn_blocking(move || secrets.set(id, &pw)))
                        .detach();
                }
                this.project.connections.push(r.config.clone());
                this.conns.entry(id).or_default().password = r.password.clone();
                this.save_now(cx);
                this.connect(id, window, cx);
                cx.notify();
            });
        });
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let projects: Vec<(Uuid, String)> = self.projects.iter().map(|p| (p.id, p.name.clone())).collect();
        let current = self.project.id;
        let row = h_flex()
            .h_full()
            .flex_1()
            .pr_2()
            .gap_2()
            .child(
                Button::new("project-switcher")
                    .label(self.project.name.clone())
                    .small()
                    .ghost()
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, _| {
                        let mut menu = menu;
                        for (id, name) in &projects {
                            let w = weak.clone();
                            let id = *id;
                            menu = menu.item(PopupMenuItem::new(name.clone()).checked(id == current).on_click(
                                move |_, window, cx| {
                                    let _ = w.update(cx, |this, cx| this.switch_project(id, window, cx));
                                },
                            ));
                        }
                        let (w1, w2, w3) = (weak.clone(), weak.clone(), weak.clone());
                        menu.separator()
                            .item(PopupMenuItem::new("New project…").on_click(move |_, window, cx| {
                                let _ = w1.update(cx, |this, cx| this.new_project(window, cx));
                            }))
                            .item(PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                                let _ = w2.update(cx, |this, cx| this.rename_project(window, cx));
                            }))
                            .item(PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
                                let _ = w3.update(cx, |this, cx| this.delete_project(window, cx));
                            }))
                    }),
            )
            .child(self.render_menus(cx))
            .child(div().flex_1())
            .when((tokens::zoom(cx) - 1.0).abs() > 0.001, |d| {
                d.child(
                    Button::new("zoom-reset")
                        .label(format!("{:.0} %", tokens::zoom(cx) * 100.0))
                        .small()
                        .ghost()
                        .tooltip_with_action("Reset zoom", &ResetZoom, None)
                        .on_click(cx.listener(|this, _, _, cx| this.zoom(0, cx))),
                )
            })
            .child(
                Button::new("theme")
                    .icon(match tokens::theme_pref(cx) {
                        tokens::ThemePref::Light => Icon::new(IconName::Sun),
                        tokens::ThemePref::Dark => Icon::new(IconName::Moon),
                        tokens::ThemePref::System => Icon::new(gpui_kit::assets::IconName::SunMoon),
                    })
                    .small()
                    .ghost()
                    .tooltip(format!("{} · click to change", tokens::theme_pref(cx).label()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let next = tokens::theme_pref(cx).next();
                        this.set_theme(next, window, cx);
                    })),
            )
            .child(
                Button::new("palette")
                    .icon(IconName::Search)
                    .label("Go to…")
                    .small()
                    .ghost()
                    .tooltip_with_action("Go to a table or command", &OpenPalette, None)
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(&OpenPalette, window, cx))),
            );
        // The toolkit's title bar puts the window buttons on the same row.
        gpui_kit::component::TitleBar::new()
            .h(tokens::ui(34., cx))
            .pl(tokens::ui(6., cx))
            .child(row)
    }

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        h_flex()
            .id("tab-strip")
            .h_8()
            .flex_shrink_0()
            .overflow_x_scroll()
            .bg(cx.theme().tab_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .children(self.tabs.iter().enumerate().map(|(ix, t)| {
                let is_active = active == Some(ix);
                let conn = self.project.connection(t.connection(cx));
                let color = conn.and_then(|c| tokens::connection(c.color, cx));
                let dirty = t.is_dirty(cx);
                let busy = t.is_busy(cx);
                let tip = t.tooltip(cx);
                // Active: the window background with an accent underline. The connection colour
                // is a dot before the title, so production tabs do not look like errors (4.3).
                h_flex()
                    .id(("tab", ix))
                    .relative()
                    .h_full()
                    .px_3()
                    .gap_1p5()
                    .text_sm()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .when(is_active, |d| {
                        d.bg(cx.theme().background)
                            .text_color(cx.theme().tab_active_foreground)
                            .child(
                                div()
                                    .absolute()
                                    .bottom_0()
                                    .left_0()
                                    .right_0()
                                    .h(px(2.))
                                    .bg(cx.theme().primary),
                            )
                    })
                    .when(!is_active, |d| {
                        d.text_color(cx.theme().tab_foreground)
                            .hover(|d| d.bg(cx.theme().tab_active.opacity(0.5)))
                    })
                    .when_some(color, |d, c| d.child(div().size_1p5().rounded_full().bg(c)))
                    .cursor_pointer()
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| this.close_tab(ix, window, cx)),
                    )
                    .context_menu({
                        let weak = cx.weak_entity();
                        move |menu: PopupMenu, _, cx| tab_menu::tab_menu(menu, weak.clone(), ix, cx)
                    })
                    .child(match t {
                        TabView::Console(_) => Icon::new(IconName::SquareTerminal).xsmall(),
                        TabView::Table(_) => Icon::new(gpui_kit::assets::IconName::Table).xsmall(),
                    })
                    .when(conn.is_some_and(|c| c.read_only), |d| {
                        d.child(
                            Icon::new(gpui_kit::assets::IconName::Lock)
                                .xsmall()
                                .text_color(tokens::secondary_text(cx)),
                        )
                    })
                    .child(t.title(cx))
                    .when(dirty, |d| d.child(div().text_color(tokens::changed(cx)).child("●")))
                    .when(busy && !dirty, |d| {
                        d.child(div().text_color(tokens::secondary_text(cx)).child("…"))
                    })
                    .child(
                        Button::new(("close-tab", ix))
                            .icon(IconName::Close)
                            .xsmall()
                            .ghost()
                            .tooltip_with_action("Close tab", &CloseTab, None)
                            .on_click(cx.listener(move |this, _, window, cx| this.close_tab(ix, window, cx))),
                    )
            }))
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let line = |keys: &'static str, text: &'static str| {
            h_flex()
                .gap_3()
                .child(
                    div()
                        .w_20()
                        .text_right()
                        .font_family(cx.theme().mono_font_family.clone())
                        .child(keys),
                )
                .child(div().child(text))
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_color(tokens::secondary_text(cx))
            .child(
                v_flex()
                    .gap_2()
                    .items_start()
                    .child(h_flex().w_full().justify_center().text_xl().pb_2().child(APP_NAME))
                    .child(line("Ctrl+K", "go to a table or command"))
                    .child(line("Ctrl+T", "new console"))
                    .child(line("", "double-click a table to open it"))
                    .child(line("Ctrl+B", "toggle the sidebar")),
            )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active.and_then(|i| self.tabs.get(i));
        let status = tab.map(|t| t.status(cx)).unwrap_or_default();
        let conn = tab.and_then(|t| self.project.connection(t.connection(cx)));
        let version = tab
            .and_then(|t| self.conns.get(&t.connection(cx)))
            .and_then(|rt| match &rt.status {
                ConnStatus::Connected { session } => {
                    Some(format!("{} {}", session.engine().label(), session.server_version()))
                }
                _ => None,
            });
        let sep = || div().text_color(tokens::secondary_text(cx)).child("·");
        h_flex()
            .h_6()
            .px_2()
            .gap_2()
            .flex_shrink_0()
            .text_xs()
            .bg(cx.theme().status_bar)
            .border_t_1()
            .border_color(cx.theme().status_bar_border)
            .when_some(conn, |d, c| {
                d.child(
                    h_flex()
                        .gap_1()
                        .when_some(tokens::connection(c.color, cx), |d, col| {
                            d.child(div().size_2().rounded_full().bg(col))
                        })
                        .child(c.name.clone()),
                )
            })
            .when_some(status.rows.clone(), |d, r| d.child(sep()).child(r))
            .when_some(status.timing.clone(), |d, t| d.child(sep()).child(t))
            .when_some(status.note.clone(), |d, n| {
                d.child(sep())
                    .child(div().text_color(tokens::secondary_text(cx)).child(n))
            })
            .when_some(status.error.clone(), |d, e| {
                d.child(sep()).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(tokens::error_text(cx))
                        .child(e),
                )
            })
            .child(div().flex_1())
            .when_some(self.batch_progress(), |d, (done, total, ok, failed)| {
                if done < total {
                    d.child(format!("Connecting {done} of {total}"))
                } else if failed == 0 {
                    d.child(format!("{ok} connected"))
                } else {
                    d.child(format!("{ok} connected,")).child(
                        div()
                            .id("batch-failed")
                            .text_color(tokens::error_text(cx))
                            .cursor_pointer()
                            .hover(|d| d.underline())
                            .child(format!("{failed} failed"))
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_failed(cx))),
                    )
                }
            })
            .when_some(self.backup_progress(), |d, text| {
                d.child(
                    div()
                        .id("backup-progress")
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .child(text)
                        .on_click(cx.listener(|this, _, window, cx| this.show_backups(window, cx))),
                )
            })
            .when(self.failed_backups() > 0, |d| {
                let n = self.failed_backups();
                d.child(
                    div()
                        .id("backup-failed")
                        .cursor_pointer()
                        .text_color(tokens::error_text(cx))
                        .hover(|d| d.underline())
                        .child(format!("{n} backup{} failed", if n == 1 { "" } else { "s" }))
                        .on_click(cx.listener(|this, _, window, cx| this.show_backups(window, cx))),
                )
            })
            .when_some(status.export.clone(), |d, ex| {
                let cancel = ex.cancel.clone();
                d.child(format!("Exporting {}", format::rows(ex.rows))).child(
                    Button::new("cancel-export")
                        .label("Cancel")
                        .xsmall()
                        .ghost()
                        .on_click(move |_, _, _| cancel.cancel()),
                )
            })
            .when_some(version, |d, v| {
                d.child(div().text_color(tokens::secondary_text(cx)).child(v))
            })
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_tab_phases(cx);
        // Nothing focused (a menu closed over a removed element): shortcuts need a focused
        // element to dispatch from, so the active tab takes it back.
        if window.focused(cx).is_none() && !window.has_active_dialog(cx) {
            cx.defer_in(window, |this, window, cx| {
                if window.focused(cx).is_none() {
                    match this.active.and_then(|i| this.tabs.get(i)).cloned() {
                        Some(t) => t.focus(window, cx),
                        None => this.focus_handle.focus(window, cx),
                    }
                }
            });
        }
        let main: AnyElement = match self.active.and_then(|i| self.tabs.get(i)) {
            Some(t) => v_flex()
                .size_full()
                .child(self.render_tab_strip(cx))
                .child(div().flex_1().min_h_0().child(t.element()))
                .into_any_element(),
            None if !self.tabs.is_empty() => v_flex()
                .size_full()
                .child(self.render_tab_strip(cx))
                .child(self.render_empty(cx))
                .into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };
        let content: AnyElement = if self.sidebar_visible {
            h_resizable("workspace-split")
                .child(
                    resizable_panel()
                        .size(px(260.))
                        .size_range(px(180.)..px(520.))
                        .child(self.render_sidebar(window, cx)),
                )
                .child(resizable_panel().child(main))
                .into_any_element()
        } else {
            main
        };
        v_flex()
            .key_context(WORKSPACE)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::open_palette))
            .on_action(cx.listener(Self::new_console))
            .on_action(cx.listener(Self::add_connection))
            .on_action(cx.listener(Self::open_config))
            .on_action(cx.listener(|this, _: &EditConfig, window, cx| this.edit_config(window, cx)))
            .on_action(cx.listener(|this, _: &ShowBackups, window, cx| this.show_backups(window, cx)))
            .on_action(cx.listener(|this, _: &NewProject, window, cx| this.new_project(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, window, cx| {
                // Visible but not focused: focus it. Otherwise show or hide it (3.7).
                if this.sidebar_visible && !this.sidebar_focus.contains_focused(window, cx) {
                    this.sidebar_focus.focus(window, cx);
                } else {
                    this.sidebar_visible = !this.sidebar_visible;
                    if this.sidebar_visible {
                        this.sidebar_focus.focus(window, cx);
                    } else if let Some(t) = this.active.and_then(|i| this.tabs.get(i)).cloned() {
                        t.focus(window, cx);
                    } else {
                        this.focus_handle.focus(window, cx);
                    }
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusSidebarFilter, window, cx| this.focus_side_filter(window, cx)))
            .on_action(cx.listener(|this, _: &TrailBack, window, cx| this.walk_trail(true, window, cx)))
            .on_action(cx.listener(|this, _: &TrailForward, window, cx| this.walk_trail(false, window, cx)))
            .on_action(cx.listener(|this, _: &ShowHideSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(a) = this.active {
                    this.close_tab(a, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                this.reopen_closed_tab(window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                if let Some(a) = this.active {
                    let n = this.tabs.len();
                    this.activate((a + 1) % n, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                if let Some(a) = this.active {
                    let n = this.tabs.len();
                    this.activate((a + n - 1) % n, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ActivateTab1, w, cx| this.activate_nth(0, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab2, w, cx| this.activate_nth(1, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab3, w, cx| this.activate_nth(2, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab4, w, cx| this.activate_nth(3, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab5, w, cx| this.activate_nth(4, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab6, w, cx| this.activate_nth(5, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab7, w, cx| this.activate_nth(6, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab8, w, cx| this.activate_nth(7, w, cx)))
            .on_action(cx.listener(|this, _: &ActivateTab9, w, cx| this.activate_nth(8, w, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1, cx)))
            .on_action(cx.listener(|this, _: &ResetZoom, _, cx| this.zoom(0, cx)))
            .on_action(cx.listener(|this, _: &CycleTheme, window, cx| {
                let next = tokens::theme_pref(cx).next();
                this.set_theme(next, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ThemeLight, w, cx| this.set_theme(tokens::ThemePref::Light, w, cx)))
            .on_action(cx.listener(|this, _: &ThemeDark, w, cx| this.set_theme(tokens::ThemePref::Dark, w, cx)))
            .on_action(cx.listener(|this, _: &ThemeSystem, w, cx| this.set_theme(tokens::ThemePref::System, w, cx)))
            .on_action(cx.listener(|this, _: &Quit, _, cx| {
                this.save_now(cx);
                cx.quit();
            }))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(div().flex_1().min_h_0().child(content))
            .child(self.render_status_bar(cx))
    }
}

/// The sidebar paste binding lives here so the action is local to the sidebar.
pub mod menu_paste {
    use gpui_kit::*;
    actions!(baselynx_sidebar, [PasteUrl]);
    pub const CONTEXT: &str = "PgSidebar";
    pub fn bind(cx: &mut App) {
        let b = crate::actions::key("secondary-v", PasteUrl, Some(CONTEXT), cx);
        cx.bind_keys([b]);
    }
}
