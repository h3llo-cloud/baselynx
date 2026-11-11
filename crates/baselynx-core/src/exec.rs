//! Running statements and streaming rows as text.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, pin_mut};
use tokio::sync::mpsc;
use tokio_postgres::SimpleQueryMessage;

use crate::error::{CoreError, Result};
use crate::runtime;
use crate::session::Session;

pub const BATCH_ROWS: usize = 256;
pub const CHANNEL_CAPACITY: usize = 1024 / BATCH_ROWS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    pub type_oid: Option<u32>,
    pub type_name: Option<String>,
}

impl ColumnMeta {
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_oid: None,
            type_name: None,
        }
    }
}

/// A row: every value as PostgreSQL prints it, `None` for `NULL`.
pub type Row = Vec<Option<String>>;

#[derive(Debug, Clone, PartialEq)]
pub enum ExecEvent {
    Columns(Vec<ColumnMeta>),
    Rows(Vec<Row>),
    Done {
        /// Rows returned (for statements that return rows) or affected (others).
        count: u64,
        /// True when the statement returned a row description.
        returned_rows: bool,
        elapsed: Duration,
        truncated: bool,
    },
}

/// Runs one statement on `session` and streams its events. Dropping the receiver cancels
/// the statement on the server. With `row_cap`, the statement is cancelled once that
/// many rows have arrived and `Done` reports `truncated: true`.
pub fn execute(session: &Arc<Session>, sql: String, row_cap: Option<usize>) -> mpsc::Receiver<Result<ExecEvent>> {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY.max(2));
    let session = session.clone();
    runtime::runtime().spawn(async move {
        if let Err(e) = run(&session, &sql, row_cap, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    rx
}

/// Column types for a statement, from a prepared (never executed) statement. Any failure
/// leaves the types unknown; the run then reports the real error.
async fn column_types(session: &Session, sql: &str) -> Option<Vec<(u32, String)>> {
    if !session.named_statements_ok() {
        return None;
    }
    let stmt = match session.client().prepare(sql).await {
        Ok(s) => s,
        Err(e) => {
            if is_pooler_error(&CoreError::from(e)) {
                session.mark_pooled();
            }
            return None;
        }
    };
    Some(
        stmt.columns()
            .iter()
            .map(|c| (c.type_().oid(), c.type_().name().to_string()))
            .collect(),
    )
}

/// Errors a transaction-mode pooler gives for named prepared statements: the statement is
/// unknown on the next server connection (26000), already exists there (42P05), or the
/// pooler refuses the protocol message (08P01).
pub fn is_pooler_error(e: &CoreError) -> bool {
    matches!(e.sqlstate(), Some("26000" | "42P05" | "08P01"))
}

async fn run(
    session: &Arc<Session>,
    sql: &str,
    row_cap: Option<usize>,
    tx: &mpsc::Sender<Result<ExecEvent>>,
) -> Result<()> {
    let start = Instant::now();
    let types = column_types(session, sql).await;
    if session.is_closed() {
        return Err(CoreError::Closed);
    }
    let stream = session.client().simple_query_raw(sql).await?;
    pin_mut!(stream);

    let mut batch: Vec<Row> = Vec::with_capacity(BATCH_ROWS);
    let mut received = 0usize;
    let mut returned_rows = false;
    let mut capped = false;
    let mut cancel_sent = false;
    let mut count = 0u64;

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                let err = CoreError::from(e);
                if (capped || cancel_sent) && err.is_cancel() {
                    break;
                }
                return Err(err);
            }
        };
        match msg {
            SimpleQueryMessage::RowDescription(cols) => {
                returned_rows = true;
                let metas = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let t = types.as_ref().and_then(|t| t.get(i));
                        ColumnMeta {
                            name: c.name().to_string(),
                            type_oid: t.map(|t| t.0),
                            type_name: t.map(|t| t.1.clone()),
                        }
                    })
                    .collect();
                if tx.send(Ok(ExecEvent::Columns(metas))).await.is_err() {
                    let _ = session.cancel().await;
                    return Ok(());
                }
            }
            SimpleQueryMessage::Row(row) => {
                if capped {
                    continue;
                }
                let values: Row = (0..row.len()).map(|i| row.get(i).map(str::to_string)).collect();
                batch.push(values);
                received += 1;
                if batch.len() >= BATCH_ROWS && tx.send(Ok(ExecEvent::Rows(std::mem::take(&mut batch)))).await.is_err()
                {
                    let _ = session.cancel().await;
                    return Ok(());
                }
                if row_cap.is_some_and(|cap| received >= cap) {
                    capped = true;
                    if !cancel_sent {
                        cancel_sent = true;
                        let _ = session.cancel().await;
                    }
                }
            }
            SimpleQueryMessage::CommandComplete(n) => count = n,
            _ => {}
        }
    }
    if !batch.is_empty() && tx.send(Ok(ExecEvent::Rows(batch))).await.is_err() {
        return Ok(());
    }
    if capped {
        count = received as u64;
    }
    let _ = tx
        .send(Ok(ExecEvent::Done {
            count,
            returned_rows,
            elapsed: start.elapsed(),
            truncated: capped,
        }))
        .await;
    Ok(())
}

/// Collected result of a statement, for callers that want everything at once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Collected {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    pub count: u64,
    pub returned_rows: bool,
    pub elapsed: Duration,
    pub truncated: bool,
}

/// Runs a statement and gathers all its events.
pub async fn execute_collect(session: &Arc<Session>, sql: String, row_cap: Option<usize>) -> Result<Collected> {
    let mut rx = execute(session, sql, row_cap);
    let mut out = Collected::default();
    while let Some(ev) = rx.recv().await {
        match ev? {
            ExecEvent::Columns(c) => out.columns = c,
            ExecEvent::Rows(mut r) => out.rows.append(&mut r),
            ExecEvent::Done {
                count,
                returned_rows,
                elapsed,
                truncated,
            } => {
                out.count = count;
                out.returned_rows = returned_rows;
                out.elapsed = elapsed;
                out.truncated = truncated;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;

    fn db(code: &str) -> CoreError {
        CoreError::Db(DbError {
            message: "x".into(),
            code: code.into(),
            detail: None,
            hint: None,
            position: None,
        })
    }

    #[test]
    fn pooler_errors() {
        assert!(is_pooler_error(&db("26000")));
        assert!(is_pooler_error(&db("42P05")));
        assert!(is_pooler_error(&db("08P01")));
        assert!(!is_pooler_error(&db("42601")));
        assert!(!is_pooler_error(&CoreError::Closed));
    }
}
