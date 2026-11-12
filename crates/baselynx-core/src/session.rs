//! One database connection: PostgreSQL, MySQL/MariaDB or SQLite.
//!
//! The rest of the core works through [`Session`] and branches on [`Conn`] where engines
//! differ; Postgres code lives here and in the modules that predate other engines, the
//! others in [`crate::engine`].

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::DuplexStream;
use tokio_postgres::Client;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::APP_NAME;
use crate::engine::sqlite::Lite;
use crate::error::{CoreError, Result};
use crate::k8s::{self, KubeTarget};
use crate::model::{ConnectionConfig, Engine, SslMode};
use crate::runtime;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The engine-specific connection inside a [`Session`].
pub(crate) enum Conn {
    Pg { client: Client, tls: MakeRustlsConnect },
    Mysql(crate::engine::mysql::My),
    Sqlite(Lite),
}

pub struct Session {
    conn: Conn,
    engine: Engine,
    server_version: String,
    /// Set when the session runs through a Kubernetes port-forward; cancel requests need
    /// their own stream.
    kube: Option<KubeTarget>,
    /// Behind a connection pooler (PgBouncer in transaction mode): named prepared
    /// statements and session state do not survive between statements.
    pooled: AtomicBool,
    read_only: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("engine", &self.engine)
            .field("server_version", &self.server_version)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Session {
    pub(crate) fn new(conn: Conn, engine: Engine, server_version: String, read_only: bool) -> Self {
        Self {
            conn,
            engine,
            server_version,
            kube: None,
            pooled: AtomicBool::new(false),
            read_only,
        }
    }

    pub(crate) fn conn(&self) -> &Conn {
        &self.conn
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    pub fn dialect(&self) -> crate::sql::Dialect {
        self.engine.dialect()
    }

    /// The Postgres client. Panics for other engines: call it only in Postgres code paths
    /// (and Postgres tests).
    pub fn client(&self) -> &Client {
        match &self.conn {
            Conn::Pg { client, .. } => client,
            _ => panic!("client() on a {} session", self.engine.label()),
        }
    }

    /// `server_version` as the server reports it, e.g. `17.2`.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// False behind a connection pooler, where named prepared statements are not safe in
    /// transaction pooling.
    pub fn named_statements_ok(&self) -> bool {
        !self.is_pooled()
    }

    /// True when the session runs through a pooler: first guessed from a `-pooler` service
    /// name, then confirmed by the server's answer to a named statement (D8).
    pub fn is_pooled(&self) -> bool {
        self.pooled.load(Ordering::Relaxed)
    }

    pub fn mark_pooled(&self) {
        self.pooled.store(true, Ordering::Relaxed);
    }

    /// Every transaction of this session is read-only (3.8).
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The kube target when the session goes through a port-forward.
    pub fn kube(&self) -> Option<&KubeTarget> {
        self.kube.as_ref()
    }

    pub fn is_closed(&self) -> bool {
        match &self.conn {
            Conn::Pg { client, .. } => client.is_closed(),
            Conn::Mysql(_) | Conn::Sqlite(_) => false,
        }
    }

    /// Asks the server to cancel whatever this session is running. Safe to call when idle.
    pub fn cancel(self: &Arc<Self>) -> impl Future<Output = Result<()>> + Send + 'static {
        let pg = match &self.conn {
            Conn::Pg { client, tls } => Some((client.cancel_token(), tls.clone())),
            Conn::Mysql(_) => None,
            Conn::Sqlite(lite) => {
                lite.interrupt();
                None
            }
        };
        let kube = self.kube.clone();
        let mysql = (self.engine == Engine::Mysql).then(|| self.clone());
        runtime::spawn(async move {
            if let Some(session) = mysql {
                return crate::engine::mysql::cancel(&session).await;
            }
            let Some((token, mut tls)) = pg else { return Ok(()) };
            match kube {
                None => token.cancel_query(tls).await?,
                Some(t) => {
                    let stream = k8s::open_stream(&t).await?;
                    let connect = MakeTlsConnect::<DuplexStream>::make_tls_connect(&mut tls, &kube_host(&t))
                        .map_err(|e| CoreError::Connect(e.to_string()))?;
                    token.cancel_query_raw(stream, connect).await?
                }
            }
            Ok(())
        })
    }
}

/// Opens a session on the core runtime. The password is used once and not kept.
pub fn connect(
    config: &ConnectionConfig,
    password: Option<String>,
) -> impl Future<Output = Result<Arc<Session>>> + Send + 'static {
    let config = config.clone();
    runtime::spawn(async move {
        match config.engine {
            Engine::Sqlite => return Ok(Arc::new(crate::engine::sqlite::connect(&config).await?)),
            Engine::Mysql => return Ok(Arc::new(crate::engine::mysql::connect(&config, password).await?)),
            Engine::Postgres => {}
        }
        let session = connect_any(config.clone(), password).await?;
        if config.read_only {
            // The server refuses every write of the session with its own error.
            session
                .client()
                .simple_query("SET default_transaction_read_only = on")
                .await?;
        }
        Ok(session)
    })
}

