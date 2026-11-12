//! Persisted data: projects, connections and tab state.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
    #[serde(default)]
    pub tabs: Vec<TabState>,
    #[serde(default)]
    pub active_tab: Option<Uuid>,
    /// Width of the record panel in table tabs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_width: Option<f32>,
    /// Filters used per table, `<connection>/<schema>.<table>` → oldest first, last 20.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub filter_history: std::collections::BTreeMap<String, Vec<String>>,
    /// The `baselynx.yaml` this project was opened from; it is read again at every start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
    /// Where backups go and how many are kept; from the config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backups: Option<crate::config::BackupSettings>,
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            connections: Vec::new(),
            tabs: Vec::new(),
            active_tab: None,
            record_width: None,
            filter_history: Default::default(),
            config_file: None,
            backups: None,
        }
    }

    pub fn connection(&self, id: Uuid) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyFull => "verify-full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" | "false" => Some(SslMode::Disable),
            "allow" | "prefer" | "preferred" => Some(SslMode::Prefer),
            "require" | "required" | "true" | "verify-ca" => Some(SslMode::Require),
            "verify-full" | "verify-identity" => Some(SslMode::VerifyFull),
            _ => None,
        }
    }
}

/// The database engine of a connection. Postgres is the default, so projects saved
/// before other engines existed load unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    #[default]
    Postgres,
    /// MySQL and MariaDB.
    Mysql,
    /// A database file; `ConnectionConfig::database` holds its path.
    Sqlite,
}

impl Engine {
    pub const ALL: [Engine; 3] = [Engine::Postgres, Engine::Mysql, Engine::Sqlite];

    pub fn dialect(self) -> crate::sql::Dialect {
        match self {
            Engine::Postgres => crate::sql::Dialect::Postgres,
            Engine::Mysql => crate::sql::Dialect::MySql,
            Engine::Sqlite => crate::sql::Dialect::Sqlite,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::Mysql => "MySQL",
            Engine::Sqlite => "SQLite",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mysql => 3306,
            Engine::Sqlite => 0,
        }
    }

    /// A file, not a server: no host, port, user or password.
    pub fn is_file(self) -> bool {
        self == Engine::Sqlite
    }

    pub fn is_default(&self) -> bool {
        *self == Engine::Postgres
    }
}

/// The user's environment marker for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionColor {
    #[default]
    None,
    Green,
    Amber,
    Red,
}

impl ConnectionColor {
    pub const ALL: [ConnectionColor; 4] = [
        ConnectionColor::None,
        ConnectionColor::Green,
        ConnectionColor::Amber,
        ConnectionColor::Red,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ConnectionColor::None => "None",
            ConnectionColor::Green => "Green",
            ConnectionColor::Amber => "Amber",
            ConnectionColor::Red => "Red",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Engine::is_default")]
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default)]
    pub save_password: bool,
    #[serde(default)]
    pub color: ConnectionColor,
    /// Set for a database in Kubernetes: baselynx tunnels to it and reads its credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube: Option<crate::k8s::KubeTarget>,
    /// Look without touching: every session is read-only, and a Kubernetes
    /// database is reached through its replica service when it has one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Set for a connection from a config file: `cluster:<cluster>/<namespace>/<name>` or
    /// `server:<name>`. The file owns such connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_key: Option<String>,
    /// A backup schedule from the config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<crate::config::BackupPlan>,
    /// The config file lets a restore write into this database (only when it is empty).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_restore: bool,
}

