//! HTTP transport for the HEM modelling service.
//!
//! Thin Axum wrapper over `hem-api`: it maps HTTP requests onto `hem_api::simulate` and translates
//! `ApiError` into status codes. All modelling logic lives in `hem-api` so this transport and the
//! `hem-lambda` transport can share it (design doc §5.2).

use axum::{
    extract::Json,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use hem_api::{
    compare, list_weather, simulate, ApiError, CompareRequest, CompareResponse, SimulateRequest,
    SimulateResponse,
};
use serde_json::json;
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/archetypes", get(list_archetypes))
        .route("/weather", get(list_weather_datasets))
        .route("/simulate", post(run_simulation))
        .route("/compare", post(run_comparison));

    let addr: SocketAddr = std::env::var("HEM_SERVER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()
        .expect("HEM_SERVER_ADDR must be a valid socket address");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!("HEM modelling service listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn healthz() -> &'static str {
    "ok"
}

/// `GET /archetypes` — list available dwelling archetypes.
async fn list_archetypes() -> Json<serde_json::Value> {
    Json(json!({ "archetypes": hem_profiles::list() }))
}

/// `GET /weather` — list available weather datasets selectable via a request's `weather` field.
async fn list_weather_datasets() -> Json<serde_json::Value> {
    Json(json!({ "weather": list_weather() }))
}

/// `POST /simulate` — assemble an input from an archetype + glazing overrides, run HEM, return the
/// summary. The engine run is CPU-bound and synchronous, so it runs on the blocking pool to avoid
/// stalling the async runtime.
async fn run_simulation(Json(req): Json<SimulateRequest>) -> Result<Json<SimulateResponse>, ApiErrorResponse> {
    let result = tokio::task::spawn_blocking(move || simulate(&req))
        .await
        .map_err(|e| ApiErrorResponse::internal(format!("simulation task failed: {e}")))?;
    Ok(Json(result?))
}

/// `POST /compare` — run an archetype with baseline vs upgraded glazing and return both summaries
/// plus the headline reductions.
async fn run_comparison(
    Json(req): Json<CompareRequest>,
) -> Result<Json<CompareResponse>, ApiErrorResponse> {
    let result = tokio::task::spawn_blocking(move || compare(&req))
        .await
        .map_err(|e| ApiErrorResponse::internal(format!("comparison task failed: {e}")))?;
    Ok(Json(result?))
}

/// Wraps `ApiError` (and internal failures) so it can be returned as an HTTP response with an
/// appropriate status code and a JSON error body.
struct ApiErrorResponse {
    status: StatusCode,
    message: String,
}

impl ApiErrorResponse {
    fn internal(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
}

impl From<ApiError> for ApiErrorResponse {
    fn from(err: ApiError) -> Self {
        let status = match &err {
            ApiError::UnknownArchetype(_) | ApiError::UnknownWeather(_) => StatusCode::NOT_FOUND,
            ApiError::InvalidInput(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Weather(_) | ApiError::Calculation(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
