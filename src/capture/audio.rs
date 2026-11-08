//! Captura de áudio de um processo (WASAPI Process Loopback) e PCM em blocos de 20 ms.
//! A fonte de teste é um tom senoidal, usado quando não há janela escolhida.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemAlloc,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, Interface, Ref, implement, Result as WinResult};

pub const SAMPLE_RATE: u32 = 48_000;
/// Amostras por canal em um quadro de 20 ms.
pub const FRAME_SAMPLES: usize = 960;
pub const CHANNELS: usize = 2;
/// Amostras intercaladas (`FRAME_SAMPLES * CHANNELS`) de um bloco de 20 ms.
pub const CHUNK_SAMPLES: usize = FRAME_SAMPLES * CHANNELS;

/// Blocos de 20 ms pendentes entre a captura e o encoder Opus.
const CHUNK_QUEUE: usize = 50;
/// Atraso alvo do silêncio de preenchimento: se o loopback não entregar a tempo, completa com zeros.
const LEAD_FRAMES: u64 = 4_800;
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Loopback do sistema: a primeira ativação do dispositivo de saída pode demorar bem mais.
const SYSTEM_ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

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

    /// Próximo quadro: `CHUNK_SAMPLES` amostras intercaladas.
    pub fn next_frame(&mut self) -> Vec<i16> {
        let mut out = Vec::with_capacity(CHUNK_SAMPLES);
        for _ in 0..FRAME_SAMPLES {
            let value = (self.phase.sin() * self.amplitude * i16::MAX as f64) as i16;
            out.extend_from_slice(&[value; CHANNELS]);
            self.phase = (self.phase + self.step) % std::f64::consts::TAU;
        }
        out
    }
}

/// Acumula PCM intercalado e devolve blocos completos de 20 ms; o resto fica para a próxima chamada.
#[derive(Default)]
pub struct PcmChunker {
    pending: Vec<i16>,
}

impl PcmChunker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        self.pending.extend_from_slice(samples);
        let mut chunks = Vec::new();
        while self.pending.len() >= CHUNK_SAMPLES {
            let rest = self.pending.split_off(CHUNK_SAMPLES);
            chunks.push(std::mem::replace(&mut self.pending, rest));
        }
        chunks
    }
}

/// Captura de áudio de um processo. Ao ser descartada, a thread de captura é sinalizada para parar.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl AudioCapture {
    /// Ativa o Process Loopback do `pid` (com os processos filhos) e começa a entregar blocos de 20 ms
    /// estéreo i16 pelo receiver devolvido. A ativação é assíncrona no Windows; aqui ela é aguardada.
    pub fn start(pid: u32) -> Result<(Self, Receiver<Vec<i16>>)> {
        Self::start_process(pid)
    }

    /// Ativa o Process Loopback do `pid`.
    pub fn start_process(pid: u32) -> Result<(Self, Receiver<Vec<i16>>)> {
        let (chunk_tx, chunk_rx) = mpsc::sync_channel(CHUNK_QUEUE);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("audio-loopback".to_owned())
            .spawn(move || thread_main(pid, thread_stop, chunk_tx, ready_tx))
            .context("não consegui iniciar a thread de áudio")?;
        ready_rx
            .recv_timeout(ACTIVATION_TIMEOUT + Duration::from_secs(2))
            .context("a ativação do áudio do processo não respondeu")??;
        Ok((
            Self {
                stop,
                _thread: thread,
            },
            chunk_rx,
        ))
    }

    /// Ativa o loopback do endpoint de saída padrão (áudio do sistema inteiro) e começa a entregar
    /// blocos de 20 ms estéreo 48 kHz i16 pelo receiver devolvido.
    pub fn start_system() -> Result<(Self, Receiver<Vec<i16>>)> {
        let (chunk_tx, chunk_rx) = mpsc::sync_channel(CHUNK_QUEUE);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("audio-system-loopback".to_owned())
            .spawn(move || system_thread_main(thread_stop, chunk_tx, ready_tx))
            .context("não consegui iniciar a thread de áudio do sistema")?;
        // O primeiro uso do dispositivo de saída pode levar ~20 s (ele "acorda"); depois é instantâneo.
        ready_rx
            .recv_timeout(SYSTEM_ACTIVATION_TIMEOUT)
            .context("a ativação do áudio do sistema não respondeu")??;
        Ok((
            Self {
                stop,
                _thread: thread,
            },
            chunk_rx,
        ))
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn thread_main(
    pid: u32,
    stop: Arc<AtomicBool>,
    chunks: SyncSender<Vec<i16>>,
    ready: SyncSender<Result<()>>,
) {
    let mut ready = Some(ready);
    if let Err(err) = run_loopback(pid, &stop, &chunks, &mut ready) {
        match ready.take() {
            Some(ready) => {
                let _ = ready.send(Err(err));
            }
            None => warn!(error = %format!("{err:#}"), "captura de áudio parou"),
        }
    }
}

/// Handler de conclusão da ativação assíncrona: avisa a thread de captura.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone {
    done: SyncSender<()>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(&self, _operation: Ref<IActivateAudioInterfaceAsyncOperation>) -> WinResult<()> {
        let _ = self.done.try_send(());
        Ok(())
    }
}

