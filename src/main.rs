mod controllers;
mod git_actor;
mod models;
mod state;

use crate::state::PostOffice;
use axum::{
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
            std::env::var("RUST_LOG").unwrap_or_else(|_| "allman=debug,tower_http=debug".into()),
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
        .layer(Extension(state));

    // Address to bind to
    let addr = SocketAddr::from(([0, 0, 0, 0], 8000));
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check() -> &'static str {
    "OK"
}

// Basic placeholder for the MCP JSON-RPC handler
async fn mcp_handler(
    Extension(state): Extension<PostOffice>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    tracing::debug!("Received MCP payload: {:?}", payload);
    let response = controllers::handle_mcp_request(state, payload).await;
    Json(response)
}
