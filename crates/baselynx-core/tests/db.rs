//! Integration tests against a real server with `fixtures/seed.sql`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use baselynx_core::catalog::{self, RelKind, RelationInfo};
use baselynx_core::edit::{self, CellValue, ChangeSet, RowDelete, RowInsert, RowUpdate};
use baselynx_core::exec;
use baselynx_core::export::{self, ExportFormat, ExportSource};
use baselynx_core::model::SortDir;
use baselynx_core::session::Session;
use baselynx_core::sql::Dialect;
use baselynx_core::table::{self, PageRequest};
use baselynx_core::{CancellationToken, CoreError, watch};
use common::{block_on, connect, db};

fn describe(s: &Arc<Session>, table: &str) -> RelationInfo {
    block_on(catalog::describe_relation(s, "demo", table)).unwrap()
}

fn query(s: &Arc<Session>, sql: &str) -> exec::Collected {
    block_on(exec::execute_collect(s, sql.to_string(), None)).unwrap()
}

fn scalar(s: &Arc<Session>, sql: &str) -> Option<String> {
    query(s, sql).rows.first().and_then(|r| r[0].clone())
}

// ---------------------------------------------------------------- catalog

#[test]
fn catalog_schemas_and_relations() {
    let s = connect();
    let schemas = block_on(catalog::list_schemas(&s)).unwrap();
    let demo = schemas.iter().find(|x| x.name == "demo").unwrap();
    assert!(!demo.system);
    assert!(schemas.iter().any(|x| x.name == "pg_catalog" && x.system));
    assert!(schemas.last().unwrap().system, "system schemas come last");
    assert!(
        !schemas
            .iter()
            .any(|x| x.name.starts_with("pg_toast") || x.name.starts_with("pg_temp"))
    );

    let rels = block_on(catalog::list_relations(&s, "demo")).unwrap();
    let names: Vec<_> = rels.iter().map(|r| r.name.as_str()).collect();
    for n in [
        "Weird Name",
        "all_types",
        "composite_key",
        "customers",
        "customers_v",
        "events",
        "no_pk",
        "unique_only",
    ] {
        assert!(names.contains(&n), "{n} missing from {names:?}");
    }
    assert_eq!(
        rels.iter().find(|r| r.name == "customers_v").unwrap().kind,
        RelKind::View
    );

    let all = block_on(catalog::list_all_relations(&s)).unwrap();
    assert!(all.iter().any(|r| r.schema == "demo" && r.name == "events"));
    assert!(!all.iter().any(|r| r.schema == "pg_catalog"));
}

#[test]
fn catalog_row_keys() {
    let s = connect();
    let key = |t: &str| describe(&s, t).row_key;
    let v = |xs: &[&str]| Some(xs.iter().map(|x| x.to_string()).collect::<Vec<_>>());
    assert_eq!(key("customers"), v(&["id"]));
    assert_eq!(key("events"), v(&["id"]));
    assert_eq!(key("composite_key"), v(&["region", "code"]));
    assert_eq!(key("no_pk"), None);
    assert_eq!(key("unique_only"), v(&["email"]));
    assert_eq!(key("Weird Name"), v(&["id"]));
    assert_eq!(key("customers_v"), None);

    assert!(describe(&s, "customers").editable);
    assert!(describe(&s, "unique_only").editable);
    assert!(!describe(&s, "no_pk").editable);
    let view = describe(&s, "customers_v");
    assert!(!view.editable);
    assert_eq!(view.kind, RelKind::View);
    assert!(view.read_only_reason().unwrap().contains("view"));
}

