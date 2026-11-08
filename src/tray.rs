//! Ícone na bandeja do sistema, controle da janela de console, instância única e autostart.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tracing::{error, info, warn};
use tray_icon::{TrayIconBuilder, TrayIconEvent};
use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, LPARAM, WPARAM,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, PostThreadMessageW, SetTimer, TranslateMessage, MSG,
    WM_QUIT, WM_TIMER,
};

use crate::server::AppState;

const RUN_SUBKEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: windows::core::PCWSTR = w!("winshare");
const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\winshare");

/// Guarda de mutex de instância única. Fecha o handle no drop.
pub struct InstanceMutex(HANDLE);

unsafe impl Send for InstanceMutex {}
unsafe impl Sync for InstanceMutex {}

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Verifica e garante que apenas uma instância do aplicativo esteja em execução.
/// Se já houver outra instância, abre a página de controle no navegador e devolve `Ok(None)`.
pub fn init_single_instance(control_port: u16) -> Result<Option<InstanceMutex>> {
    let handle = unsafe { CreateMutexW(None, true, MUTEX_NAME) }
        .context("falha ao chamar CreateMutexW para instância única")?;

    let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
    if last_error == ERROR_ALREADY_EXISTS {
        let control_url = format!("http://127.0.0.1:{}/control", control_port);
        open_control_page(&control_url);
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(None);
    }

    Ok(Some(InstanceMutex(handle)))
}

/// Verifica se já existe uma instância do winshare ativa no sistema.
pub fn is_instance_running() -> bool {
    let result = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, MUTEX_NAME) };
    match result {
        Ok(handle) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            true
        }
        Err(_) => false,
    }
}

/// Abre uma URL no navegador padrão via comando `cmd /C start "" <url>`.
pub fn open_control_page(url: &str) {
    if cfg!(windows) {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
}

/// Monta o comando de inicialização com o Windows para o registro HKCU.
/// Formato: `"<caminho do exe>" --config "<caminho absoluto do config>" --supervised`
pub fn format_autostart_command(exe_path: &Path, config_path: &Path) -> String {
    format!(
        "\"{}\" --config \"{}\" --supervised",
        exe_path.display(),
        config_path.display()
    )
}

/// Consulta o registro do Windows (HKCU) para verificar se "Iniciar com o Windows" está ativado.
pub fn is_autostart_enabled() -> Result<bool> {
    let mut hkey = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, None, KEY_READ, &mut hkey) };
    if status != ERROR_SUCCESS {
        return Ok(false);
    }

    let mut data_type = REG_VALUE_TYPE::default();
    let mut data_len = 0u32;
    let query_status = unsafe {
        RegQueryValueExW(
            hkey,
            VALUE_NAME,
            None,
            Some(&mut data_type),
            None,
            Some(&mut data_len),
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    Ok(query_status == ERROR_SUCCESS)
}

/// Ativa ou desativa a inicialização com o Windows em HKCU.
pub fn set_autostart_enabled(enable: bool, config_path: &Path) -> Result<()> {
    let mut hkey = HKEY::default();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_SUBKEY, None, KEY_SET_VALUE, &mut hkey) };
    if status != ERROR_SUCCESS {
        anyhow::bail!("falha ao abrir chave de inicialização no registro (código {status:?})");
    }

    let result = if enable {
        let exe_path = std::env::current_exe().context("falha ao obter caminho do executável")?;
        let abs_config = if config_path.is_absolute() {
            config_path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(config_path))
                .unwrap_or_else(|_| config_path.to_path_buf())
        };
        let cmd = format_autostart_command(&exe_path, &abs_config);
        let utf16: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes =
            unsafe { std::slice::from_raw_parts(utf16.as_ptr() as *const u8, utf16.len() * 2) };
        unsafe { RegSetValueExW(hkey, VALUE_NAME, None, REG_SZ, Some(bytes)) }
    } else {
        unsafe { RegDeleteValueW(hkey, VALUE_NAME) }
    };

    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if !enable && result == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if result != ERROR_SUCCESS {
        anyhow::bail!("falha ao atualizar registro de inicialização (código {result:?})");
    }
    Ok(())
}

