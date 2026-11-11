//! Table pages: the generated SELECT, filters and counts.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::catalog::RelationInfo;
use crate::edit::CellValue;
use crate::error::{CoreError, Result};
use crate::exec::{self, ColumnMeta, Row};
use crate::model::SortDir;
use crate::runtime;
use crate::session::Session;
use crate::sql::{has_top_level_semicolon, quote_ident, quote_literal, quote_qualified, split};

pub const PAGE_SIZES: [u32; 4] = [100, 500, 1000, 5000];
pub const DEFAULT_PAGE_SIZE: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub schema: String,
    pub table: String,
    /// Raw text after `WHERE`.
    pub filter: Option<String>,
    pub sort: Option<(String, SortDir)>,
    /// Zero-based.
    pub page: u64,
    pub page_size: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    pub has_next: bool,
    pub elapsed: Duration,
}

/// Checks a filter before it is sent: one expression, no second statement.
pub fn validate_filter(filter: &str) -> Result<()> {
    if has_top_level_semicolon(filter) || split(filter).len() > 1 {
        return Err(CoreError::Invalid(
            "The filter must be a single condition without a semicolon.".into(),
        ));
    }
    Ok(())
}

fn normalized_filter(filter: &Option<String>) -> Option<&str> {
    filter.as_deref().map(str::trim).filter(|f| !f.is_empty())
}

/// `SELECT * FROM … WHERE (…) ORDER BY …`, optionally with `LIMIT page_size + 1 OFFSET …`.
pub fn build_select(req: &PageRequest, row_key: Option<&[String]>, paged: bool) -> Result<String> {
    let mut sql = format!("SELECT * FROM {}", quote_qualified(&req.schema, &req.table));
    if let Some(f) = normalized_filter(&req.filter) {
        validate_filter(f)?;
        sql.push_str(&format!(" WHERE ({f})"));
    }
    let mut order: Vec<String> = Vec::new();
    if let Some((col, dir)) = &req.sort {
        let dir = match dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        order.push(format!("{} {dir} NULLS LAST", quote_ident(col)));
    }
    if let Some(key) = row_key {
        for k in key {
            if req.sort.as_ref().is_some_and(|(c, _)| c == k) {
                continue;
            }
            order.push(format!("{} ASC", quote_ident(k)));
        }
    }
    if !order.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order.join(", "));
    }
    if paged {
        let size = req.page_size.max(1) as u64;
        sql.push_str(&format!(" LIMIT {} OFFSET {}", size + 1, req.page * size));
    }
    Ok(sql)
}

/// Loads one page. Column types come from `rel`.
pub fn load_page(
    session: &Arc<Session>,
    rel: &RelationInfo,
    req: PageRequest,
) -> impl Future<Output = Result<Page>> + Send + 'static {
    let session = session.clone();
    let built = build_select(&req, rel.row_key.as_deref(), true);
    let rel = rel.clone();
    runtime::spawn(async move {
        let sql = built?;
        let mut got = exec::execute_collect(&session, sql, None).await?;
        let size = req.page_size.max(1) as usize;
        let has_next = got.rows.len() > size;
        got.rows.truncate(size);
        let columns = got
            .columns
            .into_iter()
            .map(|mut c| {
                if let Some(info) = rel.column(&c.name) {
                    c.type_oid = Some(info.type_oid);
                    c.type_name = Some(info.type_name.clone());
                }
                c
            })
            .collect();
        Ok(Page {
            columns,
            rows: got.rows,
            has_next,
            elapsed: got.elapsed,
        })
    })
}

pub fn count_rows(
    session: &Arc<Session>,
    schema: &str,
    table: &str,
    filter: Option<String>,
) -> impl Future<Output = Result<u64>> + Send + 'static {
    let session = session.clone();
    let mut sql = format!("SELECT count(*) FROM {}", quote_qualified(schema, table));
    let filter_result = match normalized_filter(&filter) {
        Some(f) => validate_filter(f).map(|_| sql.push_str(&format!(" WHERE ({f})"))),
        None => Ok(()),
    };
    runtime::spawn(async move {
        filter_result?;
        let got = exec::execute_collect(&session, sql, None).await?;
        got.rows
            .first()
            .and_then(|r| r.first().cloned().flatten())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| CoreError::Internal("count returned no value".into()))
    })
}

