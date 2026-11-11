//! Backups with pg_dump and restores with pg_restore or psql.
//!
//! A backup is a directory `<backups>/<connection>/<UTC time>/` holding the files and a
//! `manifest.json` with their SHA-256 sums. It is written as `<time>.partial` and renamed
//! when complete, so a half-written backup is never mistaken for a good one.
//!
//! Formats:
//! - `dump`: `<database>.dump`, pg_dump's compressed custom format;
//! - `sql`: `schema.sql` (tables, types, functions), `data.sql` (rows) and `indexes.sql`
//!   (indexes, keys, constraints, triggers), in the order a restore runs them.
//!
//! A restore is for disasters only. It runs only when every check in [`preflight`] passes:
//! the config file allows restores into the target, the target is read-write, the backup is
//! complete and unchanged, and the target database is empty. The caller must also pass the
//! target database's name, typed by the user. Everything runs in one transaction, so a
//! failed restore leaves the database empty.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::Command;

use crate::config::{BackupFormat, BackupPlan, BackupSettings, BackupSource, Keep};
use crate::error::{CoreError, Result};
use crate::k8s::{self, KubeTarget};
use crate::model::{ConnectionConfig, SslMode};
use crate::runtime;

const MANIFEST: &str = "manifest.json";
const PARTIAL: &str = ".partial";
const FAILURE: &str = "last-failure.json";
const STAMP: &str = "%Y-%m-%dT%H-%M-%SZ";
const SQL_FILES: [(&str, &str); 3] = [
    ("schema.sql", "pre-data"),
    ("data.sql", "data"),
    ("indexes.sql", "post-data"),
];

// ---------------------------------------------------------------- manifest

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub baselynx_version: String,
    /// The connection's name when the backup was taken.
    pub connection: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_key: Option<String>,
    /// Where the rows came from: `shop-prod/billing/orders-db-ro` or `host:port`.
    pub source: String,
    pub database: String,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub pg_dump_version: String,
    #[serde(default)]
    pub server_version: String,
    pub formats: Vec<BackupFormat>,
    pub files: Vec<ManifestFile>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub row_security: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_tables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub format: BackupFormat,
    pub bytes: u64,
    pub sha256: String,
}

/// A complete backup on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRun {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

// ---------------------------------------------------------------- tools

/// pg_dump, pg_restore or psql: from the configured directory, PATH, or on Windows the
/// newest `C:\Program Files\PostgreSQL\<version>\bin`.
pub fn pg_tool(name: &str, tools: Option<&str>) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if let Some(dir) = tools {
        return Path::new(dir).join(&exe);
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(&exe);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    if cfg!(windows)
        && let Some(pf) = std::env::var_os("ProgramFiles")
        && let Ok(entries) = std::fs::read_dir(Path::new(&pf).join("PostgreSQL"))
    {
        let best = entries
            .flatten()
            .filter_map(|e| {
                let v: u32 = e.file_name().to_str()?.parse().ok()?;
                let p = e.path().join("bin").join(&exe);
                p.is_file().then_some((v, p))
            })
            .max_by_key(|(v, _)| *v);
        if let Some((_, p)) = best {
            return p;
        }
    }
    PathBuf::from(exe)
}

