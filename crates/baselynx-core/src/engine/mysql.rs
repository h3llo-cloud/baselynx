//! MySQL and MariaDB through mysql_async (rustls). One connection per session behind a
//! mutex; a second, short connection sends `KILL QUERY` to cancel. Databases in
//! Kubernetes are reached through a local port-forward listener.

use std::sync::Arc;
use std::time::Instant;

use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Opts, OptsBuilder, Params, SslOpts, TxOpts, Value};
use tokio::sync::{Mutex, mpsc};

use crate::catalog::{ColumnInfo, ForeignKey, RelKind, RelationInfo, RelationRef, SchemaInfo};
use crate::edit::{CommitReport, Planned};
use crate::error::{ChangeIndex, CoreError, DbError, Result};
use crate::exec::{BATCH_ROWS, ColumnMeta, ExecEvent, Row};
use crate::k8s::{self, LocalForward};
use crate::model::{ConnectionConfig, Engine, SslMode};
use crate::session::{CONNECT_TIMEOUT, Conn, Session};
use crate::sql::Dialect;

const D: Dialect = Dialect::MySql;

/// Schemas MySQL and MariaDB keep for themselves: shown last, collapsed.
const SYSTEM_SCHEMAS: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

pub struct My {
    conn: Mutex<mysql_async::Conn>,
    opts: Opts,
    id: u32,
    /// Keeps the local listener of a Kubernetes database alive.
    _forward: Option<LocalForward>,
}

fn my(session: &Session) -> &My {
    match session.conn() {
        Conn::Mysql(m) => m,
        _ => unreachable!("not a MySQL session"),
    }
}

impl From<mysql_async::Error> for CoreError {
    fn from(err: mysql_async::Error) -> Self {
        match err {
            mysql_async::Error::Server(e) => {
                // 1317: query execution was interrupted (KILL QUERY).
                if e.code == 1317 {
                    return CoreError::Cancelled;
                }
                CoreError::Db(DbError {
                    message: e.message,
                    code: e.code.to_string(),
                    detail: Some(format!("SQLSTATE {}", e.state)),
                    hint: None,
                    position: None,
                })
            }
            mysql_async::Error::Io(e) => CoreError::Connect(e.to_string()),
            // After a wrong password, MariaDB on Windows offers Windows authentication
            // (GSSAPI) next, which the driver does not speak: report it as refused login.
            mysql_async::Error::Driver(mysql_async::DriverError::UnknownAuthPlugin { name }) => CoreError::Db(DbError {
                message: format!(
                    "access denied: the server asked for the {name} plugin, which baselynx does not support;                      check the password, or use an account with mysql_native_password or caching_sha2_password"
                ),
                code: "1045".into(),
                detail: None,
                hint: None,
                position: None,
            }),
            mysql_async::Error::Driver(e) => {
                if matches!(e, mysql_async::DriverError::ConnectionClosed) {
                    CoreError::Closed
                } else {
                    CoreError::Invalid(e.to_string())
                }
            }
            other => CoreError::Invalid(other.to_string()),
        }
    }
}

fn builder(host: &str, port: u16, user: &str, password: Option<&str>, database: &str) -> OptsBuilder {
    OptsBuilder::default()
        .ip_or_hostname(host)
        .tcp_port(port)
        .user(Some(user))
        .pass(password)
        .db_name((!database.is_empty()).then_some(database))
        .prefer_socket(false)
        // UPDATE … reports the rows it matched, not only the ones it changed, so editing a
        // cell to the value it already has still counts as one row.
        .client_found_rows(true)
}

async fn open(opts: Opts) -> Result<mysql_async::Conn> {
    tokio::time::timeout(CONNECT_TIMEOUT, mysql_async::Conn::new(opts))
        .await
        .map_err(|_| CoreError::Connect("timed out connecting".into()))?
        .map_err(|e| match e {
            mysql_async::Error::Server(_)
            | mysql_async::Error::Driver(mysql_async::DriverError::UnknownAuthPlugin { .. }) => CoreError::from(e),
            other => CoreError::Connect(other.to_string()),
        })
}

