use serde::Serialize;
use std::{collections::HashMap, sync::{Mutex, OnceLock}};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State};

mod ahk;
mod config;
mod scripts;

use config::{Database, Game, Profile, Script, Settings};

const OVERLAY_PORT: u16 = 17823;
const TRAY_SHOW_ID: &str = "tray_show";
const TRAY_HIDE_ID: &str = "tray_hide";
const TRAY_QUIT_ID: &str = "tray_quit";
const GLOBAL_GAME_EXE: &str = "*";

#[cfg(target_os = "windows")]
struct WindowTransformState {
    hwnd: isize,
    style: i32,
    ex_style: i32,
    placement: winapi::um::winuser::WINDOWPLACEMENT,
    borderless: bool,
    stretched: bool,
}

#[cfg(target_os = "windows")]
enum WindowTransform {
    Borderless,
    Stretch,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct WindowClientBounds {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

pub struct AppState {
    pub db_path: std::path::PathBuf,
    pub scripts_path: std::path::PathBuf,
    /// The single always-on AutoHotkey process holding the Copilot remap and every armed
    /// profile's hotkeys and lifecycle events. Regenerated + relaunched
    /// whenever folders or profiles change.
    pub hotkeys_ahk: Mutex<ahk::AhkManager>,
    pub overlay_config: Mutex<config::OverlayConfig>,
    /// One Windows Job Object per profile (each kill-on-close) holding that profile's launched
    /// script processes. Everything in the job dies when the app process exits — even on a crash
    /// or hard-kill — and when the profile is disarmed, so scripts can never outlive the app.
    #[cfg(target_os = "windows")]
    pub script_jobs: Mutex<HashMap<String, JobObject>>,
    #[cfg(target_os = "windows")]
    window_transforms: Mutex<HashMap<String, WindowTransformState>>,
}

/// A Windows Job Object configured to kill every process in it when the job handle closes. The
/// app holds the only handle, so if the app dies (quit, crash, or `taskkill`) the OS closes the
/// handle and terminates all the job's script processes and their descendants.
#[cfg(target_os = "windows")]
pub struct JobObject(winapi::um::winnt::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for JobObject {}

#[cfg(target_os = "windows")]
impl JobObject {
    fn new() -> Self {
        use winapi::um::jobapi2::{CreateJobObjectW, SetInformationJobObject};
        use winapi::um::winnt::{
            JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
            if !handle.is_null() {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &mut info as *mut _ as *mut winapi::ctypes::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }
            JobObject(handle)
        }
    }

    fn assign(&self, process: std::os::windows::io::RawHandle) {
        if self.0.is_null() {
            return;
        }
        use winapi::um::jobapi2::AssignProcessToJobObject;
        unsafe {
            AssignProcessToJobObject(self.0, process as winapi::um::winnt::HANDLE);
        }
    }

    fn terminate(&self) {
        if self.0.is_null() {
            return;
        }
        use winapi::um::jobapi2::TerminateJobObject;
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for JobObject {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                winapi::um::handleapi::CloseHandle(self.0);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct OverlayEventPayload {
    event: String,
    hotkey_trigger: Option<String>,
    state_id: Option<String>,
}

#[cfg(target_os = "windows")]
static DEBUG_LOG_STATE: OnceLock<Mutex<HashMap<&'static str, String>>> = OnceLock::new();

#[cfg(target_os = "windows")]
static MOUSE_HOOK_EVENTS: OnceLock<std::sync::mpsc::SyncSender<(i32, i32)>> = OnceLock::new();

#[cfg(target_os = "windows")]
fn is_physical_right_button_down(code: i32, message: u32, flags: u32) -> bool {
    const LLMHF_INJECTED_FLAG: u32 = 0x0000_0001;
    code >= 0
        && message == winapi::um::winuser::WM_RBUTTONDOWN
        && flags & LLMHF_INJECTED_FLAG == 0
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: usize,
    lparam: isize,
) -> isize {
    use winapi::um::winuser::{CallNextHookEx, MSLLHOOKSTRUCT};
    if code >= 0 {
        let ms = &*(lparam as *const MSLLHOOKSTRUCT);
        if is_physical_right_button_down(code, wparam as u32, ms.flags) {
            if let Some(events) = MOUSE_HOOK_EVENTS.get() {
                // A low-level hook blocks Windows' input queue until it returns. Never call
                // WebView/Tauri APIs here; drop excess UI events instead of delaying input.
                let _ = events.try_send((ms.pt.x, ms.pt.y));
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

#[cfg(target_os = "windows")]
fn start_mouse_hook(handle: tauri::AppHandle) {
    use winapi::um::winuser::{SetWindowsHookExW, WH_MOUSE_LL};
    let (events, pending_events) = std::sync::mpsc::sync_channel(16);
    if MOUSE_HOOK_EVENTS.set(events).is_err() {
        return;
    }
    std::thread::spawn(move || {
        while let Ok((x, y)) = pending_events.recv() {
            if let Some(overlay) = handle.get_webview_window("overlay") {
                if overlay.is_visible().unwrap_or(false) {
                    let _ = overlay.emit("overlay-right-click", (x, y));
                }
            }
        }
    });
    std::thread::spawn(|| unsafe {
        use winapi::um::winuser::{GetMessageW, TranslateMessage, DispatchMessageW, MSG};
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), std::ptr::null_mut(), 0);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

#[cfg(all(test, target_os = "windows"))]
mod mouse_hook_tests {
    use super::is_physical_right_button_down;
    use winapi::um::winuser::WM_RBUTTONDOWN;

    #[test]
    fn only_physical_right_button_down_is_forwarded() {
        assert!(is_physical_right_button_down(0, WM_RBUTTONDOWN, 0));
        assert!(!is_physical_right_button_down(-1, WM_RBUTTONDOWN, 0));
        assert!(!is_physical_right_button_down(0, WM_RBUTTONDOWN, 1));
        assert!(!is_physical_right_button_down(0, WM_RBUTTONDOWN + 1, 0));
    }
}

#[cfg(target_os = "windows")]
fn debug_log_once(key: &'static str, message: String) {
    let state = DEBUG_LOG_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut state = state.lock().unwrap();
    if state.get(key).map(|prev| prev == &message).unwrap_or(false) {
        return;
    }
    state.insert(key, message.clone());
    eprintln!("{message}");
}

#[cfg(target_os = "windows")]
fn get_window_title(hwnd: winapi::shared::windef::HWND) -> String {
    use winapi::um::winuser::GetWindowTextW;

    unsafe {
        let mut buf = [0u16; 260];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

#[tauri::command]
fn get_database(state: State<AppState>) -> Result<Database, String> {
    config::load_db(&state.db_path)
}

#[tauri::command]
fn debug_overlay_log(message: String) {
    eprintln!("[debug][overlay_frontend] {message}");
}

#[tauri::command]
fn upsert_game(state: State<AppState>, game: Game) -> Result<Database, String> {
    let mut game = game;
    if game.id == config::GLOBAL_FOLDER_ID {
        game.exe = GLOBAL_GAME_EXE.to_string();
    }
    let mut db = config::load_db(&state.db_path)?;
    match db.games.iter_mut().find(|g| g.id == game.id) {
        Some(existing) => *existing = game,
        None => db.games.push(game),
    }
    config::save_db(&state.db_path, &db)?;
    sync_hotkeys(&state, &db);
    Ok(db)
}

#[tauri::command]
fn delete_game(state: State<AppState>, id: String) -> Result<Database, String> {
    // The Global folder is a permanent fixture and can never be deleted.
    if id == config::GLOBAL_FOLDER_ID {
        return Err("The Global folder can't be deleted.".to_string());
    }
    let mut db = config::load_db(&state.db_path)?;
    db.games.retain(|g| g.id != id);
    config::save_db(&state.db_path, &db)?;
    sync_hotkeys(&state, &db);
    Ok(db)
}

#[tauri::command]
fn upsert_profile(state: State<AppState>, game_id: String, profile: Profile) -> Result<Database, String> {
    let profile_id = profile.id.clone();
    let mut db = config::load_db(&state.db_path)?;
    let game = db.games.iter_mut().find(|g| g.id == game_id)
        .ok_or_else(|| "Folder not found".to_string())?;
    match game.profiles.iter_mut().find(|p| p.id == profile_id) {
        Some(existing) => *existing = profile,
        None => game.profiles.push(profile),
    }
    config::save_db(&state.db_path, &db)?;
    sync_hotkeys(&state, &db);
    Ok(db)
}

#[tauri::command]
fn delete_profile(state: State<AppState>, game_id: String, profile_id: String) -> Result<Database, String> {
    let mut db = config::load_db(&state.db_path)?;
    let game = db.games.iter_mut().find(|g| g.id == game_id)
        .ok_or_else(|| "Folder not found".to_string())?;
    game.profiles.retain(|p| p.id != profile_id);
    config::save_db(&state.db_path, &db)?;
    sync_hotkeys(&state, &db);
    Ok(db)
}

/// Arm or disarm a profile. Arming makes its hotkeys/events/scripts live automatically whenever its
/// folder's app is focused; multiple profiles can be armed at once.
#[tauri::command]
fn set_profile_armed(app: tauri::AppHandle, state: State<AppState>, profile_id: String, armed: bool) -> Result<Database, String> {
    let mut db = config::load_db(&state.db_path)?;
    let mut found = false;
    for game in &mut db.games {
        if let Some(profile) = game.profiles.iter_mut().find(|p| p.id == profile_id) {
            profile.armed = armed;
            found = true;
            break;
        }
    }
    if !found {
        return Err("Profile not found".to_string());
    }
    config::save_db(&state.db_path, &db)?;
    sync_hotkeys(&state, &db);
    if !armed {
        // Stop the profile's running scripts and, if it owns the visible overlay, drop it.
        kill_profile_scripts(&state, &profile_id);
        clear_overlay(&app);
        set_overlay_visible(&app, false);
    }
    Ok(db)
}

fn build_overlay_config(profiles: &[Profile], profile: &Profile) -> config::OverlayConfig {
    config::OverlayConfig {
        items: config::resolve_profile_overlay_items(profiles, profile)
            .into_iter()
            .cloned()
            .collect(),
        states: config::resolve_profile_states(profiles, profile)
            .into_iter()
            .cloned()
            .collect(),
        hotkeys: config::resolve_profile_hotkeys(profiles, profile)
            .into_iter()
            .map(|hotkey| config::OverlayHotkeyStateBinding {
                trigger: hotkey.trigger.clone(),
                state_id: hotkey.state_id.clone(),
            })
            .collect(),
    }
}

fn send_overlay(app: &tauri::AppHandle, profiles: &[Profile], profile: &Profile, enabled: bool) {
    let overlay_config = if enabled {
        build_overlay_config(profiles, profile)
    } else {
        config::OverlayConfig::default()
    };
    *app.state::<AppState>().overlay_config.lock().unwrap() = overlay_config.clone();
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.emit("overlay-config", &overlay_config);
    }
}

fn clear_overlay(app: &tauri::AppHandle) {
    let overlay_config = config::OverlayConfig::default();
    *app.state::<AppState>().overlay_config.lock().unwrap() = overlay_config.clone();
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.emit("overlay-config", &overlay_config);
    }
}

fn emit_overlay_event(app: &tauri::AppHandle, event: &str, hotkey_trigger: Option<String>, state_id: Option<String>) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.emit(
            "overlay-event",
            OverlayEventPayload {
                event: event.to_string(),
                hotkey_trigger,
                state_id,
            },
        );
    }
}

fn set_overlay_visible(app: &tauri::AppHandle, visible: bool) {
    if let Some(window) = app.get_webview_window("overlay") {
        let is_visible = window.is_visible().unwrap_or(false);
        if visible != is_visible {
            let _ = if visible { window.show() } else { window.hide() };
        }
    }
}

fn main_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("main").or_else(|| {
        app.webview_windows()
            .into_iter()
            .find_map(|(label, window)| (label != "overlay").then_some(window))
    })
}

fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("MacroToolbox")
    .inner_size(1100.0, 700.0)
    .min_inner_size(800.0, 500.0)
    .maximized(true)
    .visible(true)
    .build()
}

fn restore_main_window(window: &tauri::WebviewWindow) {
    // Preserve the maximized state across the restore dance below: SW_RESTORE and unminimize
    // both collapse a maximized window back to its normal size, so re-apply maximize after.
    // Default to true so a freshly-created (maximized) window still comes up maximized.
    let was_maximized = window.is_maximized().unwrap_or(true);
    let _ = window.set_skip_taskbar(false);
    let _ = window.show();

    #[cfg(target_os = "windows")]
    if let Ok(hwnd) = window.hwnd() {
        unsafe {
            use winapi::um::winuser::{SetForegroundWindow, ShowWindow, SW_SHOW};

            let hwnd = hwnd.0 as winapi::shared::windef::HWND;
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
    }

    let _ = window.unminimize();
    if was_maximized {
        let _ = window.maximize();
    }
    let _ = window.set_focus();
    // WebView2 doesn't reliably fire visibilitychange for native show/hide, so tell the
    // frontend explicitly that it just became visible (it re-checks for updates on this).
    let _ = window.emit("main-window-shown", ());
}

fn hide_window_for_tray(window: &tauri::WebviewWindow) {
    let _ = window.set_skip_taskbar(true);
    let _ = window.hide();
}

fn show_main_window(app: &tauri::AppHandle) {
    eprintln!("[tray] restore requested");
    if let Some(window) = main_window(app) {
        restore_main_window(&window);
        return;
    }

    let app = app.clone();
    let main_thread_app = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(window) = main_window(&main_thread_app) {
            restore_main_window(&window);
        } else if let Ok(window) = create_main_window(&main_thread_app) {
            restore_main_window(&window);
        } else {
            let labels = main_thread_app.webview_windows().keys().cloned().collect::<Vec<_>>();
            eprintln!("[tray] restore failed: no main window found, labels={labels:?}");
        }
    });
}

#[tauri::command]
fn reveal_main_window(app: tauri::AppHandle) {
    show_main_window(&app);
}

fn hide_main_window(app: &tauri::AppHandle) {
    eprintln!("[tray] hide requested");
    if let Some(window) = main_window(app) {
        hide_window_for_tray(&window);
    } else {
        let labels = app.webview_windows().keys().cloned().collect::<Vec<_>>();
        eprintln!("[tray] hide failed: no main window found, labels={labels:?}");
    }
}

fn quit_app(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    state.hotkeys_ahk.lock().unwrap().kill();
    kill_all_scripts(&state);
    app.exit(0);
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, TRAY_SHOW_ID, "Show MacroToolbox", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, TRAY_HIDE_ID, "Hide to Tray", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, TRAY_QUIT_ID, "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show_item, &hide_item, &separator, &quit_item])?;

    let mut tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("MacroToolbox")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            TRAY_SHOW_ID => show_main_window(app),
            TRAY_HIDE_ID => hide_main_window(app),
            TRAY_QUIT_ID => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => show_main_window(tray.app_handle()),
                _ => {}
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        tray = tray.icon(icon);
    }

    tray.build(app)?;
    Ok(())
}

#[tauri::command]
fn get_overlay_config(state: State<AppState>) -> config::OverlayConfig {
    state.overlay_config.lock().unwrap().clone()
}

fn is_global_game_exe(exe: &str) -> bool {
    exe.trim() == GLOBAL_GAME_EXE
}

/// Regenerate the combined AHK script from every armed profile and its folder target and
/// (re)launch the single always-on process. It stays alive even with no armed profiles because
/// it also owns the Copilot-key remap. Called after any profile/arming change. Note: an edit
/// the process relaunches — fine because edits happen from the manager UI, not in a game.
fn sync_hotkeys(state: &AppState, db: &Database) {
    let mut armed: Vec<ahk::ArmedProfile> = Vec::new();
    for game in &db.games {
        for profile in &game.profiles {
            if profile.armed {
                armed.push(ahk::ArmedProfile {
                    siblings: &game.profiles,
                    profile,
                    exe: &game.exe,
                });
            }
        }
    }

    let mut mgr = state.hotkeys_ahk.lock().unwrap();
    let script = ahk::generate_combined_script(&armed);
    let script_path = state.scripts_path.join("hotkeys.ahk");
    if std::fs::write(&script_path, &script).is_ok() {
        let _ = mgr.launch(&db.settings.ahk_exe, &script_path);
    }
}

/// Find the profile with the given id and the sibling slice it lives in (for overlay pushes).
fn find_profile<'a>(db: &'a Database, profile_id: &str) -> Option<(&'a [Profile], &'a Profile)> {
    db.games.iter().find_map(|g| {
        g.profiles.iter().find(|p| p.id == profile_id).map(|p| (g.profiles.as_slice(), p))
    })
}

/// Push the overlay config for a just-focused profile (by id), or clear the overlay when no
/// armed overlay profile is focused. Driven by the AHK `/focus` route.
fn push_overlay_for_focus(handle: &tauri::AppHandle, profile_id: Option<String>) {
    let state = handle.state::<AppState>();
    let db = match config::load_db(&state.db_path) {
        Ok(db) => db,
        Err(_) => return,
    };
    if let Some(id) = profile_id {
        if let Some((siblings, profile)) = find_profile(&db, &id) {
            if profile.kind == "overlay" && !profile.overlay_disabled {
                send_overlay(handle, siblings, profile, true);
                emit_overlay_event(handle, "profile_activated", None, None);
                return;
            }
        }
    }
    emit_overlay_event(handle, "profile_deactivated", None, None);
    clear_overlay(handle);
    set_overlay_visible(handle, false);
}

/// The foreground window's executable name (lowercased), used to resolve which armed profile
/// owns the overlay/viewport right now.
#[cfg(target_os = "windows")]
fn foreground_exe() -> Option<String> {
    let hwnd = unsafe { winapi::um::winuser::GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    get_window_process_name(hwnd)
}

#[cfg(target_os = "windows")]
fn get_global_overlay_origin() -> (i32, i32, i32, i32) {
    use winapi::um::winuser::GetDesktopWindow;

    let (physical_left, physical_top, physical_width, physical_height) = get_virtual_screen_bounds();
    let scale = get_window_scale_factor(unsafe { GetDesktopWindow() });
    (
        physical_to_logical(physical_left, scale),
        physical_to_logical(physical_top, scale),
        physical_to_logical(physical_width, scale),
        physical_to_logical(physical_height, scale),
    )
}

#[cfg(target_os = "windows")]
fn get_global_viewport() -> WindowClientBounds {
    let (left, top, width, height) = get_virtual_screen_bounds();
    WindowClientBounds { left, top, width, height }
}

/// The exe whose window the overlay currently belongs to: the foreground app if an armed,
/// overlay-enabled profile targets it, else "*" if an armed global overlay profile exists.
#[cfg(target_os = "windows")]
fn overlay_target_exe(db: &Database) -> Option<String> {
    if let Some(fg) = foreground_exe() {
        let matches = db.games.iter().any(|game| {
            game.exe.eq_ignore_ascii_case(&fg)
                && game.profiles.iter().any(|profile| {
                    profile.armed && profile.kind == "overlay" && !profile.overlay_disabled
                })
        });
        if matches {
            return Some(fg);
        }
    }
    let has_global = db.games.iter().any(|game| {
        is_global_game_exe(&game.exe)
            && game.profiles.iter().any(|profile| {
                profile.armed && profile.kind == "overlay" && !profile.overlay_disabled
            })
    });
    if has_global {
        return Some(GLOBAL_GAME_EXE.to_string());
    }
    None
}

#[tauri::command]
fn get_overlay_origin(state: State<AppState>) -> Result<(i32, i32, i32, i32), String> {
    #[cfg(target_os = "windows")]
    {
        let db = config::load_db(&state.db_path)?;
        let target = match overlay_target_exe(&db) {
            Some(exe) => exe,
            None => return Ok((0, 0, 0, 0)),
        };
        if is_global_game_exe(&target) {
            return Ok(get_global_overlay_origin());
        }
        let hwnd = find_window_by_exe(&target)
            .ok_or_else(|| format!("Window not found for '{target}'"))?;
        let scale = get_window_scale_factor(hwnd);
        let bounds = get_game_client_bounds(&target)?;
        let (virtual_left, virtual_top, _, _) = get_virtual_screen_bounds();
        let origin = (
            physical_to_logical(bounds.left - virtual_left, scale),
            physical_to_logical(bounds.top - virtual_top, scale),
            physical_to_logical(bounds.width, scale),
            physical_to_logical(bounds.height, scale),
        );
        Ok(origin)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = state;
        Ok((0, 0, 0, 0))
    }
}

#[cfg(target_os = "windows")]
fn get_active_game_viewport(handle: &tauri::AppHandle) -> Result<WindowClientBounds, String> {
    let state = handle.state::<AppState>();
    let db = config::load_db(&state.db_path)?;
    let target = overlay_target_exe(&db)
        .ok_or_else(|| "No overlay target".to_string())?;
    if is_global_game_exe(&target) {
        return Ok(get_global_viewport());
    }
    get_game_client_bounds(&target)
}

fn decode_query_value(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' if idx + 2 < bytes.len() => {
                let hex = &value[idx + 1..idx + 3];
                if let Ok(parsed) = u8::from_str_radix(hex, 16) {
                    out.push(parsed);
                    idx += 3;
                } else {
                    out.push(bytes[idx]);
                    idx += 1;
                }
            }
            byte => {
                out.push(byte);
                idx += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn get_query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(name, value)| (name == key).then(|| decode_query_value(value)))
}

fn start_overlay_listener(handle: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(format!("127.0.0.1:{OVERLAY_PORT}")).await {
            Ok(l) => { eprintln!("[overlay] TCP listener bound on port {OVERLAY_PORT}"); l }
            Err(e) => { eprintln!("[overlay] TCP bind FAILED: {e}"); return; }
        };
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let action = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let route = action.split('?').next().unwrap_or(action);
                    let (status, body) = if route == "/event" {
                        let event = get_query_param(action, "type")
                            .filter(|value| !value.is_empty())
                            .unwrap_or_else(|| "hotkey_triggered".to_string());
                        let hotkey_trigger = get_query_param(action, "hotkey_trigger")
                            .filter(|value| !value.is_empty());
                        let state_id = get_query_param(action, "state_id")
                            .filter(|value| !value.is_empty());
                        emit_overlay_event(&handle, &event, hotkey_trigger, state_id);
                        ("200 OK", Vec::new())
                    } else if route == "/script" {
                        if let Some(id) = get_query_param(action, "id").filter(|v| !v.is_empty()) {
                            run_script_by_id(&handle, &id);
                        }
                        ("200 OK", Vec::new())
                    } else if route == "/borderless" {
                        match get_query_param(action, "exe").filter(|value| !value.is_empty()) {
                            Some(exe) => match toggle_borderless(handle.state(), exe) {
                                Ok(enabled) => ("200 OK", enabled.to_string().into_bytes()),
                                Err(err) => ("404 Not Found", err.into_bytes()),
                            },
                            None => ("400 Bad Request", b"Missing executable".to_vec()),
                        }
                    } else if route == "/stretch" {
                        match get_query_param(action, "exe").filter(|value| !value.is_empty()) {
                            Some(exe) => match toggle_stretch(handle.state(), exe) {
                                Ok(enabled) => ("200 OK", enabled.to_string().into_bytes()),
                                Err(err) => ("404 Not Found", err.into_bytes()),
                            },
                            None => ("400 Bad Request", b"Missing executable".to_vec()),
                        }
                    } else if route == "/killprocess" {
                        match get_query_param(action, "exe").filter(|value| !value.is_empty()) {
                            Some(exe) => match kill_game(exe) {
                                Ok(()) => ("200 OK", Vec::new()),
                                Err(err) => ("404 Not Found", err.into_bytes()),
                            },
                            None => ("400 Bad Request", b"Missing executable".to_vec()),
                        }
                    } else if route == "/focus" {
                        // The AHK script reports which armed profile's app just became focused
                        // (or none). Push that profile's overlay config, or clear it.
                        push_overlay_for_focus(&handle, get_query_param(action, "id").filter(|v| !v.is_empty()));
                        ("200 OK", Vec::new())
                    } else {
                        match route {
                            "/show" => {
                                set_overlay_visible(&handle, true);
                                ("200 OK", Vec::new())
                            }
                            "/hide" => {
                                set_overlay_visible(&handle, false);
                                ("200 OK", Vec::new())
                            }
                            "/viewport" => {
                                #[cfg(target_os = "windows")]
                                {
                                    match get_active_game_viewport(&handle) {
                                        Ok(bounds) => (
                                            "200 OK",
                                            format!("{},{},{},{}", bounds.left, bounds.top, bounds.width, bounds.height).into_bytes(),
                                        ),
                                        Err(err) => ("404 Not Found", err.into_bytes()),
                                    }
                                }

                                #[cfg(not(target_os = "windows"))]
                                {
                                    ("501 Not Implemented", b"Not supported on this platform".to_vec())
                                }
                            }
                            _ => {
                                if let Some(window) = handle.get_webview_window("overlay") {
                                    let visible = window.is_visible().unwrap_or(false);
                                    set_overlay_visible(&handle, !visible);
                                }
                                ("200 OK", Vec::new())
                            }
                        }
                    };

                    let headers = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(headers.as_bytes()).await;
                    if !body.is_empty() {
                        let _ = stream.write_all(&body).await;
                    }
                });
            }
        }
    });
}

