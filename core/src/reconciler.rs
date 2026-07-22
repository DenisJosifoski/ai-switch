//! Startup reconciliation — checks for models that may have been running
//! before ai-switch started and determines the initial state.

use crate::config::Config;
use crate::process_manager::{ProcessError, PortState};
use std::net::TcpStream;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Build an HTTP client with a timeout to prevent indefinite hangs on
/// unresponsive or hung listeners during startup reconciliation.
fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client build failed")
}

/// Result of the startup reconciliation.
#[derive(Debug)]
pub enum ReconcileResult {
    /// Exactly one model is running — return its id.
    OneRunning(String),
    /// Multiple models are running — surface a warning.
    MultipleRunning(Vec<String>),
    /// No models are running — all start in Stopped state.
    NoneRunning,
}

/// Reconciler — checks for running models at startup.
pub struct Reconciler {
    config: Config,
}

impl Reconciler {
    /// Create a new reconciler from a loaded config.
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the reconciliation.
    pub fn reconcile(&self) -> Result<ReconcileResult, ProcessError> {
        let mut running_ids: Vec<String> = Vec::new();

        for model in &self.config.models {
            match PortState::from_port_check(model.port) {
                PortState::OccupiedByModel => {
                    debug!("found running model '{}' on port {}", model.id, model.port);
                    running_ids.push(model.id.clone());
                }
                PortState::OccupiedByUnknown(pid) => {
                    warn!(
                        "port {} occupied by unknown process (pid: {})",
                        model.port, pid
                    );
                }
                PortState::Free => {}
            }
        }

        let result = match running_ids.len() {
            0 => ReconcileResult::NoneRunning,
            1 => ReconcileResult::OneRunning(running_ids.into_iter().next().unwrap()),
            _ => ReconcileResult::MultipleRunning(running_ids),
        };

        info!("reconciliation result: {:?}", result);
        Ok(result)
    }
}

/// Extension trait for PortState to check if a port is occupied by a llama-server.
trait PortCheck {
    fn from_port_check(port: u16) -> PortState;
}

impl PortCheck for PortState {
    fn from_port_check(port: u16) -> Self {
        // Try to connect to the port
        if TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            // Port is occupied — probe it for /v1/models
            let url = format!("http://127.0.0.1:{}/v1/models", port);
            if let Ok(resp) = http_client().get(&url).send() {
                if resp.status().is_success() {
                    // Check if it's a valid llama-server response
                    if let Ok(body) = resp.text() {
                        if body.contains("\"id\"") && body.contains("model") {
                            return PortState::OccupiedByModel;
                        }
                    }
                }
            }
            // Occupied but not a llama-server — try to get pid
            if let Ok(pid) = crate::process_manager::ProcessManager::get_port_pid(port) {
                return PortState::OccupiedByUnknown(pid);
            }
            PortState::OccupiedByUnknown(0)
        } else {
            PortState::Free
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_check_on_free_port() {
        // Port 9999 should be free
        let state = PortState::from_port_check(9999);
        assert_eq!(state, PortState::Free);
    }
}
