//! Process lifecycle management for ai-switch.
//!
//! Defines a `ProcessGuard` trait with a Linux implementation, and provides
//! high-level operations: start, stop, switch, and zombie-port handling.

use nix::sys::signal::{SIGKILL, SIGTERM};
use nix::unistd::{getpgid, Pid};
use std::net::TcpStream;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::config::Config;

/// Error types for process management operations.
#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("another model is already running")]
    AnotherModelRunning,

    #[error("model '{0}' is not running")]
    NotRunning(String),

    #[error("port {port} occupied by unknown process (pid: {pid})")]
    PortOccupiedByUnknownProcess { pid: u32, port: u16 },

    #[error("failed to spawn process: {0}")]
    Spawn(#[from] std::io::Error),

    #[error("process exited unexpectedly with code: {0}")]
    UnexpectedExit(i32),

    #[error("shutdown timeout exceeded for model '{0}'")]
    ShutdownTimeout(String),

    #[error("port {0} still occupied after shutdown timeout")]
    PortStillOccupied(u16),

    #[error("health check failed during startup of '{0}'")]
    HealthCheckFailed(String),

    #[error("I/O error: {0}")]
    Io(std::io::Error),

    #[error("signal error: {0}")]
    Signal(#[from] nix::Error),

    #[error("cannot signal process group — target is the current process group (pid: {pid})")]
    CannotSignalOwnProcessGroup { pid: i32 },
}

impl From<config::ConfigError> for ProcessError {
    fn from(e: config::ConfigError) -> Self {
        ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("config error: {}", e),
        ))
    }
}

impl From<toml::de::Error> for ProcessError {
    fn from(e: toml::de::Error) -> Self {
        ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("toml parse error: {}", e),
        ))
    }
}

/// Guard for a running model process.
pub trait ProcessGuard: Send {
    /// Set up and start the model process.
    fn setup(script: &Path, port: u16, log_dir: &Path) -> Result<Self, ProcessError>
    where
        Self: Sized;

    /// Terminate the model process.
    fn terminate(&self) -> Result<(), ProcessError>;
}

/// Linux implementation of ProcessGuard.
#[cfg(target_os = "linux")]
pub struct LinuxProcessGuard {
    pid: Option<Pid>,
    #[allow(dead_code)]
    port: u16,
    shutdown_timeout_sec: u16,
}