#[tauri::command]
fn get_ahk_status(state: State<AppState>) -> bool {
    state.hotkeys_ahk.lock().unwrap().is_running()
}

#[cfg(target_os = "windows")]
fn get_window_process_name(hwnd: winapi::shared::windef::HWND) -> Option<String> {
    use winapi::shared::minwindef::DWORD;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::psapi::GetModuleFileNameExW;
    use winapi::um::winnt::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    use winapi::um::winuser::GetWindowThreadProcessId;

    unsafe {
        let mut pid: DWORD = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }

        let proc = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if proc.is_null() {
            return None;
        }

        let mut buf = [0u16; 260];
        let len = GetModuleFileNameExW(proc, std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32);
        CloseHandle(proc);
        if len == 0 {
            return None;
        }

        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.split('\\').last().unwrap_or("").to_lowercase())
    }
}

/// Executable names of currently-open apps — visible, titled, top-level windows — for the
/// scope editor's dropdown. Deduped and sorted (BTreeSet).
#[cfg(target_os = "windows")]
#[tauri::command]
fn list_open_executables() -> Vec<String> {
    use std::collections::BTreeSet;
    use winapi::shared::minwindef::{BOOL, LPARAM, TRUE};
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{EnumWindows, GetWindow, GetWindowTextLengthW, IsWindowVisible, GW_OWNER};

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let set = &mut *(lparam as *mut BTreeSet<String>);
        // Only real, foreground-able app windows: visible, no owner, with a title bar text.
        if IsWindowVisible(hwnd) == 0 { return TRUE; }
        if !GetWindow(hwnd, GW_OWNER).is_null() { return TRUE; }
        if GetWindowTextLengthW(hwnd) == 0 { return TRUE; }
        if let Some(name) = get_window_process_name(hwnd) {
            if !name.is_empty() {
                set.insert(name);
            }
        }
        TRUE
    }

    let mut set: BTreeSet<String> = BTreeSet::new();
    unsafe { EnumWindows(Some(enum_cb), &mut set as *mut _ as LPARAM); }
    set.into_iter().collect()
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
fn list_open_executables() -> Vec<String> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn get_window_client_area(hwnd: winapi::shared::windef::HWND) -> i64 {
    use winapi::shared::windef::RECT;
    use winapi::um::winuser::GetClientRect;

    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &mut rect) == 0 {
            return 0;
        }

        let width = (rect.right - rect.left).max(0);
        let height = (rect.bottom - rect.top).max(0);
        i64::from(width) * i64::from(height)
    }
}

