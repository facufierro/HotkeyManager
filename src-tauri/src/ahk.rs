use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use crate::config::{self, Profile};

const GLOBAL_GAME_EXE: &str = "*";

/// A kill-on-close Windows Job Object holding the manager's AutoHotkey process. The manager
/// (owned by the app for its whole lifetime) holds the only handle, so if the app process dies
/// for any reason — clean quit, crash, or taskkill — the OS closes the handle and terminates the
/// AutoHotkey process. This prevents an orphaned AutoHotkey64.exe from locking the bundled
/// binary and breaking the updater. Mirrors the job used for launched user scripts.
#[cfg(target_os = "windows")]
struct AhkJob(winapi::um::winnt::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for AhkJob {}

#[cfg(target_os = "windows")]
impl AhkJob {
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
            AhkJob(handle)
        }
    }

    fn assign(&self, child: &Child) {
        use std::os::windows::io::AsRawHandle;
        use winapi::um::jobapi2::AssignProcessToJobObject;
        if self.0.is_null() {
            return;
        }
        unsafe {
            AssignProcessToJobObject(self.0, child.as_raw_handle() as winapi::um::winnt::HANDLE);
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for AhkJob {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { winapi::um::handleapi::CloseHandle(self.0); }
        }
    }
}

/// Keep the AutoHotkey process responsive while the app sits idle in the tray. Windows drops
/// idle background processes into EcoQoS power throttling, which adds latency to the low-level
/// keyboard hook that dispatches every hotkey — so the first keypress after an idle stretch
/// lands late (feels like a "cold start") before the throttled process spins back up. Opting
/// out of throttling and nudging the priority above idle keeps hotkey dispatch prompt.
#[cfg(target_os = "windows")]
fn keep_process_responsive(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use winapi::um::processthreadsapi::SetPriorityClass;

    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000; // not in the enabled winapi features

    let handle = child.as_raw_handle() as winapi::um::winnt::HANDLE;
    if handle.is_null() {
        return;
    }
    disable_power_throttling(handle);
    unsafe { SetPriorityClass(handle, ABOVE_NORMAL_PRIORITY_CLASS); }
}

/// Opt a process out of Windows EcoQoS "Efficiency Mode" power throttling. Idle in the tray with
/// no visible window, Windows throttles the app — and Efficiency Mode extends across the process
/// tree to the child AutoHotkey process that dispatches hotkeys. A throttled process's low-level
/// keyboard hook times out and Windows drops it, leaving hotkeys dead until the process is
/// scheduled again (a standalone AHK script, not being a throttled background child, never hits
/// this). Applied to BOTH the AHK child and the app process itself, so the tree stays un-throttled.
#[cfg(target_os = "windows")]
pub fn disable_power_throttling(process: winapi::um::winnt::HANDLE) {
    use winapi::um::processthreadsapi::SetProcessInformation;

    // Not in winapi 0.3.9: the ProcessPowerThrottling info class (4) and its state struct.
    // Declared locally to match <winnt.h>/<processthreadsapi.h>.
    const PROCESS_POWER_THROTTLING: u32 = 4;
    const PROCESS_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;
    const PROCESS_POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;

    #[repr(C)]
    struct ProcessPowerThrottlingState {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }

    if process.is_null() {
        return;
    }
    unsafe {
        // ControlMask selects the policy we manage; StateMask 0 = leave EcoQoS explicitly OFF.
        let mut state = ProcessPowerThrottlingState {
            version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            control_mask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: 0,
        };
        SetProcessInformation(
            process,
            PROCESS_POWER_THROTTLING,
            &mut state as *mut _ as *mut winapi::ctypes::c_void,
            std::mem::size_of::<ProcessPowerThrottlingState>() as u32,
        );
    }
}

/// Default precise key-down duration (ms) for a repeat tap when the behavior doesn't
/// specify one. Kept small so a game that acts on the key per frame registers ~one press.
const DEFAULT_REPEAT_HOLD_MS: u64 = 6;

pub struct AhkManager {
    process: Option<Child>,
    bundled_ahk_exe: Option<PathBuf>,
    #[cfg(target_os = "windows")]
    job: AhkJob,
}

impl AhkManager {
    pub fn new(resource_dir: Option<PathBuf>) -> Self {
        Self {
            process: None,
            bundled_ahk_exe: resource_dir
                .map(|dir| dir.join("resources").join("autohotkey").join("AutoHotkey64.exe"))
                .filter(|path| path.exists()),
            #[cfg(target_os = "windows")]
            job: AhkJob::new(),
        }
    }

    pub fn launch(&mut self, ahk_exe: &str, script_path: &Path) -> Result<(), String> {
        // Kill old process in the background so we don't block waiting for it to exit.
        // AHK's #SingleInstance Force also causes the old instance to exit on its own.
        if let Some(mut old) = self.process.take() {
            std::thread::spawn(move || {
                let _ = old.kill();
                let _ = old.wait();
            });
        }

        // AutoHotkeyUX.exe is a launcher that spawns a child and exits — use the v2
        // interpreter directly so we can track the process.
        let exe = resolve_ahk_exe(ahk_exe, self.bundled_ahk_exe.as_deref());

        let child = Command::new(&exe)
            .arg(script_path)
            .spawn()
            .map_err(|e| format!("Failed to launch '{exe}': {e}. Check the bundled AutoHotkey file or the path in Settings."))?;
        // Tie the process to a kill-on-close job so it can never outlive the app — a crash or
        // hard-kill that skips kill() would otherwise orphan this AutoHotkey process, and a
        // lingering AutoHotkey64.exe keeps the bundled binary locked so the updater's file
        // replacement fails ("Unable to uninstall!").
        #[cfg(target_os = "windows")]
        self.job.assign(&child);
        #[cfg(target_os = "windows")]
        keep_process_responsive(&child);
        self.process = Some(child);
        Ok(())
    }

    /// Resolve the configured interpreter exactly as manager-owned scripts do, including the
    /// bundled AutoHotkey v2 executable when no custom path is configured.
    pub fn executable_path(&self, configured: &str) -> String {
        resolve_ahk_exe(configured, self.bundled_ahk_exe.as_deref())
    }