#[test]
fn catalog_column_flags() {
    let s = connect();
    let rel = describe(&s, "all_types");
    let c = |n: &str| rel.column(n).unwrap().clone();
    assert!(c("c_generated").generated);
    assert!(!c("c_generated").writable());
    assert!(c("c_identity").identity_always);
    assert!(c("c_identity").not_null);
    assert!(c("id").not_null);
    assert!(!c("c_text").not_null);
    assert_eq!(c("c_varchar").type_name, "character varying(20)");
    assert_eq!(c("c_mood").type_name, "demo.mood");
    assert_eq!(c("c_int4_arr").type_name, "integer[]");
    assert_eq!(c("c_int8").type_oid, 20);
    assert_eq!(rel.columns.first().unwrap().name, "id");
    assert!(describe(&s, "customers").column("tier").unwrap().has_default);
}

// ---------------------------------------------------------------- exec

#[test]
fn exec_rows_and_nulls() {
    let s = connect();
    let got = query(&s, "SELECT * FROM demo.all_types ORDER BY id");
    assert!(got.returned_rows);
    assert_eq!(got.count, 3);
    assert_eq!(got.rows.len(), 3);
    let col = |name: &str| got.columns.iter().position(|c| c.name == name).unwrap();
    let r1 = &got.rows[0];
    assert_eq!(r1[col("c_int8")].as_deref(), Some("9007199254740993"));
    assert_eq!(r1[col("c_bool")].as_deref(), Some("t"));
    assert_eq!(r1[col("c_bytea")].as_deref(), Some("\\x0102ff"));
    assert_eq!(r1[col("c_text")].as_deref(), Some("hello, \"world\""));
    assert_eq!(r1[col("c_mood")].as_deref(), Some("happy"));
    assert_eq!(r1[col("c_generated")].as_deref(), Some("10"));
    let r3 = &got.rows[2];
    assert!(r3[col("c_text")].is_none());
    assert!(r3[col("c_jsonb")].is_none());
    assert_eq!(got.columns[col("c_int4")].type_oid, Some(23));
    assert_eq!(got.columns[col("c_text")].type_name.as_deref(), Some("text"));
}

#[test]
fn exec_error_carries_sqlstate() {
    let s = connect();
    let err = block_on(exec::execute_collect(&s, "SELECT * FROM no_such_table".into(), None)).unwrap_err();
    assert_eq!(err.sqlstate(), Some("42P01"));
    assert!(err.db().unwrap().message.contains("no_such_table"));
    let err = block_on(exec::execute_collect(&s, "SELEC 1".into(), None)).unwrap_err();
    assert_eq!(err.sqlstate(), Some("42601"));
    // The session is still usable.
    assert_eq!(scalar(&s, "SELECT 1").as_deref(), Some("1"));
}

#[test]
fn exec_statement_kinds() {
    let s = connect();
    let got = query(&s, "CREATE TEMP TABLE t_kinds (a int)");
    assert!(!got.returned_rows);
    let got = query(&s, "INSERT INTO t_kinds SELECT generate_series(1, 7)");
    assert!(!got.returned_rows);
    assert_eq!(got.count, 7);
    let got = query(&s, "SELECT a FROM t_kinds WHERE false");
    assert!(got.returned_rows);
    assert_eq!(got.columns.len(), 1);
    assert!(got.rows.is_empty());
}

#[test]
fn exec_row_cap_truncates() {
    let s = connect();
    let got = block_on(exec::execute_collect(
        &s,
        "SELECT * FROM demo.events".into(),
        Some(1000),
    ))
    .unwrap();
    assert!(got.truncated);
    assert_eq!(got.rows.len(), 1000);
    // A small result under the cap is not truncated.
    let got = block_on(exec::execute_collect(
        &s,
        "SELECT * FROM demo.customers".into(),
        Some(10_000),
    ))
    .unwrap();
    assert!(!got.truncated);
    assert_eq!(got.rows.len(), 1000);
    // The cancel sent for the cap does not hit the next statement.
    assert_eq!(
        scalar(&s, "SELECT count(*) FROM demo.unique_only").as_deref(),
        Some("2")
    );
}

