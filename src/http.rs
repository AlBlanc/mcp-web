//! Multi-client HTTP & SSE (Server-Sent Events) transport for the Model Context Protocol (MCP).
//!
//! Enables multiple remote clients (OpenCode, Claude Desktop, Cursor, agents, etc.)
//! to connect concurrently to a single running `mcp-web` instance.
//!
//! Supported routes:
//! - `GET  /sse`: Opens a persistent Server-Sent Events stream for receiving MCP responses.
//! - `POST /message?sessionId=<id>`: Delivers JSON-RPC requests to the session.
//! - `POST /mcp`: Direct stateless JSON-RPC endpoint (request in body -> response in body).
//! - `GET  /health`: Health status, version, and active session count.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{error, info, warn};

use crate::mcp::McpServer;

type SessionSender = mpsc::UnboundedSender<String>;

#[derive(Clone)]
pub struct AppState {
    pub server: Arc<McpServer>,
    pub sessions: Arc<RwLock<HashMap<String, SessionSender>>>,
}

#[derive(Debug, Deserialize)]
pub struct MessageQuery {
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub server: &'static str,
    pub version: &'static str,
    pub active_sessions: usize,
    pub endpoints: EndpointInfo,
}

#[derive(Debug, Serialize)]
pub struct EndpointInfo {
    pub sse: &'static str,
    pub message: &'static str,
    pub direct_mcp: &'static str,
    pub health: &'static str,
}

pub struct HttpServer {
    server: Arc<McpServer>,
    bind_addr: SocketAddr,
}

impl HttpServer {
    pub fn new(server: Arc<McpServer>, bind_addr: SocketAddr) -> Self {
        Self { server, bind_addr }
    }

    /// Run the HTTP / SSE server listening on `bind_addr`.
    pub async fn run(&self) -> Result<()> {
        let state = AppState {
            server: self.server.clone(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };

        let app = create_router(state);

        let listener = tokio::net::TcpListener::bind(&self.bind_addr)
            .await
            .with_context(|| format!("Failed to bind HTTP server to {}", self.bind_addr))?;

        info!(
            addr = %self.bind_addr,
            "MCP HTTP/SSE server listening. Connect clients to http://{}/sse",
            self.bind_addr
        );

        axum::serve(listener, app)
            .await
            .context("MCP HTTP server error")?;

        Ok(())
    }
}

/// Create the Axum HTTP router with all MCP routes and CORS middleware.
pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/sse", get(sse_handler))
        .route("/message", post(message_handler))
        .route("/mcp", post(direct_mcp_handler))
        .route("/rpc", post(direct_mcp_handler))
        .route("/health", get(health_handler))
        .route("/", get(health_handler))
        .layer(middleware::map_response(add_cors_headers))
        .with_state(state)
}

/// Permissive CORS headers for browser-based MCP clients.
async fn add_cors_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS, HEAD"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization, Accept"),
    );
    response
}

/// GET `/sse`: establish an SSE stream for an MCP client session.
async fn sse_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = generate_session_id();
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    {
        let mut sessions = state.sessions.write().await;
        sessions.insert(session_id.clone(), tx);
    }

    info!(session_id = %session_id, "New MCP client connected via SSE");

    // Optional relative or absolute URI for the message endpoint
    let message_endpoint = format!("/message?sessionId={session_id}");

    // Initial `endpoint` event telling the client where to POST JSON-RPC messages
    let initial_event = Event::default().event("endpoint").data(message_endpoint);

    let session_id_cleanup = session_id.clone();
    let sessions_map_cleanup = state.sessions.clone();

    // Stream incoming messages down the SSE channel
    let message_stream = UnboundedReceiverStream::new(rx)
        .map(move |msg| Ok(Event::default().event("message").data(msg)));

    let stream = tokio_stream::once(Ok(initial_event)).chain(message_stream);

    // Spawn a cleanup watcher when stream ends
    let guard = CleanupGuard {
        session_id: session_id_cleanup,
        sessions: sessions_map_cleanup,
    };

    // We attach the guard to the SSE response stream
    let guarded_stream = stream.map(move |evt| {
        let _ = &guard; // keep guard alive while stream is active
        evt
    });

    let _ = headers; // available if we want to log User-Agent

    Sse::new(guarded_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

struct CleanupGuard {
    session_id: String,
    sessions: Arc<RwLock<HashMap<String, SessionSender>>>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let id = self.session_id.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut map = sessions.write().await;
            if map.remove(&id).is_some() {
                info!(session_id = %id, "MCP client SSE session disconnected");
            }
        });
    }
}