/// One condition for "Filter by this value" / "Exclude this value".
pub fn quick_filter(column: &str, value: &CellValue, exclude: bool) -> String {
    let col = quote_ident(column);
    match (value, exclude) {
        (CellValue::Null, false) => format!("{col} IS NULL"),
        (CellValue::Null, true) => format!("{col} IS NOT NULL"),
        (CellValue::Text(v), false) => format!("{col} = {}", quote_literal(v)),
        (CellValue::Text(v), true) => format!("{col} <> {}", quote_literal(v)),
    }
}

/// The filter that finds the row a foreign key points at: `"a" = '1' AND "b" = 'x'`.
/// `None` when a value is NULL (the key then points nowhere) or the lists differ in length.
pub fn key_filter(columns: &[String], values: &[Option<String>]) -> Option<String> {
    if columns.is_empty() || columns.len() != values.len() {
        return None;
    }
    let parts: Option<Vec<String>> = columns
        .iter()
        .zip(values)
        .map(|(c, v)| v.as_ref().map(|v| quick_filter(c, &CellValue::Text(v.clone()), false)))
        .collect();
    parts.map(|p| p.join(" AND "))
}

/// Joins a condition to an existing filter with `AND`, keeping the existing one intact.
pub fn append_filter(existing: Option<&str>, condition: &str) -> String {
    match existing.map(str::trim).filter(|e| !e.is_empty()) {
        Some(e) => format!("({e}) AND {condition}"),
        None => condition.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> PageRequest {
        PageRequest {
            schema: "demo".into(),
            table: "Weird Name".into(),
            filter: None,
            sort: None,
            page: 0,
            page_size: 500,
        }
    }

    #[test]
    fn select_shapes() {
        let key = vec!["id".to_string()];
        assert_eq!(
            build_select(&req(), Some(&key), true).unwrap(),
            "SELECT * FROM \"demo\".\"Weird Name\" ORDER BY \"id\" ASC LIMIT 501 OFFSET 0"
        );
        let mut r = req();
        r.filter = Some("  a > 1 ".into());
        r.sort = Some(("select".into(), SortDir::Desc));
        r.page = 2;
        r.page_size = 100;
        assert_eq!(
            build_select(&r, Some(&key), true).unwrap(),
            "SELECT * FROM \"demo\".\"Weird Name\" WHERE (a > 1) ORDER BY \"select\" DESC NULLS LAST, \"id\" ASC LIMIT 101 OFFSET 200"
        );
        assert_eq!(
            build_select(&req(), None, false).unwrap(),
            "SELECT * FROM \"demo\".\"Weird Name\""
        );
        // sorting by the key column does not repeat it
        let mut r = req();
        r.sort = Some(("id".into(), SortDir::Asc));
        assert!(
            build_select(&r, Some(&key), false)
                .unwrap()
                .ends_with("ORDER BY \"id\" ASC NULLS LAST")
        );
    }

    #[test]
    fn rejects_second_statement() {
        let mut r = req();
        r.filter = Some("1=1; drop table x".into());
        assert!(build_select(&r, None, true).is_err());
        r.filter = Some("a = 1;".into());
        assert!(build_select(&r, None, true).is_err());
        r.filter = Some("a = ';'".into());
        assert!(build_select(&r, None, true).is_ok());
    }

    #[test]
    fn key_filters() {
        let cols = vec!["region".to_string(), "code".to_string()];
        assert_eq!(
            key_filter(&cols, &[Some("eu".into()), Some("1".into())]).as_deref(),
            Some("\"region\" = 'eu' AND \"code\" = '1'")
        );
        assert_eq!(key_filter(&cols, &[Some("eu".into()), None]), None);
        assert_eq!(key_filter(&cols, &[Some("eu".into())]), None);
    }

    #[test]
    fn quick_filters() {
        assert_eq!(
            quick_filter("name", &CellValue::Text("O'Brien".into()), false),
            "\"name\" = 'O''Brien'"
        );
        assert_eq!(
            quick_filter("name", &CellValue::Text("a\\b".into()), true),
            "\"name\" <> E'a\\\\b'"
        );
        assert_eq!(quick_filter("x", &CellValue::Null, false), "\"x\" IS NULL");
        assert_eq!(quick_filter("x", &CellValue::Null, true), "\"x\" IS NOT NULL");
        assert_eq!(append_filter(None, "\"x\" IS NULL"), "\"x\" IS NULL");
        assert_eq!(append_filter(Some("  "), "c"), "c");
        assert_eq!(
            append_filter(Some("a = 1 OR b = 2"), "\"x\" IS NULL"),
            "(a = 1 OR b = 2) AND \"x\" IS NULL"
        );
    }
}