/// Connects as the config says. `prefer` tries TLS first and falls back to a plain
/// connection; `require` encrypts without checking the certificate; `verify-full` checks it.
pub async fn connect(config: &ConnectionConfig, password: Option<String>) -> Result<Session> {
    let (host, port, database, user, password, forward) = match &config.kube {
        Some(target) => {
            let mut target = target.clone();
            if config.read_only
                && let Some(ro) = target.replica.clone()
            {
                target.service = ro;
            }
            let creds = k8s::credentials(&target, config).await?;
            let forward = k8s::local_forward(target).await?;
            (
                "127.0.0.1".to_string(),
                forward.port,
                creds.database,
                creds.username,
                creds.password.or(password),
                Some(forward),
            )
        }
        None => (
            config.host.clone(),
            config.port,
            config.database.clone(),
            config.user.clone(),
            password,
            None,
        ),
    };
    let base = builder(&host, port, &user, password.as_deref(), &database);
    let insecure = SslOpts::default().with_danger_accept_invalid_certs(true);
    let (conn, opts) = match config.ssl_mode {
        SslMode::Disable => {
            let opts = Opts::from(base);
            (open(opts.clone()).await?, opts)
        }
        SslMode::Require => {
            let opts = Opts::from(base.ssl_opts(insecure));
            (open(opts.clone()).await?, opts)
        }
        SslMode::VerifyFull => {
            let opts = Opts::from(base.ssl_opts(SslOpts::default()));
            (open(opts.clone()).await?, opts)
        }
        SslMode::Prefer => {
            let tls = Opts::from(base.clone().ssl_opts(insecure));
            match open(tls.clone()).await {
                Ok(c) => (c, tls),
                // A wrong password is not a TLS problem: say so instead of retrying.
                Err(e) if e.is_auth_failure() => return Err(e),
                Err(_) => {
                    let plain = Opts::from(base);
                    (open(plain.clone()).await?, plain)
                }
            }
        }
    };
    let mut conn = conn;
    if config.read_only {
        conn.query_drop("SET SESSION TRANSACTION READ ONLY").await?;
    }
    let version: Option<String> = conn.query_first("SELECT VERSION()").await?;
    let id = conn.id();
    Ok(Session::new(
        Conn::Mysql(My {
            conn: Mutex::new(conn),
            opts,
            id,
            _forward: forward,
        }),
        Engine::Mysql,
        version.unwrap_or_default(),
        config.read_only,
    ))
}

/// `KILL QUERY` from a second connection: stops what the session runs, keeps the session.
pub(crate) async fn cancel(session: &Session) -> Result<()> {
    let m = my(session);
    let mut killer = open(m.opts.clone()).await?;
    let result = killer.query_drop(format!("KILL QUERY {}", m.id)).await;
    let _ = killer.disconnect().await;
    // 1094: the query had already finished.
    match result {
        Err(mysql_async::Error::Server(e)) if e.code == 1094 => Ok(()),
        other => Ok(other?),
    }
}

/// The SQL type of a result column, as the grid and "is it a number?" read it.
fn type_name(c: &Column) -> String {
    let binary = c.character_set() == 63;
    let base = match c.column_type() {
        ColumnType::MYSQL_TYPE_TINY => "tinyint",
        ColumnType::MYSQL_TYPE_SHORT => "smallint",
        ColumnType::MYSQL_TYPE_INT24 => "mediumint",
        ColumnType::MYSQL_TYPE_LONG => "int",
        ColumnType::MYSQL_TYPE_LONGLONG => "bigint",
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => "decimal",
        ColumnType::MYSQL_TYPE_FLOAT => "float",
        ColumnType::MYSQL_TYPE_DOUBLE => "double",
        ColumnType::MYSQL_TYPE_YEAR => "year",
        ColumnType::MYSQL_TYPE_BIT => "bit",
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "date",
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => "datetime",
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "time",
        ColumnType::MYSQL_TYPE_JSON => "json",
        ColumnType::MYSQL_TYPE_ENUM => "enum",
        ColumnType::MYSQL_TYPE_SET => "set",
        ColumnType::MYSQL_TYPE_GEOMETRY => "geometry",
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => {
            if binary {
                "varbinary"
            } else {
                "varchar"
            }
        }
        ColumnType::MYSQL_TYPE_STRING => {
            if binary {
                "binary"
            } else {
                "char"
            }
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            if binary {
                "blob"
            } else {
                "text"
            }
        }
        _ => "unknown",
    };
    if c.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
        format!("{base} unsigned")
    } else {
        base.to_string()
    }
}

