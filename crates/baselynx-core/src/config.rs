//! The baselynx config file, `baselynx.yaml`: databases in Kubernetes clusters and on plain
//! servers, their modes, backup schedules and whether a restore may ever write into them.
//!
//! ```yaml
//! version: 1
//! project: Production
//! backups:
//!   directory: backups
//! clusters:
//!   shop-prod:
//!     kubeconfig: ~/.kube/shop-prod.yaml
//!     namespaces:
//!       billing:
//!         orders-db:
//!           service: orders-db-rw
//!           replica: orders-db-ro
//!           credentials: orders-db-credentials
//!           backup: {every: 24h}
//! servers:
//!   local:
//!     host: 127.0.0.1
//!     database: scratch
//!     user: me
//! ```
//!
//! The file is the source of truth for what it lists: opening it again updates those
//! connections, adds new ones and removes the ones it no longer lists. Connections made by
//! hand in the same project are left alone. Ids, names given in the app, colours not set in
//! the file and every tab survive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CoreError, Result};
use crate::k8s::{KubeCredentials, KubeTarget};
use crate::model::{ConnectionColor, ConnectionConfig, ParsedUrl, Project, SslMode, parse_connection_url};

/// The file name baselynx looks for next to its binary.
pub const FILE_NAME: &str = "baselynx.yaml";
const VERSION: u32 = 1;

// ---------------------------------------------------------------- file format

