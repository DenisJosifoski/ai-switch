//! Phase 5 — Reverse proxy server.
//!
//! A transparent local HTTP reverse proxy listening on `127.0.0.1:proxy_port`
//! (default 9080). It inspects the active model state dynamically and forwards
//! all incoming API requests to whichever model is currently Ready in the
//! ProcessManager.
//!
//! - Model Ready → forward to `http://127.0.0.1:{active_model_port}`
//! - No model / Error → HTTP 503 with JSON error body
//! - Loading (starting/restarting) → HTTP 503 with "currently starting" message
//!
//! The proxy runs on a background thread and never blocks the GTK main loop.
//! It binds to `127.0.0.1` only — never `0.0.0.0`.

use std::io::{Read, Result as IoResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Request, Response, Server};
use tracing::{debug, error, info, warn};

/// Shared state between the proxy server and the application.
///
/// Updated by the app whenever a model starts, stops, switches, or restarts.
/// The proxy reads this state on every incoming request to decide where to
/// forward (or whether to return 503).
#[derive(Debug, Clone)]
pub struct ProxyState {
    /// The port of the currently active model server, if any.
    /// `None` means no model is running.
    pub target_port: Option<u16>,

    /// Whether a model is currently in a transitional state (starting / restarting).
    /// When `true`, the proxy returns 503 even if `target_port` is set, because
    /// the model on that port is not yet Ready to serve requests.
    pub is_loading: bool,
}

impl Default for ProxyState {
    fn default() -> Self {
        Self {
            target_port: None,
            is_loading: false,
        }
    }
}

impl ProxyState {
    /// Create a new proxy state with no active model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the target port and mark the model as loaded (Ready).
    pub fn set_target(&mut self, port: u16) {
        self.target_port = Some(port);
        self.is_loading = false;
    }

    /// Mark the proxy as loading (model is starting/restarting).
    pub fn set_loading(&mut self) {
        self.is_loading = true;
    }

    /// Clear the target port and mark as not loading.
    pub fn clear(&mut self) {
        self.target_port = None;
        self.is_loading = false;
    }
}

/// A reverse proxy server that forwards requests to the active model.
///
/// Runs on a background std::thread (not the GTK main loop). The proxy reads
/// the shared `ProxyState` on every request to determine the forwarding target.
pub struct ProxyServer {
    shutdown_flag: Arc<AtomicBool>,
    stop_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

impl ProxyServer {
    /// Create and start the proxy server on the given port with the provided state.
    ///
    /// Returns `Ok(Self)` if the server started successfully, or an error string
    /// if binding failed (e.g., port already in use).
    pub fn new(proxy_port: u16, state: Arc<Mutex<ProxyState>>) -> Result<Self, String> {
        let addr = format!("127.0.0.1:{}", proxy_port);

        let server = Server::http(&addr)
            .map_err(|e| format!("failed to bind proxy server to {}: {}", addr, e))?;

        // Graceful shutdown via oneshot channel
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = Arc::clone(&shutdown_flag);
        let state_for_proxy = Arc::clone(&state);

        std::thread::spawn(move || {
            info!(
                "reverse proxy started on http://127.0.0.1:{}",
                proxy_port
            );

            for req in server.incoming_requests() {
                // Check shutdown signal first
                if stop_rx.try_recv().is_ok() || shutdown_flag_clone.load(Ordering::Relaxed) {
                    break;
                }

                let state = Arc::clone(&state_for_proxy);
                handle_proxy_request(req, state);
            }

            info!("reverse proxy stopped");
        });

        Ok(Self {
            shutdown_flag,
            stop_tx: Mutex::new(Some(stop_tx)),
        })
    }

    /// Gracefully shut down the proxy server.
    pub fn stop(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Handle an incoming proxy request by inspecting state and forwarding.
fn handle_proxy_request(mut req: Request, state: Arc<Mutex<ProxyState>>) {
    let proxy_state = match state.lock() {
        Ok(s) => s,
        Err(e) => {
            error!("proxy state lock poisoned: {}", e);
            return;
        }
    };

    // No active model → 503 Service Unavailable
    if proxy_state.target_port.is_none() {
        drop(proxy_state);
        let _ = req.respond(error_response(
            503,
            "No active model server in ai-switch",
        ));
        return;
    }

    // Model is currently starting / restarting → 503
    if proxy_state.is_loading {
        drop(proxy_state);
        let _ = req.respond(error_response(
            503,
            "Model server is currently starting/restarting",
        ));
        return;
    }

    let target_port = proxy_state.target_port.unwrap();
    drop(proxy_state);

    // Forward to the active model server
    let target_base = format!("http://127.0.0.1:{}", target_port);
    let target_url = format!("{}{}", target_base, req.url());

    // Build headers for the forwarded request
    let mut forward_headers = Vec::new();
    for header in req.headers() {
        let field_bytes = header.field.as_str().as_bytes();
        forward_headers.push(
            Header::from_bytes(field_bytes, header.value.as_bytes())
                .unwrap_or_else(|_| {
                    Header::from_bytes(field_bytes, b"")
                        .expect("header construction should never fail")
                }),
        );
    }

    // Read the request body
    let mut request_body = Vec::new();
    let reader = req.as_reader();
    let _ = reader.read_to_end(&mut request_body);

    // Convert tiny_http method to reqwest method
    let method = match req.method().as_str() {
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        _ => reqwest::Method::GET,
    };

    // Send the request to the model server via reqwest (blocking client)
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to build reqwest client: {}", e);
            let _ = req.respond(error_response(503, "Proxy client error"));
            return;
        }
    };

