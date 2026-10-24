//! API de controle (somente loopback): listar janelas, iniciar, parar e consultar o estado.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::capture::video;
use crate::rtc::broadcaster::Broadcaster;
use crate::server::{AppState, LiveSession};
use crate::windows_list::{WindowInfo, list_visible_windows};

const CONTROL_HTML: &str = include_str!("../../web/control.html");

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/control", get(control_page))
        .route("/api/windows", get(windows))
        .route("/api/start", post(start))
        .route("/api/stop", post(stop))
        .route("/api/status", get(status))
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    /// Usa a fonte sintética em vez de uma janela.
    #[serde(default)]
    pub test: bool,
    pub hwnd: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct StatusBody {
    pub state: &'static str,
    pub source: Option<&'static str>,
    pub viewers: usize,
    pub max_viewers: usize,
    pub link: Option<String>,
    pub tunnel: &'static str,
    pub notices: Vec<String>,
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

async fn control_page() -> Html<&'static str> {
    Html(CONTROL_HTML)
}

async fn windows() -> Result<Json<Vec<WindowInfo>>, ApiError> {
    let list = tokio::task::spawn_blocking(list_visible_windows)
        .await
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(list))
}

async fn start(
    State(state): State<Arc<AppState>>,
    Json(request): Json<StartRequest>,
) -> Result<Json<StatusBody>, ApiError> {
    let mut live = state.live.lock().expect("live poisoned");
    if live.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "já existe uma transmissão ativa",
        ));
    }

    let (broadcaster, source) = if request.test {
        let broadcaster = Broadcaster::start_test_source(tokio::runtime::Handle::current())
            .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
        (broadcaster, "test")
    } else if let Some(hwnd) = request.hwnd {
        // Captura de janela (fase 3) ainda não existe: responde 501 com o motivo.
        let reason = match video::start_window_capture(hwnd) {
            Ok(()) => "captura de janela indisponível".to_owned(),
            Err(err) => format!("{err:#}"),
        };
        return Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, reason));
    } else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "escolha uma janela (hwnd) ou use \"test\": true",
        ));
    };

    let (stop, _) = watch::channel(false);
    *live = Some(LiveSession {
        broadcaster,
        source,
        stop,
    });
    info!(source, "transmissão iniciada");
    drop(live);
    Ok(Json(snapshot(&state)))
}

async fn stop(State(state): State<Arc<AppState>>) -> Json<StatusBody> {
    if state.stop_live() {
        info!("transmissão encerrada");
    } else {
        warn!("pedido de parada sem transmissão ativa");
    }
    Json(snapshot(&state))
}

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusBody> {
    Json(snapshot(&state))
}

fn snapshot(state: &AppState) -> StatusBody {
    let live = state.live.lock().expect("live poisoned");
    let (status, source, link) = match live.as_ref() {
        Some(session) => ("live", Some(session.source), Some(state.viewer_link())),
        None => ("idle", None, None),
    };
    StatusBody {
        state: status,
        source,
        viewers: state.viewers.load(Ordering::SeqCst),
        max_viewers: state.config.max_viewers,
        link,
        tunnel: match state.config.tunnel {
            crate::config::TunnelMode::External => "external",
            crate::config::TunnelMode::Managed => "managed",
            crate::config::TunnelMode::None => "none",
        },
        notices: state.notices.clone(),
    }
}
