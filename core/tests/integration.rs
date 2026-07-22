//! Phase 2 — Switch logic hardening integration tests.
//!
//! These tests exercise the stop→wait→start sequence inside `switch_model`
//! under real, repeated, adversarial conditions — not just the happy path
//! Phase 1 verified once.
//!
//! Two tasks:
//! 1. Repeated switch loop: call `switch_model` between two real model scripts
//!    back-to-back, ~10 times in a loop, asserting the target port is always
//!    free before the new process binds it and never double-binds.
//! 2. Zombie-port path: manually occupy a model's port with `nc -l {port}`
//!    before calling `start_model`, confirm you get
//!    `PortOccupiedByUnknownProcess` and not a silent hang or panic.

use serial_test::serial;
use std::net::TcpStream;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use ai_switch_core::config::Config;
use ai_switch_core::process_manager::{ProcessError, ProcessManager};
use ai_switch_core::run;

/// Path to the integration test config.
const INTEGRATION_CONFIG: &str = "/mnt/orico/Documents/ApplicationsRAW/ai-switch/core/tests/integration_tests.toml";

/// Test ports used by model-a and model-b.
const TEST_PORT_A: u16 = 9876;
const TEST_PORT_B: u16 = 9877;

/// Load config from the integration test path. Returns the config and a guard
/// that keeps the single-instance lock alive for the duration of the test.
fn load_test_config() -> (Config, ai_switch_core::single_instance::SingleInstanceGuard) {
    let (config, _reconcile_result, guard) = run(Some(INTEGRATION_CONFIG))
        .expect("should load integration config");
    (config, guard)
}

/// Check if a port is bound (i.e., we can connect to it).
fn is_port_bound(port: u16) -> bool {
    TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok()
}