#[test]
fn exec_cancel_stops_sleep() {
    let s = connect();
    let started = Instant::now();
    let mut rx = exec::execute(&s, "SELECT pg_sleep(30)".into(), None);
    std::thread::sleep(Duration::from_millis(300));
    block_on(s.cancel()).unwrap();
    let mut err = None;
    while let Some(ev) = block_on(rx.recv()) {
        if let Err(e) = ev {
            err = Some(e);
        }
    }
    let err = err.expect("the sleep was not cancelled");
    assert!(err.is_cancel(), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(scalar(&s, "SELECT 2").as_deref(), Some("2"));
}

// ---------------------------------------------------------------- table pages

fn page_req(table: &str, page: u64, size: u32) -> PageRequest {
    PageRequest {
        schema: "demo".into(),
        table: table.into(),
        filter: None,
        sort: None,
        page,
        page_size: size,
    }
}

#[test]
fn table_page_boundaries() {
    // `events` is never changed by other tests, which run in parallel.
    let s = connect();
    let rel = describe(&s, "events");
    let p0 = block_on(table::load_page(&s, &rel, page_req("events", 0, 500))).unwrap();
    assert_eq!(p0.rows.len(), 500);
    assert!(p0.has_next);
    assert_eq!(p0.rows[0][0].as_deref(), Some("1"));
    assert_eq!(p0.columns[0].type_oid, Some(20));
    let p1 = block_on(table::load_page(&s, &rel, page_req("events", 1, 500))).unwrap();
    assert_eq!(p1.rows[0][0].as_deref(), Some("501"));
    assert!(p1.has_next);
    let mut last = page_req("events", 1, 500);
    last.filter = Some("id <= 1000".into());
    let p = block_on(table::load_page(&s, &rel, last.clone())).unwrap();
    assert_eq!(p.rows.len(), 500);
    assert!(!p.has_next);
    last.page = 2;
    let p = block_on(table::load_page(&s, &rel, last)).unwrap();
    assert!(p.rows.is_empty());
    assert!(!p.has_next);
}

#[test]
fn table_filter_sort_and_count() {
    let s = connect();
    let rel = describe(&s, "events");
    let mut r = page_req("events", 0, 100);
    r.filter = Some("kind = 'refund'".into());
    let p = block_on(table::load_page(&s, &rel, r)).unwrap();
    let kind = p.columns.iter().position(|c| c.name == "kind").unwrap();
    assert_eq!(p.rows.len(), 100);
    assert!(p.rows.iter().all(|row| row[kind].as_deref() == Some("refund")));

    let n = block_on(table::count_rows(&s, "demo", "events", Some("kind = 'refund'".into()))).unwrap();
    assert_eq!(n, 250_000);
    let n = block_on(table::count_rows(&s, "demo", "customers", None)).unwrap();
    assert!(n >= 990);

    let rel = describe(&s, "customers");
    let mut r = page_req("customers", 0, 5);
    r.sort = Some(("balance".into(), SortDir::Desc));
    let p = block_on(table::load_page(&s, &rel, r)).unwrap();
    let bal: Vec<f64> = p
        .rows
        .iter()
        .map(|x| x[2].as_deref().unwrap().parse().unwrap())
        .collect();
    assert!(bal.windows(2).all(|w| w[0] >= w[1]), "{bal:?}");
}

#[test]
fn table_order_is_stable_across_pages() {
    let s = connect();
    let rel = describe(&s, "events");
    // `processed` has only two values; the key breaks the ties.
    let mut seen = Vec::new();
    for page in 0..10 {
        let mut r = page_req("events", page, 100);
        r.filter = Some("id <= 1000".into());
        r.sort = Some(("processed".into(), SortDir::Asc));
        let p = block_on(table::load_page(&s, &rel, r)).unwrap();
        seen.extend(p.rows.iter().map(|row| row[0].clone().unwrap().parse::<u64>().unwrap()));
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 1000, "pages overlap or skip rows");
}

#[test]
fn table_rejects_second_statement_and_loads_weird_names() {
    let s = connect();
    let rel = describe(&s, "customers");
    let mut r = page_req("customers", 0, 10);
    r.filter = Some("true; DROP TABLE demo.customers".into());
    let err = block_on(table::load_page(&s, &rel, r)).unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)));

    let rel = describe(&s, "Weird Name");
    let p = block_on(table::load_page(&s, &rel, page_req("Weird Name", 0, 10))).unwrap();
    assert_eq!(p.rows.len(), 2);
    let names: Vec<_> = p.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "select", "a\"b"]);

    let mut r = page_req("Weird Name", 0, 10);
    r.filter = Some(table::quick_filter("a\"b", &CellValue::Null, false, Dialect::Postgres));
    let p = block_on(table::load_page(&s, &rel, r)).unwrap();
    assert_eq!(p.rows.len(), 1);
    assert_eq!(p.rows[0][0].as_deref(), Some("2"));
}

