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
unsafe extern "system" fn shell_tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COPYDATA {
        // Acknowledge Shell_NotifyIcon broadcasts with TRUE (1) so callers never block
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
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
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

    let (_doc, initial_cfg) = bootstrap_config_and_themes()?;
    let start_time = Instant::now();

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
                        cfg_ipc.lock().misc.dwm_hook_enabled = dwm_hook_enabled;
                        if !dwm_hook_enabled {
                            eject_dwm_hooks();
                        }
                        let _ = send_ipc_command_to(PIPE_NAME_WM, &cmd).await;
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
                error!("IPC server error: {e}");
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
