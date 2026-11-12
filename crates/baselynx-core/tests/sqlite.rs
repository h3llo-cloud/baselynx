//! SQLite through the same core API the app uses for Postgres. No server: each test makes
//! its own database file.

use std::path::Path;
use std::sync::Arc;

use baselynx_core::catalog::{self, RelKind};
use baselynx_core::edit::{self, CellValue, ChangeSet, RowDelete, RowInsert, RowUpdate};
use baselynx_core::exec;
use baselynx_core::model::{ConnectionConfig, Engine, SortDir};
use baselynx_core::session::{self, Session};
use baselynx_core::table::{self, PageRequest};

const SCHEMA: &str = r#"
CREATE TABLE customers (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    name TEXT,
    balance NUMERIC DEFAULT 0
);
CREATE TABLE orders (
    id INTEGER PRIMARY KEY,
    customer_id INTEGER NOT NULL REFERENCES customers(id),
    total REAL NOT NULL,
    note TEXT,
    total_cents INTEGER GENERATED ALWAYS AS (CAST(total * 100 AS INTEGER)) VIRTUAL
);
CREATE TABLE region_codes (
    region TEXT NOT NULL,
    code INTEGER NOT NULL,
    label TEXT,
    PRIMARY KEY (region, code)
);
CREATE TABLE logbook (at TEXT, line TEXT);
CREATE TABLE "Weird Name" (id INTEGER PRIMARY KEY, data BLOB);
CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
INSERT INTO customers (email, name, balance) VALUES
    ('ann@example.com', 'Ann', 10), ('bob@example.com', 'Bob', -2.5), ('cy@example.com', NULL, 0);
INSERT INTO orders (customer_id, total, note) VALUES (1, 120.5, 'gift'), (1, 30, NULL), (2, 250, 'it''s big');
INSERT INTO region_codes VALUES ('eu', 1, 'one'), ('eu', 2, 'two');
INSERT INTO "Weird Name" VALUES (1, x'00ff10');
"#;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    baselynx_core::runtime::runtime().block_on(f)
}