// ---------------------------------------------------------------- edits

fn key(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn text(v: &str) -> CellValue {
    CellValue::Text(v.into())
}

#[test]
fn edit_every_all_types_column_through_text() {
    let s = connect();
    let rel = describe(&s, "all_types");
    let src = query(&s, "SELECT * FROM demo.all_types WHERE id = 1");
    let set: Vec<(String, CellValue)> = rel
        .columns
        .iter()
        .zip(src.rows[0].iter())
        .filter(|(c, _)| c.writable() && c.name != "id")
        .map(|(c, v)| (c.name.clone(), CellValue::from_option(v.clone())))
        .collect();
    assert!(set.len() >= 20);
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: key(&[("id", "2")]),
            set,
        }],
        ..Default::default()
    };
    let report = block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!(report.updated, 1);
    let after = query(&s, "SELECT * FROM demo.all_types WHERE id = 2");
    for (i, c) in rel.columns.iter().enumerate() {
        if c.writable() && c.name != "id" {
            assert_eq!(after.rows[0][i], src.rows[0][i], "column {}", c.name);
        }
    }
}

#[test]
fn edit_set_null_and_update_key() {
    let s = connect();
    let rel = describe(&s, "customers");
    let changes = ChangeSet {
        updates: vec![
            RowUpdate {
                key: key(&[("id", "10")]),
                set: vec![("note".into(), CellValue::Null)],
            },
            RowUpdate {
                key: key(&[("id", "920")]),
                set: vec![("id".into(), text("5920"))],
            },
        ],
        ..Default::default()
    };
    block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!(scalar(&s, "SELECT note FROM demo.customers WHERE id = 10"), None);
    assert_eq!(
        scalar(&s, "SELECT count(*) FROM demo.customers WHERE id = 10").as_deref(),
        Some("1")
    );
    assert_eq!(
        scalar(&s, "SELECT name FROM demo.customers WHERE id = 5920").as_deref(),
        Some("Customer 920")
    );
    assert_eq!(
        scalar(&s, "SELECT count(*) FROM demo.customers WHERE id = 920").as_deref(),
        Some("0")
    );
}

#[test]
fn edit_composite_key_update_and_delete() {
    let s = connect();
    let rel = describe(&s, "composite_key");
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: key(&[("region", "eu"), ("code", "2")]),
            set: vec![("label".into(), text("deux"))],
        }],
        deletes: vec![RowDelete {
            key: key(&[("region", "us"), ("code", "1")]),
        }],
        ..Default::default()
    };
    let r = block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!((r.updated, r.deleted), (1, 1));
    assert_eq!(
        scalar(
            &s,
            "SELECT label FROM demo.composite_key WHERE region = 'eu' AND code = 2"
        )
        .as_deref(),
        Some("deux")
    );
    assert_eq!(
        scalar(&s, "SELECT count(*) FROM demo.composite_key WHERE region = 'us'").as_deref(),
        Some("0")
    );
}

