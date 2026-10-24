//! Servidor HTTP em duas portas:
//! - pública (0.0.0.0:public_port): só /s/<token>, /viewer e o WebSocket do espectador;
//! - controle (127.0.0.1:control_port): /control e /api/*, aceitos apenas de loopback.

pub mod api;
pub mod signaling;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::{ConnectInfo, Path, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header::HOST};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use tokio::sync::watch;
use webrtc::media_stream::track_local::TrackLocal;

use crate::config::{Config, TunnelMode};
use crate::rtc::broadcaster::Broadcaster;
use crate::rtc::peer::PortPool;

const VIEWER_HTML: &str = include_str!("../../web/viewer.html");

/// Cabeçalhos que indicam que a requisição passou por um proxy/tunnel.
const PROXY_HEADERS: [&str; 3] = ["cf-connecting-ip", "x-forwarded-for", "forwarded"];

/// Token aleatório de 128 bits, em hex, gerado a cada execução.
pub fn generate_token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Transmissão em andamento.
pub struct LiveSession {
    pub broadcaster: Broadcaster,
    pub source: &'static str,
    /// Ao receber `true`, os WebSockets dos espectadores encerram.
    pub stop: watch::Sender<bool>,
}

pub struct AppState {
    pub config: Config,
    pub token: String,
    /// Avisos mostrados no controle (ex.: cloudflared ausente).
    pub notices: Vec<String>,
    pub live: std::sync::Mutex<Option<LiveSession>>,
    pub viewers: AtomicUsize,
    pub ports: PortPool,
}

/// Vaga de espectador; libera ao ser descartada.
pub struct ViewerSlot {
    state: Arc<AppState>,
}

impl Drop for ViewerSlot {
    fn drop(&mut self) {
        self.state.viewers.fetch_sub(1, Ordering::SeqCst);
    }
}

impl AppState {
    pub fn new(config: Config, token: String, notices: Vec<String>) -> Self {
        let ports = PortPool::new(config.udp_port_min, config.udp_port_max);
        Self {
            config,
            token,
            notices,
            live: std::sync::Mutex::new(None),
            viewers: AtomicUsize::new(0),
            ports,
        }
    }

    /// Reserva uma vaga e devolve as tracks e o canal de parada da transmissão atual.
    pub fn join_viewer(
        self: &Arc<Self>,
    ) -> Result<(ViewerSlot, Vec<Arc<dyn TrackLocal>>, watch::Receiver<bool>), &'static str> {
        let live = self.live.lock().expect("live poisoned");
        let session = live.as_ref().ok_or("a transmissão não está ativa")?;
        if session.broadcaster.ended() {
            return Err("a transmissão não está ativa");
        }
        // Keyframe para o peer novo decodificar já no primeiro quadro.
        session.broadcaster.request_keyframe();
        let max = self.config.max_viewers;
        self.viewers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .map_err(|_| "limite de espectadores atingido")?;
        Ok((
            ViewerSlot {
                state: self.clone(),
            },
            session.broadcaster.tracks(),
            session.stop.subscribe(),
        ))
    }

    /// Encerra a transmissão: avisa os espectadores e libera a fonte.
    pub fn stop_live(&self) -> bool {
        let session = self.live.lock().expect("live poisoned").take();
        match session {
            Some(session) => {
                let _ = session.stop.send(true);
                session.broadcaster.stop();
                true
            }
            None => false,
        }
    }

    pub fn viewer_link(&self) -> String {
        format!("{}/s/{}", public_base_url(&self.config), self.token)
    }
}

pub fn public_base_url(config: &Config) -> String {
    match config.tunnel {
        TunnelMode::None => format!("http://{}:{}", config.public_host, config.public_port),
        TunnelMode::External | TunnelMode::Managed => format!("https://{}", config.public_host),
    }
}

/// Rotas públicas. Nada de /control ou /api aqui: essas rotas não existem nesta porta.
pub fn public_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/s/{token}", get(viewer_page))
        .route("/viewer", get(viewer_page_without_token))
        .route("/ws/{token}", get(viewer_socket))
        .with_state(state)
}

/// Rotas de controle, protegidas por `require_local`.
pub fn control_router(state: Arc<AppState>) -> Router {
    api::routes()
        .layer(middleware::from_fn(require_local))
        .with_state(state)
}

async fn viewer_page(State(state): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    Html(VIEWER_HTML).into_response()
}

async fn viewer_page_without_token() -> Html<&'static str> {
    Html(VIEWER_HTML)
}