#[cfg(target_os = "windows")]
fn get_window_scale_factor(hwnd: winapi::shared::windef::HWND) -> f64 {
    use winapi::um::winuser::GetDpiForWindow;

    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 {
            1.0
        } else {
            dpi as f64 / 96.0
        }
    }
}

#[cfg(target_os = "windows")]
fn physical_to_logical(value: i32, scale: f64) -> i32 {
    ((value as f64) / scale).round() as i32
}

#[cfg(target_os = "windows")]
fn find_window_by_exe(exe: &str) -> Option<winapi::shared::windef::HWND> {
    use winapi::shared::minwindef::{BOOL, LPARAM, TRUE};
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{EnumWindows, GetForegroundWindow, IsWindowVisible};

    struct FindData {
        target: String,
        hwnd: HWND,
        area: i64,
    }

    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.is_null()
            && IsWindowVisible(foreground) != 0
            && get_window_process_name(foreground).as_deref() == Some(&exe.to_lowercase())
        {
            debug_log_once(
                "selected_window",
                format!(
                    "[debug][window] source=foreground exe={} hwnd=0x{:X} title={:?} area={}",
                    exe,
                    foreground as usize,
                    get_window_title(foreground),
                    get_window_client_area(foreground),
                ),
            );
            return Some(foreground);
        }
    }

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam as *mut FindData);
        if IsWindowVisible(hwnd) == 0 { return TRUE; }
        if get_window_process_name(hwnd).as_deref() == Some(&data.target) {
            let area = get_window_client_area(hwnd);
            if area > data.area {
                data.hwnd = hwnd;
                data.area = area;
            }
        }
        TRUE
    }

    let mut data = FindData {
        target: exe.to_lowercase(),
        hwnd: std::ptr::null_mut(),
        area: 0,
    };
    unsafe { EnumWindows(Some(enum_cb), &mut data as *mut _ as LPARAM); }
    if data.hwnd.is_null() {
        debug_log_once(
            "selected_window",
            format!("[debug][window] source=enum exe={} hwnd=<none>", exe),
        );
        None
    } else {
        debug_log_once(
            "selected_window",
            format!(
                "[debug][window] source=largest exe={} hwnd=0x{:X} title={:?} area={}",
                exe,
                data.hwnd as usize,
                get_window_title(data.hwnd),
                data.area,
            ),
        );
        Some(data.hwnd)
    }
}

