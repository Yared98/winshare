//! Transmissão única: um encode de vídeo e um de áudio, compartilhados por todos os peers.
//! Cada peer recebe as mesmas tracks, então o custo de encode não cresce com os espectadores.
//!
//! Fontes: a janela escolhida (WGC + Process Loopback) ou a fonte de teste (vídeo animado + tom).
//! Vídeo e áudio carimbam os quadros com `Instant` do mesmo processo, então usam um relógio comum.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::runtime::Handle;
use tracing::{debug, error, info, warn};
use webrtc::media_stream::track_local::TrackLocal;

use ::rtc::media::Sample;
use ::rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MIME_TYPE_OPUS};
use ::rtc::rtp_transceiver::PayloadType;
use ::rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};

use crate::capture::audio::{AudioCapture, CHANNELS, SAMPLE_RATE, SineSource};
use crate::capture::video::{CapturedFrame, WindowCapture, WindowState, window_pid};
use crate::encode::h264::{H264Encoder, even_size};
use crate::encode::opus::OpusEncoder;
use crate::rtc::bitrate::{BitrateController, StreamHealth};
use crate::rtc::fanout::FanoutTrack;

/// Payload types negociados (iguais em todos os peers, pois o MediaEngine é o mesmo).
pub const VIDEO_PAYLOAD_TYPE: PayloadType = 102;
pub const AUDIO_PAYLOAD_TYPE: PayloadType = 120;

pub const TEST_WIDTH: usize = 640;
pub const TEST_HEIGHT: usize = 480;
pub const TEST_FPS: u32 = 30;
const TEST_TONE_HZ: f64 = 440.0;
const TEST_TONE_AMPLITUDE: f64 = 0.2;
const AUDIO_FRAME: Duration = Duration::from_millis(20);
const VIDEO_FPS: f32 = 30.0;
/// Duração de quadro usada quando o WGC ainda não deu dois carimbos.
const FALLBACK_FRAME: Duration = Duration::from_micros(33_333);
const WINDOW_POLL: Duration = Duration::from_millis(100);
const IDLE_REPEAT: Duration = Duration::from_millis(200);
pub const NOTICE_WINDOW_CLOSED: &str = "janela fechada";
pub const NOTICE_MINIMIZED: &str =
    "janela minimizada: o Windows não envia quadros; restaure a janela para continuar";

/// Codec H.264 (Constrained Baseline, packetization-mode=1).
pub fn video_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![
                RTCPFeedback {
                    typ: "goog-remb".to_owned(),
                    parameter: "".to_owned(),
                },
                RTCPFeedback {
                    typ: "ccm".to_owned(),
                    parameter: "fir".to_owned(),
                },
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "".to_owned(),
                },
                RTCPFeedback {
                    typ: "nack".to_owned(),
                    parameter: "pli".to_owned(),
                },
            ],
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

/// Estado da fonte visível para a API: fim da captura e avisos.
#[derive(Default)]
pub struct SourceStatus {
    ended: AtomicBool,
    minimized: AtomicBool,
    notices: Mutex<Vec<String>>,
}

impl SourceStatus {
    fn add_notice(&self, notice: impl Into<String>) {
        let mut notices = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
        let notice = notice.into();
        if !notices.contains(&notice) {
            notices.push(notice);
        }
    }

    /// Encerra a transmissão por causa da fonte (ex.: janela fechada).
    fn end(&self, reason: impl Into<String>) {
        self.ended.store(true, Ordering::SeqCst);
        self.add_notice(reason);
    }

    fn set_minimized(&self, minimized: bool) {
        self.minimized.store(minimized, Ordering::SeqCst);
    }

    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    pub fn notices(&self) -> Vec<String> {
        let mut notices = self
            .notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if self.minimized.load(Ordering::SeqCst) {
            notices.push(NOTICE_MINIMIZED.to_owned());
        }
        notices
    }
}

pub struct Broadcaster {
    video: Arc<FanoutTrack>,
    audio: Arc<FanoutTrack>,
    stop: Arc<AtomicBool>,
    shared: Arc<BroadcasterShared>,
    status: Arc<SourceStatus>,
}

