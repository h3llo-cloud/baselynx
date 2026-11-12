//! SQLite through rusqlite (the library is compiled in). One connection per session, used
//! on the runtime's blocking pool; an interrupt handle cancels what it runs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use tokio::sync::mpsc;

use crate::catalog::{ColumnInfo, ForeignKey, RelKind, RelationInfo, RelationRef, SchemaInfo};
use crate::edit::{CommitReport, Planned};
use crate::error::{ChangeIndex, CoreError, Result};
use crate::exec::{BATCH_ROWS, ColumnMeta, ExecEvent, Row};
use crate::model::{ConnectionConfig, Engine};
use crate::runtime;
use crate::session::{Conn, Session};

pub struct Lite {
    conn: Arc<Mutex<Connection>>,
    interrupt: InterruptHandle,
}

impl Lite {
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// Runs `f` with the connection on the blocking pool.
    pub(crate) async fn with<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        runtime::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| CoreError::Internal("the SQLite connection was poisoned".into()))?;
            f(&mut guard)
        })
        .await
    }
}

fn lite(session: &Session) -> &Lite {
    match session.conn() {
        Conn::Sqlite(l) => l,
        _ => unreachable!("not a SQLite session"),
    }
}

/// Opens the file named by `config.database`. A missing file is an error, never a new empty
/// database: a typo in a path should not look like an empty database.
pub async fn connect(config: &ConnectionConfig) -> Result<Session> {
    let path = config.database.clone();
    let read_only = config.read_only;
    runtime::spawn_blocking(move || {
        if path.trim().is_empty() {
            return Err(CoreError::Connect("no SQLite file given".into()));
        }
        if !std::path::Path::new(&path).is_file() {
            return Err(CoreError::Connect(format!("{path}: no such file")));
        }
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
        };
        let conn = Connection::open_with_flags(&path, flags).map_err(|e| CoreError::Connect(format!("{path}: {e}")))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        let version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
        let interrupt = conn.get_interrupt_handle();
        Ok(Session::new(
            Conn::Sqlite(Lite {
                conn: Arc::new(Mutex::new(conn)),
                interrupt,
            }),
            Engine::Sqlite,
            version,
            read_only,
        ))
    })
    .await
}

/// A value as text, like Postgres prints it: blobs as `\x` hex.
fn text(v: ValueRef<'_>) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("\\x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            Some(s)
        }
    }
}

/// Runs one statement and streams its events, like [`crate::exec::execute`].
pub(crate) async fn run(
    session: &Arc<Session>,
    sql: String,
    row_cap: Option<usize>,
    tx: mpsc::Sender<Result<ExecEvent>>,
) -> Result<()> {
    let start = Instant::now();
    lite(session)
        .with(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let ncols = stmt.column_count();
            if ncols == 0 {
                let n = stmt.raw_execute()?;
                let _ = tx.blocking_send(Ok(ExecEvent::Done {
                    count: n as u64,
                    returned_rows: false,
                    elapsed: start.elapsed(),
                    truncated: false,
                }));
                return Ok(());
            }
            let metas: Vec<ColumnMeta> = stmt
                .columns()
                .map(|c| ColumnMeta {
                    name: c.name().to_string(),
                    type_oid: None,
                    type_name: c.decl_type().map(str::to_string),
                })
                .collect();
            if tx.blocking_send(Ok(ExecEvent::Columns(metas))).is_err() {
                return Ok(());
            }
            let mut rows = stmt.raw_query();
            let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
            let mut received = 0usize;
            let mut capped = false;
            while let Some(row) = rows.next()? {
                let values: Row = (0..ncols).map(|i| row.get_ref(i).ok().and_then(text)).collect();
                batch.push(values);
                received += 1;
                if batch.len() >= BATCH_ROWS
                    && tx
                        .blocking_send(Ok(ExecEvent::Rows(std::mem::take(&mut batch))))
                        .is_err()
                {
                    return Ok(());
                }
                if row_cap.is_some_and(|cap| received >= cap) {
                    capped = true;
                    break;
                }
            }
            if !batch.is_empty() && tx.blocking_send(Ok(ExecEvent::Rows(batch))).is_err() {
                return Ok(());
            }
            let _ = tx.blocking_send(Ok(ExecEvent::Done {
                count: received as u64,
                returned_rows: true,
                elapsed: start.elapsed(),
                truncated: capped,
            }));
            Ok(())
        })
        .await
}

/// Text rows of an internal catalog query.
fn rows(conn: &Connection, sql: &str, params: &[&str]) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(sql)?;
    let n = stmt.column_count();
    let mut out = Vec::new();
    let mut q = stmt.query(rusqlite::params_from_iter(params.iter()))?;
    while let Some(r) = q.next()? {
        out.push((0..n).map(|i| r.get_ref(i).ok().and_then(text)).collect());
    }
    Ok(out)
}

