//! API de controle (somente loopback): listar janelas, iniciar, parar e consultar o estado.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::rtc::broadcaster::Broadcaster;
use crate::server::{AppState, LiveSession, ViewerKind, signaling};
use crate::turn;
use crate::windows_list::{WindowInfo, list_visible_windows};

const CONTROL_HTML: &str = include_str!("../../web/control.html");

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/control", get(control_page))
        .route("/api/windows", get(windows))
        .route("/api/start", post(start))
        .route("/api/stop", post(stop))
        .route("/api/status", get(status))
        .route("/api/preview", get(preview_socket))
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
    pub hwnd: Option<u64>,
    pub uptime_secs: Option<u64>,
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

async fn preview_socket(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| signaling::handle_viewer(socket, state, ViewerKind::Preview))
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
    // Checagem rápida antes da busca de TURN, para não gastar a chamada à Cloudflare à toa.
    let busy = state
        .live
        .lock()
        .expect("live poisoned")
        .as_ref()
        .is_some_and(|session| !session.broadcaster.ended());
    if busy {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "já existe uma transmissão ativa",
        ));
    }
    // A busca é assíncrona: feita antes do lock, que é std e não pode atravessar um await.
    let ice = turn::initial_ice_servers(&state.config).await;

    let mut live = state.live.lock().expect("live poisoned");
    if live.as_ref().is_some_and(|session| !session.broadcaster.ended()) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "já existe uma transmissão ativa",
        ));
    }
    // Uma transmissão encerrada pela fonte (janela fechada) é substituída.
    if let Some(old) = live.take() {
        let _ = old.stop.send(true);
        old.broadcaster.stop();
    }

    let handle = tokio::runtime::Handle::current();
    let bitrate = state.config.video_bitrate_bps();
    let (broadcaster, source, hwnd) = if request.test {
        let broadcaster = Broadcaster::start_test_source(handle, bitrate)
            .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
        (broadcaster, "test", None)
    } else if let Some(hwnd) = request.hwnd {
        let broadcaster = Broadcaster::start_window(handle, hwnd, bitrate).map_err(|err| {
            warn!(error = %format!("{err:#}"), "não consegui iniciar a captura");
            ApiError::new(StatusCode::BAD_REQUEST, format!("{err:#}"))
        })?;
        (broadcaster, "window", Some(hwnd))
    } else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "escolha uma janela (hwnd) ou use \"test\": true",
        ));
    };

    let (stop, _) = watch::channel(false);
    let ice_servers = Arc::new(Mutex::new(ice));
    turn::spawn_refresh(state.config.clone(), ice_servers.clone(), stop.subscribe());
    *live = Some(LiveSession {
        broadcaster,
        source,
        hwnd,
        started_at: std::time::Instant::now(),
        stop,
        ice_servers,
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
    let mut notices = state.notices.clone();
    let (status, source, hwnd, uptime_secs, link) = match live.as_ref() {
        Some(session) if session.broadcaster.ended() => {
            // A fonte encerrou (ex.: janela fechada): derruba os espectadores uma vez, sem remover a sessão.
            let _ = session.stop.send_replace(true);
            notices.extend(session.broadcaster.notices());
            ("ended", Some(session.source), None, None, None)
        }
        Some(session) => {
            notices.extend(session.broadcaster.notices());
            let uptime = session.started_at.elapsed().as_secs();
            ("live", Some(session.source), session.hwnd, Some(uptime), Some(state.viewer_link()))
        }
        None => ("idle", None, None, None, None),
    };
    StatusBody {
        state: status,
        source,
        hwnd,
        uptime_secs,
        viewers: state.viewers.load(Ordering::SeqCst),
        max_viewers: state.config.max_viewers,
        link,
        tunnel: match state.config.tunnel {
            crate::config::TunnelMode::External => "external",
            crate::config::TunnelMode::Managed => "managed",
            crate::config::TunnelMode::None => "none",
        },
        notices,
    }
}
