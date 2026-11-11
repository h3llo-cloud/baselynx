//! Change sets: preview and commit in one transaction.

use std::future::Future;
use std::sync::Arc;

use tokio_postgres::types::Type;

use crate::catalog::RelationInfo;
use crate::error::{ChangeIndex, CoreError, Result};
use crate::runtime;
use crate::session::Session;
use crate::sql::{quote_ident, quote_literal, quote_qualified};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CellValue {
    Null,
    Text(String),
}

impl CellValue {
    pub fn from_option(v: Option<String>) -> Self {
        match v {
            Some(s) => CellValue::Text(s),
            None => CellValue::Null,
        }
    }

    pub fn as_option(&self) -> Option<&str> {
        match self {
            CellValue::Null => None,
            CellValue::Text(s) => Some(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowUpdate {
    /// Row key values as loaded, not as edited.
    pub key: Vec<(String, String)>,
    pub set: Vec<(String, CellValue)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowDelete {
    pub key: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowInsert {
    /// Only the columns the user filled in.
    pub values: Vec<(String, CellValue)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub updates: Vec<RowUpdate>,
    pub deletes: Vec<RowDelete>,
    pub inserts: Vec<RowInsert>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.updates.len() + self.deletes.len() + self.inserts.len()
    }

    /// "3 updates, 2 deletes, 1 insert"
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let mut push = |n: usize, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        push(self.updates.len(), "update", "updates");
        push(self.deletes.len(), "delete", "deletes");
        push(self.inserts.len(), "insert", "inserts");
        if parts.is_empty() {
            "no changes".into()
        } else {
            parts.join(", ")
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitReport {
    pub updated: usize,
    pub deleted: usize,
    pub inserted: usize,
}

/// One statement to run: SQL with `$n` text parameters.
struct Planned {
    sql: String,
    params: Vec<Option<String>>,
    /// UPDATE and DELETE must affect exactly one row.
    expect_one: bool,
    key_label: Option<String>,
}

fn type_of<'a>(rel: &'a RelationInfo, column: &str) -> Result<&'a str> {
    rel.column(column)
        .map(|c| c.type_name.as_str())
        .ok_or_else(|| CoreError::Invalid(format!("unknown column {column}")))
}

fn key_label(key: &[(String, String)]) -> String {
    key.iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn plan(rel: &RelationInfo, changes: &ChangeSet) -> Result<Vec<Planned>> {
    let target = quote_qualified(&rel.schema, &rel.name);
    let mut out = Vec::with_capacity(changes.len());
    let where_key = |key: &[(String, String)], params: &mut Vec<Option<String>>| -> Result<String> {
        let mut parts = Vec::with_capacity(key.len());
        for (col, val) in key {
            params.push(Some(val.clone()));
            parts.push(format!(
                "{} = ${}::text::{}",
                quote_ident(col),
                params.len(),
                type_of(rel, col)?
            ));
        }
        Ok(parts.join(" AND "))
    };
    for u in &changes.updates {
        let mut params = Vec::new();
        let mut sets = Vec::new();
        for (col, val) in &u.set {
            let info = rel
                .column(col)
                .ok_or_else(|| CoreError::Invalid(format!("unknown column {col}")))?;
            if !info.writable() {
                return Err(CoreError::Invalid(format!("column {col} cannot be written")));
            }
            params.push(val.as_option().map(str::to_string));
            sets.push(format!(
                "{} = ${}::text::{}",
                quote_ident(col),
                params.len(),
                info.type_name
            ));
        }
        if sets.is_empty() {
            continue;
        }
        let w = where_key(&u.key, &mut params)?;
        out.push(Planned {
            sql: format!("UPDATE {target} SET {} WHERE {w}", sets.join(", ")),
            params,
            expect_one: true,
            key_label: Some(key_label(&u.key)),
        });
    }
    for d in &changes.deletes {
        let mut params = Vec::new();
        let w = where_key(&d.key, &mut params)?;
        out.push(Planned {
            sql: format!("DELETE FROM {target} WHERE {w}"),
            params,
            expect_one: true,
            key_label: Some(key_label(&d.key)),
        });
    }
    for ins in &changes.inserts {
        let values: Vec<_> = ins
            .values
            .iter()
            .filter(|(c, _)| rel.column(c).is_some_and(|i| i.writable()))
            .collect();
        if values.is_empty() {
            out.push(Planned {
                sql: format!("INSERT INTO {target} DEFAULT VALUES"),
                params: Vec::new(),
                expect_one: false,
                key_label: None,
            });
            continue;
        }
        let mut params = Vec::new();
        let mut cols = Vec::new();
        let mut vals = Vec::new();
        for (col, val) in values {
            params.push(val.as_option().map(str::to_string));
            cols.push(quote_ident(col));
            vals.push(format!("${}::text::{}", params.len(), type_of(rel, col)?));
        }
        out.push(Planned {
            sql: format!(
                "INSERT INTO {target} ({}) VALUES ({})",
                cols.join(", "),
                vals.join(", ")
            ),
            params,
            expect_one: false,
            key_label: None,
        });
    }
    Ok(out)
}

/// The statements with literal values, for the review dialog. Never executed.
pub fn preview(rel: &RelationInfo, changes: &ChangeSet) -> Result<Vec<String>> {
    Ok(plan(rel, changes)?
        .into_iter()
        .map(|p| inline_params(&p.sql, &p.params))
        .collect())
}

/// Replaces `$n::text::` with the literal. Only used on SQL this module generated.
fn inline_params(sql: &str, params: &[Option<String>]) -> String {
    let mut out = sql.to_string();
    // Replace from the highest index so `$1` does not match the start of `$10`.
    for (i, p) in params.iter().enumerate().rev() {
        let needle = format!("${}::text::", i + 1);
        let lit = match p {
            Some(v) => format!("{}::", quote_literal(v)),
            None => "NULL::".to_string(),
        };
        out = out.replacen(&needle, &lit, 1);
    }
    out
}

async fn run_typed(
    client: &tokio_postgres::Client,
    sql: &str,
    params: &[Option<String>],
) -> std::result::Result<u64, tokio_postgres::Error> {
    let stream = client
        .query_typed_raw(sql, params.iter().map(|v| (v, Type::TEXT)))
        .await?;
    futures_util::pin_mut!(stream);
    while futures_util::StreamExt::next(&mut stream).await.transpose()?.is_some() {}
    Ok(stream.rows_affected().unwrap_or(0))
}

pub fn commit(
    session: &Arc<Session>,
    rel: &RelationInfo,
    changes: ChangeSet,
) -> impl Future<Output = Result<CommitReport>> + Send + 'static {
    let session = session.clone();
    let planned = plan(rel, &changes);
    runtime::spawn(async move {
        let planned = planned?;
        let client = session.client();
        client.batch_execute("BEGIN").await?;
        for (i, p) in planned.iter().enumerate() {
            // Unnamed statements: they work through PgBouncer in transaction pooling.
            let result = run_typed(client, &p.sql, &p.params).await;
            let failure = match result {
                Ok(n) if p.expect_one && n != 1 => Some(CoreError::Invalid(format!(
                    "expected 1 row for {}, the statement affected {n}",
                    p.key_label.as_deref().unwrap_or("this row")
                ))),
                Ok(_) => None,
                Err(e) => Some(CoreError::from(e)),
            };
            if let Some(err) = failure {
                let _ = client.batch_execute("ROLLBACK").await;
                return Err(CoreError::Commit {
                    index: ChangeIndex(i),
                    source: Box::new(err),
                });
            }
        }
        if let Err(e) = client.batch_execute("COMMIT").await {
            let _ = client.batch_execute("ROLLBACK").await;
            return Err(e.into());
        }
        Ok(CommitReport {
            updated: changes.updates.iter().filter(|u| !u.set.is_empty()).count(),
            deleted: changes.deletes.len(),
            inserted: changes.inserts.len(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnInfo, RelKind};

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_name: ty.into(),
            type_oid: 0,
            not_null: false,
            has_default: false,
            generated: false,
            identity_always: false,
        }
    }

    fn rel() -> RelationInfo {
        let mut gen_col = col("g", "integer");
        gen_col.generated = true;
        RelationInfo {
            schema: "demo".into(),
            name: "t".into(),
            kind: RelKind::Table,
            columns: vec![
                col("id", "integer"),
                col("name", "text"),
                col("note", "character varying(20)"),
                gen_col,
            ],
            row_key: Some(vec!["id".into()]),
            editable: true,
            foreign_keys: vec![],
            referenced_by: vec![],
        }
    }

    #[test]
    fn preview_statements() {
        let changes = ChangeSet {
            updates: vec![RowUpdate {
                key: vec![("id".into(), "7".into())],
                set: vec![
                    ("name".into(), CellValue::Text("O'Brien".into())),
                    ("note".into(), CellValue::Null),
                ],
            }],
            deletes: vec![RowDelete {
                key: vec![("id".into(), "9".into())],
            }],
            inserts: vec![
                RowInsert {
                    values: vec![
                        ("name".into(), CellValue::Text("x".into())),
                        ("g".into(), CellValue::Text("1".into())),
                    ],
                },
                RowInsert { values: vec![] },
            ],
        };
        let p = preview(&rel(), &changes).unwrap();
        assert_eq!(
            p,
            vec![
                "UPDATE \"demo\".\"t\" SET \"name\" = 'O''Brien'::text, \"note\" = NULL::character varying(20) WHERE \"id\" = '7'::integer",
                "DELETE FROM \"demo\".\"t\" WHERE \"id\" = '9'::integer",
                "INSERT INTO \"demo\".\"t\" (\"name\") VALUES ('x'::text)",
                "INSERT INTO \"demo\".\"t\" DEFAULT VALUES",
            ]
        );
        assert_eq!(changes.summary(), "1 update, 1 delete, 2 inserts");
    }

    #[test]
    fn generated_column_cannot_be_updated() {
        let changes = ChangeSet {
            updates: vec![RowUpdate {
                key: vec![("id".into(), "1".into())],
                set: vec![("g".into(), CellValue::Text("2".into()))],
            }],
            ..Default::default()
        };
        assert!(preview(&rel(), &changes).is_err());
    }

    #[test]
    fn inline_many_params() {
        let params: Vec<Option<String>> = (1..=11).map(|i| Some(i.to_string())).collect();
        let sql = (1..=11)
            .map(|i| format!("${i}::text::int"))
            .collect::<Vec<_>>()
            .join(",");
        let out = inline_params(&sql, &params);
        assert_eq!(
            out,
            (1..=11).map(|i| format!("'{i}'::int")).collect::<Vec<_>>().join(",")
        );
    }
}
