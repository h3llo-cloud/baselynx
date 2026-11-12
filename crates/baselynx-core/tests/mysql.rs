//! MySQL / MariaDB through the same core API the app uses for Postgres.
//!
//! Opt-in: set `BASELYNX_TEST_MYSQL_DSN=mysql://root:password@127.0.0.1:3306/` (a user who
//! may create databases). Each test makes its own database `baselynx_test_<random>` and
//! drops it after. Backup tests also need `mysqldump` and `mysql` on PATH, or their
//! directory in `BASELYNX_TEST_MYSQL_TOOLS`. Without the DSN every test passes trivially.

use std::sync::Arc;

use baselynx_core::catalog::{self, RelKind};
use baselynx_core::edit::{self, CellValue, ChangeSet, RowDelete, RowInsert, RowUpdate};
use baselynx_core::exec;
use baselynx_core::model::{ConnectionConfig, Engine, SortDir, SslMode, parse_connection_url};
use baselynx_core::session::{self, Session};
use baselynx_core::table::{self, PageRequest};

const SCHEMA: &[&str] = &[
    "CREATE TABLE customers (
        id INT AUTO_INCREMENT PRIMARY KEY,
        email VARCHAR(100) NOT NULL UNIQUE,
        name VARCHAR(100),
        balance DECIMAL(10,2) NOT NULL DEFAULT 0
    ) ENGINE=InnoDB",
    "CREATE TABLE orders (
        id INT AUTO_INCREMENT PRIMARY KEY,
        customer_id INT NOT NULL,
        total DOUBLE NOT NULL,
        note TEXT,
        data VARBINARY(16),
        total_cents BIGINT AS (total * 100) VIRTUAL,
        CONSTRAINT fk_orders_customer FOREIGN KEY (customer_id) REFERENCES customers(id)
    ) ENGINE=InnoDB",
    "CREATE TABLE region_codes (region VARCHAR(10) NOT NULL, code INT NOT NULL, label TEXT, PRIMARY KEY (region, code)) ENGINE=InnoDB",
    "CREATE TABLE logbook (at DATETIME, line TEXT) ENGINE=InnoDB",
    "CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100",
    "INSERT INTO customers (email, name, balance) VALUES ('ann@example.com', 'Ann', 10), ('bob@example.com', 'Bob', -2.5), ('cy@example.com', NULL, 0)",
    "INSERT INTO orders (customer_id, total, note, data) VALUES (1, 120.5, 'gift', X'00ff10'), (1, 30, NULL, NULL), (2, 250, 'it''s big', NULL)",
    "INSERT INTO region_codes VALUES ('eu', 1, 'one'), ('eu', 2, 'two')",
];

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    baselynx_core::runtime::runtime().block_on(f)
}

/// The admin connection from the environment, or `None` to skip.
fn admin() -> Option<(ConnectionConfig, Option<String>)> {
    let dsn = std::env::var("BASELYNX_TEST_MYSQL_DSN").ok()?;
    let u = parse_connection_url(&dsn).expect("BASELYNX_TEST_MYSQL_DSN must be a mysql:// URL");
    assert_eq!(
        u.engine,
        Engine::Mysql,
        "BASELYNX_TEST_MYSQL_DSN must be a mysql:// URL"
    );
    let mut c = ConnectionConfig::new("mysql-admin");
    c.engine = Engine::Mysql;
    c.host = u.host.unwrap_or_else(|| "127.0.0.1".into());
    c.port = u.port.unwrap_or(3306);
    c.user = u.user.unwrap_or_else(|| "root".into());
    c.database = String::new();
    c.ssl_mode = u.ssl_mode.unwrap_or(SslMode::Prefer);
    Some((c, u.password))
}

/// A fresh database with the schema above; dropped when the guard goes.
struct TestDb {
    config: ConnectionConfig,
    password: Option<String>,
    admin: Arc<Session>,
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let sql = format!("DROP DATABASE IF EXISTS `{}`", self.config.database);
        let _ = block_on(exec::execute_collect(&self.admin, sql, None));
    }
}

