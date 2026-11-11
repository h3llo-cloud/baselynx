//! Opt-in check against a real cluster: a config file and one database in it.
//!
//! BASELYNX_LIVE_CONFIG=path/to/baselynx.yaml BASELYNX_LIVE_DB="shop-prod · orders-db" \
//!   cargo test -p baselynx-core --test kube_live -- --ignored --nocapture
//!
//! Read-only: connects, lists schemas, runs and cancels `pg_sleep`. Prints no credentials.

use std::time::{Duration, Instant};

use baselynx_core::{catalog, config, exec, runtime, session};

#[test]
#[ignore]
fn connects_through_a_port_forward() {
    let file = std::env::var("BASELYNX_LIVE_CONFIG").expect("set BASELYNX_LIVE_CONFIG");
    let name = std::env::var("BASELYNX_LIVE_DB").expect("set BASELYNX_LIVE_DB");
    let parsed = config::load(std::path::Path::new(&file)).unwrap();
    eprintln!("{} databases", parsed.databases.len());
    let mut db = parsed
        .databases
        .into_iter()
        .find(|d| d.config.name == name)
        .expect("no such database in the file");
    // Look without touching.
    db.config.read_only = true;
    runtime::runtime().block_on(async {
        let started = Instant::now();
        let s = session::connect(&db.config, None).await.unwrap();
        eprintln!(
            "connected in {:?}, PostgreSQL {}",
            started.elapsed(),
            s.server_version()
        );
        let schemas = catalog::list_schemas(&s).await.unwrap();
        let rels = catalog::list_all_relations(&s).await.unwrap();
        eprintln!("{} schemas, {} relations", schemas.len(), rels.len());

        let mut rx = exec::execute(&s, "SELECT pg_sleep(5)".into(), None);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let started = Instant::now();
        s.cancel().await.unwrap();
        let mut cancelled = false;
        while let Some(ev) = rx.recv().await {
            if let Err(e) = ev {
                cancelled = e.is_cancel();
            }
        }
        assert!(cancelled, "the sleep was not cancelled");
        eprintln!("cancelled through a second port-forward in {:?}", started.elapsed());
        let got = exec::execute_collect(&s, "SELECT 1".into(), None).await.unwrap();
        assert_eq!(got.rows[0][0].as_deref(), Some("1"));
    });
}
