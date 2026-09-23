//! Persistent lazyamp preferences (`$XDG_CONFIG_HOME/lazyamp/config.toml`).

use crate::amp::StartOptions;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Maximum remembered working directories.
pub const MAX_RECENT_DIRS: usize = 12;

/// On-disk configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub defaults: StartDefaults,
    /// Recently used directories, newest first.
    #[serde(default)]
    pub recent_dirs: Vec<String>,
}

/// Default flags applied when starting `amp --no-tui`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StartDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<String>,
    /// Built-in Amp modes: `low`, `medium`, `high`, `ultra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `debug`, `info`, `warn`, `error`, `audit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_config: Option<String>,
    /// `private`, `unlisted`, `workspace`, `group`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(default)]
    pub remote_control_terminal: bool,
    #[serde(default)]
    pub discover_dirs: bool,
    #[serde(default)]
    pub amp_env: bool,
}

impl StartDefaults {
    /// Convert stored defaults into Amp start options.
    pub fn to_start_options(&self) -> StartOptions {
        StartOptions {
            runner_id: nonempty(self.runner_id.as_deref()),
            mode: nonempty(self.mode.as_deref()),
            log_level: nonempty(self.log_level.as_deref()),
            settings_file: nonempty(self.settings_file.as_deref()),
            mcp_config: nonempty(self.mcp_config.as_deref()),
            visibility: nonempty(self.visibility.as_deref()),
            remote_control_terminal: self.remote_control_terminal,
            extra_dirs: Vec::new(),
            discover_dirs: self.discover_dirs,
            amp_env: self.amp_env,
        }
    }
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Resolve the config file path using the process XDG/config directory.
pub fn config_path() -> PathBuf {
    config_path_from(dirs::config_dir())
}

/// Resolve the config file under an explicit config home (testable).
pub fn config_path_from(config_dir: Option<PathBuf>) -> PathBuf {
    config_dir
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("lazyamp")
        .join("config.toml")
}

/// Directory for spawn-PID registry and runner logs.
pub fn state_dir() -> PathBuf {
    state_dir_from(dirs::state_dir().or_else(dirs::data_local_dir))
}

/// Resolve the state directory under an explicit state home (testable).
pub fn state_dir_from(state_home: Option<PathBuf>) -> PathBuf {
    state_home
        .unwrap_or_else(|| PathBuf::from(".local").join("state"))
        .join("lazyamp")
}

impl Config {
    /// Load config from the default path. Missing file yields defaults.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    /// Load config from `path`. Missing file yields defaults.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    /// Parse TOML into a [`Config`].
    ///
    /// `recent_dirs` may be written at the top level (preferred) or accidentally
    /// after `[defaults]` — TOML treats the latter as `defaults.recent_dirs`.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let value: toml::Value = raw.parse().context("invalid lazyamp config.toml")?;
        let mut parsed: Config = value
            .clone()
            .try_into()
            .context("invalid lazyamp config.toml")?;
        if parsed.recent_dirs.is_empty() {
            if let Some(arr) = value
                .get("defaults")
                .and_then(|d| d.get("recent_dirs"))
                .and_then(|v| v.as_array())
            {
                parsed.recent_dirs = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                    .collect();
            }
        }
        Ok(parsed.normalized())
    }

    /// Serialize to pretty TOML.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize config")
    }

    /// Write config to `path`, creating parent directories.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(path, self.to_toml()?)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// Write config to the default path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Remember `path` as the most recent directory.
    pub fn remember_dir(&mut self, path: &Path) {
        let rendered = path.to_string_lossy().to_string();
        if rendered.is_empty() {
            return;
        }
        self.recent_dirs.retain(|p| p != &rendered);
        self.recent_dirs.insert(0, rendered);
        self.recent_dirs.truncate(MAX_RECENT_DIRS);
    }

    fn normalized(mut self) -> Self {
        self.defaults.runner_id = nonempty(self.defaults.runner_id.as_deref());
        self.defaults.mode = nonempty(self.defaults.mode.as_deref());
        self.defaults.log_level = nonempty(self.defaults.log_level.as_deref());
        self.defaults.settings_file = nonempty(self.defaults.settings_file.as_deref());
        self.defaults.mcp_config = nonempty(self.defaults.mcp_config.as_deref());
        self.defaults.visibility = nonempty(self.defaults.visibility.as_deref());
        self.recent_dirs.retain(|p| !p.trim().is_empty());
        self.recent_dirs.truncate(MAX_RECENT_DIRS);
        self
    }
}

