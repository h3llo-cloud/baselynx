//! Backups and guarded restores against the test server (pg_dump, pg_restore and psql from
//! the embedded installation, or PATH when `BASELYNX_TEST_DSN` names another server).

mod common;

use std::path::PathBuf;

use baselynx_core::backup::{self, BackupJob};
use baselynx_core::config::{BackupFormat, BackupPlan, BackupSettings};
use baselynx_core::model::ConnectionConfig;
use baselynx_core::session;
use common::{block_on, db};

/// The embedded server's bin directory, when the tests run against it.
fn tools() -> Option<String> {
    let install = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/baselynx-testdb/install");
    std::fs::read_dir(install)
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin"))
        .find(|p| p.join(if cfg!(windows) { "pg_dump.exe" } else { "pg_dump" }).is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

fn settings(dir: &std::path::Path) -> BackupSettings {
    BackupSettings {
        tools: tools(),
        ..BackupSettings::default_in(dir)
    }
}

/// An empty database on the test server; the harness drops `baselynx_test_*` on its next run.
fn empty_db() -> ConnectionConfig {
    let base = &db().config;
    let mut admin = base.clone();
    admin.database = "postgres".into();
    let name = format!("baselynx_test_restore_{}", uuid::Uuid::new_v4().simple());
    block_on(async {
        let s = session::connect(&admin, db().password.clone()).await.unwrap();
        s.client()
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
            .unwrap();
    });
    let mut c = base.clone();
    c.database = name;
    c.name = "restore target".into();
    c.allow_restore = true;
    c.config_key = Some("server:restore-target".into());
    c
}

fn count(c: &ConnectionConfig, sql: &str) -> i64 {
    block_on(async {
        let s = session::connect(c, db().password.clone()).await.unwrap();
        s.client().query_one(sql, &[]).await.unwrap().get(0)
    })
}

const CUSTOMERS: &str = "SELECT count(*) FROM demo.customers";
const FKS: &str = "SELECT count(*) FROM pg_constraint WHERE contype = 'f' AND connamespace = 'demo'::regnamespace";

#[test]
fn backup_then_guarded_restore() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = db().config.clone();
    source.config_key = Some("server:source".into());
    let job = BackupJob {
        config: source.clone(),
        password: db().password.clone(),
        settings: settings(dir.path()),
        plan: BackupPlan::manual(vec![BackupFormat::Dump, BackupFormat::Sql]),
    };
    let run = block_on(backup::run_backup(job)).expect("backup");
    let names: Vec<_> = run.manifest.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.contains(&"schema.sql") && names.contains(&"data.sql") && names.contains(&"indexes.sql"));
    assert!(run.manifest.pg_dump_version.contains("pg_dump"));
    assert!(!run.manifest.server_version.is_empty());
    assert_eq!(run.manifest.database, source.database);
    backup::verify_files(&run.dir).expect("checksums match");
    let conn_dir = backup::connection_dir(&settings(dir.path()), &source);
    assert_eq!(backup::list_runs(&conn_dir).len(), 1);
    assert!(backup::last_backup(&settings(dir.path()), &source).is_some());
    let manifest_text = std::fs::read_to_string(run.dir.join("manifest.json")).unwrap();
    assert!(!manifest_text.contains("baselynx-test"), "no password in the manifest");

    let tools = tools();
    let restore = |target: &ConnectionConfig, format: BackupFormat, typed: &str| {
        block_on(backup::restore(
            target.clone(),
            db().password.clone(),
            run.dir.clone(),
            format,
            tools.clone(),
            typed.to_string(),
        ))
    };

    // Into the source itself: it is not empty.
    let mut into_source = source.clone();
    into_source.allow_restore = true;
    let pre = block_on(backup::preflight(
        into_source.clone(),
        db().password.clone(),
        run.dir.clone(),
        tools.clone(),
    ))
    .unwrap();
    assert!(!pre.passed());
    assert!(pre.same_connection);
    let empty = pre.checks.iter().find(|c| c.label.contains("empty")).unwrap();
    assert!(!empty.ok && empty.detail.contains("demo."), "{empty:?}");
    let err = restore(&into_source, BackupFormat::Dump, &source.database)
        .unwrap_err()
        .to_string();
    assert!(err.contains("empty"), "{err}");

    // An empty database, but the config file does not allow restores.
    let target = empty_db();
    let mut not_allowed = target.clone();
    not_allowed.allow_restore = false;
    let err = restore(&not_allowed, BackupFormat::Dump, &target.database)
        .unwrap_err()
        .to_string();
    assert!(err.contains("allow"), "{err}");
    // Read-only.
    let mut read_only = target.clone();
    read_only.read_only = true;
    assert!(restore(&read_only, BackupFormat::Dump, &target.database).is_err());
    // The wrong name typed.
    let err = restore(&target, BackupFormat::Dump, "postgres")
        .unwrap_err()
        .to_string();
    assert!(err.contains("type"), "{err}");
    assert_eq!(
        count(&target, "SELECT count(*) FROM pg_namespace WHERE nspname = 'demo'"),
        0
    );

    // Every check passes: the dump goes in.
    let pre = block_on(backup::preflight(
        target.clone(),
        db().password.clone(),
        run.dir.clone(),
        tools.clone(),
    ))
    .unwrap();
    assert!(pre.passed(), "{:?}", pre.checks);
    assert!(!pre.same_connection);
    assert_eq!(pre.database.as_deref(), Some(target.database.as_str()));
    let report = restore(&target, BackupFormat::Dump, &target.database).expect("restore the dump");
    assert_eq!(report.format, BackupFormat::Dump);
    assert_eq!(count(&target, CUSTOMERS), count(&source, CUSTOMERS));
    assert_eq!(count(&target, FKS), count(&source, FKS));
    // A second restore into it is refused: it is no longer empty.
    assert!(restore(&target, BackupFormat::Dump, &target.database).is_err());

    // The SQL files go into another empty database.
    let target2 = empty_db();
    restore(&target2, BackupFormat::Sql, &target2.database).expect("restore the SQL files");
    assert_eq!(count(&target2, CUSTOMERS), count(&source, CUSTOMERS));
    assert_eq!(count(&target2, FKS), count(&source, FKS));

    // A changed file fails the check, and nothing is restored.
    let target3 = empty_db();
    let data = run.dir.join("data.sql");
    let mut text = std::fs::read_to_string(&data).unwrap();
    text.push_str("\n-- changed\n");
    std::fs::write(&data, text).unwrap();
    let err = restore(&target3, BackupFormat::Sql, &target3.database)
        .unwrap_err()
        .to_string();
    assert!(err.contains("changed"), "{err}");
    assert_eq!(
        count(&target3, "SELECT count(*) FROM pg_namespace WHERE nspname = 'demo'"),
        0
    );
}

