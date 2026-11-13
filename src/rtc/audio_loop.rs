//! Laço de captura e codificação de áudio da transmissão.
//! Captura áudio do processo alvo, do sistema ou gera tom senoidal de teste,
//! codifica em Opus a 48 kHz e alimenta a FanoutTrack correspondente.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::runtime::Handle;
use tracing::{debug, error, warn};

use ::rtc::media::Sample;

use crate::capture::audio::{AudioCapture, SineSource};
use crate::encode::opus::OpusEncoder;
use crate::rtc::broadcaster::SourceStatus;
use crate::rtc::fanout::FanoutTrack;
use crate::rtc::video_loop::pace;

pub const AUDIO_FRAME: Duration = Duration::from_millis(20);
pub const TEST_TONE_HZ: f64 = 440.0;
const TEST_TONE_AMPLITUDE: f64 = 0.2;

/// Fonte de áudio de uma transmissão.
pub enum AudioInput {
    Tone,
    /// Process Loopback do processo dono da janela.
    Process(u32),
    /// Loopback do áudio do sistema inteiro (dispositivo padrão de saída).
    System,
}

pub fn run_audio(
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
        AudioInput::System => run_system_audio(&handle, &track, ssrc, encoder, &stop, &status),
    }
    debug!("thread de áudio encerrada");
}

fn run_system_audio(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    mut encoder: OpusEncoder,
    stop: &AtomicBool,
    status: &SourceStatus,
) {
    let (_capture, chunks) = match AudioCapture::start_system() {
        Ok(started) => started,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "sem áudio do sistema; transmitindo só vídeo");
            status.add_notice(format!("sem áudio do sistema: {err:#}"));
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
