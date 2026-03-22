use axum::{
    extract::{State, WebSocketUpgrade},
    extract::ws,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

use crate::bot::BotManager;
use crate::config::token_info_list;

// ─── App State ───────────────────────────────────────────────────────────────

pub struct AppState {
    pub bot: BotManager,
    pub api_key: String,
}

// ─── Request / Response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub token: String,
    pub order_size_usd: Option<f64>,
    pub leverage: Option<f64>,
    pub time_limit_secs: Option<u64>,
}

#[derive(Serialize)]
struct ApiOk<T: Serialize> {
    success: bool,
    data: T,
}

#[derive(Serialize)]
struct ApiErr {
    success: bool,
    error: String,
}

fn ok<T: Serialize>(data: T) -> Json<ApiOk<T>> {
    Json(ApiOk { success: true, data })
}

fn err(msg: impl Into<String>) -> (StatusCode, Json<ApiErr>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiErr { success: false, error: msg.into() }),
    )
}

// ─── Auth helper ──────────────────────────────────────────────────────────────

fn check_auth(headers: &HeaderMap, api_key: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|token| token == api_key)
        .unwrap_or(false)
}

// ─── Router ───────────────────────────────────────────────────────────────────

pub fn create_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/tokens", get(get_tokens))
        .route("/api/status", get(get_status))
        .route("/api/start", post(start_bot))
        .route("/api/stop", post(stop_bot))
        .route("/api/sessions", get(get_sessions))
        .route("/api/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn get_tokens() -> impl IntoResponse {
    ok(token_info_list())
}

async fn get_status(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    if !check_auth(&headers, &state.api_key) {
        return (StatusCode::UNAUTHORIZED, Json(ApiErr { success: false, error: "Unauthorized".into() })).into_response();
    }
    ok(state.bot.status()).into_response()
}

async fn start_bot(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartRequest>,
) -> Response {
    if !check_auth(&headers, &state.api_key) {
        return (StatusCode::UNAUTHORIZED, Json(ApiErr { success: false, error: "Unauthorized".into() })).into_response();
    }

    match state.bot.start(&req.token, req.order_size_usd, req.leverage, req.time_limit_secs).await {
        Ok(()) => ok(serde_json::json!({ "started": true, "token": req.token })).into_response(),
        Err(e) => err(e.to_string()).into_response(),
    }
}

async fn stop_bot(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    if !check_auth(&headers, &state.api_key) {
        return (StatusCode::UNAUTHORIZED, Json(ApiErr { success: false, error: "Unauthorized".into() })).into_response();
    }

    match state.bot.stop() {
        Ok(()) => ok(serde_json::json!({ "stopped": true })).into_response(),
        Err(e) => err(e.to_string()).into_response(),
    }
}

async fn get_sessions(
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    if !check_auth(&headers, &state.api_key) {
        return (StatusCode::UNAUTHORIZED, Json(ApiErr { success: false, error: "Unauthorized".into() })).into_response();
    }
    ok(state.bot.sessions()).into_response()
}

// ─── WebSocket streaming ──────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    // Auth via query param or header (WS clients can't easily set headers)
    let authed = check_auth(&headers, &state.api_key);
    if !authed {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: ws::WebSocket, state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;
        let snapshot = state.bot.status();
        let msg = serde_json::to_string(&snapshot).unwrap_or_default();
        if socket.send(ws::Message::Text(msg.into())).await.is_err() {
            break;
        }
    }
}
