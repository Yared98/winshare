//! Encoder H.264 (openh264). Saída em Annex B, pronta para o H264 payloader do webrtc-rs.

use anyhow::{Context, Result};
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer};

pub struct H264Encoder {
    inner: Encoder,
    width: usize,
    height: usize,
}

impl H264Encoder {
    /// `width` e `height` precisam ser pares.
    pub fn new(width: usize, height: usize) -> Result<Self> {
        anyhow::ensure!(
            width % 2 == 0 && height % 2 == 0,
            "largura e altura precisam ser pares"
        );
        let inner = Encoder::new().context("falha ao criar encoder openh264")?;
        Ok(Self {
            inner,
            width,
            height,
        })
    }

    /// Força um IDR no próximo quadro (viewer novo ou PLI/FIR).
    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }

    /// Codifica um quadro RGB8 (`width * height * 3` bytes) e devolve os NALs em Annex B.
    pub fn encode_rgb(&mut self, rgb: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(
            rgb.len() == self.width * self.height * 3,
            "tamanho de quadro RGB incorreto"
        );
        let source = RgbSliceU8::new(rgb, (self.width, self.height));
        let yuv = YUVBuffer::from_rgb8_source(source);
        let stream = self
            .inner
            .encode(&yuv)
            .context("falha ao codificar quadro H.264")?;
        Ok(stream.to_vec())
    }
}