#[cfg(target_os = "linux")]
impl LinuxProcessGuard {
    /// Set up and start the model process on Linux with PR_SET_PDEATHSIG.
    fn setup(script: &Path, port: u16, log_dir: &Path) -> Result<Self, ProcessError> {
        let log_file = Self::open_log_file(script, log_dir)?;

        // Fork the child so we can set PDEATHSIG before exec
        let parent_pid = unsafe { libc::getpid() };
        let fork_result = unsafe { libc::fork() };
        if fork_result < 0 {
            return Err(ProcessError::Spawn(std::io::Error::last_os_error()));
        }

        if fork_result == 0 {
            // Child process
            // BUG 4 FIX: After setting PDEATHSIG, verify the parent is still alive.
            // If getppid() differs from the original parent pid, the parent died
            // between fork and prctl — the child should not continue.
            let ret = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
            if ret != 0 {
                std::process::exit(1);
            }

            // Check for race: parent died after fork but before prctl took effect
            let current_ppid = unsafe { libc::getppid() as i32 };
            if current_ppid != parent_pid {
                // Parent died — the OS already sent SIGTERM via PDEATHSIG,
                // but we need to exit anyway since our "parent" is gone.
                std::process::exit(1);
            }

            // Create a new session so we're not a child of the parent's process group
            let ret = unsafe { libc::setsid() };
            if ret < 0 {
                std::process::exit(1);
            }

            // Redirect stdout/stderr to log file
            let stdout_fd = log_file.as_raw_fd();
            nix::unistd::dup2(stdout_fd, libc::STDOUT_FILENO as i32).ok();
            nix::unistd::dup2(stdout_fd, libc::STDERR_FILENO as i32).ok();

           // Set PORT env var
            std::env::set_var("PORT", port.to_string());

            // Execute the script via /bin/bash to ensure reliable execution
            // across all Linux filesystems regardless of shebang.
            let bash_c = std::ffi::CString::new("/bin/bash").unwrap();
            let script_c = std::ffi::CString::new(script.to_string_lossy().as_bytes()).unwrap();
            let argv: [*const libc::c_char; 3] = [bash_c.as_ptr(), script_c.as_ptr(), std::ptr::null()];

            unsafe {
                libc::execvp(bash_c.as_ptr(), argv.as_ptr());
            }

            // If execvp returns, it failed — log the error to stderr
            // (redirected to log file in this child process)
            eprintln!("execvp failed for {}: {}", script.display(), std::io::Error::last_os_error());
            std::process::exit(1);
        }

        // Parent process — track the PID directly (no Child struct needed)
        let pid = Pid::from_raw(fork_result);

        // Wait a bit for the child to exec (max 5s)
        for _ in 0..50 {
            let status = nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
            match status {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                // BUG 3 FIX: If waitpid returns a non-stillalive status (child exited),
                // the script likely failed — return an error rather than silently succeeding.
                Ok(status) => {
                    if let nix::sys::wait::WaitStatus::Exited(_, exit_code) = status {
                        return Err(ProcessError::Spawn(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("child process exited with code {}", exit_code),
                        )));
                    }
                    // Other statuses (signal, stopped etc.) — also indicate failure
                    return Err(ProcessError::Spawn(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "child process exited unexpectedly",
                    )));
                }
                Err(_) => break, // process gone
            }
        }

        Ok(Self {
            pid: Some(pid),
            port,
            shutdown_timeout_sec: 10,
        })
    }

    /// Get the PID of the running process.
    pub fn pid(&self) -> Option<Pid> {
        self.pid
    }

    fn open_log_file(script: &Path, log_dir: &Path) -> Result<std::fs::File, ProcessError> {
        use std::fs;
        fs::create_dir_all(log_dir).map_err(|e| ProcessError::Io(e))?;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("{}_{}.log", script.file_stem().unwrap_or_default().to_string_lossy(), timestamp);
        let log_path = log_dir.join(filename);

        Ok(fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&log_path)?)
    }

    fn terminate_process_group(pid: Pid, timeout_sec: u16) -> Result<(), ProcessError> {
        let raw_pid = pid.as_raw();

        // Safety: do not signal pid <= 0 (invalid or kernel process).
        if raw_pid <= 0 {
            return Err(ProcessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("cannot signal PID {} (must be positive)", raw_pid),
            )));
        }

        // Get the process group ID of the target.
        let pgid = match getpgid(Some(pid)) {
            Ok(pgid) => pgid,
            // ESRCH: process already gone — treat as dead.
            Err(nix::errno::Errno::ESRCH) => {
                debug!("process {} already gone (ESRCH)", pid);
                return Ok(());
            }
            Err(e) => {
                return Err(ProcessError::Signal(e));
            }
        };

        // Safety: never kill our own process group.
        let our_pgid = unsafe { libc::getpgid(0) };
        if our_pgid >= 0 {
            if pgid.as_raw() == our_pgid {
                return Err(ProcessError::CannotSignalOwnProcessGroup { pid: raw_pid });
            }
        }

        // Helper closure to signal the process group via -PGID.
        let signal_target = |target: Pid, sig: nix::sys::signal::Signal| -> Result<(), ProcessError> {
            let raw = target.as_raw();
            let sig_raw = sig as libc::c_int;
            // Negate raw to target the process group in Linux kill()
            let ret = unsafe { libc::kill(-raw, sig_raw) };
            if ret != 0 {
                let e = std::io::Error::last_os_error();
                // ESRCH: process already gone — not an error.
                if e.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                return Err(ProcessError::Io(e));
            }
            Ok(())
        };

        // Step a: Send SIGTERM to the process group.
        signal_target(pgid, SIGTERM)?;

        // Step b: Wait/poll until the port is free or shutdown timeout expires.
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_sec as u64);
        loop {
            // Wait for our direct child pid (which is the session leader)
            match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(_) | Err(_) => return Ok(()),
            }
        }

        // Step c: If still alive, send SIGKILL to the process group.
        warn!("process group {} didn't shut down gracefully, sending SIGKILL", pgid);
        signal_target(pgid, SIGKILL)?;

        // Step d: Wait/reap the child after SIGKILL (up to 5s).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Ok(_) | Err(_) => return Ok(()),
            }
        }

        Err(ProcessError::ShutdownTimeout("unknown".to_string()))
    }
}

