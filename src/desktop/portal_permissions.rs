//! Persisted xdg-desktop-portal restore tokens.
//!
//! A restore token lets the same application reopen its ScreenCast /
//! RemoteDesktop session without a new consent dialog. Tokens are credentials:
//! the file is mode 0600 and the value is never logged.

use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Store {
    #[serde(default)]
    entries: std::collections::BTreeMap<String, Entry>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Entry {
    #[serde(default)]
    token: String,
    /// Last observed permission grant. `false` means the compositor rejected
    /// the request or the token was revoked.
    #[serde(default)]
    granted: bool,
    #[serde(default)]
    updated: u64,
}

fn path() -> Option<PathBuf> {
    crate::config::Config::path()
        .ok()
        .map(|path| path.with_file_name("portal_permissions.json"))
}

fn load() -> Store {
    path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store() -> std::sync::MutexGuard<'static, Store> {
    static STORE: std::sync::OnceLock<Mutex<Store>> = std::sync::OnceLock::new();
    STORE
        .get_or_init(|| Mutex::new(load()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn persist(store: &Store) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = path().ok_or("config directory unavailable")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.new");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(store).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The last token returned by the portal for this permission key.
pub fn restore_token(key: &str) -> Option<String> {
    store()
        .entries
        .get(key)
        .map(|entry| entry.token.clone())
        .filter(|token| !token.is_empty() && token.len() <= 4096)
}

/// Persist a token returned by `Start`. An empty token keeps the old value.
pub fn save_token(key: &str, token: &str) {
    if token.is_empty() || token.len() > 4096 {
        return;
    }
    let mut store = store();
    let entry = store.entries.entry(key.to_owned()).or_default();
    if entry.token == token && entry.granted {
        return;
    }
    entry.token = token.to_owned();
    entry.granted = true;
    entry.updated = now();
    if let Err(error) = persist(&store) {
        tracing::warn!("portal permission cache not saved: {error}");
    }
}

/// Record the outcome of a permission request. A rejected request keeps the
/// token (the compositor may accept it after an unlock) but marks it unused.
pub fn record_permission(key: &str, granted: bool) {
    let mut store = store();
    let entry = store.entries.entry(key.to_owned()).or_default();
    if entry.granted == granted && entry.updated != 0 {
        return;
    }
    entry.granted = granted;
    entry.updated = now();
    if let Err(error) = persist(&store) {
        tracing::warn!("portal permission cache not saved: {error}");
    }
}

pub fn last_granted(key: &str) -> Option<bool> {
    store().entries.get(key).map(|entry| entry.granted)
}

/// Permission key for a capture source selection.
pub fn key(source: &str) -> String {
    format!("desktop:{source}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_without_losing_tokens() {
        let mut store = Store::default();
        store.entries.insert(
            "desktop:monitor".into(),
            Entry {
                token: "token".into(),
                granted: true,
                updated: 7,
            },
        );
        let bytes = serde_json::to_vec(&store).unwrap();
        let decoded: Store = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.entries["desktop:monitor"].token, "token");
        assert!(decoded.entries["desktop:monitor"].granted);
    }

    #[test]
    fn keys_are_scoped_to_the_source_kind() {
        assert_eq!(key("monitor"), "desktop:monitor");
        assert_ne!(key("monitor"), key("window"));
    }
}