    pub fn kill(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn is_running(&mut self) -> bool {
        if let Some(child) = self.process.as_mut() {
            matches!(child.try_wait(), Ok(None))
        } else {
            false
        }
    }

    /// Re-apply the responsiveness settings (un-throttle + priority) to the running process.
    /// The one-shot opt-out at launch doesn't hold: Windows re-applies Efficiency Mode to a
    /// background app's whole process tree each time it drops to the tray, re-throttling this
    /// AutoHotkey process. Called periodically so it stays exempt for as long as it runs.
    pub fn keep_responsive(&mut self) {
        #[cfg(target_os = "windows")]
        if let Some(child) = self.process.as_ref() {
            keep_process_responsive(child);
        }
    }
}

fn resolve_ahk_exe(configured: &str, bundled: Option<&Path>) -> String {
    if configured.is_empty() {
        if let Some(path) = bundled {
            return path.to_string_lossy().into_owned();
        }
        return "AutoHotkey.exe".to_string();
    }
    // If user pointed to AutoHotkeyUX.exe, find the real v2 interpreter next to it
    let path = std::path::Path::new(configured);
    if path.file_name().and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case("AutoHotkeyUX.exe"))
        .unwrap_or(false)
    {
        // Try sibling directories: ../v2/AutoHotkey64.exe
        let candidates = ["AutoHotkey64.exe", "AutoHotkey.exe"];
        let parent = path.parent().and_then(|p| p.parent()); // UX/../ = _App/
        if let Some(base) = parent {
            for sub in &["v2", ""] {
                for name in &candidates {
                    let candidate = if sub.is_empty() {
                        base.join(name)
                    } else {
                        base.join(sub).join(name)
                    };
                    if candidate.exists() {
                        return candidate.to_string_lossy().into_owned();
                    }
                }
            }
        }
    }
    configured.to_string()
}

/// One armed profile plus the sibling slice it needs to resolve `parent_id` inheritance.
pub struct ArmedProfile<'a> {
    pub siblings: &'a [Profile],
    pub profile: &'a Profile,
}

/// Emit one armed profile's `#HotIf` block(s). Hotkeys/scripts sit under
/// `WinActive("ahk_exe <exe>") && enabled["<id>"]` (or just `enabled["<id>"]` for exe "*") so
/// they only fire while that app is focused; toggle keys sit under a focus-only gate so a
/// disabled profile can still be re-enabled. `used_keys` is keyed by exe: the same key may be
/// bound for different apps, but within one app the first armed profile wins it (a duplicate
/// `X::` label under an identical #HotIf would fail to load and kill every hotkey).
fn generate_profile_block(
    ap: &ArmedProfile,
    used_keys: &mut HashMap<String, HashSet<String>>,
    repeat_ups: &mut HashSet<String>,
    repeat_up_lines: &mut String,
) -> String {
    let p = ap.profile;
    let exe = p.exe.trim();
    let global_game = exe == GLOBAL_GAME_EXE;
    let id = escape_ahk_string(&p.id);
    let exe_esc = escape_ahk_string(exe);
    let resolved = config::resolve_profile_hotkeys(ap.siblings, p);
    let keyset = used_keys.entry(exe.to_string()).or_default();
    let mut lines = String::new();

    for hk in resolved {
        let ahk_key = trigger_to_key(&hk.trigger);
        if ahk_key.is_empty() { continue; }
        if !keyset.insert(ahk_key.clone()) { continue; }
        let trigger = escape_ahk_string(&hk.trigger);
        // The overlay only reacts to a hotkey_triggered ping for a binding that carries a
        // state_id (it drives overlay state flags/timers). For every other hotkey the ping is
        // a wasted blocking localhost round-trip on the hotkey's own thread — its first-call
        // COM init and the idle-throttled backend are what make an early keypress feel like a
        // cold start — so emit it only when a state_id makes it meaningful.
        let notify = if hk.state_id.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
            format!("    SendAppEvent(\"hotkey_triggered\", \"{trigger}\")\n")
        } else {
            String::new()
        };

        // A behavior that is exactly one hold(...) becomes a true held remap: the key stays
        // down while the hotkey is held, released by a paired wildcard key-up hotkey.
        if let (Some(hold_arg), Some(up_key)) = (parse_pure_hold(&hk.behavior), up_hotkey(&hk.trigger)) {
            let keys = escape_ahk_string(&hold_arg);
            lines.push_str(&format!(
                "{ahk_key}:: {{\n    HoldKeyDown(\"{keys}\")\n{notify}}}\n{up_key}:: HoldKeyUp(\"{keys}\")\n"
            ));
            continue;
        }

        // A behavior that is exactly one repeat(...) becomes a hold-to-repeat. The loop
        // outlives the #HotIf press gate, so it re-checks focus (exe) and this profile's
        // enabled flag (id) itself each tick.
        if let Some((repeat_keys, interval, hold)) = parse_pure_repeat(&hk.behavior) {
            let poll_key = trigger_bare_key(&hk.trigger);
            if !poll_key.is_empty() {
                let keys = escape_ahk_string(&repeat_keys);
                let poll_key = escape_ahk_string(&poll_key);
                let repeat_exe = if global_game { String::new() } else { exe_esc.clone() };
                lines.push_str(&format!(
                    "{ahk_key}:: {{\n    repeatDown[\"{poll_key}\"] := true\n{notify}    RepeatHold(\"{keys}\", {interval}, \"{poll_key}\", \"{repeat_exe}\", {hold}, \"{id}\")\n}}\n"
                ));
                // One global key-up hotkey per physical key clears the repeat flag. `~` lets the
                // native key-up through so normal typing of the key still works; keyed by the bare
                // key because there is only one physical key regardless of how many triggers use it.
                if repeat_ups.insert(poll_key.clone()) {
                    repeat_up_lines.push_str(&format!(
                        "~*{poll_key} up:: repeatDown[\"{poll_key}\"] := false\n"
                    ));
                }
                continue;
            }
        }

        let behavior = escape_ahk_string(&hk.behavior);
        // Run the behavior first, then notify the overlay (when a state_id makes the ping
        // meaningful): the ping is a blocking localhost request, so doing it after keeps a
        // busy backend from delaying the output.
        lines.push_str(&format!(
            "{ahk_key}:: {{\n    ExecuteBehavior(\"{behavior}\")\n{notify}}}\n"
        ));
    }

    for script in &p.scripts {
        if !script.enabled || script.trigger != "hotkey" { continue; }
        let ahk_key = trigger_to_key(&script.hotkey);
        if ahk_key.is_empty() || !keyset.insert(ahk_key.clone()) { continue; }
        let sid = escape_ahk_string(&script.id);
        lines.push_str(&format!("{ahk_key}:: RunScript(\"{sid}\")\n"));
    }

    // Toggle keys only bind when explicitly set; the overlay-toggle is skipped when it equals
    // the hotkeys-toggle. Both flip THIS profile's enabled flag, gated by focus only so a
    // disabled profile can be re-enabled from its own app.
    let toggle_h = p.toggle_hotkeys_key.as_deref()
        .and_then(|k| { let k = trigger_to_key(k); if k.is_empty() { None } else { Some(k) } });
    let toggle_o = p.toggle_overlay_key.as_deref()
        .and_then(|k| { let k = trigger_to_key(k); if k.is_empty() { None } else { Some(k) } })
        .filter(|k| Some(k) != toggle_h.as_ref());
    let mut toggle_lines = String::new();
    if let Some(k) = &toggle_h { toggle_lines.push_str(&format!("{k}:: ToggleEnabled(\"{id}\")\n")); }
    if let Some(k) = &toggle_o { toggle_lines.push_str(&format!("{k}:: ToggleEnabled(\"{id}\")\n")); }

    let mut out = String::new();
    if !lines.is_empty() {
        if global_game {
            out.push_str(&format!("#HotIf enabled[\"{id}\"]\n{lines}#HotIf\n"));
        } else {
            out.push_str(&format!("#HotIf WinActive(\"ahk_exe {exe_esc}\") && enabled[\"{id}\"]\n{lines}#HotIf\n"));
        }
    }
    if !toggle_lines.is_empty() {
        if global_game {
            out.push_str(&format!("#HotIf\n{toggle_lines}#HotIf\n"));
        } else {
            out.push_str(&format!("#HotIf WinActive(\"ahk_exe {exe_esc}\")\n{toggle_lines}#HotIf\n"));
        }
    }
    out
}

