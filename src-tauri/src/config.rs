use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// The db.json schema version. Bumped when the shape changes so `load_db` can migrate
/// older files. v3 = folder-owned executable + profile-owned behaviors/scripts/arming.
pub const CURRENT_DB_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Database {
    #[serde(default)]
    pub version: u32,
    #[serde(rename = "scopes", alias = "games")]
    pub games: Vec<Game>,
    pub settings: Settings,
}

/// A folder owns one target executable shared by all of its profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

/// A user script owned by a profile. It runs either when its hotkey is pressed (while the
/// folder's app is focused) or when the folder's app is launched. The body is either inline
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

/// A profile owns behaviors, scripts, overlay items, and states that run against its folder's
/// target executable. Arming controls whether those behaviors are active.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// "hotkeys" | "scripts" | "overlay" — what the profile is for (drives the editor UI).
    #[serde(default = "default_profile_kind")]
    pub kind: String,
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
            exe: "*".to_string(),
            image: None,
            profiles: Vec::new(),
        });
        return true;
    }
    if indices.len() == 1 {
        let global = &mut db.games[indices[0]];
        if global.exe != "*" {
            global.exe = "*".to_string();
            return true;
        }
        return false;
    }
    // Merge every duplicate Global folder's profiles into the first, then drop the extras.
    let keep = indices[0];
    let merged: Vec<Profile> = indices.iter().flat_map(|&i| db.games[i].profiles.clone()).collect();
    db.games[keep].profiles = merged;
    db.games[keep].exe = "*".to_string();
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

    let (mut db, migrated) = match version {
        v if v >= CURRENT_DB_VERSION => {
            (serde_json::from_value(value).map_err(|e| e.to_string())?, false)
        }
        2 => {
            let legacy: legacy_v2::Database = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let mut backup = path.as_os_str().to_owned();
            backup.push(".v2.bak");
            let _ = std::fs::copy(path, std::path::Path::new(&backup));
            (legacy_v2::migrate(legacy), true)
        }
        _ => {
            let legacy: legacy_v1::Database = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let mut backup = path.as_os_str().to_owned();
            backup.push(".v1.bak");
            let _ = std::fs::copy(path, std::path::Path::new(&backup));
            (legacy_v1::migrate(legacy), true)
        }
    };

    // Self-heal the always-present global folder, then persist only when something changed.
    let added_global = ensure_global_folder(&mut db);
    if migrated || added_global {
        save_db(path, &db)?;
    }
    Ok(db)
}

