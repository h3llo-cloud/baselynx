//! baselynx's database workspace inside another application.
//!
//! The host gives the connections (with passwords held in memory only) and a folder for tabs,
//! filters and query history. The embedded view has the sidebar of schemas and tables, the tab
//! strip with table and console tabs, the record panel, the review and export dialogs and the
//! status line; it has no title bar, menus, connection form, config file or backup screens, and
//! no way to add, change or remove a connection.
//!
//! ```ignore
//! baselynx_app::embed::init(cx);
//! let ws = cx.new(|cx| DbWorkspace::new(connections, options, window, cx));
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use baselynx_core::model::{ConnectionColor, ConnectionConfig, Project};
use baselynx_core::store::{MemorySecretStore, SecretStore, Store};
use gpui_kit::*;
use uuid::Uuid;

use crate::actions::*;
use crate::ui::workspace::Workspace;

/// The key context of the embedded workspace: its own shortcuts act only while it has the focus.
pub const CONTEXT: &str = "BaselynxEmbed";

/// Registers what the embedded screens need: baselynx's sizes (the host's theme stays), the
/// bindings scoped to baselynx's views, and a few workspace commands under [`CONTEXT`]. The
/// palette, tab, zoom and quit shortcuts are left to the host.
pub fn init(cx: &mut App) {
    crate::ui::tokens::install_embedded(cx);
    let mut b = scoped(cx);
    b.extend([
        key("secondary-t", NewConsole, Some(CONTEXT), cx),
        key("secondary-b", ToggleSidebar, Some(CONTEXT), cx),
        key("alt-[", TrailBack, Some(CONTEXT), cx),
        key("alt-]", TrailForward, Some(CONTEXT), cx),
        key("secondary-shift-k", FocusSidebarFilter, Some(CONTEXT), cx),
    ]);
    cx.bind_keys(b);
}

/// A connection the host supplies. The password goes to an in-memory store and nowhere else.
#[derive(Clone)]
pub struct EmbedConnection {
    pub config: ConnectionConfig,
    pub password: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EmbedOptions {
    /// Where tabs, filters and query history are kept. No secret is written there.
    pub store_root: PathBuf,
    /// Every write is reviewed before it is committed: table edits always are; consoles then run
    /// writes inside a transaction that must be committed (as on a red connection).
    pub review_all_writes: bool,
    /// Every session is read-only, so the server refuses writes.
    pub read_only: bool,
}

/// What the workspace tells its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbedEvent {
    /// A connection could not be opened or was lost; the host may repair the route and call
    /// [`DbWorkspace::set_connection`].
    ConnectionLost { id: Uuid, error: String },
}

/// A table of an open connection, for the host's "Go to…".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub connection: Uuid,
    pub schema: String,
    pub table: String,
    /// `schema.table`
    pub label: String,
    /// The connection's name.
    pub detail: String,
}

/// What the embedded workspace changes in a connection it is given.
pub(crate) fn prepare(mut config: ConnectionConfig, options: &EmbedOptions) -> ConnectionConfig {
    config.save_password = false;
    config.kube = None;
    config.backup = None;
    config.allow_restore = false;
    config.config_key = None;
    if options.read_only {
        config.read_only = true;
    }
    if options.review_all_writes {
        config.color = ConnectionColor::Red;
    }
    config
}

/// The project kept in `store_root`: the one there before (its tabs come back), with exactly the
/// given connections.
fn project_for(store: &Store, connections: Vec<ConnectionConfig>) -> Project {
    let (projects, _) = store.list_projects();
    let mut project = projects.into_iter().next().unwrap_or_else(|| Project::new("embedded"));
    project.connections = connections;
    project
}

pub struct DbWorkspace {
    ws: Entity<Workspace>,
    options: EmbedOptions,
    _sub: Subscription,
}

impl EventEmitter<EmbedEvent> for DbWorkspace {}

impl DbWorkspace {
    pub fn new(
        connections: Vec<EmbedConnection>,
        options: EmbedOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = Store::at(&options.store_root)
            .or_else(|_| Store::at(std::env::temp_dir().join("baselynx-embed")))
            .expect("a folder for the embedded workspace");
        let secrets = Arc::new(MemorySecretStore::default());
        let mut passwords = HashMap::new();
        let mut configs = Vec::new();
        for c in connections {
            if let Some(p) = &c.password {
                let _ = secrets.set(c.config.id, p);
                passwords.insert(c.config.id, p.clone());
            }
            configs.push(prepare(c.config, &options));
        }
        let project = project_for(&store, configs);
        let flags = crate::ui::workspace::EmbedFlags {
            read_only: options.read_only,
        };
        let secrets: Arc<dyn SecretStore> = secrets;
        let ws = cx.new(|cx| Workspace::embedded(store, secrets, project, passwords, flags, window, cx));
        let sub = cx.subscribe(&ws, |_, _, ev: &EmbedEvent, cx| cx.emit(ev.clone()));
        Self { ws, options, _sub: sub }
    }

    pub fn workspace(&self) -> &Entity<Workspace> {
        &self.ws
    }

    /// Replaces host, port or password of a live connection (after a tunnel was reopened); its
    /// tabs reconnect.
    pub fn set_connection(
        &mut self,
        id: Uuid,
        connection: EmbedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut config = prepare(connection.config, &self.options);
        config.id = id;
        let password = connection.password;
        self.ws
            .update(cx, |w, cx| w.embed_set_connection(config, password, window, cx));
    }

    /// Tables of the open connections whose `schema.name` matches `query`, best first.
    pub fn palette_items(&self, query: &str, cx: &App) -> Vec<PaletteItem> {
        self.ws.read(cx).embed_palette_items(query)
    }

    /// Opens a table tab (the host's "Go to…").
    pub fn open_table(
        &mut self,
        connection: Uuid,
        schema: &str,
        table: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (schema, table) = (schema.to_string(), table.to_string());
        self.ws
            .update(cx, |w, cx| w.embed_open_table(connection, &schema, &table, window, cx));
    }
}

impl Render for DbWorkspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.ws.clone())
    }
}
