//! Desktop host for third-party wallpaper engines (Wallpaper Engine, Lively) while `explorer.exe`
//! is suppressed.
//!
//! Those engines don't need an explorer *service*; they need explorer's desktop window. They find
//! the `Progman` ("Program Manager") window, send it the undocumented message `0x052C`, and parent
//! their render window into the `WorkerW` that explorer creates behind the desktop icons. With
//! explorer gone there is no `Progman`, so they have nowhere to draw.
//!
//! This module provides a stand-in `Progman` with the Windows 11 24H2+ child layout (`WorkerW`
//! child, plus an empty `SHELLDLL_DefView` for engines that look for it). The host only exists
//! while explorer is suppressed and no real `Progman` is present, stays hidden until an engine
//! asks for a `WorkerW`, and hides `chickenwm-paper` while an engine is attached.

use chickenwm_core::failsafe::EXPLORER_KILLER_ENABLED;
use chickenwm_core::monitor::reserve_work_areas_without_taskbar;
use std::cell::Cell;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};
use tracing::info;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    GetShellWindow, GetSystemMetrics, GetWindow, IsWindowVisible, PostQuitMessage, RegisterClassW,
    SetTimer, SetWindowPos, ShowWindow, TranslateMessage, GW_CHILD, HWND_BOTTOM, MA_NOACTIVATE,
    MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WINDOWPOS, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_MOUSEACTIVATE, WM_TIMER, WM_WINDOWPOSCHANGING, WNDCLASSW, WS_CHILD,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

/// Undocumented message wallpaper engines send to `Progman` to request a `WorkerW` to draw into.
const WM_SPAWN_WORKERW: u32 = 0x052C;
const TICK_TIMER_ID: usize = 1;
/// After showing the host, give the engine this long to parent its window before treating an
/// empty `WorkerW` as "engine gone".
const ATTACH_GRACE: Duration = Duration::from_secs(5);

#[link(name = "user32")]
unsafe extern "system" {
    /// Registers the desktop window returned by `GetShellWindow`. Not exposed by the `windows`
    /// crate; only succeeds while no other shell window exists.
    fn SetShellWindow(hwnd: HWND) -> BOOL;
}

thread_local! {
    static WORKERW: Cell<isize> = const { Cell::new(0) };
    static SHOWN_AT: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn virtual_screen() -> [i32; 4] {
    unsafe {
        [
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        ]
    }
}

fn find_window(class: &str) -> Option<HWND> {
    let class = wide(class);
    unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) }
        .ok()
        .filter(|h| !h.0.is_null())
}

