//! Shared types between the workspace and its tabs.

use baselynx_core::CancellationToken;
use gpui_kit::*;
use std::sync::Arc;

use baselynx_core::catalog::RelationRef;
use baselynx_core::model::{ConnectionColor, ConnectionConfig, Engine};
use uuid::Uuid;

/// What a tab needs to open its own session.
#[derive(Clone)]
pub struct ConnTarget {
    pub config: ConnectionConfig,
    pub password: Option<String>,
    /// The connection's relations, as loaded for the sidebar and the palette.
    pub relations: Arc<Vec<RelationRef>>,
    /// The connection's catalog session, for completion's column lists.
    pub catalog: Option<Arc<baselynx_core::session::Session>>,
}

impl ConnTarget {
    pub fn label(&self) -> ConnLabel {
        ConnLabel::of(&self.config)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnLabel {
    pub id: Uuid,
    pub name: String,
    pub color: ConnectionColor,
    /// Decides how the tab writes SQL (quoting, splitting).
    pub engine: Engine,
}

impl ConnLabel {
    pub fn of(config: &ConnectionConfig) -> Self {
        Self {
            id: config.id,
            name: config.name.clone(),
            color: config.color,
            engine: config.engine,
        }
    }

    pub fn dialect(&self) -> baselynx_core::sql::Dialect {
        self.engine.dialect()
    }
}

/// Emitted by tabs so the workspace saves and re-renders.
#[derive(Clone, Debug)]
pub enum TabEvent {
    /// Persisted state changed (text, filter, page size …).
    StateChanged,
    /// Only what is shown changed (status, dirty flag, busy).
    Updated,
    /// The user asked to connect the tab's connection.
    Connect,
    /// Open a table of the same connection, filtered (following a foreign key).
    OpenTable {
        schema: String,
        table: String,
        filter: Option<String>,
    },
    /// The same table on another connection, with the same filter, sort and page size.
    /// `replace` swaps the current tab instead of opening another.
    OpenElsewhere { conn: Uuid, replace: bool },
    /// Move a console to another connection, keeping its text.
    MoveConsole { conn: Uuid },
    /// The record panel was resized; the width is remembered per project.
    RecordWidth(f32),
    /// A filter was applied on a table tab: for its history.
    FilterUsed {
        schema: String,
        table: String,
        filter: String,
    },
    /// A console ran a statement: for the query history.
    Ran {
        text: String,
        rows: Option<u64>,
        duration: std::time::Duration,
    },
}

/// Other connections a tab could switch to: label and whether it is connected.
pub type Alternatives = std::rc::Rc<dyn Fn(&App) -> Vec<(ConnLabel, bool)>>;

/// Where a tab's connection stands while the tab has no session target yet.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ConnPhase {
    #[default]
    Connecting,
    Disconnected,
    Failed(SharedString),
}

/// What the status bar shows for the active tab.
#[derive(Default, Clone)]
pub struct TabStatus {
    pub rows: Option<SharedString>,
    pub timing: Option<SharedString>,
    pub error: Option<SharedString>,
    pub export: Option<ExportProgress>,
    /// A passing remark: "Reconnected to orders-db".
    pub note: Option<SharedString>,
}

#[derive(Clone)]
pub struct ExportProgress {
    pub rows: u64,
    pub cancel: CancellationToken,
}

/// A background export owned by a tab.
pub struct ExportJob {
    pub rows: u64,
    pub cancel: CancellationToken,
    pub _task: Task<()>,
}

impl ExportJob {
    pub fn progress(&self) -> ExportProgress {
        ExportProgress {
            rows: self.rows,
            cancel: self.cancel.clone(),
        }
    }
}