/// A new database file with the schema above.
fn database(dir: &Path) -> ConnectionConfig {
    let path = dir.join("shop.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    drop(conn);
    let mut c = ConnectionConfig::new("shop");
    c.engine = Engine::Sqlite;
    c.database = path.to_string_lossy().into_owned();
    c
}

fn connect(c: &ConnectionConfig) -> Arc<Session> {
    block_on(session::connect(c, None)).expect("open the file")
}

fn scalar(s: &Arc<Session>, sql: &str) -> Option<String> {
    block_on(exec::execute_collect(s, sql.into(), None)).unwrap().rows[0][0].clone()
}

#[test]
fn catalog_and_keys() {
    let dir = tempfile::tempdir().unwrap();
    let s = connect(&database(dir.path()));
    assert_eq!(s.engine(), Engine::Sqlite);
    assert!(s.server_version().starts_with('3'));

    let schemas = block_on(catalog::list_schemas(&s)).unwrap();
    assert_eq!(
        schemas.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
        vec!["main"]
    );
    let rels = block_on(catalog::list_relations(&s, "main")).unwrap();
    let names: Vec<_> = rels.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "Weird Name",
            "big_orders",
            "customers",
            "logbook",
            "orders",
            "region_codes"
        ]
    );
    assert_eq!(
        rels.iter().find(|r| r.name == "big_orders").unwrap().kind,
        RelKind::View
    );
    assert_eq!(
        rels.iter().find(|r| r.name == "region_codes").unwrap().primary_key,
        vec!["region", "code"]
    );
    assert_eq!(block_on(catalog::list_all_relations(&s)).unwrap().len(), 6);

    let orders = block_on(catalog::describe_relation(&s, "main", "orders")).unwrap();
    assert!(orders.editable);
    assert_eq!(orders.row_key.as_deref(), Some(&["id".to_string()][..]));
    let id = orders.column("id").unwrap();
    assert!(id.has_default, "INTEGER PRIMARY KEY is the rowid");
    assert!(orders.column("total_cents").unwrap().generated);
    assert!(!orders.column("total_cents").unwrap().writable());
    assert_eq!(orders.foreign_keys.len(), 1);
    assert_eq!(orders.foreign_keys[0].to_table, "customers");
    assert_eq!(orders.foreign_keys[0].from_cols, vec!["customer_id"]);
    assert_eq!(orders.foreign_keys[0].to_cols, vec!["id"]);

    let customers = block_on(catalog::describe_relation(&s, "main", "customers")).unwrap();
    assert_eq!(customers.referenced_by.len(), 1);
    assert_eq!(customers.referenced_by[0].from_table, "orders");

    let logbook = block_on(catalog::describe_relation(&s, "main", "logbook")).unwrap();
    assert!(!logbook.editable, "no key: read-only");
    let view = block_on(catalog::describe_relation(&s, "main", "big_orders")).unwrap();
    assert!(!view.editable);

    let cols = block_on(catalog::list_columns(&s, "main", "customers")).unwrap();
    assert_eq!(cols[1], ("email".to_string(), "TEXT".to_string()));
    assert!(
        block_on(catalog::column_stats(&s, "main", "customers", "email"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn pages_filters_and_values() {
    let dir = tempfile::tempdir().unwrap();
    let s = connect(&database(dir.path()));
    let rel = block_on(catalog::describe_relation(&s, "main", "orders")).unwrap();
    let req = PageRequest {
        schema: "main".into(),
        table: "orders".into(),
        filter: Some("total > 50".into()),
        sort: Some(("note".into(), SortDir::Desc)),
        page: 0,
        page_size: 1,
    };
    let page = block_on(table::load_page(&s, &rel, req)).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert!(page.has_next);
    assert_eq!(page.rows[0][3].as_deref(), Some("it's big"));
    assert_eq!(page.columns[2].type_name.as_deref(), Some("REAL"));
    assert!(page.columns[2].is_numeric());
    assert_eq!(
        block_on(table::count_rows(&s, "main", "orders", Some("note IS NULL".into()))).unwrap(),
        1
    );

    // Blobs come back as \x hex; NULLs as None.
    let got = block_on(exec::execute_collect(
        &s,
        "SELECT data, NULL FROM \"Weird Name\"".into(),
        None,
    ))
    .unwrap();
    assert_eq!(got.rows[0][0].as_deref(), Some("\\x00ff10"));
    assert_eq!(got.rows[0][1], None);

    // A row cap stops early and says so.
    let capped = block_on(exec::execute_collect(&s, "SELECT * FROM customers".into(), Some(2))).unwrap();
    assert_eq!(capped.rows.len(), 2);
    assert!(capped.truncated);

    // Statements without rows report the rows they changed.
    let upd = block_on(exec::execute_collect(
        &s,
        "UPDATE customers SET balance = 1".into(),
        None,
    ))
    .unwrap();
    assert!(!upd.returned_rows);
    assert_eq!(upd.count, 3);

    // A server error keeps its message.
    let err = block_on(exec::execute_collect(&s, "SELECT nope FROM customers".into(), None)).unwrap_err();
    assert!(err.to_string().contains("no such column"), "{err}");

    // Filters that would add a second statement are refused before they are sent.
    let mut bad = PageRequest {
        schema: "main".into(),
        table: "orders".into(),
        filter: Some("1=1; DROP TABLE orders".into()),
        sort: None,
        page: 0,
        page_size: 10,
    };
    assert!(block_on(table::load_page(&s, &rel, bad.clone())).is_err());
    bad.filter = Some("note = 'a;b'".into());
    assert!(block_on(table::load_page(&s, &rel, bad)).is_ok());
}

#[test]
fn edits_commit_in_one_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let s = connect(&database(dir.path()));
    let rel = block_on(catalog::describe_relation(&s, "main", "region_codes")).unwrap();
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: vec![("region".into(), "eu".into()), ("code".into(), "1".into())],
            set: vec![("label".into(), CellValue::Text("uno".into()))],
        }],
        deletes: vec![RowDelete {
            key: vec![("region".into(), "eu".into()), ("code".into(), "2".into())],
        }],
        inserts: vec![RowInsert {
            values: vec![
                ("region".into(), CellValue::Text("us".into())),
                ("code".into(), CellValue::Text("7".into())),
                ("label".into(), CellValue::Null),
            ],
        }],
    };
    let preview = edit::preview(&rel, &changes, s.dialect()).unwrap();
    assert_eq!(
        preview[0],
        "UPDATE \"main\".\"region_codes\" SET \"label\" = 'uno' WHERE \"region\" = 'eu' AND \"code\" = '1'"
    );
    let report = block_on(edit::commit(&s, &rel, changes)).unwrap();
    assert_eq!((report.updated, report.deleted, report.inserted), (1, 1, 1));
    assert_eq!(
        scalar(
            &s,
            "SELECT group_concat(region || code || ':' || ifnull(label, '-'), ' ') FROM region_codes ORDER BY 1"
        )
        .as_deref(),
        Some("eu1:uno us7:-")
    );
    // Text parameters land in INTEGER columns as integers.
    assert_eq!(
        scalar(&s, "SELECT typeof(code) FROM region_codes WHERE region = 'us'").as_deref(),
        Some("integer")
    );

    // A failing change rolls back the ones before it.
    let customers = block_on(catalog::describe_relation(&s, "main", "customers")).unwrap();
    let failing = ChangeSet {
        updates: vec![
            RowUpdate {
                key: vec![("id".into(), "1".into())],
                set: vec![("name".into(), CellValue::Text("Anne".into()))],
            },
            RowUpdate {
                key: vec![("id".into(), "2".into())],
                // email is UNIQUE: taking Ann's fails.
                set: vec![("email".into(), CellValue::Text("ann@example.com".into()))],
            },
        ],
        ..Default::default()
    };
    let err = block_on(edit::commit(&s, &customers, failing)).unwrap_err();
    assert!(err.to_string().starts_with("change 2 failed"), "{err}");
    assert_eq!(
        scalar(&s, "SELECT name FROM customers WHERE id = 1").as_deref(),
        Some("Ann")
    );

    // A row that is gone is reported, not silently skipped.
    let gone = ChangeSet {
        deletes: vec![RowDelete {
            key: vec![("id".into(), "99".into())],
        }],
        ..Default::default()
    };
    assert!(
        block_on(edit::commit(&s, &customers, gone))
            .unwrap_err()
            .to_string()
            .contains("expected 1 row")
    );
}