/// Build ONE always-on AHK script for every armed profile, each gated to its own app.
pub fn generate_combined_script(armed: &[ArmedProfile]) -> String {
    // Specific-exe blocks first, "*" last, so an app's binding takes precedence over a global
    // one for the same key.
    let mut ordered: Vec<&ArmedProfile> = armed.iter().collect();
    ordered.sort_by_key(|ap| ap.profile.exe.trim() == GLOBAL_GAME_EXE);

    let mut used_keys: HashMap<String, HashSet<String>> = HashMap::new();
    let mut repeat_ups: HashSet<String> = HashSet::new();
    let mut repeat_up_lines = String::new();
    let mut blocks = String::new();
    let mut enabled_init = String::new();
    let mut overlay_init = String::new();

    for ap in &ordered {
        let p = ap.profile;
        let exe = p.exe.trim();
        if exe.is_empty() { continue; }
        let id = escape_ahk_string(&p.id);
        enabled_init.push_str(&format!("enabled[\"{id}\"] := true\n"));
        blocks.push_str(&generate_profile_block(ap, &mut used_keys, &mut repeat_ups, &mut repeat_up_lines));
        // Only overlay-type profiles drive the overlay window; a hotkeys/scripts profile never
        // shows it (otherwise an armed hotkeys profile would pop an empty overlay).
        if p.kind == "overlay" && !p.overlay_disabled {
            let exe_esc = if exe == GLOBAL_GAME_EXE { "*".to_string() } else { escape_ahk_string(exe) };
            overlay_init.push_str(&format!(
                "overlayProfiles.Push(Map(\"id\", \"{id}\", \"exe\", \"{exe_esc}\"))\n"
            ));
        }
    }

    let header = format!(
        r###"#Requires AutoHotkey v2.0
#SingleInstance Force

; A held hotkey (a hold/repeat remap, or a held accent key) auto-repeats dozens of times a
; second, which would trip AHK's runaway-hotkey guard (default 70 per 2s) and pop a warning
; that kills every hotkey. Raise it well above any real hold rate.
A_MaxHotkeysPerInterval := 1000

CoordMode "Pixel", "Screen"
CoordMode "Mouse", "Screen"
SetTitleMatchMode 2

global enabled := Map()
global overlayVisible := false
global overlayProfiles := []
global lastFocusId := ""
global repeatDown := Map()
global copilotState := "idle"
global copilotShiftSuppressed := false
global copilotCtrlHeld := false
global copilotCtrlReleasePending := false
{enabled_init}{overlay_init}OnExit HideOverlayOnExit
OnExit ReleaseCopilotHeld

SendOverlayCommand(path) {{
    try {{
        xhr := ComObject("WinHttp.WinHttpRequest.5.1")
        xhr.Open("GET", "http://127.0.0.1:17823/" path, false)
        ; Bound the wait so a momentarily busy backend can't stall the keypress that triggered it.
        xhr.SetTimeouts(500, 500, 500, 500)
        xhr.Send()
    }} catch Error {{
    }}
}}

UriEncode(str) {{
    result := ""
    loop parse, str
    {{
        code := Ord(A_LoopField)
        if ((code >= 0x30 && code <= 0x39) || (code >= 0x41 && code <= 0x5A) || (code >= 0x61 && code <= 0x7A) || code = 0x2D || code = 0x2E || code = 0x5F || code = 0x7E)
            result .= Chr(code)
        else
            result .= Format("%{{:02X}}", code)
    }}
    return result
}}

SendAppEvent(eventType, hotkeyTrigger := "", stateId := "") {{
    path := "event?type=" UriEncode(eventType)
    if (hotkeyTrigger != "")
        path .= "&hotkey_trigger=" UriEncode(hotkeyTrigger)
    if (stateId != "")
        path .= "&state_id=" UriEncode(stateId)
    SendOverlayCommand(path)
}}

RunScript(id) {{
    SendOverlayCommand("script?id=" UriEncode(id))
}}

; The overlay follows the focused app: find the first armed overlay profile whose app is
; focused (specific apps first, then "*"), tell the backend which profile's config to push,
; and show/hide from that profile's enabled flag.
SyncOverlay(*) {{
    global enabled, overlayProfiles, overlayVisible, lastFocusId
    focusId := ""
    shouldShow := false
    for p in overlayProfiles {{
        if (p["exe"] = "*" || WinActive("ahk_exe " p["exe"])) {{
            focusId := p["id"]
            shouldShow := enabled.Has(focusId) && enabled[focusId]
            break
        }}
    }}
    if (focusId != lastFocusId) {{
        lastFocusId := focusId
        SendOverlayCommand(focusId = "" ? "focus" : "focus?id=" UriEncode(focusId))
    }}
    if (shouldShow != overlayVisible) {{
        overlayVisible := shouldShow
        SendOverlayCommand(shouldShow ? "show" : "hide")
    }}
}}

ToggleEnabled(id) {{
    global enabled
    if enabled.Has(id)
        enabled[id] := !enabled[id]
}}

HideOverlayOnExit(*) {{
    global overlayVisible
    if !overlayVisible
        return
    overlayVisible := false
    SendOverlayCommand("hide")
}}

SetTimer SyncOverlay, 200
SyncOverlay()

; Windows silently removes low-level hooks when the system sleeps or the hook
; times out while the process is throttled in the background (idle in the tray),
; leaving hotkeys dead even though this script is still running. Install both hooks
; up front and reinstall them after a wake or a detected process stall.
InstallKeybdHook true, true
InstallMouseHook true, true
global lastHealthTick := A_TickCount

ReinstallHooks() {{
    InstallKeybdHook true, true
    InstallMouseHook true, true
}}

OnMessage 0x218, OnPowerBroadcast  ; WM_POWERBROADCAST

OnPowerBroadcast(wParam, lParam, msg, hwnd) {{
    ; PBT_APMRESUMESUSPEND (0x7) / PBT_APMRESUMEAUTOMATIC (0x12): just woke up.
    if (wParam = 0x7 || wParam = 0x12)
        ReinstallHooks()
}}

CheckHookHealth(*) {{
    global lastHealthTick
    ; This 1-second timer firing far late means the process was throttled or suspended (idle in
    ; the tray under Efficiency Mode, or a real sleep). Windows drops the low-level hooks in that
    ; state, so reinstall them the instant we run again — this is what makes hotkeys recover
    ; promptly after the process resumes.
    gap := A_TickCount - lastHealthTick
    lastHealthTick := A_TickCount
    if (gap > 3000) {{
        ReinstallHooks()
    }}
}}
SetTimer CheckHookHealth, 1000

; A previous force-terminated instance can leave synthesized modifiers logically down.
; Clear only keys which are not physically held, so startup cannot cancel real input.
ReleaseStaleCopilotModifiers()

; --- TEMP DIAGNOSTIC: liveness heartbeat, logged next to this script (ahk-heartbeat.log).
; Consecutive lines whose A_TickCount differs by far more than 1000 mean Windows froze or
; throttled this process while it was idle in the tray; a "started" line partway through the
; file means the process was killed and relaunched. Lets us tell a frozen process apart from an
; alive-but-hook-dead one. Remove once the tray-idle latency is pinned down.
global heartbeatLog := A_ScriptDir "\ahk-heartbeat.log"
try FileAppend "=== started A_Now=" A_Now " tick=" A_TickCount "`n", heartbeatLog
Heartbeat() {{
    global heartbeatLog
    try FileAppend A_TickCount " " A_Now "`n", heartbeatLog
}}
SetTimer Heartbeat, 1000

; The Copilot key arrives as LWin+LShift+F23 (SC06E). Keep this remap in the same
; process as every other hotkey so there is one keyboard hook and one modifier-state owner.
CopilotKeyPhysicallyDown() {{
    ; Poll Windows directly instead of trusting hook state: this still detects release if the
    ; low-level hook misses the F23 key-up while Windows is resuming or under load.
    return (DllCall("GetAsyncKeyState", "Int", 0x86, "Short") & 0x8000) != 0
}}

ReleaseStaleCopilotModifiers() {{
    if !GetKeyState("RCtrl", "P") && !CopilotKeyPhysicallyDown()
        SendInput "{{Blind}}{{RCtrl up}}"
    if !GetKeyState("LWin", "P")
        SendInput "{{Blind}}{{LWin up}}"
    if !GetKeyState("LShift", "P")
        SendInput "{{Blind}}{{LShift up}}"
}}

ReleaseCopilotCtrl() {{
    global copilotCtrlHeld, copilotCtrlReleasePending
    if !copilotCtrlHeld
        return
    copilotCtrlHeld := false
    ; Do not cancel a real Right Ctrl which happens to be held at the same time. Keep the
    ; poll alive and clear our synthetic DownR immediately after the physical key is released.
    if GetKeyState("RCtrl", "P") {{
        copilotCtrlReleasePending := true
    }} else {{
        copilotCtrlReleasePending := false
        SetTimer CheckCopilotRelease, 0
        SendInput "{{Blind}}{{RCtrl up}}"
    }}
}}

ReleaseCopilotHeld(*) {{
    global copilotCtrlReleasePending
    ReleaseCopilotCtrl()
    if copilotCtrlReleasePending && !GetKeyState("RCtrl", "P")
        SendInput "{{Blind}}{{RCtrl up}}"
    copilotCtrlReleasePending := false
    if !GetKeyState("LWin", "P")
        SendInput "{{Blind}}{{LWin up}}"
    if !GetKeyState("LShift", "P")
        SendInput "{{Blind}}{{LShift up}}"
}}

CheckCopilotRelease(*) {{
    global copilotCtrlHeld, copilotCtrlReleasePending
    if copilotCtrlHeld && !CopilotKeyPhysicallyDown()
        ReleaseCopilotCtrl()
    if copilotCtrlReleasePending && !GetKeyState("RCtrl", "P") {{
        copilotCtrlReleasePending := false
        SetTimer CheckCopilotRelease, 0
        SendInput "{{Blind}}{{RCtrl up}}"
    }}
}}

PassCopilotKeys() {{
    global copilotState, copilotShiftSuppressed
    if copilotState = "waiting" {{
        copilotState := "lwin_passed"
        if copilotShiftSuppressed {{
            copilotShiftSuppressed := false
            SendInput "{{LWin down}}{{LShift down}}"
        }} else {{
            SendInput "{{LWin down}}"
        }}
    }}
}}

$*LWin::{{
    global copilotState
    copilotState := "waiting"
    SetTimer(PassCopilotKeys, -30)
}}

$*LWin up::{{
    global copilotState, copilotShiftSuppressed
    if copilotState = "waiting" {{
        SetTimer(PassCopilotKeys, 0)
        copilotState := "idle"
        if copilotShiftSuppressed {{
            copilotShiftSuppressed := false
            SendInput "{{LWin down}}{{LShift down}}{{LWin up}}"
        }} else {{
            SendInput "{{LWin down}}{{LWin up}}"
        }}
    }} else if copilotState = "lwin_passed" {{
        copilotState := "idle"
        SendInput "{{LWin up}}"
    }}
}}

$*LShift::{{
    global copilotState, copilotShiftSuppressed
    if copilotState = "waiting" {{
        copilotShiftSuppressed := true
    }} else {{
        copilotShiftSuppressed := false
        SendInput "{{LShift down}}"
    }}
}}

$*LShift up::{{
    global copilotShiftSuppressed
    if copilotShiftSuppressed {{
        copilotShiftSuppressed := false
    }} else {{
        SendInput "{{LShift up}}"
    }}
}}

$*SC06E::{{
    global copilotState, copilotShiftSuppressed, copilotCtrlHeld, copilotCtrlReleasePending
    SetTimer(PassCopilotKeys, 0)
    copilotState := "copilot"
    copilotShiftSuppressed := false
    if copilotCtrlHeld
        return
    copilotCtrlHeld := true
    copilotCtrlReleasePending := false
    SendInput "{{Blind}}{{RCtrl downR}}"
    SetTimer CheckCopilotRelease, 10
}}

$*SC06E up::{{
    global copilotState
    if copilotState = "copilot"
        copilotState := "idle"
    ReleaseCopilotCtrl()
}}

{repeat_up_lines}
{blocks}
"###
    );

    header + BEHAVIOR_ENGINE
}

