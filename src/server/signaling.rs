//! Sinalização WebSocket do espectador: oferta do servidor, resposta e candidatos ICE nos dois sentidos.
//!
//! Mensagens (JSON, campo `type`):
//! - servidor -> navegador: `auth_required`, `offer` (sdp + ice_servers), `ice` (candidate), `error` (message)
//! - navegador -> servidor: `auth` (password), `answer` (sdp), `ice` (candidate), `keyframe`, `stats`, `ping`

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ::rtc::peer_connection::transport::RTCIceCandidateInit;
use axum::extract::ws::{Message, WebSocket};
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::rtc::peer::{Peer, PeerEvent};
pub use crate::server::viewer_stats::{
    ALLOWED_LAST_REASONS, ViewerStats, client_ip, generate_viewer_id, mask_ip, sanitize_stats,
};
use crate::server::{AppState, ViewerKind};
use crate::turn::IceServer;

const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// Limite de uma mensagem do navegador (a answer SDP tem alguns KB).
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Limite da mensagem `stats`, que vem da internet e vai para o painel.
const MAX_STATS_BYTES: usize = 4 * 1024;

type Tx = SplitSink<WebSocket, Message>;
type Rx = SplitStream<WebSocket>;

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

    let (tx, rx) = socket.split();
    let (mut tx, mut rx) = (tx, rx);

    let (slot, tracks, mut stop, ice_servers, broadcaster_shared) = match state.join_viewer(kind) {
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
                    return;
                }
            }
        }
        // Só entra na lista do painel depois de passar pela senha.
        state.register_viewer(viewer_id.clone());
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

    let (peer, mut events) = match Peer::new(&ice_servers, udp_port, tracks, Some(broadcaster_shared)).await {
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
                    // A resposta SDP passa de 4 KB; o limite geral é folgado e o de `stats` é 4 KB.
                    if text.len() > MAX_MESSAGE_BYTES {
                        debug!("mensagem de cliente excedeu o limite");
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
        "stats" if text.len() <= MAX_STATS_BYTES => {
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
}
