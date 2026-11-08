//! Sinalização WebSocket do espectador: oferta do servidor, resposta e candidatos ICE nos dois sentidos.
//!
//! Mensagens (JSON, campo `type`):
//! - servidor -> navegador: `auth_required`, `offer` (sdp + ice_servers), `ice` (candidate), `error` (message)
//! - navegador -> servidor: `auth` (password), `answer` (sdp), `ice` (candidate), `keyframe`, `stats`, `ping`

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::rtc::peer_connection::transport::RTCIceCandidateInit;
use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::rtc::peer::{Peer, PeerEvent};
use crate::server::{AppState, ViewerKind};
use crate::turn::IceServer;

const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

type Tx = SplitSink<WebSocket, Message>;
type Rx = SplitStream<WebSocket>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ViewerStats {
    pub rtt_ms: u32,
    pub loss_pct: f32,
    pub fps: u32,
    pub kbps: u32,
    pub path: String,
    pub reconnects: u32,
    pub last_reason: String,
}

impl Default for ViewerStats {
    fn default() -> Self {
        Self {
            rtt_ms: 0,
            loss_pct: 0.0,
            fps: 0,
            kbps: 0,
            path: "direct".to_string(),
            reconnects: 0,
            last_reason: String::new(),
        }
    }
}

pub const ALLOWED_LAST_REASONS: &[&str] = &[
    "travou",
    "ice-failed",
    "ice-closed",
    "ice-desconectado",
    "ws-fechado",
    "timeout",
    "nenhum",
    "iniciou",
];

#[derive(Debug, Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    kind: String,
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IceCandidate {
    candidate: String,
    #[serde(rename = "sdpMid")]
    sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex")]
    sdp_mline_index: Option<u16>,
}

pub fn generate_viewer_id() -> String {
    let bytes: [u8; 3] = rand::random();
    format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

pub fn mask_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.x", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}", s[0], s[1], s[2])
        }
    }
}

pub fn client_ip(headers: &HeaderMap, socket_addr: SocketAddr) -> (IpAddr, String) {
    if let Some(cf_ip) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        let masked = mask_ip(cf_ip);
        (cf_ip, masked)
    } else {
        let ip = socket_addr.ip();
        let masked = mask_ip(ip);
        (ip, masked)
    }
}

pub fn sanitize_stats(raw: &Value) -> Option<ViewerStats> {
    let rtt_ms = raw
        .get("rtt_ms")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 60_000.0) as u32)
        .unwrap_or(0);

    let loss_pct = raw
        .get("loss_pct")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 100.0) as f32)
        .unwrap_or(0.0);

    let fps = raw
        .get("fps")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 240.0) as u32)
        .unwrap_or(0);

    let kbps = raw
        .get("kbps")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 100_000.0) as u32)
        .unwrap_or(0);

    let reconnects = raw
        .get("reconnects")
        .and_then(|v| v.as_f64())
        .map(|v| v.clamp(0.0, 10_000.0) as u32)
        .unwrap_or(0);

    let path_str = raw.get("path").and_then(|v| v.as_str()).unwrap_or("direct");
    let path = if path_str.eq_ignore_ascii_case("relay") {
        "relay".to_string()
    } else {
        "direct".to_string()
    };

    let reason_raw = raw.get("last_reason").and_then(|v| v.as_str()).unwrap_or("");
    let truncated: String = reason_raw.chars().take(32).collect();
    let last_reason = if ALLOWED_LAST_REASONS.iter().any(|&r| r == truncated) {
        truncated
    } else if truncated.is_empty() {
        String::new()
    } else {
        "outro".to_string()
    };

    Some(ViewerStats {
        rtt_ms,
        loss_pct,
        fps,
        kbps,
        path,
        reconnects,
        last_reason,
    })
}

