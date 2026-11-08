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
use crate::windows_list::{MonitorInfo, WindowInfo, list_monitors, list_visible_windows};

const CONTROL_HTML: &str = include_str!("../../web/control.html");

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/control", get(control_page))
        .route("/api/windows", get(windows))
        .route("/api/monitors", get(monitors))
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
    pub monitor: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ViewerStatusItem {
    pub id: String,
    pub connected_secs: u64,
    pub path: String,
    pub rtt_ms: u32,
    pub loss_pct: f32,
    pub fps: u32,
    pub kbps: u32,
    pub reconnects: u32,
    pub last_reason: String,
}

#[derive(Debug, Serialize)]
pub struct StatusBody {
    pub state: &'static str,
    pub source: Option<&'static str>,
    pub hwnd: Option<u64>,
    pub monitor: Option<u64>,
    pub uptime_secs: Option<u64>,
    /// Taxa-alvo atual do vídeo e pior perda entre os espectadores (só ao vivo).
    pub stream: Option<crate::rtc::bitrate::StreamHealth>,
    pub viewers: usize,
    pub max_viewers: usize,
    pub link: Option<String>,
    pub tunnel: &'static str,
    pub notices: Vec<String>,
    pub viewer_list: Vec<ViewerStatusItem>,
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
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer_addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // WebSocket não passa por CORS: sem esta checagem, qualquer site aberto neste
    // computador poderia abrir o preview (que não pede senha).
    if !local_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let client_ip = signaling::client_ip(&headers, peer_addr);
    ws.on_upgrade(move |socket| {
        signaling::handle_viewer(socket, state, ViewerKind::Preview, client_ip)
    })
}

/// Aceita só Origin `http(s)://` de host local. Sem Origin (cliente que não é navegador) também passa:
/// o `require_local` já garantiu que a conexão vem deste computador.
pub fn local_origin(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|host| !host.contains('/') && crate::server::is_local_host(host))
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

async fn monitors() -> Result<Json<Vec<MonitorInfo>>, ApiError> {
    let list = tokio::task::spawn_blocking(list_monitors)
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
    let (broadcaster, source, hwnd, monitor) = if request.test {
        let broadcaster = Broadcaster::start_test_source(handle, bitrate)
            .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
        (broadcaster, "test", None, None)
    } else if let Some(hwnd) = request.hwnd {
        let broadcaster = Broadcaster::start_window(handle, hwnd, bitrate).map_err(|err| {
            warn!(error = %format!("{err:#}"), "não consegui iniciar a captura");
            ApiError::new(StatusCode::BAD_REQUEST, format!("{err:#}"))
        })?;
        (broadcaster, "window", Some(hwnd), None)
    } else if let Some(mon) = request.monitor {
        let broadcaster = Broadcaster::start_monitor(handle, mon, bitrate).map_err(|err| {
            warn!(error = %format!("{err:#}"), "não consegui iniciar a captura do monitor");
            ApiError::new(StatusCode::BAD_REQUEST, format!("{err:#}"))
        })?;
        (broadcaster, "monitor", None, Some(mon))
    } else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "escolha um monitor (monitor), uma janela (hwnd) ou use \"test\": true",
        ));
    };

    let (stop, _) = watch::channel(false);
    let ice_servers = Arc::new(Mutex::new(ice));
    turn::spawn_refresh(state.config.clone(), ice_servers.clone(), stop.subscribe());
    *live = Some(LiveSession {
        broadcaster,
        source,
        hwnd,
        monitor,
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
    let stream = live
        .as_ref()
        .filter(|session| !session.broadcaster.ended())
        .map(|session| session.broadcaster.health());
    let (status, source, hwnd, monitor, uptime_secs, link) = match live.as_ref() {
        Some(session) if session.broadcaster.ended() => {
            // A fonte encerrou (ex.: janela fechada): derruba os espectadores uma vez, sem remover a sessão.
            let _ = session.stop.send_replace(true);
            notices.extend(session.broadcaster.notices());
            ("ended", Some(session.source), None, None, None, None)
        }
        Some(session) => {
            notices.extend(session.broadcaster.notices());
            let uptime = session.started_at.elapsed().as_secs();
            ("live", Some(session.source), session.hwnd, session.monitor, Some(uptime), Some(state.viewer_link()))
        }
        None => ("idle", None, None, None, None, None),
    };
    StatusBody {
        state: status,
        source,
        hwnd,
        monitor,
        uptime_secs,
        stream,
        viewers: state.viewers.load(Ordering::SeqCst),
        max_viewers: state.config.max_viewers,
        link,
        tunnel: match state.config.tunnel {
            crate::config::TunnelMode::External => "external",
            crate::config::TunnelMode::Managed => "managed",
            crate::config::TunnelMode::None => "none",
        },
        notices,
        viewer_list: state.viewer_list(),
    }
}

#[cfg(test)]
mod tests {
    use super::local_origin;
    use axum::http::{HeaderMap, HeaderValue, header::ORIGIN};

    fn with_origin(value: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(ORIGIN, HeaderValue::from_str(value).unwrap());
        map
    }

    #[test]
    fn preview_origin_must_be_local() {
        assert!(local_origin(&HeaderMap::new()));
        assert!(local_origin(&with_origin("http://localhost:7878")));
        assert!(local_origin(&with_origin("http://127.0.0.1:7878")));
        assert!(local_origin(&with_origin("http://[::1]:7878")));
        assert!(!local_origin(&with_origin("https://site-malicioso.com")));
        assert!(!local_origin(&with_origin("http://localhost.evil.com")));
        assert!(!local_origin(&with_origin("null")));
        assert!(!local_origin(&with_origin("http://localhost:7878/x")));
    }

    #[test]
    fn start_request_deserializes_monitor() {
        let json = r#"{"monitor": 123456}"#;
        let req: super::StartRequest = serde_json::from_str(json).expect("StartRequest desserializa monitor");
        assert_eq!(req.monitor, Some(123456));
        assert_eq!(req.hwnd, None);
        assert!(!req.test);
    }

    #[test]
    fn status_with_source_monitor() {
        let body = super::StatusBody {
            state: "live",
            source: Some("monitor"),
            hwnd: None,
            monitor: Some(654321),
            uptime_secs: Some(42),
            stream: None,
            viewers: 1,
            max_viewers: 10,
            link: Some("https://example.com/s/xyz".to_string()),
            tunnel: "none",
            notices: Vec::new(),
            viewer_list: Vec::new(),
        };
        let val = serde_json::to_value(&body).expect("StatusBody serializa");
        assert_eq!(val["source"], "monitor");
        assert_eq!(val["monitor"], 654321);
        assert_eq!(val["hwnd"], serde_json::Value::Null);
    }
}
