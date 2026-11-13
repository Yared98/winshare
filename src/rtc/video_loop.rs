//! Laço de captura e codificação de vídeo da transmissão.
//! Recebe quadros da fonte (janela, monitor ou teste sintético), codifica em H.264
//! e envia amostras RTP para a FanoutTrack distribuída entre os espectadores.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::PoisonError;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::runtime::Handle;
use tracing::{debug, error, info};

use ::rtc::media::Sample;

use crate::capture::video::{CaptureDevice, CapturedFrame, WindowState};
use crate::config::VideoSettings;
use crate::encode::h264::{H264Encoder, even_size};
use crate::rtc::broadcaster::{BroadcasterShared, SourceStatus};
use crate::rtc::fanout::FanoutTrack;

pub const TEST_WIDTH: usize = 640;
pub const TEST_HEIGHT: usize = 480;
pub const TEST_FPS: u32 = 30;
const VIDEO_FPS: f32 = 30.0;
/// Duração de quadro usada quando o WGC ainda não deu dois carimbos.
const FALLBACK_FRAME: Duration = Duration::from_micros(33_333);
const WINDOW_POLL: Duration = Duration::from_millis(100);
const IDLE_REPEAT: Duration = Duration::from_millis(200);
pub const NOTICE_WINDOW_CLOSED: &str = "janela fechada";

/// Fonte de vídeo de uma transmissão.
pub enum VideoInput {
    Test,
    Capture(CaptureDevice),
}

/// Tudo que a thread de vídeo precisa, para não estourar a lista de argumentos.
pub struct VideoJob {
    pub handle: Handle,
    pub track: Arc<FanoutTrack>,
    pub ssrc: u32,
    pub input: VideoInput,
    pub settings: VideoSettings,
    pub stop: Arc<AtomicBool>,
    pub shared: Arc<BroadcasterShared>,
    pub status: Arc<SourceStatus>,
}

pub fn run_video(job: VideoJob) {
    let VideoJob {
        handle,
        track,
        ssrc,
        input,
        settings,
        stop,
        shared,
        status,
    } = job;
    let initial_bitrate_bps = settings.max_bps();
    match input {
        VideoInput::Test => run_test_video(&handle, &track, ssrc, initial_bitrate_bps, &stop, &shared),
        VideoInput::Capture(capture) => {
            run_capture_video(&handle, &track, ssrc, capture, initial_bitrate_bps, &stop, &shared, &status)
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
fn run_capture_video(
    handle: &Handle,
    track: &FanoutTrack,
    ssrc: u32,
    mut capture: CaptureDevice,
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
                info!("fonte de captura encerrada");
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

/// Dorme até `next` (agenda por relógio, sem acumular atraso). Se já passou, reancora.
pub(crate) fn pace(next: &mut Instant) {
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
