//! Linux desktop application catalog for UU Quick Launch.
//!
//! The official viewer asks for the host's installed applications through
//! RPC 23, optionally loads an icon through RPC 24, and sends a top-level
//! `LaunchApp` message to start one. This module deliberately exposes only
//! `.desktop` entries and never accepts an arbitrary command from the viewer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

const MAX_APPS: usize = 256;
const MAX_ICON_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub key: String,
    pub name: String,
    pub app_id: String,
    pub target_path: String,
    pub icon: Option<PathBuf>,
    pub desktop_path: PathBuf,
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub fn catalog() -> Vec<AppEntry> {
    let mut apps = BTreeMap::new();
    for directory in data_directories() {
        collect(&directory.join("applications"), &mut apps, 0);
    }
    apps.into_values().take(MAX_APPS).collect()
}

fn data_directories() -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_owned())
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect();
    if let Some(user) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    {
        directories.insert(0, user);
    }
    directories
}

fn collect(directory: &Path, apps: &mut BTreeMap<String, AppEntry>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, apps, depth + 1);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("desktop") {
            continue;
        }
        if path
            .components()
            .any(|component| component.as_os_str().eq_ignore_ascii_case("wine"))
        {
            continue;
        }
        let Some(entry) = parse_entry(&path) else {
            continue;
        };
        apps.entry(entry.key.clone()).or_insert(entry);
    }
}

fn parse_entry(path: &Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_desktop = false;
    let mut kind = None;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut hidden = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_desktop = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop {
            continue;
        }
        if let Some(value) = line.strip_prefix("Type=") {
            kind = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("Name=") {
            name = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("Exec=") {
            exec = Some(clean_exec(value));
        } else if let Some(value) = line.strip_prefix("Icon=") {
            icon = Some(value.trim().to_owned());
        } else if line == "Hidden=true" || line == "NoDisplay=true" {
            hidden = true;
        }
    }
    if hidden || kind != Some("Application") {
        return None;
    }
    let name = name.filter(|value| !value.is_empty())?;
    let app_id = path.file_name()?.to_str()?.to_owned();
    let target = exec.unwrap_or_else(|| path.to_string_lossy().into_owned());
    let key = format!("{:016x}", fnv1a(path.to_string_lossy().as_bytes()));
    Some(AppEntry {
        key,
        name,
        app_id,
        target_path: target,
        icon: icon.and_then(|value| {
            let path = PathBuf::from(&value);
            path.is_absolute().then_some(path)
        }),
        desktop_path: path.to_path_buf(),
    })
}

fn clean_exec(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|part| !part.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_icon(value: &str) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() && path.is_file() {
        return Some(path);
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(value);
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
    }
    roots.push(PathBuf::from("/usr/share/icons"));
    roots.push(PathBuf::from("/usr/share/pixmaps"));
    for root in roots {
        if let Some(found) = search_icon(&root, name, 0) {
            return Some(found);
        }
    }
    None
}

fn search_icon(directory: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let entries = std::fs::read_dir(directory).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = search_icon(&path, name, depth + 1) {
                return Some(found);
            }
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if stem == name && matches!(extension, "png" | "svg" | "xpm") {
            return Some(path);
        }
    }
    None
}

pub fn find(key: &str) -> Option<AppEntry> {
    catalog().into_iter().find(|app| app.key == key)
}

pub fn launch(key: &str) -> Result<(), String> {
    let app = find(key).ok_or_else(|| "unknown quick-launch application".to_owned())?;
    let launched = std::process::Command::new("gio")
        .arg("launch")
        .arg(&app.desktop_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok();
    if launched {
        return Ok(());
    }
    std::process::Command::new("gtk-launch")
        .arg(app.app_id.trim_end_matches(".desktop"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("launch {}: {error}", app.name))
}

pub fn icon(key: &str) -> Option<Vec<u8>> {
    let app = find(key)?;
    let path = app.icon.or_else(|| {
        let content = std::fs::read_to_string(&app.desktop_path).ok()?;
        let name = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("Icon="))?;
        resolve_icon(name)
    })?;
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.len() as usize > MAX_ICON_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}