#[cfg(target_os = "windows")]
fn focus_game_window(exe: &str) {
    if let Some(hwnd) = find_window_by_exe(exe) {
        unsafe { winapi::um::winuser::SetForegroundWindow(hwnd); }
    }
}

#[cfg(target_os = "windows")]
fn get_window_client_bounds(hwnd: winapi::shared::windef::HWND) -> Result<WindowClientBounds, String> {
    use winapi::shared::windef::{POINT, RECT};
    use winapi::um::winuser::{ClientToScreen, GetClientRect};

    unsafe {
        let mut rect: RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &mut rect) == 0 {
            return Err("Failed to read game client area".to_string());
        }

        let mut top_left = POINT { x: rect.left, y: rect.top };
        if ClientToScreen(hwnd, &mut top_left) == 0 {
            return Err("Failed to convert game client origin".to_string());
        }

        let bounds = WindowClientBounds {
            left: top_left.x,
            top: top_left.y,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        };
        debug_log_once(
            "client_bounds",
            format!(
                "[debug][client_bounds] hwnd=0x{:X} title={:?} left={} top={} width={} height={}",
                hwnd as usize,
                get_window_title(hwnd),
                bounds.left,
                bounds.top,
                bounds.width,
                bounds.height,
            ),
        );
        Ok(bounds)
    }
}