/// Cria um ícone circular RGBA com suavização de borda (anti-aliasing) em código.
pub fn create_circle_icon(r: u8, g: u8, b: u8) -> Result<tray_icon::Icon> {
    let size = 32u32;
    let radius = 13.0f32;
    let center = (size as f32) / 2.0;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);

    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 + 0.5) - center;
            let dy = (y as f32 + 0.5) - center;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius - 0.5 {
                rgba.extend_from_slice(&[r, g, b, 255]);
            } else if dist < radius + 0.5 {
                let alpha = ((radius + 0.5 - dist) * 255.0).clamp(0.0, 255.0) as u8;
                rgba.extend_from_slice(&[r, g, b, alpha]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    tray_icon::Icon::from_rgba(rgba, size, size).context("falha ao criar ícone da bandeja")
}

/// Controlador da thread do ícone na bandeja. Encerra a thread e remove o ícone no drop/shutdown.
pub struct TrayHandle {
    thread_id: u32,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl TrayHandle {
    /// Envia WM_QUIT para o laço de mensagens da thread do tray e aguarda seu término.
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.join_handle.take() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = handle.join();
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Inicia a thread do ícone na bandeja do Windows e o laço de mensagens Win32.
pub fn spawn_tray(
    state: Arc<AppState>,
    config_path: PathBuf,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) -> Result<TrayHandle> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let join_handle = std::thread::Builder::new()
        .name("winshare-tray".into())
        .spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            if ready_tx.send(thread_id).is_err() {
                return;
            }
            run_tray_loop(state, config_path, shutdown_tx);
        })
        .context("falha ao iniciar thread da bandeja")?;

    let thread_id = ready_rx
        .recv()
        .context("thread da bandeja finalizou prematuramente")?;

    Ok(TrayHandle {
        thread_id,
        join_handle: Some(join_handle),
    })
}

