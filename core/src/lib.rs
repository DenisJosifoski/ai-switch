//! ai-switch core engine — headless, testable library for managing local
//! llama.cpp model servers.
//!
//! This crate has zero GTK dependency and can be tested with `cargo test`.

pub mod config;
pub mod health_monitor;
pub mod process_manager;
pub mod reconciler;
pub mod single_instance;

use config::Config;
use health_monitor::HealthMonitor;
use process_manager::{ModelState, ProcessError, ProcessManager};
use reconciler::ReconcileResult;
use tracing::info;

/// Run the core engine: single-instance check → config load → reconciliation
/// → health monitoring. Returns the initial model state and a guard that keeps
/// the single-instance lock alive for the program's lifetime.
pub fn run(
    config_path: Option<&str>,
) -> Result<(Config, ReconcileResult, single_instance::SingleInstanceGuard), ProcessError> {
    // Single-instance check — keep the guard alive for the program's lifetime
    let guard = match single_instance::SingleInstanceGuard::try_acquire() {
        Ok(g) => g,
        Err(_) => return Err(ProcessError::AnotherModelRunning),
    };

    // Load config (or use example if no config exists)
    let config = if let Some(path) = config_path {
        // Load from specified path for testing — use toml::from_str directly
        // so serde's #[serde(default)] handles missing fields correctly.
        let path_buf = std::path::PathBuf::from(path);
        let content = std::fs::read_to_string(&path_buf).map_err(|e| {
            ProcessError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to read config: {}", e),
            ))
        })?;
        let raw: Config = toml::from_str(&content).map_err(|e| {
            ProcessError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("toml parse error: {}", e),
            ))
        })?;
        Config::validate(&raw, &path_buf).map_err(|e| {
            ProcessError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("config validation error: {}", e),
            ))
        })?
    } else {
        Config::load().map_err(|e| {
            ProcessError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("config load error: {}", e),
            ))
        })?
    };

    // Reconcile — check for already-running models
    let reconciler = reconciler::Reconciler::new(config.clone());
    let reconcile_result = reconciler.reconcile()?;

    info!("core engine initialized");
    Ok((config, reconcile_result, guard))
}

/// Start a model and wait until it's ready.
pub fn start_and_wait(
    config: &Config,
    model_id: &str,
) -> Result<(ProcessManager, ModelState), ProcessError> {
    let mut pm = ProcessManager::new(config.clone());
    pm.start_model(model_id)?;

    // Find the model's port for health monitoring
    let model_config = config
        .models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| ProcessError::NotRunning(model_id.to_string()))?;

    let monitor = HealthMonitor::new(model_config.port, model_config.health_timeout_sec);
    let state = monitor.wait_until_ready()?;

    Ok((pm, state))
}

/// Stop a running model.
pub fn stop_model(pm: &mut ProcessManager, model_id: &str, fast_shutdown: bool) -> Result<(), ProcessError> {
    pm.stop_model(model_id, fast_shutdown)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_without_config_returns_not_found() {
        // Without a config file, run() should return NotFound.
        // Override environment variables to prevent finding the real user config.
        std::env::set_var("XDG_CONFIG_HOME", "/nonexistent-xdg-config");
        std::env::set_var("HOME", "/nonexistent-home");

        let result = run(None);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("no config"));
    }


   #[test]
    fn test_run_with_example_config_returns_guard() {
        // With a valid config path, run() should succeed and return the guard.
        // This tests that the SingleInstanceGuard is now returned (Bug 2 fix).
        // Create a temp dir with dummy scripts so validation passes.
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("test-model.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho ok\n").ok();
        #[cfg(unix)]
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&script_path)
            .status()
            .ok();

        let config_content = format!(
            "schema_version = 1\n\n[[models]]\nid = \"test\"\nname = \"Test\"\nscript_path = \"{}\"\nport = 9999\nhealth_timeout_sec = 5\n",
            script_path.display()
        );
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, &config_content).unwrap();

        let result = run(Some(config_path.to_str().unwrap()));
        assert!(result.is_ok());
        let (_config, _reconcile_result, _guard) = result.unwrap();
    }

    #[test]
    fn test_example_config_is_available() {
        let example = config::example_config();
        assert!(!example.is_empty());
        assert!(example.contains("schema_version"));
    }
}