#[cfg(target_os = "windows")]
fn get_game_client_bounds(exe: &str) -> Result<WindowClientBounds, String> {
    let hwnd = find_window_by_exe(exe)
        .ok_or_else(|| format!("Game window not found for '{exe}'"))?;
    let bounds = get_window_client_bounds(hwnd)?;
    debug_log_once(
        "game_bounds",
        format!(
            "[debug][game_bounds] exe={} hwnd=0x{:X} left={} top={} width={} height={}",
            exe, hwnd as usize, bounds.left, bounds.top, bounds.width, bounds.height,
        ),
    );
    Ok(bounds)
}

#[cfg(target_os = "windows")]
fn get_virtual_screen_bounds() -> (i32, i32, i32, i32) {
    use winapi::um::winuser::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

#[tauri::command]
fn kill_game(exe: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use winapi::shared::minwindef::{FALSE};
        use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
        use winapi::um::processthreadsapi::{OpenProcess, TerminateProcess};
        use winapi::um::tlhelp32::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
        use winapi::um::winnt::PROCESS_TERMINATE;

        let exe_lower = exe.to_lowercase();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE { return Err("Failed to snapshot processes".to_string()); }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut killed = false;
            if Process32FirstW(snap, &mut entry) != FALSE {
                loop {
                    let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                    if name == exe_lower {
                        let proc = OpenProcess(PROCESS_TERMINATE, FALSE, entry.th32ProcessID);
                        if !proc.is_null() {
                            TerminateProcess(proc, 1);
                            CloseHandle(proc);
                            killed = true;
                        }
                    }
                    if Process32NextW(snap, &mut entry) == FALSE { break; }
                }
            }
            CloseHandle(snap);
            if killed { Ok(()) } else { Err(format!("Process '{}' not found", exe)) }
        }
    }
    #[cfg(not(target_os = "windows"))]
    Err("Not supported on this platform".to_string())
}

#[tauri::command]
fn toggle_borderless(state: State<AppState>, exe: String) -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    return toggle_window_transform(&state, &exe, WindowTransform::Borderless);
    #[cfg(not(target_os = "windows"))]
    Err("Not supported on this platform".to_string())
}

#[tauri::command]
fn toggle_stretch(state: State<AppState>, exe: String) -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    return toggle_window_transform(&state, &exe, WindowTransform::Stretch);
    #[cfg(not(target_os = "windows"))]
    Err("Not supported on this platform".to_string())
}

#[cfg(target_os = "windows")]
fn toggle_window_transform(
    state: &AppState,
    exe: &str,
    transform: WindowTransform,
) -> Result<bool, String> {
    let key = exe.to_lowercase();
    let hwnd = find_window_by_exe(exe)
        .ok_or_else(|| format!("Game window not found for '{exe}'"))?;
    let hwnd_value = hwnd as isize;
    let mut transforms = state.window_transforms.lock().unwrap();

    if transforms.get(&key).is_some_and(|saved| saved.hwnd != hwnd_value) {
        transforms.remove(&key);
    }

    let newly_captured = !transforms.contains_key(&key);
    if newly_captured {
        transforms.insert(key.clone(), capture_window_transform(hwnd)?);
    }

    let (enabled, remove_state, error) = {
        let saved = transforms.get_mut(&key).unwrap();
        let previous = (saved.borderless, saved.stretched);
        let enabled = match transform {
            WindowTransform::Borderless => {
                saved.borderless = !saved.borderless;
                saved.borderless
            }
            WindowTransform::Stretch => {
                saved.stretched = !saved.stretched;
                saved.stretched
            }
        };

        match unsafe { apply_window_transform(hwnd, saved) } {
            Ok(()) => (enabled, !saved.borderless && !saved.stretched, None),
            Err(error) => {
                saved.borderless = previous.0;
                saved.stretched = previous.1;
                let _ = unsafe { apply_window_transform(hwnd, saved) };
                (enabled, newly_captured, Some(error))
            }
        }
    };

    if remove_state {
        transforms.remove(&key);
    }
    match error {
        Some(error) => Err(error),
        None => Ok(enabled),
    }
}