fn cell(r: &Row, i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

/// Attached databases: `main` and any `ATTACH`ed ones; `temp` is left out.
fn schemas(conn: &Connection) -> Result<Vec<String>> {
    Ok(rows(
        conn,
        "SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq",
        &[],
    )?
    .iter()
    .map(|r| cell(r, 0))
    .collect())
}

pub(crate) async fn list_schemas(session: &Arc<Session>) -> Result<Vec<SchemaInfo>> {
    lite(session)
        .with(|conn| {
            Ok(schemas(conn)?
                .into_iter()
                .map(|name| SchemaInfo { name, system: false })
                .collect())
        })
        .await
}

fn primary_key(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    Ok(rows(
        conn,
        "SELECT name FROM pragma_table_info(?1, ?2) WHERE pk > 0 ORDER BY pk",
        &[table, schema],
    )?
    .iter()
    .map(|r| cell(r, 0))
    .collect())
}

fn relations_in(conn: &Connection, schema: &str) -> Result<Vec<RelationRef>> {
    let sql = format!(
        "SELECT name, type FROM {}.sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name",
        crate::sql::Dialect::Sqlite.quote_ident(schema)
    );
    let mut out = Vec::new();
    for r in rows(conn, &sql, &[])? {
        let name = cell(&r, 0);
        let kind = if cell(&r, 1) == "view" {
            RelKind::View
        } else {
            RelKind::Table
        };
        let primary_key = if kind == RelKind::Table {
            primary_key(conn, schema, &name)?
        } else {
            Vec::new()
        };
        out.push(RelationRef {
            schema: schema.to_string(),
            name,
            kind,
            estimate: None,
            primary_key,
        });
    }
    Ok(out)
}

pub(crate) async fn list_relations(session: &Arc<Session>, schema: String) -> Result<Vec<RelationRef>> {
    lite(session).with(move |conn| relations_in(conn, &schema)).await
}

pub(crate) async fn list_all_relations(session: &Arc<Session>) -> Result<Vec<RelationRef>> {
    lite(session)
        .with(|conn| {
            let mut out = Vec::new();
            for s in schemas(conn)? {
                out.extend(relations_in(conn, &s)?);
            }
            Ok(out)
        })
        .await
}

pub(crate) async fn list_columns(
    session: &Arc<Session>,
    schema: String,
    table: String,
) -> Result<Vec<(String, String)>> {
    lite(session)
        .with(move |conn| {
            Ok(rows(
                conn,
                "SELECT name, type FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
                &[&table, &schema],
            )?
            .iter()
            .map(|r| (cell(r, 0), cell(r, 1)))
            .collect())
        })
        .await
}

/// Foreign keys declared on `table`, grouped by constraint. A key that names no target
/// columns points at the target's primary key.
fn foreign_keys_of(conn: &Connection, schema: &str, table: &str) -> Result<Vec<ForeignKey>> {
    let list = rows(
        conn,
        "SELECT id, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        &[table, schema],
    )?;
    let mut out: Vec<ForeignKey> = Vec::new();
    let mut current: Option<String> = None;
    for r in list {
        let id = cell(&r, 0);
        if current.as_deref() != Some(id.as_str()) {
            current = Some(id.clone());
            out.push(ForeignKey {
                name: format!("fk_{table}_{id}"),
                from_schema: schema.to_string(),
                from_table: table.to_string(),
                from_cols: Vec::new(),
                to_schema: schema.to_string(),
                to_table: cell(&r, 1),
                to_cols: Vec::new(),
            });
        }
        let fk = out.last_mut().expect("pushed above");
        fk.from_cols.push(cell(&r, 2));
        if let Some(to) = r.get(3).cloned().flatten() {
            fk.to_cols.push(to);
        }
    }
    for fk in &mut out {
        if fk.to_cols.is_empty() {
            fk.to_cols = primary_key(conn, schema, &fk.to_table)?;
        }
    }
    Ok(out)
}

pub(crate) async fn describe_relation(session: &Arc<Session>, schema: String, name: String) -> Result<RelationInfo> {
    lite(session)
        .with(move |conn| {
            let master = format!("{}.sqlite_master", crate::sql::Dialect::Sqlite.quote_ident(&schema));
            let kind = rows(
                conn,
                &format!("SELECT type, sql FROM {master} WHERE name = ?1"),
                &[&name],
            )?
            .into_iter()
            .next()
            .ok_or_else(|| CoreError::Invalid(format!("relation {schema}.{name} does not exist")))?;
            let rel_kind = match cell(&kind, 0).as_str() {
                "table" => RelKind::Table,
                "view" => RelKind::View,
                _ => return Err(CoreError::Invalid(format!("{schema}.{name} is not a table or view"))),
            };
            // `hidden`: 1 for virtual-table columns, 2 and 3 for generated ones.
            let cols = rows(
                conn,
                "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
                &[&name, &schema],
            )?;
            let single_int_pk = cols.iter().filter(|r| cell(r, 4) != "0").count() == 1
                && cols
                    .iter()
                    .any(|r| cell(r, 4) == "1" && cell(r, 1).eq_ignore_ascii_case("integer"));
            let columns: Vec<ColumnInfo> = cols
                .iter()
                .filter(|r| cell(r, 5) != "1")
                .map(|r| {
                    let pk = cell(r, 4) != "0";
                    ColumnInfo {
                        name: cell(r, 0),
                        type_name: cell(r, 1),
                        type_oid: 0,
                        not_null: cell(r, 2) == "1" || pk,
                        // An INTEGER PRIMARY KEY is the rowid: SQLite fills it in.
                        has_default: r.get(3).is_some_and(Option::is_some) || (pk && single_int_pk),
                        generated: matches!(cell(r, 5).as_str(), "2" | "3"),
                        identity_always: false,
                    }
                })
                .collect();
            let mut row_key = None;
            if rel_kind == RelKind::Table {
                let pk = primary_key(conn, &schema, &name)?;
                if !pk.is_empty() {
                    row_key = Some(pk);
                } else {
                    // A unique index over not-null columns, by name.
                    let idx = rows(
                        conn,
                        "SELECT name FROM pragma_index_list(?1, ?2) WHERE \"unique\" = 1 AND partial = 0 ORDER BY name",
                        &[&name, &schema],
                    )?;
                    for i in idx {
                        let names: Vec<String> = rows(
                            conn,
                            "SELECT name FROM pragma_index_info(?1, ?2) ORDER BY seqno",
                            &[&cell(&i, 0), &schema],
                        )?
                        .iter()
                        .map(|r| cell(r, 0))
                        .collect();
                        let ok = !names.is_empty()
                            && names.iter().all(|n| columns.iter().any(|c| &c.name == n && c.not_null));
                        if ok {
                            row_key = Some(names);
                            break;
                        }
                    }
                }
            }
            let foreign_keys = if rel_kind == RelKind::Table {
                foreign_keys_of(conn, &schema, &name)?
            } else {
                Vec::new()
            };
            let mut referenced_by = Vec::new();
            for other in relations_in(conn, &schema)? {
                if other.kind != RelKind::Table {
                    continue;
                }
                for fk in foreign_keys_of(conn, &schema, &other.name)? {
                    if fk.to_table == name {
                        referenced_by.push(fk);
                    }
                }
            }
            Ok(RelationInfo {
                editable: rel_kind.is_table() && row_key.is_some(),
                schema,
                name,
                kind: rel_kind,
                columns,
                row_key,
                foreign_keys,
                referenced_by,
            })
        })
        .await
}

/// Runs the planned statements in one transaction; nothing stays if one fails.
pub(crate) async fn commit(
    session: &Arc<Session>,
    planned: Vec<Planned>,
    report: CommitReport,
) -> Result<CommitReport> {
    lite(session)
        .with(move |conn| {
            let tx = conn.transaction()?;
            for (i, p) in planned.iter().enumerate() {
                let result = tx
                    .prepare(&p.sql)
                    .and_then(|mut stmt| stmt.execute(rusqlite::params_from_iter(p.params.iter())));
                let failure = match result {
                    Ok(n) if p.expect_one && n != 1 => Some(CoreError::Invalid(format!(
                        "expected 1 row for {}, the statement affected {n}",
                        p.key_label.as_deref().unwrap_or("this row")
                    ))),
                    Ok(_) => None,
                    Err(e) => Some(CoreError::from(e)),
                };
                if let Some(err) = failure {
                    // Dropping the transaction rolls it back.
                    return Err(CoreError::Commit {
                        index: ChangeIndex(i),
                        source: Box::new(err),
                    });
                }
            }
            tx.commit()?;
            Ok(report)
        })
        .await
}

// ---------------------------------------------------------------- backups

/// A consistent copy of the database file through SQLite's online backup API.
pub(crate) fn copy_file(src: &str, dest: &std::path::Path) -> Result<()> {
    let conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.backup(rusqlite::MAIN_DB, dest, None::<fn(rusqlite::backup::Progress)>)?;
    Ok(())
}

/// `PRAGMA integrity_check` on a backup copy.
pub(crate) fn check_file(path: &std::path::Path) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verdict: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if verdict != "ok" {
        return Err(CoreError::Invalid(format!(
            "the copy fails SQLite's integrity check: {verdict}"
        )));
    }
    Ok(())
}

