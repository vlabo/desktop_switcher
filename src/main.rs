#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("This app is Windows-only.");
}

#[cfg(windows)]
mod app {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_NOREPEAT, VK_1,
    };
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageW, IsIconic, IsWindow,
        LoadIconW, PostQuitMessage, RegisterClassW, SetForegroundWindow, ShowWindow,
        TrackPopupMenu, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HICON, HMENU,
        IDI_APPLICATION, MF_SEPARATOR, MF_STRING, MSG, SW_RESTORE, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
        TPM_RIGHTBUTTON, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_HOTKEY, WM_RBUTTONUP, WM_USER,
        WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };

    const WM_TRAYICON: u32 = WM_USER + 1;

    const HOTKEY_FIRST: i32 = 1;
    const HOTKEY_LAST: i32 = 9;

    const MENU_EXIT_ID: usize = 1000;

    #[derive(Default)]
    struct State {
        // Store raw handle value to keep State Send+Sync.
        last_focus_by_desktop: HashMap<u32, usize>,
    }

    static STATE: OnceLock<Mutex<State>> = OnceLock::new();

    fn state() -> &'static Mutex<State> {
        STATE.get_or_init(|| Mutex::new(State::default()))
    }

    fn wstr(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(once(0)).collect()
    }

    unsafe fn save_focus_for_current_desktop(app_hwnd: HWND) {
        let fg = unsafe { GetForegroundWindow() };
        if fg.0.is_null() || fg == app_hwnd {
            return;
        }

        let Ok(cur) = winvd::get_current_desktop() else {
            return;
        };
        let Ok(idx) = cur.get_index() else {
            return;
        };

        if let Ok(mut st) = state().lock() {
            st.last_focus_by_desktop.insert(idx, fg.0 as usize);
        }
    }

    unsafe fn restore_focus_for_desktop(desktop_index: u32) {
        let hwnd = {
            let Ok(st) = state().lock() else {
                return;
            };
            st.last_focus_by_desktop.get(&desktop_index).copied()
        };

        let Some(hwnd_raw) = hwnd else {
            return;
        };

        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);

        if unsafe { IsWindow(hwnd) }.as_bool() == false {
            return;
        }

        // Only focus if the window is on that desktop, or is pinned.
        let pinned = winvd::is_pinned_window(hwnd).unwrap_or(false);
        let on_desktop = winvd::get_desktop_by_window(hwnd)
            .and_then(|d| d.get_index())
            .map(|i| i == desktop_index)
            .unwrap_or(false);
        if !pinned && !on_desktop {
            return;
        }

        if unsafe { IsIconic(hwnd) }.as_bool() {
            let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
        }
        let _ = unsafe { SetForegroundWindow(hwnd) };
    }

    unsafe fn try_switch_desktop(app_hwnd: HWND, desktop_index: u32) {
        unsafe { save_focus_for_current_desktop(app_hwnd) };

        let Ok(count) = winvd::get_desktop_count() else {
            return;
        };
        if desktop_index >= count {
            return;
        }
        if winvd::switch_desktop(desktop_index).is_err() {
            return;
        }

        // Switching can be async; retry briefly until the desktop becomes current.
        for _ in 0..12 {
            let on_target = winvd::get_current_desktop()
                .and_then(|d| d.get_index())
                .map(|i| i == desktop_index)
                .unwrap_or(true);

            if on_target {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }

        unsafe { restore_focus_for_desktop(desktop_index) };
    }

    unsafe fn add_tray_icon(hwnd: HWND, hicon: HICON) {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = hicon;

        let tip = wstr("d_switch (Alt+1..9)");
        let tip_len = nid.szTip.len().min(tip.len());
        nid.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);

        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        }
    }

    unsafe fn remove_tray_icon(hwnd: HWND) {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    unsafe fn show_tray_menu(hwnd: HWND) {
        unsafe {
            let menu = CreatePopupMenu().unwrap_or(HMENU(null_mut()));
            if menu.0.is_null() {
                return;
            }

            // Optional direct desktop entries (still "source code as config").
            for i in HOTKEY_FIRST..=HOTKEY_LAST {
                let label = wstr(&format!("Desktop {}\tAlt+{}", i, i));
                let _ = AppendMenuW(menu, MF_STRING, i as usize, PCWSTR(label.as_ptr()));
            }

            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR(null()));
            let exit = wstr("Exit");
            let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT_ID, PCWSTR(exit.as_ptr()));

            let mut pt = windows::Win32::Foundation::POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                hwnd,
                None,
            );
        }
    }

    unsafe fn register_hotkeys(hwnd: HWND) {
        for id in HOTKEY_FIRST..=HOTKEY_LAST {
            let vk = (VK_1.0 + (id - 1) as u16) as u32;
            // MOD_NOREPEAT: prevent repeats while holding keys.
            unsafe {
                let mods: HOT_KEY_MODIFIERS = MOD_ALT | MOD_NOREPEAT;
                let _ = RegisterHotKey(hwnd, id, mods, vk);
            }
        }
    }

    unsafe fn unregister_hotkeys(hwnd: HWND) {
        for id in HOTKEY_FIRST..=HOTKEY_LAST {
            unsafe {
                let _ = UnregisterHotKey(hwnd, id);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CREATE => {
                let hicon =
                    unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or(HICON(null_mut()));
                unsafe {
                    add_tray_icon(hwnd, hicon);
                    register_hotkeys(hwnd);
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                let id = wparam.0 as i32;
                if (HOTKEY_FIRST..=HOTKEY_LAST).contains(&id) {
                    let desktop_index = (id - 1) as u32;
                    unsafe { try_switch_desktop(hwnd, desktop_index) };
                }
                LRESULT(0)
            }
            WM_TRAYICON => {
                // Right click (or key equivalent) on the tray icon.
                if lparam.0 as u32 == WM_RBUTTONUP {
                    unsafe { show_tray_menu(hwnd) };
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let cmd = (wparam.0 & 0xffff) as usize;
                if cmd == MENU_EXIT_ID {
                    let _ = unsafe { DestroyWindow(hwnd) };
                    return LRESULT(0);
                }

                // Desktop menu entries use IDs 1..9.
                if (HOTKEY_FIRST as usize..=HOTKEY_LAST as usize).contains(&cmd) {
                    unsafe { try_switch_desktop(hwnd, (cmd as u32) - 1) };
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe {
                    unregister_hotkeys(hwnd);
                    remove_tray_icon(hwnd);
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    pub fn run() -> windows::core::Result<()> {
        unsafe {
            // Ensure COM is initialized for Win32 shell APIs / winvd internals.
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );

            let hinstance = GetModuleHandleW(PCWSTR(null()))?;
            let class_name = wstr("d_switch_hidden_window");

            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };

            let atom = RegisterClassW(&wc);
            if atom == 0 {
                // If it fails, still try to proceed: CreateWindowExW will fail if needed.
            }

            let _hwnd = CreateWindowExW(
                Default::default(),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(wstr("d_switch").as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                hinstance,
                None,
            )?;

            // Standard message loop.
            let mut msg = MSG::default();
            loop {
                let res = GetMessageW(&mut msg, HWND(null_mut()), 0, 0);
                if res == BOOL(0) {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let _ = windows::Win32::System::Com::CoUninitialize();
            Ok(())
        }
    }
}

#[cfg(windows)]
fn main() {
    let _ = app::run();
}