/// Escape a string for safe embedding inside an AHK v2 double-quoted string literal.
/// The backtick is AHK's escape character, so it must be handled first; an unescaped
/// backtick (or quote / newline) in user text would otherwise terminate the string
/// early and shift every following brace, producing an unparseable script.
fn escape_ahk_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '`'  => out.push_str("``"),
            '"'  => out.push_str("`\""),
            '\n' => out.push_str("`n"),
            '\r' => out.push_str("`r"),
            '\t' => out.push_str("`t"),
            _    => out.push(ch),
        }
    }
    out
}

/// If `behavior` is exactly a single `hold(...)` action, return its inner key string.
/// A multi-step behavior (containing `;`) is not treated as a remap.
fn parse_pure_hold(behavior: &str) -> Option<String> {
    let b = behavior.trim();
    let lower = b.to_lowercase();
    if !lower.starts_with("hold(") || !b.ends_with(')') {
        return None;
    }
    let inner = b[5..b.len() - 1].trim();
    if inner.is_empty() || inner.contains(';') {
        return None;
    }
    Some(inner.to_string())
}

/// If `behavior` is exactly a single `repeat(<keys>, <interval_ms>)` action, return the
/// key string (which may carry modifiers) and the interval. A multi-step behavior
/// (containing `;`) is not treated as a hold-to-repeat.
/// Parses `repeat(<keys>, <interval>[, <hold>])`. Returns (keys, interval_ms, hold_ms).
/// `hold` (the precise key-down duration of each tap) is optional and defaults to
/// DEFAULT_REPEAT_HOLD_MS. The key part never contains a comma (modifiers are
/// space-separated), so splitting on commas is unambiguous.
fn parse_pure_repeat(behavior: &str) -> Option<(String, u64, u64)> {
    let b = behavior.trim();
    let lower = b.to_lowercase();
    if !lower.starts_with("repeat(") || !b.ends_with(')') {
        return None;
    }
    let inner = b["repeat(".len()..b.len() - 1].trim();
    if inner.contains(';') {
        return None;
    }
    let parts: Vec<&str> = inner.splitn(3, ',').collect();
    if parts.len() < 2 {
        return None;
    }
    let keys = parts[0].trim();
    let interval = parts[1].trim().parse::<u64>().ok()?;
    let hold = match parts.get(2) {
        Some(h) => h.trim().parse::<u64>().ok()?,
        None => DEFAULT_REPEAT_HOLD_MS,
    };
    if keys.is_empty() || interval == 0 {
        return None;
    }
    Some((keys.to_string(), interval, hold))
}