fn database() -> Option<TestDb> {
    let (admin_cfg, password) = admin()?;
    let admin = block_on(session::connect(&admin_cfg, password.clone())).expect("connect to the test server");
    let name = format!("baselynx_test_{}", uuid::Uuid::new_v4().simple());
    block_on(exec::execute_collect(&admin, format!("CREATE DATABASE `{name}`"), None)).unwrap();
    let mut config = admin_cfg.clone();
    config.database = name;
    config.name = "shop".into();
    let s = block_on(session::connect(&config, password.clone())).unwrap();
    for sql in SCHEMA {
        block_on(exec::execute_collect(&s, sql.to_string(), None)).unwrap();
    }
    Some(TestDb {
        config,
        password,
        admin,
    })
}

fn connect(db: &TestDb) -> Arc<Session> {
    block_on(session::connect(&db.config, db.password.clone())).unwrap()
}

fn scalar(s: &Arc<Session>, sql: &str) -> Option<String> {
    block_on(exec::execute_collect(s, sql.into(), None)).unwrap().rows[0][0].clone()
}

#[test]
fn catalog_and_keys() {
    let Some(db) = database() else { return };
    let s = connect(&db);
    assert_eq!(s.engine(), Engine::Mysql);
    let schema = db.config.database.clone();

    let schemas = block_on(catalog::list_schemas(&s)).unwrap();
    assert!(schemas.iter().any(|x| x.name == schema && !x.system));
    assert!(schemas.iter().any(|x| x.name == "information_schema" && x.system));
    assert!(schemas.last().unwrap().system, "system schemas come last");

    let rels = block_on(catalog::list_relations(&s, &schema)).unwrap();
    let names: Vec<_> = rels.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["big_orders", "customers", "logbook", "orders", "region_codes"]
    );
    assert_eq!(rels[0].kind, RelKind::View);
    assert_eq!(
        rels.iter().find(|r| r.name == "region_codes").unwrap().primary_key,
        vec!["region", "code"]
    );

    let orders = block_on(catalog::describe_relation(&s, &schema, "orders")).unwrap();
    assert!(orders.editable);
    assert!(orders.column("id").unwrap().has_default, "auto_increment");
    assert!(orders.column("total_cents").unwrap().generated);
    assert_eq!(orders.foreign_keys.len(), 1);
    assert_eq!(orders.foreign_keys[0].to_table, "customers");
    let customers = block_on(catalog::describe_relation(&s, &schema, "customers")).unwrap();
    assert_eq!(customers.referenced_by.len(), 1);
    let logbook = block_on(catalog::describe_relation(&s, &schema, "logbook")).unwrap();
    assert!(!logbook.editable);
}