/// Cycle helper used by the flags panel.
pub fn cycle_choice(current: Option<&str>, choices: &[&str]) -> Option<String> {
    if choices.is_empty() {
        return None;
    }
    let Some(cur) = current.map(str::trim).filter(|s| !s.is_empty()) else {
        return Some(choices[0].to_string());
    };
    match choices.iter().position(|c| c.eq_ignore_ascii_case(cur)) {
        Some(i) if i + 1 < choices.len() => Some(choices[i + 1].to_string()),
        _ => None,
    }
}

pub const MODES: &[&str] = &["low", "medium", "high", "ultra"];
pub const LOG_LEVELS: &[&str] = &["debug", "info", "warn", "error", "audit"];
pub const VISIBILITIES: &[&str] = &["private", "unlisted", "workspace", "group"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_is_default() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn parse_roundtrip() {
        let raw = r#"
[defaults]
runner_id = "dev-box"
mode = "high"
log_level = "info"
settings_file = "/tmp/settings.json"
mcp_config = "/tmp/mcp.json"
visibility = "private"
remote_control_terminal = true
discover_dirs = true
amp_env = false

recent_dirs = ["/tmp/a", "/tmp/b"]
"#;
        let cfg = Config::parse(raw).unwrap();
        assert_eq!(cfg.defaults.runner_id.as_deref(), Some("dev-box"));
        assert_eq!(cfg.defaults.mode.as_deref(), Some("high"));
        assert!(cfg.defaults.remote_control_terminal);
        assert_eq!(cfg.recent_dirs, vec!["/tmp/a", "/tmp/b"]);

        let again = Config::parse(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(cfg, again);
    }

    #[test]
    fn blanks_become_none() {
        let cfg = Config::parse(
            r#"
[defaults]
runner_id = "  "
mode = ""
"#,
        )
        .unwrap();
        assert_eq!(cfg.defaults.runner_id, None);
        assert_eq!(cfg.defaults.mode, None);
    }

    #[test]
    fn remember_dir_newest_first_and_dedup() {
        let mut cfg = Config::default();
        cfg.remember_dir(Path::new("/a"));
        cfg.remember_dir(Path::new("/b"));
        cfg.remember_dir(Path::new("/a"));
        assert_eq!(cfg.recent_dirs, vec!["/a", "/b"]);
    }

    #[test]
    fn remember_dir_caps_length() {
        let mut cfg = Config::default();
        for i in 0..20 {
            cfg.remember_dir(Path::new(&format!("/p{i}")));
        }
        assert_eq!(cfg.recent_dirs.len(), MAX_RECENT_DIRS);
        assert_eq!(cfg.recent_dirs[0], "/p19");
    }

    #[test]
    fn config_path_from_joins_lazyamp() {
        let path = config_path_from(Some(PathBuf::from("/xdg/config")));
        assert_eq!(path, PathBuf::from("/xdg/config/lazyamp/config.toml"));
    }

    #[test]
    fn state_dir_from_joins_lazyamp() {
        let path = state_dir_from(Some(PathBuf::from("/xdg/state")));
        assert_eq!(path, PathBuf::from("/xdg/state/lazyamp"));
    }

    #[test]
    fn load_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.toml");
        assert_eq!(Config::load_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn save_and_load_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("config.toml");
        let mut cfg = Config::default();
        cfg.defaults.runner_id = Some("box".into());
        cfg.remember_dir(Path::new("/work"));
        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn defaults_map_to_start_options() {
        let defaults = StartDefaults {
            runner_id: Some("box".into()),
            mode: Some("ultra".into()),
            log_level: Some("debug".into()),
            settings_file: Some("/s.json".into()),
            mcp_config: Some("/m.json".into()),
            visibility: Some("workspace".into()),
            remote_control_terminal: true,
            discover_dirs: true,
            amp_env: true,
        };
        let opts = defaults.to_start_options();
        assert_eq!(opts.runner_id.as_deref(), Some("box"));
        assert!(opts.remote_control_terminal);
        assert!(opts.discover_dirs);
        assert!(opts.amp_env);
    }

    #[test]
    fn cycle_choice_wraps_to_none() {
        assert_eq!(cycle_choice(None, MODES).as_deref(), Some("low"));
        assert_eq!(cycle_choice(Some("low"), MODES).as_deref(), Some("medium"));
        assert_eq!(cycle_choice(Some("ultra"), MODES), None);
        assert_eq!(cycle_choice(Some("nope"), MODES), None);
    }
}
