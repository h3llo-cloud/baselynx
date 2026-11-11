//! Pending edits of a table tab: changed cells, new rows, rows marked for deletion.
//! Pure state, no UI.

use std::collections::{BTreeMap, BTreeSet};

use baselynx_core::catalog::RelationInfo;
use baselynx_core::edit::{CellValue, ChangeSet, RowDelete, RowInsert, RowUpdate};
use baselynx_core::exec::Row;

/// What a cell shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown<'a> {
    Value(&'a str),
    Null,
    /// A new row's cell the user has not filled: the column default applies.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Loaded,
    Changed,
    New,
    Deleted,
}

/// The pending edits, without the loaded rows: what undo and redo step through.
#[derive(Debug, Default, Clone, PartialEq)]
struct Edits {
    changed: BTreeMap<(usize, usize), CellValue>,
    deleted: BTreeSet<usize>,
    new_rows: Vec<Vec<Option<CellValue>>>,
}

/// Undo steps kept per page.
const UNDO_DEPTH: usize = 200;

#[derive(Debug, Default, Clone)]
pub struct EditModel {
    loaded: Vec<Row>,
    /// (loaded row, column) → new value.
    changed: BTreeMap<(usize, usize), CellValue>,
    deleted: BTreeSet<usize>,
    /// Cells of new rows; `None` means DEFAULT.
    new_rows: Vec<Vec<Option<CellValue>>>,
    columns: usize,
    /// Edit states before each change, oldest first.
    undo: Vec<Edits>,
    redo: Vec<Edits>,
}

impl EditModel {
    pub fn new(loaded: Vec<Row>, columns: usize) -> Self {
        Self {
            loaded,
            columns,
            ..Default::default()
        }
    }

    /// Replaces the loaded rows and drops every pending edit.
    pub fn reset(&mut self, loaded: Vec<Row>, columns: usize) {
        *self = Self::new(loaded, columns);
    }

    fn edits(&self) -> Edits {
        Edits {
            changed: self.changed.clone(),
            deleted: self.deleted.clone(),
            new_rows: self.new_rows.clone(),
        }
    }

    fn restore(&mut self, e: Edits) {
        self.changed = e.changed;
        self.deleted = e.deleted;
        self.new_rows = e.new_rows;
    }

    /// Records the state before a change; a change that changes nothing leaves no step.
    fn checkpoint(&mut self, before: Edits) {
        if before != self.edits() {
            self.undo.push(before);
            if self.undo.len() > UNDO_DEPTH {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Steps back one change: a cell edit, an added row, a deletion. Returns false when
    /// there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        let now = self.edits();
        self.redo.push(now);
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        let now = self.edits();
        self.undo.push(now);
        self.restore(next);
        true
    }

    /// Appends streamed rows (console results).
    pub fn append_loaded(&mut self, mut rows: Vec<Row>) {
        self.loaded.append(&mut rows);
    }

    pub fn loaded(&self) -> &[Row] {
        &self.loaded
    }

    pub fn loaded_len(&self) -> usize {
        self.loaded.len()
    }

    pub fn row_count(&self) -> usize {
        self.loaded.len() + self.new_rows.len()
    }

    pub fn is_new(&self, row: usize) -> bool {
        row >= self.loaded.len()
    }

    pub fn row_state(&self, row: usize) -> RowState {
        if self.is_new(row) {
            RowState::New
        } else if self.deleted.contains(&row) {
            RowState::Deleted
        } else if self.changed.range((row, 0)..(row + 1, 0)).next().is_some() {
            RowState::Changed
        } else {
            RowState::Loaded
        }
    }

    pub fn shown(&self, row: usize, col: usize) -> Shown<'_> {
        fn from(v: &CellValue) -> Shown<'_> {
            match v {
                CellValue::Null => Shown::Null,
                CellValue::Text(s) => Shown::Value(s),
            }
        }
        if self.is_new(row) {
            return match self.new_rows.get(row - self.loaded.len()).and_then(|r| r.get(col)) {
                Some(Some(v)) => from(v),
                _ => Shown::Default,
            };
        }
        if let Some(v) = self.changed.get(&(row, col)) {
            return from(v);
        }
        match self.loaded.get(row).and_then(|r| r.get(col)) {
            Some(Some(s)) => Shown::Value(s),
            _ => Shown::Null,
        }
    }