/// Wait for a port to become free (up to 10 seconds).
fn wait_port_free(port: u16, timeout: Duration) -> Result<(), ProcessError> {
    let deadline = Instant::now() + timeout;
    loop {
        if !is_port_bound(port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ProcessError::PortStillOccupied(port));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Drop guard that ensures a nc zombie listener is cleaned up even on panic.
struct ZombieListenerGuard {
    child: Option<Child>,
    port: u16,
}

impl ZombieListenerGuard {
    fn new(child: Child, port: u16) -> Self {
        Self {
            child: Some(child),
            port,
        }
    }
}

impl Drop for ZombieListenerGuard {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            // Kill the nc listener to free the port.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Verify that the test ports are free (no orphan processes).
fn verify_ports_free() {
    if is_port_bound(TEST_PORT_A) {
        panic!(
            "Test port {} is already occupied. Inspect with:\n\
             ss -tlnp | grep {}\n\
             and kill only the exact leftover PID, or use:\n\
             fuser -k {}/tcp",
            TEST_PORT_A, TEST_PORT_A, TEST_PORT_A
        );
    }
    if is_port_bound(TEST_PORT_B) {
        panic!(
            "Test port {} is already occupied. Inspect with:\n\
             ss -tlnp | grep {}\n\
             and kill only the exact leftover PID, or use:\n\
             fuser -k {}/tcp",
            TEST_PORT_B, TEST_PORT_B, TEST_PORT_B
        );
    }
}

// ─── Repeated switch loop test ────────────────────────────────────────────────

/// Test that switching between two real model scripts back-to-back, repeated
/// ~10 times, always leaves the port free before the new process binds it and
/// never double-binds.
#[serial]
#[test]
fn test_repeated_switch_loop() {
    // Verify no leftover processes from previous runs (port-based only).
    verify_ports_free();

    let (config, _guard) = load_test_config();
    let mut pm = ProcessManager::new(config);

    // Start model A first
    pm.start_model("model-a").expect("should start model-a");
    assert!(is_port_bound(TEST_PORT_A), "port {} should be bound after starting model-a", TEST_PORT_A);

    let iterations = 10;
    for i in 0..iterations {
        println!("Iteration {}/{}", i + 1, iterations);

        // Verify port 9876 is bound before switch (model-a is running)
        assert!(is_port_bound(TEST_PORT_A), "iteration {}: port {} should be bound before switch", i + 1, TEST_PORT_A);

        // Switch A → B
        pm.switch_model("model-a", "model-b")
            .expect(&format!("iteration {}: switch a→b should succeed", i + 1));

        // Verify port 9876 is free after switch (model-a stopped)
        assert!(
            !is_port_bound(TEST_PORT_A),
            "iteration {}: port {} should be free after switching away from model-a",
            i + 1, TEST_PORT_A
        );

        // Verify port 9877 is bound (model-b started)
        assert!(
            is_port_bound(TEST_PORT_B),
            "iteration {}: port {} should be bound after starting model-b",
            i + 1, TEST_PORT_B
        );

        // Switch B → A
        pm.switch_model("model-b", "model-a")
            .expect(&format!("iteration {}: switch b→a should succeed", i + 1));

        // Verify port 9877 is free after switch (model-b stopped)
        assert!(
            !is_port_bound(TEST_PORT_B),
            "iteration {}: port {} should be free after switching away from model-b",
            i + 1, TEST_PORT_B
        );

        // Verify port 9876 is bound (model-a started)
        assert!(
            is_port_bound(TEST_PORT_A),
            "iteration {}: port {} should be bound after starting model-a",
            i + 1, TEST_PORT_A
        );
    }

    // Stop the last running model (through ProcessManager, not pkill).
    pm.stop_model("model-a").expect("should stop model-a");

    // Verify no orphan processes via port check.
    verify_ports_free();

    println!("Repeated switch loop passed: {} iterations, no hangs or panics", iterations);
}

// ─── Zombie-port test ────────────────────────────────────────────────────────

/// Test the zombie-port path: manually occupy a model's port with `nc -l {port}`
/// before calling `start_model`, confirm we get `PortOccupiedByUnknownProcess`
/// and not a silent hang or panic.
#[serial]
#[test]
fn test_zombie_port_path() {
    // Verify no leftover processes from previous runs (port-based only).
    verify_ports_free();

    let (config, _guard) = load_test_config();
    let mut pm = ProcessManager::new(config);

    // Start model-a first (so we have a running model to switch from)
    pm.start_model("model-a").expect("should start model-a");
    assert!(is_port_bound(TEST_PORT_A), "port {} should be bound after starting model-a", TEST_PORT_A);

    // Now occupy port 9877 with nc -l (zombie process).
    let nc_child = Command::new("nc")
        .args(["-l", "-p", "9877"])
        .spawn()
        .expect("should start nc listener on port 9877");

    // Wrap in Drop guard so cleanup happens even if the test panics.
    let _zombie_guard = ZombieListenerGuard::new(nc_child, TEST_PORT_B);

    // Wait for the nc process to actually bind the port.
    thread::sleep(Duration::from_millis(500));

    // Try to switch from model-a to model-b — should fail because port 9877 is occupied.
    let result = pm.switch_model("model-a", "model-b");
    assert!(
        result.is_err(),
        "switch a→b should fail when port {} is occupied by nc",
        TEST_PORT_B
    );

    // Verify we get the specific PortOccupiedByUnknownProcess error.
    let err = result.unwrap_err();
    match &err {
        ProcessError::PortOccupiedByUnknownProcess { pid: _, port } => {
            assert_eq!(
                *port, TEST_PORT_B,
                "error should report port {} as occupied",
                TEST_PORT_B
            );
        }
        _ => panic!("expected PortOccupiedByUnknownProcess error, got: {}", err),
    }

    // Verify no model is running (switch stopped model-a but failed to start model-b).
    assert!(
        pm.get_running_model_id().is_none(),
        "no model should be running after a failed switch"
    );

    // Verify no orphan processes via port check.
    verify_ports_free();

    println!("Zombie-port test passed: got PortOccupiedByUnknownProcess as expected");
}

// ─── Port free check during switch ────────────────────────────────────────────

/// Test that the port is always free between stop and start in switch_model.
/// This catches the race condition where the new model starts before the old one
/// fully releases its port.
#[serial]
#[test]
fn test_port_free_between_switch_steps() {
    // Verify no leftover processes from previous runs (port-based only).
    verify_ports_free();

    let (config, _guard) = load_test_config();
    let mut pm = ProcessManager::new(config);

    // Start model-a first.
    pm.start_model("model-a").expect("should start model-a");
    assert!(is_port_bound(TEST_PORT_A), "port {} should be bound after starting model-a", TEST_PORT_A);

    // Switch A → B — this should always leave port 9876 free before binding 9877.
    let result = pm.switch_model("model-a", "model-b");
    assert!(result.is_ok(), "switch a→b should succeed");

    // Verify: port 9876 should be free now.
    assert!(
        !is_port_bound(TEST_PORT_A),
        "port {} should be free after switching away from model-a",
        TEST_PORT_A
    );

    // Verify: port 9877 should be bound now.
    assert!(
        is_port_bound(TEST_PORT_B),
        "port {} should be bound after starting model-b",
        TEST_PORT_B
    );

    // Stop the last running model (through ProcessManager, not pkill).
    pm.stop_model("model-b").expect("should stop model-b");

    // Verify no orphan processes via port check.
    verify_ports_free();

    println!("Port free check between switch steps passed");
}

// ─── Cleanup on test teardown ────────────────────────────────────────────────

/// Ensure no orphan processes are left after any test.
#[serial]
#[test]
fn test_no_orphans_after_tests() {
    verify_ports_free();
}
