//! Schemas, relations, columns and row keys: from `pg_catalog` here, from each engine's
//! own catalog in [`crate::engine`].
//!
//! Queries use unnamed statements (`query_typed`), which work through PgBouncer in
//! transaction pooling, as Kubernetes `-pooler` services run it.

use std::future::Future;
use std::sync::Arc;

use tokio_postgres::types::Type;

use crate::engine::sqlite;
use crate::error::Result;
use crate::model::Engine;
use crate::runtime;
use crate::session::Session;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaInfo {
    pub name: String,
    /// `pg_catalog` and `information_schema`: shown last and collapsed.
    pub system: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl RelKind {
    fn from_relkind(k: &str) -> Option<Self> {
        Some(match k {
            "r" => RelKind::Table,
            "p" => RelKind::PartitionedTable,
            "v" => RelKind::View,
            "m" => RelKind::MaterializedView,
            "f" => RelKind::ForeignTable,
            _ => return None,
        })
    }

    pub fn is_table(self) -> bool {
        matches!(self, RelKind::Table | RelKind::PartitionedTable)
    }

    pub fn label(self) -> &'static str {
        match self {
            RelKind::Table => "table",
            RelKind::PartitionedTable => "partitioned table",
            RelKind::View => "view",
            RelKind::MaterializedView => "materialized view",
            RelKind::ForeignTable => "foreign table",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationRef {
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    /// The planner's row estimate (`pg_class.reltuples`); `None` before the first ANALYZE
    /// and for views.
    pub estimate: Option<u64>,
    /// Primary key columns, empty when there is none (for `customers 42` in the palette).
    pub primary_key: Vec<String>,
}

/// A foreign key: `from` columns of one table reference `to` columns of another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub from_schema: String,
    pub from_table: String,
    pub from_cols: Vec<String>,
    pub to_schema: String,
    pub to_table: String,
    pub to_cols: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// `format_type(atttypid, atttypmod)`, usable as a cast target.
    pub type_name: String,
    pub type_oid: u32,
    pub not_null: bool,
    pub has_default: bool,
    pub generated: bool,
    pub identity_always: bool,
}

impl ColumnInfo {
    /// Generated and identity-always columns cannot be written.
    pub fn writable(&self) -> bool {
        !self.generated && !self.identity_always
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationInfo {
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    pub columns: Vec<ColumnInfo>,
    /// Columns that identify a row: the primary key, else the first suitable unique index.
    pub row_key: Option<Vec<String>>,
    pub editable: bool,
    /// Keys of this table that reference other tables.
    pub foreign_keys: Vec<ForeignKey>,
    /// Keys of other tables that reference this one.
    pub referenced_by: Vec<ForeignKey>,
}

impl RelationInfo {
    /// The first foreign key this column takes part in.
    pub fn foreign_key_of(&self, column: &str) -> Option<&ForeignKey> {
        self.foreign_keys
            .iter()
            .find(|f| f.from_cols.iter().any(|c| c == column))
    }

    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn is_key_column(&self, name: &str) -> bool {
        self.row_key.as_ref().is_some_and(|k| k.iter().any(|c| c == name))
    }

    /// Why the relation cannot be edited, for the read-only notice.
    pub fn read_only_reason(&self) -> Option<&'static str> {
        if self.editable {
            None
        } else if !self.kind.is_table() {
            Some("Read-only: this is a view or foreign table.")
        } else {
            Some("Read-only: the table has no primary key or unique not-null index.")
        }
    }
}

/// Primary key columns of relation `c`, in key order.
const PRIMARY_KEY_COLS: &str = "ARRAY(SELECT a.attname::text FROM pg_catalog.pg_index i \
     CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) \
     JOIN pg_catalog.pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
     WHERE i.indrelid = c.oid AND i.indisprimary ORDER BY k.ord)";

const SYSTEM_FILTER: &str =
    "n.nspname <> 'pg_toast' AND n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%'";

pub fn list_schemas(session: &Arc<Session>) -> impl Future<Output = Result<Vec<SchemaInfo>>> + Send + 'static {
    let session = session.clone();
    runtime::spawn(async move {
        if session.engine() == Engine::Sqlite {
            return sqlite::list_schemas(&session).await;
        }
        let sql = format!(
            "SELECT n.nspname::text, (n.nspname IN ('pg_catalog', 'information_schema')) AS system \
             FROM pg_catalog.pg_namespace n WHERE {SYSTEM_FILTER} ORDER BY 2, 1"
        );
        let rows = session.client().query_typed(&sql, &[]).await?;
        Ok(rows
            .iter()
            .map(|r| SchemaInfo {
                name: r.get(0),
                system: r.get(1),
            })
            .collect())
    })
}

pub fn list_relations(
    session: &Arc<Session>,
    schema: &str,
) -> impl Future<Output = Result<Vec<RelationRef>>> + Send + 'static {
    let session = session.clone();
    let schema = schema.to_string();
    runtime::spawn(async move {
        if session.engine() == Engine::Sqlite {
            return sqlite::list_relations(&session, schema).await;
        }
        let sql = format!(
            "SELECT n.nspname::text, c.relname::text, c.relkind::text, c.reltuples::float8, {PRIMARY_KEY_COLS} \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
             ORDER BY c.relname"
        );
        let rows = session.client().query_typed(&sql, &[(&schema, Type::TEXT)]).await?;
        Ok(rows.iter().filter_map(relation_ref).collect())
    })
}

