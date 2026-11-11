//! Statements run in a project's consoles: text, connection, time,
//! duration and row count, newest last, at most [`MAX_ENTRIES`].
//!
//! Kept beside the project files as `history/<project id>.json`, so the project file stays
//! small and saving one never rewrites the other.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CoreError, Result};
use crate::store::{Store, write_atomic};

pub const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub text: String,
    pub connection: Uuid,
    /// Seconds since the Unix epoch.
    pub at: i64,
    pub duration_ms: u64,
    /// Rows returned or affected; `None` when the statement failed.
    #[serde(default)]
    pub rows: Option<u64>,
}

/// Appends an entry; the same text run again moves to the end instead of repeating.
pub fn push(entries: &mut Vec<HistoryEntry>, entry: HistoryEntry) {
    entries.retain(|e| !(e.text == entry.text && e.connection == entry.connection));
    entries.push(entry);
    if entries.len() > MAX_ENTRIES {
        let extra = entries.len() - MAX_ENTRIES;
        entries.drain(..extra);
    }
}

fn path(store: &Store, project: Uuid) -> PathBuf {
    store.root().join("history").join(format!("{project}.json"))
}

/// The project's history; empty when there is none or the file cannot be read.
pub fn load(store: &Store, project: Uuid) -> Vec<HistoryEntry> {
    fs::read(path(store, project))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(store: &Store, project: Uuid, entries: &[HistoryEntry]) -> Result<()> {
    let p = path(store, project);
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir).map_err(|e| CoreError::Storage(format!("cannot create {}: {e}", dir.display())))?;
    }
    let bytes = serde_json::to_vec(entries).map_err(|e| CoreError::Storage(e.to_string()))?;
    write_atomic(&p, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str, conn: Uuid) -> HistoryEntry {
        HistoryEntry {
            text: text.into(),
            connection: conn,
            at: 1,
            duration_ms: 3,
            rows: Some(1),
        }
    }

    #[test]
    fn round_trip_dedup_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let project = Uuid::new_v4();
        let conn = Uuid::new_v4();
        assert!(load(&store, project).is_empty());

        let mut h = Vec::new();
        push(&mut h, entry("select 1", conn));
        push(&mut h, entry("select 2", conn));
        push(&mut h, entry("select 1", conn));
        assert_eq!(
            h.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["select 2", "select 1"]
        );
        push(&mut h, entry("select 1", Uuid::new_v4()));
        assert_eq!(h.len(), 3, "another connection is another entry");

        for i in 0..(MAX_ENTRIES + 20) {
            push(&mut h, entry(&format!("select {i}"), conn));
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.last().unwrap().text, format!("select {}", MAX_ENTRIES + 19));

        save(&store, project, &h).unwrap();
        assert_eq!(load(&store, project), h);
    }
}