fn run_loopback(
    pid: u32,
    stop: &AtomicBool,
    chunks: &SyncSender<Vec<i16>>,
    ready: &mut Option<SyncSender<Result<()>>>,
) -> Result<()> {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let result = loopback_session(pid, stop, chunks, ready);
    if com_ok {
        unsafe { CoUninitialize() };
    }
    result
}

fn loopback_session(
    pid: u32,
    stop: &AtomicBool,
    chunks: &SyncSender<Vec<i16>>,
    ready: &mut Option<SyncSender<Result<()>>>,
) -> Result<()> {
    let client = activate_client(pid)?;

    let format = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * CHANNELS as u32 * 2,
        nBlockAlign: CHANNELS as u16 * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let event = unsafe { CreateEventW(None, false, false, None) }.context("CreateEventW falhou")?;
    let result = (|| -> Result<()> {
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    0, // duração do buffer: 0 (o loopback do processo aceita só o padrão)
                    0,
                    &format,
                    None,
                )
                .context("IAudioClient::Initialize (loopback do processo) falhou")?;
            client
                .SetEventHandle(event)
                .context("SetEventHandle falhou")?;
        }
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("IAudioCaptureClient indisponível")?;
        unsafe { client.Start() }.context("IAudioClient::Start falhou")?;
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        info!(pid, "captura de áudio do processo iniciada");

        let started = Instant::now();
        let mut chunker = PcmChunker::new();
        let mut received_frames: u64 = 0;
        let mut dropped_chunks: u64 = 0;
        while !stop.load(Ordering::SeqCst) {
            unsafe { WaitForSingleObject(event, POLL.as_millis() as u32) };
            // Drena todos os pacotes disponíveis.
            loop {
                let packet = unsafe { capture.GetNextPacketSize() }.context("GetNextPacketSize falhou")?;
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .context("GetBuffer falhou")?;
                let samples = frames as usize * CHANNELS;
                let pcm: Vec<i16> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    vec![0; samples]
                } else {
                    // SAFETY: o WASAPI entrega `frames` quadros estéreo i16 no buffer até ReleaseBuffer.
                    unsafe { std::slice::from_raw_parts(data as *const i16, samples) }.to_vec()
                };
                unsafe { capture.ReleaseBuffer(frames) }.context("ReleaseBuffer falhou")?;
                received_frames += frames as u64;
                dropped_chunks += send_chunks(&mut chunker, &pcm, chunks, stop);
            }
            // Sem áudio do processo, completa com silêncio para manter a linha do tempo.
            let due = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
            if received_frames + LEAD_FRAMES < due {
                let missing = (due - LEAD_FRAMES - received_frames) / FRAME_SAMPLES as u64
                    * FRAME_SAMPLES as u64;
                received_frames += missing;
                let silence = vec![0i16; missing as usize * CHANNELS];
                dropped_chunks += send_chunks(&mut chunker, &silence, chunks, stop);
            }
        }
        if dropped_chunks > 0 {
            debug!(dropped_chunks, "blocos de áudio descartados (encoder atrasado)");
        }
        let _ = unsafe { client.Stop() };
        Ok(())
    })();
    let _ = unsafe { CloseHandle(event) };
    result
}