impl ConnectionConfig {
    /// Where it points, for tooltips and prompts: `user@host:port/db`, or a SQLite file.
    pub fn describe(&self) -> String {
        if self.engine.is_file() {
            self.database.clone()
        } else {
            format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
        }
    }

    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            engine: Engine::Postgres,
            host: "localhost".into(),
            port: 5432,
            database: "postgres".into(),
            user: "postgres".into(),
            ssl_mode: SslMode::Prefer,
            save_password: true,
            color: ConnectionColor::None,
            kube: None,
            read_only: false,
            config_key: None,
            backup: None,
            allow_restore: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabState {
    Console {
        id: Uuid,
        connection: Uuid,
        title: String,
        #[serde(default)]
        text: String,
    },
    Table {
        id: Uuid,
        connection: Uuid,
        schema: String,
        table: String,
        #[serde(default)]
        filter: Option<String>,
        #[serde(default)]
        sort: Option<(String, SortDir)>,
        page_size: u32,
        #[serde(default)]
        refresh_secs: Option<u32>,
    },
}

impl TabState {
    pub fn id(&self) -> Uuid {
        match self {
            TabState::Console { id, .. } | TabState::Table { id, .. } => *id,
        }
    }

    pub fn connection(&self) -> Uuid {
        match self {
            TabState::Console { connection, .. } | TabState::Table { connection, .. } => *connection,
        }
    }
}

/// Fields parsed from a connection string: `postgres://`, `postgresql://`, `mysql://`,
/// `mariadb://` or `sqlite:`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedUrl {
    pub engine: Engine,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<SslMode>,
}

/// Parses `postgres://user:pass@host:port/db?sslmode=require`, the same shape with
/// `mysql://` or `mariadb://`, and `sqlite:///path/to/file.db` (or `sqlite:file.db`).
/// Returns `None` when the text is not such a string.
pub fn parse_connection_url(text: &str) -> Option<ParsedUrl> {
    let text = text.trim();
    if let Some(path) = text.strip_prefix("sqlite:") {
        return parse_sqlite(path);
    }
    let (engine, rest) = match text
        .strip_prefix("postgresql://")
        .or_else(|| text.strip_prefix("postgres://"))
    {
        Some(r) => (Engine::Postgres, r),
        None => (
            Engine::Mysql,
            text.strip_prefix("mysql://")
                .or_else(|| text.strip_prefix("mariadb://"))?,
        ),
    };
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match main.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (main, None),
    };
    let mut out = ParsedUrl {
        engine,
        ..Default::default()
    };
    let host_part = match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            match userinfo.split_once(':') {
                Some((u, p)) => {
                    out.user = non_empty(percent_decode(u));
                    out.password = Some(percent_decode(p));
                }
                None => out.user = non_empty(percent_decode(userinfo)),
            }
            host
        }
        None => authority,
    };
    // IPv6 in brackets: [::1]:5432
    if let Some(stripped) = host_part.strip_prefix('[') {
        if let Some((h, after)) = stripped.split_once(']') {
            out.host = non_empty(h.to_string());
            if let Some(p) = after.strip_prefix(':') {
                out.port = p.parse().ok();
            }
        }
    } else {
        match host_part.rsplit_once(':') {
            Some((h, p)) => {
                out.host = non_empty(percent_decode(h));
                out.port = p.parse().ok();
            }
            None => out.host = non_empty(percent_decode(host_part)),
        }
    }
    if let Some(path) = path {
        out.database = non_empty(percent_decode(path));
    }
    if let Some(query) = query {
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v);
            match k {
                "sslmode" | "ssl-mode" | "ssl_mode" => out.ssl_mode = SslMode::parse(&v),
                "user" => out.user = non_empty(v),
                "password" => out.password = Some(v),
                "dbname" => out.database = non_empty(v),
                "host" => out.host = non_empty(v),
                "port" => out.port = v.parse().ok(),
                _ => {}
            }
        }
    }
    Some(out)
}

