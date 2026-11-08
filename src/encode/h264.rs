//! Encoder H.264 (openh264). Recebe quadros BGRA, converte para I420 e devolve NALs em Annex B,
//! prontos para o H264 payloader do webrtc-rs.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode};
use openh264::formats::YUVBuffer;

/// Ajusta largura e altura para pares (corta a última coluna/linha ímpar). Mínimo 2.
pub fn even_size(width: u32, height: u32) -> (usize, usize) {
    (((width as usize) & !1).max(2), ((height as usize) & !1).max(2))
}

/// Converte BGRA para I420 (YUV 4:2:0, BT.601, faixa limitada 16..235) na área `width` x `height`.
/// `stride` é o tamanho da linha de origem em bytes. `width` e `height` precisam ser pares.
/// A crominância de cada bloco 2x2 é a média RGB dos quatro pixels.
pub fn bgra_to_i420(bgra: &[u8], width: usize, height: usize, stride: usize, out: &mut Vec<u8>) {
    debug_assert!(width % 2 == 0 && height % 2 == 0);
    let luma_len = width * height;
    let chroma_w = width / 2;
    let chroma_len = chroma_w * (height / 2);
    out.clear();
    out.resize(luma_len + 2 * chroma_len, 0);
    let (y_plane, chroma) = out.split_at_mut(luma_len);
    let (u_plane, v_plane) = chroma.split_at_mut(chroma_len);

    for cy in 0..height / 2 {
        for cx in 0..chroma_w {
            let (mut sum_r, mut sum_g, mut sum_b) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                let row = (cy * 2 + dy) * stride;
                for dx in 0..2 {
                    let px = row + (cx * 2 + dx) * 4;
                    let (b, g, r) = (bgra[px] as i32, bgra[px + 1] as i32, bgra[px + 2] as i32);
                    y_plane[(cy * 2 + dy) * width + cx * 2 + dx] = luma(r, g, b);
                    sum_r += r;
                    sum_g += g;
                    sum_b += b;
                }
            }
            let (r, g, b) = ((sum_r + 2) >> 2, (sum_g + 2) >> 2, (sum_b + 2) >> 2);
            u_plane[cy * chroma_w + cx] = chroma_u(r, g, b);
            v_plane[cy * chroma_w + cx] = chroma_v(r, g, b);
        }
    }
}

#[inline]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

#[inline]
fn chroma_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8
}

#[inline]
fn chroma_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8
}

pub struct H264Encoder {
    inner: Encoder,
    width: usize,
    height: usize,
    last_keyframe: Instant,
    keyframe_interval: Duration,
}

impl H264Encoder {
    /// Cria um novo encoder com o intervalo de quadros-chave padrão da configuração (ou 5 s).
    pub fn new(
        width: usize,
        height: usize,
        bitrate_bps: u32,
        fps: f32,
    ) -> Result<Self> {
        let keyframe_interval = crate::config::last_loaded()
            .map(|c| Duration::from_secs(u64::from(c.keyframe_interval_secs)))
            .unwrap_or(Duration::from_secs(5));
        Self::with_keyframe_interval(width, height, bitrate_bps, fps, keyframe_interval)
    }

