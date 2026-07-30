use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// The db.json schema version. Bumped when the shape changes so `load_db` can migrate
/// older files. v2 = scope-becomes-folder + profile-owns-exe/scripts/arming.
pub const CURRENT_DB_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Database {
    #[serde(default)]
    pub version: u32,
    #[serde(rename = "scopes", alias = "games")]
    pub games: Vec<Game>,
    pub settings: Settings,
}

/// A Scope is now just a named folder that groups profiles. Everything else (the target
/// executable, hotkeys, scripts, overlay, toggle keys) lives on the Profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

/// A user script owned by a profile. It runs either when its hotkey is pressed (while the
/// profile's app is focused) or when the profile's app is launched. The body is either inline
/// code typed by the user or a path to a script file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Script {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_script_enabled")]
    pub enabled: bool,
    /// "hotkey" | "launch"
    pub trigger: String,
    /// The key combo when `trigger` is "hotkey".
    #[serde(default)]
    pub hotkey: String,
    /// "python" | "autohotkey". Missing on older saved scripts, which were always Python.
    #[serde(default = "default_script_language")]
    pub language: String,
    /// "code" | "path"
    pub source: String,
    /// Inline Python or AutoHotkey code when `source` is "code".
    #[serde(default)]
    pub code: String,
    /// Path to a `.py` or `.ahk` file when `source` is "path".
    #[serde(default)]
    pub path: String,
}

fn default_script_enabled() -> bool {
    true
}

fn default_script_language() -> String {
    "python".to_string()
}

fn default_profile_kind() -> String {
    "hotkeys".to_string()
}