fn is_binary(c: &Column) -> bool {
    c.character_set() == 63
        && !matches!(
            c.column_type(),
            ColumnType::MYSQL_TYPE_TINY
                | ColumnType::MYSQL_TYPE_SHORT
                | ColumnType::MYSQL_TYPE_INT24
                | ColumnType::MYSQL_TYPE_LONG
                | ColumnType::MYSQL_TYPE_LONGLONG
                | ColumnType::MYSQL_TYPE_DECIMAL
                | ColumnType::MYSQL_TYPE_NEWDECIMAL
                | ColumnType::MYSQL_TYPE_FLOAT
                | ColumnType::MYSQL_TYPE_DOUBLE
                | ColumnType::MYSQL_TYPE_YEAR
                | ColumnType::MYSQL_TYPE_DATE
                | ColumnType::MYSQL_TYPE_NEWDATE
                | ColumnType::MYSQL_TYPE_DATETIME
                | ColumnType::MYSQL_TYPE_DATETIME2
                | ColumnType::MYSQL_TYPE_TIMESTAMP
                | ColumnType::MYSQL_TYPE_TIMESTAMP2
                | ColumnType::MYSQL_TYPE_TIME
                | ColumnType::MYSQL_TYPE_TIME2
        )
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// A value as text. The text protocol sends everything as bytes; binary columns become
/// `\x` hex like Postgres's bytea.
fn text(v: &Value, binary: bool) -> Option<String> {
    match v {
        Value::NULL => None,
        Value::Bytes(b) if binary => Some(hex(b)),
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(f) => Some(f.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
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
    let m = my(session);
    let mut conn = m.conn.lock().await;
    let mut result = conn.query_iter(sql).await?;
    let columns: Vec<Column> = result.columns_ref().to_vec();
    if columns.is_empty() {
        let count = result.affected_rows();
        result.drop_result().await?;
        let _ = tx
            .send(Ok(ExecEvent::Done {
                count,
                returned_rows: false,
                elapsed: start.elapsed(),
                truncated: false,
            }))
            .await;
        return Ok(());
    }
    let metas = columns
        .iter()
        .map(|c| ColumnMeta {
            name: c.name_str().into_owned(),
            type_oid: None,
            type_name: Some(type_name(c)),
        })
        .collect();
    let binary: Vec<bool> = columns.iter().map(is_binary).collect();
    if tx.send(Ok(ExecEvent::Columns(metas))).await.is_err() {
        drop(result);
        drop(conn);
        let _ = cancel(session).await;
        return Ok(());
    }
    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    let mut received = 0usize;
    let mut capped = false;
    while let Some(row) = result.next().await? {
        let values: Row = (0..row.len())
            .map(|i| row.as_ref(i).and_then(|v| text(v, binary[i])))
            .collect();
        batch.push(values);
        received += 1;
        if batch.len() >= BATCH_ROWS && tx.send(Ok(ExecEvent::Rows(std::mem::take(&mut batch)))).await.is_err() {
            drop(result);
            drop(conn);
            let _ = cancel(session).await;
            return Ok(());
        }
        if row_cap.is_some_and(|cap| received >= cap) {
            capped = true;
            break;
        }
    }
    if capped {
        // Stop the server from sending the rest, then drain what is in flight.
        let _ = cancel(session).await;
        let _ = result.drop_result().await;
    }
    if !batch.is_empty() && tx.send(Ok(ExecEvent::Rows(batch))).await.is_err() {
        return Ok(());
    }
    let _ = tx
        .send(Ok(ExecEvent::Done {
            count: received as u64,
            returned_rows: true,
            elapsed: start.elapsed(),
            truncated: capped,
        }))
        .await;
    Ok(())
}

/// Text rows of an internal catalog query.
async fn rows(conn: &mut mysql_async::Conn, sql: String) -> Result<Vec<Row>> {
    let mut result = conn.query_iter(sql).await?;
    let mut out = Vec::new();
    while let Some(row) = result.next().await? {
        out.push(
            (0..row.len())
                .map(|i| row.as_ref(i).and_then(|v| text(v, false)))
                .collect(),
        );
    }
    Ok(out)
}

fn cell(r: &Row, i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

fn lit(s: &str) -> String {
    D.quote_literal(s)
}

pub(crate) async fn list_schemas(session: &Arc<Session>) -> Result<Vec<SchemaInfo>> {
    let mut conn = my(session).conn.lock().await;
    let found = rows(
        &mut conn,
        "SELECT schema_name FROM information_schema.schemata ORDER BY schema_name".into(),
    )
    .await?;
    let mut out: Vec<SchemaInfo> = found
        .iter()
        .map(|r| {
            let name = cell(r, 0);
            let system = SYSTEM_SCHEMAS.contains(&name.as_str());
            SchemaInfo { name, system }
        })
        .collect();
    out.sort_by_key(|s| s.system);
    Ok(out)
}

/// Primary key columns per table of `schema`, in key order.
async fn primary_keys(
    conn: &mut mysql_async::Conn,
    schema: &str,
) -> Result<std::collections::HashMap<String, Vec<String>>> {
    let found = rows(
        conn,
        format!(
            "SELECT table_name, column_name FROM information_schema.key_column_usage \
             WHERE table_schema = {} AND constraint_name = 'PRIMARY' ORDER BY table_name, ordinal_position",
            lit(schema)
        ),
    )
    .await?;
    let mut out: std::collections::HashMap<String, Vec<String>> = Default::default();
    for r in found {
        out.entry(cell(&r, 0)).or_default().push(cell(&r, 1));
    }
    Ok(out)
}

async fn relations_in(conn: &mut mysql_async::Conn, schema: &str) -> Result<Vec<RelationRef>> {
    let keys = primary_keys(conn, schema).await?;
    let found = rows(
        conn,
        format!(
            "SELECT table_name, table_type, table_rows FROM information_schema.tables \
             WHERE table_schema = {} ORDER BY table_name",
            lit(schema)
        ),
    )
    .await?;
    Ok(found
        .iter()
        .map(|r| {
            let name = cell(r, 0);
            let kind = if cell(r, 1).contains("VIEW") {
                RelKind::View
            } else {
                RelKind::Table
            };
            RelationRef {
                schema: schema.to_string(),
                primary_key: keys.get(&name).cloned().unwrap_or_default(),
                estimate: r.get(2).cloned().flatten().and_then(|v| v.parse().ok()),
                name,
                kind,
            }
        })
        .collect())
}

pub(crate) async fn list_relations(session: &Arc<Session>, schema: String) -> Result<Vec<RelationRef>> {
    let mut conn = my(session).conn.lock().await;
    relations_in(&mut conn, &schema).await
}

pub(crate) async fn list_all_relations(session: &Arc<Session>) -> Result<Vec<RelationRef>> {
    let schemas = list_schemas(session).await?;
    let mut conn = my(session).conn.lock().await;
    let mut out = Vec::new();
    for s in schemas.iter().filter(|s| !s.system) {
        out.extend(relations_in(&mut conn, &s.name).await?);
    }
    Ok(out)
}

pub(crate) async fn list_columns(
    session: &Arc<Session>,
    schema: String,
    table: String,
) -> Result<Vec<(String, String)>> {
    let mut conn = my(session).conn.lock().await;
    let found = rows(
        &mut conn,
        format!(
            "SELECT column_name, column_type FROM information_schema.columns \
             WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
            lit(&schema),
            lit(&table)
        ),
    )
    .await?;
    Ok(found.iter().map(|r| (cell(r, 0), cell(r, 1))).collect())
}

pub(crate) async fn describe_relation(session: &Arc<Session>, schema: String, name: String) -> Result<RelationInfo> {
    let mut conn = my(session).conn.lock().await;
    let kind = rows(
        &mut conn,
        format!(
            "SELECT table_type FROM information_schema.tables WHERE table_schema = {} AND table_name = {}",
            lit(&schema),
            lit(&name)
        ),
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| CoreError::Invalid(format!("relation {schema}.{name} does not exist")))?;
    let kind = if cell(&kind, 0).contains("VIEW") {
        RelKind::View
    } else {
        RelKind::Table
    };

    // EXTRA: auto_increment, VIRTUAL GENERATED, STORED GENERATED, DEFAULT_GENERATED …
    let cols = rows(
        &mut conn,
        format!(
            "SELECT column_name, column_type, is_nullable, column_default, extra \
             FROM information_schema.columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
            lit(&schema),
            lit(&name)
        ),
    )
    .await?;
    let columns: Vec<ColumnInfo> = cols
        .iter()
        .map(|r| {
            let extra = cell(r, 4).to_ascii_lowercase();
            let generated = extra.contains("virtual generated") || extra.contains("stored generated");
            ColumnInfo {
                name: cell(r, 0),
                type_name: cell(r, 1),
                type_oid: 0,
                not_null: cell(r, 2) == "NO",
                has_default: r.get(3).is_some_and(Option::is_some) || extra.contains("auto_increment"),
                generated,
                identity_always: false,
            }
        })
        .collect();

    // The primary key, else a unique index over not-null columns.
    let indexes = rows(
        &mut conn,
        format!(
            "SELECT index_name, column_name FROM information_schema.statistics \
             WHERE table_schema = {} AND table_name = {} AND non_unique = 0 \
             ORDER BY index_name <> 'PRIMARY', index_name, seq_in_index",
            lit(&schema),
            lit(&name)
        ),
    )
    .await?;
    let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
    for r in indexes {
        let idx = cell(&r, 0);
        match grouped.last_mut() {
            Some((n, cols)) if *n == idx => cols.push(cell(&r, 1)),
            _ => grouped.push((idx, vec![cell(&r, 1)])),
        }
    }
    let row_key = grouped
        .into_iter()
        .find(|(idx, names)| {
            idx == "PRIMARY" || names.iter().all(|n| columns.iter().any(|c| &c.name == n && c.not_null))
        })
        .map(|(_, names)| names);

    let keys = rows(
        &mut conn,
        format!(
            "SELECT constraint_name, table_schema, table_name, column_name, \
                    referenced_table_schema, referenced_table_name, referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE referenced_table_name IS NOT NULL \
               AND ((table_schema = {s} AND table_name = {n}) \
                 OR (referenced_table_schema = {s} AND referenced_table_name = {n})) \
             ORDER BY table_schema, table_name, constraint_name, ordinal_position",
            s = lit(&schema),
            n = lit(&name)
        ),
    )
    .await?;
    let mut all: Vec<ForeignKey> = Vec::new();
    for r in keys {
        let same = all
            .last()
            .is_some_and(|f| f.name == cell(&r, 0) && f.from_schema == cell(&r, 1) && f.from_table == cell(&r, 2));
        if !same {
            all.push(ForeignKey {
                name: cell(&r, 0),
                from_schema: cell(&r, 1),
                from_table: cell(&r, 2),
                from_cols: Vec::new(),
                to_schema: cell(&r, 4),
                to_table: cell(&r, 5),
                to_cols: Vec::new(),
            });
        }
        let fk = all.last_mut().expect("pushed above");
        fk.from_cols.push(cell(&r, 3));
        fk.to_cols.push(cell(&r, 6));
    }
    let foreign_keys = all
        .iter()
        .filter(|f| f.from_schema == schema && f.from_table == name)
        .cloned()
        .collect();
    let referenced_by = all
        .into_iter()
        .filter(|f| f.to_schema == schema && f.to_table == name)
        .collect();
    Ok(RelationInfo {
        editable: kind.is_table() && row_key.is_some(),
        schema,
        name,
        kind,
        columns,
        row_key,
        foreign_keys,
        referenced_by,
    })
}

/// Runs the planned statements in one transaction; nothing stays if one fails. (Plain
/// row changes only: no DDL, which MySQL would commit on its own.)
pub(crate) async fn commit(
    session: &Arc<Session>,
    planned: Vec<Planned>,
    report: CommitReport,
) -> Result<CommitReport> {
    let mut conn = my(session).conn.lock().await;
    let mut tx = conn.start_transaction(TxOpts::default()).await?;
    for (i, p) in planned.iter().enumerate() {
        let params: Vec<Value> = p
            .params
            .iter()
            .map(|v| v.as_ref().map_or(Value::NULL, |s| Value::Bytes(s.clone().into_bytes())))
            .collect();
        let params = if params.is_empty() {
            Params::Empty
        } else {
            Params::Positional(params)
        };
        let result = tx.exec_drop(&p.sql, params).await;
        let failure = match result {
            Ok(()) if p.expect_one && tx.affected_rows() != 1 => Some(CoreError::Invalid(format!(
                "expected 1 row for {}, the statement affected {}",
                p.key_label.as_deref().unwrap_or("this row"),
                tx.affected_rows()
            ))),
            Ok(()) => None,
            Err(e) => Some(CoreError::from(e)),
        };
        if let Some(err) = failure {
            let _ = tx.rollback().await;
            return Err(CoreError::Commit {
                index: ChangeIndex(i),
                source: Box::new(err),
            });
        }
    }
    tx.commit().await?;
    Ok(report)
}

/// What the current database holds (at most four), for the "is the target empty?" check,
/// and its name.
pub(crate) async fn objects(session: &Arc<Session>) -> Result<(String, Vec<String>)> {
    let mut conn = my(session).conn.lock().await;
    let db: Option<String> = conn.query_first("SELECT DATABASE()").await?;
    let found = rows(
        &mut conn,
        "SELECT CONCAT(LOWER(table_type), ' ', table_name) FROM information_schema.tables WHERE table_schema = DATABASE() \
         UNION ALL SELECT CONCAT(LOWER(routine_type), ' ', routine_name) FROM information_schema.routines WHERE routine_schema = DATABASE() \
         UNION ALL SELECT CONCAT('event ', event_name) FROM information_schema.events WHERE event_schema = DATABASE() \
         LIMIT 4"
            .into(),
    )
    .await?;
    Ok((db.unwrap_or_default(), found.iter().map(|r| cell(r, 0)).collect()))
}
