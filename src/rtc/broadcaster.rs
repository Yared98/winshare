//! Transmissão única: um encode de vídeo e um de áudio, compartilhados por todos os peers.
//! Cada peer recebe as mesmas tracks, então o custo de encode não cresce com os espectadores.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::runtime::Handle;
use tracing::{debug, error, info};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;

use ::rtc::media::Sample;
use ::rtc::media_stream::MediaStreamTrack;
use ::rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use ::rtc::rtp_transceiver::PayloadType;
use ::rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};

use crate::capture::audio::{CHANNELS, SAMPLE_RATE, SineSource};
use crate::capture::video::test_frame;
use crate::encode::h264::H264Encoder;
use crate::encode::opus::OpusEncoder;

/// Payload types negociados (iguais em todos os peers, pois o MediaEngine é o mesmo).
pub const VIDEO_PAYLOAD_TYPE: PayloadType = 102;
pub const AUDIO_PAYLOAD_TYPE: PayloadType = 120;

pub const TEST_WIDTH: usize = 640;
pub const TEST_HEIGHT: usize = 480;
pub const TEST_FPS: u32 = 30;
const TEST_TONE_HZ: f64 = 440.0;
const TEST_TONE_AMPLITUDE: f64 = 0.2;
const AUDIO_FRAME: Duration = Duration::from_millis(20);

/// Codec H.264 (Constrained Baseline, packetization-mode=1).
pub fn video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: VIDEO_PAYLOAD_TYPE,
        ..Default::default()
    }
}

/// Codec Opus estéreo a 48 kHz.
pub fn audio_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: SAMPLE_RATE,
            channels: CHANNELS as u16,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: AUDIO_PAYLOAD_TYPE,
        ..Default::default()
    }
}

fn encoding(ssrc: u32, codec: RTCRtpCodecParameters) -> RTCRtpEncodingParameters {
    RTCRtpEncodingParameters {
        rtp_coding_parameters: RTCRtpCodingParameters {
            ssrc: Some(ssrc),
            ..Default::default()
        },
        codec: codec.rtp_codec,
        ..Default::default()
    }
}

pub struct Broadcaster {
    video: Arc<TrackLocalStaticSample>,
    audio: Arc<TrackLocalStaticSample>,
    stop: Arc<AtomicBool>,
}

impl Broadcaster {
    /// Inicia a fonte sintética (vídeo animado + tom senoidal) em threads próprias.
    /// Deve ser chamado dentro de um runtime tokio: o `Handle` é usado para escrever nas tracks.
    pub fn start_test_source(handle: Handle) -> Result<Self> {
        let video_ssrc: u32 = rand::random();
        let audio_ssrc: u32 = rand::random();
        let video = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "screencast-stream".to_owned(),
                "screencast-video".to_owned(),
                "screencast-video".to_owned(),
                RtpCodecKind::Video,
                vec![encoding(video_ssrc, video_codec())],
            ),
        )?);
        let audio = Arc::new(TrackLocalStaticSample::new(
            Instant::now(),
            MediaStreamTrack::new(
                "screencast-stream".to_owned(),
                "screencast-audio".to_owned(),
                "screencast-audio".to_owned(),
                RtpCodecKind::Audio,
                vec![encoding(audio_ssrc, audio_codec())],
            ),
        )?);

        let stop = Arc::new(AtomicBool::new(false));
        let video_encoder = H264Encoder::new(TEST_WIDTH, TEST_HEIGHT)?;
        let audio_encoder = OpusEncoder::new()?;

        {
            let (track, stop, handle) = (video.clone(), stop.clone(), handle.clone());
            thread::Builder::new()
                .name("video-source".to_owned())
                .spawn(move || run_video(handle, track, video_ssrc, video_encoder, stop))
                .context("não consegui iniciar a thread de vídeo")?;
        }
        {
            let (track, stop) = (audio.clone(), stop.clone());
            thread::Builder::new()
                .name("audio-source".to_owned())
                .spawn(move || run_audio(handle, track, audio_ssrc, audio_encoder, stop))
                .context("não consegui iniciar a thread de áudio")?;
        }
        info!(
            "fonte de teste iniciada ({TEST_WIDTH}x{TEST_HEIGHT}@{TEST_FPS} + tom de {TEST_TONE_HZ} Hz)"
        );
        Ok(Self { video, audio, stop })
    }

    /// Tracks adicionadas a cada peer (as mesmas instâncias para todos).
    pub fn tracks(&self) -> Vec<Arc<dyn TrackLocal>> {
        vec![
            self.video.clone() as Arc<dyn TrackLocal>,
            self.audio.clone() as Arc<dyn TrackLocal>,
        ]
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Broadcaster {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_video(
    handle: Handle,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    mut encoder: H264Encoder,
    stop: Arc<AtomicBool>,
) {
    let frame = Duration::from_secs_f64(1.0 / TEST_FPS as f64);
    let mut next = Instant::now();
    let mut index: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        if index % TEST_FPS as u64 == 0 {
            encoder.force_keyframe();
        }
        let rgb = test_frame(index, TEST_WIDTH, TEST_HEIGHT);
        match encoder.encode_rgb(&rgb) {
            Ok(data) if !data.is_empty() => {
                let sample = Sample {
                    data: Bytes::from(data),
                    timestamp: Instant::now(),
                    duration: frame,
                    ..Sample::new(Instant::now())
                };
                let writer = track.sample_writer(ssrc, VIDEO_PAYLOAD_TYPE);
                if let Err(err) = handle.block_on(writer.write_sample(&sample)) {
                    // Sem viewers conectados a escrita falha; é esperado.
                    debug!(%err, "quadro de vídeo não entregue");
                }
            }
            Ok(_) => {}
            Err(err) => {
                error!(error = %err, "encoder de vídeo parou");
                break;
            }
        }
        index += 1;
        next += frame;
        pace(&mut next);
    }
    debug!("thread de vídeo encerrada");
}

fn run_audio(
    handle: Handle,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    mut encoder: OpusEncoder,
    stop: Arc<AtomicBool>,
) {
    let mut tone = SineSource::new(TEST_TONE_HZ, TEST_TONE_AMPLITUDE);
    let mut next = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let pcm = tone.next_frame();
        match encoder.encode(&pcm) {
            Ok(data) => {
                let sample = Sample {
                    data: Bytes::from(data),
                    timestamp: Instant::now(),
                    duration: AUDIO_FRAME,
                    ..Sample::new(Instant::now())
                };
                let writer = track.sample_writer(ssrc, AUDIO_PAYLOAD_TYPE);
                if let Err(err) = handle.block_on(writer.write_sample(&sample)) {
                    debug!(%err, "quadro de áudio não entregue");
                }
            }
            Err(err) => {
                error!(error = %err, "encoder de áudio parou");
                break;
            }
        }
        next += AUDIO_FRAME;
        pace(&mut next);
    }
    debug!("thread de áudio encerrada");
}

/// Dorme até `next` (agenda por relógio, sem acumular atraso). Se já passou, reancora.
fn pace(next: &mut Instant) {
    let now = Instant::now();
    if *next > now {
        thread::sleep(*next - now);
    } else {
        *next = now;
    }
}
