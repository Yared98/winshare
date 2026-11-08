//! Lista as janelas visíveis do desktop com título e PID (Win32 EnumWindows)
//! e os monitores disponíveis (windows-capture).

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;
use windows_capture::monitor::Monitor;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Handle HWND como inteiro.
    pub hwnd: u64,
    pub title: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Retorna os monitores disponíveis no sistema.
pub fn list_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    let monitors = match Monitor::enumerate() {
        Ok(m) => m,
        Err(_) => return Ok(Vec::new()),
    };
    let primary_hmon = Monitor::primary().ok().map(|p| p.as_raw_hmonitor());
    let mut result = Vec::with_capacity(monitors.len());
    for (i, m) in monitors.into_iter().enumerate() {
        let id = m.as_raw_hmonitor() as usize as u64;
        let name = m
            .name()
            .or_else(|_| m.device_string())
            .or_else(|_| m.device_name())
            .unwrap_or_else(|_| format!("Monitor {}", i + 1));
        let width = m.width().unwrap_or(0);
        let height = m.height().unwrap_or(0);
        let primary = primary_hmon == Some(m.as_raw_hmonitor());
        result.push(MonitorInfo {
            id,
            name,
            width,
            height,
            primary,
        });
    }
    Ok(result)
}

/// Retorna as janelas visíveis com título não vazio.
pub fn list_visible_windows() -> anyhow::Result<Vec<WindowInfo>> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    let lparam = LPARAM(&mut windows as *mut Vec<WindowInfo> as isize);
    // SAFETY: `enum_proc` só roda durante esta chamada, e `windows` continua vivo até ela retornar.
    if let Err(err) = unsafe { EnumWindows(Some(enum_proc), lparam) } {
        // Em sessões sem desktop / sem janelas, EnumWindows pode retornar FALSE com GetLastError == 0.
        if err.code().0 != 0 {
            return Err(err.into());
        }
    }
    Ok(windows)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` aponta para o Vec criado em `list_visible_windows`, vivo durante o enum.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return TRUE;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return TRUE;
        }
        let title = String::from_utf16_lossy(&buf[..copied as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let windows = &mut *(lparam.0 as *mut Vec<WindowInfo>);
        windows.push(WindowInfo {
            hwnd: hwnd.0 as usize as u64,
            title,
            pid,
        });
    }
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_without_error() {
        // Em sessões sem desktop a lista pode vir vazia; o importante é não falhar.
        let windows = list_visible_windows().expect("EnumWindows falhou");
        assert!(windows.iter().all(|w| !w.title.is_empty()));
    }

    #[test]
    fn list_monitors_does_not_fail() {
        match list_monitors() {
            Ok(monitors) => {
                if monitors.is_empty() {
                    eprintln!("Aviso: nenhum monitor encontrado (ambiente sem desktop interativo)");
                } else {
                    for m in &monitors {
                        assert!(m.id > 0);
                        assert!(!m.name.is_empty());
                    }
                }
            }
            Err(err) => {
                eprintln!("Pulando: listagem de monitores falhou sem desktop interativo: {err:#}");
            }
        }
    }
}