/// A Profile targets one executable and owns everything that runs against it: hotkeys,
/// scripts, overlay, states. When `armed`, its hotkeys/scripts are live automatically while
/// its `exe` is the focused window. `exe == "*"` means "any app / always". Profiles with an
/// empty `exe` are simply inert (never focused-matched).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// "hotkeys" | "scripts" | "overlay" — what the profile is for (drives the editor UI).
    #[serde(default = "default_profile_kind")]
    pub kind: String,
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub armed: bool,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub hotkeys: Vec<Hotkey>,
    #[serde(default)]
    pub states: Vec<ProfileState>,
    #[serde(default)]
    pub overlay_items: Vec<OverlayItem>,
    #[serde(default)]
    pub overlay_triggers: Vec<OverlayTrigger>,
    #[serde(default)]
    pub overlay_groups: Vec<OverlayGroup>,
    #[serde(default)]
    pub scripts: Vec<Script>,
    #[serde(default)]
    pub overlay_disabled: bool,
    #[serde(default)]
    pub toggle_hotkeys_key: Option<String>,
    #[serde(default)]
    pub toggle_overlay_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayGroup {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OverlayConfig {
    #[serde(default)]
    pub items: Vec<OverlayItem>,
    #[serde(default)]
    pub states: Vec<ProfileState>,
    #[serde(default)]
    pub hotkeys: Vec<OverlayHotkeyStateBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileState {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayHotkeyStateBinding {
    pub trigger: String,
    #[serde(default)]
    pub state_id: Option<String>,
}

fn default_overlay_display_mode() -> String {
    "always".to_string()
}

fn default_timer_color() -> String {
    "#ffffff".to_string()
}

fn default_timer_font_size() -> u32 {
    22
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayItem {
    Timer {
        id: String,
        #[serde(default)]
        name: String,
        x: f64,
        y: f64,
        duration_ms: u64,
        #[serde(default = "default_timer_color")]
        color: String,
        #[serde(default = "default_timer_font_size")]
        font_size: u32,
        #[serde(default)]
        state_id: Option<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        timer_state_id: Option<String>,
        #[serde(default)]
        visible_when: Option<String>,
        #[serde(default = "default_overlay_display_mode")]
        display_mode: String,
        #[serde(default)]
        hotkey_trigger: Option<String>,
        #[serde(default)]
        show_duration_ms: Option<u64>,
        #[serde(default)]
        timer_key: Option<String>,
    },
    Icon  {
        id: String,
        #[serde(default)]
        name: String,
        x: f64,
        y: f64,
        w: u32,
        h: u32,
        src: Option<String>,
        #[serde(default)]
        state_id: Option<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        visible_when: Option<String>,
        #[serde(default = "default_overlay_display_mode")]
        display_mode: String,
        #[serde(default)]
        hotkey_trigger: Option<String>,
        #[serde(default)]
        show_duration_ms: Option<u64>,
    },
    Bar   {
        id: String,
        #[serde(default)]
        name: String,
        x: f64,
        y: f64,
        w: u32,
        h: u32,
        color: String,
        max_value: f64,
        #[serde(default)]
        state_id: Option<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        visible_when: Option<String>,
        #[serde(default = "default_overlay_display_mode")]
        display_mode: String,
        #[serde(default)]
        hotkey_trigger: Option<String>,
        #[serde(default)]
        show_duration_ms: Option<u64>,
    },
    Text  {
        id: String,
        #[serde(default)]
        name: String,
        x: f64,
        y: f64,
        font_size: u32,
        color: String,
        content: String,
        #[serde(default)]
        state_id: Option<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        visible_when: Option<String>,
        #[serde(default = "default_overlay_display_mode")]
        display_mode: String,
        #[serde(default)]
        hotkey_trigger: Option<String>,
        #[serde(default)]
        show_duration_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayTrigger {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub hotkey_trigger: Option<String>,
    pub action: String,
    pub state_key: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hotkey {
    #[serde(default)]
    pub name: String,
    pub trigger: String,
    pub behavior: String,
    #[serde(default)]
    pub state_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    pub ahk_exe: String,
    /// Python interpreter used to run scripts. Empty falls back to `python` then the
    /// Windows `py` launcher.
    #[serde(default)]
    pub python_exe: String,
    #[serde(default)]
    pub open_to_tray: bool,
    #[serde(default)]
    pub close_to_tray: bool,
    #[serde(default)]
    pub launch_on_startup: bool,
}

/// The reserved id of the always-present "Global" folder. It is a permanent container (never
/// deleted); whatever profiles live inside it are entirely user-managed.
pub const GLOBAL_FOLDER_ID: &str = "global";

/// Guarantee exactly one Global folder (reserved id `GLOBAL_FOLDER_ID`) always exists. Creates an
/// empty one if missing, and collapses any duplicates (a past bug created more than one) into a
/// single folder, preserving every profile. Its contents are never touched otherwise. Returns
/// true if it changed anything.
pub fn ensure_global_folder(db: &mut Database) -> bool {
    let indices: Vec<usize> = db.games.iter().enumerate()
        .filter(|(_, g)| g.id == GLOBAL_FOLDER_ID)
        .map(|(i, _)| i)
        .collect();

    if indices.is_empty() {
        db.games.push(Game {
            id: GLOBAL_FOLDER_ID.to_string(),
            name: "Global".to_string(),
            image: None,
            profiles: Vec::new(),
        });
        return true;
    }
    if indices.len() == 1 {
        return false;
    }
    // Merge every duplicate Global folder's profiles into the first, then drop the extras.
    let keep = indices[0];
    let merged: Vec<Profile> = indices.iter().flat_map(|&i| db.games[i].profiles.clone()).collect();
    db.games[keep].profiles = merged;
    for &i in indices.iter().skip(1).rev() {
        db.games.remove(i);
    }
    true
}

pub fn load_db(path: &Path) -> Result<Database, String> {
    if !path.exists() {
        let mut db = Database { version: CURRENT_DB_VERSION, ..Default::default() };
        ensure_global_folder(&mut db);
        save_db(path, &db)?;
        return Ok(db);
    }
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    let (mut db, migrated) = if version >= CURRENT_DB_VERSION {
        (serde_json::from_value(value).map_err(|e| e.to_string())?, false)
    } else {
        // Old shape (scope-owned exe/scripts/active_profile) → migrate to profile-owned. Back up
        // the pre-migration file once.
        let legacy: legacy_v1::Database = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let mut backup = path.as_os_str().to_owned();
        backup.push(".v1.bak");
        let _ = std::fs::copy(path, std::path::Path::new(&backup));
        (legacy_v1::migrate(legacy), true)
    };

    // Self-heal the always-present global folder, then persist only when something changed.
    let added_global = ensure_global_folder(&mut db);
    if migrated || added_global {
        save_db(path, &db)?;
    }
    Ok(db)
}

/// Reads the pre-v2 db.json shape (exe/scripts/toggle-keys/overlay_disabled on the Scope, plus
/// `active_profile`) and rewrites it so each Profile owns those fields. See CURRENT_DB_VERSION.
mod legacy_v1 {
    use super::{Database as NewDatabase, Profile, Script, Settings, CURRENT_DB_VERSION};
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Database {
        #[serde(rename = "scopes", alias = "games", default)]
        pub games: Vec<Game>,
        #[serde(default)]
        pub settings: Settings,
    }

    #[derive(Deserialize)]
    pub struct Game {
        pub id: String,
        #[serde(default)]
        pub name: String,
        #[serde(default)]
        pub exe: String,
        #[serde(default)]
        pub image: Option<String>,
        #[serde(default)]
        pub active_profile: Option<String>,
        // Deserialized as the NEW Profile — the new fields simply default and are filled below.
        #[serde(default)]
        pub profiles: Vec<Profile>,
        #[serde(default)]
        pub overlay_disabled: bool,
        #[serde(default)]
        pub toggle_hotkeys_key: Option<String>,
        #[serde(default)]
        pub toggle_overlay_key: Option<String>,
        #[serde(default)]
        pub scripts: Vec<Script>,
    }

    pub fn migrate(legacy: Database) -> NewDatabase {
        let games = legacy
            .games
            .into_iter()
            .map(|g| {
                // Which profile ends up armed: the one that was active; or, for the old global
                // "*" scope (which had no active_profile but always ran its first profile via
                // sync_global_scope), the first profile — otherwise its hotkeys would go dead.
                let armed_id: Option<String> = match g.active_profile.clone() {
                    Some(id) => Some(id),
                    None if g.exe.trim() == "*" => g.profiles.first().map(|p| p.id.clone()),
                    None => None,
                };
                let profiles = g
                    .profiles
                    .into_iter()
                    .map(|mut p| {
                        p.exe = g.exe.clone();
                        p.overlay_disabled = g.overlay_disabled;
                        p.toggle_hotkeys_key = g.toggle_hotkeys_key.clone();
                        p.toggle_overlay_key = g.toggle_overlay_key.clone();
                        p.armed = armed_id.as_deref() == Some(p.id.as_str());
                        // Scope-wide scripts applied to every profile in the scope; give each
                        // profile its own copy with a unique id so they keep firing per-app.
                        p.scripts = g
                            .scripts
                            .iter()
                            .cloned()
                            .map(|mut s| {
                                s.id = format!("{}-{}", p.id, s.id);
                                s
                            })
                            .collect();
                        p
                    })
                    .collect();
                super::Game { id: g.id, name: g.name, image: g.image, profiles }
            })
            .collect();
        NewDatabase { version: CURRENT_DB_VERSION, games, settings: legacy.settings }
    }
}

pub fn save_db(path: &Path, db: &Database) -> Result<(), String> {
    // Stamp the current version on every write so a fresh default or any caller's db is
    // correctly versioned and never re-migrates on the next load.
    let mut db = db.clone();
    db.version = CURRENT_DB_VERSION;
    let content = serde_json::to_string_pretty(&db).map_err(|e| e.to_string())?;
    // Write to a temp file then rename, so an interrupted write can't corrupt db.json.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, content).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn overlay_item_id(item: &OverlayItem) -> String {
    match item {
        OverlayItem::Timer { id, .. }
        | OverlayItem::Icon { id, .. }
        | OverlayItem::Bar { id, .. }
        | OverlayItem::Text { id, .. } => id.clone(),
    }
}

fn resolve_profile_entries<'a, T, F, K>(
    profiles: &'a [Profile],
    profile: &'a Profile,
    select: F,
    key_of: fn(&T) -> K,
    visited: &mut HashSet<&'a str>,
) -> Vec<&'a T>
where
    F: Copy + Fn(&'a Profile) -> &'a [T],
    K: PartialEq,
{
    if !visited.insert(profile.id.as_str()) {
        return vec![];
    }

    let mut resolved = match &profile.parent_id {
        Some(parent_id) => profiles
            .iter()
            .find(|candidate| candidate.id == *parent_id)
            .map(|parent| resolve_profile_entries(profiles, parent, select, key_of, visited))
            .unwrap_or_default(),
        None => vec![],
    };

    for value in select(profile) {
        let key = key_of(value);
        if let Some(slot) = resolved.iter_mut().find(|entry| key_of(*entry) == key) {
            *slot = value;
        } else {
            resolved.push(value);
        }
    }

    resolved
}

pub fn resolve_profile_hotkeys<'a>(profiles: &'a [Profile], profile: &'a Profile) -> Vec<&'a Hotkey> {
    let mut visited = HashSet::new();
    resolve_profile_entries(
        profiles,
        profile,
        |current| current.hotkeys.as_slice(),
        |hotkey| hotkey.trigger.clone(),
        &mut visited,
    )
}

pub fn resolve_profile_states<'a>(profiles: &'a [Profile], profile: &'a Profile) -> Vec<&'a ProfileState> {
    let mut visited = HashSet::new();
    resolve_profile_entries(
        profiles,
        profile,
        |current| current.states.as_slice(),
        |state| state.id.clone(),
        &mut visited,
    )
}

pub fn resolve_profile_overlay_items<'a>(profiles: &'a [Profile], profile: &'a Profile) -> Vec<&'a OverlayItem> {
    let mut visited = HashSet::new();
    resolve_profile_entries(
        profiles,
        profile,
        |current| current.overlay_items.as_slice(),
        overlay_item_id,
        &mut visited,
    )
}
