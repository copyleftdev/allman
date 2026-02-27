mod controllers;
mod git_actor;
mod models;
mod state;

use crate::state::PostOffice;
use axum::{
    extract::DefaultBodyLimit,
    response::IntoResponse,
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::Value;
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

    // Drain the persist pipeline before exiting
    tracing::info!("Server stopped, draining persist pipeline...");
    drop(state);
    tracing::info!("Shutdown complete");

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C handler");
    tracing::info!("Received shutdown signal");
}

async fn health_check() -> &'static str {
    "OK"
}

// Basic placeholder for the MCP JSON-RPC handler
async fn mcp_handler(
    Extension(state): Extension<PostOffice>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
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
    Json(response)
}