async fn connect_any(config: ConnectionConfig, password: Option<String>) -> Result<Arc<Session>> {
    {
        if let Some(mut target) = config.kube.clone() {
            // Read-only goes to the replica service when the database has one.
            if config.read_only
                && let Some(ro) = target.replica.clone()
            {
                target.service = ro;
            }
            return connect_kube(config, target, password).await;
        }
        let mut pg = tokio_postgres::Config::new();
        pg.host(&config.host)
            .port(config.port)
            .dbname(&config.database)
            .user(&config.user)
            .application_name(APP_NAME)
            .connect_timeout(CONNECT_TIMEOUT)
            .keepalives(true)
            .keepalives_idle(Duration::from_secs(60));
        if let Some(p) = &password {
            pg.password(p.as_bytes());
        }
        pg.ssl_mode(match config.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
        let tls = make_tls(config.ssl_mode)?;
        let (client, connection) =
            tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(1), pg.connect(tls.clone()))
                .await
                .map_err(|_| {
                    CoreError::Connect(format!("timed out connecting to {}:{}", config.host, config.port))
                })??;
        tokio::spawn(async move {
            // Ends when the client is dropped or the server goes away.
            let _ = connection.await;
        });
        let server_version = server_version(&client).await?;
        Ok(Arc::new(Session::new(
            Conn::Pg { client, tls },
            Engine::Postgres,
            server_version,
            config.read_only,
        )))
    }
}

async fn server_version(client: &Client) -> Result<String> {
    let rows = client.simple_query("SHOW server_version").await?;
    Ok(rows
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
            _ => None,
        })
        .unwrap_or_default())
}

/// The in-cluster name of the service, used for TLS server names.
fn kube_host(t: &KubeTarget) -> String {
    format!("{}.{}.svc", t.service, t.namespace)
}

/// Connects through a port-forward stream with credentials from the target's secret.
async fn connect_kube(config: ConnectionConfig, target: KubeTarget, password: Option<String>) -> Result<Arc<Session>> {
    let creds = k8s::credentials(&target, &config).await?;
    let mut pg = tokio_postgres::Config::new();
    pg.dbname(&creds.database)
        .user(&creds.username)
        .application_name(APP_NAME)
        .ssl_mode(match config.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
    if let Some(p) = creds.password.or(password) {
        pg.password(p.as_bytes());
    }
    let mut tls = make_tls(config.ssl_mode)?;
    let stream = k8s::open_stream(&target).await?;
    let connect = MakeTlsConnect::<DuplexStream>::make_tls_connect(&mut tls, &kube_host(&target))
        .map_err(|e| CoreError::Connect(e.to_string()))?;
    let (client, connection) = tokio::time::timeout(CONNECT_TIMEOUT, pg.connect_raw(stream, connect))
        .await
        .map_err(|_| CoreError::Connect(format!("timed out connecting to {}", target.describe())))??;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let server_version = server_version(&client).await?;
    let mut session = Session::new(
        Conn::Pg { client, tls },
        Engine::Postgres,
        server_version,
        config.read_only,
    );
    session.pooled = AtomicBool::new(target.service.contains("pooler"));
    session.kube = Some(target);
    Ok(Arc::new(session))
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

fn native_roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut store = RootCertStore::empty();
            let loaded = rustls_native_certs::load_native_certs();
            for cert in loaded.certs {
                let _ = store.add(cert);
            }
            Arc::new(store)
        })
        .clone()
}

fn make_tls(mode: SslMode) -> Result<MakeRustlsConnect> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| CoreError::Internal(e.to_string()))?;
    let config = match mode {
        SslMode::VerifyFull => builder.with_root_certificates(native_roots()).with_no_client_auth(),
        // `prefer` and `require` encrypt without verifying the certificate, as libpq does.
        SslMode::Disable | SslMode::Prefer | SslMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider())))
            .with_no_client_auth(),
    };
    Ok(MakeRustlsConnect::new(config))
}

#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
