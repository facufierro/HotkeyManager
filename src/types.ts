export interface Hotkey {
  name: string;
  trigger: string;
  behavior: string;
  state_id: string | null;
}

export interface ProfileState {
  id: string;
  name: string;
  duration_ms: number | null;
}

export interface OverlayHotkeyStateBinding {
  trigger: string;
  state_id: string | null;
}

export type OverlayTriggerEvent = "profile_activated" | "profile_deactivated" | "hotkey_triggered";

export type OverlayTriggerAction = "set_flag" | "clear_flag" | "toggle_flag" | "start_timer" | "stop_timer";

export interface OverlayTrigger {
  id: string;
  event: OverlayTriggerEvent;
  hotkey_trigger: string | null;
  action: OverlayTriggerAction;
  state_key: string;
  duration_ms: number | null;
}

export interface OverlayConfig {
  items: OverlayItem[];
  states: ProfileState[];
  hotkeys: OverlayHotkeyStateBinding[];
}

export type OverlayDisplayMode = "always" | "timed_hotkey" | "toggle_hotkey";

interface OverlayItemBase {
  id: string;
  name: string;
  x: number;
  y: number;
  state_id: string | null;
  group_id?: string | null;
  visible_when?: string | null;
  display_mode?: OverlayDisplayMode;
  hotkey_trigger?: string | null;
  show_duration_ms?: number | null;
}

export type OverlayItem =
  | (OverlayItemBase & { type: "timer"; duration_ms: number; color: string; font_size: number; timer_state_id: string | null; timer_key?: string | null })
  | (OverlayItemBase & { type: "icon"; w: number; h: number; src: string | null })
  | (OverlayItemBase & { type: "bar"; w: number; h: number; color: string; max_value: number })
  | (OverlayItemBase & { type: "text"; font_size: number; color: string; content: string });

export interface OverlayGroup {
  id: string;
  name: string;
}

export type ProfileKind = "hotkeys" | "scripts" | "overlay";

export interface Profile {
  id: string;
  name: string;
  kind: ProfileKind;
  armed: boolean;
  parent_id: string | null;
  hotkeys: Hotkey[];
  states: ProfileState[];
  overlay_items: OverlayItem[];
  overlay_triggers: OverlayTrigger[];
  overlay_groups: OverlayGroup[];
  scripts: Script[];
  overlay_disabled?: boolean;
  toggle_hotkeys_key: string | null;
  toggle_overlay_key: string | null;
}

export type ScriptTrigger = "hotkey" | "launch";
export type ScriptSource = "code" | "path";
export type ScriptLanguage = "python" | "autohotkey";

export interface Script {
  id: string;
  name: string;
  enabled: boolean;
  trigger: ScriptTrigger;
  hotkey: string;
  language: ScriptLanguage;
  source: ScriptSource;
  code: string;
  path: string;
}

/** A folder owns the application target shared by its profiles. */
export interface Scope {
  id: string;
  name: string;
  exe: string;
  image: string | null;
  profiles: Profile[];
}

export interface Settings {
  ahk_exe: string;
  python_exe: string;
  open_to_tray: boolean;
  close_to_tray: boolean;
  launch_on_startup: boolean;
}

export interface Database {
  version: number;
  scopes: Scope[];
  settings: Settings;
}
