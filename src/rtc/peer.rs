//! Uma conexão WebRTC por espectador. O servidor é quem oferece (offer) e o navegador responde (answer).

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use ::rtc::interceptor::Registry;
use ::rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use ::rtc::peer_connection::configuration::media_engine::MediaEngine;
use ::rtc::peer_connection::sdp::RTCSessionDescription;
use ::rtc::peer_connection::transport::RTCIceCandidateInit;
use ::rtc::rtp_transceiver::rtp_sender::RtpCodecKind;

use crate::config::{Config, TurnConfig};
use crate::rtc::broadcaster::{audio_codec, video_codec};

const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";

/// Evento de um peer que o sinalizador precisa repassar ao navegador.
pub enum PeerEvent {
    LocalIce(RTCIceCandidateInit),
    State(RTCPeerConnectionState),
}

struct Handler {
    tx: mpsc::UnboundedSender<PeerEvent>,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        match event.candidate.to_json() {
            Ok(init) => {
                let _ = self.tx.send(PeerEvent::LocalIce(init));
            }
            Err(err) => debug!(%err, "candidato ICE local inválido"),
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug!(?state, "estado da conexão do espectador");
        let _ = self.tx.send(PeerEvent::State(state));
    }
}

/// Reserva portas UDP da faixa configurada para as conexões ICE, sem repetir portas em uso.
pub struct PortPool {
    min: u16,
    max: u16,
    in_use: Mutex<HashSet<u16>>,
}

impl PortPool {
    pub fn new(min: u16, max: u16) -> Self {
        Self {
            min,
            max,
            in_use: Mutex::new(HashSet::new()),
        }
    }

    pub fn acquire(&self) -> Result<u16> {
        let mut in_use = self.in_use.lock().expect("PortPool poisoned");
        for port in self.min..=self.max {
            if in_use.contains(&port) {
                continue;
            }
            // A sonda é imediatamente descartada; só confirma que a porta está livre agora.
            if UdpSocket::bind(("0.0.0.0", port)).is_ok() {
                in_use.insert(port);
                return Ok(port);
            }
        }
        Err(anyhow!(
            "nenhuma porta UDP livre em {}-{}",
            self.min,
            self.max
        ))
    }

    pub fn release(&self, port: u16) {
        self.in_use.lock().expect("PortPool poisoned").remove(&port);
    }
}

pub struct Peer {
    conn: Arc<dyn PeerConnection>,
}

impl Peer {
    /// Cria o peer, adiciona as tracks compartilhadas e devolve o canal de eventos.
    pub async fn new(
        config: &Config,
        udp_port: u16,
        tracks: Vec<Arc<dyn TrackLocal>>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<PeerEvent>)> {
        let mut media = MediaEngine::default();
        media
            .register_codec(video_codec(), RtpCodecKind::Video)
            .context("codec H.264 não registrado")?;
        media
            .register_codec(audio_codec(), RtpCodecKind::Audio)
            .context("codec Opus não registrado")?;
        let registry = register_default_interceptors(Registry::new(), &mut media)
            .context("interceptors RTP não registrados")?;

        let configuration = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers(config.turn.as_ref()))
            .build();

        let (tx, rx) = mpsc::unbounded_channel();
        let pc = PeerConnectionBuilder::new()
            .with_configuration(configuration)
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { tx }))
            .with_udp_addrs(vec![SocketAddr::from(([0, 0, 0, 0], udp_port))])
            .build()
            .await
            .context("falha ao criar a conexão WebRTC")?;

        let conn: Arc<dyn PeerConnection> = Arc::new(pc);
        for track in tracks {
            conn.add_track(track)
                .await
                .context("falha ao adicionar track")?;
        }
        info!(udp_port, "peer criado");
        Ok((Self { conn }, rx))
    }

    /// Gera o SDP de oferta e o define como descrição local (a coleta de ICE começa aqui).
    pub async fn create_offer(&self) -> Result<String> {
        let offer = self
            .conn
            .create_offer(None)
            .await
            .context("create_offer falhou")?;
        self.conn
            .set_local_description(offer.clone())
            .await
            .context("set_local_description falhou")?;
        Ok(offer.sdp)
    }

    pub async fn accept_answer(&self, sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(sdp).context("answer SDP inválido")?;
        self.conn
            .set_remote_description(answer)
            .await
            .context("set_remote_description falhou")
    }

    pub async fn add_ice(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        self.conn
            .add_ice_candidate(candidate)
            .await
            .context("candidato ICE remoto rejeitado")
    }

    pub async fn close(&self) {
        if let Err(err) = self.conn.close().await {
            debug!(%err, "erro ao fechar o peer");
        }
    }
}

fn ice_servers(turn: Option<&TurnConfig>) -> Vec<RTCIceServer> {
    let mut servers = vec![RTCIceServer {
        urls: vec![DEFAULT_STUN.to_owned()],
        ..Default::default()
    }];
    if let Some(turn) = turn {
        servers.push(RTCIceServer {
            urls: vec![turn.url.clone()],
            username: turn.username.clone(),
            credential: turn.credential.clone(),
        });
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtc::broadcaster::Broadcaster;

    #[test]
    fn port_pool_hands_out_distinct_free_ports() {
        // Faixa fora das portas efêmeras e reservadas do Windows.
        let pool = PortPool::new(50_200, 50_210);
        let a = pool.acquire().unwrap();
        let b = pool.acquire().unwrap();
        assert_ne!(a, b);
        assert!((50_200..=50_210).contains(&a));
        pool.release(a);
        pool.release(b);
    }

    #[test]
    fn port_pool_reports_exhaustion() {
        let pool = PortPool::new(50_220, 50_220);
        let held = pool.acquire().unwrap();
        assert!(pool.acquire().is_err());
        pool.release(held);
        assert_eq!(pool.acquire().unwrap(), held);
    }

    #[tokio::test]
    async fn offer_contains_h264_and_opus() {
        let config = Config::default();
        let broadcaster = Broadcaster::start_test_source(tokio::runtime::Handle::current(), 6_000_000)
            .expect("fonte de teste");
        let (peer, _events) = Peer::new(&config, 50_230, broadcaster.tracks())
            .await
            .expect("peer");
        let sdp = peer.create_offer().await.expect("offer");
        assert!(sdp.contains("H264/90000"), "SDP sem H264");
        assert!(sdp.contains("opus/48000/2"), "SDP sem Opus");
        peer.close().await;
        broadcaster.stop();
    }
}
