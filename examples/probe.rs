//! Diagnóstico: captura 5 s de uma janela (vídeo WGC + áudio do processo) e imprime os números.
//! Uso: `cargo run --example probe -- <hwnd>` (hwnd decimal ou 0x...). Rode com uma janela real aberta.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use screencast::capture::audio::{AudioCapture, CHUNK_SAMPLES};
use screencast::capture::video::{WindowCapture, window_pid};
use screencast::encode::h264::{H264Encoder, even_size};

const RUN_FOR: Duration = Duration::from_secs(5);

fn parse_hwnd(text: &str) -> Result<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).context("hwnd hexadecimal inválido"),
        None => text.parse().context("hwnd inválido"),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let arg = std::env::args()
        .nth(1)
        .context("uso: cargo run --example probe -- <hwnd>")?;
    let hwnd = parse_hwnd(&arg)?;
    // Diagnóstico: `--no-encode` pula o H.264 e `--no-audio` pula o loopback.
    let flags: Vec<String> = std::env::args().skip(2).collect();
    let (do_encode, do_audio) = (!flags.iter().any(|f| f == "--no-encode"), !flags.iter().any(|f| f == "--no-audio"));
    let pid = window_pid(hwnd)?;
    println!("janela {hwnd} (0x{hwnd:x}), processo {pid}");

    let mut video = WindowCapture::start(hwnd)?;
    let audio_started = if do_audio { Some(AudioCapture::start(pid)) } else { None };
    let (audio, chunks) = match audio_started {
        Some(Ok((audio, chunks))) => (Some(audio), Some(chunks)),
        Some(Err(err)) => {
            video.stop();
            bail!("áudio do processo indisponível: {err:#}");
        }
        None => (None, None),
    };

    let started = Instant::now();
    let mut frames = 0u64;
    let mut size = (0u32, 0u32);
    let mut encoder: Option<H264Encoder> = None;
    let mut encoded_bytes = 0usize;
    let mut encode_time = Duration::ZERO;
    let mut audio_chunks = 0u64;
    let mut audio_samples = 0u64;
    let mut audio_peak = 0i16;
    let mut audio_nonzero = 0u64;
    let mut audio_energy = 0f64;
    let mut audio_head: Vec<i16> = Vec::new();

    while started.elapsed() < RUN_FOR {
        if let Some(frame) = video.next_frame(Duration::from_millis(5)) {
            frames += 1;
            size = (frame.width, frame.height);
            let (w, h) = even_size(frame.width, frame.height);
            if encoder.as_ref().map(H264Encoder::dimensions) != Some((w, h)) {
                encoder = Some(H264Encoder::new(w, h, 6_000_000, 30.0)?);
            }
            if do_encode {
                let t = Instant::now();
                let stride = frame.width as usize * 4;
                let data = encoder
                    .as_mut()
                    .context("encoder ausente")?
                    .encode_bgra(&frame.bgra, stride)?;
                encode_time += t.elapsed();
                encoded_bytes += data.len();
            }
        }
        if let Some(chunks) = &chunks {
            while let Ok(chunk) = chunks.try_recv() {
                audio_chunks += 1;
                audio_samples += chunk.len() as u64;
                debug_assert_eq!(chunk.len(), CHUNK_SAMPLES);
                for &s in &chunk {
                    audio_peak = audio_peak.max(s.saturating_abs());
                    audio_energy += (s as f64) * (s as f64);
                    if s != 0 {
                        audio_nonzero += 1;
                    }
                    if audio_head.len() < 24 && s != 0 {
                        audio_head.push(s);
                    }
                }
            }
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let state = video.state();
    let dropped = video.dropped_frames();
    drop(audio);
    drop(chunks);
    video.stop();

    println!("duracao: {elapsed:.2} s");
    println!("video: {frames} quadros ({:.1} fps), tamanho {}x{}, descartados {dropped}, estado {state:?}",
        frames as f64 / elapsed, size.0, size.1);
    if frames > 0 {
        println!(
            "encode: {:.1} ms/quadro medio, {:.2} Mbps",
            encode_time.as_secs_f64() * 1000.0 / frames as f64,
            encoded_bytes as f64 * 8.0 / elapsed / 1_000_000.0
        );
    }
    println!("audio: rms {:.2}, primeiras amostras nao nulas {:?}", (audio_energy / audio_samples.max(1) as f64).sqrt(), audio_head);
    println!("audio: {audio_chunks} blocos de 20 ms, {audio_samples} amostras intercaladas (esperado ~{:.0}), pico {audio_peak}, amostras nao nulas {audio_nonzero}",
        elapsed * 96_000.0);
    Ok(())
}