    let mut request_builder =
        client.request(method, &target_url);

    for header in &forward_headers {
        let field_name = std::str::from_utf8(header.field.as_str().as_bytes()).unwrap_or("");
        request_builder = request_builder.header(field_name, header.value.as_str());
    }

    if !request_body.is_empty() {
        request_builder = request_builder.body(request_body.clone());
    }

    let response = match request_builder.send() {
        Ok(resp) => resp,
        Err(e) => {
            debug!("forward failed to model server {}: {}", target_port, e);
            let _ = req.respond(error_response(503, "Model server unavailable"));
            return;
        }
    };

    let status = response.status().as_u16();
    let resp_headers = response.headers();

    // Build response headers for tiny_http
    let mut response_headers = Vec::new();
    for (name, value) in resp_headers.iter() {
        response_headers.push(
            Header::from_bytes(name.as_str(), value.as_bytes())
                .unwrap_or_else(|_| {
                    Header::from_bytes(name.as_str(), b"")
                        .expect("header construction should never fail")
                }),
        );
    }

    // Add content-length header if available
    if let Some(content_length) = response.content_length() {
        response_headers.push(
            Header::from_bytes("content-length", content_length.to_string().as_bytes())
                .unwrap_or_else(|_| {
                    Header::from_bytes("content-length", b"0")
                        .expect("header construction should never fail")
                }),
        );
    }

    // Create a streaming response that yields chunks as they arrive from the
    // backend. This preserves SSE streaming for chat completions — tokens are
    // sent to the client as soon as they're generated, rather than waiting for
    // the entire response to complete.
    let streaming_body = StreamingBody {
        reader: response,
    };

    let tiny_response = Response::new(
        tiny_http::StatusCode(status),
        response_headers,
        Box::new(streaming_body),
        None, // data_length computed from Read trait
        None, // additional_headers
    );

    if let Err(e) = req.respond(tiny_response) {
        debug!("failed to respond to proxy client: {}", e);
    }
}

/// A streaming body reader that reads chunks from a backend response.
///
/// Used to preserve SSE streaming for chat completions — the proxy pipes
/// the response body through as it arrives rather than buffering the full
/// response before forwarding.
struct StreamingBody {
    reader: reqwest::blocking::Response,
}

impl Read for StreamingBody {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        self.reader.read(buf)
    }
}

/// Build an error response with a JSON body.
fn error_response(status: u16, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = format!("{{\"error\": \"{}\"}}", message);
    Response::from_data(body.into_bytes())
        .with_status_code(tiny_http::StatusCode(status))
        .with_header(
            Header::from_bytes("content-type", b"application/json")
                .unwrap_or_else(|_| {
                    Header::from_bytes("content-type", b"application/json")
                        .expect("should never fail")
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_state_default() {
        let state = ProxyState::default();
        assert!(state.target_port.is_none());
        assert!(!state.is_loading);
    }

    #[test]
    fn test_proxy_state_set_target() {
        let mut state = ProxyState::new();
        state.set_target(8081);
        assert_eq!(state.target_port, Some(8081));
        assert!(!state.is_loading);
    }

    #[test]
    fn test_proxy_state_set_loading() {
        let mut state = ProxyState::new();
        state.set_target(8081);
        state.set_loading();
        assert_eq!(state.target_port, Some(8081));
        assert!(state.is_loading);
    }

    #[test]
    fn test_proxy_state_clear() {
        let mut state = ProxyState::new();
        state.set_target(8081);
        state.clear();
        assert!(state.target_port.is_none());
        assert!(!state.is_loading);
    }

    #[test]
    fn test_proxy_state_lifecycle() {
        let mut state = ProxyState::new();

        // Initial: no model
        assert!(state.target_port.is_none());
        assert!(!state.is_loading);

        // Model starting
        state.set_loading();
        assert!(state.is_loading);

        // Model ready
        state.set_target(9081);
        assert_eq!(state.target_port, Some(9081));
        assert!(!state.is_loading);

        // Model stopped
        state.clear();
        assert!(state.target_port.is_none());
        assert!(!state.is_loading);

        // Switch to another model
        state.set_loading();
        state.set_target(9082);
        assert_eq!(state.target_port, Some(9082));
        assert!(!state.is_loading);
    }
}
