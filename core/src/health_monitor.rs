//! Health monitoring for a running model process.
//!
//! Polls `/v1/models` on the active model's port during startup,
//! transitioning the model state from Starting → Loading → Ready
//! (or Error if the health check timeout is exceeded).

use crate::process_manager::{ModelState, ProcessError};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Health monitor — polls `/v1/models` to track model startup progress.
pub struct HealthMonitor {
    port: u16,
    timeout_sec: u16,
}

impl HealthMonitor {
    /// Create a new health monitor for the given port and timeout.
    pub fn new(port: u16, timeout_sec: u16) -> Self {
        Self { port, timeout_sec }
    }

    /// Start monitoring until the model is Ready or the timeout is exceeded.
    #[allow(unused_assignments)]
    pub fn wait_until_ready(&self) -> Result<ModelState, ProcessError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(self.timeout_sec as u64);
        let mut state = ModelState::Starting;

        loop {
            match self.check_health() {
                Ok(true) => {
                    state = ModelState::Ready;
                    break;
                }
                Ok(false) => {
                    // Model is still loading — check if we should report Loading or keep waiting
                    if let Some(progress) = self.get_loading_progress() {
                        state = ModelState::Loading;
                        debug!("model loading progress: {}", progress);
                    } else {
                        state = ModelState::Starting;
                    }
                }
                Err(e) => {
                    // Health check failed — model might still be starting up
                    warn!("health check failed: {}", e);
                    // Don't fail immediately — give it a few more seconds
                    if std::time::Instant::now() + Duration::from_secs(3) >= deadline {
                        state = ModelState::Error(format!(
                            "health check timeout after {}s",
                            self.timeout_sec
                        ));
                        break;
                    }
                }
            }

            if std::time::Instant::now() >= deadline {
                state = ModelState::Error(format!(
                    "health check timeout after {}s",
                    self.timeout_sec
                ));
                break;
            }

            // Wait 1 second before next poll
            std::thread::sleep(Duration::from_secs(1));
        }

        info!("model health state: {:?}", state);
        Ok(state)
    }

    /// Check if the model is healthy (Ready).
    fn check_health(&self) -> Result<bool, ProcessError> {
        let url = format!("http://127.0.0.1:{}/v1/models", self.port);
        match reqwest::blocking::get(&url) {
            Ok(resp) => {
                if resp.status().is_success() {
                    // Check if it's a valid llama-server response
                    if let Ok(body) = resp.text() {
                        if body.contains("\"id\"") && body.contains("model") {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
            Err(e) => Err(ProcessError::HealthCheckFailed(format!(
                "could not reach {}: {}",
                url, e
            ))),
        }
    }

    /// Get the current loading progress (best effort).
    fn get_loading_progress(&self) -> Option<String> {
        let url = format!("http://127.0.0.1:{}/v1/models", self.port);
        match reqwest::blocking::get(&url) {
            Ok(resp) => {
                if resp.status().is_success() {
                    if let Ok(body) = resp.text() {
                        // Try to extract model name from response
                        if let Some(start) = body.find("\"id\"") {
                            if let Some(end) = body[start..].find('"') {
                                let id = &body[start + 4..start + end];
                                return Some(format!("loading: {}", id));
                            }
                        }
                    }
                }
                None
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_monitor_creation() {
        let monitor = HealthMonitor::new(8081, 30);
        assert_eq!(monitor.port, 8081);
        assert_eq!(monitor.timeout_sec, 30);
    }

    #[test]
    fn test_health_check_on_free_port() {
        let monitor = HealthMonitor::new(9999, 5);
        // Port 9999 should be free — expect an error
        let result = monitor.check_health();
        assert!(result.is_err());
    }
}
