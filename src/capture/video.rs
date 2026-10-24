//! Fonte de vídeo. A fonte de teste é animada; a captura WGC de janela é um stub (fase 3).

use anyhow::{Result, bail};

/// Gera um quadro RGB8 animado de `width` x `height` (`width * height * 3` bytes).
/// Gradiente que se move, um quadrado branco em deslocamento e uma barra de progresso.
pub fn test_frame(index: u64, width: usize, height: usize) -> Vec<u8> {
    let mut rgb = vec![0u8; width * height * 3];
    let shift = (index as usize * 4) % 256;
    let square = 80usize;
    let square_x = (index as usize * 6) % (width - square);
    let square_y = height / 3;
    let bar_len = (index as usize * 8) % width;

    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 3;
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
            rgb[offset] = r;
            rgb[offset + 1] = g;
            rgb[offset + 2] = b;
        }
    }
    rgb
}

/// Captura de uma janela específica via Windows Graphics Capture. Ainda não implementada.
pub fn start_window_capture(hwnd: u64) -> Result<()> {
    bail!("captura da janela {hwnd} (WGC) ainda não implementada; use a fonte de teste")
}