#[test]
fn edit_delete_then_insert_same_key() {
    let s = connect();
    query(&s, "CREATE TABLE demo.reuse (k int PRIMARY KEY, v text)");
    query(&s, "INSERT INTO demo.reuse VALUES (1, 'old')");
    let rel = describe(&s, "reuse");
    let changes = ChangeSet {
        deletes: vec![RowDelete {
            key: key(&[("k", "1")]),
        }],
        inserts: vec![RowInsert {
            values: vec![("k".into(), text("1")), ("v".into(), text("new"))],
        }],
        ..Default::default()
    };
    block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!(
        scalar(&s, "SELECT v FROM demo.reuse WHERE k = 1").as_deref(),
        Some("new")
    );
}

#[test]
fn edit_inserts_with_some_and_no_columns() {
    let s = connect();
    let rel = describe(&s, "customers");
    let changes = ChangeSet {
        inserts: vec![RowInsert {
            values: vec![("name".into(), text("Inserted by test"))],
        }],
        ..Default::default()
    };
    block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!(
        scalar(
            &s,
            "SELECT tier || '/' || balance FROM demo.customers WHERE name = 'Inserted by test'"
        )
        .as_deref(),
        Some("basic/0.00")
    );

    query(
        &s,
        "CREATE TABLE demo.defaults_only (id serial PRIMARY KEY, created timestamptz DEFAULT now())",
    );
    let rel = describe(&s, "defaults_only");
    let changes = ChangeSet {
        inserts: vec![RowInsert { values: vec![] }, RowInsert { values: vec![] }],
        ..Default::default()
    };
    let r = block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!(r.inserted, 2);
    assert_eq!(
        scalar(&s, "SELECT count(*) FROM demo.defaults_only").as_deref(),
        Some("2")
    );
}