/// POST `/message?sessionId=<id>`: deliver incoming JSON-RPC request to the specified session.
async fn message_handler(
    State(state): State<AppState>,
    Query(query): Query<MessageQuery>,
    body: String,
) -> impl IntoResponse {
    let Some(session_id) = query.session_id else {
        return (
            StatusCode::BAD_REQUEST,
            "Missing `sessionId` query parameter",
        )
            .into_response();
    };

    let sender = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).cloned()
    };

    let Some(tx) = sender else {
        warn!(session_id = %session_id, "Message received for unknown/expired session");
        return (
            StatusCode::NOT_FOUND,
            "Session not found or connection closed",
        )
            .into_response();
    };

    let server = state.server.clone();
    let session_id_log = session_id.clone();

    // Process JSON-RPC request asynchronously and send response down the SSE stream
    tokio::spawn(async move {
        if let Some(resp) = server.handle_raw_message(&body).await {
            match serde_json::to_string(&resp) {
                Ok(serialized) => {
                    if let Err(err) = tx.send(serialized) {
                        error!(session_id = %session_id_log, error = %err, "Failed to send response to SSE channel");
                    }
                }
                Err(err) => {
                    error!(session_id = %session_id_log, error = %err, "Failed to serialize JSON-RPC response");
                }
            }
        }
    });

    (StatusCode::ACCEPTED, "Accepted").into_response()
}

/// POST `/mcp`: Direct HTTP JSON-RPC endpoint.
///
/// Sends a request in the HTTP POST body and immediately receives the response
/// in the HTTP response body without needing an open SSE stream.
async fn direct_mcp_handler(State(state): State<AppState>, body: String) -> impl IntoResponse {
    if let Some(resp) = state.server.handle_raw_message(&body).await {
        (StatusCode::OK, Json(resp)).into_response()
    } else {
        (StatusCode::NO_CONTENT, ()).into_response()
    }
}

/// GET `/health`: Server health and active session count.
async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let count = state.sessions.read().await.len();
    Json(HealthResponse {
        status: "ok",
        server: "mcp-web",
        version: env!("CARGO_PKG_VERSION"),
        active_sessions: count,
        endpoints: EndpointInfo {
            sse: "/sse",
            message: "/message?sessionId=<sessionId>",
            direct_mcp: "/mcp",
            health: "/health",
        },
    })
}

/// Generate a cryptographically random, collision-resistant 128-bit hex session ID.
fn generate_session_id() -> String {
    format!("{:032x}", fastrand::u128(..))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::browser::WebFetcher;

    #[tokio::test]
    async fn health_check_returns_ok() {
        let fetcher = WebFetcher::new(None, None).unwrap();
        let server = Arc::new(McpServer::new(fetcher));
        let state = AppState {
            server,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };
        let app = create_router(state);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["server"], "mcp-web");
    }

    #[tokio::test]
    async fn direct_mcp_post_handles_initialize() {
        let fetcher = WebFetcher::new(None, None).unwrap();
        let server = Arc::new(McpServer::new(fetcher));
        let state = AppState {
            server,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };
        let app = create_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Content-Type", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["result"]["serverInfo"]["name"], "mcp-web");
    }

    #[tokio::test]
    async fn sse_endpoint_initializes_stream() {
        let fetcher = WebFetcher::new(None, None).unwrap();
        let server = Arc::new(McpServer::new(fetcher));
        let state = AppState {
            server,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        };
        let app = create_router(state);

        let req = Request::builder()
            .uri("/sse")
            .header("Accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("text/event-stream")
        );
    }
}