/// A PostgreSQL tool that never asks anything and, on Windows, opens no console window.
fn tool_command(tool: &Path) -> Command {
    let mut cmd = Command::new(tool);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

async fn tool_version(tool: &Path) -> Result<String> {
    let out = tool_command(tool)
        .arg("--version")
        .output()
        .await
        .map_err(|e| CoreError::Invalid(format!("{} cannot run: {e}", tool.display())))?;
    if !out.status.success() {
        return Err(CoreError::Invalid(format!("{} --version failed", tool.display())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------------------------------------------------------------- endpoints

/// A host and port pg_dump or psql can reach, with credentials. For a Kubernetes database
/// it is a listener on 127.0.0.1 that opens a port-forward stream for every connection;
/// the listener stops when this is dropped.
struct Endpoint {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: Option<String>,
    ssl_mode: SslMode,
    /// For the manifest and messages.
    source: String,
    _proxy: Option<tokio::task::AbortHandle>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Some(h) = &self._proxy {
            h.abort();
        }
    }
}

impl Endpoint {
    fn command(&self, tool: &Path) -> Command {
        let mut cmd = tool_command(tool);
        // -w: fail instead of prompting for a password nobody can type.
        cmd.arg("-w")
            .arg("-h")
            .arg(&self.host)
            .arg("-p")
            .arg(self.port.to_string())
            .arg("-U")
            .arg(&self.user)
            .env("PGAPPNAME", "baselynx-backup")
            .env("PGCONNECT_TIMEOUT", "15")
            .env(
                "PGSSLMODE",
                match self.ssl_mode {
                    SslMode::Disable => "disable",
                    SslMode::Prefer => "prefer",
                    SslMode::Require => "require",
                    SslMode::VerifyFull => "verify-full",
                },
            )
            .env_remove("PGPASSWORD")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = &self.password {
            cmd.env("PGPASSWORD", p);
        }
        cmd
    }
}

/// The primary, or the replica when `replica` is set and the database has one.
async fn endpoint(config: &ConnectionConfig, password: Option<String>, replica: bool) -> Result<Endpoint> {
    let Some(mut target) = config.kube.clone() else {
        return Ok(Endpoint {
            host: config.host.clone(),
            port: config.port,
            database: config.database.clone(),
            user: config.user.clone(),
            password,
            ssl_mode: config.ssl_mode,
            source: format!("{}:{}", config.host, config.port),
            _proxy: None,
        });
    };
    if replica && let Some(ro) = target.replica.clone() {
        target.service = ro;
    }
    let creds = k8s::credentials(&target, config).await?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CoreError::Connect(format!("local listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CoreError::Connect(format!("local listener: {e}")))?
        .port();
    let source = format!("{}/{}/{}", target.cluster, target.namespace, target.service);
    let proxy = tokio::spawn(proxy(listener, target)).abort_handle();
    Ok(Endpoint {
        host: "127.0.0.1".into(),
        port,
        database: creds.database,
        user: creds.username,
        password: creds.password.or(password),
        ssl_mode: config.ssl_mode,
        source,
        _proxy: Some(proxy),
    })
}

async fn proxy(listener: TcpListener, target: KubeTarget) {
    while let Ok((mut socket, _)) = listener.accept().await {
        let target = target.clone();
        tokio::spawn(async move {
            if let Ok(mut stream) = k8s::open_stream(&target).await {
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            }
        });
    }
}

/// Runs a tool; the error carries its stderr (never the password, which is in the environment).
async fn run(mut cmd: Command, what: &str, timeout: Duration) -> Result<String> {
    let child = cmd
        .spawn()
        .map_err(|e| CoreError::Invalid(format!("{what} cannot run: {e}")))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| CoreError::Invalid(format!("{what} took longer than {} minutes", timeout.as_secs() / 60)))?
        .map_err(|e| CoreError::Invalid(format!("{what}: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        // The last lines say what went wrong.
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(CoreError::Invalid(format!("{what} failed: {}", tail.join("\n"))));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------- layout

/// The directory of a connection's backups: `<cluster>/<namespace>/<name>`,
/// `servers/<name>`, or `connections/<name>` for one made by hand.
pub fn connection_dir(settings: &BackupSettings, config: &ConnectionConfig) -> PathBuf {
    let clean = |s: &str| -> String {
        let s: String = s
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        s.trim_matches('.').to_string()
    };
    let mut dir = PathBuf::from(&settings.directory);
    match (&config.config_key, &config.kube) {
        (Some(key), _) if key.starts_with("server:") => {
            dir.push("servers");
            dir.push(clean(&key["server:".len()..]));
        }
        (_, Some(k)) => {
            dir.push(clean(&k.cluster));
            dir.push(clean(&k.namespace));
            dir.push(clean(&k.forward));
        }
        _ => {
            dir.push("connections");
            dir.push(clean(&config.name));
        }
    }
    dir
}

/// Complete backups of a connection, newest first.
pub fn list_runs(dir: &Path) -> Vec<BackupRun> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<BackupRun> = entries
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().ends_with(PARTIAL))
        .filter_map(|e| {
            read_manifest(&e.path()).ok().map(|m| BackupRun {
                dir: e.path(),
                manifest: m,
            })
        })
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.manifest.finished));
    runs
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path).map_err(|e| CoreError::Invalid(format!("{}: {e}", path.display())))?;
    serde_json::from_str(&text).map_err(|e| CoreError::Invalid(format!("{}: {e}", path.display())))
}

/// When the last complete backup finished.
pub fn last_backup(settings: &BackupSettings, config: &ConnectionConfig) -> Option<DateTime<Utc>> {
    list_runs(&connection_dir(settings, config))
        .first()
        .map(|r| r.manifest.finished)
}

/// Due when there is no backup yet, or the last is at least `every` old.
pub fn is_due(plan: &BackupPlan, last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last {
        None => true,
        Some(t) => now.signed_duration_since(t).num_seconds() >= plan.every_secs as i64,
    }
}

/// The day, week or month a backup falls in.
type PeriodKey = fn(&DateTime<Utc>) -> String;

/// Which backups to keep (same order as `times`): the newest always; the newest `last`; and
/// the newest in each of the last `daily` days, `weekly` ISO weeks and `monthly` months.
pub fn select_keep(times: &[DateTime<Utc>], keep: &Keep) -> Vec<bool> {
    let mut order: Vec<usize> = (0..times.len()).collect();
    order.sort_by(|&a, &b| times[b].cmp(&times[a]));
    let mut out = vec![false; times.len()];
    if let Some(&newest) = order.first() {
        out[newest] = true;
    }
    for &ix in order.iter().take(keep.last) {
        out[ix] = true;
    }
    let buckets: [(usize, PeriodKey); 3] = [
        (keep.daily, |t| t.format("%Y-%m-%d").to_string()),
        (keep.weekly, |t| {
            let w = t.iso_week();
            format!("{}-W{:02}", w.year(), w.week())
        }),
        (keep.monthly, |t| t.format("%Y-%m").to_string()),
    ];
    for (limit, key) in buckets {
        let mut seen = std::collections::HashSet::new();
        for &ix in &order {
            if seen.len() >= limit {
                break;
            }
            if seen.insert(key(&times[ix])) {
                out[ix] = true;
            }
        }
    }
    out
}

/// Removes complete backups the policy does not keep, and partial ones left by a crash.
/// Returns how many were removed.
pub fn prune(dir: &Path, keep: &Keep) -> usize {
    let runs = list_runs(dir);
    let times: Vec<_> = runs.iter().map(|r| r.manifest.finished).collect();
    let keep_flags = select_keep(&times, keep);
    let mut removed = 0;
    for (run, k) in runs.iter().zip(keep_flags) {
        if !k && std::fs::remove_dir_all(&run.dir).is_ok() {
            removed += 1;
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(PARTIAL) && std::fs::remove_dir_all(e.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| CoreError::Invalid(format!("{}: {e}", path.display())))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| CoreError::Invalid(format!("{}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        h.update(&buf[..n]);
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((hex, total))
}

/// Checks that every file of a backup is present and unchanged.
pub fn verify_files(dir: &Path) -> Result<Manifest> {
    let m = read_manifest(dir)?;
    if m.files.is_empty() {
        return Err(CoreError::Invalid("the manifest lists no files".into()));
    }
    for f in &m.files {
        if f.name.contains(['/', '\\']) || f.name.contains("..") {
            return Err(CoreError::Invalid(format!(
                "the manifest names an odd file: {}",
                f.name
            )));
        }
        let (sum, bytes) = sha256_file(&dir.join(&f.name))?;
        if sum != f.sha256 || bytes != f.bytes {
            return Err(CoreError::Invalid(format!(
                "{} has changed since the backup was taken",
                f.name
            )));
        }
    }
    Ok(m)
}

/// Objects in a custom-format dump, read with `pg_restore --list`.
async fn dump_entries(file: &Path, tools: Option<&str>) -> Result<usize> {
    let mut cmd = tool_command(&pg_tool("pg_restore", tools));
    cmd.arg("--list")
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = run(cmd, "pg_restore --list", Duration::from_secs(300)).await?;
    let n = out
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with(';'))
        .count();
    if n == 0 {
        return Err(CoreError::Invalid("the dump lists no objects".into()));
    }
    Ok(n)
}

// ---------------------------------------------------------------- backup

pub struct BackupJob {
    pub config: ConnectionConfig,
    /// For a connection that is not in Kubernetes (from the keyring).
    pub password: Option<String>,
    pub settings: BackupSettings,
    pub plan: BackupPlan,
}

/// Takes a backup, verifies it, then applies the retention policy. Runs on the core runtime.
pub fn run_backup(job: BackupJob) -> impl Future<Output = Result<BackupRun>> + Send + 'static {
    runtime::spawn(async move {
        let dir = connection_dir(&job.settings, &job.config);
        let result = backup_inner(job).await;
        // The last failure stays on disk until a backup succeeds, so it survives a restart.
        match &result {
            Ok(_) => {
                let _ = std::fs::remove_file(dir.join(FAILURE));
            }
            Err(e) => {
                let _ = std::fs::create_dir_all(&dir);
                let failure = Failure {
                    at: Utc::now(),
                    error: e.to_string(),
                };
                let _ = std::fs::write(
                    dir.join(FAILURE),
                    serde_json::to_string_pretty(&failure).expect("serializes"),
                );
            }
        }
        result
    })
}

/// The last failed backup of a connection, kept until one succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub at: DateTime<Utc>,
    pub error: String,
}

/// What is on disk for one connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub dir: PathBuf,
    /// Complete backups, newest first.
    pub runs: Vec<BackupRun>,
    pub failure: Option<Failure>,
}

impl Status {
    pub fn last(&self) -> Option<&BackupRun> {
        self.runs.first()
    }

    pub fn total_bytes(&self) -> u64 {
        self.runs
            .iter()
            .flat_map(|r| r.manifest.files.iter())
            .map(|f| f.bytes)
            .sum()
    }

    /// When the schedule next takes one: now when there is none yet.
    pub fn next_due(&self, plan: &BackupPlan) -> Option<DateTime<Utc>> {
        let last = self.last()?.manifest.finished;
        Some(last + chrono::Duration::seconds(plan.every_secs as i64))
    }
}

/// Reads a connection's backups and last failure. Blocking: call it off the UI thread.
pub fn status(settings: &BackupSettings, config: &ConnectionConfig) -> Status {
    let dir = connection_dir(settings, config);
    let failure = std::fs::read_to_string(dir.join(FAILURE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    Status {
        runs: list_runs(&dir),
        failure,
        dir,
    }
}

async fn backup_inner(job: BackupJob) -> Result<BackupRun> {
    let tools = job.settings.tools.clone();
    let dump = pg_tool("pg_dump", tools.as_deref());
    let pg_dump_version = tool_version(&dump).await?;
    let dir = connection_dir(&job.settings, &job.config);
    std::fs::create_dir_all(&dir).map_err(|e| CoreError::Storage(format!("{}: {e}", dir.display())))?;
    let started = Utc::now();
    let stamp = started.format(STAMP).to_string();
    let partial = dir.join(format!("{stamp}{PARTIAL}"));
    let final_dir = dir.join(&stamp);

    let has_replica = job.config.kube.as_ref().is_some_and(|k| k.replica.is_some());
    let sources: &[bool] = if job.plan.from == BackupSource::Replica && has_replica {
        &[true, false]
    } else {
        &[false]
    };
    let mut last_err = None;
    for &replica in sources {
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial).map_err(|e| CoreError::Storage(format!("{}: {e}", partial.display())))?;
        match dump_into(&job, &dump, &partial, replica).await {
            Ok((files, source, database, server_version)) => {
                let manifest = Manifest {
                    format_version: 1,
                    baselynx_version: env!("CARGO_PKG_VERSION").to_string(),
                    connection: job.config.name.clone(),
                    config_key: job.config.config_key.clone(),
                    source,
                    database,
                    started,
                    finished: Utc::now(),
                    pg_dump_version: pg_dump_version.clone(),
                    server_version,
                    formats: job.plan.formats.clone(),
                    files,
                    row_security: job.plan.row_security,
                    excluded_tables: job.plan.exclude_tables.clone(),
                };
                let json = serde_json::to_string_pretty(&manifest).expect("manifest serializes");
                std::fs::write(partial.join(MANIFEST), json)
                    .map_err(|e| CoreError::Storage(format!("{}: {e}", partial.display())))?;
                std::fs::rename(&partial, &final_dir)
                    .map_err(|e| CoreError::Storage(format!("{}: {e}", final_dir.display())))?;
                prune(&dir, job.plan.keep.as_ref().unwrap_or(&job.settings.keep));
                return Ok(BackupRun {
                    dir: final_dir,
                    manifest,
                });
            }
            Err(e) => last_err = Some(e),
        }
    }
    let _ = std::fs::remove_dir_all(&partial);
    Err(last_err.unwrap_or_else(|| CoreError::Invalid("no source to back up from".into())))
}

async fn dump_into(
    job: &BackupJob,
    dump: &Path,
    dir: &Path,
    replica: bool,
) -> Result<(Vec<ManifestFile>, String, String, String)> {
    let ep = endpoint(&job.config, job.password.clone(), replica).await?;
    let timeout = job.settings.timeout();
    let common = |cmd: &mut Command| {
        cmd.arg("-d").arg(&ep.database).arg("--lock-wait-timeout=10s");
        if job.plan.row_security {
            cmd.arg("--enable-row-security");
        }
        for t in &job.plan.exclude_tables {
            cmd.arg(format!("--exclude-table={t}"));
        }
    };
    let mut files = Vec::new();
    let mut formats = job.plan.formats.clone();
    formats.sort();
    formats.dedup();
    for format in formats {
        match format {
            BackupFormat::Dump => {
                let name = format!("{}.dump", file_stem(&ep.database));
                let mut cmd = ep.command(dump);
                common(&mut cmd);
                cmd.arg("-Fc").arg("-f").arg(dir.join(&name));
                run(cmd, &format!("pg_dump from {}", ep.source), timeout).await?;
                dump_entries(&dir.join(&name), job.settings.tools.as_deref()).await?;
                let (sha256, bytes) = sha256_file(&dir.join(&name))?;
                files.push(ManifestFile {
                    name,
                    format,
                    bytes,
                    sha256,
                });
            }
            BackupFormat::Sql => {
                for (name, section) in SQL_FILES {
                    let mut cmd = ep.command(dump);
                    common(&mut cmd);
                    // Owners name roles another server may not have.
                    cmd.arg("-Fp")
                        .arg("--no-owner")
                        .arg(format!("--section={section}"))
                        .arg("-f")
                        .arg(dir.join(name));
                    run(cmd, &format!("pg_dump {section} from {}", ep.source), timeout).await?;
                    let (sha256, bytes) = sha256_file(&dir.join(name))?;
                    files.push(ManifestFile {
                        name: name.to_string(),
                        format,
                        bytes,
                        sha256,
                    });
                }
            }
        }
    }
    let server_version = server_version_of(dir, &files).await;
    Ok((files, ep.source.clone(), ep.database.clone(), server_version))
}

/// "Dumped from database version" from the header of a plain file, when there is one.
async fn server_version_of(dir: &Path, files: &[ManifestFile]) -> String {
    let Some(f) = files.iter().find(|f| f.format == BackupFormat::Sql) else {
        return String::new();
    };
    let Ok(mut file) = tokio::fs::File::open(dir.join(&f.name)).await else {
        return String::new();
    };
    let mut head = vec![0u8; 4096];
    let n = file.read(&mut head).await.unwrap_or(0);
    String::from_utf8_lossy(&head[..n])
        .lines()
        .find_map(|l| l.strip_prefix("-- Dumped from database version "))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn file_stem(database: &str) -> String {
    let s: String = database
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "database".into() } else { s }
}

// ---------------------------------------------------------------- restore

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    pub checks: Vec<Check>,
    pub manifest: Option<Manifest>,
    /// The target database's name: the user types it to confirm.
    pub database: Option<String>,
    /// The backup was taken from this same connection.
    pub same_connection: bool,
}

impl Preflight {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

const NOT_EMPTY_SQL: &str = r"
SELECT kind || ' ' || name FROM (
  SELECT 'relation' AS kind, n.nspname || '.' || c.relname AS name
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE c.relkind IN ('r','p','v','m','S','f')
     AND n.nspname NOT IN ('pg_catalog','information_schema')
     AND n.nspname NOT LIKE 'pg\_toast%' AND n.nspname NOT LIKE 'pg\_temp%'
  UNION ALL
  SELECT 'schema', nspname FROM pg_namespace
   WHERE nspname NOT IN ('public','information_schema')
     AND nspname NOT LIKE 'pg\_%'
  UNION ALL
  SELECT 'function', n.nspname || '.' || p.proname
    FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
   WHERE n.nspname NOT IN ('pg_catalog','information_schema')
     AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e')
) found ORDER BY 1 LIMIT 4";

/// Everything that must hold before a restore. Connects to the target only to look.
pub fn preflight(
    target: ConnectionConfig,
    password: Option<String>,
    run_dir: PathBuf,
    tools: Option<String>,
) -> impl Future<Output = Result<Preflight>> + Send + 'static {
    runtime::spawn(async move { Ok(preflight_inner(&target, password, &run_dir, tools.as_deref()).await) })
}

async fn preflight_inner(
    target: &ConnectionConfig,
    password: Option<String>,
    run_dir: &Path,
    tools: Option<&str>,
) -> Preflight {
    let mut checks = Vec::new();
    let mut push = |label: &str, ok: bool, detail: String| {
        checks.push(Check {
            label: label.to_string(),
            ok,
            detail,
        })
    };
    push(
        "The config file allows restores into this connection",
        target.allow_restore,
        if target.allow_restore {
            "allow_restore: true".into()
        } else if target.config_key.is_some() {
            "add allow_restore: true to this database in the config file".into()
        } else {
            "only connections from a config file with allow_restore: true".into()
        },
    );
    push(
        "The connection is read-write",
        !target.read_only,
        if target.read_only {
            "it is read-only".into()
        } else {
            String::new()
        },
    );

    let manifest = verify_files(run_dir);
    let mut dump_ok = true;
    if let Ok(m) = &manifest
        && let Some(f) = m.files.iter().find(|f| f.format == BackupFormat::Dump)
        && let Err(e) = dump_entries(&run_dir.join(&f.name), tools).await
    {
        dump_ok = false;
        push("The backup is complete and unchanged", false, e.to_string());
    }
    if dump_ok {
        match &manifest {
            Ok(m) => push(
                "The backup is complete and unchanged",
                true,
                format!("{} files, checksums match", m.files.len()),
            ),
            Err(e) => push("The backup is complete and unchanged", false, e.to_string()),
        }
    }
    let manifest = manifest.ok();

    // The tool of every format the backup has: the dialog lets the user pick either.
    let formats = manifest.as_ref().map(|m| m.formats.clone()).unwrap_or_default();
    for needed in [(BackupFormat::Dump, "pg_restore"), (BackupFormat::Sql, "psql")]
        .into_iter()
        .filter(|(f, _)| formats.contains(f))
        .map(|(_, tool)| tool)
    {
        match tool_version(&pg_tool(needed, tools)).await {
            Ok(v) => push(&format!("{needed} is available"), true, v),
            Err(e) => push(&format!("{needed} is available"), false, e.to_string()),
        }
    }

    let mut database = None;
    let mut look = target.clone();
    look.read_only = false;
    match crate::session::connect(&look, password).await {
        Ok(s) => {
            let client = s.client();
            database = client
                .query_one("SELECT current_database()::text", &[])
                .await
                .ok()
                .map(|r| r.get::<_, String>(0));
            match client.query(NOT_EMPTY_SQL, &[]).await {
                Ok(rows) if rows.is_empty() => push("The target database is empty", true, String::new()),
                Ok(rows) => {
                    let mut found: Vec<String> = rows.iter().take(3).map(|r| r.get::<_, String>(0)).collect();
                    if rows.len() > 3 {
                        found.push("more".into());
                    }
                    push(
                        "The target database is empty",
                        false,
                        format!("it holds {}", found.join(", ")),
                    )
                }
                Err(e) => push("The target database is empty", false, e.to_string()),
            }
        }
        Err(e) => push("The target database is empty", false, format!("could not connect: {e}")),
    }
    let same_connection = manifest
        .as_ref()
        .is_some_and(|m| m.config_key.is_some() && m.config_key == target.config_key);
    Preflight {
        checks,
        manifest,
        database,
        same_connection,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub database: String,
    pub format: BackupFormat,
    pub duration: Duration,
}

/// Restores a backup into an empty database. Runs every check again and refuses unless all
/// pass and `typed` is the target database's name.
pub fn restore(
    target: ConnectionConfig,
    password: Option<String>,
    run_dir: PathBuf,
    format: BackupFormat,
    tools: Option<String>,
    typed: String,
) -> impl Future<Output = Result<RestoreReport>> + Send + 'static {
    runtime::spawn(async move {
        let started = std::time::Instant::now();
        let pre = preflight_inner(&target, password.clone(), &run_dir, tools.as_deref()).await;
        if let Some(failed) = pre.checks.iter().find(|c| !c.ok) {
            return Err(CoreError::Invalid(format!(
                "refused: {} — {}",
                failed.label.to_lowercase(),
                failed.detail
            )));
        }
        let database = pre.database.clone().unwrap_or_default();
        if database.is_empty() || typed.trim() != database {
            return Err(CoreError::Invalid(format!(
                "refused: type the target database's name, {database}, to confirm"
            )));
        }
        let manifest = pre.manifest.expect("checked");
        if !manifest.formats.contains(&format) {
            return Err(CoreError::Invalid(format!(
                "this backup has no {} files",
                format.label()
            )));
        }
        let mut primary = target.clone();
        primary.read_only = false;
        // Never the replica: the restore writes.
        let ep = endpoint(&primary, password, false).await?;
        if ep.database != database {
            return Err(CoreError::Invalid(format!(
                "refused: the connection now points at {}, not {database}",
                ep.database
            )));
        }
        let long = Duration::from_secs(6 * 3600);
        match format {
            BackupFormat::Dump => {
                let file = manifest
                    .files
                    .iter()
                    .find(|f| f.format == BackupFormat::Dump)
                    .expect("formats lists dump");
                let mut cmd = ep.command(&pg_tool("pg_restore", tools.as_deref()));
                cmd.arg("-d")
                    .arg(&database)
                    .arg("--single-transaction")
                    .arg("--exit-on-error")
                    .arg("--no-owner")
                    .arg(run_dir.join(&file.name));
                run(cmd, "pg_restore", long).await?;
            }
            BackupFormat::Sql => {
                let mut cmd = ep.command(&pg_tool("psql", tools.as_deref()));
                cmd.arg("-X")
                    .arg("-q")
                    .arg("-d")
                    .arg(&database)
                    .arg("-v")
                    .arg("ON_ERROR_STOP=1")
                    .arg("--single-transaction");
                for (name, _) in SQL_FILES {
                    cmd.arg("-f").arg(run_dir.join(name));
                }
                run(cmd, "psql", long).await?;
            }
        }
        Ok(RestoreReport {
            database,
            format,
            duration: started.elapsed(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, d, h, 0, 0).unwrap()
    }

    #[test]
    fn due_after_the_interval() {
        let plan = BackupPlan {
            every_secs: 24 * 3600,
            ..BackupPlan::manual(vec![BackupFormat::Dump])
        };
        assert!(is_due(&plan, None, at(10, 12)));
        assert!(!is_due(&plan, Some(at(10, 0)), at(10, 23)));
        assert!(is_due(&plan, Some(at(10, 0)), at(11, 0)));
    }

    #[test]
    fn keeps_the_newest_per_day_week_and_month() {
        // Two a day for ten days, newest last.
        let times: Vec<_> = (1..=10).flat_map(|d| [at(d, 1), at(d, 13)]).collect();
        let keep = Keep {
            last: 2,
            daily: 3,
            weekly: 0,
            monthly: 0,
        };
        let flags = select_keep(&times, &keep);
        let kept: Vec<_> = times.iter().zip(&flags).filter(|(_, k)| **k).map(|(t, _)| *t).collect();
        assert_eq!(kept, vec![at(8, 13), at(9, 13), at(10, 1), at(10, 13)]);
        let none = Keep {
            last: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
        };
        assert_eq!(
            select_keep(&times, &none).iter().filter(|k| **k).count(),
            1,
            "the newest stays"
        );
    }

    #[test]
    fn a_failure_is_kept_until_a_backup_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = BackupSettings::default_in(dir.path());
        settings.tools = Some(dir.path().join("no-such-bin").to_string_lossy().into_owned());
        let mut c = ConnectionConfig::new("x");
        c.config_key = Some("server:x".into());
        let job = BackupJob {
            config: c.clone(),
            password: None,
            settings: settings.clone(),
            plan: BackupPlan::manual(vec![BackupFormat::Dump]),
        };
        assert!(runtime::runtime().block_on(run_backup(job)).is_err());
        let st = status(&settings, &c);
        assert!(
            st.failure.as_ref().is_some_and(|f| f.error.contains("pg_dump")),
            "{st:?}"
        );
        assert!(st.runs.is_empty());
    }

    #[test]
    fn directories_per_connection() {
        let settings = BackupSettings::default_in(Path::new("/data"));
        let mut c = ConnectionConfig::new("my db/1");
        assert!(connection_dir(&settings, &c).ends_with("backups/connections/my_db_1"));
        c.config_key = Some("server:local".into());
        assert!(connection_dir(&settings, &c).ends_with("backups/servers/local"));
    }
}