#[test]
fn edit_failure_rolls_back_everything() {
    let s = connect();
    let rel = describe(&s, "customers");
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: key(&[("id", "40")]),
            set: vec![("name".into(), text("should not stick"))],
        }],
        inserts: vec![RowInsert {
            values: vec![("name".into(), CellValue::Null)],
        }],
        ..Default::default()
    };
    let err = block_on(edit::commit(&s, &rel, changes)).unwrap_err();
    match &err {
        CoreError::Commit { index, source } => {
            assert_eq!(index.0, 1);
            assert_eq!(source.sqlstate(), Some("23502"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        scalar(&s, "SELECT name FROM demo.customers WHERE id = 40").as_deref(),
        Some("Customer 40")
    );
    // the session is not left inside a transaction: there, now() is the transaction start
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(scalar(&s, "SELECT now() = statement_timestamp()").as_deref(), Some("t"));
}

#[test]
fn edit_no_matching_row_rolls_back() {
    let s = connect();
    let rel = describe(&s, "customers");
    for missing in [
        ChangeSet {
            updates: vec![
                RowUpdate {
                    key: key(&[("id", "50")]),
                    set: vec![("name".into(), text("nope"))],
                },
                RowUpdate {
                    key: key(&[("id", "999999")]),
                    set: vec![("name".into(), text("nope"))],
                },
            ],
            ..Default::default()
        },
        ChangeSet {
            updates: vec![RowUpdate {
                key: key(&[("id", "50")]),
                set: vec![("name".into(), text("nope"))],
            }],
            deletes: vec![RowDelete {
                key: key(&[("id", "999999")]),
            }],
            ..Default::default()
        },
    ] {
        let err = block_on(edit::commit(&s, &rel, missing)).unwrap_err();
        assert!(matches!(err, CoreError::Commit { .. }), "{err:?}");
        assert_eq!(
            scalar(&s, "SELECT name FROM demo.customers WHERE id = 50").as_deref(),
            Some("Customer 50")
        );
    }
}

#[test]
fn edit_single_key_delete() {
    let s = connect();
    let rel = describe(&s, "customers");
    let changes = ChangeSet {
        deletes: vec![RowDelete {
            key: key(&[("id", "930")]),
        }],
        ..Default::default()
    };
    block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!(
        scalar(&s, "SELECT count(*) FROM demo.customers WHERE id = 930").as_deref(),
        Some("0")
    );
}

// ---------------------------------------------------------------- export

fn query_source(sql: &str, columns: Vec<exec::ColumnMeta>) -> ExportSource {
    let db = db();
    ExportSource::Query {
        connection: Box::new(db.config.clone()),
        password: db.password.clone(),
        sql: sql.into(),
        columns,
    }
}

fn run_export(
    source: ExportSource,
    format: ExportFormat,
    path: std::path::PathBuf,
) -> Result<export::ExportReport, CoreError> {
    let (tx, _rx) = watch::channel(0u64);
    block_on(export::export(
        source,
        format,
        path,
        "all_types".into(),
        tx,
        CancellationToken::new(),
    ))
}

#[test]
fn export_csv_round_trip() {
    let s = connect();
    let sql = "SELECT * FROM demo.all_types WHERE id IN (1, 3) ORDER BY id";
    let expected = query(&s, sql);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all_types.csv");
    let report = run_export(
        query_source(sql, expected.columns.clone()),
        ExportFormat::Csv,
        path.clone(),
    )
    .unwrap();
    assert_eq!(report.rows, 2);
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"\xEF\xBB\xBF"));
    assert!(String::from_utf8_lossy(&bytes).contains("\r\n"));
    let mut reader = csv::ReaderBuilder::new().from_reader(&bytes[3..]);
    let header: Vec<String> = reader.headers().unwrap().iter().map(str::to_string).collect();
    assert_eq!(
        header,
        expected.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
    );
    let rows: Vec<Vec<String>> = reader
        .records()
        .map(|r| r.unwrap().iter().map(str::to_string).collect())
        .collect();
    for (got, want) in rows.iter().zip(expected.rows.iter()) {
        let want: Vec<String> = want.iter().map(|v| v.clone().unwrap_or_default()).collect();
        assert_eq!(got, &want);
    }
    assert!(!dir.path().join("all_types.csv.part").exists());
}

#[test]
fn export_xlsx_typed_cells() {
    use calamine::{Data, Reader, Xlsx, open_workbook};
    let s = connect();
    // Row 2 is rewritten by an edit test running in parallel, so only rows 1 and 3 are used.
    let sql = "SELECT *, 42::int8 AS small_int8, false AS f_bool FROM demo.all_types WHERE id IN (1, 3) ORDER BY id";
    let expected = query(&s, sql);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all_types.xlsx");
    run_export(
        query_source(sql, expected.columns.clone()),
        ExportFormat::Xlsx,
        path.clone(),
    )
    .unwrap();
    let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
    let range = wb.worksheet_range("all_types").unwrap();
    let col = |name: &str| expected.columns.iter().position(|c| c.name == name).unwrap();
    let cell = |r: usize, name: &str| range.get((r, col(name))).cloned().unwrap_or(Data::Empty);
    assert_eq!(cell(0, "c_int4"), Data::String("c_int4".into()));
    assert!(
        matches!(cell(1, "c_int4"), Data::Float(f) if f == 123456.0),
        "{:?}",
        cell(1, "c_int4")
    );
    assert!(matches!(cell(1, "c_float8"), Data::Float(f) if f == 3.5));
    assert!(matches!(cell(1, "c_numeric"), Data::Float(f) if (f - 12345.6789).abs() < 1e-9));
    // 16 significant digits stay text so the value is exact
    assert_eq!(cell(1, "c_int8"), Data::String("9007199254740993".into()));
    assert!(
        matches!(cell(1, "small_int8"), Data::Float(f) if f == 42.0),
        "{:?}",
        cell(1, "small_int8")
    );
    assert_eq!(cell(1, "c_bool"), Data::Bool(true));
    assert_eq!(cell(1, "f_bool"), Data::Bool(false));
    // dates and timestamps are text exactly as the server printed them
    let ts = expected.rows[0][col("c_timestamptz")].clone().unwrap();
    assert_eq!(cell(1, "c_timestamptz"), Data::String(ts));
    assert_eq!(cell(1, "c_text"), Data::String("hello, \"world\"".into()));
    assert_eq!(cell(2, "c_text"), Data::Empty);
    assert_eq!(cell(2, "c_int8"), Data::Empty);
}