/// Envia blocos de 20 ms; devolve quantos foram descartados porque a fila estava cheia.
/// Se o consumidor saiu, sinaliza `stop` para a captura terminar sem erro.
fn send_chunks(
    chunker: &mut PcmChunker,
    pcm: &[i16],
    chunks: &SyncSender<Vec<i16>>,
    stop: &AtomicBool,
) -> u64 {
    let mut dropped = 0;
    for chunk in chunker.push(pcm) {
        match chunks.try_send(chunk) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => dropped += 1,
            Err(TrySendError::Disconnected(_)) => {
                stop.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
    dropped
}

/// Ativa `IAudioClient` para o loopback do processo `pid`.
fn activate_client(pid: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // Os parâmetros vão num PROPVARIANT do tipo VT_BLOB. O blob precisa ser memória do COM: com um
    // ponteiro para a pilha, o heap corrompe (STATUS_HEAP_CORRUPTION), indicando que o sistema o libera
    // com CoTaskMemFree.
    let size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    let blob = unsafe { CoTaskMemAlloc(size) } as *mut AUDIOCLIENT_ACTIVATION_PARAMS;
    anyhow::ensure!(!blob.is_null(), "CoTaskMemAlloc falhou");
    unsafe { blob.write(params) };
    let mut variant = PROPVARIANT::default();
    unsafe {
        let inner = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: size as u32,
            pBlobData: blob as *mut u8,
        };
    }

    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone { done: done_tx }.into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant as *const PROPVARIANT),
            &handler,
        )
    }
    .context("ActivateAudioInterfaceAsync falhou")?;

    done_rx
        .recv_timeout(ACTIVATION_TIMEOUT)
        .context("tempo esgotado ativando o áudio do processo")?;
    let mut hr = HRESULT(0);
    let mut activated = None;
    unsafe { operation.GetActivateResult(&mut hr, &mut activated) }
        .context("GetActivateResult falhou")?;
    hr.ok().context("a ativação do áudio do processo falhou (o processo pode não ter áudio)")?;
    let unknown = activated.context("ativação sem interface")?;
    unknown.cast::<IAudioClient>().context("interface de áudio inesperada")
}

fn system_thread_main(
    stop: Arc<AtomicBool>,
    chunks: SyncSender<Vec<i16>>,
    ready: SyncSender<Result<()>>,
) {
    let mut ready = Some(ready);
    if let Err(err) = run_system_loopback(&stop, &chunks, &mut ready) {
        match ready.take() {
            Some(ready) => {
                let _ = ready.send(Err(err));
            }
            None => warn!(error = %format!("{err:#}"), "captura de áudio do sistema parou"),
        }
    }
}

fn run_system_loopback(
    stop: &AtomicBool,
    chunks: &SyncSender<Vec<i16>>,
    ready: &mut Option<SyncSender<Result<()>>>,
) -> Result<()> {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let result = system_loopback_session(stop, chunks, ready);
    if com_ok {
        unsafe { CoUninitialize() };
    }
    result
}