/// The wildcard key-up hotkey that releases a held remap, e.g. trigger "shift win f23"
/// -> "*F23 up". `*` makes it fire on the key release regardless of modifier state.
/// Returns None when the trigger resolves to no key.
fn up_hotkey(trigger: &str) -> Option<String> {
    let key = trigger_bare_key(trigger);
    if key.is_empty() { None } else { Some(format!("*{key} up")) }
}

/// The bare key of a trigger with modifiers stripped, AHK-cased: "shift win f23" ->
/// "F23"; a modifier-only trigger like "win" -> "LWin".
fn trigger_bare_key(trigger: &str) -> String {
    let trigger = trigger.trim().to_lowercase();
    let mut key = String::new();
    let mut modifier_key = String::new();
    for part in trigger.split_whitespace() {
        match part {
            "ctrl"   => modifier_key = "Control".to_string(),
            "lctrl"  => modifier_key = "LControl".to_string(),
            "rctrl"  => modifier_key = "RControl".to_string(),
            "shift"  => modifier_key = "Shift".to_string(),
            "lshift" => modifier_key = "LShift".to_string(),
            "rshift" => modifier_key = "RShift".to_string(),
            "alt"    => modifier_key = "Alt".to_string(),
            "lalt"   => modifier_key = "LAlt".to_string(),
            "ralt"   => modifier_key = "RAlt".to_string(),
            "win"    => modifier_key = "LWin".to_string(),
            "lwin"   => modifier_key = "LWin".to_string(),
            "rwin"   => modifier_key = "RWin".to_string(),
            k        => key = k.to_string(),
        }
    }
    if key.is_empty() {
        return modifier_key;
    }
    if let Some(rest) = key.strip_prefix('f') {
        if rest.parse::<u32>().is_ok() {
            key = format!("F{rest}");
        }
    }
    if let Some(sc) = layout_scancode(&key).or_else(|| us_scancode(&key).map(String::from)) {
        key = sc;
    }
    key
}