#[test]
fn read_only_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = database(dir.path());
    c.read_only = true;
    let s = connect(&c);
    assert!(s.is_read_only());
    assert!(scalar(&s, "SELECT count(*) FROM customers").is_some());
    let err = block_on(exec::execute_collect(&s, "DELETE FROM customers".into(), None)).unwrap_err();
    assert!(err.to_string().contains("readonly"), "{err}");

    let mut missing = c.clone();
    missing.database = dir.path().join("nope.db").to_string_lossy().into_owned();
    let err = block_on(session::connect(&missing, None)).unwrap_err();
    assert!(err.to_string().contains("no such file"), "{err}");
    assert!(!dir.path().join("nope.db").exists(), "a missing file is never created");
}

#[test]
fn cancel_stops_a_long_query() {
    let dir = tempfile::tempdir().unwrap();
    let s = connect(&database(dir.path()));
    let sql = "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n) SELECT count(*) FROM n".to_string();
    let mut rx = exec::execute(&s, sql, None);
    std::thread::sleep(std::time::Duration::from_millis(300));
    block_on(s.cancel()).unwrap();
    let mut cancelled = false;
    block_on(async {
        while let Some(ev) = rx.recv().await {
            if let Err(e) = ev {
                cancelled = e.is_cancel();
            }
        }
    });
    assert!(cancelled, "the query was interrupted");
}

mod backups {
    use super::*;
    use baselynx_core::backup::{self, BackupJob};
    use baselynx_core::config::{BackupFormat, BackupPlan, BackupSettings};

