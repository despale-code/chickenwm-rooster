//! Phase 1.4, Phase 6.1, Phase 6.2 & Phase 10.2: `chickenwm-rooster`
//! - Emergency Failsafe (`--restore-explorer` CLI flag + `Ctrl+Alt+Shift+Esc` hardware listener)
//! - `explorer.exe` Process Suppressor & `Winlogon\Shell` + `Directory\shell` Manager
//! - `Shell_TrayWnd` System Tray & Desktop Host (prevents legacy apps hanging on `Shell_NotifyIcon`)
//! - Supervisor Watchdog (`chickenwm-wm`, `chickenwm-binds`, `chickenwm-paper`) with `<50ms` respawn
//!   and 3-crashes-in-10s automatic `explorer.exe` failsafe recovery
//! - Primary IPC Server (`\\.\pipe\chickenwm-ipc`)

// No console window: the daemon runs as the Winlogon shell at login.
#![windows_subsystem = "windows"]

mod desktop_host;

use anyhow::Result;
use clap::Parser;
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::config::{load_or_init_config, HyprConfig};
use chickenwm_core::failsafe::{
    eject_dwm_hooks, is_winlogon_shell_chickenwm, register_files_shell_handler, set_winlogon_shell,
    spawn_emergency_failsafe_thread, trigger_emergency_restore, DWM_HOOK_ACTIVE,
    EXPLORER_KILLER_ENABLED, FAILSAFE_TRIGGERED,
};
use chickenwm_core::handles::SafeHandle;
use chickenwm_core::init_tracing;
use chickenwm_core::ipc::{
    send_ipc_command_to, serve_ipc, Action, DaemonStatus, IpcCommand, IpcResponse, PIPE_NAME_BAR,
    PIPE_NAME_DAEMON, PIPE_NAME_LAUNCHER, PIPE_NAME_PAPER, PIPE_NAME_WM,
};
use chickenwm_core::monitor::enumerate_monitors;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetMessageW, RegisterClassW, RegisterWindowMessageW,
    SendNotifyMessageW, HWND_BROADCAST, MSG, WM_COPYDATA, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

#[derive(Parser, Debug)]
#[command(
    name = "chickenwm-rooster",
    version,
    about = "ChickenWM Master Shell Supervisor, IPC Router, Tray Host & Watchdog"
)]
struct Cli {
    /// Phase 1.4: Immediately restore `explorer.exe` as the default Windows shell and exit.
    #[arg(long = "restore-explorer")]
    restore_explorer: bool,

    /// Unhide and restore all windows that were left hidden on inactive workspaces and exit.
    #[arg(long = "restore-windows")]
    restore_windows: bool,

    /// Opt-in: Set ChickenWM as the `Winlogon\Shell` and actively suppress `explorer.exe`.
    #[arg(long = "replace-shell")]
    replace_shell: bool,

    /// Run without spawning child modules (useful when debugging a single module standalone).
    #[arg(long = "no-children")]
    no_children: bool,

    /// Install ChickenWM as the per-user login shell so it starts automatically at every login
    /// (in place of explorer.exe), then exit. Undo with `--uninstall-autostart`.
    #[arg(long = "install-autostart")]
    install_autostart: bool,

    /// Remove the autostart shell entry (restores explorer.exe as the login shell) and exit.
    #[arg(long = "uninstall-autostart")]
    uninstall_autostart: bool,

    /// Register the scheduled task that starts ChickenWM as administrator without a prompt
    /// (needs an elevated shell; Settings runs this for you) and exit.
    #[arg(long = "install-admin-task")]
    install_admin_task: bool,

    /// Remove that scheduled task and exit.
    #[arg(long = "remove-admin-task")]
    remove_admin_task: bool,

    /// Internal: wait for the daemon with this PID to exit before starting (used when an
    /// existing daemon hands the shell over to this elevated one).
    #[arg(long = "takeover")]
    takeover: Option<u32>,
}

/// Waits (up to `timeout`) for process `pid` to exit.
fn wait_for_process_exit(pid: u32, timeout: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(handle, timeout.as_millis() as u32);
            let _ = CloseHandle(handle);
        }
    }
}