/// Scancode of the key that produces this character on the user's active keyboard
/// layout, so a trigger binds the key the user means by that character — on QWERTZ
/// "z" is a different physical key than on QWERTY. Letters/digits only: that is the
/// set the recorder stores as characters. Punctuation is stored by physical position
/// (e.code), so resolving it as a character would bind the wrong key (on German, "["
/// lives on AltGr+8 — not the Ü key the user actually pressed). None when the
/// character doesn't exist on the layout (e.g. Cyrillic); us_scancode is the fallback.
fn layout_scancode(key: &str) -> Option<String> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphanumeric() {
        return None;
    }
    use winapi::um::winuser::{GetKeyboardLayout, MapVirtualKeyExW, VkKeyScanExW, MAPVK_VK_TO_VSC};
    unsafe {
        let hkl = GetKeyboardLayout(0);
        let vk = VkKeyScanExW(c as u16, hkl);
        if vk == -1 {
            return None;
        }
        let sc = MapVirtualKeyExW((vk & 0xFF) as u32, MAPVK_VK_TO_VSC, hkl);
        if sc == 0 {
            return None;
        }
        Some(format!("SC{sc:03X}"))
    }
}

/// Scancode names (US-QWERTY positions) for keys whose single-character AHK names
/// resolve through the ACTIVE keyboard layout. Fallback when the character isn't on
/// the active layout — a name like "q" doesn't exist on e.g. a Cyrillic layout, so
/// the hotkey would never fire there; the US position is the best guess.
fn us_scancode(key: &str) -> Option<&'static str> {
    Some(match key {
        "a" => "SC01E", "b" => "SC030", "c" => "SC02E", "d" => "SC020",
        "e" => "SC012", "f" => "SC021", "g" => "SC022", "h" => "SC023",
        "i" => "SC017", "j" => "SC024", "k" => "SC025", "l" => "SC026",
        "m" => "SC032", "n" => "SC031", "o" => "SC018", "p" => "SC019",
        "q" => "SC010", "r" => "SC013", "s" => "SC01F", "t" => "SC014",
        "u" => "SC016", "v" => "SC02F", "w" => "SC011", "x" => "SC02D",
        "y" => "SC015", "z" => "SC02C",
        "1" => "SC002", "2" => "SC003", "3" => "SC004", "4" => "SC005",
        "5" => "SC006", "6" => "SC007", "7" => "SC008", "8" => "SC009",
        "9" => "SC00A", "0" => "SC00B",
        "-" => "SC00C", "=" => "SC00D", "[" => "SC01A", "]" => "SC01B",
        "\\" => "SC02B", ";" => "SC027", "'" => "SC028", "`" => "SC029",
        "," => "SC033", "." => "SC034", "/" => "SC035",
        _ => return None,
    })
}

fn trigger_to_key(trigger: &str) -> String {
    let trigger = trigger.trim().to_lowercase();
    let mut mods = String::new();
    let mut key = String::new();
    let mut modifier_key = String::new();
    // On AltGr layouts RAlt always comes with a synthetic LCtrl held, which would
    // block a non-wildcard ralt hotkey from ever firing; `*` tolerates it (AHK
    // still prefers a non-wildcard hotkey when one matches exactly).
    let mut altgr = false;

    for part in trigger.split_whitespace() {
        match part {
            "ctrl"  => { mods.push('^'); modifier_key = "Control".to_string(); }
            "lctrl" => { mods.push_str("<^"); modifier_key = "LControl".to_string(); }
            "rctrl" => { mods.push_str(">^"); modifier_key = "RControl".to_string(); }
            "shift" => { mods.push('+'); modifier_key = "Shift".to_string(); }
            "lshift" => { mods.push_str("<+"); modifier_key = "LShift".to_string(); }
            "rshift" => { mods.push_str(">+"); modifier_key = "RShift".to_string(); }
            "alt"   => { mods.push('!'); modifier_key = "Alt".to_string(); }
            "lalt" => { mods.push_str("<!"); modifier_key = "LAlt".to_string(); }
            "ralt" => { mods.push_str(">!"); modifier_key = "RAlt".to_string(); altgr = true; }
            "win" => { mods.push('#'); modifier_key = "LWin".to_string(); }
            "lwin" => { mods.push_str("<#"); modifier_key = "LWin".to_string(); }
            "rwin" => { mods.push_str(">#"); modifier_key = "RWin".to_string(); }
            k       => key = k.to_string(),
        }
    }

    let wild = if altgr { "*" } else { "" };
    if key.is_empty() {
        if modifier_key.is_empty() {
            return modifier_key;
        }
        return format!("{wild}{modifier_key}");
    }

    if let Some(rest) = key.strip_prefix('f') {
        if rest.parse::<u32>().is_ok() {
            key = format!("F{rest}");
        }
    }

    if let Some(sc) = layout_scancode(&key).or_else(|| us_scancode(&key).map(String::from)) {
        key = sc;
    }

    format!("${wild}{mods}{key}")
}

const BEHAVIOR_ENGINE: &str = r###"; Resolve a single-character key name for sending. When the character exists on the
; active layout, keep the name: AHK then picks that layout's key (and shift state), so
; "y" types y on QWERTZ too — a fixed US scancode would press the wrong key there.
; The US-QWERTY scancode is only a fallback for layouts where the character doesn't
; exist at all (e.g. Cyrillic), where the name would fail to resolve.
PhysKey(key) {
    static sc := Map(
        "a", "SC01E", "b", "SC030", "c", "SC02E", "d", "SC020",
        "e", "SC012", "f", "SC021", "g", "SC022", "h", "SC023",
        "i", "SC017", "j", "SC024", "k", "SC025", "l", "SC026",
        "m", "SC032", "n", "SC031", "o", "SC018", "p", "SC019",
        "q", "SC010", "r", "SC013", "s", "SC01F", "t", "SC014",
        "u", "SC016", "v", "SC02F", "w", "SC011", "x", "SC02D",
        "y", "SC015", "z", "SC02C",
        "1", "SC002", "2", "SC003", "3", "SC004", "4", "SC005",
        "5", "SC006", "6", "SC007", "7", "SC008", "8", "SC009",
        "9", "SC00A", "0", "SC00B",
        "-", "SC00C", "=", "SC00D", "[", "SC01A", "]", "SC01B",
        "\", "SC02B", ";", "SC027", "'", "SC028", "``", "SC029",
        ",", "SC033", ".", "SC034", "/", "SC035")
    if sc.Has(key) {
        try {
            if (GetKeySC(key))
                return key
        }
        return sc[key]
    }
    return key
}

