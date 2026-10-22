use crate::error::{Error, Result};
use crate::util::random_device_id;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_API_BASE: &str = "https://api.nrd.nie.163.com";
fn default_api_base() -> String {
    DEFAULT_API_BASE.to_owned()
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, resolved against `$HOME`.
fn xdg_download_dir(home: &Path) -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let text = fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    let home = home.to_string_lossy();
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?
        .trim()
        .trim_matches('"')
        .replace("${HOME}", &home)
        .replace("$HOME", &home);
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_api_base")]
    pub api_base: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    /// UUID sent as `X-Param-client-id`; generated on first use.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Stable value sent as `X-Param-System-ID`.
    #[serde(default)]
    pub system_id: Option<String>,
    /// True after `/api/v1/device/windows/init` returned a server device id.
    #[serde(default)]
    pub device_registered: bool,
    /// Optional multipart upload endpoint used by `device wallpaper --file`.
    #[serde(default)]
    pub upload_url: Option<String>,
    /// Default shell requested from a controlled device.
    #[serde(default)]
    pub shell: Option<String>,
    /// What the portal is allowed to capture: monitor, window or virtual.
    #[serde(default)]
    pub display_source: Option<String>,
    /// Capture backend preference: auto, portal, wlroots or x11.
    #[serde(default)]
    pub desktop_backend: Option<String>,
    /// Directory the viewer's file-transfer window opens on and receives into.
    #[serde(default)]
    pub file_save_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            token: None,
            user_id: None,
            device_id: None,
            device_name: None,
            client_id: None,
            system_id: None,
            device_registered: false,
            upload_url: None,
            shell: None,
            display_source: None,
            desktop_backend: None,
            file_save_path: None,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        if let Ok(path) = std::env::var("OPENUUR_CONFIG") {
            return Ok(PathBuf::from(path));
        }
        let state = dirs::state_dir().map(|base| base.join("openuur").join("config.toml"));
        let legacy = dirs::config_dir().map(|base| base.join("openuur").join("config.toml"));
        match (state, legacy) {
            // Keep using an existing config from the old location until the
            // user moves it, so upgrading does not lose the token or device id.
            (Some(state), Some(legacy)) if !state.exists() && legacy.exists() => Ok(legacy),
            (Some(state), _) => Ok(state),
            (None, Some(legacy)) => Ok(legacy),
            (None, None) => Err(Error::Config(
                "cannot determine the user state directory".to_owned(),
            )),
        }
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
    }

    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialize config: {e}")))?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&tmp, perms)?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Return the configured device id, generating and persisting one if needed.
    pub fn ensure_device_id(&mut self) -> Result<String> {
        if let Some(id) = &self.device_id {
            if !id.trim().is_empty() {
                return Ok(id.clone());
            }
        }
        let id = random_device_id();
        self.device_id = Some(id.clone());
        self.save()?;
        Ok(id)
    }

    pub fn ensure_device_name(&mut self) -> String {
        if let Some(name) = &self.device_name {
            if !name.trim().is_empty() {
                return name.clone();
            }
        }
        let name = default_device_name();
        self.device_name = Some(name.clone());
        let _ = self.save();
        name
    }

    pub fn ensure_client_id(&mut self) -> String {
        if let Some(id) = &self.client_id {
            if !id.trim().is_empty() {
                return id.clone();
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.client_id = Some(id.clone());
        let _ = self.save();
        id
    }

    pub fn ensure_system_id(&mut self) -> String {
        if let Some(id) = &self.system_id {
            if !id.trim().is_empty() {
                return id.clone();
            }
        }
        let id = "deaddead-dead-dead-dead-deaddeaddead".to_owned();
        self.system_id = Some(id.clone());
        let _ = self.save();
        id
    }

    pub fn require_token(&self) -> Result<String> {
        self.token
            .clone()
            .filter(|t| !t.trim().is_empty())
            .ok_or(Error::NotLoggedIn)
    }

    /// Directory the file-transfer window starts in and uploads default to.
    /// Falls back to `$XDG_DOWNLOAD_DIR`, `~/Downloads`, then
    /// `~/.local/share/openuur/downloads`.
    pub fn file_save_path(&self) -> PathBuf {
        let expand = |value: &str| {
            let value = value.trim();
            if let Some(rest) = value.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    return PathBuf::from(home).join(rest);
                }
            }
            PathBuf::from(value)
        };
        if let Some(path) = self
            .file_save_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        {
            return expand(path);
        }
        if let Ok(path) = std::env::var("UU_FILE_SAVE_PATH") {
            if !path.trim().is_empty() {
                return expand(&path);
            }
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        if let Some(downloads) = xdg_download_dir(&home) {
            if downloads.is_dir() {
                return downloads;
            }
        }
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
        home.join(".local/share/openuur/downloads")
    }

    /// Display source selected for the portal: `monitor` (default), `window`
    /// or `virtual`.
    pub fn display_source(&self) -> crate::desktop::SourceKind {
        let configured = self
            .display_source
            .as_deref()
            .map(str::to_owned)
            .or_else(|| std::env::var("UU_DISPLAY_SOURCE").ok())
            .unwrap_or_else(|| "monitor".to_owned());
        crate::desktop::SourceKind::parse(&configured).unwrap_or_default()
    }

    /// Capture backend preference. The config file wins over the environment,
    /// matching `display_source`.
    pub fn desktop_backend(&self) -> crate::desktop::BackendChoice {
        let configured = self
            .desktop_backend
            .clone()
            .or_else(|| std::env::var("UU_DESKTOP_BACKEND").ok())
            .unwrap_or_else(|| "auto".to_owned());
        crate::desktop::BackendChoice::parse(&configured).unwrap_or_default()
    }
}

pub fn default_device_name() -> String {
    std::env::var("OPENUUR_DEVICE_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openuur-device".to_owned())
}
