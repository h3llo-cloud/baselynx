//! Project files and saved passwords.
//!
//! Layout under the configuration directory:
//! `state.json` holds the last project; `projects/<uuid>.json` holds one project each.
//! Passwords never go into these files: they live in the OS keyring.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::APP_NAME;
use crate::error::{CoreError, Result};
use crate::model::Project;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub last_project: Option<Uuid>,
    /// `light`, `dark`, or absent to follow the system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Interface zoom; absent means 100 %.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<f32>,
}

/// A project file that could not be read. It is reported and left untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableProject {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The per-user configuration directory, e.g. `%APPDATA%\baselynx\config` on Windows.
    /// `BASELYNX_CONFIG_DIR` overrides the location (used for testing).
    pub fn open_default() -> Result<Self> {
        if let Some(dir) = std::env::var_os("BASELYNX_CONFIG_DIR") {
            return Self::at(dir);
        }
        let dirs = directories::ProjectDirs::from("", "", APP_NAME)
            .ok_or_else(|| CoreError::Storage("no home directory found".into()))?;
        Self::at(dirs.config_dir())
    }

    pub fn at(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("projects"))
            .map_err(|e| CoreError::Storage(format!("cannot create {}: {e}", root.display())))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn project_path(&self, id: Uuid) -> PathBuf {
        self.root.join("projects").join(format!("{id}.json"))
    }

    pub fn load_state(&self) -> AppState {
        fs::read(self.root.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, state: &AppState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| CoreError::Storage(e.to_string()))?;
        write_atomic(&self.root.join("state.json"), &bytes)
    }

    /// All readable projects sorted by name, plus the files that could not be read.
    pub fn list_projects(&self) -> (Vec<Project>, Vec<UnreadableProject>) {
        let mut projects = Vec::new();
        let mut unreadable = Vec::new();
        let Ok(entries) = fs::read_dir(self.root.join("projects")) else {
            return (projects, unreadable);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match read_project(&path) {
                Ok(p) => projects.push(p),
                Err(error) => unreadable.push(UnreadableProject { path, error }),
            }
        }
        projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        (projects, unreadable)
    }

    pub fn load_project(&self, id: Uuid) -> Result<Project> {
        read_project(&self.project_path(id)).map_err(CoreError::Storage)
    }

    pub fn save_project(&self, project: &Project) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(project).map_err(|e| CoreError::Storage(e.to_string()))?;
        write_atomic(&self.project_path(project.id), &bytes)
    }

    /// Deletes the project file and the saved passwords of its connections.
    pub fn delete_project(&self, project: &Project, secrets: &dyn SecretStore) -> Result<()> {
        for c in &project.connections {
            secrets.delete(c.id)?;
        }
        match fs::remove_file(self.project_path(project.id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::Storage(e.to_string())),
        }
    }

    /// Loads the last used project, or the first one, or creates `Default` on first start.
    pub fn open_startup_project(&self) -> Result<(Project, Vec<UnreadableProject>)> {
        let (projects, unreadable) = self.list_projects();
        let state = self.load_state();
        if let Some(p) = state
            .last_project
            .and_then(|id| projects.iter().find(|p| p.id == id))
            .or_else(|| projects.first())
        {
            return Ok((p.clone(), unreadable));
        }
        let project = Project::new("Default");
        self.save_project(&project)?;
        self.save_state(&AppState {
            last_project: Some(project.id),
            ..Default::default()
        })?;
        Ok((project, unreadable))
    }
}

fn read_project(path: &Path) -> std::result::Result<Project, String> {
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

/// Writes through a temporary file in the same directory, then renames over the target.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| CoreError::Storage(format!("no parent for {}", path.display())))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        Uuid::new_v4().simple()
    ));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(CoreError::Storage(format!("cannot write {}: {e}", path.display())));
    }
    Ok(())
}

/// Where saved passwords live. The app uses [`KeyringStore`]; tests use [`MemorySecretStore`].
pub trait SecretStore: Send + Sync {
    fn get(&self, connection: Uuid) -> Result<Option<String>>;
    fn set(&self, connection: Uuid, password: &str) -> Result<()>;
    fn delete(&self, connection: Uuid) -> Result<()>;
}

/// The OS credential store: Windows Credential Manager, macOS Keychain, Linux Secret Service.
/// Service `baselynx`, account `<connection id>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(connection: Uuid) -> Result<keyring::v1::Entry> {
        keyring::v1::Entry::new(APP_NAME, &connection.to_string()).map_err(|e| CoreError::Keyring(e.to_string()))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, connection: Uuid) -> Result<Option<String>> {
        match Self::entry(connection)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::v1::Error::NoEntry) => Ok(None),
            Err(e) => Err(CoreError::Keyring(e.to_string())),
        }
    }

    fn set(&self, connection: Uuid, password: &str) -> Result<()> {
        Self::entry(connection)?
            .set_password(password)
            .map_err(|e| CoreError::Keyring(e.to_string()))
    }

    fn delete(&self, connection: Uuid) -> Result<()> {
        match Self::entry(connection)?.delete_credential() {
            Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(e) => Err(CoreError::Keyring(e.to_string())),
        }
    }
}