/// Shows or hides `chickenwm-paper` so an attached engine's wallpaper isn't covered by ours.
fn set_paper_visible(visible: bool) {
    if let Some(paper) = find_window("ChickenWMPaperClass") {
        unsafe {
            let _ = ShowWindow(paper, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }
}

/// Sizes the host to the virtual screen, with the `WorkerW` child filling it.
fn fit_to_virtual_screen(progman: HWND, show: bool) {
    let [x, y, w, h] = virtual_screen();
    let flags = if show {
        SWP_NOACTIVATE | SWP_SHOWWINDOW
    } else {
        SWP_NOACTIVATE
    };
    unsafe {
        let _ = SetWindowPos(progman, HWND_BOTTOM, x, y, w, h, flags);
        let workerw = HWND(WORKERW.get() as *mut core::ffi::c_void);
        if !workerw.0.is_null() {
            let _ = SetWindowPos(workerw, None, 0, 0, w, h, SWP_NOACTIVATE);
        }
    }
}

fn engine_attached() -> bool {
    let workerw = HWND(WORKERW.get() as *mut core::ffi::c_void);
    !workerw.0.is_null() && unsafe { GetWindow(workerw, GW_CHILD) }.is_ok_and(|c| !c.0.is_null())
}

fn tick(progman: HWND) {
    // Explorer is coming back (suppression turned off / failsafe): step aside for the real one.
    if !EXPLORER_KILLER_ENABLED.load(Ordering::Relaxed) {
        unsafe {
            let _ = DestroyWindow(progman);
        }
        return;
    }
    unsafe {
        if GetShellWindow().0.is_null() {
            let _ = SetShellWindow(progman);
        }
        if !IsWindowVisible(progman).as_bool() {
            return;
        }
    }
    let past_grace = SHOWN_AT
        .get()
        .is_none_or(|t| t.elapsed() >= ATTACH_GRACE);
    if past_grace && !engine_attached() {
        info!("Wallpaper engine detached; restoring chickenwm-paper.");
        unsafe {
            let _ = ShowWindow(progman, SW_HIDE);
        }
        SHOWN_AT.set(None);
        set_paper_visible(true);
    }
}

unsafe extern "system" fn progman_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_SPAWN_WORKERW => {
                if SHOWN_AT.get().is_none() {
                    info!("Wallpaper engine requested a WorkerW; activating desktop host.");
                }
                set_paper_visible(false);
                fit_to_virtual_screen(hwnd, true);
                SHOWN_AT.set(Some(Instant::now()));
                LRESULT(0)
            }
            // Stay at the bottom of the z-order: clicks on the wallpaper must never raise it
            // over normal windows.
            WM_WINDOWPOSCHANGING if lparam.0 != 0 => {
                let wp = &mut *(lparam.0 as *mut WINDOWPOS);
                wp.hwndInsertAfter = HWND_BOTTOM;
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_DISPLAYCHANGE => {
                fit_to_virtual_screen(hwnd, false);
                reserve_work_areas_without_taskbar();
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TICK_TIMER_ID => {
                tick(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe extern "system" fn plain_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Registers the window classes and builds the hidden host tree. Returns the `Progman` window.
fn create_host() -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
        let progman_class = wide("Progman");
        let workerw_class = wide("WorkerW");
        let defview_class = wide("SHELLDLL_DefView");
        let listview_class = wide("SysListView32");

        for (class, proc, brush) in [
            (&progman_class, progman_proc as _, black),
            (&workerw_class, plain_proc as _, black),
            (&defview_class, plain_proc as _, HBRUSH::default()),
            (&listview_class, plain_proc as _, HBRUSH::default()),
        ] {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                hbrBackground: brush,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
        }

        let [x, y, w, h] = virtual_screen();
        let title = wide("Program Manager");
        let progman = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(progman_class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance,
            None,
        )
        .ok()?;

        // Empty icon view: some engines locate `SHELLDLL_DefView` before the `WorkerW`.
        let empty = wide("");
        let folder_view = wide("FolderView");
        if let Ok(defview) = CreateWindowExW(
            Default::default(),
            PCWSTR(defview_class.as_ptr()),
            PCWSTR(empty.as_ptr()),
            WS_CHILD,
            0,
            0,
            0,
            0,
            progman,
            None,
            hinstance,
            None,
        ) {
            let _ = CreateWindowExW(
                Default::default(),
                PCWSTR(listview_class.as_ptr()),
                PCWSTR(folder_view.as_ptr()),
                WS_CHILD,
                0,
                0,
                0,
                0,
                defview,
                None,
                hinstance,
                None,
            );
        }

        let workerw = CreateWindowExW(
            Default::default(),
            PCWSTR(workerw_class.as_ptr()),
            PCWSTR(empty.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            0,
            w,
            h,
            progman,
            None,
            hinstance,
            None,
        )
        .ok()?;
        WORKERW.set(workerw.0 as isize);

        let _ = SetShellWindow(progman);
        let _ = SetTimer(progman, TICK_TIMER_ID, 1000, None);
        Some(progman)
    }
}

/// Spawns the desktop host thread. It waits until explorer is suppressed and gone, serves the
/// host until explorer returns, then goes back to waiting.
pub fn spawn_desktop_host_thread() {
    let _ = thread::Builder::new()
        .name("chickenwm-desktop-host".to_string())
        .spawn(|| loop {
            let explorer_gone = EXPLORER_KILLER_ENABLED.load(Ordering::Relaxed)
                && find_window("Progman").is_none();
            if !explorer_gone {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            // Explorer's taskbar reservation outlives explorer; drop it so tiles reach the
            // bottom of the screen.
            reserve_work_areas_without_taskbar();
            let Some(_progman) = create_host() else {
                thread::sleep(Duration::from_secs(2));
                continue;
            };
            info!("Desktop host ready for wallpaper engines (Progman/WorkerW).");
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            WORKERW.set(0);
            SHOWN_AT.set(None);
            set_paper_visible(true);
            info!("Desktop host closed (explorer.exe returning).");
        });
}