#[test]
fn a_failed_restore_leaves_the_database_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = db().config.clone();
    source.config_key = Some("server:source2".into());
    let job = BackupJob {
        config: source,
        password: db().password.clone(),
        settings: settings(dir.path()),
        plan: BackupPlan::manual(vec![BackupFormat::Sql]),
    };
    let run = block_on(backup::run_backup(job)).expect("backup");
    // Break the last file in a way the checksum still matches: rewrite the manifest too.
    let post = run.dir.join("indexes.sql");
    std::fs::write(&post, "CREATE INDEX broken ON demo.no_such_table (x);\n").unwrap();
    let mut m = run.manifest.clone();
    let (sum, bytes) = {
        use sha2::Digest;
        let bytes = std::fs::read(&post).unwrap();
        let h = sha2::Sha256::digest(&bytes);
        (
            h.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            bytes.len() as u64,
        )
    };
    let f = m.files.iter_mut().find(|f| f.name == "indexes.sql").unwrap();
    f.sha256 = sum;
    f.bytes = bytes;
    std::fs::write(run.dir.join("manifest.json"), serde_json::to_string(&m).unwrap()).unwrap();

    let target = empty_db();
    let err = block_on(backup::restore(
        target.clone(),
        db().password.clone(),
        run.dir.clone(),
        BackupFormat::Sql,
        tools(),
        target.database.clone(),
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("psql failed"), "{err}");
    assert_eq!(
        count(&target, "SELECT count(*) FROM pg_namespace WHERE nspname = 'demo'"),
        0,
        "one transaction: nothing stays"
    );
}
