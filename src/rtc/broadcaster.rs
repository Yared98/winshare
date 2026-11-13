//! Transmissão única: um encode de vídeo e um de áudio, compartilhados por todos os peers.
//! Cada peer recebe as mesmas tracks, então o custo de encode não cresce com os espectadores.
//!
//! Fontes: a janela escolhida (WGC + Process Loopback) ou a fonte de teste (vídeo animado + tom).
//! Vídeo e áudio carimbam os quadros com `Instant` do mesmo processo, então usam um relógio comum.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::runtime::Handle;
use tracing::{info, warn};
use webrtc::media_stream::track_local::TrackLocal;

use ::rtc::rtp_transceiver::rtp_sender::RtpCodecKind;

use crate::capture::video::{CaptureDevice, MonitorCapture, WindowCapture, window_pid};
pub use crate::config::VideoSettings;
use crate::rtc::audio_loop::{AudioInput, TEST_TONE_HZ, run_audio};
use crate::rtc::bitrate::{BitrateController, StreamHealth};
pub use crate::rtc::codecs::{
    AUDIO_PAYLOAD_TYPE, VIDEO_PAYLOAD_TYPE, audio_codec, encoding, video_codec,
};
use crate::rtc::fanout::FanoutTrack;
pub use crate::rtc::video_loop::{
    NOTICE_WINDOW_CLOSED, TEST_FPS, TEST_HEIGHT, TEST_WIDTH, VideoInput, VideoJob, run_video,
    test_frame,
};

pub const NOTICE_MINIMIZED: &str =
    "janela minimizada: o Windows não envia quadros; restaure a janela para continuar";

/// Estado da fonte visível para a API: fim da captura e avisos.
#[derive(Default)]
pub struct SourceStatus {
    ended: AtomicBool,
    minimized: AtomicBool,
    notices: Mutex<Vec<String>>,
}

impl SourceStatus {
    pub fn add_notice(&self, notice: impl Into<String>) {
        let mut notices = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
        let notice = notice.into();
        if !notices.contains(&notice) {
            notices.push(notice);
        }
    }

    /// Encerra a transmissão por causa da fonte (ex.: janela fechada).
    pub fn end(&self, reason: impl Into<String>) {
        self.ended.store(true, Ordering::SeqCst);
        self.add_notice(reason);
    }

    pub fn set_minimized(&self, minimized: bool) {
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

pub struct BroadcasterShared {
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

pub struct Broadcaster {
    video: Arc<FanoutTrack>,
    audio: Arc<FanoutTrack>,
    stop: Arc<AtomicBool>,
    shared: Arc<BroadcasterShared>,
    status: Arc<SourceStatus>,
}

impl Broadcaster {
    /// Expõe o estado compartilhado da transmissão (bitrate, keyframe).
    pub fn shared(&self) -> Arc<BroadcasterShared> {
        self.shared.clone()
    }

    /// Inicia a fonte sintética (vídeo animado + tom senoidal) em threads próprias.
    /// Deve ser chamado dentro de um runtime tokio: o `Handle` é usado para escrever nas tracks.
    pub fn start_test_source(handle: Handle, settings: VideoSettings) -> Result<Self> {
        let broadcaster = Self::start(handle, VideoInput::Test, AudioInput::Tone, settings)?;
        info!(
            "fonte de teste iniciada ({TEST_WIDTH}x{TEST_HEIGHT}@{TEST_FPS} + tom de {TEST_TONE_HZ} Hz)"
        );
        Ok(broadcaster)
    }

    /// Inicia a captura real de uma janela (vídeo WGC) e o áudio do processo dela (Process Loopback).
    /// Se o áudio falhar, a transmissão segue só com vídeo e o aviso aparece em `notices`.
    pub fn start_window(handle: Handle, hwnd: u64, settings: VideoSettings) -> Result<Self> {
        let capture = WindowCapture::start(hwnd)
            .with_context(|| format!("não consegui capturar a janela {hwnd}"))?;
        let pid = window_pid(hwnd)?;
        let broadcaster = Self::start(
            handle,
            VideoInput::Capture(CaptureDevice::Window(capture)),
            AudioInput::Process(pid),
            settings,
        )?;
        info!(hwnd, pid, "captura da janela iniciada");
        Ok(broadcaster)
    }

    /// Inicia a captura de um monitor inteiro (vídeo WGC) e o áudio de todo o sistema (loopback padrão).
    /// Se o áudio falhar, a transmissão segue só com vídeo e o aviso aparece em `notices`.
    pub fn start_monitor(handle: Handle, monitor_id: u64, settings: VideoSettings) -> Result<Self> {
        let capture = MonitorCapture::start(monitor_id)
            .with_context(|| format!("não consegui capturar o monitor {monitor_id}"))?;
        let broadcaster = Self::start(
            handle,
            VideoInput::Capture(CaptureDevice::Monitor(capture)),
            AudioInput::System,
            settings,
        )?;
        info!(monitor_id, "captura do monitor iniciada");
        Ok(broadcaster)
    }

    fn start(
        handle: Handle,
        video_input: VideoInput,
        audio_input: AudioInput,
        settings: VideoSettings,
    ) -> Result<Self> {
        let video_ssrc: u32 = rand::random();
        let audio_ssrc: u32 = rand::random();

        let min_kbps = settings.min_kbps;
        let max_kbps = settings.max_kbps.max(min_kbps);
        let keyframe_interval = settings.keyframe_interval;
        let bitrate_controller = BitrateController::new(min_kbps, max_kbps);

        let shared = Arc::new(BroadcasterShared {
            keyframe: Arc::new(AtomicBool::new(false)),
            last_keyframe_request: Mutex::new(Instant::now() - Duration::from_secs(1)),
            bitrate: Arc::new(Mutex::new(bitrate_controller)),
            keyframe_interval,
        });

        let video_track_label = "screencast-video".to_owned();
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
            settings,
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
    }
}