/// Every relation outside the system schemas, for the command palette.
pub fn list_all_relations(session: &Arc<Session>) -> impl Future<Output = Result<Vec<RelationRef>>> + Send + 'static {
    let session = session.clone();
    runtime::spawn(async move {
        if session.engine() == Engine::Sqlite {
            return sqlite::list_all_relations(&session).await;
        }
        let sql = format!(
            "SELECT n.nspname::text, c.relname::text, c.relkind::text, c.reltuples::float8, {PRIMARY_KEY_COLS} \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND {SYSTEM_FILTER} \
               AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
             ORDER BY 1, 2"
        );
        let rows = session.client().query_typed(&sql, &[]).await?;
        Ok(rows.iter().filter_map(relation_ref).collect())
    })
}

fn relation_ref(r: &tokio_postgres::Row) -> Option<RelationRef> {
    let tuples: f64 = r.get(3);
    Some(RelationRef {
        schema: r.get(0),
        name: r.get(1),
        kind: RelKind::from_relkind(r.get::<_, &str>(2))?,
        estimate: (tuples >= 0.0).then_some(tuples as u64),
        primary_key: r.get(4),
    })
}

/// Column names and types of one relation, in order: one query, for completion.
pub fn list_columns(
    session: &Arc<Session>,
    schema: &str,
    name: &str,
) -> impl Future<Output = Result<Vec<(String, String)>>> + Send + 'static {
    let session = session.clone();
    let schema = schema.to_string();
    let name = name.to_string();
    runtime::spawn(async move {
        if session.engine() == Engine::Sqlite {
            return sqlite::list_columns(&session, schema, name).await;
        }
        let rows = session
            .client()
            .query_typed(
                "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod) \
                 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[(&schema, Type::TEXT), (&name, Type::TEXT)],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    })
}

/// What the planner knows about a column from the last ANALYZE. Read from
/// `pg_stats`: instant, and never a scan of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// Share of NULLs, 0 to 1.
    pub null_frac: f32,
    /// Distinct values, estimated.
    pub distinct: Option<u64>,
    /// Most common values with their share, most common first.
    pub common: Vec<(String, f32)>,
}

/// Splits PostgreSQL's text form of a one-dimensional array: `{a,"b c",NULL}`.
pub fn parse_text_array(text: &str) -> Vec<Option<String>> {
    let inner = text
        .trim()
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or("");
    let mut out = Vec::new();
    if inner.is_empty() {
        return out;
    }
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '\\' if quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' if !quoted => {
                let v = std::mem::take(&mut cur);
                out.push(if !was_quoted && v == "NULL" { None } else { Some(v) });
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    out.push(if !was_quoted && cur == "NULL" { None } else { Some(cur) });
    out
}

/// `None` when the table was never analysed (or the column has no statistics).
pub fn column_stats(
    session: &Arc<Session>,
    schema: &str,
    table: &str,
    column: &str,
) -> impl Future<Output = Result<Option<ColumnStats>>> + Send + 'static {
    let session = session.clone();
    let (schema, table, column) = (schema.to_string(), table.to_string(), column.to_string());
    runtime::spawn(async move {
        if session.engine() == Engine::Sqlite {
            return Ok(None);
        }
        let rows = session
            .client()
            .query_typed(
                "SELECT s.null_frac::float8, s.n_distinct::float8, s.most_common_vals::text, \
                        s.most_common_freqs::float8[], c.reltuples::float8 \
                 FROM pg_catalog.pg_stats s \
                 JOIN pg_catalog.pg_namespace n ON n.nspname = s.schemaname \
                 JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = s.tablename \
                 WHERE s.schemaname = $1 AND s.tablename = $2 AND s.attname = $3 \
                 LIMIT 1",
                &[(&schema, Type::TEXT), (&table, Type::TEXT), (&column, Type::TEXT)],
            )
            .await?;
        let Some(r) = rows.first() else { return Ok(None) };
        let null_frac: f64 = r.get(0);
        let n_distinct: f64 = r.get(1);
        let vals: Option<String> = r.get(2);
        let freqs: Option<Vec<f64>> = r.get(3);
        let tuples: f64 = r.get(4);
        // Negative n_distinct is a share of the rows.
        let distinct = if n_distinct >= 0.0 {
            Some(n_distinct as u64)
        } else if tuples > 0.0 {
            Some((-n_distinct * tuples).round() as u64)
        } else {
            None
        };
        let common = vals
            .map(|v| parse_text_array(&v))
            .unwrap_or_default()
            .into_iter()
            .zip(freqs.unwrap_or_default())
            .map(|(v, f)| (v.unwrap_or_else(|| "NULL".into()), f as f32))
            .collect();
        Ok(Some(ColumnStats {
            null_frac: null_frac as f32,
            distinct,
            common,
        }))
    })
}