/// A key that takes either a scalar or a mapping. Serde's untagged enums would report only
/// "did not match any variant"; this reports what is wrong in the mapping (an unknown key, a
/// wrong type), and the parser adds where.
macro_rules! either_or {
    ($ty:ident, String => $scalar:ident, $full:ty => $map:ident) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                use serde::de::Error as _;
                match serde_json::Value::deserialize(d)? {
                    serde_json::Value::String(s) => Ok($ty::$scalar(s)),
                    other => serde_json::from_value::<$full>(other)
                        .map($ty::$map)
                        .map_err(D::Error::custom),
                }
            }
        }
    };
    ($ty:ident, Bool => $scalar:ident, $full:ty => $map:ident) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                use serde::de::Error as _;
                match serde_json::Value::deserialize(d)? {
                    serde_json::Value::Bool(b) => Ok($ty::$scalar(b)),
                    other => serde_json::from_value::<$full>(other)
                        .map($ty::$map)
                        .map_err(D::Error::custom),
                }
            }
        }
    };
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    /// The project the file opens as.
    project: String,
    #[serde(default)]
    backups: Option<BackupsSection>,
    #[serde(default)]
    clusters: IndexMap<String, ClusterSection>,
    #[serde(default)]
    servers: IndexMap<String, ServerEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupsSection {
    /// Where backups go; relative to the config file.
    #[serde(default)]
    directory: Option<String>,
    /// The directory of pg_dump, pg_restore and psql; default: PATH, then the newest
    /// PostgreSQL installation on Windows.
    #[serde(default)]
    tools: Option<String>,
    /// Formats for a schedule that names none.
    #[serde(default)]
    formats: Option<Vec<BackupFormat>>,
    #[serde(default)]
    keep: Option<Keep>,
    /// How long one pg_dump may run.
    #[serde(default)]
    timeout: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterSection {
    kubeconfig: String,
    #[serde(default)]
    context: Option<String>,
    /// Colour of every database in the cluster; default red when the name says prod.
    #[serde(default)]
    color: Option<ConnectionColor>,
    /// Kubernetes namespace → database name → database.
    #[serde(default)]
    namespaces: IndexMap<String, IndexMap<String, KubeDbSection>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KubeDbSection {
    /// The name shown in baselynx; default `<cluster> · <database name>`.
    #[serde(default)]
    name: Option<String>,
    /// The primary's service (a pooler works for browsing; backups prefer the replica).
    #[serde(default)]
    service: Option<String>,
    /// A pod, instead of a service.
    #[serde(default)]
    pod: Option<String>,
    /// The read-only replicas' service: read-only mode and backups use it.
    #[serde(default)]
    replica: Option<String>,
    /// The service (or container) port; default 5432.
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    credentials: Option<CredentialsSection>,
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    color: Option<ConnectionColor>,
    #[serde(default)]
    backup: Option<BackupSection>,
    /// Lets "Restore from backup" write into this database (when it is empty). Default false.
    #[serde(default)]
    allow_restore: bool,
}

#[derive(Debug)]
enum CredentialsSection {
    /// The name of a secret in the database's namespace.
    Secret(String),
    Full(CredentialsFull),
}

either_or!(CredentialsSection, String => Secret, CredentialsFull => Full);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsFull {
    #[serde(default)]
    secret: Option<String>,
    /// Which secret key holds what, when the secret does not use the usual names:
    /// `database`, `user`, `password`, `url`.
    #[serde(default)]
    keys: BTreeMap<String, String>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSection {
    #[serde(default)]
    name: Option<String>,
    /// `postgres://user:password@host:port/database?sslmode=…`; the keys below override
    /// its parts.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    ssl: Option<String>,
    /// Plain text, kept only in memory. Leave it out and baselynx asks once and keeps it in the
    /// system keyring.
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    color: Option<ConnectionColor>,
    #[serde(default)]
    backup: Option<BackupSection>,
    #[serde(default)]
    allow_restore: bool,
}

/// A server: a connection string, or keys.
// Lives only while the file is read.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum ServerEntry {
    Url(String),
    Full(ServerSection),
}

either_or!(ServerEntry, String => Url, ServerSection => Full);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Mode {
    ReadWrite,
    ReadOnly,
}

#[derive(Debug)]
enum BackupSection {
    /// `backup: false` (or `true`, every 24 hours).
    Flag(bool),
    Plan(BackupPlanSection),
}

either_or!(BackupSection, Bool => Flag, BackupPlanSection => Plan);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupPlanSection {
    /// At most one backup per this long, taken when baselynx starts: `12h`, `1d`, `90m`.
    #[serde(default)]
    every: Option<String>,
    #[serde(default)]
    formats: Option<Vec<BackupFormat>>,
    #[serde(default)]
    from: Option<BackupSource>,
    /// pg_dump --enable-row-security, for tables that force row-level security.
    #[serde(default)]
    row_security: bool,
    /// Tables pg_dump leaves out (pg_dump patterns).
    #[serde(default)]
    exclude_tables: Vec<String>,
    #[serde(default)]
    keep: Option<Keep>,
}

// ---------------------------------------------------------------- resolved settings

/// How a database is backed up. Stored on the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupPlan {
    /// Seconds between backups; one is taken at start-up when the last is older.
    pub every_secs: u64,
    pub formats: Vec<BackupFormat>,
    #[serde(default)]
    pub from: BackupSource,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub row_security: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep: Option<Keep>,
}

impl BackupPlan {
    pub fn every(&self) -> Duration {
        Duration::from_secs(self.every_secs)
    }

    /// A one-off backup of a connection that has no schedule.
    pub fn manual(formats: Vec<BackupFormat>) -> Self {
        Self {
            every_secs: 0,
            formats,
            from: BackupSource::Replica,
            row_security: false,
            exclude_tables: Vec::new(),
            keep: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupFormat {
    /// pg_dump's compressed custom format, restored with pg_restore.
    Dump,
    /// Plain SQL: `schema.sql`, `data.sql` and `indexes.sql`, restored with psql.
    Sql,
}

impl BackupFormat {
    pub fn label(self) -> &'static str {
        match self {
            BackupFormat::Dump => "dump",
            BackupFormat::Sql => "SQL",
        }
    }
}

/// Where a backup reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupSource {
    /// The replica when there is one, else the primary; the primary again if the replica fails.
    #[default]
    Replica,
    Primary,
}

/// Which backups to keep: the newest `last`, and the newest of each of the last `daily`
/// days, `weekly` weeks and `monthly` months. The newest backup is always kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    #[serde(default)]
    pub last: usize,
    #[serde(default)]
    pub daily: usize,
    #[serde(default)]
    pub weekly: usize,
    #[serde(default)]
    pub monthly: usize,
}

impl Default for Keep {
    fn default() -> Self {
        Self {
            last: 7,
            daily: 14,
            weekly: 8,
            monthly: 12,
        }
    }
}

/// Project-wide backup settings. Stored on the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupSettings {
    /// Absolute directory.
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<String>,
    pub formats: Vec<BackupFormat>,
    pub keep: Keep,
    pub timeout_secs: u64,
}

impl BackupSettings {
    /// Settings for a project without a config file: backups under baselynx's data directory.
    pub fn default_in(dir: &Path) -> Self {
        Self {
            directory: dir.join("backups").to_string_lossy().into_owned(),
            tools: None,
            formats: vec![BackupFormat::Dump],
            keep: Keep::default(),
            timeout_secs: 30 * 60,
        }
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// One database the file lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDb {
    pub config: ConnectionConfig,
    /// Set when the file gives a name; otherwise a name changed in the app is kept.
    pub explicit_name: bool,
    /// Set when the file gives a colour.
    pub explicit_color: bool,
    /// A password written in the file; it goes to the keyring, never the project.
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub project: String,
    pub backups: BackupSettings,
    pub databases: Vec<ConfigDb>,
}

// ---------------------------------------------------------------- parsing

/// Parses `1d`, `24h`, `90m`, `30s` and sums like `1h30m`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        total += n * match ch {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
    }
    if !num.is_empty() {
        return None;
    }
    (total > 0).then(|| Duration::from_secs(total))
}

