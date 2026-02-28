mod controllers;
mod git_actor;
mod models;
mod state;

use crate::state::PostOffice;
use axum::{
    body::Bytes,
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "allman=info,tower_http=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting allman (The MCP Agent Mail Server)");

    // Load configuration
    dotenv::dotenv().ok();
    let index_path_str = std::env::var("INDEX_PATH").unwrap_or_else(|_| "allman_index".to_string());
    let index_path = std::path::Path::new(&index_path_str);
    let repo_root_str = std::env::var("REPO_ROOT").unwrap_or_else(|_| "allman_repo".to_string());
    let repo_root = std::path::Path::new(&repo_root_str);

    let state = PostOffice::new(index_path, repo_root)?;

    // Build the application router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/mcp", post(mcp_handler))
        .layer(Extension(state.clone()))
        .layer(DefaultBodyLimit::max(1_048_576)); // 1 MB request body limit

    // Address to bind to
    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Drain the persist pipeline before exiting — shutdown() drops the channel
    // sender, then joins the persist worker thread to ensure all pending Tantivy
    // batches are committed before the process exits (DR34-H3).
    tracing::info!("Server stopped, draining persist pipeline...");
    state.shutdown();
    tracing::info!("Shutdown complete");

    Ok(())
}

async fn shutdown_signal() {
    // Handle both SIGINT (Ctrl-C) and SIGTERM (Docker/Kubernetes/systemd).
    // Previously only SIGINT was handled — SIGTERM bypassed the graceful
    // shutdown path, potentially losing pending Tantivy batches and git
    // commits (DR35-H2).
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received SIGINT (Ctrl-C)");
            }
            _ = sigterm.recv() => {
                tracing::info!("Received SIGTERM");
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install CTRL+C handler");
        tracing::info!("Received shutdown signal");
    }
}

async fn health_check() -> &'static str {
    "OK"
}

// JSON-RPC 2.0 handler. Parses raw bytes to return -32700 on malformed JSON
// instead of Axum's default HTTP 422 (which violates the JSON-RPC spec).
async fn mcp_handler(Extension(state): Extension<PostOffice>, body: Bytes) -> Response {
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "Parse error" }
            }))
            .into_response();
        }
    };
    let method = payload
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let tool = payload
        .pointer("/params/name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    tracing::debug!(method, tool, "MCP request");
    let response = controllers::handle_mcp_request(state, payload).await;

    // JSON-RPC 2.0: notifications (no id) return Value::Null — respond with 204.
    if response.is_null() {
        return StatusCode::NO_CONTENT.into_response();
    }

    Json(response).into_response()
}