/// Atende um espectador ou preview do início ao fim da conexão.
pub async fn handle_viewer(
    socket: WebSocket,
    state: Arc<AppState>,
    kind: ViewerKind,
    client_ip: (IpAddr, String),
) {
    let viewer_id = generate_viewer_id();
    let (_, masked_ip) = client_ip;
    let started_at = Instant::now();

    if kind == ViewerKind::Public {
        state.register_viewer(viewer_id.clone());
    }

    let (tx, rx) = socket.split();
    let (mut tx, mut rx) = (tx, rx);

    let (slot, tracks, mut stop, ice_servers) = match state.join_viewer(kind) {
        Ok(joined) => joined,
        Err(reason) => {
            send_error(&mut tx, reason).await;
            log_disconnect(kind, &viewer_id, &masked_ip, reason, started_at);
            if kind == ViewerKind::Public {
                state.remove_viewer(&viewer_id);
            }
            return;
        }
    };
    // `slot` libera a vaga de espectador (se for público) ao sair desta função.
    let _slot = slot;

    if kind == ViewerKind::Public {
        if let Some(expected) = state.config.password.as_deref() {
            match authenticate(&mut tx, &mut rx, expected).await {
                Ok(()) => {}
                Err(reason) => {
                    log_disconnect(kind, &viewer_id, &masked_ip, reason, started_at);
                    state.remove_viewer(&viewer_id);
                    return;
                }
            }
        }
    }

    let udp_port = match state.ports.acquire() {
        Ok(port) => port,
        Err(err) => {
            warn!(error = %err, "sem porta UDP para o espectador");
            let reason = "servidor sem portas disponíveis";
            send_error(&mut tx, reason).await;
            log_disconnect(kind, &viewer_id, &masked_ip, reason, started_at);
            if kind == ViewerKind::Public {
                state.remove_viewer(&viewer_id);
            }
            return;
        }
    };

    let (peer, mut events) = match Peer::new(&ice_servers, udp_port, tracks).await {
        Ok(created) => created,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "não consegui criar o peer");
            state.ports.release(udp_port);
            let reason = "erro de negociação";
            send_error(&mut tx, "não foi possível preparar a conexão").await;
            log_disconnect(kind, &viewer_id, &masked_ip, reason, started_at);
            if kind == ViewerKind::Public {
                state.remove_viewer(&viewer_id);
            }
            return;
        }
    };

    let disconnect_reason = run_session(
        &peer,
        &ice_servers,
        &mut tx,
        &mut rx,
        &mut events,
        &mut stop,
        &state,
        &viewer_id,
        kind,
    )
    .await;

    peer.close().await;
    state.ports.release(udp_port);

    if kind == ViewerKind::Public {
        state.remove_viewer(&viewer_id);
    }
    log_disconnect(kind, &viewer_id, &masked_ip, disconnect_reason, started_at);
}

fn log_disconnect(
    kind: ViewerKind,
    id: &str,
    ip: &str,
    reason: &str,
    started_at: Instant,
) {
    let duration_secs = started_at.elapsed().as_secs();
    match kind {
        ViewerKind::Public => {
            info!(
                id = %id,
                ip = %ip,
                reason = %reason,
                duration_secs = duration_secs,
                "espectador desconectado: {reason} (sessão: {duration_secs}s)"
            );
        }
        ViewerKind::Preview => {
            info!(
                id = %id,
                ip = %ip,
                reason = %reason,
                duration_secs = duration_secs,
                "preview desconectado: {reason} (sessão: {duration_secs}s)"
            );
        }
    }
}