fn literal(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => format!("{f:?}"),
        ValueRef::Text(t) => crate::sql::Dialect::Sqlite.quote_literal(&String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => {
            let mut s = String::with_capacity(3 + b.len() * 2);
            s.push_str("X'");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s.push('\'');
            s
        }
    }
}

/// The plain SQL form: `schema.sql` (tables), `data.sql` (rows as INSERTs) and
/// `indexes.sql` (indexes, triggers, views), in the order a restore runs them.
pub(crate) fn write_sql(src: &str, dir: &std::path::Path, exclude: &[String]) -> Result<()> {
    use std::io::Write;
    let conn = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let objects = rows(
        &conn,
        "SELECT type, name, sql FROM sqlite_master \
         WHERE sql IS NOT NULL AND substr(name, 1, 7) <> 'sqlite_' ORDER BY rowid",
        &[],
    )?;
    let io = |e: std::io::Error| CoreError::Storage(format!("{}: {e}", dir.display()));
    let mut schema = std::io::BufWriter::new(std::fs::File::create(dir.join("schema.sql")).map_err(io)?);
    let mut data = std::io::BufWriter::new(std::fs::File::create(dir.join("data.sql")).map_err(io)?);
    let mut later = std::io::BufWriter::new(std::fs::File::create(dir.join("indexes.sql")).map_err(io)?);
    // Rows can arrive before the rows they point at; the keys are checked at COMMIT.
    writeln!(data, "PRAGMA defer_foreign_keys = ON;").map_err(io)?;
    for o in &objects {
        let (kind, name, sql) = (cell(o, 0), cell(o, 1), cell(o, 2));
        if kind != "table" {
            writeln!(later, "{sql};").map_err(io)?;
            continue;
        }
        writeln!(schema, "{sql};").map_err(io)?;
        if exclude.iter().any(|x| x == &name || x == &format!("main.{name}")) {
            continue;
        }
        // Stored columns only: generated ones (hidden 2 and 3) are computed again on insert.
        let d = crate::sql::Dialect::Sqlite;
        let table = d.quote_ident(&name);
        let cols: Vec<String> = rows(
            &conn,
            "SELECT name FROM pragma_table_xinfo(?1) WHERE hidden = 0 ORDER BY cid",
            &[&name],
        )?
        .iter()
        .map(|r| d.quote_ident(&cell(r, 0)))
        .collect();
        if cols.is_empty() {
            continue;
        }
        let list = cols.join(", ");
        let mut stmt = conn.prepare(&format!("SELECT {list} FROM {table}"))?;
        let n = stmt.column_count();
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let values: Vec<String> = (0..n)
                .map(|i| r.get_ref(i).map(literal).unwrap_or_else(|_| "NULL".into()))
                .collect();
            writeln!(data, "INSERT INTO {table} ({list}) VALUES ({});", values.join(", ")).map_err(io)?;
        }
    }
    schema.flush().map_err(io)?;
    data.flush().map_err(io)?;
    later.flush().map_err(io)?;
    Ok(())
}