/// Deliberately restarts every supervised module. Done under the module lock and with the crash
/// history cleared, so the watchdog neither races the restart nor counts it towards the
/// "3 crashes in 10 s" explorer failsafe.
fn restart_modules(modules: &Mutex<Vec<WatchedModule>>) {
    let mut mods = modules.lock();
    for m in mods.iter_mut() {
        if let Some(mut child) = m.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    // Windows the old WM parked on other workspaces would stay hidden forever otherwise.
    chickenwm_core::failsafe::restore_all_desktop_windows();
    // Give the window manager's pipe and hooks a moment to be released before respawning.
    thread::sleep(Duration::from_millis(300));
    for m in mods.iter_mut() {
        m.crash_timestamps.clear();
        m.spawn_child();
    }
}

/// Stops the supervised modules and the daemon without restoring explorer, because an elevated
/// daemon (already launched by the caller) is taking over.
fn hand_over_shell() -> ! {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    EXPLORER_KILLER_ENABLED.store(false, Ordering::SeqCst);
    chickenwm_core::failsafe::terminate_modules();
    std::process::exit(0)
}

/// Raises the daemon (and thus its whole watchdog/IPC pipeline) to HIGH priority so input and
/// window management stay responsive under load. Not REALTIME — that can starve the OS.
fn raise_process_priority() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS};
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
}

struct WatchedModule {
    name: &'static str,
    exe_name: &'static str,
    child: Option<Child>,
    crash_timestamps: VecDeque<Instant>,
}

/// `CREATE_NO_WINDOW`: the daemon has no console, so console-subsystem ChickenWM modules would
/// otherwise each open their own terminal window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl WatchedModule {
    fn new(name: &'static str, exe_name: &'static str) -> Self {
        Self {
            name,
            exe_name,
            child: None,
            crash_timestamps: VecDeque::new(),
        }
    }

    fn spawn_child(&mut self) -> bool {
        let mut exe_path = std::env::current_exe().unwrap_or_default();
        exe_path.set_file_name(self.exe_name);
        let res = if exe_path.exists() {
            Command::new(&exe_path).creation_flags(CREATE_NO_WINDOW).spawn()
        } else {
            Command::new(self.exe_name).creation_flags(CREATE_NO_WINDOW).spawn()
        };
        match res {
            Ok(c) => {
                info!("Spawned supervised module {} (PID {})", self.name, c.id());
                self.child = Some(c);
                true
            }
            Err(e) => {
                warn!("Could not spawn {}: {}", self.name, e);
                false
            }
        }
    }
}

/// Phase 6.1: Terminates any running `explorer.exe` instances when `EXPLORER_KILLER_ENABLED` is `true`.
fn kill_unauthorized_explorer_instances() {
    if !EXPLORER_KILLER_ENABLED.load(Ordering::Relaxed)
        || FAILSAFE_TRIGGERED.load(Ordering::Relaxed)
    {
        return;
    }

    unsafe {
        let Ok(snap_raw) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let Some(snap) = SafeHandle::new(snap_raw) else {
            return;
        };

        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap.raw(), &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let proc_name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if proc_name.eq_ignore_ascii_case("explorer.exe") {
                    if let Ok(proc_raw) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID)
                    {
                        if let Some(proc) = SafeHandle::new(proc_raw) {
                            info!(
                                "Suppressing unauthorized explorer.exe instance (PID {})",
                                entry.th32ProcessID
                            );
                            let _ = TerminateProcess(proc.raw(), 0);
                        }
                    }
                }
                if Process32NextW(snap.raw(), &mut entry).is_err() {
                    break;
                }
            }
        }
    }
}

/// Phase 6.2: System Tray & Desktop Host (`Shell_TrayWnd`) so legacy tray apps broadcasting
/// `Shell_NotifyIcon` (`WM_COPYDATA`) never hang when `explorer.exe` is suppressed.
/// Reads the balloon text out of a `Shell_NotifyIcon` `WM_COPYDATA` (`TRAYNOTIFYDATAW`, which
/// carries `NOTIFYICONDATAW` in its fixed 32-bit-compatible layout).
unsafe fn parse_tray_balloon(lparam: LPARAM) -> Option<chickenwm_core::ipc::Notification> {
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    const NIM_ADD: u32 = 0;
    const NIM_MODIFY: u32 = 1;
    const NIF_INFO: u32 = 0x10;
    const NIIF_ERROR: u32 = 3;
    let cds = unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref()? };
    if cds.dwData != 1 || cds.lpData.is_null() || (cds.cbData as usize) < 944 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let wstr_at = |o: usize, chars: usize| {
        let units: Vec<u16> = bytes[o..o + chars * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    };
    if u32_at(0) != 0x3475_3423 {
        return None; // TRAYNOTIFYDATA signature
    }
    let message = u32_at(4);
    let flags = u32_at(20);
    if !matches!(message, NIM_ADD | NIM_MODIFY) || flags & NIF_INFO == 0 {
        return None;
    }
    let body = wstr_at(296, 256);
    let title = wstr_at(812, 64);
    if body.trim().is_empty() && title.trim().is_empty() {
        return None; // an empty balloon means "hide the balloon"
    }
    let info_flags = u32_at(940);
    // App name: the process that owns the icon's window.
    let owner = HWND(u32_at(12) as usize as *mut core::ffi::c_void);
    let mut pid = 0u32;
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(owner, Some(&mut pid)) };
    let app = process_display_name(pid).unwrap_or_else(|| "App".to_string());
    Some(chickenwm_core::ipc::Notification {
        app,
        title: if title.is_empty() { body.lines().next().unwrap_or_default().to_string() } else { title },
        body,
        timeout_secs: None,
        urgency: (info_flags & 0x0F == NIIF_ERROR).then(|| "critical".to_string()),
    })
}