async fn viewer_socket(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    ws.on_upgrade(move |socket| signaling::handle_viewer(socket, state))
}

/// Middleware: só aceita requisições de loopback, sem cabeçalhos de proxy e com Host local.
pub async fn require_local(request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    match check_local_request(peer, request.headers()) {
        Ok(()) => next.run(request).await,
        Err(status) => status.into_response(),
    }
}

/// Regra de acesso ao controle. Devolve 403 se a requisição não for local.
pub fn check_local_request(peer: Option<IpAddr>, headers: &HeaderMap) -> Result<(), StatusCode> {
    let peer = peer.ok_or(StatusCode::FORBIDDEN)?;
    if !peer.is_loopback() {
        return Err(StatusCode::FORBIDDEN);
    }
    if PROXY_HEADERS.iter().any(|name| headers.contains_key(*name)) {
        return Err(StatusCode::FORBIDDEN);
    }
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    if is_local_host(host) {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

/// Aceita `localhost`, `127.0.0.1` e `[::1]`, com ou sem porta.
pub fn is_local_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &host[..end + 2],
            None => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .any(|local| name.eq_ignore_ascii_case(local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request as HttpRequest};
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default(), "a".repeat(32), vec![]))
    }

    fn headers(host: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(HOST, HeaderValue::from_str(host).unwrap());
        map
    }

    fn local() -> Option<IpAddr> {
        Some(IpAddr::from([127, 0, 0, 1]))
    }

    fn remote() -> Option<IpAddr> {
        Some(IpAddr::from([192, 168, 1, 50]))
    }

    fn with_peer(mut request: HttpRequest<Body>, addr: SocketAddr) -> HttpRequest<Body> {
        request.extensions_mut().insert(ConnectInfo(addr));
        request
    }

    #[test]
    fn token_is_128_bit_hex_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn accepts_loopback_with_local_host() {
        assert!(check_local_request(local(), &headers("localhost:7878")).is_ok());
        assert!(check_local_request(local(), &headers("127.0.0.1:7878")).is_ok());
        assert!(check_local_request(local(), &headers("[::1]:7878")).is_ok());
    }

    #[test]
    fn rejects_non_loopback_peer() {
        assert_eq!(
            check_local_request(remote(), &headers("localhost:7878")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            check_local_request(None, &headers("localhost")),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn rejects_proxy_headers_and_foreign_host() {
        let mut proxied = headers("localhost:7878");
        proxied.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(
            check_local_request(local(), &proxied),
            Err(StatusCode::FORBIDDEN)
        );

        assert_eq!(
            check_local_request(local(), &headers("winshare.exemplo.com.br")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            check_local_request(local(), &headers("localhost.evil.com")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            check_local_request(local(), &HeaderMap::new()),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[tokio::test]
    async fn control_api_refuses_remote_clients() {
        let app = control_router(state());
        let request = with_peer(
            HttpRequest::builder()
                .uri("/api/status")
                .header(HOST, "localhost:7878")
                .body(Body::empty())
                .unwrap(),
            SocketAddr::from(([192, 168, 1, 50], 5000)),
        );
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn control_api_accepts_local_clients() {
        let app = control_router(state());
        let request = with_peer(
            HttpRequest::builder()
                .uri("/api/status")
                .header(HOST, "127.0.0.1:7878")
                .body(Body::empty())
                .unwrap(),
            SocketAddr::from(([127, 0, 0, 1], 5000)),
        );
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn public_port_has_no_control_routes() {
        let app = public_router(state());
        for uri in ["/control", "/api/status", "/api/start", "/api/windows"] {
            let request = with_peer(
                HttpRequest::builder()
                    .uri(uri)
                    .header(HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
                SocketAddr::from(([127, 0, 0, 1], 5000)),
            );
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} não deveria existir"
            );
        }
    }

    #[tokio::test]
    async fn viewer_page_requires_the_right_token() {
        let state = state();
        let good = format!("/s/{}", state.token);
        let app = public_router(state);
        let wrong = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/s/outro")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
        let right = app
            .oneshot(
                HttpRequest::builder()
                    .uri(good)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(right.status(), StatusCode::OK);
    }

    #[test]
    fn host_parsing_handles_ports_and_brackets() {
        assert!(is_local_host("localhost"));
        assert!(is_local_host("LOCALHOST:9"));
        assert!(is_local_host("[::1]"));
        assert!(!is_local_host("[::1"));
        assert!(!is_local_host("example.com:80"));
    }

    #[test]
    fn stop_without_session_is_noop() {
        assert!(!state().stop_live());
    }
}
