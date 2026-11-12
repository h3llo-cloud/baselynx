//! Change sets: preview and commit in one transaction.

use std::future::Future;
use std::sync::Arc;

use tokio_postgres::types::Type;

use crate::catalog::RelationInfo;
use crate::error::{ChangeIndex, CoreError, Result};
use crate::model::Engine;
use crate::runtime;
use crate::session::Session;
use crate::sql::{Dialect, quote_literal};

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

/// One statement to run, with its parameters as text, and the same statement with the
/// values written in for the review dialog.
pub(crate) struct Planned {
    pub sql: String,
    pub preview: String,
    pub params: Vec<Option<String>>,
    /// UPDATE and DELETE must affect exactly one row.
    pub expect_one: bool,
    pub key_label: Option<String>,
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

/// One value: the placeholder for the statement and the literal for the preview. Postgres
/// sends text and casts it to the column's type; MySQL and SQLite convert text parameters
/// themselves.
fn bind(d: Dialect, params: &mut Vec<Option<String>>, value: Option<&str>, type_name: &str) -> (String, String) {
    params.push(value.map(str::to_string));
    let n = params.len();
    match d {
        Dialect::Postgres => (
            format!("${n}::text::{type_name}"),
            match value {
                Some(v) => format!("{}::{type_name}", quote_literal(v)),
                None => format!("NULL::{type_name}"),
            },
        ),
        Dialect::MySql | Dialect::Sqlite => (
            d.placeholder(n),
            value.map_or_else(|| "NULL".to_string(), |v| d.quote_literal(v)),
        ),
    }
}

pub(crate) fn plan(rel: &RelationInfo, changes: &ChangeSet, d: Dialect) -> Result<Vec<Planned>> {
    let target = d.qualified(&rel.schema, &rel.name);
    let mut out = Vec::with_capacity(changes.len());
    let where_key = |key: &[(String, String)], params: &mut Vec<Option<String>>| -> Result<(String, String)> {
        let (mut sql, mut prev) = (Vec::with_capacity(key.len()), Vec::with_capacity(key.len()));
        for (col, val) in key {
            let (ph, lit) = bind(d, params, Some(val), type_of(rel, col)?);
            sql.push(format!("{} = {ph}", d.quote_ident(col)));
            prev.push(format!("{} = {lit}", d.quote_ident(col)));
        }
        Ok((sql.join(" AND "), prev.join(" AND ")))
    };
    for u in &changes.updates {
        let mut params = Vec::new();
        let (mut sets, mut sets_prev) = (Vec::new(), Vec::new());
        for (col, val) in &u.set {
            let info = rel
                .column(col)
                .ok_or_else(|| CoreError::Invalid(format!("unknown column {col}")))?;
            if !info.writable() {
                return Err(CoreError::Invalid(format!("column {col} cannot be written")));
            }
            let (ph, lit) = bind(d, &mut params, val.as_option(), &info.type_name);
            sets.push(format!("{} = {ph}", d.quote_ident(col)));
            sets_prev.push(format!("{} = {lit}", d.quote_ident(col)));
        }
        if sets.is_empty() {
            continue;
        }
        let (w, w_prev) = where_key(&u.key, &mut params)?;
        out.push(Planned {
            sql: format!("UPDATE {target} SET {} WHERE {w}", sets.join(", ")),
            preview: format!("UPDATE {target} SET {} WHERE {w_prev}", sets_prev.join(", ")),
            params,
            expect_one: true,
            key_label: Some(key_label(&u.key)),
        });
    }
    for del in &changes.deletes {
        let mut params = Vec::new();
        let (w, w_prev) = where_key(&del.key, &mut params)?;
        out.push(Planned {
            sql: format!("DELETE FROM {target} WHERE {w}"),
            preview: format!("DELETE FROM {target} WHERE {w_prev}"),
            params,
            expect_one: true,
            key_label: Some(key_label(&del.key)),
        });
    }
    for ins in &changes.inserts {
        let values: Vec<_> = ins
            .values
            .iter()
            .filter(|(c, _)| rel.column(c).is_some_and(|i| i.writable()))
            .collect();
        if values.is_empty() {
            // MySQL has no DEFAULT VALUES.
            let sql = match d {
                Dialect::MySql => format!("INSERT INTO {target} () VALUES ()"),
                _ => format!("INSERT INTO {target} DEFAULT VALUES"),
            };
            out.push(Planned {
                preview: sql.clone(),
                sql,
                params: Vec::new(),
                expect_one: false,
                key_label: None,
            });
            continue;
        }
        let mut params = Vec::new();
        let (mut cols, mut vals, mut vals_prev) = (Vec::new(), Vec::new(), Vec::new());
        for (col, val) in values {
            let (ph, lit) = bind(d, &mut params, val.as_option(), type_of(rel, col)?);
            cols.push(d.quote_ident(col));
            vals.push(ph);
            vals_prev.push(lit);
        }
        out.push(Planned {
            sql: format!(
                "INSERT INTO {target} ({}) VALUES ({})",
                cols.join(", "),
                vals.join(", ")
            ),
            preview: format!(
                "INSERT INTO {target} ({}) VALUES ({})",
                cols.join(", "),
                vals_prev.join(", ")
            ),
            params,
            expect_one: false,
            key_label: None,
        });
    }
    Ok(out)
}

/// The statements with literal values, for the review dialog. Never executed.
pub fn preview(rel: &RelationInfo, changes: &ChangeSet, dialect: Dialect) -> Result<Vec<String>> {
    Ok(plan(rel, changes, dialect)?.into_iter().map(|p| p.preview).collect())
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
    let planned = plan(rel, &changes, session.dialect());
    let report = CommitReport {
        updated: changes.updates.iter().filter(|u| !u.set.is_empty()).count(),
        deleted: changes.deletes.len(),
        inserted: changes.inserts.len(),
    };
    runtime::spawn(async move {
        let planned = planned?;
        match session.engine() {
            Engine::Sqlite => return crate::engine::sqlite::commit(&session, planned, report).await,
            Engine::Mysql => return crate::engine::mysql::commit(&session, planned, report).await,
            Engine::Postgres => {}
        }
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
        Ok(report)
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
        let p = preview(&rel(), &changes, Dialect::Postgres).unwrap();
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
        assert!(preview(&rel(), &changes, Dialect::Postgres).is_err());
    }

    #[test]
    fn other_engines() {
        let changes = ChangeSet {
            updates: vec![RowUpdate {
                key: vec![("id".into(), "7".into())],
                set: vec![("name".into(), CellValue::Text(r"a\b".into()))],
            }],
            inserts: vec![RowInsert { values: vec![] }],
            ..Default::default()
        };
        let my = plan(&rel(), &changes, Dialect::MySql).unwrap();
        assert_eq!(my[0].sql, "UPDATE `demo`.`t` SET `name` = ? WHERE `id` = ?");
        // MySQL reads backslashes as escapes, so the preview doubles them.
        assert_eq!(my[0].preview, r"UPDATE `demo`.`t` SET `name` = 'a\\b' WHERE `id` = '7'");
        assert_eq!(my[0].params, vec![Some(r"a\b".to_string()), Some("7".to_string())]);
        assert_eq!(my[1].sql, "INSERT INTO `demo`.`t` () VALUES ()");
        let lite = plan(&rel(), &changes, Dialect::Sqlite).unwrap();
        assert_eq!(lite[0].sql, "UPDATE \"demo\".\"t\" SET \"name\" = ? WHERE \"id\" = ?");
        assert_eq!(
            lite[0].preview,
            r#"UPDATE "demo"."t" SET "name" = 'a\b' WHERE "id" = '7'"#
        );
        assert_eq!(lite[1].sql, "INSERT INTO \"demo\".\"t\" DEFAULT VALUES");
        // Postgres numbers its parameters; past nine they stay apart.
        let many = ChangeSet {
            inserts: vec![RowInsert {
                values: (0..11)
                    .map(|_| ("name".to_string(), CellValue::Text("x".into())))
                    .collect(),
            }],
            ..Default::default()
        };
        let pg = plan(&rel(), &many, Dialect::Postgres).unwrap();
        assert!(pg[0].sql.contains("$10::text::text, $11::text::text"), "{}", pg[0].sql);
    }
}