async fn run_session(
    peer: &Peer,
    ice_servers: &[IceServer],
    tx: &mut Tx,
    rx: &mut Rx,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<PeerEvent>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    state: &Arc<AppState>,
    viewer_id: &str,
    kind: ViewerKind,
) -> &'static str {
    let offer = match peer.create_offer().await {
        Ok(sdp) => sdp,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "falha ao criar a oferta");
            send_error(tx, "falha ao negociar a conexão").await;
            return "erro de negociação";
        }
    };
    if !send_json(tx, offer_message(&offer, ice_servers)).await {
        return "WS fechado pelo navegador";
    }
    info!(id = %viewer_id, "espectador conectado; oferta enviada");

    let mut last_keyframe: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = stop.changed() => {
                send_error(tx, "a transmissão foi encerrada").await;
                break "transmissão parada";
            }
            event = events.recv() => match event {
                Some(PeerEvent::LocalIce(candidate)) => {
                    let payload = json!({
                        "type": "ice",
                        "candidate": {
                            "candidate": candidate.candidate,
                            "sdpMid": candidate.sdp_mid,
                            "sdpMLineIndex": candidate.sdp_mline_index,
                        }
                    });
                    if !send_json(tx, payload).await {
                        break "WS fechado pelo navegador";
                    }
                }
                Some(PeerEvent::State(state)) => {
                    use webrtc::peer_connection::RTCPeerConnectionState as S;
                    if matches!(state, S::Failed | S::Closed) {
                        break "peer Failed/Closed";
                    }
                }
                None => {
                    break "peer Failed/Closed";
                }
            },
            message = rx.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if text.len() > 4096 {
                        debug!("mensagem de cliente excedeu limite de 4 KB");
                        continue;
                    }
                    handle_client_text(peer, text.as_str(), state, viewer_id, kind, &mut last_keyframe).await;
                }
                Some(Ok(Message::Close(_))) => {
                    break "WS fechado pelo navegador";
                }
                None | Some(Err(_)) => {
                    break "WS fechado pelo navegador";
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn handle_client_text(
    peer: &Peer,
    text: &str,
    state: &Arc<AppState>,
    viewer_id: &str,
    kind: ViewerKind,
    last_keyframe: &mut Option<Instant>,
) {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(err) => {
            debug!(%err, "mensagem de sinalização inválida");
            return;
        }
    };
    let kind_str = match value.get("type").and_then(|v| v.as_str()) {
        Some(k) => k,
        None => return,
    };
    match kind_str {
        "answer" => match value.get("sdp").and_then(|v| v.as_str()) {
            Some(sdp) => {
                if let Err(err) = peer.accept_answer(sdp.to_string()).await {
                    warn!(error = %format!("{err:#}"), "answer rejeitada");
                }
            }
            None => debug!("answer sem sdp"),
        },
        "ice" => {
            if let Some(c) = value.get("candidate") {
                if let Ok(c) = serde_json::from_value::<IceCandidate>(c.clone()) {
                    let init = RTCIceCandidateInit {
                        candidate: c.candidate,
                        sdp_mid: c.sdp_mid,
                        sdp_mline_index: c.sdp_mline_index,
                        username_fragment: None,
                        url: None,
                    };
                    if let Err(err) = peer.add_ice(init).await {
                        debug!(error = %format!("{err:#}"), "candidato ICE remoto ignorado");
                    }
                }
            }
        }
        "keyframe" => {
            let now = Instant::now();
            let allow = match last_keyframe {
                Some(t) => now.duration_since(*t) >= Duration::from_secs(1),
                None => true,
            };
            if allow {
                *last_keyframe = Some(now);
                state.request_keyframe();
            }
        }
        "stats" => {
            if kind == ViewerKind::Public {
                if let Some(stats) = sanitize_stats(&value) {
                    state.update_viewer_stats(viewer_id, stats);
                }
            }
        }
        "ping" => {}
        other => debug!(kind = other, "mensagem de sinalização desconhecida"),
    }
}

/// Pede a senha da sala. Devolve `Ok(())` se estiver correta, senão `Err(motivo)`.
async fn authenticate(tx: &mut Tx, rx: &mut Rx, expected: &str) -> Result<(), &'static str> {
    if !send_json(tx, json!({ "type": "auth_required" })).await {
        return Err("WS fechado pelo navegador");
    }
    let wait = async {
        while let Some(Ok(Message::Text(text))) = rx.next().await {
            if text.len() > 4096 {
                continue;
            }
            let parsed: Option<ClientMessage> = serde_json::from_str(text.as_str()).ok();
            if let Some(message) = parsed {
                if message.kind == "auth" {
                    return message.password;
                }
            }
        }
        None
    };
    match timeout(AUTH_TIMEOUT, wait).await {
        Ok(Some(given)) if given == expected => Ok(()),
        Ok(_) => {
            warn!("senha incorreta ou ausente; espectador recusado");
            send_error(tx, "senha incorreta").await;
            Err("senha incorreta")
        }
        Err(_) => {
            send_error(tx, "tempo esgotado para informar a senha").await;
            Err("timeout de senha")
        }
    }
}

async fn send_error(tx: &mut Tx, message: &str) {
    let _ = send_json(tx, json!({ "type": "error", "message": message })).await;
    let _ = tx.close().await;
}

/// Mensagem `offer`: SDP e os servidores ICE (STUN + TURN) que o navegador deve usar.
pub fn offer_message(sdp: &str, ice_servers: &[IceServer]) -> Value {
    json!({ "type": "offer", "sdp": sdp, "ice_servers": ice_servers })
}