    pub fn loaded_value(&self, row: usize, col: usize) -> Option<&str> {
        self.loaded.get(row).and_then(|r| r.get(col)).and_then(|v| v.as_deref())
    }

    pub fn is_cell_changed(&self, row: usize, col: usize) -> bool {
        self.changed.contains_key(&(row, col))
    }

    /// Sets a cell. Setting a loaded cell back to its loaded value removes the change.
    /// Cells of deleted rows cannot change.
    pub fn set(&mut self, row: usize, col: usize, value: CellValue) {
        let before = self.edits();
        self.set_inner(row, col, value);
        self.checkpoint(before);
    }

    /// Sets many cells to one value as a single undo step (a column selection).
    pub fn set_many(&mut self, cells: &[(usize, usize)], value: CellValue) {
        let before = self.edits();
        for &(r, c) in cells {
            self.set_inner(r, c, value.clone());
        }
        self.checkpoint(before);
    }

    /// Sets cells to their own values as a single undo step (a multi-line paste).
    pub fn set_each(&mut self, edits: Vec<((usize, usize), CellValue)>) {
        let before = self.edits();
        for ((r, c), v) in edits {
            self.set_inner(r, c, v);
        }
        self.checkpoint(before);
    }

    /// Reverts many cells as a single undo step.
    pub fn revert_many(&mut self, cells: &[(usize, usize)]) {
        let before = self.edits();
        for &(r, c) in cells {
            self.revert_inner(r, c);
        }
        self.checkpoint(before);
    }

    fn set_inner(&mut self, row: usize, col: usize, value: CellValue) {
        if self.is_new(row) {
            if let Some(r) = self.new_rows.get_mut(row - self.loaded.len())
                && col < r.len()
            {
                r[col] = Some(value);
            }
            return;
        }
        if self.deleted.contains(&row) || row >= self.loaded.len() {
            return;
        }
        let loaded = CellValue::from_option(self.loaded[row].get(col).cloned().flatten());
        if loaded == value {
            self.changed.remove(&(row, col));
        } else {
            self.changed.insert((row, col), value);
        }
    }

    fn revert_inner(&mut self, row: usize, col: usize) {
        if self.is_new(row) {
            if let Some(r) = self.new_rows.get_mut(row - self.loaded.len())
                && col < r.len()
            {
                r[col] = None;
            }
        } else {
            self.changed.remove(&(row, col));
        }
    }

    /// Appends a new row; returns its index.
    pub fn add_row(&mut self) -> usize {
        let before = self.edits();
        self.new_rows.push(vec![None; self.columns]);
        self.checkpoint(before);
        self.row_count() - 1
    }

    /// Delete row: marks a loaded row (dropping its edits) or unmarks it; removes a new row.
    pub fn toggle_delete(&mut self, row: usize) {
        let before = self.edits();
        self.toggle_inner(row);
        self.checkpoint(before);
    }

    fn toggle_inner(&mut self, row: usize) {
        if self.is_new(row) {
            let ix = row - self.loaded.len();
            if ix < self.new_rows.len() {
                self.new_rows.remove(ix);
            }
            return;
        }
        if row >= self.loaded.len() {
            return;
        }
        if !self.deleted.remove(&row) {
            self.deleted.insert(row);
            self.changed.retain(|(r, _), _| *r != row);
        }
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.deleted.contains(&row)
    }

    /// Number of statements a commit would run.
    pub fn pending(&self) -> usize {
        let changed_rows: BTreeSet<usize> = self.changed.keys().map(|(r, _)| *r).collect();
        changed_rows.len() + self.deleted.len() + self.new_rows.len()
    }

