//! Configuration parsing and validation for ai-switch.
//!
//! Loads `config.toml` from the XDG config directory, validates it, and
//! provides defaults for optional settings.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Error types for configuration operations.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error reading config: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("configuration error: {0}")]
    Validation(String),

    #[error("no config file found at any expected location")]
    NotFound,
}

impl From<toml::ser::Error> for ConfigError {
    fn from(e: toml::ser::Error) -> Self {
        ConfigError::TomlParse(toml::de::Error::custom(e.to_string()))
    }
}

// Need serde::de::Error trait in scope for `custom` method
use serde::de::Error as _;

/// A single model configuration entry.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ModelConfig {
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "deserialize_path")]
    pub script_path: PathBuf,
    pub port: u16,
    #[serde(default = "default_health_timeout")]
    pub health_timeout_sec: u16,
}

fn default_health_timeout() -> u16 {
    30
}

fn deserialize_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Ok(PathBuf::from(s))
}

/// Deserialize an optional PathBuf where an empty string ("") is treated as
/// None, allowing the accessor's default to kick in. This supports the
/// documented convention of `log_dir = ""` meaning "use default".
fn deserialize_optional_pathbuf<'de, D>(deserializer: D) -> Result<Option<PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) if s.is_empty() => Ok(None),  // "" → None, triggers default
        Some(s) => Ok(Some(PathBuf::from(s))),
        None => Ok(None),
    }
}

/// Global settings section.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct GlobalSettings {
  #[serde(default, deserialize_with = "deserialize_optional_pathbuf")]
    pub log_dir: Option<PathBuf>,
    #[serde(default)]
    pub proxy_port: Option<u16>,
    #[serde(default)]
    pub auto_restart_on_context_full: Option<bool>,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            log_dir: None,
            proxy_port: None,
            auto_restart_on_context_full: None,
        }
    }
}

/// Root configuration.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Config {
    pub schema_version: u8,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default)]
    pub global: GlobalSettings,
}

impl Config {
    /// Resolve the config file path from XDG directories.
    pub fn resolve_path() -> Option<PathBuf> {
        // Try $XDG_CONFIG_HOME first
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            let p = PathBuf::from(xdg).join("ai-switch").join("config.toml");
            if p.exists() {
                return Some(p);
            }
        }

        // Fallback to ~/.config/ai-switch/config.toml
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home).join(".config").join("ai-switch").join("config.toml");
            if p.exists() {
                return Some(p);
            }
        }

        None
    }

    /// Load and validate a config from the resolved path.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::resolve_path().ok_or(ConfigError::NotFound)?;
        let content = std::fs::read_to_string(&path)?;

        // Parse TOML directly — serde's #[serde(default)] handles missing fields.
        // The real defaults (log_dir → ~/.local/share/ai-switch/logs/,
        // proxy_port → 8080, auto_restart_on_context_full → true) are applied
        // in the accessor methods via unwrap_or_else(Self::default_*()).
        let raw: Self = toml::from_str(&content).map_err(ConfigError::TomlParse)?;
        Self::validate(&raw, &path)
    }

    /// Validate a loaded config.
    pub fn validate(config: &Self, _config_path: &Path) -> Result<Self, ConfigError> {
        // Check for empty model list
        if config.models.is_empty() {
            return Err(ConfigError::Validation(
                "no models configured — add at least one model to the config".into(),
            ));
        }

        // Check for duplicate ports
        let mut seen_ports: std::collections::HashSet<u16> =
            std::collections::HashSet::new();
        for model in &config.models {
            if !seen_ports.insert(model.port) {
                return Err(ConfigError::Validation(format!(
                    "duplicate port {} for models '{}' and '{}'",
                    model.port,
                    config
                        .models
                        .iter()
                        .find(|m| m.port == model.port && m.id != model.id)
                        .map(|m| &m.id)
                        .unwrap_or(&model.id),
                    model.id
                )));
            }
        }

        // Check for missing script files
        for model in &config.models {
            if !model.script_path.exists() {
                return Err(ConfigError::Validation(format!(
                    "script not found: {} (configured for model '{}')",
                    model.script_path.display(),
                    model.id,
                )));
            }
        }

        Ok(config.clone())
    }

    /// Get the default log directory.
    pub fn default_log_dir() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home)
                .join(".local/share/ai-switch/logs/")
        } else {
            PathBuf::from("logs/")
        }
    }

    /// Get the default proxy port.
    pub fn default_proxy_port() -> u16 {
        9080
    }

    /// Get the default auto-restart setting.
    pub fn default_auto_restart_on_context_full() -> bool {
        true
    }

    /// Get the effective log directory.
    pub fn log_dir(&self) -> PathBuf {
        self.global
            .log_dir
            .clone()
            .unwrap_or_else(Self::default_log_dir)
    }

    /// Get the effective proxy port.
    pub fn proxy_port(&self) -> u16 {
        self.global.proxy_port.unwrap_or_else(Self::default_proxy_port)
    }

    /// Get the effective auto-restart setting.
    pub fn auto_restart_on_context_full(&self) -> bool {
        self.global
            .auto_restart_on_context_full
            .unwrap_or_else(Self::default_auto_restart_on_context_full)
    }
}