ExecuteBehavior(str) {
    MouseGetPos &savedX, &savedY
    locked := false
    try {
        for token in StrSplit(str, ";") {
            token := Trim(token)
            if (token = "")
                continue
            if (token = "savecursor") {
                MouseGetPos &savedX, &savedY
            } else if (token = "restorecursor") {
                if locked {
                    BlockInput "MouseMoveOff"
                    locked := false
                }
                SendMode "Event"
                SetMouseDelay -1
                MouseMove savedX, savedY, 0
            } else if (token = "lock") {
                BlockInput "MouseMove"
                locked := true
            } else if RegExMatch(token, "i)^goto\((-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\)$", &m) {
                SendMode "Event"
                SetMouseDelay -1
                GetGameViewport(&gameX, &gameY, &gameW, &gameH)
                MouseMove gameX + ResolveCoord(m[1], gameW), gameY + ResolveCoord(m[2], gameH), 0
            } else if RegExMatch(token, "i)^press\((.+)\)$", &m) {
                for k in StrSplit(m[1], ",")
                    DoPress(Trim(k))
                Sleep 30
            } else if RegExMatch(token, "i)^repeat\((.+?),\s*(\d+)(?:,\s*\d+)?\)$", &m) {
                DoPress(Trim(m[1]))
                Sleep 30
            } else if RegExMatch(token, "i)^hold\((.+)\)$", &m) {
                DoPress(Trim(m[1]))
            } else if RegExMatch(token, "i)^state\((.+)\)$", &m) {
                SendAppEvent("state_triggered", "", Trim(m[1]))
            } else if RegExMatch(token, "i)^sleep\((\d+)\)$", &m) {
                Sleep Integer(m[1])
            } else if RegExMatch(token, "i)^send\((.+)\)$", &m) {
                SendInput("{Text}" m[1])
            }
        }
    } finally {
        if locked
            BlockInput "MouseMoveOff"
    }
}

ResolveCoord(value, size) {
    numeric := Number(value)
    if (Abs(numeric) <= 100)
        return Round((numeric / 100) * size)
    return Round(numeric)
}

GetGameViewport(&x, &y, &w, &h) {
    if TryGetViewportFromApp(&x, &y, &w, &h)
        return

    WinGetClientPos &x, &y, &w, &h, "A"
    bestArea := 0

    for childHwnd in WinGetControlsHwnd("A") {
        if !DllCall("IsWindowVisible", "ptr", childHwnd, "int")
            continue

        rect := Buffer(16, 0)
        if !DllCall("GetWindowRect", "ptr", childHwnd, "ptr", rect.Ptr, "int")
            continue

        left := NumGet(rect, 0, "int")
        top := NumGet(rect, 4, "int")
        right := NumGet(rect, 8, "int")
        bottom := NumGet(rect, 12, "int")

        childX := Max(left, x)
        childY := Max(top, y)
        childRight := Min(right, x + w)
        childBottom := Min(bottom, y + h)
        childW := childRight - childX
        childH := childBottom - childY
        if (childW <= 0 || childH <= 0)
            continue

        area := childW * childH
        if (area > bestArea) {
            bestArea := area
            x := childX
            y := childY
            w := childW
            h := childH
        }
    }
}

TryGetViewportFromApp(&x, &y, &w, &h) {
    try {
        xhr := ComObject("WinHttp.WinHttpRequest.5.1")
        xhr.Open("GET", "http://127.0.0.1:17823/viewport", false)
        xhr.Send()
        if (xhr.Status != 200)
            return false

        parts := StrSplit(Trim(xhr.ResponseText), ",")
        if (parts.Length != 4)
            return false

        x := Integer(parts[1])
        y := Integer(parts[2])
        w := Integer(parts[3])
        h := Integer(parts[4])
        return (w > 0 && h > 0)
    } catch Error {
        return false
    }
}

DoPress(keyStr, holdMs := 30, spin := false) {
    mods := ""
    key  := ""
    for part in StrSplit(Trim(StrLower(keyStr)), " ") {
        if (part = "ctrl")
            mods .= "^"
        else if (part = "lctrl")
            mods .= "<^"
        else if (part = "rctrl")
            mods .= ">^"
        else if (part = "shift")
            mods .= "+"
        else if (part = "lshift")
            mods .= "<+"
        else if (part = "rshift")
            mods .= ">+"
        else if (part = "alt")
            mods .= "!"
        else if (part = "lalt")
            mods .= "<!"
        else if (part = "ralt")
            mods .= ">!"
        else if (part = "win")
            mods .= "#"
        else if (part = "lwin")
            mods .= "<#"
        else if (part = "rwin")
            mods .= ">#"
        else
            key := part
    }
    if RegExMatch(key, "i)^f(\d+)$", &m)
        key := "F" m[1]
    key := PhysKey(key)
    ; If no key was given, the modifier itself is the key to press
    if (key = "") {
        if (mods = "<^")
            DoPressKey("LCtrl")
        else if (mods = ">^")
            DoPressKey("RCtrl")
        else if (mods = "^")
            DoPressKey("Ctrl")
        else if (mods = "<+")
            DoPressKey("LShift")
        else if (mods = ">+")
            DoPressKey("RShift")
        else if (mods = "+")
            DoPressKey("Shift")
        else if (mods = "<!")
            DoPressKey("LAlt")
        else if (mods = ">!")
            DoPressKey("RAlt")
        else if (mods = "!")
            DoPressKey("Alt")
        else if (mods = "<#")
            DoPressKey("LWin")
        else if (mods = ">#")
            DoPressKey("RWin")
        else if (mods = "#")
            DoPressKey("LWin")
        return
    }
    ctrlKey := ""
    if InStr(mods, "<^")
        ctrlKey := "LCtrl"
    else if InStr(mods, ">^")
        ctrlKey := "RCtrl"
    else if InStr(mods, "^")
        ctrlKey := "Ctrl"
    shiftKey := ""
    if InStr(mods, "<+")
        shiftKey := "LShift"
    else if InStr(mods, ">+")
        shiftKey := "RShift"
    else if InStr(mods, "+")
        shiftKey := "Shift"
    altKey := ""
    if InStr(mods, "<!")
        altKey := "LAlt"
    else if InStr(mods, ">!")
        altKey := "RAlt"
    else if InStr(mods, "!")
        altKey := "Alt"
    if (key = "m1" || key = "m2") {
        phys    := (key = "m1") ? "LButton" : "RButton"
        wasHeld := GetKeyState(phys, "P")
        SendModState(ctrlKey,  "Down")
        SendModState(shiftKey, "Down")
        SendModState(altKey,   "Down")
        if wasHeld
            SendInput("{" phys " Up}")
        Sleep 30
        SendInput("{" phys " Down}")
        Sleep 30
        SendInput("{" phys " Up}")
        SendModState(altKey,   "Up")
        SendModState(shiftKey, "Up")
        SendModState(ctrlKey,  "Up")
        return
    }
    SendModState(ctrlKey,  "Down")
    SendModState(shiftKey, "Down")
    SendModState(altKey,   "Down")
    if (mods != "")
        Sleep 20
    SendInput("{" key " Down}")
    try {
        if (spin)
            SpinHold(holdMs)  ; precise sub-Sleep-granularity hold for the repeat tap
        else
            Sleep holdMs
    } finally {
        SendInput("{" key " Up}")   ; always release, even if the hold throws, so no key sticks
    }
    if (mods != "")
        Sleep 20
    SendModState(altKey,   "Up")
    SendModState(shiftKey, "Up")
    SendModState(ctrlKey,  "Up")
}