/// "Steam" for steam.exe: the executable's name without extension, capitalized.
fn process_display_name(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let stem = std::path::Path::new(&path).file_stem()?.to_string_lossy().into_owned();
        let mut chars = stem.chars();
        Some(chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or(stem))
    }
}

/// Icons registered with our `Shell_TrayWnd` (`Shell_NotifyIcon`), served to roobar.
static TRAY: std::sync::Mutex<Vec<chickenwm_core::ipc::TrayIcon>> = std::sync::Mutex::new(Vec::new());

fn tray_icons() -> Vec<chickenwm_core::ipc::TrayIcon> {
    let mut icons = TRAY.lock().map(|t| t.clone()).unwrap_or_default();
    // Apps that exited without NIM_DELETE leave their icon behind: drop dead windows.
    icons.retain(|i| unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(HWND(i.hwnd as usize as *mut core::ffi::c_void)).as_bool() });
    icons
}

/// Applies a `Shell_NotifyIcon` add / modify / delete / set-version to `TRAY`.
unsafe fn track_tray_icon(lparam: LPARAM) {
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::UI::WindowsAndMessaging::{CopyIcon, DestroyIcon, HICON};
    const NIM_ADD: u32 = 0;
    const NIM_MODIFY: u32 = 1;
    const NIM_DELETE: u32 = 2;
    const NIM_SETVERSION: u32 = 4;
    const NIF_MESSAGE: u32 = 1;
    const NIF_ICON: u32 = 2;
    const NIF_TIP: u32 = 4;
    const NIF_STATE: u32 = 8;
    let Some(cds) = (unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref() }) else { return };
    if cds.dwData != 1 || cds.lpData.is_null() || (cds.cbData as usize) < 812 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    if u32_at(0) != 0x3475_3423 {
        return;
    }
    let (message, hwnd, uid, flags) = (u32_at(4), u64::from(u32_at(12)), u32_at(16), u32_at(20));
    let Ok(mut tray) = TRAY.lock() else { return };
    let pos = tray.iter().position(|i| i.hwnd == hwnd && i.uid == uid);
    let free = |icon: u64| {
        if icon != 0 {
            let _ = unsafe { DestroyIcon(HICON(icon as usize as *mut core::ffi::c_void)) };
        }
    };
    match message {
        NIM_DELETE => {
            if let Some(p) = pos {
                free(tray.remove(p).icon);
            }
        }
        NIM_SETVERSION => {
            if let Some(p) = pos {
                tray[p].version = u32_at(808);
            }
        }
        NIM_ADD | NIM_MODIFY => {
            if pos.is_none() && message == NIM_MODIFY {
                return;
            }
            let p = pos.unwrap_or_else(|| {
                let mut pid = 0u32;
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(HWND(hwnd as usize as *mut core::ffi::c_void), Some(&mut pid)) };
                tray.push(chickenwm_core::ipc::TrayIcon { hwnd, uid, pid, ..Default::default() });
                tray.len() - 1
            });
            let item = &mut tray[p];
            if flags & NIF_MESSAGE != 0 {
                item.callback_message = u32_at(24);
            }
            if flags & NIF_ICON != 0 {
                // The app may destroy its icon after this call: keep our own copy.
                let theirs = HICON(u32_at(28) as usize as *mut core::ffi::c_void);
                let copy = unsafe { CopyIcon(theirs) }.map(|h| h.0 as usize as u64).unwrap_or(0);
                free(std::mem::replace(&mut item.icon, copy));
            }
            if flags & NIF_TIP != 0 {
                let units: Vec<u16> = bytes[32..32 + 256].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
                item.tip = String::from_utf16_lossy(&units);
            }
            if flags & NIF_STATE != 0 {
                let (state, mask) = (u32_at(288), u32_at(292));
                if mask & 1 != 0 {
                    item.hidden = state & 1 != 0; // NIS_HIDDEN
                }
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn shell_tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COPYDATA {
        unsafe { track_tray_icon(lparam) };
        // Shell_NotifyIcon: show balloon text through chickenwm-notify, then acknowledge with
        // TRUE (1) so callers never block.
        if let Some(note) = unsafe { parse_tray_balloon(lparam) } {
            thread::spawn(move || {
                let _ = chickenwm_core::ipc::send_ipc_command_blocking(
                    chickenwm_core::ipc::PIPE_NAME_NOTIFY,
                    &chickenwm_core::ipc::IpcCommand::Notify(note),
                );
            });
        }
        return LRESULT(1);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn spawn_system_tray_host_thread() {
    let _ = thread::Builder::new()
        .name("chickenwm-tray-host".to_string())
        .spawn(|| unsafe {
            let class_name: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
            let Ok(hinstance) = GetModuleHandleW(None) else {
                return;
            };
            let wc = WNDCLASSW {
                lpfnWndProc: Some(shell_tray_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            // Only registers if explorer.exe isn't currently owning Shell_TrayWnd
            if RegisterClassW(&wc) != 0 {
                let Ok(hwnd) = CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    PCWSTR(class_name.as_ptr()),
                    PCWSTR(class_name.as_ptr()),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    hinstance,
                    None,
                ) else {
                    return;
                };
                if !hwnd.0.is_null() {
                    let tb_created: Vec<u16> = "TaskbarCreated\0".encode_utf16().collect();
                    let msg_id = RegisterWindowMessageW(PCWSTR(tb_created.as_ptr()));
                    if msg_id != 0 {
                        let _ = SendNotifyMessageW(HWND_BROADCAST, msg_id, WPARAM(0), LPARAM(0));
                    }
                    info!("Registered ChickenWM Shell_TrayWnd system tray broadcast host.");
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                        windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
                    }
                }
            }
        });
}

fn execute_exec_once_commands(cfg: &HyprConfig) {
    for cmd_line in &cfg.exec_once {
        // Core supervised modules are managed by the watchdog directly
        let lower = cmd_line.to_ascii_lowercase();
        if lower.starts_with("chickenwm-paper")
            || lower.starts_with("chickenwm-wm")
            || lower.starts_with("chickenwm-binds")
            || lower.starts_with("roobar")
        {
            continue;
        }
        info!("Running exec-once startup command: {}", cmd_line);
        let parts: Vec<&str> = cmd_line.split_whitespace().collect();
        if let Some((prog, args)) = parts.split_first() {
            let mut full_exe = std::env::current_exe().unwrap_or_default();
            full_exe.set_file_name(prog);
            if full_exe.exists() {
                let _ = Command::new(full_exe)
                    .args(args)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
            } else {
                let _ = Command::new(prog).args(args).spawn();
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // GUI subsystem: borrow the launching terminal's console (if any) so CLI flags still print.
    unsafe {
        let _ = windows::Win32::System::Console::AttachConsole(
            windows::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
    init_tracing("chickenwm_rooster");
    let cli = Cli::parse();

    if cli.install_autostart {
        let exe = std::env::current_exe()?.to_string_lossy().into_owned();
        set_winlogon_shell(true, Some(&exe))?;
        println!("ok: ChickenWM set as the login shell. It will start automatically at your next login.");
        println!("     exe: {exe}");
        println!("     undo: chickenwm-rooster --uninstall-autostart  (or Ctrl+Alt+Shift+Esc)");
        return Ok(());
    }
    if cli.uninstall_autostart {
        set_winlogon_shell(false, None)?;
        println!("ok: autostart removed; explorer.exe restored as the login shell.");
        return Ok(());
    }

    if cli.install_admin_task || cli.remove_admin_task {
        let exe = std::env::current_exe()?;
        let res = if cli.install_admin_task {
            chickenwm_core::elevation::install_admin_task(&exe)
        } else {
            chickenwm_core::elevation::remove_admin_task()
        };
        return match res {
            Ok(()) => {
                println!("ok");
                Ok(())
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1)
            }
        };
    }

    // An older daemon is handing the shell over to this (elevated) one: let it finish first.
    if let Some(pid) = cli.takeover {
        wait_for_process_exit(pid, Duration::from_secs(15));
    }

    // One daemon per session, decided before we touch the shell registry or explorer. A second
    // copy (typically Winlogon relaunching the shell after explorer was closed) idles until the
    // running daemon exits instead of exiting itself, so Winlogon doesn't keep relaunching it.
    let wait_ms = if cli.takeover.is_some() { 10_000 } else { 0 };
    let mut instance_guard = match chickenwm_core::instance::claim_daemon_instance(wait_ms) {
        chickenwm_core::instance::Instance::Primary(guard) => Some(guard),
        dup @ chickenwm_core::instance::Instance::Duplicate(_) => {
            info!("chickenwm-rooster is already running; this copy stays idle");
            dup.wait_for_exit();
            return Ok(());
        }
    };

    // High priority so dragging/tiling stays smooth while apps are busy.
    raise_process_priority();

    // Phase 1.4: Immediate `--restore-explorer` CLI recovery flag
    if cli.restore_explorer {
        trigger_emergency_restore(true);
        return Ok(());
    }

    // Immediate `--restore-windows` CLI recovery flag
    if cli.restore_windows {
        let count = chickenwm_core::failsafe::restore_all_desktop_windows();
        println!("ok: restored {count} hidden desktop windows.");
        return Ok(());
    }

    // Phase 1.4: Dedicated zero-dependency hardware failsafe listener (`Ctrl + Alt + Shift + Escape`)
    let _failsafe = spawn_emergency_failsafe_thread();
    // `theme { auto_dark = … }`: light/dark by the clock (or sunset / sunrise).
    chickenwm_core::theme_tools::spawn_auto_dark_thread();

    let (_doc, initial_cfg) = bootstrap_config_and_themes()?;
    let start_time = Instant::now();

    // "Run as administrator" is on but we were started normally (e.g. as the login shell): start
    // the elevated copy (silently through the scheduled task, else via the UAC prompt) and let it
    // take over. Declining the prompt just carries on unelevated.
    if initial_cfg.misc.run_as_admin
        && !cli.no_children
        && cli.takeover.is_none()
        && !chickenwm_core::elevation::is_elevated()
    {
        if chickenwm_core::elevation::admin_task_exists() {
            // The task's daemon has no --takeover PID to wait for: let go of the instance
            // mutex first so it becomes the daemon instead of idling behind us.
            if let Some(guard) = instance_guard.take() {
                guard.release();
            }
            if chickenwm_core::elevation::start_admin_task() {
                info!("Started the elevated ChickenWM task; this unelevated daemon exits");
                return Ok(());
            }
            instance_guard = match chickenwm_core::instance::claim_daemon_instance(0) {
                chickenwm_core::instance::Instance::Primary(guard) => Some(guard),
                chickenwm_core::instance::Instance::Duplicate(_) => return Ok(()),
            };
        }
        let exe = std::env::current_exe()?;
        let pid = std::process::id().to_string();
        match chickenwm_core::elevation::run_elevated(&exe, &["--takeover", &pid], false) {
            Ok(_) => return Ok(()),
            Err(e) => warn!("Staying unelevated: {e}"),
        }
    }
    // Held until the process exits.
    let _instance_guard = instance_guard;

    if cli.replace_shell || initial_cfg.misc.suppress_explorer {
        EXPLORER_KILLER_ENABLED.store(true, Ordering::SeqCst);
    }
    if cli.replace_shell || initial_cfg.misc.replace_winlogon_shell {
        let _ = set_winlogon_shell(true, None);
    }
    if initial_cfg.misc.register_files_handler {
        let _ = register_files_shell_handler(true, None);
    }

    // Phase 6.2: Start System Tray & Desktop Host thread
    spawn_system_tray_host_thread();
    // Stand-in Progman/WorkerW so Wallpaper Engine / Lively keep working without explorer.exe.
    desktop_host::spawn_desktop_host_thread();

    // Phase 6.1: Spawn explorer.exe process suppressor polling thread (checks every 150ms when enabled)
    thread::Builder::new()
        .name("chickenwm-explorer-suppressor".to_string())
        .spawn(|| loop {
            kill_unauthorized_explorer_instances();
            thread::sleep(Duration::from_millis(150));
        })?;

    // Phase 6.1: Run `exec-once` commands
    execute_exec_once_commands(&initial_cfg);

    let shared_cfg = Arc::new(Mutex::new(initial_cfg));
    let modules: Arc<Mutex<Vec<WatchedModule>>> = Arc::new(Mutex::new(vec![
        WatchedModule::new("chickenwm-wm", "chickenwm-wm.exe"),
        WatchedModule::new("chickenwm-binds", "chickenwm-binds.exe"),
        WatchedModule::new("chickenwm-paper", "chickenwm-paper.exe"),
        WatchedModule::new("roobar", "roobar.exe"),
        WatchedModule::new("chickenwm-notify", "chickenwm-notify.exe"),
    ]));

    // Phase 10.2: Supervisor Watchdog (<50ms respawn, 3 crashes in 10s -> emergency restore)
    if !cli.no_children {
        {
            let mut mods = modules.lock();
            for m in mods.iter_mut() {
                m.spawn_child();
            }
        }

        let mods_watchdog = Arc::clone(&modules);
        thread::Builder::new()
            .name("chickenwm-supervisor-watchdog".to_string())
            .spawn(move || loop {
                thread::sleep(Duration::from_millis(35));
                if FAILSAFE_TRIGGERED.load(Ordering::Relaxed)
                    || SHUTTING_DOWN.load(Ordering::Relaxed)
                {
                    break;
                }
                let mut mods = mods_watchdog.lock();
                for m in mods.iter_mut() {
                    let exited = match m.child.as_mut() {
                        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                        None => false,
                    };
                    if exited {
                        let now = Instant::now();
                        m.crash_timestamps
                            .retain(|&t| now.duration_since(t) <= Duration::from_secs(10));
                        m.crash_timestamps.push_back(now);

                        if m.crash_timestamps.len() >= 3 {
                            error!(
                                "CRITICAL: Module '{}' crashed 3 times within 10 seconds! Triggering automatic explorer.exe failsafe recovery!",
                                m.name
                            );
                            trigger_emergency_restore(true);
                            return;
                        }

                        warn!(
                            "Watchdog detected '{}' exit ({}/3 in 10s); respawning within <50ms...",
                            m.name,
                            m.crash_timestamps.len()
                        );
                        m.spawn_child();
                    }
                }
            })?;
    }

    info!("chickenwm-rooster listening on {}", PIPE_NAME_DAEMON);

    let cfg_ipc = Arc::clone(&shared_cfg);
    let mods_ipc = Arc::clone(&modules);
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C; shutting down ChickenWM daemon gracefully...");
        }
        res = serve_ipc(PIPE_NAME_DAEMON, move |cmd| {
            let cfg_ipc = Arc::clone(&cfg_ipc);
            let mods_ipc = Arc::clone(&mods_ipc);
            async move {
                match cmd {
                    // `exit` dispatcher: shut the whole shell down, not just the WM (the
                    // watchdog would otherwise respawn a WM that exited on its own).
                    IpcCommand::Dispatch(Action::Exit) => graceful_shutdown().await,
                    IpcCommand::RestartElevated => {
                        if chickenwm_core::elevation::is_elevated() {
                            return IpcResponse::Ok {
                                message: "ChickenWM already runs as administrator".to_string(),
                            };
                        }
                        // Shows the UAC prompt and blocks until it is answered.
                        let launched = tokio::task::spawn_blocking(|| {
                            let exe = std::env::current_exe()?;
                            let pid = std::process::id().to_string();
                            chickenwm_core::elevation::run_elevated(&exe, &["--takeover", &pid], false)
                        })
                        .await;
                        match launched {
                            Ok(Ok(_)) => {
                                // The elevated daemon waits for us to exit, then starts everything.
                                tokio::spawn(async {
                                    tokio::time::sleep(Duration::from_millis(150)).await;
                                    hand_over_shell()
                                });
                                IpcResponse::Ok {
                                    message: "Restarting ChickenWM as administrator".to_string(),
                                }
                            }
                            Ok(Err(e)) => IpcResponse::Error { message: format!("{e:#}") },
                            Err(e) => IpcResponse::Error { message: e.to_string() },
                        }
                    }
                    IpcCommand::RestoreExplorer => {
                        trigger_emergency_restore(true);
                        IpcResponse::Ok {
                            message: "Restored explorer.exe".to_string(),
                        }
                    }
                    IpcCommand::ReloadConfig => {
                        if let Ok((_, new_cfg)) = load_or_init_config() {
                            EXPLORER_KILLER_ENABLED
                                .store(new_cfg.misc.suppress_explorer, Ordering::SeqCst);
                            *cfg_ipc.lock() = new_cfg;
                        }
                        let _ = send_ipc_command_to(PIPE_NAME_WM, &IpcCommand::ReloadConfig).await;
                        let _ = send_ipc_command_to(PIPE_NAME_PAPER, &IpcCommand::ReloadConfig).await;
                        let _ = send_ipc_command_to(PIPE_NAME_BAR, &IpcCommand::ReloadConfig).await;
                        let _ =
                            send_ipc_command_to(PIPE_NAME_LAUNCHER, &IpcCommand::ReloadConfig).await;
                        IpcResponse::Ok {
                            message: "Reloaded chickenwm.conf and CSS across all modules".to_string(),
                        }
                    }
                    IpcCommand::Dispatch(ref action) => {
                        if let Ok(resp) = send_ipc_command_to(PIPE_NAME_WM, &cmd).await {
                            resp
                        } else {
                            IpcResponse::Ok {
                                message: format!("Queued dispatcher {:?}", action),
                            }
                        }
                    }
                    IpcCommand::SetFramerate(fps) => {
                        cfg_ipc.lock().misc.fps_limit = fps.unwrap_or(0);
                        let _ = send_ipc_command_to(PIPE_NAME_WM, &cmd).await;
                        let _ = send_ipc_command_to(PIPE_NAME_PAPER, &cmd).await;
                        IpcResponse::Ok {
                            message: format!(
                                "Framerate limit set to {}",
                                fps.map(|f| format!("{f} FPS"))
                                    .unwrap_or_else(|| "Monitor Native Refresh Rate".to_string())
                            ),
                        }
                    }
                    IpcCommand::QueryClients => match send_ipc_command_to(PIPE_NAME_WM, &cmd).await {
                        Ok(resp) => resp,
                        Err(_) => IpcResponse::Clients(Vec::new()),
                    },
                    IpcCommand::QueryMonitors => match send_ipc_command_to(PIPE_NAME_WM, &cmd).await {
                        Ok(resp) => resp,
                        Err(_) => IpcResponse::Monitors(enumerate_monitors()),
                    },
                    IpcCommand::QueryBarState => match send_ipc_command_to(PIPE_NAME_WM, &cmd).await {
                        Ok(resp) => resp,
                        Err(_) => IpcResponse::BarState(Default::default()),
                    },
                    IpcCommand::NotifyBarState(_) => {
                        let _ = send_ipc_command_to(PIPE_NAME_BAR, &cmd).await;
                        IpcResponse::Ok {
                            message: "Forwarded BarState to roobar".to_string(),
                        }
                    }
                    IpcCommand::QueryTray => IpcResponse::Tray(tray_icons()),
                    cmd @ (IpcCommand::Notify(_) | IpcCommand::DismissNotifications | IpcCommand::ToggleNotificationCenter | IpcCommand::QueryNotificationStatus) => {
                        match send_ipc_command_to(chickenwm_core::ipc::PIPE_NAME_NOTIFY, &cmd).await {
                            Ok(resp) => resp,
                            Err(e) => IpcResponse::Error {
                                message: format!("chickenwm-notify isn't running: {e}"),
                            },
                        }
                    }
                    IpcCommand::SummonLauncher => {
                        if send_ipc_command_to(PIPE_NAME_LAUNCHER, &IpcCommand::SummonLauncher)
                            .await
                            .is_err()
                        {
                            let mut exe = std::env::current_exe().unwrap_or_default();
                            exe.set_file_name("chickenwm-launcher.exe");
                            if exe.exists() {
                                let _ = Command::new(exe).creation_flags(CREATE_NO_WINDOW).spawn();
                            } else {
                                let _ = Command::new("chickenwm-launcher.exe")
                                    .creation_flags(CREATE_NO_WINDOW)
                                    .spawn();
                            }
                        }
                        IpcResponse::Ok {
                            message: "Summoned chickenwm-launcher".to_string(),
                        }
                    }
                    IpcCommand::SetShellConfig {
                        replace_winlogon_shell,
                        suppress_explorer,
                        register_files_handler: reg_files,
                    } => {
                        EXPLORER_KILLER_ENABLED.store(suppress_explorer, Ordering::SeqCst);
                        let _ = set_winlogon_shell(replace_winlogon_shell, None);
                        let _ = register_files_shell_handler(reg_files, None);
                        {
                            let mut c = cfg_ipc.lock();
                            c.misc.suppress_explorer = suppress_explorer;
                            c.misc.replace_winlogon_shell = replace_winlogon_shell;
                            c.misc.register_files_handler = reg_files;
                        }
                        IpcResponse::Ok {
                            message: "Updated shell replacement & explorer suppressor settings"
                                .to_string(),
                        }
                    }
                    IpcCommand::SetCompositorMode { dwm_hook_enabled } => {
                        let changed = {
                            let mut c = cfg_ipc.lock();
                            let changed = c.misc.dwm_hook_enabled != dwm_hook_enabled;
                            c.misc.dwm_hook_enabled = dwm_hook_enabled;
                            changed
                        };
                        if !dwm_hook_enabled {
                            eject_dwm_hooks();
                        } else if let Ok(mut exe) = std::env::current_exe() {
                            exe.set_file_name("chickenwm-dwm-hook.exe");
                            if exe.exists() {
                                let _ = Command::new(exe)
                                    .arg("--inject")
                                    .creation_flags(CREATE_NO_WINDOW)
                                    .status();
                            }
                        }
                        if changed {
                            // Switching engines live leaves the WM and wallpaper with state from
                            // the old one, so restart every module on a clean slate. They read
                            // the already-saved config on startup.
                            let mods = Arc::clone(&mods_ipc);
                            let _ = tokio::task::spawn_blocking(move || restart_modules(&mods)).await;
                        } else {
                            let _ = send_ipc_command_to(PIPE_NAME_WM, &cmd).await;
                        }
                        IpcResponse::Ok {
                            message: format!(
                                "Compositor mode set to {}",
                                if dwm_hook_enabled {
                                    "Experimental DWM PDB Hook"
                                } else {
                                    "Standard Win32/DirectComposition (Anti-Cheat Safe)"
                                }
                            ),
                        }
                    }
                    IpcCommand::SetWallpaper { .. } => {
                        match send_ipc_command_to(PIPE_NAME_PAPER, &cmd).await {
                            Ok(r) => r,
                            Err(_) => IpcResponse::Ok {
                                message: "Wallpaper directive saved".to_string(),
                            },
                        }
                    }
                    IpcCommand::QueryStatus => {
                        let mons = enumerate_monitors();
                        let client_count =
                            match send_ipc_command_to(PIPE_NAME_WM, &IpcCommand::QueryClients).await {
                                Ok(IpcResponse::Clients(c)) => c.len(),
                                _ => 0,
                            };
                        let running: Vec<String> = mods_ipc
                            .lock()
                            .iter()
                            .filter(|m| m.child.is_some())
                            .map(|m| m.name.to_string())
                            .collect();
                        let c = cfg_ipc.lock().clone();
                        IpcResponse::Status(DaemonStatus {
                            version: env!("CARGO_PKG_VERSION").to_string(),
                            uptime_secs: start_time.elapsed().as_secs(),
                            fps_limit: c.misc.fps_limit,
                            vfr_enabled: c.misc.vfr,
                            active_layout: c.general.layout.clone(),
                            explorer_suppressed: EXPLORER_KILLER_ENABLED.load(Ordering::Relaxed),
                            winlogon_shell_replaced: is_winlogon_shell_chickenwm(),
                            dwm_hook_enabled: c.misc.dwm_hook_enabled
                                || DWM_HOOK_ACTIVE.load(Ordering::Relaxed),
                            modules_running: running,
                            monitor_count: mons.len(),
                            client_count,
                        })
                    }
                }
            }
        }) => {
            if let Err(e) = res {
                error!("IPC server error: {e:#}");
            }
        }
    }

    graceful_shutdown().await
}

/// Set once a graceful shutdown starts, so the watchdog stops respawning modules we're closing.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Shuts the whole shell down cleanly: the WM restores its own hidden windows and exits, then the
/// failsafe routine stops the remaining modules, restores any windows still tagged as hidden,
/// brings explorer.exe back exactly once, and exits this process.
async fn graceful_shutdown() -> ! {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    EXPLORER_KILLER_ENABLED.store(false, Ordering::SeqCst);
    let _ = send_ipc_command_to(PIPE_NAME_WM, &IpcCommand::Dispatch(Action::Exit)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    trigger_emergency_restore(true);
    std::process::exit(0)
}