pub fn describe_relation(
    session: &Arc<Session>,
    schema: &str,
    name: &str,
) -> impl Future<Output = Result<RelationInfo>> + Send + 'static {
    let session = session.clone();
    let schema = schema.to_string();
    let name = name.to_string();
    runtime::spawn(async move {
        if session.engine() == Engine::Sqlite {
            return sqlite::describe_relation(&session, schema, name).await;
        }
        let client = session.client();
        let rel = client
            .query_typed(
                "SELECT c.oid, c.relkind::text FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[(&schema, Type::TEXT), (&name, Type::TEXT)],
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| crate::error::CoreError::Invalid(format!("relation {schema}.{name} does not exist")))?;
        let oid: u32 = rel.get(0);
        let kind = RelKind::from_relkind(rel.get::<_, &str>(1))
            .ok_or_else(|| crate::error::CoreError::Invalid(format!("{schema}.{name} is not a table or view")))?;

        let cols = client
            .query_typed(
                "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), a.atttypid, \
                        a.attnotnull, a.atthasdef, a.attgenerated::text <> '', a.attidentity::text = 'a' \
                 FROM pg_catalog.pg_attribute a \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[(&oid, Type::OID)],
            )
            .await?;
        let columns: Vec<ColumnInfo> = cols
            .iter()
            .map(|r| ColumnInfo {
                name: r.get(0),
                type_name: r.get(1),
                type_oid: r.get(2),
                not_null: r.get(3),
                has_default: r.get(4),
                generated: r.get(5),
                identity_always: r.get(6),
            })
            .collect();

        // Primary key first, then unique indexes by name. Key columns only (not INCLUDE).
        let indexes = client
            .query_typed(
                "SELECT i.indisprimary, (i.indpred IS NULL AND i.indexprs IS NULL AND i.indisvalid), \
                        ARRAY(SELECT a.attname::text \
                              FROM unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) \
                              JOIN pg_catalog.pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                              WHERE k.ord <= i.indnkeyatts ORDER BY k.ord) \
                 FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
                 WHERE i.indrelid = $1 AND (i.indisprimary OR i.indisunique) \
                 ORDER BY i.indisprimary DESC, ic.relname",
                &[(&oid, Type::OID)],
            )
            .await?;
        let mut row_key = None;
        for r in &indexes {
            let primary: bool = r.get(0);
            let plain: bool = r.get(1);
            let names: Vec<String> = r.get(2);
            if names.is_empty() {
                continue;
            }
            if primary {
                row_key = Some(names);
                break;
            }
            let all_not_null = names.iter().all(|n| columns.iter().any(|c| &c.name == n && c.not_null));
            if plain && all_not_null {
                row_key = Some(names);
                break;
            }
        }
        let editable = kind.is_table() && row_key.is_some();

        // Foreign keys in both directions; constraints cloned onto partitions are skipped.
        let keys = client
            .query_typed(
                "SELECT c.conname::text, nf.nspname::text, cf.relname::text, \
                        ARRAY(SELECT a.attname::text FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) \
                              JOIN pg_catalog.pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum \
                              ORDER BY k.ord), \
                        nt.nspname::text, ct.relname::text, \
                        ARRAY(SELECT a.attname::text FROM unnest(c.confkey) WITH ORDINALITY AS k(attnum, ord) \
                              JOIN pg_catalog.pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.attnum \
                              ORDER BY k.ord) \
                 FROM pg_catalog.pg_constraint c \
                 JOIN pg_catalog.pg_class cf ON cf.oid = c.conrelid \
                 JOIN pg_catalog.pg_namespace nf ON nf.oid = cf.relnamespace \
                 JOIN pg_catalog.pg_class ct ON ct.oid = c.confrelid \
                 JOIN pg_catalog.pg_namespace nt ON nt.oid = ct.relnamespace \
                 WHERE c.contype = 'f' AND c.conparentid = 0 AND (c.conrelid = $1 OR c.confrelid = $1) \
                 ORDER BY nf.nspname, cf.relname, c.conname",
                &[(&oid, Type::OID)],
            )
            .await?;
        let mut foreign_keys = Vec::new();
        let mut referenced_by = Vec::new();
        for r in &keys {
            let fk = ForeignKey {
                name: r.get(0),
                from_schema: r.get(1),
                from_table: r.get(2),
                from_cols: r.get(3),
                to_schema: r.get(4),
                to_table: r.get(5),
                to_cols: r.get(6),
            };
            if fk.from_schema == schema && fk.from_table == name {
                foreign_keys.push(fk.clone());
            }
            if fk.to_schema == schema && fk.to_table == name {
                referenced_by.push(fk);
            }
        }
        Ok(RelationInfo {
            schema,
            name,
            kind,
            columns,
            row_key,
            editable,
            foreign_keys,
            referenced_by,
        })
    })
}