static NEXT_BROADCASTER_ID: AtomicU64 = AtomicU64::new(1);
static BROADCASTERS: Mutex<Option<HashMap<u64, Weak<BroadcasterShared>>>> = Mutex::new(None);

pub fn get_broadcaster(id: u64) -> Option<Arc<BroadcasterShared>> {
    let guard = BROADCASTERS.lock().unwrap_or_else(PoisonError::into_inner);
    guard.as_ref().and_then(|map| map.get(&id)).and_then(|w| w.upgrade())
}

fn register_broadcaster(id: u64, weak: Weak<BroadcasterShared>) {
    let mut guard = BROADCASTERS.lock().unwrap_or_else(PoisonError::into_inner);
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(id, weak);
}

fn unregister_broadcaster(id: u64) {
    let mut guard = BROADCASTERS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(map) = guard.as_mut() {
        map.remove(&id);
    }
}

pub struct BroadcasterShared {
    pub id: u64,
    pub keyframe: Arc<AtomicBool>,
    pub last_keyframe_request: Mutex<Instant>,
    pub bitrate: Arc<Mutex<BitrateController>>,
    pub keyframe_interval: Duration,
}

impl BroadcasterShared {
    /// Pede um keyframe no próximo quadro (viewer novo ou PLI/FIR).
    /// No máximo 1 a cada 500 ms (no total).
    pub fn request_keyframe(&self) {
        let now = Instant::now();
        let mut last = self.last_keyframe_request.lock().unwrap_or_else(PoisonError::into_inner);
        if now.duration_since(*last) >= Duration::from_millis(500) {
            *last = now;
            self.keyframe.store(true, Ordering::SeqCst);
        }
    }

    pub fn report_loss(&self, peer_id: u64, loss_pct: f64) {
        let mut b = self.bitrate.lock().unwrap_or_else(PoisonError::into_inner);
        b.update_loss(peer_id, loss_pct, Instant::now());
    }

    pub fn add_peer(&self, peer_id: u64) {
        let mut b = self.bitrate.lock().unwrap_or_else(PoisonError::into_inner);
        b.add_peer(peer_id);
    }

    pub fn remove_peer(&self, peer_id: u64) {
        let mut b = self.bitrate.lock().unwrap_or_else(PoisonError::into_inner);
        b.remove_peer(peer_id, Instant::now());
    }

    pub fn health(&self) -> StreamHealth {
        self.bitrate.lock().unwrap_or_else(PoisonError::into_inner).health()
    }
}

/// Fonte de vídeo de uma transmissão.
enum VideoInput {
    Test,
    Window(WindowCapture),
}

/// Fonte de áudio de uma transmissão.
enum AudioInput {
    Tone,
    /// Process Loopback do processo dono da janela.
    Process(u32),
}

/// Tudo que a thread de vídeo precisa, para não estourar a lista de argumentos.
struct VideoJob {
    handle: Handle,
    track: Arc<FanoutTrack>,
    ssrc: u32,
    input: VideoInput,
    bitrate_bps: u32,
    stop: Arc<AtomicBool>,
    shared: Arc<BroadcasterShared>,
    status: Arc<SourceStatus>,
}

impl Broadcaster {
    /// Inicia a fonte sintética (vídeo animado + tom senoidal) em threads próprias.
    /// Deve ser chamado dentro de um runtime tokio: o `Handle` é usado para escrever nas tracks.
    pub fn start_test_source(handle: Handle, bitrate_bps: u32) -> Result<Self> {
        let broadcaster = Self::start(handle, VideoInput::Test, AudioInput::Tone, bitrate_bps)?;
        info!(
            "fonte de teste iniciada ({TEST_WIDTH}x{TEST_HEIGHT}@{TEST_FPS} + tom de {TEST_TONE_HZ} Hz)"
        );
        Ok(broadcaster)
    }

    /// Inicia a captura real de uma janela (vídeo WGC) e o áudio do processo dela (Process Loopback).
    /// Se o áudio falhar, a transmissão segue só com vídeo e o aviso aparece em `notices`.
    pub fn start_window(handle: Handle, hwnd: u64, bitrate_bps: u32) -> Result<Self> {
        let capture = WindowCapture::start(hwnd)
            .with_context(|| format!("não consegui capturar a janela {hwnd}"))?;
        let pid = window_pid(hwnd)?;
        let broadcaster = Self::start(handle, VideoInput::Window(capture), AudioInput::Process(pid), bitrate_bps)?;
        info!(hwnd, pid, "captura da janela iniciada");
        Ok(broadcaster)
    }