#[derive(Debug, Default)]
pub struct MemorySecretStore {
    inner: Mutex<HashMap<Uuid, String>>,
}

impl SecretStore for MemorySecretStore {
    fn get(&self, connection: Uuid) -> Result<Option<String>> {
        Ok(self.inner.lock().unwrap().get(&connection).cloned())
    }
    fn set(&self, connection: Uuid, password: &str) -> Result<()> {
        self.inner.lock().unwrap().insert(connection, password.to_string());
        Ok(())
    }
    fn delete(&self, connection: Uuid) -> Result<()> {
        self.inner.lock().unwrap().remove(&connection);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConnectionConfig, SortDir, TabState};

    fn sample_project() -> Project {
        let mut p = Project::new("Billing");
        let c = ConnectionConfig::new("prod");
        let tab = TabState::Table {
            id: Uuid::new_v4(),
            connection: c.id,
            schema: "public".into(),
            table: "customers".into(),
            filter: Some("id > 10".into()),
            sort: Some(("name".into(), SortDir::Asc)),
            page_size: 500,
            refresh_secs: None,
        };
        p.active_tab = Some(tab.id());
        p.tabs.push(tab);
        p.tabs.push(TabState::Console {
            id: Uuid::new_v4(),
            connection: c.id,
            title: "console 1".into(),
            text: "select 1;".into(),
        });
        p.connections.push(c);
        p
    }

    #[test]
    fn project_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let p = sample_project();
        store.save_project(&p).unwrap();
        assert_eq!(store.load_project(p.id).unwrap(), p);
        let (list, bad) = store.list_projects();
        assert_eq!(list, vec![p]);
        assert!(bad.is_empty());
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let mut p = sample_project();
        store.save_project(&p).unwrap();
        p.name = "Renamed".into();
        store.save_project(&p).unwrap();
        assert_eq!(store.load_project(p.id).unwrap().name, "Renamed");
        let names: Vec<_> = fs::read_dir(dir.path().join("projects"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec![format!("{}.json", p.id)]);
    }

    #[test]
    fn unreadable_file_is_skipped_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let p = sample_project();
        store.save_project(&p).unwrap();
        let broken = dir.path().join("projects").join(format!("{}.json", Uuid::new_v4()));
        fs::write(&broken, b"{ not json").unwrap();
        let (list, bad) = store.list_projects();
        assert_eq!(list.len(), 1);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].path, broken);
        // still there after listing and after opening the startup project
        store.open_startup_project().unwrap();
        assert_eq!(fs::read(&broken).unwrap(), b"{ not json");
    }

    #[test]
    fn first_start_creates_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let (p, _) = store.open_startup_project().unwrap();
        assert_eq!(p.name, "Default");
        assert_eq!(store.load_state().last_project, Some(p.id));
        let (again, _) = store.open_startup_project().unwrap();
        assert_eq!(again.id, p.id);
    }

    #[test]
    fn startup_prefers_last_project() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let a = Project::new("A");
        let b = Project::new("B");
        store.save_project(&a).unwrap();
        store.save_project(&b).unwrap();
        store
            .save_state(&AppState {
                last_project: Some(b.id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.open_startup_project().unwrap().0.id, b.id);
    }

    #[test]
    fn no_password_in_any_file() {
        const SECRET: &str = "hunter2-very-secret-Ω";
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let secrets = MemorySecretStore::default();
        let p = sample_project();
        secrets.set(p.connections[0].id, SECRET).unwrap();
        store.save_project(&p).unwrap();
        store
            .save_state(&AppState {
                last_project: Some(p.id),
                ..Default::default()
            })
            .unwrap();
        for entry in walk(dir.path()) {
            let bytes = fs::read(&entry).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains(SECRET), "password found in {}", entry.display());
        }
    }

    #[test]
    fn delete_project_removes_file_and_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path()).unwrap();
        let secrets = MemorySecretStore::default();
        let p = sample_project();
        secrets.set(p.connections[0].id, "x").unwrap();
        store.save_project(&p).unwrap();
        store.delete_project(&p, &secrets).unwrap();
        assert!(store.load_project(p.id).is_err());
        assert_eq!(secrets.get(p.connections[0].id).unwrap(), None);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }
}