    /// `width` e `height` precisam ser pares. `bitrate_bps` é o alvo de taxa; `fps` é o teto de quadros;
    /// `keyframe_interval` é o intervalo entre IDRs periódicos forçados.
    pub fn with_keyframe_interval(
        width: usize,
        height: usize,
        bitrate_bps: u32,
        fps: f32,
        keyframe_interval: Duration,
    ) -> Result<Self> {
        anyhow::ensure!(
            width % 2 == 0 && height % 2 == 0,
            "largura e altura precisam ser pares"
        );
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(fps))
            .rate_control_mode(RateControlMode::Bitrate);
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("falha ao criar encoder openh264")?;
        Ok(Self {
            inner,
            width,
            height,
            // Força o primeiro quadro a ser IDR (o primeiro encode já é keyframe).
            last_keyframe: Instant::now() - keyframe_interval,
            keyframe_interval,
        })
    }

    /// Dimensões (pares) que este encoder aceita.
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Força um IDR no próximo quadro (viewer novo ou PLI/FIR).
    pub fn force_keyframe(&mut self) {
        self.inner.force_intra_frame();
    }

    /// Codifica um quadro BGRA de `width` x `height` (com `stride` bytes por linha) e devolve os NALs em Annex B.
    pub fn encode_bgra(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        anyhow::ensure!(
            stride >= self.width * 4 && bgra.len() >= stride * self.height,
            "tamanho de quadro BGRA incorreto"
        );
        let mut yuv = Vec::new();
        bgra_to_i420(bgra, self.width, self.height, stride, &mut yuv);
        if self.last_keyframe.elapsed() >= self.keyframe_interval {
            self.force_keyframe();
            self.last_keyframe = Instant::now();
        }
        let source = YUVBuffer::from_vec(yuv, self.width, self.height);
        let stream = self
            .inner
            .encode(&source)
            .context("falha ao codificar quadro H.264")?;
        Ok(stream.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(b: u8, g: u8, r: u8, width: usize, height: usize) -> Vec<u8> {
        (0..width * height).flat_map(|_| [b, g, r, 255]).collect()
    }

    fn convert_solid(b: u8, g: u8, r: u8) -> (u8, u8, u8) {
        let src = solid(b, g, r, 2, 2);
        let mut out = Vec::new();
        bgra_to_i420(&src, 2, 2, 8, &mut out);
        (out[0], out[4], out[5])
    }

    #[test]
    fn known_values_for_primary_colors() {
        assert_eq!(convert_solid(0, 0, 0), (16, 128, 128), "preto");
        assert_eq!(convert_solid(255, 255, 255), (235, 128, 128), "branco");
        assert_eq!(convert_solid(0, 0, 255), (82, 90, 240), "vermelho");
        assert_eq!(convert_solid(0, 255, 0), (144, 54, 34), "verde");
        assert_eq!(convert_solid(255, 0, 0), (41, 240, 110), "azul");
    }

    #[test]
    fn chroma_is_average_of_block() {
        // Dois pixels vermelhos e dois pretos: crominância de (127,0,0) em média.
        let mut src = Vec::new();
        src.extend_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]); // linha 0: vermelho, vermelho
        src.extend_from_slice(&[0, 0, 0, 255, 0, 0, 0, 255]); // linha 1: preto, preto
        let mut out = Vec::new();
        bgra_to_i420(&src, 2, 2, 8, &mut out);
        let (u, v) = (out[4], out[5]);
        assert_eq!(u, chroma_u(128, 0, 0));
        assert_eq!(v, chroma_v(128, 0, 0));
    }

    #[test]
    fn stride_is_respected() {
        // Largura 2 com stride de 12 bytes (4 bytes de lixo no fim de cada linha).
        let mut src = vec![0u8; 12 * 2];
        for row in 0..2 {
            src[row * 12..row * 12 + 8].copy_from_slice(&[255, 255, 255, 255, 255, 255, 255, 255]);
            src[row * 12 + 8..row * 12 + 12].copy_from_slice(&[9, 9, 9, 9]);
        }
        let mut out = Vec::new();
        bgra_to_i420(&src, 2, 2, 12, &mut out);
        assert_eq!(&out[..4], &[235, 235, 235, 235]);
    }

    #[test]
    fn even_size_rounds_down_and_keeps_minimum() {
        assert_eq!(even_size(1921, 1081), (1920, 1080));
        assert_eq!(even_size(1920, 1080), (1920, 1080));
        assert_eq!(even_size(1, 1), (2, 2));
        assert_eq!(even_size(0, 0), (2, 2));
    }

    #[test]
    fn encodes_a_frame_to_annex_b() {
        let mut enc = H264Encoder::new(64, 48, 1_000_000, 30.0).unwrap();
        let bgra = solid(0, 128, 255, 64, 48);
        let nals = enc.encode_bgra(&bgra, 64 * 4).unwrap();
        assert!(nals.starts_with(&[0, 0, 0, 1]) || nals.starts_with(&[0, 0, 1]));
        assert_eq!(enc.keyframe_interval, Duration::from_secs(5));
    }
}