fn system_loopback_session(
    stop: &AtomicBool,
    chunks: &SyncSender<Vec<i16>>,
    ready: &mut Option<SyncSender<Result<()>>>,
) -> Result<()> {
    let (client, format_info) = activate_system_client()?;
    let event = unsafe { CreateEventW(None, false, false, None) }.context("CreateEventW falhou")?;
    let result = (|| -> Result<()> {
        unsafe {
            client
                .SetEventHandle(event)
                .context("SetEventHandle falhou")?;
        }
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("IAudioCaptureClient indisponível")?;
        unsafe { client.Start() }.context("IAudioClient::Start falhou")?;
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        info!(
            rate = format_info.sample_rate,
            channels = format_info.channels,
            "captura de áudio do sistema iniciada"
        );

        let started = Instant::now();
        let mut chunker = PcmChunker::new();
        let mut resampler = AudioResampler::new(format_info.sample_rate, SAMPLE_RATE);
        let mut received_frames: u64 = 0;
        let mut dropped_chunks: u64 = 0;
        while !stop.load(Ordering::SeqCst) {
            unsafe { WaitForSingleObject(event, POLL.as_millis() as u32) };
            // Drena todos os pacotes disponíveis.
            loop {
                let packet = unsafe { capture.GetNextPacketSize() }.context("GetNextPacketSize falhou")?;
                if packet == 0 {
                    break;
                }
                let mut data: *mut u8 = ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .context("GetBuffer falhou")?;

                let is_silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let pcm_stereo_48k: Vec<i16> = if is_silent {
                    let out_frames = if format_info.sample_rate == SAMPLE_RATE {
                        frames as usize
                    } else {
                        ((frames as u64 * SAMPLE_RATE as u64) / format_info.sample_rate as u64) as usize
                    };
                    vec![0i16; out_frames * CHANNELS]
                } else {
                    let stereo_in = format_info.extract_stereo(data, frames as usize);
                    resampler.resample(&stereo_in)
                };

                unsafe { capture.ReleaseBuffer(frames) }.context("ReleaseBuffer falhou")?;
                let out_frames_count = pcm_stereo_48k.len() / CHANNELS;
                received_frames += out_frames_count as u64;
                dropped_chunks += send_chunks(&mut chunker, &pcm_stereo_48k, chunks, stop);
            }
            // Sem áudio no sistema, completa com silêncio para manter a linha do tempo.
            let due = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
            if received_frames + LEAD_FRAMES < due {
                let missing = (due - LEAD_FRAMES - received_frames) / FRAME_SAMPLES as u64
                    * FRAME_SAMPLES as u64;
                received_frames += missing;
                let silence = vec![0i16; missing as usize * CHANNELS];
                dropped_chunks += send_chunks(&mut chunker, &silence, chunks, stop);
            }
        }
        if dropped_chunks > 0 {
            debug!(dropped_chunks, "blocos de áudio do sistema descartados (encoder atrasado)");
        }
        let _ = unsafe { client.Stop() };
        Ok(())
    })();
    let _ = unsafe { CloseHandle(event) };
    result
}