DoPressKey(keyName) {
    SendInput("{" keyName " Down}")
    Sleep 30
    SendInput("{" keyName " Up}")
}

; Press or release a modifier only if one is set. The SendInput is kept on its own
; line: AHK v2 misparses a "{" string literal in a one-line "if" body as a block.
SendModState(modKey, dir) {
    if (modKey != "")
        SendInput("{" modKey " " dir "}")
}

; A held remap. The press hotkey calls HoldKeyDown and a paired wildcard key-up
; hotkey calls HoldKeyUp, so the key stays down for exactly as long as the trigger is
; held (e.g. a forced Copilot key behaving as Ctrl). This mirrors how AutoHotkey
; implements native key remapping:
;   - {Blind} leaves the trigger's own modifiers untouched while sending.
;   - DownR re-presses the key on the hardware's auto-repeat so it stays down.
; A key-up hotkey is the only reliable release signal: the press hotkey suppresses
; the trigger, so its logical/physical state can't be polled for release.
HoldKeyDown(keyStr) {
    for k in HoldKeyList(keyStr)
        SendInput("{Blind}{" k " DownR}")
}

HoldKeyUp(keyStr) {
    keys := HoldKeyList(keyStr)
    i := keys.Length
    while (i >= 1) {
        SendInput("{Blind}{" keys[i] " Up}")
        i--
    }
}

HoldKeyList(keyStr) {
    held := []
    key := ""
    for part in StrSplit(Trim(StrLower(keyStr)), " ") {
        if (part = "ctrl")
            held.Push("Ctrl")
        else if (part = "lctrl")
            held.Push("LCtrl")
        else if (part = "rctrl")
            held.Push("RCtrl")
        else if (part = "shift")
            held.Push("Shift")
        else if (part = "lshift")
            held.Push("LShift")
        else if (part = "rshift")
            held.Push("RShift")
        else if (part = "alt")
            held.Push("Alt")
        else if (part = "lalt")
            held.Push("LAlt")
        else if (part = "ralt")
            held.Push("RAlt")
        else if (part = "win")
            held.Push("LWin")
        else if (part = "lwin")
            held.Push("LWin")
        else if (part = "rwin")
            held.Push("RWin")
        else
            key := part
    }
    if RegExMatch(key, "i)^f(\d+)$", &m)
        key := "F" m[1]
    if (key != "")
        held.Push(PhysKey(key))
    return held
}

; Busy-wait for `ms` milliseconds using the high-resolution performance counter. AHK's
; Sleep can't reliably hold a key for only a few ms (its granularity floors near 15ms),
; and a repeat aimed at a game that acts on a key every frame it is held needs the key
; down for a short, EXACT window (about one frame) so it registers exactly one press.
SpinHold(ms) {
    static freq := 0
    if (!freq)
        DllCall("QueryPerformanceFrequency", "Int64*", &freq)
    t0 := 0
    t := 0
    DllCall("QueryPerformanceCounter", "Int64*", &t0)
    limit := t0 + Round(ms * freq / 1000)
    loop {
        DllCall("QueryPerformanceCounter", "Int64*", &t)
    } until (t >= limit)
}

; Hold-to-repeat. The press hotkey runs this loop for exactly as long as the trigger's
; physical key is held, pressing `keys` once per `interval` ms. Because it occupies the
; hotkey's single thread for the whole hold (#MaxThreadsPerHotkey is 1 by default), the
; trigger's OS key-repeat — and any key events this loop itself injects — cannot re-enter
; it, so there is only ever one loop and the rate is the interval, not the OS repeat rate.
; Each press holds the key down for exactly `holdMs` (precise busy-wait), tunable so a game
; that reads the key per frame can be made to register exactly one press per interval.
RepeatHold(keys, interval, triggerKey, exe, holdMs, enabledKey) {
    global enabled, repeatDown
    ; Stop as soon as EITHER release signal fires: the key-up hotkey clearing repeatDown, or
    ; the physical-state poll. SendInput disables the keyboard hook while it sends, so a real
    ; key-up landing in that window can be missed by one path and leave the key stuck "down";
    ; requiring both to stay true means a miss on either one can no longer wedge the loop.
    while ((repeatDown.Has(triggerKey) && repeatDown[triggerKey]) && GetKeyState(triggerKey, "P")) {
        ; Pause (don't fire) while this profile is toggled off or its app is not focused —
        ; so the repeat can't leak into other apps — but keep looping until release.
        if (!enabled[enabledKey] || (exe != "" && !WinActive("ahk_exe " exe))) {
            Sleep 15
            continue
        }
        start := A_TickCount
        DoPress(keys, holdMs, true)
        elapsed := A_TickCount - start
        if (elapsed < interval)
            Sleep interval - elapsed
    }
    repeatDown[triggerKey] := false
}"###;

#[cfg(test)]
mod tests {
    use super::generate_combined_script;

    #[test]
    fn combined_script_keeps_copilot_recovery_without_profiles() {
        let script = generate_combined_script(&[]);

        assert!(script.contains("$*SC06E::"));
        assert!(script.contains("SetTimer CheckCopilotRelease, 10"));
        assert!(script.contains("GetAsyncKeyState"));
        assert!(!script.contains("A_TimeIdleKeyboard"));
    }
}
