//! Test database harness.
//!
//! The server is `BASELYNX_TEST_DSN` when set (a `postgres://` URL of a role that may create
//! databases). Otherwise an embedded server is started once under `target/baselynx-testdb`
//! on port 54329 and left running for later test runs.
//!
//! The seed is loaded once into a template database `baselynx_seed`; each test binary gets
//! its own copy, `baselynx_test_<uuid>`, so tests may change data.

#![allow(dead_code)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use baselynx_core::model::{ConnectionConfig, SslMode, parse_connection_url};
use baselynx_core::session::{self, Session};

const EMBEDDED_PORT: u16 = 54329;
const EMBEDDED_PASSWORD: &str = "baselynx-test";
const SEED: &str = include_str!("../../../../fixtures/seed.sql");

pub struct TestDb {
    pub config: ConnectionConfig,
    pub password: Option<String>,
}

/// Runs a future on the core runtime from a plain `#[test]` thread.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    baselynx_core::runtime::runtime().block_on(fut)
}

pub fn db() -> &'static TestDb {
    static DB: OnceLock<TestDb> = OnceLock::new();
    DB.get_or_init(|| block_on(prepare()))
}

pub fn connect() -> Arc<Session> {
    let db = db();
    block_on(session::connect(&db.config, db.password.clone())).expect("connect to the test database")
}

fn seed_version() -> String {
    // FNV-1a over the seed, so editing seed.sql rebuilds the template.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in SEED.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("baselynx-seed-{h:016x}")
}

async fn prepare() -> TestDb {
    let (admin_cfg, password) = match std::env::var("BASELYNX_TEST_DSN") {
        Ok(dsn) => dsn_config(&dsn),
        Err(_) => embedded().await,
    };
    let admin = session::connect(&admin_cfg, password.clone())
        .await
        .expect("connect to the test server");
    let client = admin.client();
    let version = seed_version();

    // Remove copies left by earlier runs.
    let old = client
        .query(
            "SELECT datname::text FROM pg_database WHERE datname LIKE 'baselynx\\_test\\_%'",
            &[],
        )
        .await
        .unwrap();
    for r in old {
        let name: String = r.get(0);
        let _ = client
            .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .await;
    }

    let current: Option<String> = client
        .query_opt(
            "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = 'baselynx_seed'",
            &[],
        )
        .await
        .unwrap()
        .and_then(|r| r.get(0));
    if current.as_deref() != Some(version.as_str()) {
        client
            .batch_execute("DROP DATABASE IF EXISTS baselynx_seed WITH (FORCE)")
            .await
            .unwrap();
        client.batch_execute("CREATE DATABASE baselynx_seed").await.unwrap();
        let mut seed_cfg = admin_cfg.clone();
        seed_cfg.database = "baselynx_seed".into();
        let seed = session::connect(&seed_cfg, password.clone()).await.unwrap();
        seed.client().batch_execute(SEED).await.expect("load fixtures/seed.sql");
        drop(seed);
        client
            .batch_execute(&format!("COMMENT ON DATABASE baselynx_seed IS '{version}'"))
            .await
            .unwrap();
    }
    let name = format!("baselynx_test_{}", uuid::Uuid::new_v4().simple());
    client
        .batch_execute(&format!("CREATE DATABASE \"{name}\" TEMPLATE baselynx_seed"))
        .await
        .unwrap();
    let mut config = admin_cfg;
    config.database = name;
    config.name = "test".into();
    TestDb { config, password }
}

fn dsn_config(dsn: &str) -> (ConnectionConfig, Option<String>) {
    let u = parse_connection_url(dsn).expect("BASELYNX_TEST_DSN must be a postgres:// URL");
    let mut c = ConnectionConfig::new("test-admin");
    if let Some(h) = u.host {
        c.host = h;
    }
    if let Some(p) = u.port {
        c.port = p;
    }
    if let Some(user) = u.user {
        c.user = user;
    }
    c.database = u.database.unwrap_or_else(|| "postgres".into());
    c.ssl_mode = u.ssl_mode.unwrap_or(SslMode::Prefer);
    (c, u.password)
}

async fn embedded() -> (ConnectionConfig, Option<String>) {
    let mut c = ConnectionConfig::new("test-admin");
    c.host = "127.0.0.1".into();
    c.port = EMBEDDED_PORT;
    c.user = "postgres".into();
    c.database = "postgres".into();
    c.ssl_mode = SslMode::Disable;
    let password = Some(EMBEDDED_PASSWORD.to_string());

    // Already running from an earlier test run?
    if session::connect(&c, password.clone()).await.is_ok() {
        return (c, password);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/baselynx-testdb");
    let settings = postgresql_embedded::SettingsBuilder::new()
        .installation_dir(root.join("install"))
        .data_dir(root.join("data"))
        .password_file(root.join("password"))
        .host("127.0.0.1")
        .port(EMBEDDED_PORT)
        .username("postgres")
        .password(EMBEDDED_PASSWORD)
        .temporary(false)
        .timeout(Some(std::time::Duration::from_secs(180)))
        .build();
    let mut pg = postgresql_embedded::PostgreSQL::new(settings);
    pg.setup()
        .await
        .expect("install the embedded PostgreSQL (needs network on first run)");
    pg.start().await.expect("start the embedded PostgreSQL");
    // Not stopped on drop: `temporary(false)` keeps the server for the next run.
    std::mem::forget(pg);
    (c, password)
}
