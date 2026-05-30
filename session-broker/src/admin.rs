use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

use crate::session_registry::RegistryData;
use crate::AppState;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(get_sessions))
        .with_state(state)
}

pub async fn serve_admin(state: AppState, addr: &str) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to bind admin HTTP server on {addr}: {e}"))?;

    let app = build_router(state);

    axum::serve(listener, app)
        .await
        .map_err(|e| format!("admin HTTP server error: {e}"))
}

async fn get_sessions(State(state): State<AppState>) -> Json<RegistryData> {
    Json(state.session_registry.read().await)
}
