//! Fonte de áudio. A fonte de teste é um tom senoidal; o Process Loopback por processo é um stub (fase 4).

use anyhow::{Result, bail};

pub const SAMPLE_RATE: u32 = 48_000;
/// Amostras por canal em um quadro de 20 ms.
pub const FRAME_SAMPLES: usize = 960;
pub const CHANNELS: usize = 2;

/// Tom senoidal estéreo que gera quadros de 20 ms de PCM i16 intercalado.
pub struct SineSource {
    phase: f64,
    step: f64,
    amplitude: f64,
}

impl SineSource {
    pub fn new(frequency_hz: f64, amplitude: f64) -> Self {
        Self {
            phase: 0.0,
            step: std::f64::consts::TAU * frequency_hz / SAMPLE_RATE as f64,
            amplitude: amplitude.clamp(0.0, 1.0),
        }
    }

    /// Próximo quadro: `FRAME_SAMPLES * CHANNELS` amostras intercaladas.
    pub fn next_frame(&mut self) -> Vec<i16> {
        let mut out = Vec::with_capacity(FRAME_SAMPLES * CHANNELS);
        for _ in 0..FRAME_SAMPLES {
            let value = (self.phase.sin() * self.amplitude * i16::MAX as f64) as i16;
            out.extend_from_slice(&[value; CHANNELS]);
            self.phase = (self.phase + self.step) % std::f64::consts::TAU;
        }
        out
    }
}

/// Captura de áudio de um processo via Process Loopback. Ainda não implementada.
#[allow(dead_code)] // usado a partir da fase 4
pub fn start_process_loopback(pid: u32) -> Result<()> {
    bail!("captura de áudio do processo {pid} ainda não implementada; use a fonte de teste")
}