    pub fn is_dirty(&self) -> bool {
        self.pending() > 0
    }

    /// Builds the change set. `column_names[i]` names column `i` of the rows.
    pub fn change_set(&self, rel: &RelationInfo, column_names: &[String]) -> Result<ChangeSet, String> {
        let key_cols: Vec<(usize, String)> = rel
            .row_key
            .as_ref()
            .ok_or("this table has no row key")?
            .iter()
            .map(|k| {
                column_names
                    .iter()
                    .position(|c| c == k)
                    .map(|i| (i, k.clone()))
                    .ok_or_else(|| format!("key column {k} is not loaded"))
            })
            .collect::<Result<_, _>>()?;
        let key_of = |row: usize| -> Result<Vec<(String, String)>, String> {
            key_cols
                .iter()
                .map(|(i, name)| {
                    self.loaded_value(row, *i)
                        .map(|v| (name.clone(), v.to_string()))
                        .ok_or_else(|| format!("key column {name} is NULL"))
                })
                .collect()
        };
        let mut out = ChangeSet::default();
        let mut by_row: BTreeMap<usize, Vec<(String, CellValue)>> = BTreeMap::new();
        for ((row, col), v) in &self.changed {
            by_row
                .entry(*row)
                .or_default()
                .push((column_names[*col].clone(), v.clone()));
        }
        for (row, set) in by_row {
            out.updates.push(RowUpdate { key: key_of(row)?, set });
        }
        for row in &self.deleted {
            out.deletes.push(RowDelete { key: key_of(*row)? });
        }
        for r in &self.new_rows {
            out.inserts.push(RowInsert {
                values: r
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| v.clone().map(|v| (column_names[i].clone(), v)))
                    .collect(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baselynx_core::catalog::{ColumnInfo, RelKind};

    fn rows() -> Vec<Row> {
        vec![
            vec![Some("1".into()), Some("a".into())],
            vec![Some("2".into()), None],
            vec![Some("3".into()), Some("c".into())],
        ]
    }

    fn rel() -> RelationInfo {
        let col = |n: &str| ColumnInfo {
            name: n.into(),
            type_name: "text".into(),
            type_oid: 25,
            not_null: false,
            has_default: false,
            generated: false,
            identity_always: false,
        };
        RelationInfo {
            schema: "s".into(),
            name: "t".into(),
            kind: RelKind::Table,
            columns: vec![col("id"), col("v")],
            row_key: Some(vec!["id".into()]),
            editable: true,
            foreign_keys: vec![],
            referenced_by: vec![],
        }
    }

    fn names() -> Vec<String> {
        vec!["id".into(), "v".into()]
    }

    #[test]
    fn edit_and_revert_to_loaded() {
        let mut m = EditModel::new(rows(), 2);
        m.set(0, 1, CellValue::Text("z".into()));
        assert_eq!(m.pending(), 1);
        assert_eq!(m.shown(0, 1), Shown::Value("z"));
        assert_eq!(m.row_state(0), RowState::Changed);
        m.set(0, 1, CellValue::Text("a".into()));
        assert_eq!(m.pending(), 0);
        m.set(1, 1, CellValue::Text(String::new()));
        assert_eq!(m.shown(1, 1), Shown::Value(""), "empty string is not NULL");
        m.set(1, 1, CellValue::Null);
        assert!(!m.is_dirty());
    }

    #[test]
    fn two_cells_of_one_row_are_one_change() {
        let mut m = EditModel::new(rows(), 2);
        m.set(0, 0, CellValue::Text("10".into()));
        m.set(0, 1, CellValue::Text("x".into()));
        assert_eq!(m.pending(), 1);
        let cs = m.change_set(&rel(), &names()).unwrap();
        assert_eq!(cs.updates.len(), 1);
        assert_eq!(
            cs.updates[0].key,
            vec![("id".to_string(), "1".to_string())],
            "key as loaded"
        );
    }

    #[test]
    fn delete_marks_and_restores() {
        let mut m = EditModel::new(rows(), 2);
        m.set(2, 1, CellValue::Text("q".into()));
        m.toggle_delete(2);
        assert_eq!(m.pending(), 1);
        assert_eq!(m.row_state(2), RowState::Deleted);
        assert!(!m.is_cell_changed(2, 1), "edits on a deleted row are dropped");
        m.set(2, 1, CellValue::Text("ignored".into()));
        assert!(!m.is_cell_changed(2, 1));
        m.toggle_delete(2);
        assert_eq!(m.pending(), 0);
    }

    #[test]
    fn new_rows() {
        let mut m = EditModel::new(rows(), 2);
        let r = m.add_row();
        assert_eq!(r, 3);
        assert_eq!(m.shown(r, 0), Shown::Default);
        m.set(r, 1, CellValue::Text("new".into()));
        let r2 = m.add_row();
        assert_eq!(m.pending(), 2);
        let cs = m.change_set(&rel(), &names()).unwrap();
        assert_eq!(cs.inserts.len(), 2);
        assert_eq!(
            cs.inserts[0].values,
            vec![("v".to_string(), CellValue::Text("new".into()))]
        );
        assert!(cs.inserts[1].values.is_empty());
        m.toggle_delete(r2);
        assert_eq!(m.row_count(), 4);
        assert_eq!(m.pending(), 1);
    }

    #[test]
    fn undo_and_redo_step_through_edits() {
        let mut m = EditModel::new(rows(), 2);
        assert!(!m.undo());
        m.set(0, 1, CellValue::Text("x".into()));
        m.set(0, 1, CellValue::Text("y".into()));
        let r = m.add_row();
        m.toggle_delete(1);
        assert_eq!(m.pending(), 3);

        assert!(m.undo(), "the deletion");
        assert!(!m.is_deleted(1));
        assert!(m.undo(), "the new row");
        assert_eq!(m.row_count(), 3);
        assert!(m.undo(), "the second edit");
        assert_eq!(m.shown(0, 1), Shown::Value("x"));
        assert!(m.redo());
        assert_eq!(m.shown(0, 1), Shown::Value("y"));
        assert!(m.redo());
        assert_eq!(m.row_count(), 4);
        assert_eq!(r, 3);

        // A new change drops what could be redone.
        m.set(2, 1, CellValue::Null);
        assert!(!m.can_redo());
        // A change to the same value leaves no step.
        let steps = m.undo.len();
        m.set(2, 1, CellValue::Null);
        assert_eq!(m.undo.len(), steps);
        // Reloading the page forgets the steps.
        m.reset(rows(), 2);
        assert!(!m.can_undo());
    }

    #[test]
    fn many_cells_are_one_step() {
        let mut m = EditModel::new(rows(), 2);
        m.set_many(&[(0, 1), (1, 1), (2, 1)], CellValue::Text("same".into()));
        assert_eq!(m.pending(), 3);
        assert_eq!(m.shown(1, 1), Shown::Value("same"));
        assert!(m.undo());
        assert_eq!(m.pending(), 0, "one undo takes back the whole selection");
        assert!(m.redo());
        m.revert_many(&[(0, 1), (2, 1)]);
        assert_eq!(m.pending(), 1);
        assert!(m.undo());
        assert_eq!(m.pending(), 3);
    }

    #[test]
    fn change_set_order_and_keys() {
        let mut m = EditModel::new(rows(), 2);
        m.toggle_delete(1);
        m.set(0, 1, CellValue::Null);
        let cs = m.change_set(&rel(), &names()).unwrap();
        assert_eq!(cs.updates[0].set, vec![("v".to_string(), CellValue::Null)]);
        assert_eq!(cs.deletes[0].key, vec![("id".to_string(), "2".to_string())]);
        assert_eq!(cs.summary(), "1 update, 1 delete");
    }
}
