//! Encoder Opus (48 kHz, estéreo, quadros de 20 ms).

use anyhow::{Context, Result};
use opus::{Application, Channels, Encoder};

/// Tamanho máximo de um pacote Opus gerado para um quadro de 20 ms.
const MAX_PACKET: usize = 4000;

pub struct OpusEncoder {
    inner: Encoder,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let inner = Encoder::new(48_000, Channels::Stereo, Application::Audio)
            .context("falha ao criar encoder Opus")?;
        Ok(Self { inner })
    }

    /// Codifica PCM i16 intercalado estéreo (1920 amostras = 20 ms).
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        self.inner
            .encode_vec(pcm, MAX_PACKET)
            .context("falha ao codificar quadro Opus")
    }
}