/// Returns an example config.toml for first-run reference.
pub fn example_config() -> &'static str {
    include_str!("../../config.toml.example")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_temp_model_script(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
        let script = tmp.path().join(format!("{}.sh", name));
        fs::write(&script, "#!/bin/sh\necho hello\n").unwrap();
        #[cfg(unix)]
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .ok();
        script
    }

    #[allow(dead_code)]
    fn make_temp_config(
        tmp: &tempfile::TempDir,
        models: &[(&str, &str, u16)],
    ) -> PathBuf {
        let config_path = tmp.path().join("config.toml");
        let mut content = String::new();
        content.push_str("[models]\n");
        for (i, (id, name, port)) in models.iter().enumerate() {
            content.push_str(&format!(
                "[models.model_{}]\nid = \"{}\"\nname = \"{}\"\nscript_path = \"{}\"\nport = {}\nschema_version = 1\n",
                i, id, name, tmp.path().join(format!("{}.sh", id)).display(), port
            ));
        }
        content.push_str("[global]\nlog_dir = \"\"\nproxy_port = 8080\nauto_restart_on_context_full = true\n");
        fs::write(&config_path, &content).unwrap();
        config_path
    }

    #[test]
    fn test_validate_empty_model_list() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            schema_version: 1,
            models: vec![],
            global: GlobalSettings::default(),
        };
        let result = Config::validate(&config, tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no models configured"));
    }

    #[test]
    fn test_validate_duplicate_ports() {
        let tmp = tempfile::tempdir().unwrap();
        let script1 = make_temp_model_script(&tmp, "model1");
        let script2 = make_temp_model_script(&tmp, "model2");
        let config = Config {
            schema_version: 1,
            models: vec![
                ModelConfig {
                    id: "m1".to_string(),
                    name: "Model 1".to_string(),
                    script_path: script1.clone(),
                    port: 8081,
                    health_timeout_sec: 30,
                },
                ModelConfig {
                    id: "m2".to_string(),
                    name: "Model 2".to_string(),
                    script_path: script2.clone(),
                    port: 8081, // duplicate
                    health_timeout_sec: 30,
                },
            ],
            global: GlobalSettings::default(),
        };
        let result = Config::validate(&config, tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate port"));
    }

    #[test]
    fn test_validate_missing_script() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            schema_version: 1,
            models: vec![ModelConfig {
                id: "m1".to_string(),
                name: "Model 1".to_string(),
                script_path: tmp.path().join("nonexistent.sh"),
                port: 8081,
                health_timeout_sec: 30,
            }],
            global: GlobalSettings::default(),
        };
        let result = Config::validate(&config, tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("script not found"));
    }

    #[test]
    fn test_default_log_dir() {
        let dir = Config::default_log_dir();
        assert!(dir.to_string_lossy().ends_with(".local/share/ai-switch/logs/"));
    }

    #[test]
    fn test_defaults() {
        assert_eq!(Config::default_proxy_port(), 9080);
        assert!(Config::default_auto_restart_on_context_full());
    }
}