fn duration_field(value: Option<&str>, default: Duration, what: &str) -> Result<Duration> {
    match value {
        None => Ok(default),
        Some(v) => parse_duration(v)
            .ok_or_else(|| CoreError::Invalid(format!("{what}: \"{v}\" is not a duration (use e.g. 24h, 90m, 1d)"))),
    }
}

fn resolve_path(p: &str, base: &Path) -> PathBuf {
    let expanded = match p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        Some(rest) => directories::BaseDirs::new()
            .map(|d| d.home_dir().join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

fn default_color(cluster: &str) -> ConnectionColor {
    if cluster.to_lowercase().contains("prod") {
        ConnectionColor::Red
    } else {
        ConnectionColor::None
    }
}

fn credential_key(name: &str) -> Option<&'static str> {
    match name {
        "database" | "dbname" => Some("database"),
        "user" | "username" => Some("username"),
        "password" => Some("password"),
        "url" | "connection_string" => Some("connection_string"),
        _ => None,
    }
}

/// Reads and checks a config file. Relative paths are relative to the file.
pub fn parse(text: &str, path: &Path) -> Result<ConfigFile> {
    let file: File =
        serde_saphyr::from_str(text).map_err(|e| CoreError::Invalid(format!("{}: {e}", path.display())))?;
    if file.version != VERSION {
        return Err(CoreError::Invalid(format!(
            "{}: version {} is not supported (this baselynx reads version {VERSION})",
            path.display(),
            file.version
        )));
    }
    let base = path.parent().unwrap_or(Path::new("."));
    let b = file.backups.unwrap_or(BackupsSection {
        directory: None,
        tools: None,
        formats: None,
        keep: None,
        timeout: None,
    });
    let backups = BackupSettings {
        directory: resolve_path(b.directory.as_deref().unwrap_or("backups"), base)
            .to_string_lossy()
            .into_owned(),
        tools: b.tools.map(|t| resolve_path(&t, base).to_string_lossy().into_owned()),
        formats: b
            .formats
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| vec![BackupFormat::Dump]),
        keep: b.keep.unwrap_or_default(),
        timeout_secs: duration_field(b.timeout.as_deref(), Duration::from_secs(30 * 60), "backups.timeout")?.as_secs(),
    };
    let plan = |section: Option<BackupSection>, at: &str| -> Result<Option<BackupPlan>> {
        let day = Duration::from_secs(86_400);
        Ok(match section {
            None | Some(BackupSection::Flag(false)) => None,
            Some(BackupSection::Flag(true)) => Some(BackupPlan {
                every_secs: day.as_secs(),
                formats: backups.formats.clone(),
                from: BackupSource::Replica,
                row_security: false,
                exclude_tables: Vec::new(),
                keep: None,
            }),
            Some(BackupSection::Plan(p)) => Some(BackupPlan {
                every_secs: duration_field(p.every.as_deref(), day, &format!("{at}.backup.every"))?.as_secs(),
                formats: p
                    .formats
                    .filter(|f| !f.is_empty())
                    .unwrap_or_else(|| backups.formats.clone()),
                from: p.from.unwrap_or_default(),
                row_security: p.row_security,
                exclude_tables: p.exclude_tables,
                keep: p.keep,
            }),
        })
    };

    let mut databases = Vec::new();
    for (cluster_name, cluster) in file.clusters {
        let kubeconfig = resolve_path(&cluster.kubeconfig, base).to_string_lossy().into_owned();
        for (namespace, dbs) in cluster.namespaces {
            for (key, db) in dbs {
                let at = format!("clusters.{cluster_name}.namespaces.{namespace}.{key}");
                let (service, pod) = match (db.service, db.pod) {
                    (Some(s), None) => (s, false),
                    (None, Some(p)) => (p, true),
                    _ => return Err(CoreError::Invalid(format!("{at}: give exactly one of service or pod"))),
                };
                let (credentials, database, user, password) = match db.credentials {
                    None => (KubeCredentials::default(), None, None, None),
                    Some(CredentialsSection::Secret(secret)) => (
                        KubeCredentials {
                            secret: Some(secret),
                            fields: BTreeMap::new(),
                        },
                        None,
                        None,
                        None,
                    ),
                    Some(CredentialsSection::Full(c)) => {
                        let mut fields = BTreeMap::new();
                        for (k, v) in c.keys {
                            let field = credential_key(&k).ok_or_else(|| {
                                CoreError::Invalid(format!(
                                    "{at}.credentials.keys: unknown key \"{k}\" (use database, user, password, url)"
                                ))
                            })?;
                            fields.insert(field.to_string(), v);
                        }
                        (
                            KubeCredentials {
                                secret: c.secret,
                                fields,
                            },
                            c.database,
                            c.user,
                            c.password,
                        )
                    }
                };
                if credentials.secret.is_none() && (database.is_none() || user.is_none()) {
                    return Err(CoreError::Invalid(format!(
                        "{at}: credentials need a secret, or a database and a user"
                    )));
                }
                let port = db.port.unwrap_or(5432);
                let target = KubeTarget {
                    cluster: cluster_name.clone(),
                    kubeconfig: kubeconfig.clone(),
                    context: cluster.context.clone(),
                    namespace: namespace.clone(),
                    forward: key.clone(),
                    service: service.clone(),
                    pod,
                    remote_port: port,
                    replica: db.replica,
                    credentials,
                };
                let color = db.color.or(cluster.color);
                let mut config =
                    ConnectionConfig::new(db.name.clone().unwrap_or_else(|| format!("{cluster_name} · {key}")));
                config.host = format!("{service}.{namespace}");
                config.port = port;
                config.database = database.unwrap_or_default();
                config.user = user.unwrap_or_default();
                config.ssl_mode = SslMode::Prefer;
                // A password in the file stays in memory; it never goes to the keyring.
                config.save_password = false;
                config.color = color.unwrap_or_else(|| default_color(&cluster_name));
                config.read_only = db.mode == Some(Mode::ReadOnly);
                config.kube = Some(target);
                config.config_key = Some(format!("cluster:{cluster_name}/{namespace}/{key}"));
                config.backup = plan(db.backup, &at)?;
                config.allow_restore = db.allow_restore;
                databases.push(ConfigDb {
                    config,
                    explicit_name: db.name.is_some(),
                    explicit_color: color.is_some(),
                    password,
                });
            }
        }
    }
    for (key, entry) in file.servers {
        let at = format!("servers.{key}");
        let s = match entry {
            ServerEntry::Url(url) => ServerSection {
                name: None,
                url: Some(url),
                host: None,
                port: None,
                database: None,
                user: None,
                ssl: None,
                password: None,
                mode: None,
                color: None,
                backup: None,
                allow_restore: false,
            },
            ServerEntry::Full(s) => s,
        };
        let url = match s.url.as_deref() {
            None => ParsedUrl::default(),
            Some(u) => parse_connection_url(u).ok_or_else(|| {
                CoreError::Invalid(format!("{at}: not a postgres:// or postgresql:// connection string"))
            })?,
        };
        let ssl_mode = match s.ssl.as_deref() {
            None => url.ssl_mode.unwrap_or(SslMode::Prefer),
            Some(v) => SslMode::parse(v).ok_or_else(|| {
                CoreError::Invalid(format!("{at}.ssl: \"{v}\" (use disable, prefer, require, verify-full)"))
            })?,
        };
        let host = s
            .host
            .or(url.host)
            .ok_or_else(|| CoreError::Invalid(format!("{at}: no host")))?;
        let database = s
            .database
            .or(url.database)
            .ok_or_else(|| CoreError::Invalid(format!("{at}: no database")))?;
        let user = s
            .user
            .or(url.user)
            .ok_or_else(|| CoreError::Invalid(format!("{at}: no user")))?;
        let password = s.password.or(url.password);
        let mut config = ConnectionConfig::new(s.name.clone().unwrap_or_else(|| key.clone()));
        config.host = host;
        config.port = s.port.or(url.port).unwrap_or(5432);
        config.database = database;
        config.user = user;
        config.ssl_mode = ssl_mode;
        // With a password in the file baselynx keeps it in memory; without one it asks once and
        // keeps the answer in the keyring.
        config.save_password = password.is_none();
        config.color = s.color.unwrap_or(ConnectionColor::None);
        config.read_only = s.mode == Some(Mode::ReadOnly);
        config.config_key = Some(format!("server:{key}"));
        config.backup = plan(s.backup, &at)?;
        config.allow_restore = s.allow_restore;
        databases.push(ConfigDb {
            config,
            explicit_name: s.name.is_some(),
            explicit_color: s.color.is_some(),
            password,
        });
    }
    Ok(ConfigFile {
        path: path.to_path_buf(),
        project: file.project,
        backups,
        databases,
    })
}