fn activate_system_client() -> Result<(IAudioClient, AudioFormatInfo)> {
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }
    .context("CoCreateInstance(MMDeviceEnumerator) falhou")?;

    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("não há dispositivo de saída de áudio padrão disponível")?;

    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .context("falha ao ativar IAudioClient no dispositivo de saída")?;

    let mix_format_ptr = unsafe { client.GetMixFormat() }
        .context("GetMixFormat falhou")?;

    let format_info = parse_waveformat(mix_format_ptr)?;

    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let init_res = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            0,
            0,
            mix_format_ptr,
            None,
        )
    };
    unsafe { CoTaskMemFree(Some(mix_format_ptr.cast())) };
    init_res.context("IAudioClient::Initialize (loopback do sistema) falhou")?;

    Ok((client, format_info))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleType {
    Float32,
    Pcm16,
    Pcm24Packed,
    Pcm24In32,
    Pcm32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormatInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_type: SampleType,
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: windows_core::GUID =
    windows_core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows_core::GUID =
    windows_core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

fn parse_waveformat(ptr: *const WAVEFORMATEX) -> Result<AudioFormatInfo> {
    anyhow::ensure!(!ptr.is_null(), "ponteiro de formato de áudio nulo");
    let format_tag = unsafe { ptr::addr_of!((*ptr).wFormatTag).read_unaligned() };
    let sample_rate = unsafe { ptr::addr_of!((*ptr).nSamplesPerSec).read_unaligned() };
    let channels = unsafe { ptr::addr_of!((*ptr).nChannels).read_unaligned() };
    let bits_per_sample = unsafe { ptr::addr_of!((*ptr).wBitsPerSample).read_unaligned() };
    anyhow::ensure!(channels >= 1, "dispositivo de áudio com 0 canais");

    let sample_type = if format_tag == WAVE_FORMAT_IEEE_FLOAT {
        SampleType::Float32
    } else if format_tag == WAVE_FORMAT_PCM {
        match bits_per_sample {
            16 => SampleType::Pcm16,
            24 => SampleType::Pcm24Packed,
            32 => SampleType::Pcm32,
            other => anyhow::bail!("formato PCM {other} bits não suportado"),
        }
    } else if format_tag == WAVE_FORMAT_EXTENSIBLE {
        let ext_ptr = ptr as *const WAVEFORMATEXTENSIBLE;
        let sub_format = unsafe { ptr::addr_of!((*ext_ptr).SubFormat).read_unaligned() };
        if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            SampleType::Float32
        } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
            match bits_per_sample {
                16 => SampleType::Pcm16,
                24 => SampleType::Pcm24In32,
                32 => SampleType::Pcm32,
                other => anyhow::bail!("formato PCM extensível {other} bits não suportado"),
            }
        } else {
            anyhow::bail!("subformato de áudio não suportado: {:?}", sub_format);
        }
    } else {
        anyhow::bail!("formato de áudio não suportado: tag {}", format_tag);
    };

    Ok(AudioFormatInfo {
        sample_rate,
        channels,
        sample_type,
    })
}

impl AudioFormatInfo {
    pub fn extract_stereo(&self, data: *const u8, frames: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(frames * CHANNELS);
        let ch = self.channels as usize;
        match self.sample_type {
            SampleType::Float32 => {
                let slice = unsafe { std::slice::from_raw_parts(data as *const f32, frames * ch) };
                for frame in 0..frames {
                    let base = frame * ch;
                    let l = slice[base];
                    let r = if ch > 1 { slice[base + 1] } else { l };
                    out.push(float_to_i16(l));
                    out.push(float_to_i16(r));
                }
            }
            SampleType::Pcm16 => {
                let slice = unsafe { std::slice::from_raw_parts(data as *const i16, frames * ch) };
                for frame in 0..frames {
                    let base = frame * ch;
                    let l = slice[base];
                    let r = if ch > 1 { slice[base + 1] } else { l };
                    out.push(l);
                    out.push(r);
                }
            }
            SampleType::Pcm24Packed => {
                let slice = unsafe { std::slice::from_raw_parts(data, frames * ch * 3) };
                for frame in 0..frames {
                    let base = frame * ch * 3;
                    let l = (slice[base + 1] as i16) | ((slice[base + 2] as i8 as i16) << 8);
                    let r = if ch > 1 {
                        let r_base = base + 3;
                        (slice[r_base + 1] as i16) | ((slice[r_base + 2] as i8 as i16) << 8)
                    } else {
                        l
                    };
                    out.push(l);
                    out.push(r);
                }
            }
            SampleType::Pcm24In32 | SampleType::Pcm32 => {
                let slice = unsafe { std::slice::from_raw_parts(data as *const i32, frames * ch) };
                for frame in 0..frames {
                    let base = frame * ch;
                    let l = (slice[base] >> 16) as i16;
                    let r = if ch > 1 { (slice[base + 1] >> 16) as i16 } else { l };
                    out.push(l);
                    out.push(r);
                }
            }
        }
        out
    }
}