#[test]
fn pages_values_and_errors() {
    let Some(db) = database() else { return };
    let s = connect(&db);
    let schema = db.config.database.clone();
    let rel = block_on(catalog::describe_relation(&s, &schema, "orders")).unwrap();
    let req = PageRequest {
        schema: schema.clone(),
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
    assert!(page.columns[2].is_numeric());

    let got = block_on(exec::execute_collect(
        &s,
        "SELECT data, note FROM orders ORDER BY id".into(),
        None,
    ))
    .unwrap();
    assert_eq!(got.rows[0][0].as_deref(), Some("\\x00ff10"), "binary as hex");
    assert_eq!(got.rows[1][1], None);

    let upd = block_on(exec::execute_collect(
        &s,
        "UPDATE customers SET balance = 1".into(),
        None,
    ))
    .unwrap();
    assert!(!upd.returned_rows);
    assert_eq!(upd.count, 3, "rows matched, not only the ones changed");

    let err = block_on(exec::execute_collect(&s, "SELECT nope FROM customers".into(), None)).unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
    assert_eq!(err.sqlstate(), Some("1054"));

    let capped = block_on(exec::execute_collect(&s, "SELECT * FROM customers".into(), Some(2))).unwrap();
    assert_eq!(capped.rows.len(), 2);
    assert!(capped.truncated);
    // The session still works after a capped read.
    assert_eq!(scalar(&s, "SELECT 1").as_deref(), Some("1"));
}

#[test]
fn edits_commit_in_one_transaction() {
    let Some(db) = database() else { return };
    let s = connect(&db);
    let schema = db.config.database.clone();
    let rel = block_on(catalog::describe_relation(&s, &schema, "region_codes")).unwrap();
    let changes = ChangeSet {
        updates: vec![RowUpdate {
            key: vec![("region".into(), "eu".into()), ("code".into(), "1".into())],
            // The same value it has: still one row, thanks to client_found_rows.
            set: vec![("label".into(), CellValue::Text("one".into()))],
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
    block_on(edit::commit(&s, &rel, changes)).expect("commit");
    assert_eq!(scalar(&s, "SELECT COUNT(*) FROM region_codes").as_deref(), Some("2"));

    let customers = block_on(catalog::describe_relation(&s, &schema, "customers")).unwrap();
    let failing = ChangeSet {
        updates: vec![
            RowUpdate {
                key: vec![("id".into(), "1".into())],
                set: vec![("name".into(), CellValue::Text("Anne".into()))],
            },
            RowUpdate {
                key: vec![("id".into(), "2".into())],
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
}

#[test]
fn read_only_cancel_and_wrong_password() {
    let Some(db) = database() else { return };
    let mut ro = db.config.clone();
    ro.read_only = true;
    let s = block_on(session::connect(&ro, db.password.clone())).unwrap();
    let err = block_on(exec::execute_collect(&s, "DELETE FROM logbook".into(), None)).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("read"), "{err}");

    let s = connect(&db);
    let mut rx = exec::execute(&s, "SELECT SLEEP(20)".into(), None);
    std::thread::sleep(std::time::Duration::from_millis(500));
    block_on(s.cancel()).unwrap();
    let started = std::time::Instant::now();
    block_on(async { while rx.recv().await.is_some() {} });
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the sleep was stopped"
    );

    let mut bad = db.config.clone();
    bad.ssl_mode = SslMode::Disable;
    let err = block_on(session::connect(&bad, Some("wrong".into()))).unwrap_err();
    assert!(err.is_auth_failure(), "{err}");
}

#[test]
fn backup_and_guarded_restore() {
    use baselynx_core::backup::{self, BackupJob};
    use baselynx_core::config::{BackupFormat, BackupPlan, BackupSettings};
    let Some(db) = database() else { return };
    let tools = std::env::var("BASELYNX_TEST_MYSQL_TOOLS").ok();
    let dir = tempfile::tempdir().unwrap();
    let mut settings = BackupSettings::default_in(dir.path());
    settings.tools = tools.clone();
    let mut source = db.config.clone();
    source.config_key = Some("server:source".into());
    let run = block_on(backup::run_backup(BackupJob {
        config: source,
        password: db.password.clone(),
        settings,
        plan: BackupPlan::manual(vec![BackupFormat::Dump]),
    }))
    .expect("mysqldump");
    assert_eq!(run.manifest.engine, Engine::Mysql);
    assert_eq!(
        run.manifest.formats,
        vec![BackupFormat::Sql],
        "MySQL backups are SQL files"
    );

    let target = database().unwrap();
    // Not empty: the schema is already there.
    let mut target_cfg = target.config.clone();
    target_cfg.allow_restore = true;
    let err = block_on(backup::restore(
        target_cfg.clone(),
        target.password.clone(),
        run.dir.clone(),
        BackupFormat::Sql,
        tools.clone(),
        target_cfg.database.clone(),
    ))
    .unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");

    // An empty one takes it.
    let s = connect(&target);
    block_on(exec::execute_collect(&s, "DROP VIEW big_orders".into(), None)).unwrap();
    for t in ["orders", "customers", "region_codes", "logbook"] {
        block_on(exec::execute_collect(&s, format!("DROP TABLE {t}"), None)).unwrap();
    }
    block_on(backup::restore(
        target_cfg.clone(),
        target.password.clone(),
        run.dir.clone(),
        BackupFormat::Sql,
        tools,
        target_cfg.database.clone(),
    ))
    .expect("restore");
    assert_eq!(scalar(&s, "SELECT COUNT(*) FROM orders").as_deref(), Some("3"));
    assert_eq!(
        scalar(&s, "SELECT HEX(data) FROM orders WHERE id = 1").as_deref(),
        Some("00FF10")
    );
}