/// Checks `text` and writes it to `path`: never a file baselynx cannot read back. The previous
/// version is kept as `<name>.bak`; the new one is written beside it and renamed over it.
pub fn save(path: &Path, text: &str) -> Result<ConfigFile> {
    let parsed = parse(text, path)?;
    let storage = |e: std::io::Error| CoreError::Storage(format!("{}: {e}", path.display()));
    if path.is_file() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak)).map_err(storage)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(storage)?;
    std::fs::rename(&tmp, path).map_err(storage)?;
    Ok(parsed)
}

/// Line and column (1-based) of a parse error, when the message names them.
pub fn error_position(message: &str) -> Option<(usize, usize)> {
    let number_after = |key: &str| -> Option<usize> {
        let at = message.find(key)? + key.len();
        let digits: String = message[at..]
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    };
    Some((number_after("line")?, number_after("column").unwrap_or(1)))
}

/// Where to point at for a parse error in `text`: for an unknown key, the key's own line
/// (the parser names the start of the mapping that holds it); otherwise the parser's place.
pub fn locate_error(message: &str, text: &str) -> Option<(usize, usize)> {
    let reported = error_position(message);
    if let Some(start) = message.find("unknown field `") {
        let rest = &message[start + "unknown field `".len()..];
        if let Some(key) = rest.split('`').next() {
            let from = reported.map_or(0, |(l, _)| l.saturating_sub(1));
            for (i, line) in text.lines().enumerate().skip(from) {
                let trimmed = line.trim_start();
                let indent = line.len() - trimmed.len();
                let bare = trimmed.trim_start_matches(['{', ',', ' ']);
                if bare.starts_with(&format!("{key}:")) {
                    return Some((i + 1, indent + 1));
                }
                if let Some(at) = line.find(&format!("{key}:"))
                    && (line[..at].ends_with(['{', ' ', ',']))
                    && trimmed.contains('{')
                {
                    return Some((i + 1, at + 1));
                }
            }
        }
    }
    reported
}