/// `sqlite:///C:/data/app.db`, `sqlite:///home/me/app.db`, `sqlite://app.db`, `sqlite:app.db`.
fn parse_sqlite(after_scheme: &str) -> Option<ParsedUrl> {
    let path = after_scheme.strip_prefix("//").unwrap_or(after_scheme);
    let path = path.split('?').next().unwrap_or(path);
    // `/C:/…`: a Windows drive after the third slash.
    let b = path.as_bytes();
    let path = if b.len() > 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &path[1..]
    } else {
        path
    };
    let path = percent_decode(path);
    if path.is_empty() {
        return None;
    }
    Some(ParsedUrl {
        engine: Engine::Sqlite,
        database: Some(path),
        ..Default::default()
    })
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_url() {
        let u = parse_connection_url("postgres://alice:s%40cret@db.example.com:6543/billing?sslmode=require").unwrap();
        assert_eq!(u.user.as_deref(), Some("alice"));
        assert_eq!(u.password.as_deref(), Some("s@cret"));
        assert_eq!(u.host.as_deref(), Some("db.example.com"));
        assert_eq!(u.port, Some(6543));
        assert_eq!(u.database.as_deref(), Some("billing"));
        assert_eq!(u.ssl_mode, Some(SslMode::Require));
    }

    #[test]
    fn parses_minimal_and_ipv6() {
        let u = parse_connection_url("postgresql://localhost").unwrap();
        assert_eq!(u.host.as_deref(), Some("localhost"));
        assert_eq!(u.port, None);
        let u = parse_connection_url("postgres://bob@[::1]:5433/x").unwrap();
        assert_eq!(u.host.as_deref(), Some("::1"));
        assert_eq!(u.port, Some(5433));
        assert_eq!(u.user.as_deref(), Some("bob"));
        assert!(parse_connection_url("mongodb://x").is_none());
        assert!(parse_connection_url("hello").is_none());
    }

    #[test]
    fn parses_mysql_and_sqlite() {
        let u = parse_connection_url("mysql://app:pw@db.example:3307/shop?ssl-mode=REQUIRED").unwrap();
        assert_eq!(u.engine, Engine::Mysql);
        assert_eq!((u.host.as_deref(), u.port), (Some("db.example"), Some(3307)));
        assert_eq!(u.database.as_deref(), Some("shop"));
        assert_eq!(u.ssl_mode, Some(SslMode::Require));
        assert_eq!(
            parse_connection_url("mariadb://root@localhost/x").unwrap().engine,
            Engine::Mysql
        );
        let w = parse_connection_url("sqlite:///C:/data/app.db").unwrap();
        assert_eq!(
            (w.engine, w.database.as_deref()),
            (Engine::Sqlite, Some("C:/data/app.db"))
        );
        let unix = parse_connection_url("sqlite:///home/me/app.db").unwrap();
        assert_eq!(unix.database.as_deref(), Some("/home/me/app.db"));
        assert_eq!(
            parse_connection_url("sqlite:app.db").unwrap().database.as_deref(),
            Some("app.db")
        );
        assert_eq!(
            parse_connection_url("sqlite://my%20app.db")
                .unwrap()
                .database
                .as_deref(),
            Some("my app.db")
        );
        assert!(parse_connection_url("sqlite:").is_none());
        assert_eq!(
            parse_connection_url("postgres://h/db").unwrap().engine,
            Engine::Postgres
        );
        // Projects saved before engines existed are Postgres.
        let old: ConnectionConfig = serde_json::from_str(
            r#"{"id":"6f1c2b8e-1c55-4b0e-9d43-3f1a2b3c4d5e","name":"x","host":"h","port":5432,"database":"d","user":"u"}"#,
        )
        .unwrap();
        assert_eq!(old.engine, Engine::Postgres);
        assert!(!serde_json::to_string(&old).unwrap().contains("engine"));
    }

    #[test]
    fn percent_decode_edge() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%41"), "A");
    }

    #[test]
    fn tab_state_round_trip() {
        let t = TabState::Table {
            id: Uuid::new_v4(),
            connection: Uuid::new_v4(),
            schema: "public".into(),
            table: "t".into(),
            filter: Some("id > 1".into()),
            sort: Some(("id".into(), SortDir::Desc)),
            page_size: 500,
            refresh_secs: Some(5),
        };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<TabState>(&json).unwrap(), t);
    }
}