/// Reads v2's profile-owned executable shape and groups profiles under folder-owned targets.
/// A folder containing several targets is split so every profile keeps its activation behavior.
mod legacy_v2 {
    use super::{Database as NewDatabase, Game as NewGame, Profile, Settings, CURRENT_DB_VERSION, GLOBAL_FOLDER_ID};
    use serde::Deserialize;
    use std::collections::HashSet;

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
        pub image: Option<String>,
        #[serde(default)]
        pub profiles: Vec<LegacyProfile>,
    }

    #[derive(Deserialize)]
    pub struct LegacyProfile {
        #[serde(default)]
        pub exe: String,
        #[serde(flatten)]
        pub profile: Profile,
    }

    fn normalized_exe(exe: &str) -> String {
        exe.trim().to_string()
    }

    fn target_label(exe: &str) -> &str {
        match exe {
            "*" => "Any app",
            "" => "No app",
            value => value,
        }
    }

    fn split_id(
        original_id: &str,
        ordinal: usize,
        reserved: &HashSet<String>,
        allocated: &mut HashSet<String>,
    ) -> String {
        let mut suffix = ordinal;
        loop {
            let candidate = format!("{original_id}-target-{suffix}");
            if !reserved.contains(&candidate) && allocated.insert(candidate.clone()) {
                return candidate;
            }
            suffix += 1;
        }
    }

    fn materialize_cross_target_inheritance(profiles: &mut [LegacyProfile]) {
        let snapshots: Vec<Profile> = profiles.iter().map(|entry| entry.profile.clone()).collect();
        for index in 0..profiles.len() {
            let Some(parent_id) = snapshots[index].parent_id.as_deref() else { continue };
            let Some(parent_index) = snapshots.iter().position(|profile| profile.id == parent_id) else { continue };
            if normalized_exe(&profiles[index].exe)
                .eq_ignore_ascii_case(&normalized_exe(&profiles[parent_index].exe))
            {
                continue;
            }

            let profile = &snapshots[index];
            profiles[index].profile.hotkeys = super::resolve_profile_hotkeys(&snapshots, profile)
                .into_iter()
                .cloned()
                .collect();
            profiles[index].profile.states = super::resolve_profile_states(&snapshots, profile)
                .into_iter()
                .cloned()
                .collect();
            profiles[index].profile.overlay_items = super::resolve_profile_overlay_items(&snapshots, profile)
                .into_iter()
                .cloned()
                .collect();
            profiles[index].profile.parent_id = None;
        }
    }

    pub fn migrate(legacy: Database) -> NewDatabase {
        let reserved: HashSet<String> = legacy.games.iter().map(|game| game.id.clone()).collect();
        let mut allocated = HashSet::new();
        let mut games = Vec::new();

        for mut game in legacy.games {
            materialize_cross_target_inheritance(&mut game.profiles);
            let mut groups: Vec<(String, Vec<Profile>)> = Vec::new();
            for entry in game.profiles {
                let exe = normalized_exe(&entry.exe);
                if let Some((_, profiles)) = groups
                    .iter_mut()
                    .find(|(target, _)| target.eq_ignore_ascii_case(&exe))
                {
                    profiles.push(entry.profile);
                } else {
                    groups.push((exe, vec![entry.profile]));
                }
            }

            if game.id == GLOBAL_FOLDER_ID {
                allocated.insert(game.id.clone());
                let global_index = groups.iter().position(|(exe, _)| exe == "*");
                let global_profiles = global_index
                    .map(|index| groups.remove(index).1)
                    .unwrap_or_default();
                games.push(NewGame {
                    id: game.id.clone(),
                    name: game.name.clone(),
                    exe: "*".to_string(),
                    image: game.image.clone(),
                    profiles: global_profiles,
                });
                for (index, (exe, profiles)) in groups.into_iter().enumerate() {
                    games.push(NewGame {
                        id: split_id(&game.id, index + 2, &reserved, &mut allocated),
                        name: format!("{} ({})", game.name, target_label(&exe)),
                        exe,
                        image: game.image.clone(),
                        profiles,
                    });
                }
                continue;
            }

            if groups.is_empty() {
                allocated.insert(game.id.clone());
                games.push(NewGame {
                    id: game.id,
                    name: game.name,
                    exe: String::new(),
                    image: game.image,
                    profiles: Vec::new(),
                });
                continue;
            }

            let multiple_targets = groups.len() > 1;
            for (index, (exe, profiles)) in groups.into_iter().enumerate() {
                let id = if index == 0 {
                    allocated.insert(game.id.clone());
                    game.id.clone()
                } else {
                    split_id(&game.id, index + 1, &reserved, &mut allocated)
                };
                let name = if multiple_targets {
                    format!("{} ({})", game.name, target_label(&exe))
                } else {
                    game.name.clone()
                };
                games.push(NewGame {
                    id,
                    name,
                    exe,
                    image: game.image.clone(),
                    profiles,
                });
            }
        }

        NewDatabase { version: CURRENT_DB_VERSION, games, settings: legacy.settings }
    }
}

/// Reads the pre-v2 db.json shape (exe/scripts/toggle-keys/overlay_disabled on the Scope, plus
/// `active_profile`) and writes it directly into the current folder-owned executable shape.
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
                super::Game { id: g.id, name: g.name, exe: g.exe, image: g.image, profiles }
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

#[cfg(test)]
mod tests {
    use super::{legacy_v2, Hotkey};

    #[test]
    fn v2_migration_splits_multi_target_folders_and_preserves_inheritance() {
        let legacy: legacy_v2::Database = serde_json::from_value(serde_json::json!({
            "version": 2,
            "scopes": [{
                "id": "games",
                "name": "Games",
                "profiles": [{
                    "id": "base",
                    "name": "Base",
                    "exe": "First.exe",
                    "hotkeys": [{ "trigger": "f1", "behavior": "press(1)" }]
                }, {
                    "id": "child",
                    "name": "Child",
                    "exe": "Second.exe",
                    "parent_id": "base"
                }]
            }]
        })).unwrap();

        let migrated = legacy_v2::migrate(legacy);

        assert_eq!(migrated.version, 3);
        assert_eq!(migrated.games.len(), 2);
        assert_eq!(migrated.games[0].exe, "First.exe");
        assert_eq!(migrated.games[1].exe, "Second.exe");
        let child = &migrated.games[1].profiles[0];
        assert_eq!(child.parent_id, None);
        assert!(matches!(child.hotkeys.as_slice(), [Hotkey { trigger, behavior, .. }]
            if trigger == "f1" && behavior == "press(1)"));
    }
}