#[cfg(target_os = "linux")]
impl ProcessGuard for LinuxProcessGuard {
    fn setup(script: &Path, port: u16, log_dir: &Path) -> Result<Self, ProcessError>
    where
        Self: Sized,
    {
        Self::setup(script, port, log_dir)
    }

    fn terminate(&self) -> Result<(), ProcessError> {
        if let Some(pid) = self.pid {
            Self::terminate_process_group(pid, self.shutdown_timeout_sec)?;
        }
        Ok(())
    }
}

/// The current state of a model.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelState {
    Stopped,
    Starting,
    Loading,
    Ready,
    Error(String),
}

/// A running model's metadata.
pub struct RunningModel {
    pub id: String,
    pub guard: Box<dyn ProcessGuard>,
    pub state: ModelState,
}

impl std::fmt::Debug for RunningModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningModel")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish()
    }
}

/// Process manager — manages the lifecycle of model processes.
pub struct ProcessManager {
    config: Config,
    running_model: Option<RunningModel>,
}

impl ProcessManager {
    /// Create a new process manager from a loaded config.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            running_model: None,
        }
    }

    /// Start a model by id.
    pub fn start_model(&mut self, id: &str) -> Result<(), ProcessError> {
        // One-at-a-time rule
        if let Some(ref _running) = self.running_model {
            return Err(ProcessError::AnotherModelRunning);
        }

        // Find the model config
        let model_config = self
            .config
            .models
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| ProcessError::NotRunning(id.to_string()))?;

        // Check port — zombie port handling
        match Self::check_port(model_config.port) {
            PortState::Free => {}
            PortState::OccupiedByModel => {
                return Err(ProcessError::HealthCheckFailed(format!(
                    "model '{}' is already running on port {}",
                    id, model_config.port
                )));
            }
            PortState::OccupiedByUnknown(pid) => {
                return Err(ProcessError::PortOccupiedByUnknownProcess {
                    pid,
                    port: model_config.port,
                });
            }
        }

        // Spawn the process (synchronously before entering async context)
        let log_dir = self.config.log_dir();
        let guard_result = LinuxProcessGuard::setup(
            &model_config.script_path,
            model_config.port,
            &log_dir,
        );

        match guard_result {
            Ok(guard) => {
                info!("started model '{}' on port {}", id, model_config.port);
                self.running_model = Some(RunningModel {
                    id: id.to_string(),
                    guard: Box::new(guard),
                    state: ModelState::Starting,
                });
                Ok(())
            }
            Err(e) => {
                error!("failed to start model '{}': {}", id, e);
                Err(e)
            }
        }
    }

    /// Stop a model by id.
    pub fn stop_model(&mut self, id: &str) -> Result<(), ProcessError> {
        let running = self
            .running_model
            .as_ref()
            .ok_or_else(|| ProcessError::NotRunning(id.to_string()))?;

        if running.id != id {
            return Err(ProcessError::NotRunning(id.to_string()));
        }

        // Get the port from the config for the running model
        let port = self
            .config
            .models
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.port)
            .ok_or_else(|| ProcessError::NotRunning(id.to_string()))?;

        // Terminate the process group
        running.guard.terminate()?;

        // Confirm port is free via TCP bind retry (up to 10s)
        Self::wait_port_free(port, Duration::from_secs(10))?;

        info!("stopped model '{}'", id);
        self.running_model = None;
        Ok(())
    }

    /// Stop all running models.
    pub fn stop_all(&mut self) -> Result<(), ProcessError> {
        if let Some(ref running) = self.running_model {
            let id = running.id.clone();
            let port = self
                .config
                .models
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.port)
                .ok_or_else(|| ProcessError::NotRunning(id.clone()))?;
            running.guard.terminate()?;
            Self::wait_port_free(port, Duration::from_secs(10))?;
        }
        self.running_model = None;
        Ok(())
    }

    /// Switch from one model to another (atomic sequence).
    pub fn switch_model(&mut self, from_id: &str, to_id: &str) -> Result<(), ProcessError> {
        // Step 1: stop the current model
        self.stop_model(from_id)?;

        // Step 2: short delay for CUDA context release
        std::thread::sleep(Duration::from_millis(500));

        // Step 3: start the new model
        self.start_model(to_id)
    }

    /// Get the currently running model, if any.
    pub fn get_running_model(&self) -> Option<&RunningModel> {
        self.running_model.as_ref()
    }

    /// Get the currently running model id, if any.
    pub fn get_running_model_id(&self) -> Option<&str> {
        self.running_model.as_ref().map(|m| m.id.as_str())
    }

    /// Check if a port is free or occupied by a llama-server process.
    fn check_port(port: u16) -> PortState {
        // Try to bind the port
        if TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            // Port is occupied — probe it for /v1/models with a short timeout
            // to prevent indefinite hangs on unresponsive listeners.
            let url = format!("http://127.0.0.1:{}/v1/models", port);
            if let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
            {
                if let Ok(resp) = client.get(&url).send() {
                    if resp.status().is_success() {
                        // Check if it's a llama-server response (has /v1/models endpoint)
                        if let Ok(body) = resp.text() {
                            if body.contains("\"id\"") || body.contains("model") {
                                return PortState::OccupiedByModel;
                            }
                        }
                    }
                }
            }
            // Occupied but not a llama-server — try to get pid
            if let Ok(pid) = Self::get_port_pid(port) {
                return PortState::OccupiedByUnknown(pid);
            }
            PortState::OccupiedByUnknown(0)
        } else {
            PortState::Free
        }
    }

    /// Wait for a port to become free.
    fn wait_port_free(port: u16, timeout: Duration) -> Result<(), ProcessError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if TcpStream::connect(format!("127.0.0.1:{}", port)).is_err() {
                return Ok(()); // port is free
            }
            if std::time::Instant::now() >= deadline {
                return Err(ProcessError::PortStillOccupied(port));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Get the PID of a process bound to a port (best effort).
    pub fn get_port_pid(port: u16) -> Result<u32, ProcessError> {
        // Use /proc/net/tcp to find the PID — this is Linux-specific
        #[cfg(target_os = "linux")]
        {
            let tcp_path = "/proc/net/tcp";
            if let Ok(content) = std::fs::read_to_string(tcp_path) {
                for line in content.lines().skip(1) {
                    // Format: sl local_address remote_address ... state ...
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 4 {
                        let local_addr = parts[1];
                        let local_port = u16::from_str_radix(&local_addr.split(':').nth(1).unwrap_or_default(), 16).ok();
                        if local_port == Some(port) {
                            // Parse inode to find the PID
                            if parts.len() >= 8 {
                                let inode = parts[9];
                                if let Ok(inode_num) = inode.parse::<u32>() {
                                    // Search /proc/*/fd for this inode
                                    if let Ok(entries) = std::fs::read_dir("/proc") {
                                        for entry in entries.flatten() {
                                            let fd_path = entry.path().join("fd");
                                            if let Ok(fds) = std::fs::read_dir(&fd_path) {
                                                for fd_entry in fds.flatten() {
                                                    if let Ok(link) = std::fs::read_link(fd_entry.path()) {
                                                        if link.to_string_lossy().contains(&inode_num.to_string()) {
                                                            if let Some(pid_str) = entry.file_name().to_string_lossy().strip_prefix("proc") {
                                                                if let Ok(pid) = pid_str.parse::<u32>() {
                                                                    return Ok(pid);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(ProcessError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "couldn't determine port PID",
        )))
    }
}

/// State of a network port.
#[derive(Debug, PartialEq)]
pub enum PortState {
    Free,
    OccupiedByModel,
    OccupiedByUnknown(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_state_free() {
        // Port 9999 should be free (unless something is running on it)
        let state = ProcessManager::check_port(9999);
        assert_eq!(state, PortState::Free);
    }

    #[test]
    fn test_process_error_variants() {
        let err = ProcessError::AnotherModelRunning;
        assert!(err.to_string().contains("another model"));

        let err = ProcessError::NotRunning("m1".to_string());
        assert!(err.to_string().contains("m1"));

        let err = ProcessError::PortOccupiedByUnknownProcess { pid: 1234, port: 8081 };
        assert!(err.to_string().contains("8081"));
        assert!(err.to_string().contains("1234"));
    }
}
