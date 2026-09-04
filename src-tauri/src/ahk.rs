use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

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

/// Ask AutoHotkey's hidden main window to close so its OnExit input cleanup runs. Returns
/// false when no window for this child exists, in which case waiting cannot help.
#[cfg(target_os = "windows")]
fn request_graceful_exit(child: &Child) -> bool {
    use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, TRUE};
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE};

    struct ExitRequest {
        pid: DWORD,
        posted: bool,
    }

    unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let request = &mut *(lparam as *mut ExitRequest);
        let mut window_pid = 0;
        GetWindowThreadProcessId(hwnd, &mut window_pid);
        if window_pid == request.pid && PostMessageW(hwnd, WM_CLOSE, 0, 0) != 0 {
            request.posted = true;
        }
        TRUE
    }

    let mut request = ExitRequest {
        pid: child.id(),
        posted: false,
    };
    unsafe { EnumWindows(Some(enum_window), &mut request as *mut _ as LPARAM); }
    request.posted
}

fn stop_process(mut child: Child) {
    #[cfg(target_os = "windows")]
    if request_graceful_exit(&child) {
        let deadline = Instant::now() + Duration::from_millis(750);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
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
        // Let the old script release every synthetic input it owns before replacing it.
        if let Some(old) = self.process.take() {
            stop_process(old);
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
        if let Some(child) = self.process.take() {
            stop_process(child);
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
    pub exe: &'a str,
}

/// Emit one armed profile's `#HotIf` block(s). Hotkeys/scripts sit under
/// `WinActive("ahk_exe <exe>") && enabled["<id>"]` (or just `enabled["<id>"]` for exe "*") so
/// they only fire while that app is focused; toggle keys sit under a focus-only gate so a
/// disabled profile can still be re-enabled. `used_keys` tracks each activating key plus its
/// held prerequisites per exe: the same chord may be bound for different apps, but within one
/// app the first armed profile wins it (a duplicate label under an identical #HotIf would fail
/// to load and kill every hotkey).
fn generate_profile_block(
    ap: &ArmedProfile,
    used_keys: &mut HashMap<String, HashSet<String>>,
    hold_ups: &mut HashSet<String>,
    repeat_ups: &mut HashSet<String>,
) -> String {
    let p = ap.profile;
    let exe = ap.exe.trim();
    let global_game = exe == GLOBAL_GAME_EXE;
    let id = escape_ahk_string(&p.id);
    let exe_esc = escape_ahk_string(exe);
    let mut resolved = config::resolve_profile_hotkeys(ap.siblings, p);
    // AutoHotkey chooses the first matching #HotIf variant for a key. Put the most specific
    // chord first so "x y z" wins over "x z" while all three keys are down.
    resolved.sort_by_key(|hotkey| std::cmp::Reverse(trigger_chord_keys(&hotkey.trigger).len()));
    let keyset = used_keys.entry(exe.to_string()).or_default();
    let mut lines = String::new();
    let mut combo_blocks = String::new();
    let enabled_condition = if global_game {
        format!("enabled[\"{id}\"]")
    } else {
        format!("WinActive(\"ahk_exe {exe_esc}\") && enabled[\"{id}\"]")
    };
    let focus_condition = if global_game {
        String::new()
    } else {
        format!("WinActive(\"ahk_exe {exe_esc}\")")
    };

    for hk in resolved {
        let ahk_key = trigger_to_key(&hk.trigger);
        if ahk_key.is_empty() { continue; }
        let chord_keys = trigger_chord_keys(&hk.trigger);
        let physical_chord_keys = trigger_physical_chord_keys(&hk.trigger);
        let prerequisite_keys = chord_keys[..chord_keys.len().saturating_sub(1)].join(" ");
        let binding_id = format!("{ahk_key}|{prerequisite_keys}");
        if !keyset.insert(binding_id) { continue; }
        let trigger = escape_ahk_string(&hk.trigger);
        let trigger_modifiers = trigger_modifier_symbols(&hk.trigger);
        // Unmodified held remaps must still activate while an unrelated modifier is held.
        // Keep explicit modifier chords exact so shifted and unshifted shortcuts stay distinct.
        let ahk_key = if trigger_modifiers.is_empty()
            && (parse_pure_hold(&hk.behavior).is_some() || has_compound_hold(&hk.behavior))
        {
            format!("$*{}", ahk_key.trim_start_matches('$'))
        } else {
            ahk_key
        };
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
        if let Some(hold_arg) = parse_pure_hold(&hk.behavior) {
            let keys = escape_ahk_string(&hold_arg);
            let owner = escape_ahk_string(&format!("{}:{}", p.id, hk.trigger));
            let chord = escape_ahk_string(&physical_chord_keys.join(" "));
            let binding = format!(
                "{ahk_key}:: {{\n    HoldChordDown(\"{owner}\", \"{keys}\", \"{chord}\", \"{trigger_modifiers}\")\n{notify}}}\n"
            );
            push_trigger_binding(
                &mut lines,
                &mut combo_blocks,
                &enabled_condition,
                &prerequisite_keys,
                &binding,
            );
            for trigger_key in physical_chord_keys {
                hold_ups.insert(trigger_key);
            }
            continue;
        }

        // A multi-step behavior containing hold(...) runs its one-shot steps once, while each
        // hold action remains down until the trigger is released. Registering the whole behavior
        // as one owner also suppresses hardware auto-repeat from replaying its press actions.
        if has_compound_hold(&hk.behavior) {
            let behavior = escape_ahk_string(&hk.behavior);
            let owner = escape_ahk_string(&format!("{}:{}", p.id, hk.trigger));
            let chord = escape_ahk_string(&physical_chord_keys.join(" "));
            let behavior_exe = escape_ahk_string(exe);
            let binding = format!(
                "{ahk_key}:: {{\n    HoldBehaviorDown(\"{owner}\", \"{behavior}\", \"{chord}\", \"{trigger_modifiers}\", \"{behavior_exe}\")\n{notify}}}\n"
            );
            push_trigger_binding(
                &mut lines,
                &mut combo_blocks,
                &enabled_condition,
                &prerequisite_keys,
                &binding,
            );
            for trigger_key in physical_chord_keys {
                hold_ups.insert(trigger_key);
            }
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
                let chord = escape_ahk_string(&physical_chord_keys.join(" "));
                let repeat_exe = if global_game { String::new() } else { exe_esc.clone() };
                // A synthetic up for the trigger itself also changes Windows' asynchronous
                // state, so skip that fallback for same-trigger output. The AHK hook remains
                // installed for every repeat regardless.
                let use_windows_state = !repeat_output_uses_trigger(&repeat_keys, &hk.trigger);
                let binding = format!(
                    "{ahk_key}:: {{\n    repeatDown[\"{poll_key}\"] := true\n    repeatChord[\"{poll_key}\"] := \"{chord}\"\n{notify}    RepeatHold(\"{keys}\", {interval}, \"{poll_key}\", \"{chord}\", \"{repeat_exe}\", {hold}, \"{id}\", {use_windows_state}, \"{trigger_modifiers}\")\n}}\n"
                );
                push_trigger_binding(
                    &mut lines,
                    &mut combo_blocks,
                    &enabled_condition,
                    &prerequisite_keys,
                    &binding,
                );
                // One global key-up hotkey per physical key clears the repeat flag. `~` lets the
                // native key-up through so normal typing of the key still works; keyed by the bare
                // key because there is only one physical key regardless of how many triggers use it.
                repeat_ups.insert(poll_key);
                continue;
            }
        }

        let behavior = escape_ahk_string(&hk.behavior);
        let behavior_exe = escape_ahk_string(exe);
        // Run the behavior first, then notify the overlay (when a state_id makes the ping
        // meaningful): the ping is a blocking localhost request, so doing it after keeps a
        // busy backend from delaying the output.
        let binding = format!(
            "{ahk_key}:: {{\n    ExecuteBehavior(\"{behavior}\", \"{trigger_modifiers}\", \"{behavior_exe}\")\n{notify}}}\n"
        );
        push_trigger_binding(
            &mut lines,
            &mut combo_blocks,
            &enabled_condition,
            &prerequisite_keys,
            &binding,
        );
    }

    for script in &p.scripts {
        if !script.enabled || script.trigger != "hotkey" { continue; }
        let ahk_key = trigger_to_key(&script.hotkey);
        let chord_keys = trigger_chord_keys(&script.hotkey);
        let prerequisite_keys = chord_keys[..chord_keys.len().saturating_sub(1)].join(" ");
        let binding_id = format!("{ahk_key}|{prerequisite_keys}");
        if ahk_key.is_empty() || !keyset.insert(binding_id) { continue; }
        let sid = escape_ahk_string(&script.id);
        push_trigger_binding(
            &mut lines,
            &mut combo_blocks,
            &enabled_condition,
            &prerequisite_keys,
            &format!("{ahk_key}:: RunScript(\"{sid}\")\n"),
        );
    }

    // Toggle keys only bind when explicitly set; the overlay-toggle is skipped when it equals
    // the hotkeys-toggle. Both flip THIS profile's enabled flag, gated by focus only so a
    // disabled profile can be re-enabled from its own app.
    let mut toggle_lines = String::new();
    let mut toggle_combo_blocks = String::new();
    let mut toggle_bindings = HashSet::new();
    for trigger in [p.toggle_hotkeys_key.as_deref(), p.toggle_overlay_key.as_deref()]
        .into_iter()
        .flatten()
    {
        let key = trigger_to_key(trigger);
        if key.is_empty() {
            continue;
        }
        let chord_keys = trigger_chord_keys(trigger);
        let prerequisite_keys = chord_keys[..chord_keys.len().saturating_sub(1)].join(" ");
        if !toggle_bindings.insert(format!("{key}|{prerequisite_keys}")) {
            continue;
        }
        push_trigger_binding(
            &mut toggle_lines,
            &mut toggle_combo_blocks,
            &focus_condition,
            &prerequisite_keys,
            &format!("{key}:: ToggleEnabled(\"{id}\")\n"),
        );
    }

    let mut out = combo_blocks;
    if !lines.is_empty() {
        out.push_str(&format!("#HotIf {enabled_condition}\n{lines}#HotIf\n"));
    }
    out.push_str(&toggle_combo_blocks);
    if !toggle_lines.is_empty() {
        if global_game {
            out.push_str(&format!("#HotIf\n{toggle_lines}#HotIf\n"));
        } else {
            out.push_str(&format!("#HotIf WinActive(\"ahk_exe {exe_esc}\")\n{toggle_lines}#HotIf\n"));
        }
    }
    out
}

fn push_trigger_binding(
    simple_lines: &mut String,
    combo_blocks: &mut String,
    base_condition: &str,
    prerequisite_keys: &str,
    binding: &str,
) {
    if prerequisite_keys.is_empty() {
        simple_lines.push_str(binding);
        return;
    }
    let prerequisite_keys = escape_ahk_string(prerequisite_keys);
    let condition = if base_condition.is_empty() {
        format!("TriggerChordHeld(\"{prerequisite_keys}\")")
    } else {
        format!("{base_condition} && TriggerChordHeld(\"{prerequisite_keys}\")")
    };
    combo_blocks.push_str(&format!("#HotIf {condition}\n{binding}#HotIf\n"));
}

/// Build ONE always-on AHK script for every armed profile, each gated to its own app.
pub fn generate_combined_script(armed: &[ArmedProfile]) -> String {
    // Specific-exe blocks first, "*" last, so an app's binding takes precedence over a global
    // one for the same key.
    let mut ordered: Vec<&ArmedProfile> = armed.iter().collect();
    ordered.sort_by_key(|ap| ap.exe.trim() == GLOBAL_GAME_EXE);

    let mut used_keys: HashMap<String, HashSet<String>> = HashMap::new();
    let mut hold_ups: HashSet<String> = HashSet::new();
    let mut repeat_ups: HashSet<String> = HashSet::new();
    let mut blocks = String::new();
    let mut enabled_init = String::new();
    let mut overlay_init = String::new();
    let mut event_profiles_init = String::new();
    let mut target_states_init = String::new();
    let mut event_targets = HashSet::new();

    for ap in &ordered {
        let p = ap.profile;
        let exe = ap.exe.trim();
        if exe.is_empty() { continue; }
        let id = escape_ahk_string(&p.id);
        let exe_esc = escape_ahk_string(exe);
        enabled_init.push_str(&format!("enabled[\"{id}\"] := true\n"));
        blocks.push_str(&generate_profile_block(
            ap,
            &mut used_keys,
            &mut hold_ups,
            &mut repeat_ups,
        ));
        // Only overlay-type profiles drive the overlay window; other profile kinds never
        // show it (otherwise an armed hotkeys profile would pop an empty overlay).
        if p.kind == "overlay" && !p.overlay_disabled {
            let overlay_exe = if exe == GLOBAL_GAME_EXE { "*" } else { &exe_esc };
            overlay_init.push_str(&format!(
                "overlayProfiles.Push(Map(\"id\", \"{id}\", \"exe\", \"{overlay_exe}\"))\n"
            ));
        }

        if exe != GLOBAL_GAME_EXE {
            let resolved_events = if p.kind == "events" {
                config::resolve_profile_events(ap.siblings, p)
            } else {
                Vec::new()
            };
            let event_pairs: Vec<String> = resolved_events
                .into_iter()
                .filter(|event| matches!(
                    event.event.as_str(),
                    "app_started" | "app_stopped" | "window_ready" | "focus_gained" | "focus_lost"
                ))
                .filter(|event| !event.behavior.trim().is_empty())
                .map(|event| format!(
                    "\"{}\", \"{}\"",
                    escape_ahk_string(&event.event),
                    escape_ahk_string(&event.behavior),
                ))
                .collect();
            let launch_scripts: Vec<String> = p.scripts
                .iter()
                .filter(|script| script.enabled && script.trigger == "launch")
                .map(|script| format!("\"{}\"", escape_ahk_string(&script.id)))
                .collect();

            if !event_pairs.is_empty() || !launch_scripts.is_empty() {
                let event_exe = exe.to_lowercase();
                let event_exe_esc = escape_ahk_string(&event_exe);
                let events = format!("Map({})", event_pairs.join(", "));
                let scripts = format!("[{}]", launch_scripts.join(", "));
                event_profiles_init.push_str(&format!(
                    "eventProfiles.Push(Map(\"id\", \"{id}\", \"exe\", \"{event_exe_esc}\", \"target\", \"{exe_esc}\", \"events\", {events}, \"launchScripts\", {scripts}))\n"
                ));
                if event_targets.insert(event_exe) {
                    target_states_init.push_str(&format!(
                        "targetStates[\"{event_exe_esc}\"] := Map(\"initialized\", false, \"running\", false, \"windowReady\", false, \"focused\", false)\n"
                    ));
                }
            }
        }
    }

    let mut release_keys: Vec<&String> = hold_ups.union(&repeat_ups).collect();
    release_keys.sort();
    let mut release_up_lines = String::new();
    for key in release_keys {
        let releases_repeat = repeat_ups.contains(key);
        let releases_hold = hold_ups.contains(key);
        let key = escape_ahk_string(key);
        let mut actions = String::new();
        if releases_repeat {
            actions.push_str(&format!("    repeatDown[\"{key}\"] := false\n"));
        }
        if releases_hold {
            actions.push_str(&format!("    ReleaseTriggerHolds(\"{key}\")\n"));
        }
        // This handler is global so release recovery survives profile/focus changes. Always
        // pass the physical up through when no active owner suppressed its corresponding down.
        // Ignore an output tap's synthetic up when the macro presses its own trigger button;
        // the hook's physical state stays down until the user actually releases it.
        release_up_lines.push_str(&format!(
            "~*{key} up:: {{\n    if GetKeyState(\"{key}\", \"P\")\n        return\n{actions}}}\n"
        ));
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
global eventProfiles := []
global targetStates := Map()
global behaviorEventQueue := []
global behaviorEventDraining := false
global repeatDown := Map()
global repeatChord := Map()
global syntheticDown := Map()
global chordHolds := Map()
global heldOutputCounts := Map()
global mirroredMouseDown := Map()
global copilotState := "idle"
global copilotShiftSuppressed := false
global copilotShiftForwarded := false
global copilotCtrlHeld := false
global copilotCtrlReleasePending := false
global behaviorClipboardBackup := ""
global behaviorClipboardPending := false
global behaviorClipboardSequence := 0
{enabled_init}{overlay_init}{event_profiles_init}{target_states_init}OnExit ReleaseSyntheticHeld
OnExit ReleaseCopilotHeld
OnExit HideOverlayOnExit
OnExit RestoreBehaviorClipboard

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

QueueBehaviorEvent(behavior, targetExe) {{
    global behaviorEventQueue
    behaviorEventQueue.Push(Map("behavior", behavior, "target", targetExe))
    SetTimer DrainBehaviorEvents, -1
}}

DrainBehaviorEvents(*) {{
    global behaviorEventQueue, behaviorEventDraining
    if behaviorEventDraining
        return
    behaviorEventDraining := true
    try {{
        while behaviorEventQueue.Length > 0 {{
            queued := behaviorEventQueue.RemoveAt(1)
            ExecuteBehavior(queued["behavior"], "", queued["target"])
        }}
    }} finally {{
        behaviorEventDraining := false
        if behaviorEventQueue.Length > 0
            SetTimer DrainBehaviorEvents, -1
    }}
}}

DispatchTargetEvent(exe, eventName) {{
    global eventProfiles
    for profile in eventProfiles {{
        if (profile["exe"] != exe)
            continue
        if (eventName = "app_started") {{
            for scriptId in profile["launchScripts"]
                RunScript(scriptId)
        }}
        events := profile["events"]
        if events.Has(eventName)
            QueueBehaviorEvent(events[eventName], profile["target"])
    }}
}}

; One monitor owns target lifecycle transitions and overlay focus. The first observation only
; seeds state, so rebuilding this script cannot impersonate an app or focus event.
SyncRuntime(*) {{
    global enabled, targetStates, overlayProfiles, overlayVisible, lastFocusId
    for exe, state in targetStates {{
        running := ProcessExist(exe) != 0
        windowReady := running && WinExist("ahk_exe " exe) != 0
        focused := windowReady && WinActive("ahk_exe " exe) != 0
        if !state["initialized"] {{
            state["initialized"] := true
        }} else {{
            if !state["running"] && running
                DispatchTargetEvent(exe, "app_started")
            if !state["windowReady"] && windowReady
                DispatchTargetEvent(exe, "window_ready")
            if !state["focused"] && focused
                DispatchTargetEvent(exe, "focus_gained")
            if state["focused"] && !focused
                DispatchTargetEvent(exe, "focus_lost")
            if state["running"] && !running
                DispatchTargetEvent(exe, "app_stopped")
        }}
        state["running"] := running
        state["windowReady"] := windowReady
        state["focused"] := focused
    }}

    ; The overlay follows the focused app: find the first armed overlay profile whose app is
    ; focused (specific apps first, then "*"), tell the backend which profile's config to push,
    ; and show/hide from that profile's enabled flag.
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

SetTimer SyncRuntime, 200
SyncRuntime()

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
    ; If the old hook missed Copilot's key-up, its synthetic Ctrl belongs to that dead hook
    ; generation. Release it now; a still-held Copilot key will reacquire Ctrl on key repeat.
    ReleaseCopilotCtrl()
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

; The key-up hotkey is the primary release signal. This independent owner-scoped monitor
; clears a repeat if that hotkey is ever delayed or displaced by another hook callback.
CheckRepeatReleases(*) {{
    global repeatDown, repeatChord
    anyDown := false
    for triggerKey, down in repeatDown {{
        if !down
            continue
        chord := repeatChord.Has(triggerKey) ? repeatChord[triggerKey] : triggerKey
        if TriggerChordHeld(chord)
            anyDown := true
        else
            repeatDown[triggerKey] := false
    }}
    if !anyDown
        SetTimer CheckRepeatReleases, 0
}}

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
; The remap and profile hotkeys intentionally share one keyboard hook. SendInput temporarily
; removes that hook, so its RCtrl cannot act as a modifier for this script's own RCtrl hotkeys.
; SendEvent at level 1 keeps the remapped key visible to the level-0 profile bindings and lets
; the same hook forget Copilot's built-in LWin/LShift before matching the user's next key.
SendCopilotKeys(keys) {{
    previousSendLevel := SendLevel(1)
    try SendEvent(keys)
    finally SendLevel(previousSendLevel)
}}

ReleaseStaleCopilotModifiers() {{
    if !GetKeyState("RCtrl", "P")
        SendCopilotKeys("{{Blind}}{{RCtrl up}}")
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
        SetTimer CheckCopilotRelease, 10
    }} else {{
        copilotCtrlReleasePending := false
        SetTimer CheckCopilotRelease, 0
        SendCopilotKeys("{{Blind}}{{RCtrl up}}")
    }}
}}

ReleaseCopilotHeld(*) {{
    global copilotCtrlReleasePending, copilotShiftForwarded
    ReleaseCopilotCtrl()
    if copilotCtrlReleasePending && !GetKeyState("RCtrl", "P")
        SendCopilotKeys("{{Blind}}{{RCtrl up}}")
    copilotCtrlReleasePending := false
    copilotShiftForwarded := false
    if !GetKeyState("LWin", "P")
        SendInput "{{Blind}}{{LWin up}}"
    if !GetKeyState("LShift", "P")
        SendInput "{{Blind}}{{LShift up}}"
}}

CheckCopilotRelease(*) {{
    global copilotCtrlReleasePending
    if !copilotCtrlReleasePending {{
        SetTimer CheckCopilotRelease, 0
        return
    }}
    if copilotCtrlReleasePending && !GetKeyState("RCtrl", "P") {{
        copilotCtrlReleasePending := false
        SetTimer CheckCopilotRelease, 0
        SendCopilotKeys("{{Blind}}{{RCtrl up}}")
    }}
}}

CopilotShiftPhysicallyDown() {{
    return (DllCall("GetAsyncKeyState", "Int", 0xA0, "Short") & 0x8000) != 0
}}

CheckCopilotShiftRelease(*) {{
    global copilotShiftSuppressed, copilotShiftForwarded
    shiftDown := CopilotShiftPhysicallyDown()
    if copilotShiftSuppressed && !shiftDown
        copilotShiftSuppressed := false
    if copilotShiftForwarded && !shiftDown {{
        copilotShiftForwarded := false
        SendInput "{{Blind}}{{LShift up}}"
    }}
    if !copilotShiftSuppressed && !copilotShiftForwarded
        SetTimer CheckCopilotShiftRelease, 0
}}

PassCopilotKeys() {{
    global copilotState, copilotShiftSuppressed, copilotShiftForwarded
    if copilotState != "waiting"
        return
    copilotState := "lwin_passed"
    if copilotShiftSuppressed && CopilotShiftPhysicallyDown() {{
        copilotShiftSuppressed := false
        copilotShiftForwarded := true
        SendInput "{{Blind}}{{LWin down}}{{LShift down}}"
    }} else {{
        copilotShiftSuppressed := false
        SendInput "{{Blind}}{{LWin down}}"
    }}
}}

$*LWin::{{
    global copilotState
    copilotState := "waiting"
    SetTimer PassCopilotKeys, -30
}}

$*LWin up::{{
    global copilotState, copilotShiftSuppressed, copilotShiftForwarded
    if copilotState = "waiting" {{
        SetTimer PassCopilotKeys, 0
        copilotState := "idle"
        if copilotShiftSuppressed && CopilotShiftPhysicallyDown() {{
            copilotShiftSuppressed := false
            copilotShiftForwarded := true
            SendInput "{{Blind}}{{LWin down}}{{LShift down}}{{LWin up}}"
        }} else {{
            copilotShiftSuppressed := false
            SendInput "{{Blind}}{{LWin down}}{{LWin up}}"
        }}
    }} else if copilotState = "lwin_passed" {{
        copilotState := "idle"
        SendInput "{{Blind}}{{LWin up}}"
    }}
}}

; Only the Shift which immediately follows an intercepted LWin can belong to the Copilot
; chord. Every ordinary Shift press bypasses MacroToolbox and reaches games physically.
#HotIf copilotState = "waiting"
$*LShift::{{
    global copilotShiftSuppressed
    copilotShiftSuppressed := true
    SetTimer CheckCopilotShiftRelease, 5
}}
#HotIf

$*SC06E::{{
    global copilotState, copilotShiftSuppressed, copilotShiftForwarded
    global copilotCtrlHeld, copilotCtrlReleasePending
    SetTimer PassCopilotKeys, 0
    copilotState := "copilot"
    copilotShiftSuppressed := false
    SetTimer CheckCopilotShiftRelease, 0
    copilotShiftForwarded := false
    if copilotCtrlHeld
        return
    copilotCtrlHeld := true
    copilotCtrlReleasePending := false
    SetTimer CheckCopilotRelease, 0
    ; Copilot's own chord must not count as the user's Shift/Win modifiers. Clearing them and
    ; pressing RCtrl in one hook-visible batch makes Copilot+O match RCtrl+O, while a real Shift
    ; pressed afterward still makes Copilot+Shift+O match RCtrl+Shift+O.
    SendCopilotKeys("{{Blind}}{{LShift up}}{{LWin up}}{{RCtrl downR}}")
}}

$*SC06E up::{{
    global copilotState
    if copilotState = "copilot"
        copilotState := "idle"
    ReleaseCopilotCtrl()
}}

{release_up_lines}
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

/// Whether a multi-step behavior contains a hold(...) action. Pure holds use the simpler remap
/// path above; behaviors without a hold remain ordinary one-shot behaviors.
fn has_compound_hold(behavior: &str) -> bool {
    if !behavior.contains(';') {
        return false;
    }

    behavior.split(';').any(|token| {
        let token = token.trim();
        let lower = token.to_lowercase();
        if lower.starts_with("hold(") && token.ends_with(')') {
            let inner = token[5..token.len() - 1].trim();
            !inner.is_empty()
        } else {
            false
        }
    })
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

fn repeat_output_uses_trigger(repeat_keys: &str, trigger: &str) -> bool {
    let trigger_keys: Vec<String> = trigger_physical_chord_keys(trigger)
        .iter()
        .map(|key| repeat_comparison_key(key))
        .collect();
    !trigger_keys.is_empty()
        && repeat_keys.split_whitespace().any(|part| {
            let repeat_key = repeat_comparison_key(&trigger_bare_key(part));
            trigger_keys.contains(&repeat_key)
        })
}

fn repeat_comparison_key(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "control" | "lcontrol" | "rcontrol" => "control".to_string(),
        "shift" | "lshift" | "rshift" => "shift".to_string(),
        "alt" | "lalt" | "ralt" => "alt".to_string(),
        "m1" => "lbutton".to_string(),
        "m2" => "rbutton".to_string(),
        key => key.to_string(),
    }
}

/// AutoHotkey Blind-mode exclusions for modifiers contributed by a trigger. Exclusions suppress
/// those modifiers only while output is sent, so a held trigger modifier remains usable for the
/// next keypress. RAlt includes Ctrl because Windows implements AltGr as LCtrl+RAlt.
fn trigger_modifier_symbols(trigger: &str) -> String {
    let mut symbols = String::new();
    for part in trigger.split_whitespace() {
        let required = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "lctrl" | "rctrl" => "^",
            "shift" | "lshift" | "rshift" => "+",
            "alt" | "lalt" => "!",
            "ralt" => "^!",
            "win" | "lwin" | "rwin" => "#",
            _ => "",
        };
        for symbol in required.chars() {
            if !symbols.contains(symbol) {
                symbols.push(symbol);
            }
        }
    }
    symbols
}

fn is_trigger_modifier(part: &str) -> bool {
    matches!(
        part,
        "ctrl" | "lctrl" | "rctrl"
            | "shift" | "lshift" | "rshift"
            | "alt" | "lalt" | "ralt"
            | "win" | "lwin" | "rwin"
    )
}

fn normalize_trigger_key(mut key: String) -> String {
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

/// Every non-modifier in a trigger, converted to the physical key name used by AHK.
/// The final key activates the binding; all earlier keys form its held chord condition.
fn trigger_chord_keys(trigger: &str) -> Vec<String> {
    trigger
        .trim()
        .to_lowercase()
        .split_whitespace()
        .filter(|part| !is_trigger_modifier(part))
        .map(|part| normalize_trigger_key(part.to_string()))
        .collect()
}

fn trigger_physical_chord_keys(trigger: &str) -> Vec<String> {
    let keys = trigger_chord_keys(trigger);
    if !keys.is_empty() {
        return keys;
    }
    let modifier_key = trigger_bare_key(trigger);
    if modifier_key.is_empty() {
        Vec::new()
    } else {
        vec![modifier_key]
    }
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
    normalize_trigger_key(key)
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

    key = normalize_trigger_key(key);

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

; Exclude only trigger modifiers which are absent from the output. AutoHotkey releases those
; modifiers around this send and restores them immediately afterward, so holding a trigger
; modifier continues to work for subsequent keypresses.
BlindFor(triggerModifiers, outputModifiers := "") {
    for symbol in ["^", "+", "!", "#"]
        if InStr(outputModifiers, symbol)
            triggerModifiers := StrReplace(triggerModifiers, symbol)
    return "{Blind" triggerModifiers "}"
}

SendBehaviorCommand(command, configuredExe) {
    targetExe := configuredExe
    if (targetExe = "*") {
        try {
            targetExe := WinGetProcessName("A")
        } catch Error {
            return
        }
    }
    if (targetExe != "")
        SendOverlayCommand(command "?exe=" UriEncode(targetExe))
}

ExecuteBehavior(str, triggerModifiers := "", configuredExe := "", holdOwner := "", preserveHooks := false) {
    global chordHolds
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
            } else if (token = "borderless") {
                SendBehaviorCommand("borderless", configuredExe)
            } else if (token = "killprocess") {
                SendBehaviorCommand("killprocess", configuredExe)
            } else if (token = "stretch") {
                SendBehaviorCommand("stretch", configuredExe)
            } else if RegExMatch(token, "i)^goto\((-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\)$", &m) {
                SendMode "Event"
                SetMouseDelay -1
                GetGameViewport(&gameX, &gameY, &gameW, &gameH)
                MouseMove gameX + ResolveCoord(m[1], gameW), gameY + ResolveCoord(m[2], gameH), 0
            } else if RegExMatch(token, "i)^press\((.+)\)$", &m) {
                for k in StrSplit(m[1], ",")
                    DoPress(Trim(k), 30, false, preserveHooks, triggerModifiers)
                Sleep 30
            } else if RegExMatch(token, "i)^repeat\((.+?),\s*(\d+)(?:,\s*\d+)?\)$", &m) {
                DoPress(Trim(m[1]), 30, false, preserveHooks, triggerModifiers)
                Sleep 30
            } else if RegExMatch(token, "i)^hold\((.+)\)$", &m) {
                if (holdOwner = "") {
                    DoPress(Trim(m[1]), 30, false, false, triggerModifiers)
                } else {
                    heldKeys := Trim(m[1])
                    HoldKeyDown(heldKeys, triggerModifiers)
                    state := chordHolds[holdOwner]
                    state["keys"] := (state["keys"] = "") ? heldKeys : state["keys"] " " heldKeys
                }
            } else if RegExMatch(token, "i)^state\((.+)\)$", &m) {
                SendAppEvent("state_triggered", "", Trim(m[1]))
            } else if RegExMatch(token, "i)^sleep\((\d+)\)$", &m) {
                Sleep Integer(m[1])
            } else if RegExMatch(token, "i)^send\((.+)\)$", &m) {
                SendBehaviorText(m[1])
            }
        }
    } finally {
        if locked
            BlockInput "MouseMoveOff"
    }
}

SendBehaviorText(text) {
    global behaviorClipboardBackup, behaviorClipboardPending, behaviorClipboardSequence
    try {
        activeExe := WinGetProcessName("A")
    } catch Error {
        activeExe := ""
    }

    ; WhatsApp's packaged editor does not reliably consume the KEYEVENTF_UNICODE packets used
    ; by Text mode. Paste through the clipboard there, preserving every clipboard format; other
    ; applications keep the immediate Unicode-packet path.
    if (activeExe != "WhatsApp.Root.exe" && activeExe != "WhatsApp.exe") {
        SendInput("{Text}" text)
        return
    }

    if !behaviorClipboardPending {
        behaviorClipboardBackup := ClipboardAll()
        behaviorClipboardPending := true
        behaviorClipboardSequence := DllCall("GetClipboardSequenceNumber", "UInt")
    }

    try {
        A_Clipboard := text
        if !ClipWait(0.5) {
            RestoreBehaviorClipboard()
            return
        }
        behaviorClipboardSequence := DllCall("GetClipboardSequenceNumber", "UInt")
        SendInput("^v")
        ; WhatsApp reads clipboard data asynchronously after handling the paste shortcut. A
        ; one-shot timer keeps this hotkey available for another character while it does so.
        SetTimer RestoreBehaviorClipboard, -500
    } catch Error as err {
        RestoreBehaviorClipboard()
        throw err
    }
}

RestoreBehaviorClipboard(*) {
    global behaviorClipboardBackup, behaviorClipboardPending, behaviorClipboardSequence
    if !behaviorClipboardPending
        return

    ; Do not overwrite clipboard content the user copied after this macro started.
    sequenceUnchanged := behaviorClipboardSequence = DllCall("GetClipboardSequenceNumber", "UInt")
    behaviorClipboardPending := false
    behaviorClipboardSequence := 0
    if sequenceUnchanged
        try A_Clipboard := behaviorClipboardBackup
    behaviorClipboardBackup := ""
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

DoPress(keyStr, holdMs := 30, spin := false, preserveHooks := false, triggerModifiers := "") {
    ; SendEvent keeps AutoHotkey's physical-input hooks installed. Repeat taps need those
    ; hooks throughout every synthetic input so unrelated physical releases cannot disappear.
    if preserveHooks {
        SetKeyDelay -1, -1
        SetMouseDelay -1
    }
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
    blind := BlindFor(triggerModifiers, mods)
    ; Mouse-button taps always preserve the mouse hook, even outside a repeat. Otherwise a
    ; physical button-up can race the tap's temporary logical-state restoration.
    if ((key = "m1" || key = "m2") && !preserveHooks) {
        preserveHooks := true
        SetKeyDelay -1, -1
        SetMouseDelay -1
    }
    ; If no key was given, the modifier itself is the key to press
    if (key = "") {
        if (mods = "<^")
            DoPressKey("LCtrl", preserveHooks, blind)
        else if (mods = ">^")
            DoPressKey("RCtrl", preserveHooks, blind)
        else if (mods = "^")
            DoPressKey("Ctrl", preserveHooks, blind)
        else if (mods = "<+")
            DoPressKey("LShift", preserveHooks, blind)
        else if (mods = ">+")
            DoPressKey("RShift", preserveHooks, blind)
        else if (mods = "+")
            DoPressKey("Shift", preserveHooks, blind)
        else if (mods = "<!")
            DoPressKey("LAlt", preserveHooks, blind)
        else if (mods = ">!")
            DoPressKey("RAlt", preserveHooks, blind)
        else if (mods = "!")
            DoPressKey("Alt", preserveHooks, blind)
        else if (mods = "<#")
            DoPressKey("LWin", preserveHooks, blind)
        else if (mods = ">#")
            DoPressKey("RWin", preserveHooks, blind)
        else if (mods = "#")
            DoPressKey("LWin", preserveHooks, blind)
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
    ctrlOwned := false
    shiftOwned := false
    altOwned := false
    try {
        ; Acquire inside the protected region so a failed send cannot strand an earlier modifier.
        ctrlOwned := AcquireModifier(ctrlKey, preserveHooks, blind)
        shiftOwned := AcquireModifier(shiftKey, preserveHooks, blind)
        altOwned := AcquireModifier(altKey, preserveHooks, blind)
        if (key = "m1" || key = "m2") {
            phys := (key = "m1") ? "LButton" : "RButton"
            wasHeld := GetKeyState(phys, "P")
            if mirroredMouseDown.Has(phys)
                ReleaseMirroredMouse(phys, preserveHooks)
            else if wasHeld
                SendKeyEvents(blind "{" phys " Up}", preserveHooks)
            Sleep 30
            SendOwnedKeyDown(phys, preserveHooks, "Down", blind)
            try {
                Sleep 30
            } finally {
                SendOwnedKeyUp(phys, preserveHooks, blind)
            }
            ; With the hook preserved, do not restore a logical mouse-down after the user
            ; physically released the button during this tap. The mirror remains owner-tracked
            ; so a release racing this check is still observed and forwarded as an up.
            if (wasHeld && GetKeyState(phys, "P"))
                MirrorPhysicalMouseDown(phys, preserveHooks)
            return
        }
        if (mods != "")
            Sleep 20
        SendOwnedKeyDown(key, preserveHooks, "Down", blind)
        try {
            if (spin)
                SpinHold(holdMs)  ; precise sub-Sleep-granularity hold for the repeat tap
            else
                Sleep holdMs
        } finally {
            SendOwnedKeyUp(key, preserveHooks, blind)  ; always release, even if the hold throws
        }
        if (mods != "")
            Sleep 20
    } finally {
        ReleaseModifier(altKey, altOwned, preserveHooks, blind)
        ReleaseModifier(shiftKey, shiftOwned, preserveHooks, blind)
        ReleaseModifier(ctrlKey, ctrlOwned, preserveHooks, blind)
    }
}

SendKeyEvents(keys, preserveHooks) {
    if preserveHooks
        SendEvent(keys)
    else
        SendInput(keys)
}

SendOwnedKeyDown(keyName, preserveHooks, downMode := "Down", blind := "{Blind}") {
    global syntheticDown
    ; Record ownership first so an ExitApp arriving between these statements can only cause
    ; a harmless extra key-up, never leave an unowned synthetic key-down behind.
    syntheticDown[keyName] := true
    SendKeyEvents(blind "{" keyName " " downMode "}", preserveHooks)
}

SendOwnedKeyUp(keyName, preserveHooks, blind := "{Blind}") {
    global syntheticDown
    SendKeyEvents(blind "{" keyName " Up}", preserveHooks)
    if syntheticDown.Has(keyName)
        syntheticDown.Delete(keyName)
}

ReleaseSyntheticHeld(*) {
    global syntheticDown
    SetKeyDelay -1, -1
    SetMouseDelay -1
    ; Never cancel a real held input. Its eventual hardware key-up will clear Windows' logical
    ; state; everything not physically held is state owned solely by this script and must go up.
    for keyName in syntheticDown {
        if !GetKeyState(keyName, "P")
            SendEvent("{Blind}{" keyName " Up}")
    }
    syntheticDown.Clear()
}

MirrorPhysicalMouseDown(keyName, preserveHooks) {
    global mirroredMouseDown
    mirroredMouseDown[keyName] := true
    SendOwnedKeyDown(keyName, preserveHooks)
    SetTimer CheckMirroredMouseReleases, 10
}

ReleaseMirroredMouse(keyName, preserveHooks := true) {
    global mirroredMouseDown
    SendOwnedKeyUp(keyName, preserveHooks)
    if mirroredMouseDown.Has(keyName)
        mirroredMouseDown.Delete(keyName)
}

CheckMirroredMouseReleases(*) {
    global mirroredMouseDown
    released := []
    for keyName in mirroredMouseDown {
        if !GetKeyState(keyName, "P")
            released.Push(keyName)
    }
    for keyName in released
        ReleaseMirroredMouse(keyName)
    if (mirroredMouseDown.Count = 0)
        SetTimer CheckMirroredMouseReleases, 0
}

DoPressKey(keyName, preserveHooks := false, blind := "{Blind}") {
    if GetKeyState(keyName)
        return
    SendOwnedKeyDown(keyName, preserveHooks, "DownTemp", blind)
    try {
        Sleep 30
    } finally {
        if !GetKeyState(keyName, "P")
            SendOwnedKeyUp(keyName, preserveHooks, blind)
    }
}

; Acquire only modifier state that this action owns. A macro must never release a modifier
; which was already held by the user or by the Copilot remap.
AcquireModifier(modKey, preserveHooks := false, blind := "{Blind}") {
    if (modKey = "" || GetKeyState(modKey))
        return false
    SendOwnedKeyDown(modKey, preserveHooks, "DownTemp", blind)
    return true
}

ReleaseModifier(modKey, owned, preserveHooks := false, blind := "{Blind}") {
    if !owned
        return
    ; If the user physically pressed it while the action ran, their eventual physical key-up
    ; owns the release; sending one here would cancel the real held modifier.
    if !GetKeyState(modKey, "P")
        SendOwnedKeyUp(modKey, preserveHooks, blind)
}

; True while every physical keyboard key or mouse button in a recorded chord remains down.
; #HotIf uses this for the keys before the final activating key, and held/repeat actions use
; the full chord so releasing any member ends the action.
TriggerChordHeld(chordKeys) {
    for keyName in StrSplit(chordKeys, " ") {
        if (keyName != "" && !GetKeyState(keyName, "P"))
            return false
    }
    return chordKeys != ""
}

; A held remap. Its trigger owns one reference to every output until the primary key-up
; hotkey or the independent physical-release monitor observes that the trigger is no longer
; held. Reference counts let several triggers safely hold the same output. This mirrors how
; AutoHotkey implements native key remapping:
;   - Blind exclusions suppress trigger modifiers only for the output key event, then restore
;     them so a held modifier can activate another hotkey without being pressed again.
;   - Each physical trigger owns one output reference. Hardware auto-repeat is ignored so a held
;     output remains one uninterrupted down rather than repeating while the trigger stays held.
; Global key-up handlers release owners independently of profile enabled/focus conditions.
HoldKeyDown(keyStr, triggerModifiers := "") {
    SetKeyDelay -1, -1
    SetMouseDelay -1
    blind := BlindFor(triggerModifiers, KeyModifierSymbols(keyStr))
    keys := HoldKeyList(keyStr)
    for index, keyName in keys {
        AcquireHeldOutput(keyName, blind)
        ; Preserve the configured order as distinct transitions: the first output remains down
        ; before the next one is pressed. Some games miss simultaneous synthetic mouse downs.
        if (index < keys.Length)
            Sleep 30
    }
}

HoldKeyUp(keyStr, triggerModifiers := "") {
    SetKeyDelay -1, -1
    SetMouseDelay -1
    keys := HoldKeyList(keyStr)
    blind := BlindFor(triggerModifiers, KeyModifierSymbols(keyStr))
    ReleaseHeldOutputsTogether(keys, blind)
}

AcquireHeldOutput(keyName, blind) {
    global heldOutputCounts
    count := heldOutputCounts.Has(keyName) ? heldOutputCounts[keyName] : 0
    heldOutputCounts[keyName] := count + 1
    if (count = 0)
        SendOwnedKeyDown(keyName, true, "DownR", blind)
}

ReleaseHeldOutputsTogether(keys, blind) {
    global heldOutputCounts, syntheticDown
    releases := []
    index := keys.Length
    while (index >= 1) {
        keyName := keys[index]
        if heldOutputCounts.Has(keyName) {
            count := heldOutputCounts[keyName]
            if (count > 1)
                heldOutputCounts[keyName] := count - 1
            else {
                heldOutputCounts.Delete(keyName)
                releases.Push(keyName)
            }
        }
        index--
    }
    if (releases.Length = 0)
        return

    ; Send every releasable output in one event batch with all inter-event delays disabled.
    ; Windows still represents individual ups, but no sleep or separate Send call can split them.
    events := blind
    for keyName in releases
        events .= "{" keyName " Up}"
    SendKeyEvents(events, true)
    for keyName in releases {
        if syntheticDown.Has(keyName)
            syntheticDown.Delete(keyName)
    }
}

; Track every single-key or multi-input trigger independently. Global key-up handlers call
; ReleaseTriggerHolds for every physical member, so releasing any member ends its whole chord.
HoldChordDown(owner, keyStr, chordKeys, triggerModifiers := "") {
    global chordHolds
    ; Keep a trigger-up buffered until the ordered down sequence is complete. Otherwise a quick
    ; release during its inter-key delay could release the first output before the second is owned.
    previousCritical := Critical()
    try {
        ; Hardware auto-repeat invokes the press hotkey again while the trigger stays down.
        ; The existing owner represents that uninterrupted hold, so emit no additional downs.
        if chordHolds.Has(owner)
            return
        chordHolds[owner] := Map(
            "keys", keyStr,
            "chord", chordKeys,
            "modifiers", triggerModifiers
        )
        HoldKeyDown(keyStr, triggerModifiers)
    } finally {
        Critical previousCritical
    }
}

; Run a mixed behavior once per physical press while letting its hold(...) actions share the
; normal trigger-owned release path. Register ownership before the first action so key-up and
; hardware auto-repeat cannot race a behavior that is still executing.
HoldBehaviorDown(owner, behavior, chordKeys, triggerModifiers := "", configuredExe := "") {
    global chordHolds
    previousCritical := Critical()
    try {
        if chordHolds.Has(owner)
            return
        chordHolds[owner] := Map(
            "keys", "",
            "chord", chordKeys,
            "modifiers", triggerModifiers
        )
        ; Keep both input hooks installed while a preceding press(...) is emitted. SendEvent at
        ; the hotkey's default input level cannot retrigger this script's hook hotkeys, and the
        ; hook retains the trigger's physical-down state until its real release.
        ExecuteBehavior(behavior, triggerModifiers, configuredExe, owner, true)
    } catch Error as err {
        HoldChordUp(owner)
        throw err
    } finally {
        Critical previousCritical
    }
}

HoldChordUp(owner) {
    global chordHolds
    if !chordHolds.Has(owner)
        return
    state := chordHolds[owner]
    chordHolds.Delete(owner)
    if (state["keys"] != "")
        HoldKeyUp(state["keys"], state["modifiers"])
}

ReleaseTriggerHolds(triggerKey) {
    global chordHolds
    released := []
    for owner, state in chordHolds {
        for chordKey in StrSplit(state["chord"], " ") {
            if (chordKey = triggerKey) {
                released.Push(owner)
                break
            }
        }
    }
    for owner in released
        HoldChordUp(owner)
}

HoldKeyList(keyStr) {
    held := []
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
        else {
            if RegExMatch(part, "i)^f(\d+)$", &m)
                part := "F" m[1]
            held.Push(PhysKey(part))
        }
    }
    return held
}

KeyModifierSymbols(keyStr) {
    symbols := ""
    for part in StrSplit(Trim(StrLower(keyStr)), " ") {
        if (part = "ctrl" || part = "lctrl" || part = "rctrl")
            symbols .= "^"
        else if (part = "shift" || part = "lshift" || part = "rshift")
            symbols .= "+"
        else if (part = "alt" || part = "lalt" || part = "ralt")
            symbols .= "!"
        else if (part = "win" || part = "lwin" || part = "rwin")
            symbols .= "#"
    }
    return symbols
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
RepeatHold(keys, interval, triggerKey, triggerChord, exe, holdMs, enabledKey, useWindowsState, triggerModifiers := "") {
    global enabled, repeatDown
    ; Stop as soon as EITHER release signal fires: the key-up hotkey clearing repeatDown, or
    ; the independent Windows state poll. GetKeyState(..., "P") reads AutoHotkey's hook state,
    ; so it can remain stale if the hook misses a release while SendInput has it uninstalled.
    ; GetAsyncKeyState reads the current OS state instead of that same cached hook state. It is
    ; skipped when the repeated output includes the trigger itself, because that synthetic input
    ; legitimately changes the OS state; the key-up latch and hook state still cover that case.
    SetTimer CheckRepeatReleases, 10
    while ((repeatDown.Has(triggerKey) && repeatDown[triggerKey]) && TriggerChordPhysicallyDown(triggerChord, useWindowsState)) {
        ; Toggling the profile off or leaving its target app ends this press. Retaining the
        ; loop across a focus change lets a missed key-up leave a stale repeat ready to resume.
        if (!enabled[enabledKey] || (exe != "" && !WinActive("ahk_exe " exe))) {
            break
        }
        start := A_TickCount
        ; Every repeat preserves both physical-input hooks. Besides keeping this trigger's
        ; release visible, that prevents an unrelated mouse/key release from being displaced.
        DoPress(keys, holdMs, true, true, triggerModifiers)
        elapsed := A_TickCount - start
        if (elapsed < interval)
            Sleep interval - elapsed
    }
    repeatDown[triggerKey] := false
}

TriggerChordPhysicallyDown(triggerChord, useWindowsState) {
    for triggerKey in StrSplit(triggerChord, " ") {
        if (triggerKey != "" && !TriggerPhysicallyDown(triggerKey, useWindowsState))
            return false
    }
    return triggerChord != ""
}

TriggerPhysicallyDown(triggerKey, useWindowsState) {
    static mouseButtonsSwapped := DllCall("GetSystemMetrics", "Int", 23)
    if !GetKeyState(triggerKey, "P")
        return false
    if !useWindowsState
        return true
    vk := GetKeyVK(triggerKey)
    if !vk
        return false
    ; AutoHotkey's L/RButton names mean primary/secondary, while GetAsyncKeyState's virtual
    ; keys mean physical left/right. Translate them when Windows has swapped the mouse buttons.
    if mouseButtonsSwapped {
        if (triggerKey = "LButton")
            vk := 0x02
        else if (triggerKey = "RButton")
            vk := 0x01
    }
    return (DllCall("GetAsyncKeyState", "Int", vk, "Short") & 0x8000) != 0
}"###;

#[cfg(test)]
mod tests {
    use super::{
        generate_combined_script, repeat_output_uses_trigger, trigger_chord_keys,
        trigger_modifier_symbols, ArmedProfile,
    };
    use crate::config::{BehaviorEvent, Hotkey, Profile, Script};

    fn profile_with_hotkey(trigger: &str, behavior: &str) -> Profile {
        Profile {
            id: "test-profile".to_string(),
            name: "Test".to_string(),
            kind: "hotkeys".to_string(),
            armed: true,
            parent_id: None,
            hotkeys: vec![Hotkey {
                name: String::new(),
                trigger: trigger.to_string(),
                behavior: behavior.to_string(),
                state_id: None,
            }],
            events: Vec::new(),
            states: Vec::new(),
            overlay_items: Vec::new(),
            overlay_triggers: Vec::new(),
            overlay_groups: Vec::new(),
            scripts: Vec::new(),
            overlay_disabled: false,
            toggle_hotkeys_key: None,
            toggle_overlay_key: None,
        }
    }

    #[test]
    fn altgr_arrow_press_excludes_modifiers_without_releasing_the_held_trigger() {
        let mut profile = profile_with_hotkey("ralt left", "press(Home)");
        profile.hotkeys.push(Hotkey {
            name: String::new(),
            trigger: "ralt right".to_string(),
            behavior: "press(End)".to_string(),
            state_id: None,
        });
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "*",
        }];
        let script = generate_combined_script(&armed);

        assert!(script.contains(
            "$*>!left:: {\n    ExecuteBehavior(\"press(Home)\", \"^!\", \"*\")"
        ));
        assert!(script.contains(
            "$*>!right:: {\n    ExecuteBehavior(\"press(End)\", \"^!\", \"*\")"
        ));
        assert!(script.contains("return \"{Blind\" triggerModifiers \"}\""));
        assert!(!script.contains("ReleaseTriggerModifiers"));
        assert_eq!(trigger_modifier_symbols("ralt left"), "^!");
        assert_eq!(trigger_modifier_symbols("lctrl rshift lalt left"), "^+!");
    }

    #[test]
    fn behavior_commands_receive_the_profile_target() {
        let profile = profile_with_hotkey("f11", "borderless;stretch;killprocess");
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "Game.exe",
        }];
        let script = generate_combined_script(&armed);

        assert!(script.contains(
            "ExecuteBehavior(\"borderless;stretch;killprocess\", \"\", \"Game.exe\")"
        ));
        assert!(script.contains("SendBehaviorCommand(\"borderless\", configuredExe)"));
        assert!(script.contains("SendBehaviorCommand(\"killprocess\", configuredExe)"));
        assert!(script.contains("SendBehaviorCommand(\"stretch\", configuredExe)"));
        assert!(script.contains("targetExe := WinGetProcessName(\"A\")"));
    }

    #[test]
    fn regular_keys_and_mouse_buttons_can_be_held_as_trigger_chords() {
        let mut profile = profile_with_hotkey("x y", "press(Home)");
        profile.hotkeys.push(Hotkey {
            name: String::new(),
            trigger: "XButton1 y".to_string(),
            behavior: "press(End)".to_string(),
            state_id: None,
        });
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "*",
        }];
        let script = generate_combined_script(&armed);
        let x = &trigger_chord_keys("x")[0];
        let y = &trigger_chord_keys("y")[0];

        assert!(script.contains(&format!(
            "#HotIf enabled[\"test-profile\"] && TriggerChordHeld(\"{x}\")\n${y}::"
        )));
        assert!(script.contains(&format!(
            "#HotIf enabled[\"test-profile\"] && TriggerChordHeld(\"xbutton1\")\n${y}::"
        )));
        assert!(script.contains("TriggerChordHeld(chordKeys)"));
    }

    #[test]
    fn held_and_repeating_chords_end_when_any_trigger_key_is_released() {
        let mut profile = profile_with_hotkey("x y", "hold(rctrl)");
        profile.hotkeys.push(Hotkey {
            name: String::new(),
            trigger: "XButton1 f4".to_string(),
            behavior: "repeat(f5, 100, 6)".to_string(),
            state_id: None,
        });
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "*",
        }];
        let script = generate_combined_script(&armed);
        let x = &trigger_chord_keys("x")[0];
        let y = &trigger_chord_keys("y")[0];

        assert!(script.contains(&format!(
            "HoldChordDown(\"test-profile:x y\", \"rctrl\", \"{x} {y}\", \"\")"
        )));
        assert!(script.contains(&format!(
            "~*{x} up:: {{\n    ReleaseTriggerHolds(\"{x}\")"
        )));
        assert!(script.contains(&format!(
            "~*{y} up:: {{\n    ReleaseTriggerHolds(\"{y}\")"
        )));
        assert!(script.contains("repeatChord[\"F4\"] := \"xbutton1 F4\""));
        assert!(script.contains(
            "RepeatHold(\"f5\", 100, \"F4\", \"xbutton1 F4\", \"\", 6, \"test-profile\", true, \"\")"
        ));
        assert!(script.contains("TriggerChordPhysicallyDown(triggerChord, useWindowsState)"));
    }

    #[test]
    fn unmodified_hold_macros_accept_and_preserve_unrelated_modifiers() {
        for behavior in ["hold(XButton1 LButton)", "press(XButton1);hold(LButton)"] {
            let mut profile = profile_with_hotkey("XButton1", behavior);
            for trigger in ["shift XButton1", "rctrl o", "rctrl shift o"] {
                profile.hotkeys.push(Hotkey {
                    name: String::new(),
                    trigger: trigger.to_string(),
                    behavior: "hold(F5)".to_string(),
                    state_id: None,
                });
            }
            let armed = [ArmedProfile {
                siblings: std::slice::from_ref(&profile),
                profile: &profile,
                exe: "Game.exe",
            }];
            let script = generate_combined_script(&armed);

            assert!(script.contains("$*xbutton1:: {\n    Hold"));
            assert!(script.contains("\"xbutton1\", \"\""));
            assert!(script.contains("$+xbutton1:: {\n    HoldChordDown"));
            for trigger in ["rctrl o", "rctrl shift o"] {
                assert!(script.contains(&format!("{}:: {{", trigger_to_key(trigger))));
                assert!(!trigger_to_key(trigger).contains('*'));
            }
            assert!(script.contains("~*xbutton1 up:: {"));
            assert!(script.contains("ReleaseTriggerHolds(\"xbutton1\")"));
            assert!(script.contains("blind := BlindFor(triggerModifiers, KeyModifierSymbols(keyStr))"));
            assert!(script.contains("return \"{Blind\" triggerModifiers \"}\""));
            assert!(script.contains("SendOwnedKeyDown(keyName, true, \"DownR\", blind)"));
            assert!(script.contains("if chordHolds.Has(owner)\n            return"));
        }
    }

    #[test]
    fn hold_actions_keep_every_recorded_regular_key_and_mouse_button() {
        let mut profile = profile_with_hotkey("x", "hold(XButton1 RButton)");
        profile.hotkeys.push(Hotkey {
            name: String::new(),
            trigger: "z".to_string(),
            behavior: "hold(XButton1 RButton)".to_string(),
            state_id: None,
        });
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "*",
        }];
        let script = generate_combined_script(&armed);
        let hold_key_list = script
            .split_once("HoldKeyList(keyStr) {")
            .unwrap()
            .1
            .split_once("KeyModifierSymbols(keyStr) {")
            .unwrap()
            .0;
        let x = &trigger_chord_keys("x")[0];
        let z = &trigger_chord_keys("z")[0];

        assert!(script.contains(&format!(
            "HoldChordDown(\"test-profile:x\", \"XButton1 RButton\", \"{x}\", \"\")"
        )));
        assert!(script.contains(&format!(
            "HoldChordDown(\"test-profile:z\", \"XButton1 RButton\", \"{z}\", \"\")"
        )));
        assert!(script.contains(&format!(
            "~*{x} up:: {{\n    ReleaseTriggerHolds(\"{x}\")"
        )));
        assert!(script.contains(&format!(
            "~*{z} up:: {{\n    ReleaseTriggerHolds(\"{z}\")"
        )));
        assert!(hold_key_list.contains("held.Push(PhysKey(part))"));
        assert!(!hold_key_list.contains("key := part"));
        assert!(script.contains("global heldOutputCounts := Map()"));
        assert!(script.contains("heldOutputCounts[keyName] := count + 1"));
        assert!(script.contains("if (count > 1)"));
        assert!(script.contains(
            "if chordHolds.Has(owner)\n            return"
        ));
        assert!(script.contains("previousCritical := Critical()"));
        assert!(script.contains("} finally {\n        Critical previousCritical"));
        assert!(!script.contains("ReassertHeldOutputs"));
        assert!(script.contains(
            "AcquireHeldOutput(keyName, blind)\n        ; Preserve the configured order"
        ));
        assert!(script.contains("if (index < keys.Length)\n            Sleep 30"));
        assert!(script.contains("ReleaseHeldOutputsTogether(keys, blind)"));
        assert!(script.contains(
            "for keyName in releases\n        events .= \"{\" keyName \" Up}\"\n    SendKeyEvents(events, true)"
        ));
        assert!(script.contains("ReleaseTriggerHolds(triggerKey)"));
        assert!(!script.contains("CheckChordHoldReleases"));
    }

    #[test]
    fn pressing_the_trigger_then_holding_another_button_waits_for_physical_trigger_up() {
        let profile = profile_with_hotkey("XButton2", "press(XButton2);hold(RButton)");
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "*",
        }];
        let script = generate_combined_script(&armed);
        let trigger = &trigger_chord_keys("XButton2")[0];

        assert!(script.contains(&format!(
            "HoldBehaviorDown(\"test-profile:XButton2\", \"press(XButton2);hold(RButton)\", \"{trigger}\", \"\", \"*\")"
        )));
        assert!(script.contains(&format!(
            "~*{trigger} up:: {{\n    if GetKeyState(\"{trigger}\", \"P\")\n        return\n    ReleaseTriggerHolds(\"{trigger}\")"
        )));
        assert!(script.contains(
            "if chordHolds.Has(owner)\n            return\n        chordHolds[owner] := Map("
        ));
        assert!(script.contains(
            "ExecuteBehavior(behavior, triggerModifiers, configuredExe, owner, true)"
        ));
        assert!(script.contains(
            "DoPress(Trim(k), 30, false, preserveHooks, triggerModifiers)"
        ));
        assert!(script.contains(
            "HoldKeyDown(heldKeys, triggerModifiers)\n                    state := chordHolds[holdOwner]"
        ));
    }

    #[test]
    fn lifecycle_events_and_launch_scripts_share_the_seeded_target_monitor() {
        let mut profile = profile_with_hotkey("f11", "press(F11)");
        profile.kind = "events".to_string();
        profile.hotkeys.clear();
        profile.events = vec![
            BehaviorEvent {
                event: "window_ready".to_string(),
                behavior: "borderless".to_string(),
            },
            BehaviorEvent {
                event: "focus_gained".to_string(),
                behavior: "press(F5)".to_string(),
            },
        ];
        profile.scripts.push(Script {
            id: "launch-script".to_string(),
            name: "Launch".to_string(),
            enabled: true,
            trigger: "launch".to_string(),
            hotkey: String::new(),
            language: "python".to_string(),
            source: "code".to_string(),
            code: String::new(),
            path: String::new(),
        });
        let armed = [ArmedProfile {
            siblings: std::slice::from_ref(&profile),
            profile: &profile,
            exe: "Game.exe",
        }];
        let script = generate_combined_script(&armed);

        assert!(script.contains(
            "eventProfiles.Push(Map(\"id\", \"test-profile\", \"exe\", \"game.exe\", \"target\", \"Game.exe\""
        ));
        assert!(script.contains("\"window_ready\", \"borderless\""));
        assert!(script.contains("\"focus_gained\", \"press(F5)\""));
        assert!(script.contains("\"launchScripts\", [\"launch-script\"]"));
        assert!(script.contains("if !state[\"initialized\"] {\n            state[\"initialized\"] := true"));
        assert!(script.contains("DispatchTargetEvent(exe, \"app_started\")"));
        assert!(script.contains("QueueBehaviorEvent(events[eventName], profile[\"target\"])"));
        assert!(script.contains("SetTimer SyncRuntime, 200\nSyncRuntime()"));
    }

    #[test]
    fn combined_script_keeps_copilot_recovery_without_profiles() {
        let script = generate_combined_script(&[]);

        assert!(script.contains("$*SC06E::"));
        assert!(script.contains(
            "SendCopilotKeys(\"{Blind}{LShift up}{LWin up}{RCtrl downR}\")"
        ));
        assert!(!script.contains("SendCopilotKeys(\"{Blind}{LCtrl downR}\")"));
        assert!(script.contains(
            "previousSendLevel := SendLevel(1)\n    try SendEvent(keys)\n    finally SendLevel(previousSendLevel)"
        ));
        assert!(script.contains(
            "copilotCtrlReleasePending := true\n        SetTimer CheckCopilotRelease, 10"
        ));
        assert!(script.contains(
            "if !copilotCtrlReleasePending {\n        SetTimer CheckCopilotRelease, 0\n        return"
        ));
        assert!(!script.contains("CopilotKeyPhysicallyDown"));
        assert!(!script.contains("copilotCtrlHeld && !"));
        assert!(script.contains(
            "InstallMouseHook true, true\n    ; If the old hook missed Copilot's key-up"
        ));
        assert!(script.contains("#HotIf copilotState = \"waiting\"\n$*LShift::"));
        assert_eq!(script.matches("$*LShift::").count(), 1);
        assert!(!script.contains("$*LShift up::"));
        assert!(script.contains(
            "AcquireModifier(modKey, preserveHooks := false, blind := \"{Blind}\")"
        ));
        assert!(script.contains("if (modKey = \"\" || GetKeyState(modKey))"));
        assert!(script.contains("if !GetKeyState(modKey, \"P\")"));
        assert!(!script.contains("SendModState("));
        assert!(!script.contains("A_TimeIdleKeyboard"));
        assert!(script.contains("SendBehaviorText(m[1])"));
        assert!(script.contains("activeExe != \"WhatsApp.Root.exe\""));
        assert!(script.contains("behaviorClipboardBackup := ClipboardAll()"));
        assert!(script.contains("SetTimer RestoreBehaviorClipboard, -500"));
        assert!(script.contains("OnExit RestoreBehaviorClipboard"));
    }

    #[test]
    fn repeat_release_check_uses_independent_state_when_safe() {
        let script = generate_combined_script(&[]);

        assert!(script.contains("TriggerChordPhysicallyDown(triggerChord, useWindowsState)"));
        assert!(script.contains("DllCall(\"GetAsyncKeyState\", \"Int\", vk, \"Short\") & 0x8000"));
        assert!(script.contains("&& TriggerChordPhysicallyDown(triggerChord, useWindowsState)"));
        assert!(script.contains("DllCall(\"GetSystemMetrics\", \"Int\", 23)"));
        assert!(script.contains("DoPress(keys, holdMs, true, true, triggerModifiers)"));
        assert!(script.contains("if preserveHooks\n        SendEvent(keys)"));
        assert!(script.contains("SetKeyDelay -1, -1"));
        assert!(script.contains("SetMouseDelay -1"));
        assert!(script.contains("SetTimer CheckRepeatReleases, 10"));
        assert!(!script.contains("&& GetKeyState(triggerKey, \"P\")"));
    }

    #[test]
    fn repeat_ends_when_its_profile_loses_authority() {
        let script = generate_combined_script(&[]);

        assert!(script.contains(
            "if (!enabled[enabledKey] || (exe != \"\" && !WinActive(\"ahk_exe \" exe))) {\n            break"
        ));
        assert!(!script.contains("so the repeat can't leak into other apps"));
    }

    #[test]
    fn generated_script_releases_owned_synthetic_input_on_exit() {
        let script = generate_combined_script(&[]);

        assert!(script.contains("global syntheticDown := Map()"));
        assert!(script.contains("OnExit ReleaseSyntheticHeld"));
        assert!(script.contains(
            "OnExit ReleaseSyntheticHeld\nOnExit ReleaseCopilotHeld\nOnExit HideOverlayOnExit"
        ));
        assert!(script.contains("syntheticDown[keyName] := true"));
        assert!(script.contains("if !GetKeyState(keyName, \"P\")\n            SendEvent"));
        assert!(script.contains("AcquireHeldOutput(k, blind)"));
        assert!(script.contains("ReleaseHeldOutputsTogether(keys, blind)"));
        assert!(script.contains("global mirroredMouseDown := Map()"));
        assert!(script.contains("MirrorPhysicalMouseDown(phys, preserveHooks)"));
        assert!(script.contains("SetTimer CheckMirroredMouseReleases, 10"));
    }

    #[test]
    fn repeat_output_trigger_comparison_ignores_modifiers_and_case() {
        assert!(repeat_output_uses_trigger("XButton1", "shift xbutton1"));
        assert!(repeat_output_uses_trigger("f4", "CTRL F4"));
        assert!(repeat_output_uses_trigger("ctrl f4", "ctrl"));
        assert!(repeat_output_uses_trigger("lctrl f4", "ctrl"));
        assert!(repeat_output_uses_trigger("m1", "LButton"));
        assert!(!repeat_output_uses_trigger("f5", "ctrl f4"));
        assert!(!repeat_output_uses_trigger("rctrl f5", "lctrl f4"));
    }
}