/// What the target file holds besides SQLite's own tables (at most four, for the dialog).
pub(crate) fn file_objects(path: &str) -> Result<Vec<String>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(rows(
        &conn,
        "SELECT type || ' ' || name FROM sqlite_master WHERE substr(name, 1, 7) <> 'sqlite_' ORDER BY 1 LIMIT 4",
        &[],
    )?
    .iter()
    .map(|r| cell(r, 0))
    .collect())
}

/// Replaces the (empty) target's content with a backup copy, in one step.
pub(crate) fn restore_file(target: &str, copy: &std::path::Path) -> Result<()> {
    let mut conn = Connection::open_with_flags(target, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.restore(rusqlite::MAIN_DB, copy, None::<fn(rusqlite::backup::Progress)>)?;
    Ok(())
}

/// Runs the SQL files in one transaction: a failure leaves the target as it was.
pub(crate) fn restore_sql(target: &str, files: &[std::path::PathBuf]) -> Result<()> {
    let mut conn = Connection::open_with_flags(target, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA foreign_keys = ON")?;
    let tx = conn.transaction()?;
    for f in files {
        let text = std::fs::read_to_string(f).map_err(|e| CoreError::Storage(format!("{}: {e}", f.display())))?;
        tx.execute_batch(&text)?;
    }
    tx.commit()?;
    Ok(())
}
