//! Distribui as mesmas amostras codificadas para várias conexões.
//!
//! No webrtc 0.21 uma track local só fica ligada a UMA conexão por vez (`bind` substitui a
//! anterior e o `unbind` de uma conexão antiga desliga a track para quem ficou). Compartilhar a
//! mesma track entre espectadores fazia o vídeo parar ~7 s depois de qualquer reconexão.
//! Aqui cada espectador ganha sua própria track; o encoder continua único.

use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Instant;

use ::rtc::media::Sample;
use ::rtc::media_stream::MediaStreamTrack;
use ::rtc::rtp_transceiver::PayloadType;
use ::rtc::rtp_transceiver::rtp_sender::{RTCRtpEncodingParameters, RtpCodecKind};
use anyhow::Result;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::debug;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;

/// Fila por espectador: ~1 s de vídeo a 60 fps. Cheia = espectador lento; descarta em vez de
/// segurar o encoder (e os outros espectadores).
const QUEUE_CAPACITY: usize = 64;

struct Subscriber {
    tx: mpsc::Sender<Sample>,
}

/// Descrição de uma track (vídeo ou áudio) e a lista de espectadores inscritos nela.
pub struct FanoutTrack {
    handle: Handle,
    /// Base dos carimbos RTP, comum a todos os espectadores.
    epoch: Instant,
    kind: RtpCodecKind,
    track_id: String,
    encoding: RTCRtpEncodingParameters,
    ssrc: u32,
    payload_type: PayloadType,
    subscribers: Mutex<Vec<Subscriber>>,
    /// Chamado quando a fila de algum espectador enche (no vídeo: pede quadro completo).
    on_overflow: Option<Box<dyn Fn() + Send + Sync>>,
}

impl FanoutTrack {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        handle: Handle,
        kind: RtpCodecKind,
        track_id: String,
        encoding: RTCRtpEncodingParameters,
        ssrc: u32,
        payload_type: PayloadType,
        on_overflow: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            handle,
            epoch: Instant::now(),
            kind,
            track_id,
            encoding,
            ssrc,
            payload_type,
            subscribers: Mutex::new(Vec::new()),
            on_overflow,
        }
    }

    /// Cria a track de um espectador novo. A conexão guarda o `Arc`; quando ela é descartada,
    /// a tarefa de escrita percebe (Weak) e o espectador sai da lista na próxima amostra.
    pub fn subscribe(&self) -> Result<Arc<TrackLocalStaticSample>> {
        let track = Arc::new(TrackLocalStaticSample::new(
            self.epoch,
            MediaStreamTrack::new(
                "screencast-stream".to_owned(),
                self.track_id.clone(),
                self.track_id.clone(),
                self.kind,
                vec![self.encoding.clone()],
            ),
        )?);
        let (tx, mut rx) = mpsc::channel::<Sample>(QUEUE_CAPACITY);
        let weak: Weak<TrackLocalStaticSample> = Arc::downgrade(&track);
        let (ssrc, payload_type) = (self.ssrc, self.payload_type);
        self.handle.spawn(async move {
            while let Some(sample) = rx.recv().await {
                let Some(track) = weak.upgrade() else { break };
                if let Err(err) = track.sample_writer(ssrc, payload_type).write_sample(&sample).await {
                    // Antes da negociação terminar a escrita falha; é esperado.
                    debug!(%err, "amostra não entregue a um espectador");
                }
            }
        });
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Subscriber { tx });
        Ok(track)
    }

    /// Entrega a amostra a todos os espectadores sem bloquear.
    pub fn write(&self, sample: Sample) {
        let mut overflow = false;
        let mut subscribers = self.subscribers.lock().unwrap_or_else(PoisonError::into_inner);
        subscribers.retain(|sub| match sub.tx.try_send(copy_sample(&sample)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                overflow = true;
                true
            }
            // A tarefa de escrita terminou: a conexão do espectador foi descartada.
            Err(TrySendError::Closed(_)) => false,
        });
        drop(subscribers);
        if overflow {
            debug!("fila de um espectador cheia; amostra descartada");
            if let Some(callback) = &self.on_overflow {
                callback();
            }
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

/// `Sample` não implementa Clone; os dados (`Bytes`) são compartilhados, não copiados.
fn copy_sample(sample: &Sample) -> Sample {
    Sample {
        data: sample.data.clone(),
        timestamp: sample.timestamp,
        duration: sample.duration,
        packet_timestamp: sample.packet_timestamp,
        prev_dropped_packets: sample.prev_dropped_packets,
        prev_padding_packets: sample.prev_padding_packets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtc::broadcaster::{VIDEO_PAYLOAD_TYPE, video_codec};
    use ::rtc::rtp_transceiver::rtp_sender::RTCRtpCodingParameters;
    use bytes::Bytes;
    use std::time::Duration;

    fn fanout() -> FanoutTrack {
        let encoding = RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(42),
                ..Default::default()
            },
            codec: video_codec().rtp_codec,
            ..Default::default()
        };
        FanoutTrack::new(
            Handle::current(),
            RtpCodecKind::Video,
            "screencast-video-teste".to_owned(),
            encoding,
            42,
            VIDEO_PAYLOAD_TYPE,
            None,
        )
    }

    fn sample() -> Sample {
        let now = Instant::now();
        Sample {
            data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
            timestamp: now,
            duration: Duration::from_millis(33),
            ..Sample::new(now)
        }
    }

    /// Regressão: cada espectador precisa de uma track própria. Com a mesma track para todos,
    /// o webrtc 0.21 liga só a última conexão e o vídeo parava para os outros.
    #[tokio::test]
    async fn each_subscriber_gets_its_own_track() {
        let fanout = fanout();
        let a = fanout.subscribe().unwrap();
        let b = fanout.subscribe().unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(fanout.subscriber_count(), 2);
    }

    #[tokio::test]
    async fn dropped_subscriber_leaves_the_list() {
        let fanout = fanout();
        let kept = fanout.subscribe().unwrap();
        let gone = fanout.subscribe().unwrap();
        drop(gone);
        // A tarefa do espectador descartado termina na primeira amostra; a seguinte o remove.
        for _ in 0..20 {
            fanout.write(sample());
            tokio::time::sleep(Duration::from_millis(10)).await;
            if fanout.subscriber_count() == 1 {
                break;
            }
        }
        assert_eq!(fanout.subscriber_count(), 1);
        drop(kept);
    }
}
