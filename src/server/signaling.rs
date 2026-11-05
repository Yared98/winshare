//! Sinalização WebSocket do espectador: oferta do servidor, resposta e candidatos ICE nos dois sentidos.
//!
//! Mensagens (JSON, campo `type`):
//! - servidor -> navegador: `auth_required`, `offer` (sdp + ice_servers), `ice` (candidate), `error` (message)
//! - navegador -> servidor: `auth` (password), `answer` (sdp), `ice` (candidate)

use std::sync::Arc;
use std::time::Duration;

use ::rtc::peer_connection::transport::RTCIceCandidateInit;
use axum::extract::ws::{Message, WebSocket};
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::rtc::peer::{Peer, PeerEvent};
use crate::server::AppState;
use crate::turn::IceServer;

const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

type Tx = SplitSink<WebSocket, Message>;
type Rx = SplitStream<WebSocket>;

#[derive(Debug, Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    kind: String,
    sdp: Option<String>,
    candidate: Option<IceCandidate>,
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

/// Atende um espectador do início ao fim da conexão.
pub async fn handle_viewer(socket: WebSocket, state: Arc<AppState>) {
    let (tx, rx) = socket.split();
    let (mut tx, mut rx) = (tx, rx);

    let (slot, tracks, mut stop, ice_servers) = match state.join_viewer() {
        Ok(joined) => joined,
        Err(reason) => {
            send_error(&mut tx, reason).await;
            return;
        }
    };
    // `slot` libera a vaga de espectador ao sair desta função.
    let _slot = slot;

    if let Some(expected) = state.config.password.as_deref() {
        if !authenticate(&mut tx, &mut rx, expected).await {
            return;
        }
    }

    let udp_port = match state.ports.acquire() {
        Ok(port) => port,
        Err(err) => {
            warn!(error = %err, "sem porta UDP para o espectador");
            send_error(&mut tx, "servidor sem portas disponíveis").await;
            return;
        }
    };

    let (peer, mut events) = match Peer::new(&ice_servers, udp_port, tracks).await {
        Ok(created) => created,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "não consegui criar o peer");
            state.ports.release(udp_port);
            send_error(&mut tx, "não foi possível preparar a conexão").await;
            return;
        }
    };

    run_session(&peer, &ice_servers, &mut tx, &mut rx, &mut events, &mut stop).await;

    peer.close().await;
    state.ports.release(udp_port);
    info!("espectador desconectado");
}

async fn run_session(
    peer: &Peer,
    ice_servers: &[IceServer],
    tx: &mut Tx,
    rx: &mut Rx,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<PeerEvent>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) {
    let offer = match peer.create_offer().await {
        Ok(sdp) => sdp,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "falha ao criar a oferta");
            send_error(tx, "falha ao negociar a conexão").await;
            return;
        }
    };
    if !send_json(tx, offer_message(&offer, ice_servers)).await {
        return;
    }
    info!("espectador conectado; oferta enviada");

    loop {
        tokio::select! {
            _ = stop.changed() => {
                send_error(tx, "a transmissão foi encerrada").await;
                break;
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
                        break;
                    }
                }
                Some(PeerEvent::State(state)) => {
                    use webrtc::peer_connection::RTCPeerConnectionState as S;
                    if matches!(state, S::Failed | S::Closed) {
                        break;
                    }
                }
                None => break,
            },
            message = rx.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    handle_client_text(peer, text.as_str()).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn handle_client_text(peer: &Peer, text: &str) {
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(err) => {
            debug!(%err, "mensagem de sinalização inválida");
            return;
        }
    };
    match message.kind.as_str() {
        "answer" => match message.sdp {
            Some(sdp) => {
                if let Err(err) = peer.accept_answer(sdp).await {
                    warn!(error = %format!("{err:#}"), "answer rejeitada");
                }
            }
            None => debug!("answer sem sdp"),
        },
        "ice" => {
            if let Some(c) = message.candidate {
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
        other => debug!(kind = other, "mensagem de sinalização desconhecida"),
    }
}

/// Pede a senha da sala. Devolve `false` (e fecha) se ela não vier certa a tempo.
async fn authenticate(tx: &mut Tx, rx: &mut Rx, expected: &str) -> bool {
    if !send_json(tx, json!({ "type": "auth_required" })).await {
        return false;
    }
    let wait = async {
        while let Some(Ok(Message::Text(text))) = rx.next().await {
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
        Ok(Some(given)) if given == expected => true,
        Ok(_) => {
            warn!("senha incorreta ou ausente; espectador recusado");
            send_error(tx, "senha incorreta").await;
            false
        }
        Err(_) => {
            send_error(tx, "tempo esgotado para informar a senha").await;
            false
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
