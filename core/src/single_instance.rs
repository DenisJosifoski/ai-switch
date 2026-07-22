//! Single-instance enforcement using the `single-instance` crate.
//!
//! On second instance, prints a message and exits immediately — no silent
//! failure.

use single_instance::SingleInstance;
use tracing::{info, warn};

/// Guard that ensures only one instance of ai-switch runs at a time.
pub struct SingleInstanceGuard {
    instance: Option<SingleInstance>,
}

impl SingleInstanceGuard {
    /// Try to acquire the single-instance lock.
    ///
    /// Returns `Ok(Self)` if this is the first instance, or `Err(AlreadyRunning)`
    /// if another instance is already running.
    pub fn try_acquire() -> Result<Self, AlreadyRunning> {
        let instance = SingleInstance::new("ai-switch").map_err(|e| {
            warn!("another instance of ai-switch is already running: {}", e);
            AlreadyRunning
        })?;

        info!("single-instance guard acquired");
        Ok(Self { instance: Some(instance) })
    }

    /// Release the single-instance lock.
    pub fn release(&mut self) {
        if let Some(instance) = self.instance.take() {
            // The single-instance crate drops the lock automatically when dropped,
            // but we want to log it. Just drop the instance.
            drop(instance);
            info!("single-instance guard released");
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Error type for already-running instances.
#[derive(Debug, Clone)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "another instance of ai-switch is already running")
    }
}

impl std::error::Error for AlreadyRunning {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_already_running_error_display() {
        let err = AlreadyRunning;
        assert_eq!(format!("{}", err), "another instance of ai-switch is already running");
    }
}