#[test]
fn export_loaded_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("result.csv");
    let source = ExportSource::Loaded {
        columns: vec![exec::ColumnMeta::named("a"), exec::ColumnMeta::named("b,c")],
        rows: vec![
            vec![Some("1".into()), None],
            vec![Some("x\"y".into()), Some("line\nbreak".into())],
        ],
    };
    let report = run_export(source, ExportFormat::Csv, path.clone()).unwrap();
    assert_eq!(report.rows, 2);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, "\u{feff}a,\"b,c\"\r\n1,\r\n\"x\"\"y\",\"line\nbreak\"\r\n");
}

#[test]
fn export_million_rows_to_csv() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.csv");
    let started = Instant::now();
    let report = run_export(
        query_source("SELECT * FROM demo.events", vec![]),
        ExportFormat::Csv,
        path.clone(),
    )
    .unwrap();
    assert_eq!(report.rows, 1_000_000);
    let lines = std::fs::read(&path).unwrap().iter().filter(|&&b| b == b'\n').count();
    assert_eq!(lines, 1_000_001);
    eprintln!("exported 1M rows in {:?}", started.elapsed());
}

#[test]
fn export_cancel_removes_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.xlsx");
    let (tx, mut rx) = watch::channel(0u64);
    let cancel = CancellationToken::new();
    let fut = export::export(
        query_source("SELECT * FROM demo.events", vec![]),
        ExportFormat::Csv,
        path.clone(),
        "events".into(),
        tx,
        cancel.clone(),
    );
    let result = block_on(async {
        let handle = tokio::spawn(fut);
        // cancel as soon as some rows were written
        while *rx.borrow_and_update() == 0 {
            if rx.changed().await.is_err() {
                break;
            }
        }
        cancel.cancel();
        handle.await.unwrap()
    });
    assert!(matches!(result, Err(CoreError::Cancelled)), "{result:?}");
    assert!(!path.exists());
    assert!(!dir.path().join("events.xlsx.part").exists());
}

// ---------------------------------------------------------------- foreign keys