/// Reads a config file from disk.
pub fn load(path: &Path) -> Result<ConfigFile> {
    let text = std::fs::read_to_string(path).map_err(|e| CoreError::Storage(format!("{}: {e}", path.display())))?;
    parse(&text, path)
}

/// `baselynx.yaml` next to the binary; for a binary under `target/<profile>`, also the
/// workspace root two levels up, so `cargo run` finds the file in the repository.
pub fn find_beside(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let here = dir.join(FILE_NAME);
    if here.is_file() {
        return Some(here);
    }
    let profile = dir.file_name()?.to_str()?;
    let target = dir.parent()?;
    if matches!(profile, "debug" | "release") && target.file_name()?.to_str()? == "target" {
        let root = target.parent()?.join(FILE_NAME);
        if root.is_file() {
            return Some(root);
        }
    }
    None
}

// ---------------------------------------------------------------- sync

fn same_path(a: &str, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(Path::new(a)) == canon(b)
}

/// The project a config file belongs to: the one opened from it before; else a project
/// without a file that already holds some of its Kubernetes databases (imported by hand
/// from the same clusters); else none, and the caller makes a new one.
pub fn find_project<'a>(projects: &'a [Project], file: &ConfigFile) -> Option<&'a Project> {
    if let Some(p) = projects
        .iter()
        .find(|p| p.config_file.as_deref().is_some_and(|f| same_path(f, &file.path)))
    {
        return Some(p);
    }
    projects
        .iter()
        .filter(|p| p.config_file.is_none())
        .map(|p| {
            let shared = file
                .databases
                .iter()
                .filter(|d| p.connections.iter().any(|c| legacy_match(c, &d.config)))
                .count();
            (shared, p)
        })
        .filter(|(n, _)| *n > 0)
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// A connection imported before config keys existed: same cluster, namespace and name.
fn legacy_match(existing: &ConnectionConfig, from_file: &ConnectionConfig) -> bool {
    existing.config_key.is_none()
        && match (&existing.kube, &from_file.kube) {
            (Some(a), Some(b)) => a.cluster == b.cluster && a.namespace == b.namespace && a.forward == b.forward,
            _ => false,
        }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// Connections with a password from the file, for the keyring.
    pub passwords: Vec<(Uuid, String)>,
}

