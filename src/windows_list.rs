//! Lista as janelas visíveis do desktop com título e PID (Win32 EnumWindows).

use serde::Serialize;
use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowInfo {
    /// Handle HWND como inteiro.
    pub hwnd: u64,
    pub title: String,
    pub pid: u32,
}

/// Retorna as janelas visíveis com título não vazio.
pub fn list_visible_windows() -> anyhow::Result<Vec<WindowInfo>> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    let lparam = LPARAM(&mut windows as *mut Vec<WindowInfo> as isize);
    // SAFETY: `enum_proc` só roda durante esta chamada, e `windows` continua vivo até ela retornar.
    unsafe { EnumWindows(Some(enum_proc), lparam)? };
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
}