    fn start(handle: Handle, video_input: VideoInput, audio_input: AudioInput, bitrate_bps: u32) -> Result<Self> {
        let video_ssrc: u32 = rand::random();
        let audio_ssrc: u32 = rand::random();

        let cfg = crate::config::last_loaded();
        let (min_kbps, kf_secs) = cfg
            .as_ref()
            .map(|c| (c.video_min_bitrate_kbps, c.keyframe_interval_secs))
            .unwrap_or_else(|| (800.min(bitrate_bps / 1000), 5));
        let max_kbps = (bitrate_bps / 1000).max(min_kbps);
        let keyframe_interval = Duration::from_secs(kf_secs as u64);
        let bitrate_controller = BitrateController::new(min_kbps, max_kbps);

        let id = NEXT_BROADCASTER_ID.fetch_add(1, Ordering::SeqCst);
        let shared = Arc::new(BroadcasterShared {
            id,
            keyframe: Arc::new(AtomicBool::new(false)),
            last_keyframe_request: Mutex::new(Instant::now() - Duration::from_secs(1)),
            bitrate: Arc::new(Mutex::new(bitrate_controller)),
            keyframe_interval,
        });
        register_broadcaster(id, Arc::downgrade(&shared));

        let video_track_label = format!("screencast-video-{id}");
        // Espectador lento (fila cheia) perde quadros: pede um quadro completo para ele se recuperar.
        let weak_shared = Arc::downgrade(&shared);
        let video = Arc::new(FanoutTrack::new(
            handle.clone(),
            RtpCodecKind::Video,
            video_track_label,
            encoding(video_ssrc, video_codec()),
            video_ssrc,
            VIDEO_PAYLOAD_TYPE,
            Some(Box::new(move || {
                if let Some(shared) = weak_shared.upgrade() {
                    shared.request_keyframe();
                }
            })),
        ));
        let audio = Arc::new(FanoutTrack::new(
            handle.clone(),
            RtpCodecKind::Audio,
            "screencast-audio".to_owned(),
            encoding(audio_ssrc, audio_codec()),
            audio_ssrc,
            AUDIO_PAYLOAD_TYPE,
            None,
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(SourceStatus::default());

        let job = VideoJob {
            handle: handle.clone(),
            track: video.clone(),
            ssrc: video_ssrc,
            input: video_input,
            bitrate_bps,
            stop: stop.clone(),
            shared: shared.clone(),
            status: status.clone(),
        };
        thread::Builder::new()
            .name("video-source".to_owned())
            .spawn(move || run_video(job))
            .context("não consegui iniciar a thread de vídeo")?;

        {
            let (track, stop, status) = (audio.clone(), stop.clone(), status.clone());
            thread::Builder::new()
                .name("audio-source".to_owned())
                .spawn(move || run_audio(handle, track, audio_ssrc, audio_input, stop, status))
                .context("não consegui iniciar a thread de áudio")?;
        }

        Ok(Self {
            video,
            audio,
            stop,
            shared,
            status,
        })
    }

    /// Tracks NOVAS para um peer (uma por espectador; o encoder é o mesmo para todos).
    /// Vazio só se a criação da track falhar, o que não acontece com codecs já validados.
    pub fn tracks(&self) -> Vec<Arc<dyn TrackLocal>> {
        let mut tracks: Vec<Arc<dyn TrackLocal>> = Vec::with_capacity(2);
        for fanout in [&self.video, &self.audio] {
            match fanout.subscribe() {
                Ok(track) => tracks.push(track),
                Err(err) => warn!(error = %format!("{err:#}"), "não consegui criar a track do espectador"),
            }
        }
        tracks
    }

    /// Quantas conexões estão inscritas no vídeo (inclui o preview do painel).
    pub fn subscriber_count(&self) -> usize {
        self.video.subscriber_count()
    }

    /// Retorna as métricas de saúde da transmissão (taxa-alvo e pior perda atual).
    pub fn health(&self) -> StreamHealth {
        self.shared.health()
    }

    /// Pede um keyframe no próximo quadro (viewer novo ou PLI/FIR).
    /// Throttle: no máximo 1 a cada 500 ms (no total).
    pub fn request_keyframe(&self) {
        self.shared.request_keyframe();
    }

    /// `true` quando a fonte encerrou a transmissão (ex.: janela fechada).
    pub fn ended(&self) -> bool {
        self.status.ended()
    }

    pub fn notices(&self) -> Vec<String> {
        self.status.notices()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for Broadcaster {
    fn drop(&mut self) {
        self.stop();
        unregister_broadcaster(self.shared.id);
    }
}

fn run_video(job: VideoJob) {
    let VideoJob {
        handle,
        track,
        ssrc,
        input,
        bitrate_bps,
        stop,
        shared,
        status,
    } = job;
    match input {
        VideoInput::Test => run_test_video(&handle, &track, ssrc, bitrate_bps, &stop, &shared),
        VideoInput::Window(capture) => {
            run_window_video(&handle, &track, ssrc, capture, bitrate_bps, &stop, &shared, &status)
        }
    }
    debug!("thread de vídeo encerrada");
}

fn run_test_video(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    initial_bitrate_bps: u32,
    stop: &AtomicBool,
    shared: &BroadcasterShared,
) {
    let keyframe_interval = shared.keyframe_interval;
    let mut current_bitrate_bps = initial_bitrate_bps;
    let mut encoder = match H264Encoder::with_keyframe_interval(TEST_WIDTH, TEST_HEIGHT, current_bitrate_bps, TEST_FPS as f32, keyframe_interval) {
        Ok(encoder) => encoder,
        Err(err) => {
            error!(error = %format!("{err:#}"), "encoder de vídeo não iniciou");
            return;
        }
    };
    let frame = Duration::from_secs_f64(1.0 / TEST_FPS as f64);
    let mut next = Instant::now();
    let mut index: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        let target_kbps = {
            let mut b = shared.bitrate.lock().unwrap_or_else(PoisonError::into_inner);
            b.tick(now);
            b.target_kbps()
        };
        let target_bps = target_kbps * 1000;
        if target_bps != current_bitrate_bps {
            info!(old_kbps = current_bitrate_bps / 1000, new_kbps = target_kbps, "taxa de vídeo ajustada; recriando encoder H.264");
            current_bitrate_bps = target_bps;
            match H264Encoder::with_keyframe_interval(TEST_WIDTH, TEST_HEIGHT, current_bitrate_bps, TEST_FPS as f32, keyframe_interval) {
                Ok(mut new_encoder) => {
                    new_encoder.force_keyframe();
                    encoder = new_encoder;
                }
                Err(err) => {
                    error!(error = %format!("{err:#}"), "falha ao recriar encoder com nova taxa");
                }
            }
        }

        if shared.keyframe.swap(false, Ordering::SeqCst) {
            encoder.force_keyframe();
        }
        let bgra = test_frame(index, TEST_WIDTH, TEST_HEIGHT);
        match encoder.encode_bgra(&bgra, TEST_WIDTH * 4) {
            Ok(data) if !data.is_empty() => {
                write_video(handle, track, ssrc, data, Instant::now(), frame);
            }
            Ok(_) => {}
            Err(err) => {
                error!(error = %format!("{err:#}"), "encoder de vídeo parou");
                break;
            }
        }
        index += 1;
        next += frame;
        pace(&mut next);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_window_video(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    mut capture: WindowCapture,
    initial_bitrate_bps: u32,
    stop: &AtomicBool,
    shared: &BroadcasterShared,
    status: &SourceStatus,
) {
    let keyframe_interval = shared.keyframe_interval;
    let mut current_bitrate_bps = initial_bitrate_bps;
    let mut encoder: Option<H264Encoder> = None;
    let mut last_stamp: Option<i64> = None;
    let mut last_frame: Option<CapturedFrame> = None;
    let mut last_sent = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        match capture.state() {
            WindowState::Closed => {
                info!("janela fechada: encerrando a transmissão");
                status.end(NOTICE_WINDOW_CLOSED);
                break;
            }
            WindowState::Minimized => status.set_minimized(true),
            WindowState::Running => status.set_minimized(false),
        }

        let now = Instant::now();
        let target_kbps = {
            let mut b = shared.bitrate.lock().unwrap_or_else(PoisonError::into_inner);
            b.tick(now);
            b.target_kbps()
        };
        let target_bps = target_kbps * 1000;
        let bitrate_changed = target_bps != current_bitrate_bps;
        if bitrate_changed {
            info!(old_kbps = current_bitrate_bps / 1000, new_kbps = target_kbps, "taxa de vídeo ajustada; recriando encoder H.264");
            current_bitrate_bps = target_bps;
        }

        // O WGC só entrega quadros quando a janela muda. Sem novidade, reenvia o último quadro
        // (ao menos a cada IDLE_REPEAT, ou na hora se há keyframe pedido), senão a imagem congela
        // e um espectador novo nunca recebe um IDR.
        let frame = match capture.next_frame(WINDOW_POLL) {
            Some(frame) => frame,
            None => match last_frame.take() {
                Some(mut frame)
                    if last_sent.elapsed() >= IDLE_REPEAT || shared.keyframe.load(Ordering::SeqCst) =>
                {
                    let elapsed_100ns = (last_sent.elapsed().as_nanos() / 100) as i64;
                    frame.timestamp_100ns = last_stamp.unwrap_or(frame.timestamp_100ns) + elapsed_100ns;
                    frame.at = Instant::now();
                    frame
                }
                other => {
                    last_frame = other;
                    continue;
                }
            },
        };
        last_sent = Instant::now();
        let (width, height) = even_size(frame.width, frame.height);
        if frame.width < 2 || frame.height < 2 {
            continue;
        }
        if encoder.as_ref().map(H264Encoder::dimensions) != Some((width, height)) || bitrate_changed {
            // Primeiro quadro, janela redimensionada ou taxa alterada: encoder novo, sempre com IDR.
            info!(width, height, bitrate_kbps = current_bitrate_bps / 1000, "dimensões ou taxa de captura: (re)criando encoder H.264");
            match H264Encoder::with_keyframe_interval(width, height, current_bitrate_bps, VIDEO_FPS, keyframe_interval) {
                Ok(mut new_encoder) => {
                    new_encoder.force_keyframe();
                    encoder = Some(new_encoder);
                }
                Err(err) => {
                    status.end(format!("encoder de vídeo: {err:#}"));
                    break;
                }
            }
        }
        let Some(enc) = encoder.as_mut() else { continue };
        if shared.keyframe.swap(false, Ordering::SeqCst) {
            enc.force_keyframe();
        }
        let duration = frame_duration(last_stamp, frame.timestamp_100ns);
        last_stamp = Some(frame.timestamp_100ns);
        let stride = frame.width as usize * 4;
        match enc.encode_bgra(&frame.bgra, stride) {
            Ok(data) if !data.is_empty() => write_video(handle, track, ssrc, data, frame.at, duration),
            Ok(_) => {}
            Err(err) => {
                status.end(format!("encoder de vídeo: {err:#}"));
                break;
            }
        }
        last_frame = Some(frame);
    }
    let dropped = capture.dropped_frames();
    if dropped > 0 {
        debug!(dropped, "quadros descartados (encoder atrasado)");
    }
    capture.stop();
}

/// Duração de um quadro a partir dos carimbos do WGC (100 ns). Limitada para não virar zero nem travar.
fn frame_duration(previous: Option<i64>, now: i64) -> Duration {
    match previous {
        Some(prev) if now > prev => Duration::from_nanos(((now - prev) as u64).saturating_mul(100))
            .clamp(Duration::from_millis(1), Duration::from_millis(200)),
        _ => FALLBACK_FRAME,
    }
}

fn write_video(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    data: Vec<u8>,
    at: Instant,
    duration: Duration,
) {
    let sample = Sample {
        data: Bytes::from(data),
        timestamp: at,
        duration,
        ..Sample::new(at)
    };
    let _ = (handle, ssrc);
    track.write(sample);
}

fn run_audio(
    handle: Handle,
    track: Arc<FanoutTrack>,
    ssrc: u32,
    input: AudioInput,
    stop: Arc<AtomicBool>,
    status: Arc<SourceStatus>,
) {
    let encoder = match OpusEncoder::new() {
        Ok(encoder) => encoder,
        Err(err) => {
            error!(error = %format!("{err:#}"), "encoder de áudio não iniciou");
            return;
        }
    };
    match input {
        AudioInput::Tone => run_tone_audio(&handle, &track, ssrc, encoder, &stop),
        AudioInput::Process(pid) => run_process_audio(&handle, &track, ssrc, encoder, pid, &stop, &status),
    }
    debug!("thread de áudio encerrada");
}

fn run_tone_audio(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    mut encoder: OpusEncoder,
    stop: &AtomicBool,
) {
    let mut tone = SineSource::new(TEST_TONE_HZ, TEST_TONE_AMPLITUDE);
    let mut next = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        let pcm = tone.next_frame();
        match encoder.encode(&pcm) {
            Ok(data) => write_audio(handle, track, ssrc, data, Instant::now()),
            Err(err) => {
                error!(error = %format!("{err:#}"), "encoder de áudio parou");
                break;
            }
        }
        next += AUDIO_FRAME;
        pace(&mut next);
    }
}

fn run_process_audio(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    mut encoder: OpusEncoder,
    pid: u32,
    stop: &AtomicBool,
    status: &SourceStatus,
) {
    // A captura fica viva enquanto esta função roda; ao sair, o Drop sinaliza a thread do WASAPI.
    let (_capture, chunks) = match AudioCapture::start(pid) {
        Ok(started) => started,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "sem áudio do processo; transmitindo só vídeo");
            status.add_notice(format!("sem áudio do processo: {err:#}"));
            return;
        }
    };
    while !stop.load(Ordering::SeqCst) {
        match chunks.recv_timeout(Duration::from_millis(100)) {
            Ok(pcm) => match encoder.encode(&pcm) {
                Ok(data) => write_audio(handle, track, ssrc, data, Instant::now()),
                Err(err) => {
                    error!(error = %format!("{err:#}"), "encoder de áudio parou");
                    break;
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn write_audio(handle: &Handle, track: &FanoutTrack, ssrc: u32, data: Vec<u8>, at: Instant) {
    let sample = Sample {
        data: Bytes::from(data),
        timestamp: at,
        duration: AUDIO_FRAME,
        ..Sample::new(at)
    };
    let _ = (handle, ssrc);
    track.write(sample);
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

/// Quadro BGRA de teste (`width * height * 4` bytes): gradiente que se move, quadrado branco e barra de progresso.
pub fn test_frame(index: u64, width: usize, height: usize) -> Vec<u8> {
    let mut bgra = vec![0u8; width * height * 4];
    let shift = (index as usize * 4) % 256;
    let square = 80usize;
    let square_x = (index as usize * 6) % (width - square);
    let square_y = height / 3;
    let bar_len = (index as usize * 8) % width;

    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let (mut r, mut g, mut b) = (
                ((x + shift) % 256) as u8,
                (y * 255 / height) as u8,
                ((x + y + shift * 2) % 256) as u8,
            );
            if x >= square_x && x < square_x + square && y >= square_y && y < square_y + square {
                r = 255;
                g = 255;
                b = 255;
            }
            if y >= height - 16 && x <= bar_len {
                r = 255;
                g = 200;
                b = 0;
            }
            bgra[offset] = b;
            bgra[offset + 1] = g;
            bgra[offset + 2] = r;
            bgra[offset + 3] = 255;
        }
    }
    bgra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_duration_uses_capture_stamps() {
        // 333_333 unidades de 100 ns = 33,3 ms.
        assert_eq!(frame_duration(Some(0), 333_333), Duration::from_nanos(33_333_300));
        assert_eq!(frame_duration(None, 0), FALLBACK_FRAME);
        assert_eq!(frame_duration(Some(10), 10), FALLBACK_FRAME, "carimbo repetido");
        assert_eq!(frame_duration(Some(0), 1), Duration::from_millis(1), "piso de 1 ms");
        assert_eq!(frame_duration(Some(0), 100_000_000), Duration::from_millis(200), "teto de 200 ms");
    }

    #[test]
    fn test_frame_is_bgra_sized() {
        let frame = test_frame(3, 128, 96);
        assert_eq!(frame.len(), 128 * 96 * 4);
        assert!(frame.chunks(4).all(|px| px[3] == 255));
    }
}
