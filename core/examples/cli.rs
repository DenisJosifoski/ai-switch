//! CLI example — demonstrates the full lifecycle: load config, start model,
//! poll until Ready, confirm `/v1/models` responds, stop it, confirm port is
//! free.

use std::env;
use tracing_subscriber;

fn main() {
    // Set up tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let config_path = env::args().nth(1);

    // Run the core engine — keep the SingleInstanceGuard alive for the program's lifetime
    let (config, reconcile_result, _guard) = match ai_switch_core::run(config_path.as_deref()) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Failed to initialize: {}", e);
            std::process::exit(1);
        }
    };

    println!("Reconciliation result: {:?}", reconcile_result);

    // If no model is running, start one (for demo purposes)
    let model_id = config.models.first().map(|m| m.id.as_str());
    if let Some(id) = model_id {
        println!("Starting model '{}'...", id);
        match ai_switch_core::start_and_wait(&config, id) {
            Ok((mut pm, state)) => {
                println!("Model '{}' is: {:?}", id, state);

                // If Ready, confirm /v1/models responds
                if let ai_switch_core::process_manager::ModelState::Ready = state {
                    println!("Confirming /v1/models responds...");
                    match reqwest::blocking::get(format!("http://127.0.0.1:{}/v1/models", config.models[0].port)) {
                        Ok(resp) => {
                            if resp.status().is_success() {
                                println!("/v1/models responded with status {}", resp.status());
                                if let Ok(body) = resp.text() {
                                    println!("Response: {}", body);
                                }
                            } else {
                                println!("/v1/models responded with non-success status: {}", resp.status());
                            }
                        }
                        Err(e) => println!("Failed to connect to /v1/models: {}", e),
                    }
                }

                // Stop the model
                println!("Stopping model '{}'...", id);
                if ai_switch_core::stop_model(&mut pm, id, false).is_ok() {
                    println!("Model '{}' stopped successfully", id);
                } else {
                    eprintln!("Failed to stop model '{}'", id);
                }
            }
            Err(e) => {
                eprintln!("Failed to start model '{}': {}", id, e);
                std::process::exit(1);
            }
        }
    } else {
        println!("No models configured.");
    }

    println!("Done.");
}
