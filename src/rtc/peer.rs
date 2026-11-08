//! Uma conexão WebRTC por espectador. O servidor é quem oferece (offer) e o navegador responde (answer).

use std::collections::{HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState,
};

use ::rtc::interceptor::{Interceptor, Packet, Registry, Slot, StreamInfo, TaggedPacket};
use ::rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use ::rtc::peer_connection::configuration::media_engine::MediaEngine;
use ::rtc::peer_connection::sdp::RTCSessionDescription;
use ::rtc::peer_connection::transport::RTCIceCandidateInit;
use ::rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use ::rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use ::rtc::rtcp::receiver_report::ReceiverReport;
use ::rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use ::rtc::sansio::Protocol;
use ::rtc::shared::error::Error;

use crate::rtc::broadcaster::{BroadcasterShared, audio_codec, get_broadcaster, video_codec};
use crate::turn::{IceServer, to_rtc};

static NEXT_PEER_ID: AtomicU64 = AtomicU64::new(1);

/// Evento de um peer que o sinalizador precisa repassar ao navegador.
pub enum PeerEvent {
    LocalIce(RTCIceCandidateInit),
    State(RTCPeerConnectionState),
}

struct Handler {
    tx: mpsc::UnboundedSender<PeerEvent>,
    peer_id: Option<u64>,
    broadcaster: Option<Arc<BroadcasterShared>>,
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
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            if let (Some(peer_id), Some(b)) = (self.peer_id, &self.broadcaster) {
                b.remove_peer(peer_id);
            }
        }
        let _ = self.tx.send(PeerEvent::State(state));
    }
}

pub struct RtcpFeedbackInterceptor {
    peer_id: u64,
    broadcaster: Arc<BroadcasterShared>,
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl RtcpFeedbackInterceptor {
    pub fn new(peer_id: u64, broadcaster: Arc<BroadcasterShared>) -> Self {
        Self {
            peer_id,
            broadcaster,
            read_queue: VecDeque::new(),
            write_queue: VecDeque::new(),
        }
    }
}

fn is_keyframe_request(packet: &Box<dyn ::rtc::rtcp::Packet>) -> bool {
    let any = packet.as_any();
    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for RtcpFeedbackInterceptor {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            for packet in packets {
                if is_keyframe_request(packet) {
                    self.broadcaster.request_keyframe();
                } else if let Some(rr) = packet.as_any().downcast_ref::<ReceiverReport>() {
                    for rep in &rr.reports {
                        let loss_pct = (rep.fraction_lost as f64 / 256.0) * 100.0;
                        self.broadcaster.report_loss(self.peer_id, loss_pct);
                    }
                } else if let Some(sr) = packet
                    .as_any()
                    .downcast_ref::<::rtc::rtcp::sender_report::SenderReport>()
                {
                    for rep in &sr.reports {
                        let loss_pct = (rep.fraction_lost as f64 / 256.0) * 100.0;
                        self.broadcaster.report_loss(self.peer_id, loss_pct);
                    }
                }
            }
            // Sem DeliverToApplication: o RTCP é consumido aqui e ninguém o lê na aplicação;
            // entregá-lo acumularia pacotes num buffer sem leitor.
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for RtcpFeedbackInterceptor {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
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
    peer_id: Option<u64>,
    broadcaster: Option<Arc<BroadcasterShared>>,
}

impl Peer {
    /// Cria o peer, adiciona as tracks compartilhadas e devolve o canal de eventos.
    pub async fn new(
        ice_servers: &[IceServer],
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

        let mut broadcaster: Option<Arc<BroadcasterShared>> = None;
        for track in &tracks {
            let label = track.label().await;
            if let Some(id_str) = label.strip_prefix("screencast-video-") {
                if let Ok(id) = id_str.parse::<u64>() {
                    if let Some(b) = get_broadcaster(id) {
                        broadcaster = Some(b);
                        break;
                    }
                }
            }
        }

        let peer_id = broadcaster.as_ref().map(|b| {
            let id = NEXT_PEER_ID.fetch_add(1, Ordering::Relaxed);
            b.add_peer(id);
            id
        });

        let registry = if let (Some(peer_id), Some(b)) = (peer_id, &broadcaster) {
            let interceptor = RtcpFeedbackInterceptor::new(peer_id, b.clone());
            registry.with(Slot::from(14_000), interceptor)
        } else {
            registry
        };

        let configuration = RTCConfigurationBuilder::new()
            .with_ice_servers(to_rtc(ice_servers))
            .build();

        let (tx, rx) = mpsc::unbounded_channel();
        let pc = PeerConnectionBuilder::new()
            .with_configuration(configuration)
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler {
                tx,
                peer_id,
                broadcaster: broadcaster.clone(),
            }))
            .with_udp_addrs(vec![
                SocketAddr::from(([0, 0, 0, 0], udp_port)),
                SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, udp_port)),
            ])
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
        Ok((
            Self {
                conn,
                peer_id,
                broadcaster,
            },
            rx,
        ))
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
        if let (Some(peer_id), Some(b)) = (self.peer_id, &self.broadcaster) {
            b.remove_peer(peer_id);
        }
        if let Err(err) = self.conn.close().await {
            debug!(%err, "erro ao fechar o peer");
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let (Some(peer_id), Some(b)) = (self.peer_id, &self.broadcaster) {
            b.remove_peer(peer_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::rtc::broadcaster::Broadcaster;
    use crate::turn::build_ice_servers;

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
        let ice = build_ice_servers(&config, None);
        let (peer, _events) = Peer::new(&ice, 50_230, broadcaster.tracks())
            .await
            .expect("peer");
        let sdp = peer.create_offer().await.expect("offer");
        assert!(sdp.contains("H264/90000"), "SDP sem H264");
        assert!(sdp.contains("opus/48000/2"), "SDP sem Opus");
        // Verifica feedback RTCP para NACK e PLI
        assert!(
            sdp.contains("nack") && sdp.contains("nack pli"),
            "SDP sem rtcp-fb nack/pli: {sdp}"
        );
        // Garante que existe linha de rtcp-fb específica para nack e nack pli
        assert!(
            sdp.lines().any(|l| l.starts_with("a=rtcp-fb:") && l.ends_with("nack")),
            "SDP sem a=rtcp-fb:<pt> nack: {sdp}"
        );
        assert!(
            sdp.lines().any(|l| l.starts_with("a=rtcp-fb:") && l.ends_with("nack pli")),
            "SDP sem a=rtcp-fb:<pt> nack pli: {sdp}"
        );
        peer.close().await;
        broadcaster.stop();
    }
}