#[cfg(target_os = "windows")]
fn capture_window_transform(
    hwnd: winapi::shared::windef::HWND,
) -> Result<WindowTransformState, String> {
    use winapi::um::winuser::{GetWindowLongW, GetWindowPlacement, GWL_EXSTYLE, GWL_STYLE, WINDOWPLACEMENT};

    unsafe {
        let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut placement) == 0 {
            return Err("Failed to read current window placement".to_string());
        }
        Ok(WindowTransformState {
            hwnd: hwnd as isize,
            style: GetWindowLongW(hwnd, GWL_STYLE),
            ex_style: GetWindowLongW(hwnd, GWL_EXSTYLE),
            placement,
            borderless: false,
            stretched: false,
        })
    }
}

#[cfg(target_os = "windows")]
unsafe fn apply_window_transform(
    hwnd: winapi::shared::windef::HWND,
    saved: &WindowTransformState,
) -> Result<(), String> {
    use winapi::um::winuser::*;

    let style = if saved.borderless {
        saved.style & !(WS_OVERLAPPEDWINDOW as i32)
    } else {
        saved.style
    };
    let ex_style = if saved.borderless {
        saved.ex_style
            & !((WS_EX_WINDOWEDGE | WS_EX_CLIENTEDGE | WS_EX_DLGMODALFRAME | WS_EX_STATICEDGE) as i32)
    } else {
        saved.ex_style
    };
    SetWindowLongW(hwnd, GWL_STYLE, style);
    SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style);

    if saved.stretched {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut monitor_info: MONITORINFO = std::mem::zeroed();
        monitor_info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut monitor_info) == 0 {
            return Err("Failed to read monitor bounds".to_string());
        }

        let bounds = monitor_info.rcMonitor;
        let mut outer = winapi::shared::windef::RECT {
            left: 0,
            top: 0,
            right: bounds.right - bounds.left,
            bottom: bounds.bottom - bounds.top,
        };
        if AdjustWindowRectEx(
            &mut outer,
            style as u32,
            (!GetMenu(hwnd).is_null()) as i32,
            ex_style as u32,
        ) == 0
        {
            return Err("Failed to calculate the stretched window frame".to_string());
        }

        // Keep the window frame, but bypass its normal WM_WINDOWPOSCHANGING size constraints so
        // fixed-size games cannot replace the requested monitor bounds with their preferred size.
        if SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            bounds.left + outer.left,
            bounds.top + outer.top,
            outer.right - outer.left,
            outer.bottom - outer.top,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSENDCHANGING,
        ) == 0 {
            return Err("Failed to stretch the window".to_string());
        }

        let mut actual: winapi::shared::windef::RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &mut actual) == 0 {
            return Err("Failed to verify the stretched client bounds".to_string());
        }
        let mut client_origin = winapi::shared::windef::POINT { x: 0, y: 0 };
        if ClientToScreen(hwnd, &mut client_origin) == 0 {
            return Err("Failed to verify the stretched client position".to_string());
        }
        let requested_width = bounds.right - bounds.left;
        let requested_height = bounds.bottom - bounds.top;
        let actual_width = actual.right - actual.left;
        let actual_height = actual.bottom - actual.top;
        if client_origin.x != bounds.left
            || client_origin.y != bounds.top
            || actual_width != requested_width
            || actual_height != requested_height
        {
            return Err(format!(
                "The target window refused the stretch (requested {requested_width}x{requested_height}, got {actual_width}x{actual_height})",
            ));
        }
        return Ok(());
    }

    let mut placement = saved.placement;
    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    if SetWindowPlacement(hwnd, &placement) == 0 {
        return Err("Failed to restore previous window placement".to_string());
    }
    if SetWindowPos(
        hwnd,
        std::ptr::null_mut(),
        0,
        0,
        0,
        0,
        SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE,
    ) == 0 {
        return Err("Failed to update the window frame".to_string());
    }
    Ok(())
}

#[tauri::command]
async fn pick_coordinate(window: tauri::WebviewWindow, exe: String) -> Result<(f64, f64), String> {
    #[cfg(target_os = "windows")]
    if !is_global_game_exe(&exe) {
        focus_game_window(&exe);
    }

    window.minimize().map_err(|e| e.to_string())?;

    #[cfg(target_os = "windows")]
    let exe_for_pick = exe.clone();

    let result = tokio::task::spawn_blocking(move || -> Result<(f64, f64), String> {
        use std::time::{Duration, Instant};
        std::thread::sleep(Duration::from_millis(400));

        #[cfg(target_os = "windows")]
        unsafe {
            use winapi::um::winuser::{GetAsyncKeyState, GetCursorPos};
            use winapi::shared::windef::POINT;

            while (GetAsyncKeyState(0x01) as u16) & 0x8000 != 0 {
                std::thread::sleep(Duration::from_millis(15));
            }

            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if Instant::now() > deadline {
                    return Err("Timed out waiting for click".to_string());
                }
                std::thread::sleep(Duration::from_millis(15));
                if (GetAsyncKeyState(0x01) as u16) & 0x8000 != 0 {
                    let mut pt = POINT { x: 0, y: 0 };
                    GetCursorPos(&mut pt);
                    let bounds = if is_global_game_exe(&exe_for_pick) {
                        get_global_viewport()
                    } else {
                        get_game_client_bounds(&exe_for_pick)?
                    };
                    while (GetAsyncKeyState(0x01) as u16) & 0x8000 != 0 {
                        std::thread::sleep(Duration::from_millis(15));
                    }
                    let rel_x = pt.x - bounds.left;
                    let rel_y = pt.y - bounds.top;
                    let x = if bounds.width <= 0 {
                        0.0
                    } else {
                        (((rel_x as f64 / bounds.width as f64) * 1000.0).round() / 10.0)
                            .clamp(0.0, 100.0)
                    };
                    let y = if bounds.height <= 0 {
                        0.0
                    } else {
                        (((rel_y as f64 / bounds.height as f64) * 1000.0).round() / 10.0)
                            .clamp(0.0, 100.0)
                    };
                    eprintln!(
                        "[debug][pick] exe={} click_screen=({}, {}) bounds=({}, {}, {}, {}) rel=({}, {}) percent=({:.1}, {:.1})",
                        exe_for_pick,
                        pt.x,
                        pt.y,
                        bounds.left,
                        bounds.top,
                        bounds.width,
                        bounds.height,
                        rel_x,
                        rel_y,
                        x,
                        y,
                    );
                    return Ok((x, y));
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        Err("Not supported on this platform".to_string())
    })
    .await
    .map_err(|e| e.to_string())?;

    let _ = window.unminimize();
    let _ = window.set_focus();
    result
}

#[tauri::command]
fn write_text_file(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

#[tauri::command]
fn read_text_file(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn read_image_as_data_url(path: String) -> Result<String, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mime = match std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif"          => "image/gif",
        "webp"         => "image/webp",
        "ico"          => "image/x-icon",
        _              => "image/png",
    };
    Ok(format!("data:{};base64,{}", mime, STANDARD.encode(&bytes)))
}

#[tauri::command]
fn set_overlay_passthrough(app: tauri::AppHandle, passthrough: bool) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.set_ignore_cursor_events(passthrough);
    }
}

#[tauri::command]
fn toggle_overlay(app: tauri::AppHandle) {
    eprintln!("[overlay] toggle_overlay command called");
    match app.get_webview_window("overlay") {
        None => eprintln!("[overlay] window not found in toggle_overlay"),
        Some(w) => {
            let visible = w.is_visible().unwrap_or(false);
            eprintln!("[overlay] visible={visible}, toggling");
            let result = if visible { w.hide() } else { w.show() };
            eprintln!("[overlay] toggle result: {result:?}");
        }
    }
}