    fn empty_file(dir: &Path, name: &str) -> ConnectionConfig {
        let path = dir.join(name);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("VACUUM")
            .unwrap();
        let mut c = ConnectionConfig::new(name);
        c.engine = Engine::Sqlite;
        c.database = path.to_string_lossy().into_owned();
        c.allow_restore = true;
        c.config_key = Some(format!("server:{name}"));
        c
    }

    fn restore(target: &ConnectionConfig, run: &Path, format: BackupFormat, typed: &str) -> baselynx_core::Result<()> {
        block_on(backup::restore(
            target.clone(),
            None,
            run.to_path_buf(),
            format,
            None,
            typed.into(),
        ))
        .map(|_| ())
    }

    #[test]
    fn copy_and_sql_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = database(dir.path());
        source.config_key = Some("server:shop".into());
        let run = block_on(backup::run_backup(BackupJob {
            config: source.clone(),
            password: None,
            settings: BackupSettings::default_in(dir.path()),
            plan: BackupPlan::manual(vec![BackupFormat::Dump, BackupFormat::Sql]),
        }))
        .expect("backup");
        assert_eq!(run.manifest.engine, Engine::Sqlite);
        let names: Vec<_> = run.manifest.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["shop.sqlite", "schema.sql", "data.sql", "indexes.sql"]);
        assert!(run.manifest.pg_dump_version.contains("SQLite"));

        // Into the source: not empty.
        let mut into_source = source.clone();
        into_source.allow_restore = true;
        let err = restore(&into_source, &run.dir, BackupFormat::Dump, "shop.db")
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"), "{err}");

        // The copy, into an empty file; the name typed is the file's name.
        let target = empty_file(dir.path(), "drill.db");
        assert!(
            restore(&target, &run.dir, BackupFormat::Dump, "nope.db")
                .unwrap_err()
                .to_string()
                .contains("type")
        );
        restore(&target, &run.dir, BackupFormat::Dump, "drill.db").expect("restore the copy");
        let s = connect(&target);
        assert_eq!(scalar(&s, "SELECT count(*) FROM orders").as_deref(), Some("3"));

        // The SQL files, into another: rows, blobs, the view and the generated column survive.
        let target2 = empty_file(dir.path(), "drill2.db");
        restore(&target2, &run.dir, BackupFormat::Sql, "drill2.db").expect("restore the SQL files");
        let s2 = connect(&target2);
        assert_eq!(scalar(&s2, "SELECT count(*) FROM big_orders").as_deref(), Some("2"));
        assert_eq!(
            scalar(&s2, "SELECT hex(data) FROM \"Weird Name\"").as_deref(),
            Some("00FF10")
        );
        assert_eq!(
            scalar(&s2, "SELECT total_cents FROM orders WHERE id = 1").as_deref(),
            Some("12050")
        );
        assert_eq!(scalar(&s2, "SELECT name FROM customers WHERE id = 3"), None);
        // A second restore is refused: no longer empty.
        assert!(restore(&target2, &run.dir, BackupFormat::Sql, "drill2.db").is_err());
    }

    #[test]
    fn a_backup_only_goes_into_the_same_engine() {
        let dir = tempfile::tempdir().unwrap();
        let source = database(dir.path());
        let run = block_on(backup::run_backup(BackupJob {
            config: source,
            password: None,
            settings: BackupSettings::default_in(dir.path()),
            plan: BackupPlan::manual(vec![BackupFormat::Dump]),
        }))
        .unwrap();
        // Pretend it came from Postgres: the manifest says so, the checksums still match.
        let path = run.dir.join("manifest.json");
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"engine\": \"sqlite\"", "\"engine\": \"postgres\"");
        std::fs::write(&path, text).unwrap();
        let target = empty_file(dir.path(), "drill.db");
        let err = restore(&target, &run.dir, BackupFormat::Dump, "drill.db")
            .unwrap_err()
            .to_string();
        assert!(err.contains("same kind"), "{err}");
    }
}