fn float_to_i16(f: f32) -> i16 {
    (f.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

pub struct AudioResampler {
    in_rate: u32,
    out_rate: u32,
    phase: f64,
    last_l: i16,
    last_r: i16,
}

impl AudioResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate,
            out_rate,
            phase: 0.0,
            last_l: 0,
            last_r: 0,
        }
    }

    pub fn resample(&mut self, stereo_in: &[i16]) -> Vec<i16> {
        if self.in_rate == self.out_rate {
            return stereo_in.to_vec();
        }
        let in_frames = stereo_in.len() / CHANNELS;
        let estimated_out_frames =
            ((in_frames as u64 * self.out_rate as u64) / self.in_rate as u64) as usize + 2;
        let mut out = Vec::with_capacity(estimated_out_frames * CHANNELS);

        let step = self.in_rate as f64 / self.out_rate as f64;
        for i in 0..in_frames {
            let curr_l = stereo_in[i * 2];
            let curr_r = stereo_in[i * 2 + 1];
            while self.phase < 1.0 {
                let l = (self.last_l as f64 + (curr_l as f64 - self.last_l as f64) * self.phase).round() as i16;
                let r = (self.last_r as f64 + (curr_r as f64 - self.last_r as f64) * self.phase).round() as i16;
                out.push(l);
                out.push(r);
                self.phase += step;
            }
            self.phase -= 1.0;
            self.last_l = curr_l;
            self.last_r = curr_r;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunker_emits_full_20ms_blocks_only() {
        let mut chunker = PcmChunker::new();
        assert!(chunker.push(&vec![1; 1000]).is_empty());
        let chunks = chunker.push(&vec![2; 1000]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), CHUNK_SAMPLES);
        assert_eq!(chunks[0][999], 1);
        assert_eq!(chunks[0][1000], 2);
        let chunks = chunker.push(&vec![3; CHUNK_SAMPLES * 2]);
        assert_eq!(chunks.len(), 2, "80 amostras sobraram + 3840 entram = 2 blocos");
    }

    #[test]
    fn chunk_is_20ms_of_stereo() {
        assert_eq!(FRAME_SAMPLES as u32 * 1000 / SAMPLE_RATE, 20);
        assert_eq!(CHUNK_SAMPLES, 1920);
    }

    #[test]
    fn sine_source_produces_full_frames() {
        let mut tone = SineSource::new(440.0, 0.5);
        let frame = tone.next_frame();
        assert_eq!(frame.len(), CHUNK_SAMPLES);
        assert!(frame.iter().any(|&s| s != 0));
    }

    #[test]
    fn resampler_same_rate_is_identity() {
        let mut resampler = AudioResampler::new(48000, 48000);
        let input = vec![100, 200, 300, 400];
        let output = resampler.resample(&input);
        assert_eq!(input, output);
    }

    #[test]
    fn resampler_converts_rates_correctly() {
        // 44100 -> 48000
        let mut resampler = AudioResampler::new(44100, 48000);
        // 44100 frames = 88200 samples
        let input = vec![1000i16; 44100 * 2];
        let output = resampler.resample(&input);
        let out_frames = output.len() / 2;
        // Should be approximately 48000 frames (+- 2 frames due to fractional phase)
        assert!((out_frames as i32 - 48000).abs() <= 2);
    }

    #[test]
    fn format_info_extracts_stereo_from_f32() {
        let info = AudioFormatInfo {
            sample_rate: 48000,
            channels: 2,
            sample_type: SampleType::Float32,
        };
        let input_f32: [f32; 4] = [0.0, 1.0, -1.0, 0.5];
        let bytes = unsafe {
            std::slice::from_raw_parts(input_f32.as_ptr() as *const u8, input_f32.len() * 4)
        };
        let pcm = info.extract_stereo(bytes.as_ptr(), 2);
        assert_eq!(pcm.len(), 4);
        assert_eq!(pcm[0], 0);
        assert_eq!(pcm[1], 32767);
        assert_eq!(pcm[2], -32767);
        assert!((pcm[3] - 16384).abs() <= 1);
    }
}