#[tauri::command]
fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Register or unregister the app to launch at login. Re-applied on every launch so the
/// registration tracks the current executable across updates and moves.
///
/// On Windows the app runs elevated (`requireAdministrator`, see build.rs). Windows refuses to
/// auto-launch an elevated app from the HKCU `Run` key at logon — it can't show a UAC prompt
/// during logon, so the entry is silently skipped and the app never starts. The autostart
/// plugin only knows the Run key, so on Windows we register a Scheduled Task with highest
/// privileges triggered at logon instead — the only supported way to start an admin app at
/// logon without a prompt. (Creating the task itself needs elevation, which we already have.)
/// Other platforms use the plugin as normal.
fn sync_autostart(app: &tauri::AppHandle, enabled: bool) {
    #[cfg(not(target_os = "windows"))]
    {
        use tauri_plugin_autostart::ManagerExt;
        let manager = app.autolaunch();
        let _ = if enabled { manager.enable() } else { manager.disable() };
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const TASK_NAME: &str = "MacroToolbox";

        // Drop the dead Run-key entry and Startup-folder shortcut earlier versions created:
        // neither can launch an elevated app at logon, yet they show as "enabled" in Task
        // Manager's Startup tab while never actually starting.
        let app_name = app
            .config()
            .product_name
            .clone()
            .unwrap_or_else(|| app.package_info().name.clone());
        {
            use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
            use winreg::RegKey;
            if let Ok(run) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                KEY_SET_VALUE,
            ) {
                let _ = run.delete_value(&app_name);
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            let _ = std::fs::remove_file(format!(
                r"{appdata}\Microsoft\Windows\Start Menu\Programs\Startup\Hotkey Manager.lnk"
            ));
        }

        if enabled {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new("schtasks")
                    .args([
                        "/Create",
                        "/F",
                        "/TN",
                        TASK_NAME,
                        "/TR",
                        &format!("\"{}\"", exe.display()),
                        "/SC",
                        "ONLOGON",
                        "/RL",
                        "HIGHEST",
                    ])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
        } else {
            let _ = std::process::Command::new("schtasks")
                .args(["/Delete", "/F", "/TN", TASK_NAME])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
}

#[tauri::command]
fn save_settings(app: tauri::AppHandle, state: State<AppState>, settings: Settings) -> Result<Database, String> {
    let mut db = config::load_db(&state.db_path)?;
    db.settings = settings;
    config::save_db(&state.db_path, &db)?;
    sync_autostart(&app, db.settings.launch_on_startup);
    Ok(db)
}

/// Download the given release installer and launch it, then quit so the installer can
/// replace the running executable. Downloading in the backend avoids the browser's
/// download-redirect handling and any webview CORS restrictions.
#[tauri::command]
async fn download_and_install_update(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("MacroToolbox")
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Could not start the download: {e}"))?;
    let bytes = client
        .get(&url)
        .send()
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|e| format!("Download failed: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;

    let mut installer_path = std::env::temp_dir();
    installer_path.push("MacroToolbox-update-setup.exe");
    std::fs::write(&installer_path, &bytes)
        .map_err(|e| format!("Could not save the installer: {e}"))?;

    // Launch the installer in passive mode so an update just applies instead of prompting:
    // /P passive (progress bar, no delete-data prompt or shortcut checkbox), /R relaunch the
    // app when done, /NS don't (re)create shortcuts. It keeps running after we exit, so we
    // then quit to unlock the running executable and let it be replaced.
    std::process::Command::new(&installer_path)
        .args(["/P", "/R", "/NS"])
        .spawn()
        .map_err(|e| format!("Could not launch the installer: {e}"))?;

    quit_app(&app);
    Ok(())
}

/// Look up a saved script by id across every scope and run it. Used by the AHK `/script`
/// route (a hotkey fired) — failures are logged, not surfaced, since there is no UI in the loop.
/// Spawn a script and place it in its owning profile's kill-on-close Job Object, so it — and
/// anything it spawns — is bound to the app's lifetime: it dies when the app exits (even a crash
/// or hard-kill) or when the profile is disarmed, and can never outlive the app.
fn spawn_tracked_script(state: &AppState, settings: &Settings, script: &Script, profile_id: &str) -> Result<(), String> {
    let ahk_exe = state
        .hotkeys_ahk
        .lock()
        .unwrap()
        .executable_path(&settings.ahk_exe);
    let child = scripts::run_script(
        &settings.python_exe,
        &ahk_exe,
        script,
        &state.scripts_path,
    )?;
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawHandle;
        let mut jobs = state.script_jobs.lock().unwrap();
        let job = jobs.entry(profile_id.to_string()).or_insert_with(JobObject::new);
        job.assign(child.as_raw_handle());
    }
    // The job now owns the process; drop our handle (on Windows it stays in the job, on other
    // platforms this is the previous fire-and-forget behavior).
    let _ = (child, profile_id);
    Ok(())
}

/// Terminate every script process launched by one profile (used when it is disarmed).
fn kill_profile_scripts(state: &AppState, profile_id: &str) {
    #[cfg(target_os = "windows")]
    {
        if let Some(job) = state.script_jobs.lock().unwrap().remove(profile_id) {
            job.terminate(); // dropping the job also closes its handle (kill-on-close)
        }
    }
    let _ = (state, profile_id);
}

/// Terminate every script process we launched (used when the app quits).
fn kill_all_scripts(state: &AppState) {
    #[cfg(target_os = "windows")]
    {
        let jobs: Vec<JobObject> = state.script_jobs.lock().unwrap().drain().map(|(_, j)| j).collect();
        for job in jobs {
            job.terminate();
        }
    }
    let _ = state;
}

fn run_script_by_id(handle: &tauri::AppHandle, id: &str) {
    let state = handle.state::<AppState>();
    let db = match config::load_db(&state.db_path) {
        Ok(db) => db,
        Err(_) => return,
    };
    for game in &db.games {
        for profile in &game.profiles {
            if let Some(script) = profile.scripts.iter().find(|s| s.id == id) {
                if let Err(e) = spawn_tracked_script(&state, &db.settings, script, &profile.id) {
                    eprintln!("[scripts] {e}");
                }
                return;
            }
        }
    }
}

fn start_watcher(handle: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_tick = std::time::SystemTime::now();
        let mut first_tick = true;
        loop {
            // Establish the process baseline immediately. Sleeping before the first observation
            // could miss an app launched during those first three seconds: it would appear as
            // already running instead of as a not-running -> running transition.
            if first_tick {
                first_tick = false;
            } else {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }

            // A wall-clock jump across a 3s sleep means the machine was suspended. Windows may
            // have dropped the AHK hooks, so relaunch the hotkeys script fresh below.
            let now = std::time::SystemTime::now();
            let resumed = now
                .duration_since(last_tick)
                .map(|gap| gap > std::time::Duration::from_secs(30))
                .unwrap_or(false);
            last_tick = now;

            let state = handle.state::<AppState>();

            let db = match config::load_db(&state.db_path) {
                Ok(db) => db,
                Err(_) => continue,
            };

            // Keep the single hotkey/Copilot script alive and relaunch it after suspend. #HotIf
            // handles profile focus gating; the Copilot remap remains active with no profiles.
            if resumed {
                state.hotkeys_ahk.lock().unwrap().kill();
            }
            let running = state.hotkeys_ahk.lock().unwrap().is_running();
            if !running {
                sync_hotkeys(&state, &db);
            }

            // Re-assert that the app and its AutoHotkey process stay out of Windows Efficiency
            // Mode: the one-shot opt-out at launch doesn't hold once the app drops to the tray, so
            // Windows re-throttles the tree and the hotkey hook goes dead after idle. Re-applying
            // it every tick keeps the tree exempt for as long as the app runs.
            #[cfg(target_os = "windows")]
            {
                ahk::disable_power_throttling(unsafe {
                    winapi::um::processthreadsapi::GetCurrentProcess()
                });
                state.hotkeys_ahk.lock().unwrap().keep_responsive();
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be the first plugin: a second launch hands off to the running instance,
        // which just brings its window to the front instead of opening another copy.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    let close_to_tray = config::load_db(&state.db_path)
                        .map(|db| db.settings.close_to_tray)
                        .unwrap_or(false);

                    if close_to_tray {
                        api.prevent_close();
                        hide_main_window(&app);
                    }
                }
                tauri::WindowEvent::Destroyed if window.label() == "main" => {
                    let app = window.app_handle();
                    let state = app.state::<AppState>();
                    let close_to_tray = config::load_db(&state.db_path)
                        .map(|db| db.settings.close_to_tray)
                        .unwrap_or(false);

                    if !close_to_tray {
                        state.hotkeys_ahk.lock().unwrap().kill();
                        kill_all_scripts(&state);
                    }
                }
                _ => {}
            }
        })
        .setup(|app| {
            // Keep the app (and thus its child AutoHotkey process tree) out of Windows
            // Efficiency Mode while it lives in the tray, so hotkeys don't go dead after idle.
            #[cfg(target_os = "windows")]
            unsafe {
                ahk::disable_power_throttling(winapi::um::processthreadsapi::GetCurrentProcess());
            }

            let data_dir = app.path().app_data_dir()
                .expect("failed to resolve app data dir");
            let scripts_dir = data_dir.join("scripts");
            std::fs::create_dir_all(&scripts_dir)
                .expect("failed to create scripts dir");

            let db_path = data_dir.join("db.json");
            if !db_path.exists() {
                config::save_db(&db_path, &Database::default())
                    .expect("failed to write initial db");
            }
            let startup_settings = config::load_db(&db_path)
                .map(|db| db.settings)
                .unwrap_or_default();
            let resource_dir = app.path().resource_dir().ok();

            app.manage(AppState {
                db_path: db_path.clone(),
                scripts_path: scripts_dir,
                hotkeys_ahk: Mutex::new(ahk::AhkManager::new(resource_dir)),
                overlay_config: Mutex::new(config::OverlayConfig::default()),
                #[cfg(target_os = "windows")]
                script_jobs: Mutex::new(HashMap::new()),
                #[cfg(target_os = "windows")]
                window_transforms: Mutex::new(HashMap::new()),
            });

            // Launch the combined Copilot remap and profile hotkeys script at startup.
            {
                let state = app.state::<AppState>();
                if let Ok(db) = config::load_db(&state.db_path) {
                    sync_hotkeys(&state, &db);
                }
            }

            // The tray/taskbar is often not ready yet when the app is auto-launched at
            // Windows login, which makes tray creation fail. Retry instead of letting the
            // error bubble up and exit the app — with open-to-tray the tray is the only UI,
            // so a hard failure here looks exactly like "the app didn't start".
            {
                let mut built = false;
                for attempt in 0..10u32 {
                    match build_tray(app) {
                        Ok(()) => { built = true; break; }
                        Err(e) => {
                            eprintln!("[tray] build attempt {} failed: {e}", attempt + 1);
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                }
                if !built {
                    eprintln!("[tray] could not create the tray icon; continuing without it");
                }
            }
            // Re-apply the login registration on every launch so the registered path
            // tracks the current executable across updates and moves.
            sync_autostart(app.handle(), startup_settings.launch_on_startup);
            if startup_settings.open_to_tray {
                hide_main_window(app.handle());
            } else {
                show_main_window(app.handle());
                if let Some(window) = main_window(app.handle()) {
                    let _ = window.maximize();
                }
            }

            #[cfg(target_os = "windows")]
            let (overlay_left, overlay_top, overlay_width, overlay_height) = {
                use winapi::um::winuser::GetDesktopWindow;

                let (physical_left, physical_top, physical_width, physical_height) = get_virtual_screen_bounds();
                let scale = get_window_scale_factor(unsafe { GetDesktopWindow() });
                (
                    physical_to_logical(physical_left, scale),
                    physical_to_logical(physical_top, scale),
                    physical_to_logical(physical_width, scale),
                    physical_to_logical(physical_height, scale),
                )
            };

            #[cfg(not(target_os = "windows"))]
            let (overlay_left, overlay_top, overlay_width, overlay_height) = (0, 0, 3840, 2160);

            eprintln!(
                "[debug][overlay_window] left={} top={} width={} height={}",
                overlay_left, overlay_top, overlay_width, overlay_height,
            );

            // Non-fatal: a transparent webview window can fail to build in the early login
            // environment; the app (tray + hotkeys) must still start without the overlay.
            match tauri::WebviewWindowBuilder::new(
                app,
                "overlay",
                tauri::WebviewUrl::App("index.html?window=overlay".into()),
            )
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .inner_size(overlay_width as f64, overlay_height as f64)
            .position(overlay_left as f64, overlay_top as f64)
            .visible(false)
            .build()
            {
                Ok(overlay) => { let _ = overlay.set_ignore_cursor_events(true); }
                Err(e) => eprintln!("[overlay] failed to create overlay window: {e}"),
            }

            // Pre-populate overlay items from the first armed overlay profile; the AHK /focus
            // path swaps to the right one once an app is focused.
            if let Ok(db) = config::load_db(&app.state::<AppState>().db_path) {
                if let Some(game) = db.games.iter().find(|g| g.profiles.iter().any(|p| p.armed && p.kind == "overlay" && !p.overlay_disabled)) {
                    if let Some(profile) = game.profiles.iter().find(|p| p.armed && p.kind == "overlay" && !p.overlay_disabled) {
                        send_overlay(app.handle(), &game.profiles, profile, true);
                    }
                }
            }

            start_overlay_listener(app.handle().clone());
            start_watcher(app.handle().clone());
            #[cfg(target_os = "windows")]
            start_mouse_hook(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_database,
            debug_overlay_log,
            get_overlay_config,
            get_overlay_origin,
            toggle_overlay,
            set_overlay_passthrough,
            pick_coordinate,
            kill_game,
            list_open_executables,
            toggle_borderless,
            toggle_stretch,
            write_text_file,
            read_text_file,
            read_image_as_data_url,
            upsert_game,
            delete_game,
            upsert_profile,
            delete_profile,
            set_profile_armed,
            get_ahk_status,
            save_settings,
            get_app_version,
            reveal_main_window,
            download_and_install_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