#[test]
fn catalog_foreign_keys_both_directions() {
    let s = connect();
    let events = describe(&s, "events");
    let fk = events
        .foreign_key_of("customer_id")
        .expect("events.customer_id references customers");
    assert_eq!((fk.to_schema.as_str(), fk.to_table.as_str()), ("demo", "customers"));
    assert_eq!(fk.to_cols, vec!["id".to_string()]);
    assert!(events.foreign_key_of("kind").is_none());

    let customers = describe(&s, "customers");
    assert!(customers.foreign_keys.is_empty());
    let back: Vec<_> = customers.referenced_by.iter().map(|f| f.from_table.as_str()).collect();
    assert_eq!(back, vec!["events"]);

    let comp = describe(&s, "composite_ref");
    let fk = comp.foreign_key_of("code").unwrap();
    assert_eq!(fk.from_cols, vec!["region".to_string(), "code".to_string()]);
    assert_eq!(fk.to_cols, vec!["region".to_string(), "code".to_string()]);
    assert_eq!(describe(&s, "composite_key").referenced_by.len(), 1);

    // The filter that finds the referenced row.
    let filter = table::key_filter(&fk.to_cols, &[Some("eu".into()), Some("1".into())], Dialect::Postgres).unwrap();
    let n = block_on(table::count_rows(&s, "demo", "composite_key", Some(filter))).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn read_only_connection_refuses_writes() {
    let db = db();
    let mut cfg = db.config.clone();
    cfg.read_only = true;
    let s = block_on(baselynx_core::session::connect(&cfg, db.password.clone())).unwrap();
    assert!(s.is_read_only());
    let err = block_on(exec::execute_collect(
        &s,
        "UPDATE demo.customers SET note = 'x' WHERE id = 1".into(),
        None,
    ))
    .unwrap_err();
    assert_eq!(err.sqlstate(), Some("25006"), "{err}");
    let read = block_on(exec::execute_collect(
        &s,
        "SELECT count(*) FROM demo.customers".into(),
        None,
    ))
    .unwrap();
    assert_eq!(read.rows.len(), 1);
}

#[test]
fn catalog_column_stats_after_analyze() {
    let s = connect();
    query(&s, "CREATE TABLE demo.glance (k text, n int)");
    query(
        &s,
        "INSERT INTO demo.glance SELECT CASE WHEN g % 10 = 0 THEN NULL WHEN g % 3 = 0 THEN 'a b' ELSE 'x' END, g FROM generate_series(1, 300) g",
    );
    assert_eq!(
        block_on(catalog::column_stats(&s, "demo", "glance", "k")).unwrap(),
        None,
        "never analysed"
    );
    query(&s, "ANALYZE demo.glance");
    let st = block_on(catalog::column_stats(&s, "demo", "glance", "k"))
        .unwrap()
        .unwrap();
    assert!((st.null_frac - 0.1).abs() < 0.02, "{}", st.null_frac);
    assert_eq!(st.distinct, Some(2));
    assert_eq!(st.common.first().map(|c| c.0.as_str()), Some("x"));
    assert!(st.common.iter().any(|c| c.0 == "a b"));
    let n = block_on(catalog::column_stats(&s, "demo", "glance", "n"))
        .unwrap()
        .unwrap();
    assert_eq!(n.distinct, Some(300), "a negative n_distinct becomes a count");
    assert_eq!(
        catalog::parse_text_array(r#"{x,"a b","q\"uote",NULL,"NULL"}"#),
        vec![
            Some("x".into()),
            Some("a b".into()),
            Some("q\"uote".into()),
            None,
            Some("NULL".into())
        ]
    );
}

#[test]
fn catalog_columns_for_completion() {
    let s = connect();
    let cols = block_on(catalog::list_columns(&s, "demo", "customers")).unwrap();
    assert_eq!(cols.first(), Some(&("id".to_string(), "integer".to_string())));
    assert!(cols.iter().any(|(n, t)| n == "balance" && t == "numeric(12,2)"));
    assert!(block_on(catalog::list_columns(&s, "demo", "nope")).unwrap().is_empty());
}

#[test]
fn catalog_primary_keys_and_estimates_in_lists() {
    let s = connect();
    let all = block_on(catalog::list_all_relations(&s)).unwrap();
    let get = |n: &str| all.iter().find(|r| r.schema == "demo" && r.name == n).unwrap();
    assert_eq!(get("customers").primary_key, vec!["id".to_string()]);
    assert_eq!(
        get("composite_key").primary_key,
        vec!["region".to_string(), "code".to_string()]
    );
    assert!(get("no_pk").primary_key.is_empty());
    assert!(get("customers_v").primary_key.is_empty());
    // `reltuples` is -1 until ANALYZE; afterwards the estimate is close.
    query(&s, "ANALYZE demo.customers");
    let all = block_on(catalog::list_relations(&s, "demo")).unwrap();
    let est = all.iter().find(|r| r.name == "customers").unwrap().estimate.unwrap();
    assert!((900..=1100).contains(&est), "{est}");
}