fn run_tray_loop(
    state: Arc<AppState>,
    config_path: PathBuf,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let icon_live = match create_circle_icon(220, 38, 38) {
        Ok(icon) => icon,
        Err(err) => {
            error!(?err, "falha ao gerar ícone ativo");
            return;
        }
    };
    let icon_stopped = match create_circle_icon(128, 128, 128) {
        Ok(icon) => icon,
        Err(err) => {
            error!(?err, "falha ao gerar ícone parado");
            return;
        }
    };

    let open_panel_item = MenuItem::new("Abrir painel", true, None);
    let copy_link_item = MenuItem::new("Copiar link", false, None);
    let stop_item = MenuItem::new("Parar transmissão", false, None);
    let autostart_initial = is_autostart_enabled().unwrap_or(false);
    let autostart_item = CheckMenuItem::new("Iniciar com o Windows", true, autostart_initial, None);
    let quit_item = MenuItem::new("Sair", true, None);

    let menu = Menu::new();
    let _ = menu.append(&open_panel_item);
    let _ = menu.append(&copy_link_item);
    let _ = menu.append(&stop_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&autostart_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_item);

    let tray_icon = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("winshare: Parado")
        .with_icon(icon_stopped.clone())
        .with_menu_on_left_click(false)
        .with_menu_on_right_click(true)
        .build()
    {
        Ok(t) => t,
        Err(err) => {
            error!(?err, "falha ao inicializar ícone da bandeja");
            return;
        }
    };

    let timer_id = unsafe { SetTimer(None, 0, 2000, None) };
    let control_url = format!("http://127.0.0.1:{}/control", state.config.control_port);

    let mut last_live: Option<bool> = None;
    let mut last_viewers: usize = usize::MAX;

    let update_state = |last_live: &mut Option<bool>, last_viewers: &mut usize| {
        let is_live = {
            let live_guard = state.live.lock().unwrap_or_else(|e| e.into_inner());
            live_guard
                .as_ref()
                .map(|s| !s.broadcaster.ended())
                .unwrap_or(false)
        };
        let viewers = state.viewers.load(Ordering::Relaxed);

        if *last_live != Some(is_live) || *last_viewers != viewers {
            *last_live = Some(is_live);
            *last_viewers = viewers;

            if is_live {
                let _ = tray_icon.set_icon(Some(icon_live.clone()));
                let tooltip = format!("winshare: Ao vivo ({} espectadores)", viewers);
                let _ = tray_icon.set_tooltip(Some(&tooltip));
                copy_link_item.set_enabled(true);
                stop_item.set_enabled(true);
            } else {
                let _ = tray_icon.set_icon(Some(icon_stopped.clone()));
                let _ = tray_icon.set_tooltip(Some("winshare: Parado"));
                copy_link_item.set_enabled(false);
                stop_item.set_enabled(false);
            }
        }
    };

    update_state(&mut last_live, &mut last_viewers);

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_TIMER {
                update_state(&mut last_live, &mut last_viewers);
            }

            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // Clique duplo no ícone da bandeja abre o painel
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::DoubleClick { .. } = event {
                    info!("clique duplo no ícone da bandeja: abrindo painel");
                    open_control_page(&control_url);
                }
            }

            // Eventos do menu da bandeja
            let mut should_quit = false;
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == open_panel_item.id() {
                    info!("menu da bandeja: abrindo painel");
                    open_control_page(&control_url);
                } else if event.id == copy_link_item.id() {
                    let link = state.viewer_link();
                    match arboard::Clipboard::new() {
                        Ok(mut clip) => {
                            if let Err(e) = clip.set_text(&link) {
                                warn!(error = %e, "falha ao copiar link para a área de transferência");
                            } else {
                                info!("link do espectador copiado para a área de transferência");
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "falha ao acessar área de transferência");
                        }
                    }
                } else if event.id == stop_item.id() {
                    info!("menu da bandeja: parando transmissão");
                    state.stop_live();
                    update_state(&mut last_live, &mut last_viewers);
                } else if event.id == autostart_item.id() {
                    let checked = autostart_item.is_checked();
                    if let Err(e) = set_autostart_enabled(checked, &config_path) {
                        warn!(error = %e, "falha ao atualizar inicialização automática no registro");
                        autostart_item.set_checked(!checked);
                    } else {
                        info!(enabled = checked, "inicialização automática atualizada");
                    }
                } else if event.id == quit_item.id() {
                    info!("menu da bandeja: 'Sair' selecionado");
                    state.stop_live();
                    let _ = shutdown_tx.send(true);
                    should_quit = true;
                    break;
                }
            }

            if should_quit {
                break;
            }
        }

        if timer_id != 0 {
            let _ = KillTimer(None, timer_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_autostart_command() {
        let exe = Path::new(r"C:\Apps\winshare\screencast.exe");
        let cfg = Path::new(r"C:\Apps\winshare\config.toml");
        let cmd = format_autostart_command(exe, cfg);
        assert_eq!(
            cmd,
            r#""C:\Apps\winshare\screencast.exe" --config "C:\Apps\winshare\config.toml" --supervised"#
        );
    }

    #[test]
    fn test_format_autostart_command_with_spaces() {
        let exe = Path::new(r"C:\Program Files\winshare\screencast.exe");
        let cfg = Path::new(r"D:\My Configs\custom config.toml");
        let cmd = format_autostart_command(exe, cfg);
        assert_eq!(
            cmd,
            r#""C:\Program Files\winshare\screencast.exe" --config "D:\My Configs\custom config.toml" --supervised"#
        );
    }

    #[test]
    fn test_create_circle_icon() {
        let icon_live = create_circle_icon(220, 38, 38);
        assert!(icon_live.is_ok());
        let icon_stopped = create_circle_icon(128, 128, 128);
        assert!(icon_stopped.is_ok());
    }
}
