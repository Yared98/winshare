//! Captura de uma janela pelo Windows Graphics Capture (WGC) e fila de quadros BGRA.
//!
//! O WGC entrega quadros numa thread própria; cada quadro é copiado para uma fila limitada.
//! Se o encoder atrasar, a fila descarta o quadro mais antigo, então a latência não cresce.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, warn};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowThreadProcessId, IsIconic, IsWindow,
};
use windows_capture::capture::{CaptureControl, Context as CaptureContext, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// Quadros pendentes entre a captura e o encoder.
const QUEUE_CAPACITY: usize = 3;

/// Quadro BGRA sem padding (`width * height * 4` bytes).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    /// Instante de chegada no relógio `Instant` do processo (comum a áudio e vídeo).
    pub at: Instant,
    /// Carimbo do próprio WGC, em unidades de 100 ns.
    pub timestamp_100ns: i64,
}

/// Fila limitada de quadros: `push` descarta o mais antigo quando cheia.
pub struct FrameQueue {
    inner: Mutex<VecDeque<CapturedFrame>>,
    ready: Condvar,
    dropped: AtomicU64,
}

impl FrameQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(QUEUE_CAPACITY)),
            ready: Condvar::new(),
            dropped: AtomicU64::new(0),
        }
    }

    /// Enfileira um quadro. Devolve `true` se foi preciso descartar o mais antigo.
    pub fn push(&self, frame: CapturedFrame) -> bool {
        let mut queue = lock(&self.inner);
        let dropped = queue.len() >= QUEUE_CAPACITY;
        if dropped {
            queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(frame);
        self.ready.notify_one();
        dropped
    }

    /// Espera até `timeout` por um quadro.
    pub fn pop_timeout(&self, timeout: Duration) -> Option<CapturedFrame> {
        let mut queue = lock(&self.inner);
        if queue.is_empty() {
            queue = self
                .ready
                .wait_timeout(queue, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        queue.pop_front()
    }

    /// Quadros descartados por atraso do encoder.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Default for FrameQueue {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Estado compartilhado entre a thread do WGC e quem consome os quadros.
pub struct Shared {
    queue: FrameQueue,
    /// Definido pelo WGC quando a janela é fechada.
    closed: AtomicBool,
    /// Pedido de parada vindo de quem controla a captura.
    stop: AtomicBool,
}

/// Handler do WGC: copia cada quadro para a fila compartilhada.
struct Handler {
    shared: Arc<Shared>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Arc<Shared>;
    type Error = anyhow::Error;

    fn new(ctx: CaptureContext<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { shared: ctx.flags })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.shared.stop.load(Ordering::SeqCst) {
            control.stop();
            return Ok(());
        }
        let at = Instant::now();
        let timestamp_100ns = frame.timestamp()?.Duration;
        let (width, height) = (frame.width(), frame.height());
        let buffer = frame.buffer()?;
        let mut scratch = Vec::new();
        let bgra = buffer.as_nopadding_buffer(&mut scratch).to_vec();
        self.shared.queue.push(CapturedFrame {
            width,
            height,
            bgra,
            at,
            timestamp_100ns,
        });
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.shared.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Situação atual da janela capturada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Running,
    /// Minimizada: o WGC não entrega quadros enquanto isso.
    Minimized,
    Closed,
}

/// Captura ativa de uma janela. Quadros saem por `next_frame`.
pub struct WindowCapture {
    hwnd: HWND,
    shared: Arc<Shared>,
    control: Option<CaptureControl<Handler, anyhow::Error>>,
}

impl WindowCapture {
    /// Inicia a captura da janela `hwnd` (handle como inteiro, como em `/api/windows`).
    pub fn start(hwnd: u64) -> Result<Self> {
        let raw = raw_hwnd(hwnd);
        anyhow::ensure!(is_window(raw), "janela {hwnd} não existe");
        let shared = Arc::new(Shared {
            queue: FrameQueue::new(),
            closed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let settings = Settings::new(
            Window::from_raw_hwnd(raw.0),
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            shared.clone(),
        );
        let control = Handler::start_free_threaded(settings)
            .map_err(|err| anyhow::anyhow!("falha ao iniciar o Windows Graphics Capture: {err:?}"))?;
        debug!(hwnd, "captura WGC iniciada");
        Ok(Self {
            hwnd: raw,
            shared,
            control: Some(control),
        })
    }

    /// Próximo quadro BGRA, ou `None` se nada chegou em `timeout`.
    pub fn next_frame(&self, timeout: Duration) -> Option<CapturedFrame> {
        self.shared.queue.pop_timeout(timeout)
    }

    /// Quadros descartados porque o encoder atrasou.
    pub fn dropped_frames(&self) -> u64 {
        self.shared.queue.dropped()
    }

    pub fn state(&self) -> WindowState {
        if self.shared.closed.load(Ordering::SeqCst) || !is_window(self.hwnd) {
            WindowState::Closed
        } else if unsafe { IsIconic(self.hwnd) }.as_bool() {
            WindowState::Minimized
        } else {
            WindowState::Running
        }
    }

    /// Encerra o WGC e espera a thread dele terminar (rápido: só posta WM_QUIT).
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(control) = self.control.take()
            && let Err(err) = control.stop()
        {
            warn!(error = ?err, "thread do WGC não terminou limpa");
        }
    }
}

// SAFETY: o HWND é só um identificador opaco; a captura é usada por uma única thread por vez
// (a thread de vídeo da transmissão), e o WGC aceita o handle de qualquer thread.
unsafe impl Send for WindowCapture {}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// PID do processo dono da janela (usado pelo Process Loopback de áudio).
pub fn window_pid(hwnd: u64) -> Result<u32> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(raw_hwnd(hwnd), Some(&mut pid)) };
    anyhow::ensure!(pid != 0, "não consegui obter o processo da janela {hwnd}");
    Ok(pid)
}

fn raw_hwnd(hwnd: u64) -> HWND {
    HWND(hwnd as usize as *mut c_void)
}

fn is_window(hwnd: HWND) -> bool {
    unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u8) -> CapturedFrame {
        CapturedFrame {
            width: 2,
            height: 2,
            bgra: vec![id; 16],
            at: Instant::now(),
            timestamp_100ns: id as i64,
        }
    }

    #[test]
    fn queue_drops_oldest_when_full() {
        let queue = FrameQueue::new();
        for id in 0..QUEUE_CAPACITY as u8 {
            assert!(!queue.push(frame(id)));
        }
        assert!(queue.push(frame(9)), "a fila cheia deve descartar");
        assert_eq!(queue.dropped(), 1);
        let order: Vec<i64> = (0..QUEUE_CAPACITY)
            .map(|_| queue.pop_timeout(Duration::ZERO).unwrap().timestamp_100ns)
            .collect();
        assert_eq!(order, vec![1, 2, 9], "o quadro 0 foi o descartado");
        assert!(queue.pop_timeout(Duration::from_millis(1)).is_none());
    }

    #[test]
    fn pop_waits_for_a_frame() {
        let queue = Arc::new(FrameQueue::new());
        let producer = queue.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.push(frame(7));
        });
        let got = queue.pop_timeout(Duration::from_secs(2));
        handle.join().unwrap();
        assert_eq!(got.unwrap().timestamp_100ns, 7);
    }
}