async fn send_json(tx: &mut Tx, value: Value) -> bool {
    tx.send(Message::Text(value.to_string().into()))
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use axum::http::HeaderValue;

    #[test]
    fn offer_message_carries_ice_servers() {
        let ice = vec![
            IceServer {
                urls: vec!["stun:stun.example:3478".to_owned()],
                username: None,
                credential: None,
            },
            IceServer {
                urls: vec!["turn:turn.example:3478".to_owned()],
                username: Some("usuario".to_owned()),
                credential: Some("credencial".to_owned()),
            },
        ];
        let msg = offer_message("v=0\r\n", &ice);
        assert_eq!(msg["type"], "offer");
        assert_eq!(msg["sdp"], "v=0\r\n");
        let servers = msg["ice_servers"].as_array().expect("ice_servers é lista");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0]["urls"][0], "stun:stun.example:3478");
        assert!(servers[0].get("username").is_none());
        assert_eq!(servers[1]["urls"][0], "turn:turn.example:3478");
        assert_eq!(servers[1]["username"], "usuario");
        assert_eq!(servers[1]["credential"], "credencial");
    }

    #[test]
    fn mask_ip_handles_ipv4_and_ipv6() {
        let v4: IpAddr = "192.168.1.50".parse().unwrap();
        assert_eq!(mask_ip(v4), "192.168.1.x");

        let v4_loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(mask_ip(v4_loopback), "127.0.0.x");

        let v6: IpAddr = "2001:db8:85a3:0:0:8a2e:370:7334".parse().unwrap();
        assert_eq!(mask_ip(v6), "2001:db8:85a3");

        let v6_loopback: IpAddr = "::1".parse().unwrap();
        assert_eq!(mask_ip(v6_loopback), "0:0:0");
    }

    #[test]
    fn client_ip_prefers_cf_connecting_ip_header() {
        let socket = SocketAddr::from(([127, 0, 0, 1], 12345));

        // Sem cabeçalho: usa o endereço do socket
        let headers = HeaderMap::new();
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(masked, "127.0.0.x");

        // Com cf-connecting-ip IPv4
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.195"));
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, "203.0.113.195".parse::<IpAddr>().unwrap());
        assert_eq!(masked, "203.0.113.x");

        // Com cf-connecting-ip IPv6
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("2804:14d:5c73:89b0::1"));
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, "2804:14d:5c73:89b0::1".parse::<IpAddr>().unwrap());
        assert_eq!(masked, "2804:14d:5c73");

        // Com valor inválido no cabeçalho: fallback para o socket
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("invalido"));
        let (ip, masked) = client_ip(&headers, socket);
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(masked, "127.0.0.x");
    }

    #[test]
    fn viewer_id_is_6_hex_chars() {
        let id1 = generate_viewer_id();
        let id2 = generate_viewer_id();
        assert_eq!(id1.len(), 6);
        assert_eq!(id2.len(), 6);
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(id2.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sanitize_stats_clamps_numbers_and_restricts_strings() {
        // Objeto válido normal
        let raw = json!({
            "rtt_ms": 45,
            "loss_pct": 2.5,
            "fps": 60,
            "kbps": 5000,
            "path": "relay",
            "reconnects": 1,
            "last_reason": "travou"
        });
        let stats = sanitize_stats(&raw).expect("stats válido");
        assert_eq!(stats.rtt_ms, 45);
        assert_eq!(stats.loss_pct, 2.5);
        assert_eq!(stats.fps, 60);
        assert_eq!(stats.kbps, 5000);
        assert_eq!(stats.path, "relay");
        assert_eq!(stats.reconnects, 1);
        assert_eq!(stats.last_reason, "travou");

        // Clamping de limites superiores e inferiores
        let raw_overflow = json!({
            "rtt_ms": 999_999,
            "loss_pct": 150.0,
            "fps": 500,
            "kbps": 999_999,
            "path": "invalid_path",
            "reconnects": 50_000,
            "last_reason": "motivo_desconhecido"
        });
        let stats = sanitize_stats(&raw_overflow).expect("clamped");
        assert_eq!(stats.rtt_ms, 60_000);
        assert_eq!(stats.loss_pct, 100.0);
        assert_eq!(stats.fps, 240);
        assert_eq!(stats.kbps, 100_000);
        assert_eq!(stats.path, "direct");
        assert_eq!(stats.reconnects, 10_000);
        assert_eq!(stats.last_reason, "outro");

        // Clamping negativo
        let raw_underflow = json!({
            "rtt_ms": -10,
            "loss_pct": -20.5,
            "fps": -5,
            "kbps": -100,
            "reconnects": -2,
        });
        let stats = sanitize_stats(&raw_underflow).expect("clamped zeros");
        assert_eq!(stats.rtt_ms, 0);
        assert_eq!(stats.loss_pct, 0.0);
        assert_eq!(stats.fps, 0);
        assert_eq!(stats.kbps, 0);
        assert_eq!(stats.reconnects, 0);

        // String longa deve ser truncada em 32 chars
        let long_reason = "a".repeat(100);
        let raw_long = json!({
            "last_reason": long_reason
        });
        let stats = sanitize_stats(&raw_long).expect("truncated");
        assert_eq!(stats.last_reason, "outro");
    }
}