/// Makes the project match the file.
pub fn sync(project: &mut Project, file: &ConfigFile) -> SyncReport {
    let mut report = SyncReport::default();
    project.name = file.project.clone();
    project.config_file = Some(file.path.to_string_lossy().into_owned());
    project.backups = Some(file.backups.clone());
    for db in &file.databases {
        let key = db.config.config_key.clone();
        let found = project
            .connections
            .iter()
            .position(|c| c.config_key == key)
            .or_else(|| project.connections.iter().position(|c| legacy_match(c, &db.config)));
        let id = match found {
            Some(ix) => {
                let existing = &mut project.connections[ix];
                let n = &db.config;
                existing.config_key = n.config_key.clone();
                existing.host = n.host.clone();
                existing.port = n.port;
                existing.database = n.database.clone();
                existing.user = n.user.clone();
                existing.ssl_mode = n.ssl_mode;
                existing.kube = n.kube.clone();
                existing.read_only = n.read_only;
                existing.backup = n.backup.clone();
                existing.allow_restore = n.allow_restore;
                if db.explicit_name {
                    existing.name = n.name.clone();
                }
                if db.explicit_color {
                    existing.color = n.color;
                }
                existing.save_password = n.save_password;
                report.updated += 1;
                existing.id
            }
            None => {
                report.added += 1;
                project.connections.push(db.config.clone());
                db.config.id
            }
        };
        if let Some(pw) = &db.password {
            report.passwords.push((id, pw.clone()));
        }
    }
    let listed: Vec<&str> = file
        .databases
        .iter()
        .filter_map(|d| d.config.config_key.as_deref())
        .collect();
    let before = project.connections.len();
    project
        .connections
        .retain(|c| c.config_key.as_deref().is_none_or(|k| listed.contains(&k)));
    report.removed = before - project.connections.len();
    let ids: Vec<Uuid> = project.connections.iter().map(|c| c.id).collect();
    project.tabs.retain(|t| ids.contains(&t.connection()));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
version: 1
project: Shop
backups:
  directory: dumps
  formats: [dump, sql]
  keep: {last: 3, daily: 7}
  timeout: 45m
clusters:
  shop-prod:
    kubeconfig: kube/prod.yaml
    namespaces:
      billing:
        orders-db:
          service: orders-db-pooler
          replica: orders-db-ro
          credentials: orders-db-credentials
          backup: {every: 12h, row_security: true, exclude_tables: [public.tmp]}
      partners:
        partner-db:
          service: partner-db-rw
          credentials: {secret: partner-app, keys: {database: dbname, user: username}}
          mode: read-only
          backup: true
  shop-dev:
    kubeconfig: C:/kube/dev.yaml
    context: dev-admin
    color: green
    namespaces:
      tools:
        scratch-db:
          name: Scratch
          pod: scratch-0
          credentials: {database: scratch, user: me, password: not-a-real-one}
          allow_restore: true
servers:
  local:
    host: 127.0.0.1
    port: 54329
    database: baselynx_ui
    user: postgres
    ssl: disable
    backup: {every: 1d, formats: [sql]}
    allow_restore: true
"#;

    fn sample() -> ConfigFile {
        parse(SAMPLE, Path::new("C:/cfg/baselynx.yaml")).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("24h"), Some(Duration::from_secs(86_400)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5_400)));
        assert_eq!(parse_duration("2d"), Some(Duration::from_secs(172_800)));
        assert_eq!(parse_duration("90"), None);
        assert_eq!(parse_duration("0h"), None);
        assert_eq!(parse_duration("1w"), None);
    }

    #[test]
    fn parses_clusters_and_servers() {
        let f = sample();
        assert_eq!(f.project, "Shop");
        assert!(Path::new(&f.backups.directory).ends_with("dumps"));
        assert_eq!(f.backups.formats, vec![BackupFormat::Dump, BackupFormat::Sql]);
        assert_eq!(f.backups.keep.last, 3);
        assert_eq!(f.backups.keep.weekly, 0);
        assert_eq!(f.backups.timeout_secs, 45 * 60);
        assert_eq!(f.databases.len(), 4);

        let orders = &f.databases[0].config;
        assert_eq!(orders.name, "shop-prod · orders-db");
        assert_eq!(orders.color, ConnectionColor::Red);
        assert!(!orders.read_only && !orders.allow_restore);
        let k = orders.kube.as_ref().unwrap();
        assert_eq!(
            (k.namespace.as_str(), k.service.as_str()),
            ("billing", "orders-db-pooler")
        );
        assert_eq!(k.replica.as_deref(), Some("orders-db-ro"));
        assert_eq!(k.remote_port, 5432);
        assert!(Path::new(&k.kubeconfig).starts_with("C:/cfg"));
        let plan = orders.backup.as_ref().unwrap();
        assert_eq!(plan.every_secs, 12 * 3600);
        assert_eq!(
            plan.formats,
            vec![BackupFormat::Dump, BackupFormat::Sql],
            "from the defaults"
        );
        assert!(plan.row_security);
        assert_eq!(plan.exclude_tables, vec!["public.tmp"]);

        let partner = &f.databases[1].config;
        assert!(partner.read_only);
        assert_eq!(partner.backup.as_ref().unwrap().every_secs, 86_400);
        let pk = partner.kube.as_ref().unwrap();
        assert_eq!(
            pk.credentials.fields.get("database").map(String::as_str),
            Some("dbname")
        );
        assert_eq!(
            pk.credentials.fields.get("username").map(String::as_str),
            Some("username")
        );

        let scratch = &f.databases[2];
        assert_eq!(scratch.config.name, "Scratch");
        assert!(scratch.explicit_name && scratch.explicit_color);
        assert_eq!(scratch.config.color, ConnectionColor::Green);
        assert_eq!(scratch.password.as_deref(), Some("not-a-real-one"));
        assert!(scratch.config.allow_restore);
        assert!(scratch.config.kube.as_ref().unwrap().pod);
        assert!(
            !serde_json::to_string(&scratch.config)
                .unwrap()
                .contains("not-a-real-one")
        );

        let local = &f.databases[3].config;
        assert_eq!((local.host.as_str(), local.port), ("127.0.0.1", 54329));
        assert_eq!(local.ssl_mode, SslMode::Disable);
        assert_eq!(local.config_key.as_deref(), Some("server:local"));
        assert_eq!(local.backup.as_ref().unwrap().formats, vec![BackupFormat::Sql]);
    }

    #[test]
    fn rejects_mistakes() {
        let bad = |text: &str| parse(text, Path::new("a.yaml")).unwrap_err().to_string();
        assert!(bad("version: 2\nproject: x\n").contains("version 2"));
        assert!(
            bad("version: 1\nproject: x\nclusters:\n  c:\n    kubeconfig: k\n    forwards: {}\n").contains("forwards")
        );
        assert!(bad("version: 1\nproject: x\nclusters:\n  c:\n    kubeconfig: k\n    namespaces:\n      ns:\n        d: {service: a, pod: b, credentials: s}\n").contains("exactly one"));
        assert!(bad("version: 1\nproject: x\nclusters:\n  c:\n    kubeconfig: k\n    namespaces:\n      ns:\n        d: {service: a}\n").contains("credentials"));
        assert!(
            bad("version: 1\nproject: x\nservers:\n  s: {host: h, database: d, user: u, backup: {every: soon}}\n")
                .contains("soon")
        );
        assert!(
            bad("version: 1\nproject: x\nservers:\n  s: {host: h, database: d, user: u, mode: write}\n")
                .contains("write")
        );
    }

    #[test]
    fn sync_adds_updates_and_removes() {
        let f = sample();
        let mut p = Project::new("old name");
        let manual = ConnectionConfig::new("made by hand");
        p.connections.push(manual.clone());
        let r = sync(&mut p, &f);
        assert_eq!((r.added, r.updated, r.removed), (4, 0, 0));
        assert_eq!(p.name, "Shop");
        assert_eq!(r.passwords.len(), 1);
        assert_eq!(p.connections.len(), 5);

        // Renamed and recoloured in the app; the file does not set name or colour for orders-db.
        let orders = p
            .connections
            .iter_mut()
            .find(|c| c.name == "shop-prod · orders-db")
            .unwrap();
        orders.name = "orders (prod)".into();
        orders.color = ConnectionColor::Amber;
        let orders_id = orders.id;

        let text = SAMPLE
            .replace("orders-db-pooler", "orders-db-rw")
            .replace("      partners:\n        partner-db:\n          service: partner-db-rw\n          credentials: {secret: partner-app, keys: {database: dbname, user: username}}\n          mode: read-only\n          backup: true\n", "");
        let f2 = parse(&text, Path::new("C:/cfg/baselynx.yaml")).unwrap();
        let r = sync(&mut p, &f2);
        assert_eq!((r.added, r.updated, r.removed), (0, 3, 1));
        let orders = p.connection(orders_id).unwrap();
        assert_eq!(orders.name, "orders (prod)");
        assert_eq!(orders.color, ConnectionColor::Amber);
        assert_eq!(orders.kube.as_ref().unwrap().service, "orders-db-rw");
        assert!(p.connection(manual.id).is_some(), "connections made by hand stay");
    }

    #[test]
    fn adopts_a_project_imported_before() {
        let f = sample();
        let mut old = Project::new("Default");
        let mut imported = f.databases[0].config.clone();
        imported.config_key = None;
        imported.id = Uuid::new_v4();
        let id = imported.id;
        old.connections.push(imported);
        let other = Project::new("Other");
        let projects = vec![other, old];
        let found = find_project(&projects, &f).unwrap();
        assert_eq!(found.name, "Default");
        let mut p = found.clone();
        let r = sync(&mut p, &f);
        assert_eq!((r.added, r.updated), (3, 1));
        assert_eq!(
            p.connections.iter().filter(|c| c.id == id).count(),
            1,
            "the old connection keeps its id"
        );
    }

    #[test]
    fn servers_from_connection_strings() {
        let text = "version: 1\nproject: p\nservers:\n  a: postgres://ann:s3cret@db.example:6432/shop?sslmode=require\n  b:\n    url: postgresql://bob@db2/stock\n    port: 5433\n    password: plain\n    color: green\n  c: {host: h, database: d, user: u}\n";
        let f = parse(text, Path::new("/x/baselynx.yaml")).unwrap();
        let a = &f.databases[0];
        assert_eq!((a.config.host.as_str(), a.config.port), ("db.example", 6432));
        assert_eq!((a.config.database.as_str(), a.config.user.as_str()), ("shop", "ann"));
        assert_eq!(a.config.ssl_mode, SslMode::Require);
        assert_eq!(a.password.as_deref(), Some("s3cret"));
        assert!(
            !a.config.save_password,
            "a password from the file is not put in the keyring"
        );
        let b = &f.databases[1];
        assert_eq!((b.config.host.as_str(), b.config.port), ("db2", 5433));
        assert_eq!(b.password.as_deref(), Some("plain"));
        assert_eq!(b.config.color, ConnectionColor::Green);
        let c = &f.databases[2];
        assert!(c.password.is_none() && c.config.save_password);
        assert!(!serde_json::to_string(&a.config).unwrap().contains("s3cret"));
        let bad = parse(
            "version: 1\nproject: p\nservers:\n  a: mysql://x\n",
            Path::new("a.yaml"),
        )
        .unwrap_err();
        assert!(bad.to_string().contains("connection string"), "{bad}");
        let bad = parse(
            "version: 1\nproject: p\nservers:\n  a: {host: h, user: u}\n",
            Path::new("a.yaml"),
        )
        .unwrap_err();
        assert!(bad.to_string().contains("no database"), "{bad}");
        // A misspelt key is named, with its line.
        let bad = parse(
            "version: 1\nproject: p\nservers:\n  a:\n    host: h\n    passwrd: x\n",
            Path::new("a.yaml"),
        )
        .unwrap_err()
        .to_string();
        assert!(bad.contains("passwrd") && error_position(&bad).is_some(), "{bad}");
        let text = "version: 1
project: p
servers:
  a:
    host: h
    passwrd: x
";
        assert_eq!(locate_error(&bad, text), Some((6, 5)), "the misspelt key's own line");
    }

    #[test]
    fn save_refuses_a_broken_file_and_keeps_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let good = "version: 1\nproject: p\n";
        save(&path, good).unwrap();
        assert!(save(&path, "version: 1\nproject: p\nclusters: [\n").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            good,
            "a broken text is never written"
        );
        save(&path, "version: 1\nproject: q\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("baselynx.yaml.bak")).unwrap(),
            good
        );
        let err = parse("version: 1\nproject: p\nclusters: [\n", &path)
            .unwrap_err()
            .to_string();
        assert!(error_position(&err).is_some(), "{err}");
    }

    #[test]
    fn the_example_file_parses() {
        let f = parse(
            include_str!("../../../baselynx.example.yaml"),
            Path::new("/x/baselynx.example.yaml"),
        )
        .unwrap();
        assert_eq!(f.databases.len(), 6);
        assert!(f.databases.iter().any(|d| d.config.allow_restore));
        assert!(
            f.databases
                .iter()
                .any(|d| d.config.config_key.as_deref() == Some("server:analytics"))
        );
    }

    #[test]
    fn finds_the_file_beside_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("target/release")).unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        assert_eq!(find_beside(&root.join("target/release/baselynx.exe")), None);
        std::fs::write(root.join(FILE_NAME), "x").unwrap();
        assert_eq!(
            find_beside(&root.join("target/release/baselynx.exe")),
            Some(root.join(FILE_NAME))
        );
        assert_eq!(find_beside(&root.join("bin/baselynx.exe")), None);
        std::fs::write(root.join("bin").join(FILE_NAME), "x").unwrap();
        assert_eq!(
            find_beside(&root.join("bin/baselynx.exe")),
            Some(root.join("bin").join(FILE_NAME))
        );
    }
}
