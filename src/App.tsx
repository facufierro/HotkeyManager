import { useState, useEffect, useCallback, useRef, type PointerEvent as ReactPointerEvent, type MouseEvent as ReactMouseEvent } from "react";
import { createPortal } from "react-dom";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog, save as saveDialog, ask } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { api } from "./api";
import type {
  Database,
  Scope,
  Profile,
  Hotkey,
  OverlayItem,
  OverlayGroup,
  ProfileState,
  Script,
  ProfileKind,
} from "./types";
import "./App.css";

type Modal =
  | { type: "addGame" }
  | { type: "editGame"; game: Scope }
  | { type: "profileSettings"; gameId: string; profile: Profile; isNew: boolean }
  | { type: "editHotkey"; gameId: string; profileId: string; index: number | null; hotkey: Hotkey; gameExe: string; states: ProfileState[] }
  | { type: "copyHotkey"; sourceGameId: string; sourceProfileId: string; hotkey: Hotkey }
  | { type: "setParent"; gameId: string; profile: Profile }
  | { type: "copyProfile"; sourceGameId: string; profile: Profile }
  | { type: "overlayItem"; gameId: string; profileId: string; index: number | null; item: OverlayItem; gameExe: string; states: ProfileState[]; groups: OverlayGroup[] }
  | { type: "copyOverlayItem"; sourceGameId: string; sourceProfileId: string; item: OverlayItem }
  | { type: "copyState"; sourceGameId: string; sourceProfileId: string; state: ProfileState }
  | { type: "profileState"; gameId: string; profileId: string; index: number | null; state: ProfileState }
  | { type: "script"; gameId: string; profileId: string; index: number | null; script: Script };

// ── Helpers ──────────────────────────────────────────────────────────────────

const GLOBAL_EXE = "*";
const GLOBAL_FOLDER_ID = "global";

function uid() {
  return crypto.randomUUID();
}

/** A folder owns one app target shared by all of its profiles. */
function blankGame(): Scope {
  return { id: uid(), name: "", exe: "", image: null, profiles: [] };
}

/** A profile owns the behaviors that run against its folder's app target. */
function blankProfile(): Profile {
  return {
    id: uid(), name: "", kind: "hotkeys", armed: false, parent_id: null,
    hotkeys: [], states: [], overlay_items: [], overlay_triggers: [], overlay_groups: [],
    scripts: [], overlay_disabled: false, toggle_hotkeys_key: null, toggle_overlay_key: null,
  };
}

/** `exe === "*"` means the folder applies to any app / always. */
function isGlobalExe(exe: string) {
  return exe.trim() === GLOBAL_EXE;
}

function blankTimer():   OverlayItem { return { type: "timer", id: uid(), name: "", x: 0, y: 0, duration_ms: 60000, color: "#ffffff", font_size: 22, state_id: null, timer_state_id: null }; }
function blankIcon():    OverlayItem { return { type: "icon",  id: uid(), name: "", x: 0, y: 0, w: 64, h: 64, src: null, state_id: null }; }
function blankBar():     OverlayItem { return { type: "bar",   id: uid(), name: "", x: 0, y: 0, w: 200, h: 20, color: "#4ade80", max_value: 100, state_id: null }; }
function blankText():    OverlayItem { return { type: "text",  id: uid(), name: "", x: 0, y: 0, font_size: 16, color: "#ffffff", content: "", state_id: null }; }
function blankState(): ProfileState { return { id: uid(), name: "", duration_ms: null }; }
function blankScript(): Script { return { id: uid(), name: "", enabled: true, trigger: "hotkey", hotkey: "", language: "python", source: "code", code: "", path: "" }; }

// ── Export / Import helpers ───────────────────────────────────────────────────

type LegacyProfileExport = Profile & { exe?: string };
type LegacyScopeExport = Omit<Scope, "exe" | "profiles"> & { exe?: string; profiles: LegacyProfileExport[] };

function remapProfileIds(profile: LegacyProfileExport, profileIds?: Map<string, string>): Profile {
  const { exe: _legacyExe, ...profileData } = profile;
  const stateMap = new Map(profile.states.map(s => [s.id, uid()]));
  const groupMap = new Map((profile.overlay_groups ?? []).map(g => [g.id, uid()]));
  return {
    ...profileData,
    id: profileIds?.get(profile.id) ?? uid(),
    armed: false,
    parent_id: profile.parent_id ? (profileIds?.get(profile.parent_id) ?? profile.parent_id) : null,
    scripts: (profile.scripts ?? []).map(s => ({ ...s, id: uid() })),
    states: profile.states.map(s => ({ ...s, id: stateMap.get(s.id)! })),
    overlay_groups: (profile.overlay_groups ?? []).map(g => ({ ...g, id: groupMap.get(g.id)! })),
    hotkeys: profile.hotkeys.map(h => ({
      ...h,
      state_id: h.state_id ? (stateMap.get(h.state_id) ?? null) : null,
    })),
    overlay_items: profile.overlay_items.map(item => ({
      ...item,
      id: uid(),
      state_id: item.state_id ? (stateMap.get(item.state_id) ?? null) : null,
      group_id: item.group_id ? (groupMap.get(item.group_id) ?? null) : null,
      ...(item.type === "timer" && item.timer_state_id
        ? { timer_state_id: stateMap.get(item.timer_state_id) ?? null }
        : {}),
    })),
  };
}

async function exportProfile(profile: Profile) {
  const path = await saveDialog({
    title: "Export Profile",
    defaultPath: `${profile.name || "profile"}.hkm-profile`,
    filters: [{ name: "HKM Profile", extensions: ["hkm-profile"] }],
  });
  if (!path) return;
  await api.writeTextFile(path, JSON.stringify({ version: 2, type: "profile", data: profile }, null, 2));
}

async function exportScope(scope: Scope) {
  const path = await saveDialog({
    title: "Export Scope",
    defaultPath: `${scope.name || "scope"}.hkm-scope`,
    filters: [{ name: "HKM Scope", extensions: ["hkm-scope"] }],
  });
  if (!path) return;
  await api.writeTextFile(path, JSON.stringify({ version: 2, type: "scope", data: scope }, null, 2));
}

async function importProfile(): Promise<Profile | null> {
  const path = await openDialog({
    title: "Import Profile",
    filters: [{ name: "HKM Profile", extensions: ["hkm-profile", "json"] }],
    multiple: false, directory: false,
  });
  if (!path) return null;
  const raw = JSON.parse(await api.readTextFile(path as string));
  if (raw.type !== "profile") throw new Error("File is not a profile export");
  return remapProfileIds(raw.data as Profile);
}

async function importScope(): Promise<Scope[] | null> {
  const path = await openDialog({
    title: "Import Scope",
    filters: [{ name: "HKM Scope", extensions: ["hkm-scope", "json"] }],
    multiple: false, directory: false,
  });
  if (!path) return null;
  const raw = JSON.parse(await api.readTextFile(path as string));
  if (raw.type !== "scope") throw new Error("File is not a scope export");
  const scope = raw.data as LegacyScopeExport;
  const profileIds = new Map(scope.profiles.map(profile => [profile.id, uid()]));
  let entries = scope.profiles.map(profile => ({
    target: typeof scope.exe === "string" ? scope.exe.trim() : (profile.exe ?? "").trim(),
    profile: remapProfileIds(profile, profileIds),
  }));

  const allProfiles = entries.map(entry => entry.profile);
  const targetsByProfile = new Map(entries.map(entry => [entry.profile.id, entry.target]));
  entries = entries.map(entry => {
    const parent = entry.profile.parent_id
      ? allProfiles.find(profile => profile.id === entry.profile.parent_id)
      : undefined;
    if (!parent || (targetsByProfile.get(parent.id) ?? "").toLowerCase() === entry.target.toLowerCase()) {
      return entry;
    }
    return {
      ...entry,
      profile: {
        ...entry.profile,
        parent_id: null,
        hotkeys: resolveHotkeys(allProfiles, entry.profile).map(({ hotkey }) => ({ ...hotkey })),
        states: resolveStates(allProfiles, entry.profile).map(({ state }) => ({ ...state })),
        overlay_items: resolveOverlayItems(allProfiles, entry.profile).map(({ item }) => ({ ...item })),
      },
    };
  });

  const groups: Array<{ exe: string; profiles: Profile[] }> = [];
  for (const entry of entries) {
    const group = groups.find(candidate => candidate.exe.toLowerCase() === entry.target.toLowerCase());
    if (group) group.profiles.push(entry.profile);
    else groups.push({ exe: entry.target, profiles: [entry.profile] });
  }
  if (groups.length === 0) groups.push({ exe: typeof scope.exe === "string" ? scope.exe.trim() : "", profiles: [] });

  return groups.map(group => ({
    ...scope,
    id: uid(),
    name: groups.length === 1
      ? scope.name
      : `${scope.name} (${isGlobalExe(group.exe) ? "Any app" : group.exe || "No app"})`,
    exe: group.exe,
    profiles: group.profiles,
  }));
}

function formatDuration(ms: number | null) {
  if (!ms || ms <= 0) return "0s";
  const totalSeconds = Math.round(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  if (minutes === 0) return `${seconds}s`;
  if (seconds === 0) return `${minutes}m`;
  return `${minutes}m ${seconds}s`;
}

function stateNameById(states: ProfileState[], stateId: string | null | undefined) {
  if (!stateId) return null;
  return states.find(state => state.id === stateId)?.name ?? null;
}

function sideModifier(code: string): string | null {
  const map: Record<string, string> = {
    ControlLeft: "lctrl",
    ControlRight: "rctrl",
    ShiftLeft: "lshift",
    ShiftRight: "rshift",
    AltLeft: "lalt",
    AltRight: "ralt",
    MetaLeft: "lwin",
    MetaRight: "rwin",
  };
  return map[code] ?? null;
}

function bindingModifiers(
  e: { ctrlKey: boolean; shiftKey: boolean; altKey: boolean; metaKey: boolean },
  activeSideModifiers: Set<string> = new Set(),
) {
  const mods: string[] = [];
  // AltGr (RAlt on non-US layouts) arrives as a synthetic LCtrl held together with
  // RAlt — drop that phantom Ctrl so AltGr combos record as plain ralt.
  const altGr = activeSideModifiers.has("ralt");
  const isHeld = (mod: string) => activeSideModifiers.has(mod) && !(altGr && mod === "lctrl");
  const addSideOrGeneric = (left: string, right: string, generic: string, active: boolean) => {
    const sides = [left, right].filter(isHeld);
    if (sides.length) mods.push(...sides);
    else if (active) mods.push(generic);
  };
  addSideOrGeneric("lctrl", "rctrl", "ctrl", e.ctrlKey && !altGr);
  addSideOrGeneric("lshift", "rshift", "shift", e.shiftKey);
  addSideOrGeneric("lalt", "ralt", "alt", e.altKey);
  addSideOrGeneric("lwin", "rwin", "win", e.metaKey);
  return mods;
}

function toAhkKey(e: KeyboardEvent, activeSideModifiers: Set<string>): string {
  const mods = bindingModifiers(e, activeSideModifiers);
  const keyMap: Record<string, string> = {
    " ": "Space", ArrowUp: "Up", ArrowDown: "Down", ArrowLeft: "Left", ArrowRight: "Right",
    PageUp: "PgUp", PageDown: "PgDn", Enter: "Enter", Escape: "Esc", Tab: "Tab",
    Backspace: "Backspace", Delete: "Del", Insert: "Ins", Home: "Home", End: "End",
    PrintScreen: "PrintScreen", NumLock: "NumLock", CapsLock: "CapsLock",
  };
  // Unlike e.key, e.code retains the keypad position. Keep keypad operators named so they
  // do not collapse into the corresponding main-keyboard character.
  const keypadKeys: Record<string, string> = {
    NumpadDivide: "NumpadDiv",
    NumpadMultiply: "NumpadMult",
    NumpadSubtract: "NumpadSub",
    NumpadAdd: "NumpadAdd",
    NumpadEnter: "NumpadEnter",
  };
  let key = keypadKeys[e.code] ?? (/^Numpad[0-9]$/.test(e.code) ? e.code : e.key);
  if (key in keyMap) key = keyMap[key];
  else if (/^F\d+$/.test(key)) key = key.toLowerCase();
  else if (key.length === 1) {
    // Record the typed character when it has an AHK name, so the binding means the key
    // label the user sees — on QWERTZ pressing Z must record "z", not the US position
    // "y". The backend resolves it back through the active layout. Shift transforms
    // (Shift+5 → "%"), punctuation, and characters with no AHK name (Cyrillic "й")
    // fall back to the physical e.code position.
    if (/^[a-z0-9]$/i.test(key)) key = key.toLowerCase();
    else {
      const punctCodes: Record<string, string> = {
        Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/", Backslash: "\\",
        BracketLeft: "[", BracketRight: "]", Backquote: "`", Minus: "-", Equal: "=",
      };
      const codeMatch = e.code.match(/^(Key|Digit)(.+)$/);
      key = codeMatch ? codeMatch[2].toLowerCase() : (punctCodes[e.code] ?? key.toLowerCase());
    }
  }
  return [...mods, key].join(" ");
}

function toAhkMouseButton(e: MouseEvent, activeSideModifiers: Set<string>): string {
  const buttonMap: Record<number, string> = {
    0: "LButton",
    1: "MButton",
    2: "RButton",
    3: "XButton1",
    4: "XButton2",
  };
  const button = buttonMap[e.button];
  if (!button) return "";
  return [...bindingModifiers(e, activeSideModifiers), button].join(" ");
}

function useBindingRecorder(recording: boolean, onCapture: (value: string) => void) {
  const onCaptureRef = useRef(onCapture);
  const activeSideModifiersRef = useRef<Set<string>>(new Set());
  onCaptureRef.current = onCapture;

  useEffect(() => {
    if (!recording) return;

    const capture = (value: string) => {
      if (value) onCaptureRef.current(value);
    };
    const keyHandler = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopImmediatePropagation();
      const sideMod = sideModifier(e.code);
      if (sideMod) {
        activeSideModifiersRef.current.add(sideMod);
        return;
      }
      capture(toAhkKey(e, activeSideModifiersRef.current));
    };
    const keyUpHandler = (e: KeyboardEvent) => {
      const sideMod = sideModifier(e.code);
      if (sideMod) activeSideModifiersRef.current.delete(sideMod);
    };
    const mouseHandler = (e: MouseEvent) => {
      e.preventDefault();
      e.stopImmediatePropagation();
      capture(toAhkMouseButton(e, activeSideModifiersRef.current));
    };
    const contextMenuHandler = (e: MouseEvent) => {
      e.preventDefault();
      e.stopImmediatePropagation();
    };

    window.addEventListener("keydown", keyHandler, true);
    window.addEventListener("keyup", keyUpHandler, true);
    window.addEventListener("mousedown", mouseHandler, true);
    window.addEventListener("contextmenu", contextMenuHandler, true);
    return () => {
      activeSideModifiersRef.current.clear();
      window.removeEventListener("keydown", keyHandler, true);
      window.removeEventListener("keyup", keyUpHandler, true);
      window.removeEventListener("mousedown", mouseHandler, true);
      window.removeEventListener("contextmenu", contextMenuHandler, true);
    };
  }, [recording]);
}

function KeyInput({ value, onChange, placeholder = "Key" }: { value: string; onChange: (v: string) => void; placeholder?: string }) {
  const [recording, setRecording] = useState(false);
  useBindingRecorder(recording, key => {
    onChange(key);
    setRecording(false);
  });

  return recording ? (
    <div className="key-recording">Press any key or mouse button…</div>
  ) : (
    <div className="input-row" style={{ margin: 0, flex: 1 }}>
      <input value={value} onChange={e => onChange(e.target.value)} placeholder={placeholder} />
      <button className="btn btn--ghost btn--sm" onClick={() => setRecording(true)} title="Record key">⌨</button>
    </div>
  );
}

type ResolvedProfileEntry<T> = { value: T; own: boolean };

function resolveProfileEntries<T>(
  profiles: Profile[],
  profile: Profile,
  select: (profile: Profile) => T[],
  keyOf: (value: T) => string,
  visited = new Set<string>(),
): Array<ResolvedProfileEntry<T>> {
  if (visited.has(profile.id)) return [];

  const nextVisited = new Set(visited);
  nextVisited.add(profile.id);

  const resolved: Array<ResolvedProfileEntry<T>> = profile.parent_id
    ? (() => {
        const parent = profiles.find(candidate => candidate.id === profile.parent_id);
        return parent
          ? resolveProfileEntries(profiles, parent, select, keyOf, nextVisited).map(entry => ({ ...entry, own: false }))
          : [];
      })()
    : [];

  for (const value of select(profile)) {
    const key = keyOf(value);
    const index = resolved.findIndex(entry => keyOf(entry.value) === key);
    const entry = { value, own: true };
    if (index >= 0) resolved[index] = entry;
    else resolved.push(entry);
  }

  return resolved;
}

function resolveHotkeys(profiles: Profile[], profile: Profile): Array<{ hotkey: Hotkey; own: boolean }> {
  return resolveProfileEntries(profiles, profile, current => current.hotkeys, hotkey => hotkey.trigger)
    .map(({ value, own }) => ({ hotkey: value, own }));
}

function resolveStates(profiles: Profile[], profile: Profile): Array<{ state: ProfileState; own: boolean }> {
  return resolveProfileEntries(profiles, profile, current => current.states, state => state.id)
    .map(({ value, own }) => ({ state: value, own }));
}

function resolveOverlayItems(profiles: Profile[], profile: Profile): Array<{ item: OverlayItem; own: boolean }> {
  return resolveProfileEntries(profiles, profile, current => current.overlay_items, item => item.id)
    .map(({ value, own }) => ({ item: value, own }));
}

// ── Sub-components ────────────────────────────────────────────────────────────

function Placeholder({ label }: { label?: string }) {
  return (
    <div className="placeholder">
      <span>{label ?? "[ image ]"}</span>
    </div>
  );
}

// ── Redesigned shell: library tree + focused editor ───────────────────────────

type Selection =
  | { kind: "profile"; folderId: string; profileId: string }
  | { kind: "settings" }
  | { kind: "empty" };

/** Whether an armed profile's folder app is present (so its hotkeys can fire). "*" is always present. */
function profileAppActive(exe: string, openExes: Set<string>): boolean {
  return isGlobalExe(exe) || openExes.has(exe.trim().toLowerCase());
}

/** Green when armed and its app is active, yellow when armed but the app is not, hollow when off. */
function StatusDot({ armed, active }: { armed: boolean; active: boolean }) {
  const cls = !armed ? "status-dot--off" : active ? "status-dot--live" : "status-dot--armed";
  return <span className={`status-dot ${cls}`} />;
}

function TargetChip({ exe }: { exe: string }) {
  const label = isGlobalExe(exe) ? "Any app" : (exe.trim() === "" ? "No app" : exe);
  return <span className="target-chip" title={label}>{label}</span>;
}

function ArmSwitch({ armed, size, onChange }: { armed: boolean; size?: "lg"; onChange: (a: boolean) => void }) {
  return (
    <label className={`scope-toggle ${size === "lg" ? "" : "scope-toggle--sm"}`}
      title={armed ? "Armed" : "Off"} onClick={e => e.stopPropagation()}>
      <input type="checkbox" checked={armed} onChange={e => onChange(e.target.checked)} />
      <span className="scope-toggle__track" aria-hidden="true" />
    </label>
  );
}

function PopoverMenu({ items, trigger = "⋯", title = "More" }: {
  items: { label: string; danger?: boolean; onClick: () => void }[]; trigger?: string; title?: string;
}) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ top: number; right: number }>({ top: 0, right: 0 });
  const btnRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      const t = e.target as Node;
      if (btnRef.current?.contains(t) || menuRef.current?.contains(t)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => { document.removeEventListener("mousedown", onDoc); document.removeEventListener("keydown", onKey); };
  }, [open]);
  return (
    <>
      <button ref={btnRef} className="icon-btn" title={title} onClick={e => {
        e.stopPropagation();
        const r = btnRef.current?.getBoundingClientRect();
        if (r) setPos({ top: r.bottom + 4, right: window.innerWidth - r.right });
        setOpen(o => !o);
      }}>{trigger}</button>
      {open && createPortal(
        <div ref={menuRef} className="menu menu--fixed" style={{ top: pos.top, right: pos.right }}>
          {items.map((it, i) => (
            <button key={i} className={it.danger ? "menu--danger" : ""}
              onClick={e => { e.stopPropagation(); setOpen(false); it.onClick(); }}>{it.label}</button>
          ))}
        </div>, document.body)}
    </>
  );
}

type CtxItem = { label: string; danger?: boolean; onClick: () => void };

/** Right-click context menu positioned at the cursor and portaled to body. */
function ContextMenu({ x, y, items, onClose }: { x: number; y: number; items: CtxItem[]; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onDoc = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) onClose(); };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    window.addEventListener("blur", onClose);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", onClose);
    };
  }, [onClose]);
  const left = Math.min(x, window.innerWidth - 210);
  const top = Math.min(y, window.innerHeight - (items.length * 34 + 16));
  return createPortal(
    <div ref={ref} className="menu menu--fixed" style={{ left, top }}>
      {items.map((it, i) => (
        <button key={i} className={it.danger ? "menu--danger" : ""}
          onClick={() => { onClose(); it.onClick(); }}>{it.label}</button>
      ))}
    </div>, document.body);
}

function EmptyState({ title, hint, action, secondary }: {
  title: string; hint?: string;
  action?: { label: string; onClick: () => void };
  secondary?: { label: string; onClick: () => void };
}) {
  return (
    <div className="empty-state">
      <h2>{title}</h2>
      {hint && <p>{hint}</p>}
      {(action || secondary) && (
        <div style={{ display: "flex", gap: 8 }}>
          {action && <button className="btn btn--primary" onClick={action.onClick}>{action.label}</button>}
          {secondary && <button className="btn btn--ghost" onClick={secondary.onClick}>{secondary.label}</button>}
        </div>
      )}
    </div>
  );
}

function Resizer({ onPointerDown }: { onPointerDown: (e: ReactPointerEvent) => void }) {
  const [drag, setDrag] = useState(false);
  return (
    <div className={`resizer ${drag ? "resizer--dragging" : ""}`}
      onPointerDown={e => {
        setDrag(true);
        onPointerDown(e);
        const up = () => { setDrag(false); window.removeEventListener("pointerup", up); };
        window.addEventListener("pointerup", up);
      }} />
  );
}

function useLibraryWidth() {
  const [w, setW] = useState(() => {
    const s = Number(localStorage.getItem("libW"));
    return s >= 240 && s <= 520 ? s : 300;
  });
  const wRef = useRef(w); wRef.current = w;
  const onPointerDown = (e: ReactPointerEvent) => {
    e.preventDefault();
    const x0 = e.clientX, w0 = w;
    const move = (ev: PointerEvent) => setW(Math.min(520, Math.max(240, w0 + ev.clientX - x0)));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      localStorage.setItem("libW", String(wRef.current));
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };
  return { w, onPointerDown };
}

function ProfileRow({ profile, folderExe, openExes, selected, onSelect, onToggleArmed, onContext }: {
  profile: Profile; folderExe: string; openExes: Set<string>; selected: boolean;
  onSelect: () => void; onToggleArmed: (a: boolean) => void; onContext: (e: ReactMouseEvent) => void;
}) {
  return (
    <button
      className={`profile-row ${selected ? "profile-row--active" : ""} ${profile.armed ? "profile-row--armed" : ""}`}
      onClick={onSelect} onContextMenu={onContext} title={profile.name}>
      <StatusDot armed={profile.armed} active={profileAppActive(folderExe, openExes)} />
      <span className="profile-row__name">{profile.name || "Untitled"}</span>
      <span className="target-chip">{profile.kind.charAt(0).toUpperCase() + profile.kind.slice(1)}</span>
      <ArmSwitch armed={profile.armed} onChange={onToggleArmed} />
    </button>
  );
}

function FolderGroup({ folder, openExes, open, selectedProfileId, visibleProfiles, onToggle, onSelectProfile, onToggleArmed, onProfileContext, onFolderContext, onModal }: {
  folder: Scope; openExes: Set<string>; open: boolean; selectedProfileId?: string;
  visibleProfiles: Profile[];
  onToggle: () => void; onSelectProfile: (id: string) => void; onToggleArmed: (pid: string, a: boolean) => void;
  onProfileContext: (e: ReactMouseEvent, folderId: string, profile: Profile) => void;
  onFolderContext: (e: ReactMouseEvent, folder: Scope) => void;
  onModal: (m: Modal) => void;
}) {
  const newProfile = (kind: ProfileKind) => onModal({ type: "profileSettings", gameId: folder.id, profile: { ...blankProfile(), kind }, isNew: true });
  const typeItems = [
    { label: "Hotkeys profile", onClick: () => newProfile("hotkeys") },
    { label: "Scripts profile", onClick: () => newProfile("scripts") },
    { label: "Overlay profile", onClick: () => newProfile("overlay") },
  ];
  return (
    <div className={`folder-group ${open ? "folder-group--open" : ""}`}>
      <div className="folder-group__head-row" onContextMenu={e => onFolderContext(e, folder)}>
        <button className="folder-group__head" onClick={onToggle}>
          <span className="folder-group__caret" />
          <span className="folder-group__name">{folder.name || "Unnamed"}</span>
        </button>
        <PopoverMenu trigger="+" title="New profile" items={typeItems} />
      </div>
      {open && (
        <div className="folder-group__profiles">
          {visibleProfiles.map(p => (
            <ProfileRow key={p.id} profile={p} folderExe={folder.exe} openExes={openExes} selected={p.id === selectedProfileId}
              onSelect={() => onSelectProfile(p.id)} onToggleArmed={a => onToggleArmed(p.id, a)}
              onContext={e => onProfileContext(e, folder.id, p)} />
          ))}
          {folder.profiles.length === 0 && (
            <div className="folder-group__empty">
              <span>New profile:</span>
              <button className="link-btn" onClick={() => newProfile("hotkeys")}>Hotkeys</button>
              <button className="link-btn" onClick={() => newProfile("scripts")}>Scripts</button>
              <button className="link-btn" onClick={() => newProfile("overlay")}>Overlay</button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function Library({ db, openExes, selection, query, collapsed, width, onQuery, onToggleCollapse, onSelect, onToggleArmed, onProfileContext, onFolderContext, onDb, onModal }: {
  db: Database; openExes: Set<string>; selection: Selection;
  query: string; collapsed: Set<string>; width: number;
  onQuery: (s: string) => void; onToggleCollapse: (id: string) => void;
  onSelect: (s: Selection) => void; onToggleArmed: (pid: string, armed: boolean) => void;
  onProfileContext: (e: ReactMouseEvent, folderId: string, profile: Profile) => void;
  onFolderContext: (e: ReactMouseEvent, folder: Scope) => void;
  onDb: (db: Database) => void; onModal: (m: Modal) => void;
}) {
  const q = query.trim().toLowerCase();
  const filtering = q !== "";
  const match = (p: Profile, folder: Scope) =>
    q === "" || p.name.toLowerCase().includes(q) || folder.exe.toLowerCase().includes(q) || folder.name.toLowerCase().includes(q);
  const selectedProfileId = selection.kind === "profile" ? selection.profileId : undefined;

  const groups = [...db.scopes]
    .sort((a, b) => a.name.localeCompare(b.name))
    .map(f => ({ f, visible: f.profiles.filter(p => match(p, f)) }))
    .filter(g => !filtering || g.visible.length > 0);

  return (
    <aside className="library" style={{ width }}>
      <div className="library__header">
        <input className="library__search" placeholder="Search profiles…" value={query} onChange={e => onQuery(e.target.value)} />
      </div>
      <div className="library__list">
        {groups.map(({ f, visible }) => (
          <FolderGroup key={f.id} folder={f} openExes={openExes}
            open={!collapsed.has(f.id) || filtering} selectedProfileId={selectedProfileId} visibleProfiles={visible}
            onToggle={() => onToggleCollapse(f.id)}
            onSelectProfile={pid => onSelect({ kind: "profile", folderId: f.id, profileId: pid })}
            onToggleArmed={onToggleArmed} onProfileContext={onProfileContext} onFolderContext={onFolderContext} onModal={onModal} />
        ))}
        {db.scopes.length === 0 && <div className="library__empty">No folders yet</div>}
        {db.scopes.length > 0 && groups.length === 0 && <div className="library__empty">No matches</div>}
      </div>
      <div className="library__footer">
        <button className="btn btn--ghost btn--full" onClick={() => onModal({ type: "addGame" })}>New folder</button>
        <button className="btn btn--ghost btn--full" onClick={async () => {
          try {
            const scopes = await importScope();
            if (!scopes) return;
            let updated: Database | null = null;
            for (const scope of scopes) updated = await api.upsertGame(scope);
            if (updated) onDb(updated);
          } catch (e) { alert(String(e)); }
        }}>Import folder</button>
        <button className={`btn btn--ghost btn--full ${selection.kind === "settings" ? "btn--primary" : ""}`}
          onClick={() => onSelect({ kind: "settings" })}>Settings</button>
      </div>
    </aside>
  );
}

/** Edit a folder's shared app target, name, and optional image. */
function FolderModal({ initial, onSave, onClose }: {
  initial: Scope;
  onSave: (g: Scope) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [image, setImage] = useState<string | null>(initial.image);
  const globalFolder = initial.id === GLOBAL_FOLDER_ID;
  const startedGlobal = globalFolder || isGlobalExe(initial.exe);
  const [anyApp, setAnyApp] = useState(startedGlobal);
  const [exe, setExe] = useState(startedGlobal ? "" : initial.exe);
  const [openExes, setOpenExes] = useState<string[]>([]);

  useEffect(() => { api.listOpenExecutables().then(setOpenExes).catch(() => {}); }, []);

  async function browseImage() {
    const selected = await openDialog({
      title: "Select Folder Image",
      filters: [{ name: "Image", extensions: ["png", "jpg", "jpeg", "webp", "gif", "ico"] }],
      multiple: false,
      directory: false,
    });
    if (!selected) return;
    try {
      setImage(await api.readImageAsDataUrl(selected as string));
    } catch (e) {
      alert(`Failed to load image: ${e}`);
    }
  }

  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>{initial.name ? "Edit Folder" : "New Folder"}</h2>
        <div className="image-picker" onClick={browseImage}>
          {image
            ? <img src={image} alt="folder" className="image-picker__preview" />
            : <Placeholder />}
        </div>
        {image && (
          <button className="btn btn--ghost btn--sm" style={{ alignSelf: "flex-start" }}
            onClick={e => { e.stopPropagation(); setImage(null); }}>
            ✕ Remove image
          </button>
        )}
        <input className="modal-name" value={name} onChange={e => setName(e.target.value)} placeholder="Folder name" autoFocus />
        <label className="checkbox-row">
          <input type="checkbox" checked={anyApp} disabled={globalFolder}
            onChange={e => setAnyApp(e.target.checked)} />
          <span>Any app</span>
        </label>
        {!anyApp && (
          <div className="input-row">
            <input value={exe} onChange={e => setExe(e.target.value)} placeholder="game.exe" />
            <select value="" onChange={e => { if (e.target.value) setExe(e.target.value); }} title="Pick an open app">
              <option value="">Open apps…</option>
              {openExes.map(exeName => <option key={exeName} value={exeName}>{exeName}</option>)}
            </select>
          </div>
        )}
        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => onSave({
            ...initial,
            name,
            exe: globalFolder || anyApp ? GLOBAL_EXE : exe.trim(),
            image,
          })}>Save</button>
        </div>
      </div>
    </div>
  );
}

/** Edit profile-owned options. The application target is owned by the containing folder. */
function ProfileSettingsModal({ initial, onSave, onClose }: {
  initial: Profile;
  onSave: (p: Profile) => void;
  onClose: () => void;
}) {
  const kind = initial.kind;
  const [name, setName] = useState(initial.name);
  const [overlayOn, setOverlayOn] = useState(!initial.overlay_disabled);
  const [toggleHotkeysKey, setToggleHotkeysKey] = useState(initial.toggle_hotkeys_key ?? "");
  const [toggleOverlayKey, setToggleOverlayKey] = useState(initial.toggle_overlay_key ?? "");

  function build(): Profile {
    return {
      ...initial,
      name: name.trim(),
      overlay_disabled: !overlayOn,
      toggle_hotkeys_key: toggleHotkeysKey || null,
      toggle_overlay_key: toggleOverlayKey || null,
    };
  }

  const kindLabel = kind.charAt(0).toUpperCase() + kind.slice(1);

  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>{initial.name ? `${kindLabel} Profile` : `New ${kindLabel} Profile`}</h2>
        <input className="modal-name" value={name} onChange={e => setName(e.target.value)} placeholder="Profile name" autoFocus />
        {kind === "overlay" && (
          <label className="checkbox-row">
            <input type="checkbox" checked={overlayOn} onChange={e => setOverlayOn(e.target.checked)} />
            <span>Show overlay</span>
          </label>
        )}
        {kind === "hotkeys" && (
          <KeyInput value={toggleHotkeysKey} onChange={setToggleHotkeysKey} placeholder="Enable-hotkeys key" />
        )}
        {kind === "overlay" && (
          <KeyInput value={toggleOverlayKey} onChange={setToggleOverlayKey} placeholder="Toggle-overlay key" />
        )}
        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => onSave(build())} disabled={!name.trim()}>Save</button>
        </div>
      </div>
    </div>
  );
}

// ── Step types & helpers ──────────────────────────────────────────────────────

type Step =
  | { type: "press" | "hold"; key: string }
  | { type: "repeat"; key: string; interval: string; hold: string }
  | { type: "goto"; x: string; y: string }
  | { type: "state"; stateId: string }
  | { type: "sleep"; ms: string }
  | { type: "send"; text: string }
  | { type: "borderless" | "killprocess" | "lock" | "savecursor" | "restorecursor" | "stretch" };

function parseSteps(behavior: string): Step[] {
  if (!behavior.trim()) return [];
  return behavior.split(";").map(s => s.trim()).filter(Boolean).flatMap((s): Step[] => {
    let m: RegExpMatchArray | null;
    if ((m = s.match(/^press\((.+)\)$/))) return [{ type: "press" as const, key: m[1] }];
    if ((m = s.match(/^hold\((.+)\)$/))) return [{ type: "hold" as const, key: m[1] }];
    if ((m = s.match(/^repeat\((.+?),\s*(\d+)(?:,\s*(\d+))?\)$/))) return [{ type: "repeat" as const, key: m[1].trim(), interval: m[2], hold: m[3] ?? "6" }];
    if ((m = s.match(/^goto\((-?\d+(?:\.\d+)?),\s*(-?\d+(?:\.\d+)?)\)$/))) return [{ type: "goto" as const, x: m[1], y: m[2] }];
    if ((m = s.match(/^state\((.+)\)$/))) return [{ type: "state" as const, stateId: m[1] }];
    if ((m = s.match(/^sleep\((\d+)\)$/))) return [{ type: "sleep" as const, ms: m[1] }];
    if ((m = s.match(/^send\((.+)\)$/))) return [{ type: "send" as const, text: m[1] }];
    if (s === "borderless") return [{ type: "borderless" as const }];
    if (s === "killprocess") return [{ type: "killprocess" as const }];
    if (s === "stretch") return [{ type: "stretch" as const }];
    if (s === "lock") return [{ type: "lock" as const }];
    if (s === "savecursor") return [{ type: "savecursor" as const }];
    if (s === "restorecursor") return [{ type: "restorecursor" as const }];
    return [];
  });
}

function stepsToString(steps: Step[]): string {
  return steps.map(s => {
    switch (s.type) {
      case "press": return `press(${s.key})`;
      case "hold":  return `hold(${s.key})`;
      case "repeat": return `repeat(${s.key},${s.interval},${s.hold || "6"})`;
      case "goto":  return `goto(${s.x},${s.y})`;
      case "state": return `state(${s.stateId})`;
      case "sleep": return `sleep(${s.ms})`;
      case "send":  return `send(${s.text})`;
      default:      return s.type;
    }
  }).join(";");
}

// ── Step sub-components ───────────────────────────────────────────────────────

function GotoInput({ x, y, gameExe, onChange }: { x: string; y: string; gameExe: string; onChange: (x: string, y: string) => void }) {
  const [picking, setPicking] = useState(false);

  async function pick() {
    setPicking(true);
    try {
      const [px, py] = await api.pickCoordinate(gameExe);
      onChange(String(px), String(py));
    } catch (e) {
      alert(String(e));
    } finally {
      setPicking(false);
    }
  }

  return (
    <div className="goto-input">
      <input type="number" min={0} max={100} step={0.1} value={x} onChange={e => onChange(e.target.value, y)} placeholder="x %" />
      <input type="number" min={0} max={100} step={0.1} value={y} onChange={e => onChange(x, e.target.value)} placeholder="y %" />
      <button className="btn btn--ghost btn--sm" onClick={pick} disabled={picking}>
        {picking ? "Click target window…" : "🎯 Pick"}
      </button>
    </div>
  );
}

function StepRow({ step, index, total, gameExe, states, onChange, onDelete, onMove }: {
  step: Step;
  index: number;
  total: number;
  gameExe: string;
  states: ProfileState[];
  onChange: (s: Step) => void;
  onDelete: () => void;
  onMove: (dir: -1 | 1) => void;
}) {
  return (
    <div className="step-row">
      <span className="step-row__type">{step.type}</span>
      <div className="step-row__params">
        {(step.type === "press" || step.type === "hold") && (
          <KeyInput value={step.key} onChange={key => onChange({ ...step, key })} />
        )}
        {step.type === "repeat" && (
          <>
            <KeyInput value={step.key} onChange={key => onChange({ ...step, key })} />
            <input type="number" min={1} value={step.interval} placeholder="interval ms" title="Interval between presses (ms)"
              onChange={e => onChange({ ...step, interval: e.target.value })} style={{ width: 90 }} />
            <input type="number" min={0} value={step.hold} placeholder="hold ms" title="Key-down time per press (ms). Lower it if a game fires too fast; raise it if presses don't register."
              onChange={e => onChange({ ...step, hold: e.target.value })} style={{ width: 80 }} />
          </>
        )}
        {step.type === "goto" && (
          <GotoInput x={step.x} y={step.y} gameExe={gameExe} onChange={(x, y) => onChange({ ...step, x, y })} />
        )}
        {step.type === "state" && (
          <select value={step.stateId} onChange={e => onChange({ ...step, stateId: e.target.value })}>
            <option value="">Select state</option>
            {states.map(state => (
              <option key={state.id} value={state.id}>{state.name}</option>
            ))}
          </select>
        )}
        {step.type === "sleep" && (
          <input type="number" value={step.ms} placeholder="ms"
            onChange={e => onChange({ ...step, ms: e.target.value })} style={{ width: 90 }} />
        )}
        {step.type === "send" && (
          <input value={step.text} placeholder="text"
            onChange={e => onChange({ ...step, text: e.target.value })} />
        )}
      </div>
      <div className="step-row__btns">
        <button className="icon-btn" disabled={index === 0} onClick={() => onMove(-1)}>↑</button>
        <button className="icon-btn" disabled={index === total - 1} onClick={() => onMove(1)}>↓</button>
        <button className="icon-btn icon-btn--danger" onClick={onDelete}>×</button>
      </div>
    </div>
  );
}

function HotkeyModal({ initial, gameExe, states, onSave, onClose }: {
  initial: Hotkey;
  gameExe: string;
  states: ProfileState[];
  onSave: (hk: Hotkey) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [trigger, setTrigger] = useState(initial.trigger);
  const [recordingTrigger, setRecordingTrigger] = useState(false);
  const [steps, setSteps] = useState<Step[]>(() => {
    const parsed = parseSteps(initial.behavior);
    if (initial.state_id && !parsed.some(step => step.type === "state")) {
      return [...parsed, { type: "state", stateId: initial.state_id }];
    }
    return parsed;
  });

  useBindingRecorder(recordingTrigger, key => {
    setTrigger(key);
    setRecordingTrigger(false);
  });

  function addStep(step: Step) { setSteps(s => [...s, step]); }
  function removeStep(i: number) { setSteps(s => s.filter((_, idx) => idx !== i)); }
  function updateStep(i: number, step: Step) { setSteps(s => s.map((x, idx) => idx === i ? step : x)); }
  function moveStep(i: number, dir: -1 | 1) {
    setSteps(s => {
      const next = [...s];
      const j = i + dir;
      if (j < 0 || j >= next.length) return s;
      [next[i], next[j]] = [next[j], next[i]];
      return next;
    });
  }

  return (
    <div className="modal-overlay">
      <div className="modal modal--wide" onClick={e => e.stopPropagation()}>
        <h2>{initial.trigger ? "Edit Hotkey" : "Add Hotkey"}</h2>

        <input className="modal-name" value={name} onChange={e => setName(e.target.value)} placeholder="Name" autoFocus />

        <div className="input-row">
          {recordingTrigger
            ? <div className="key-recording">Press any key or mouse combination…</div>
            : <input value={trigger} onChange={e => setTrigger(e.target.value)} placeholder="Trigger" />
          }
          <button className="btn btn--ghost btn--sm" onClick={() => setRecordingTrigger(r => !r)}>
            {recordingTrigger ? "Cancel" : "⌨ Record"}
          </button>
        </div>

        <div className="steps-section">
          <div className="steps-list">
            {steps.map((step, i) => (
              <StepRow key={i} step={step} index={i} total={steps.length} gameExe={gameExe} states={states}
                onChange={s => updateStep(i, s)}
                onDelete={() => removeStep(i)}
                onMove={dir => moveStep(i, dir)} />
            ))}
            {steps.length === 0 && <div className="steps-empty">No steps yet</div>}
          </div>
          <div className="step-add-btns">
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "borderless" })}>+ borderless</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "goto", x: "", y: "" })}>+ goto</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "hold", key: "" })}>+ hold</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "killprocess" })}>+ killprocess</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "lock" })}>+ lock</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "press", key: "" })}>+ press</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "repeat", key: "", interval: "100", hold: "6" })}>+ repeat</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "restorecursor" })}>+ restorecursor</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "savecursor" })}>+ savecursor</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "send", text: "" })}>+ send</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "sleep", ms: "" })}>+ sleep</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "state", stateId: states[0]?.id ?? "" })}>+ state</button>
            <button className="btn btn--ghost btn--sm" onClick={() => addStep({ type: "stretch" })}>+ stretch</button>
          </div>
        </div>

        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary"
            onClick={() => trigger.trim() && onSave({ name: name.trim(), trigger: trigger.trim(), behavior: stepsToString(steps), state_id: null })}>
            Save
          </button>
        </div>
      </div>
    </div>
  );
}

function SetParentModal({ profiles, current, onSave, onClose }: {
  profiles: Profile[];
  current: Profile;
  onSave: (parentId: string | null) => void;
  onClose: () => void;
}) {
  const [parentId, setParentId] = useState(current.parent_id ?? "");
  const options = profiles.filter(p => p.id !== current.id);
  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>Set Parent Profile</h2>
        <select value={parentId} onChange={e => setParentId(e.target.value)}>
          <option value="">Don't inherit</option>
          {options.map(p => <option key={p.id} value={p.id}>Inherit from {p.name}</option>)}
        </select>
        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => onSave(parentId || null)}>Save</button>
        </div>
      </div>
    </div>
  );
}

function CopyProfileModal({ games, sourceProfile, onSave, onClose }: {
  games: Scope[];
  sourceProfile: Profile;
  onSave: (targetGameId: string, name: string) => void;
  onClose: () => void;
}) {
  const [targetGameId, setTargetGameId] = useState(games[0]?.id ?? "");
  const [name, setName] = useState(`${sourceProfile.name} (copy)`);
  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>Copy Profile</h2>
        <select value={targetGameId} onChange={e => setTargetGameId(e.target.value)}>
          {[...games].sort((a, b) => a.name.localeCompare(b.name)).map(g => (
            <option key={g.id} value={g.id}>Copy to {g.name}</option>
          ))}
        </select>
        <input value={name} onChange={e => setName(e.target.value)} placeholder="Profile name" />
        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary"
            onClick={() => targetGameId && name.trim() && onSave(targetGameId, name.trim())}>
            Copy
          </button>
        </div>
      </div>
    </div>
  );
}

function CopyToProfileModal({
  games,
  title,
  sourceGameId,
  sourceProfileId,
  onSave,
  onClose,
}: {
  games: Scope[];
  title: string;
  sourceGameId: string;
  sourceProfileId: string;
  onSave: (targetGameId: string, targetProfileId: string) => void;
  onClose: () => void;
}) {
  const [targetGameId, setTargetGameId] = useState(sourceGameId);
  const targetProfiles = [...(games.find(game => game.id === targetGameId)?.profiles ?? [])]
    .sort((a, b) => a.name.localeCompare(b.name));
  const [targetProfileId, setTargetProfileId] = useState(sourceProfileId);

  useEffect(() => {
    if (targetProfiles.some(profile => profile.id === targetProfileId)) return;
    setTargetProfileId(targetProfiles[0]?.id ?? "");
  }, [targetProfileId, targetProfiles]);

  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>{title}</h2>
        <select value={targetGameId} onChange={e => setTargetGameId(e.target.value)}>
          {[...games].sort((a, b) => a.name.localeCompare(b.name)).map(game => (
            <option key={game.id} value={game.id}>Copy to {game.name}</option>
          ))}
        </select>
        <select value={targetProfileId} onChange={e => setTargetProfileId(e.target.value)} disabled={targetProfiles.length === 0}>
          {targetProfiles.map(profile => (
            <option key={profile.id} value={profile.id}>Into {profile.name}</option>
          ))}
          {targetProfiles.length === 0 && <option value="">No profiles</option>}
        </select>
        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => targetGameId && targetProfileId && onSave(targetGameId, targetProfileId)} disabled={!targetProfileId}>
            Copy
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Overlay components ────────────────────────────────────────────────────────

function overlayItemLabel(item: OverlayItem, groups: OverlayGroup[]): string {
  const base = item.name.trim() ? `${item.name} - ${item.type}` : item.type;
  const group = item.group_id ? groups.find(g => g.id === item.group_id) : null;
  return group ? `${base} [${group.name}]` : base;
}

function overlayItemDesc(item: OverlayItem, states: ProfileState[]): string {
  const pos = `(${item.x}, ${item.y})`;
  const stateLabel = stateNameById(states, item.state_id);
  const stateText = stateLabel ? `, visible with ${stateLabel}` : "";
  switch (item.type) {
    case "timer": {
      const m = Math.floor(item.duration_ms / 60000);
      const s = String(Math.floor((item.duration_ms % 60000) / 1000)).padStart(2, "0");
      const timerStateLabel = stateNameById(states, item.timer_state_id);
      const timerText = timerStateLabel ? `, reads ${timerStateLabel} timer` : "";
      return `${m}:${s}, ${item.font_size}px at ${pos}${stateText}${timerText}`;
    }
    case "icon":  return `${item.w}x${item.h} at ${pos}${stateText}`;
    case "bar":   return `${item.w}x${item.h} max ${item.max_value} at ${pos}${stateText}`;
    case "text":  return `"${item.content}" ${item.font_size}px at ${pos}${stateText}`;
  }
}

function stateDesc(state: ProfileState): string {
  return state.duration_ms ? `${formatDuration(state.duration_ms)}` : `toggle`;
}

function copyName(name: string, fallback: string) {
  const base = name.trim() || fallback;
  return `${base} (copy)`;
}

function hotkeyLabel(hotkey: Hotkey): string {
  return hotkey.name.trim() ? `${hotkey.name} - hotkey` : "hotkey";
}

function hotkeyDesc(hotkey: Hotkey, states: ProfileState[]): string {
  const behavior = hotkey.behavior.replace(/state\(([^)]+)\)/g, (_, stateId: string) => {
    return `state(${stateNameById(states, stateId) ?? stateId})`;
  });
  return behavior || "No behavior";
}

function HotkeyRow({ hotkey, states, inherited, onEdit, onCopy, onDelete, onOverride }: {
  hotkey: Hotkey;
  states: ProfileState[];
  inherited: boolean;
  onEdit?: () => void;
  onCopy?: () => void;
  onDelete?: () => void;
  onOverride?: () => void;
}) {
  return (
    <div className={`step-row${inherited ? " step-row--muted" : ""}`}>
      <span className="overlay-type-badge overlay-type-badge--text">{hotkeyLabel(hotkey)}</span>
      <span className="hotkey-row__trigger">{hotkey.trigger}</span>
      <span className="overlay-item-desc">{hotkeyDesc(hotkey, states)}</span>
      <div className="step-row__btns">
        {inherited ? (
          <>
            <button className="icon-btn" title="Copy" onClick={onCopy}>⧉</button>
            <button className="icon-btn" title="Override" onClick={onOverride}>✎</button>
          </>
        ) : (
          <>
            <button className="icon-btn" title="Edit" onClick={onEdit}>✏</button>
            <button className="icon-btn" title="Copy" onClick={onCopy}>⧉</button>
            <button className="icon-btn icon-btn--danger" title="Delete" onClick={onDelete}>✕</button>
          </>
        )}
      </div>
    </div>
  );
}

function OverlayItemRow({ item, states, groups, inherited, onEdit, onCopy, onDelete, onOverride }: {
  item: OverlayItem;
  states: ProfileState[];
  groups: OverlayGroup[];
  inherited: boolean;
  onEdit?: () => void;
  onCopy?: () => void;
  onDelete?: () => void;
  onOverride?: () => void;
}) {
  return (
    <div className={`step-row${inherited ? " step-row--muted" : ""}`}>
      <span className={`overlay-type-badge overlay-type-badge--${item.type}`}>{overlayItemLabel(item, groups)}</span>
      <span className="overlay-item-desc">{overlayItemDesc(item, states)}</span>
      <div className="step-row__btns">
        {inherited ? (
          <>
            <button className="icon-btn" title="Copy" onClick={onCopy}>⧉</button>
            <button className="icon-btn" title="Override" onClick={onOverride}>✎</button>
          </>
        ) : (
          <>
            <button className="icon-btn" title="Edit" onClick={onEdit}>✏</button>
            <button className="icon-btn" title="Copy" onClick={onCopy}>⧉</button>
            <button className="icon-btn icon-btn--danger" title="Delete" onClick={onDelete}>✕</button>
          </>
        )}
      </div>
    </div>
  );
}

function StateRow({ state, inherited, onEdit, onCopy, onDelete, onOverride }: {
  state: ProfileState;
  inherited: boolean;
  onEdit?: () => void;
  onCopy?: () => void;
  onDelete?: () => void;
  onOverride?: () => void;
}) {
  return (
    <div className={`step-row${inherited ? " step-row--muted" : ""}`}>
      <span className="overlay-type-badge overlay-type-badge--text">{state.name} - state</span>
      <span className="overlay-item-desc">{stateDesc(state)}</span>
      <div className="step-row__btns">
        {inherited ? (
          <>
            <button className="icon-btn" title="Copy" onClick={onCopy}>⧉</button>
            <button className="icon-btn" title="Override" onClick={onOverride}>✎</button>
          </>
        ) : (
          <>
            <button className="icon-btn" title="Edit" onClick={onEdit}>✏</button>
            <button className="icon-btn" title="Copy" onClick={onCopy}>⧉</button>
            <button className="icon-btn icon-btn--danger" title="Delete" onClick={onDelete}>✕</button>
          </>
        )}
      </div>
    </div>
  );
}

function OverlayItemModal({ initial, gameExe, states, groups, onSave, onClose }: {
  initial: OverlayItem;
  gameExe: string;
  states: ProfileState[];
  groups: OverlayGroup[];
  onSave: (item: OverlayItem) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [x, setX] = useState(String(initial.x));
  const [y, setY] = useState(String(initial.y));
  const [stateId, setStateId] = useState(initial.state_id ?? "");
  const [groupId, setGroupId] = useState(initial.group_id ?? "");

  const [mins, setMins]         = useState(String(initial.type === "timer" ? Math.floor(initial.duration_ms / 60000) : 1));
  const [secs, setSecs]         = useState(String(initial.type === "timer" ? Math.floor((initial.duration_ms % 60000) / 1000) : 0));
  const [timerColor, setTimerColor] = useState(initial.type === "timer" ? initial.color : "#ffffff");
  const [timerFontSize, setTimerFontSize] = useState(String(initial.type === "timer" ? initial.font_size : 22));
  const [timerStateId, setTimerStateId] = useState(initial.type === "timer" ? (initial.timer_state_id ?? "") : "");

  const [iw, setIw]             = useState(String(initial.type === "icon" ? initial.w : 64));
  const [ih, setIh]             = useState(String(initial.type === "icon" ? initial.h : 64));
  const [src, setSrc]           = useState<string | null>(initial.type === "icon" ? initial.src : null);

  const [bw, setBw]             = useState(String(initial.type === "bar" ? initial.w : 200));
  const [bh, setBh]             = useState(String(initial.type === "bar" ? initial.h : 20));
  const [barColor, setBarColor] = useState(initial.type === "bar" ? initial.color : "#4ade80");
  const [maxVal, setMaxVal]     = useState(String(initial.type === "bar" ? initial.max_value : 100));

  const [fontSize, setFontSize] = useState(String(initial.type === "text" ? initial.font_size : 16));
  const [txtColor, setTxtColor] = useState(initial.type === "text" ? initial.color : "#ffffff");
  const [content, setContent]   = useState(initial.type === "text" ? initial.content : "");

  async function browseIcon() {
    const selected = await openDialog({ title: "Select Icon", filters: [{ name: "Image", extensions: ["png","jpg","jpeg","webp","gif","ico"] }], multiple: false, directory: false });
    if (!selected) return;
    try { setSrc(await api.readImageAsDataUrl(selected as string)); } catch (e) { alert(String(e)); }
  }

  function build(): OverlayItem {
    const base = {
      id: initial.id,
      name: name.trim(),
      x: parseFloat(x)||0,
      y: parseFloat(y)||0,
      state_id: stateId || null,
      group_id: groupId || null,
    };
    const timerMs = ((parseInt(mins)||0)*60 + (parseInt(secs)||0)) * 1000;
    switch (initial.type) {
      case "timer": return { ...base, type: "timer", duration_ms: timerMs||60000, color: timerColor, font_size: parseInt(timerFontSize)||22, timer_state_id: timerStateId || null };
      case "icon":  return { ...base, type: "icon",  w: parseInt(iw)||64, h: parseInt(ih)||64, src };
      case "bar":   return { ...base, type: "bar",   w: parseInt(bw)||200, h: parseInt(bh)||20, color: barColor, max_value: parseFloat(maxVal)||100 };
      case "text":  return { ...base, type: "text",  font_size: parseInt(fontSize)||16, color: txtColor, content };
    }
  }

  const typeLabel = initial.type.charAt(0).toUpperCase() + initial.type.slice(1);

  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>{typeLabel}</h2>

        <input value={name} onChange={e => setName(e.target.value)} placeholder="Name" />

        <select value={stateId} onChange={e => setStateId(e.target.value)}>
          <option value="">Always visible</option>
          {states.map(state => (
            <option key={state.id} value={state.id}>Visible with {state.name}</option>
          ))}
        </select>

        {groups.length > 0 && (
          <select value={groupId} onChange={e => setGroupId(e.target.value)}>
            <option value="">No group</option>
            {groups.map(g => (
              <option key={g.id} value={g.id}>In group {g.name}</option>
            ))}
          </select>
        )}

        <GotoInput x={x} y={y} gameExe={gameExe} onChange={(nx, ny) => { setX(nx); setY(ny); }} />

        {initial.type === "timer" && <>
          <div className="form-grid form-grid--2">
            <div className="duration-input">
              <input type="number" value={mins} onChange={e => setMins(e.target.value)} placeholder="min" min={0} />
              <span>m</span>
              <input type="number" value={secs} onChange={e => setSecs(e.target.value)} placeholder="sec" min={0} max={59} />
              <span>s</span>
            </div>
            <input type="number" value={timerFontSize} onChange={e => setTimerFontSize(e.target.value)} placeholder="Font size" />
          </div>
          <div className="color-input">
            <input className="color-input__swatch" type="color" value={timerColor} onChange={e => setTimerColor(e.target.value)} />
            <input value={timerColor} onChange={e => setTimerColor(e.target.value)} placeholder="#RRGGBB" />
          </div>
          <select value={timerStateId} onChange={e => setTimerStateId(e.target.value)}>
            <option value="">Use fixed duration above</option>
            {states.filter(state => !!state.duration_ms).map(state => (
              <option key={state.id} value={state.id}>Use timer from {state.name}</option>
            ))}
          </select>
        </>}

        {initial.type === "icon" && <>
          <div className="image-picker" onClick={browseIcon}>
            {src ? (
              <img
                src={src}
                className="image-picker__preview image-picker__preview--contain"
                alt="icon"
                style={{
                  width: `${Math.max(1, parseInt(iw) || 64)}px`,
                  height: `${Math.max(1, parseInt(ih) || 64)}px`,
                }}
              />
            ) : <Placeholder />}
          </div>
          {src && <button className="btn btn--ghost btn--sm modal-inline-action" onClick={() => setSrc(null)}>✕ Remove</button>}
          <div className="form-grid form-grid--2">
            <input type="number" value={iw} onChange={e => setIw(e.target.value)} placeholder="Width" />
            <input type="number" value={ih} onChange={e => setIh(e.target.value)} placeholder="Height" />
          </div>
        </>}

        {initial.type === "bar" && <>
          <div className="form-grid form-grid--3">
            <input type="number" value={bw} onChange={e => setBw(e.target.value)} placeholder="Width" />
            <input type="number" value={bh} onChange={e => setBh(e.target.value)} placeholder="Height" />
            <input type="number" value={maxVal} onChange={e => setMaxVal(e.target.value)} placeholder="Max" />
          </div>
          <div className="color-input">
            <input className="color-input__swatch" type="color" value={barColor} onChange={e => setBarColor(e.target.value)} />
            <input value={barColor} onChange={e => setBarColor(e.target.value)} placeholder="#RRGGBB" />
          </div>
        </>}

        {initial.type === "text" && <>
          <input value={content} onChange={e => setContent(e.target.value)} placeholder="Text" />
          <div className="form-grid form-grid--2">
            <input type="number" value={fontSize} onChange={e => setFontSize(e.target.value)} placeholder="Font size" />
            <div className="color-input">
              <input className="color-input__swatch" type="color" value={txtColor} onChange={e => setTxtColor(e.target.value)} />
              <input value={txtColor} onChange={e => setTxtColor(e.target.value)} placeholder="#RRGGBB" />
            </div>
          </div>
        </>}

        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => onSave(build())}>Save</button>
        </div>
      </div>
    </div>
  );
}

function StateModal({ initial, onSave, onClose }: {
  initial: ProfileState;
  onSave: (state: ProfileState) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [mins, setMins] = useState(String(Math.floor((initial.duration_ms ?? 0) / 60000)));
  const [secs, setSecs] = useState(String(Math.floor(((initial.duration_ms ?? 0) % 60000) / 1000)));
  const durationMs = ((parseInt(mins) || 0) * 60 + (parseInt(secs) || 0)) * 1000;

  function build(): ProfileState {
    return {
      id: initial.id,
      name: name.trim(),
      duration_ms: durationMs > 0 ? durationMs : null,
    };
  }

  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>State</h2>

        <input className="modal-name" value={name} onChange={e => setName(e.target.value)} placeholder="State name" autoFocus />

        <div className="duration-input">
          <input type="number" value={mins} onChange={e => setMins(e.target.value)} min={0} placeholder="min" />
          <span>m</span>
          <input type="number" value={secs} onChange={e => setSecs(e.target.value)} min={0} max={59} placeholder="sec" />
          <span>s</span>
        </div>

        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => onSave(build())} disabled={!name.trim()}>Save</button>
        </div>
      </div>
    </div>
  );
}

function scriptTriggerDesc(script: Script): string {
  return script.trigger === "hotkey"
    ? `hotkey: ${script.hotkey || "(unset)"}`
    : "on app launch";
}

function scriptLanguageLabel(script: Script): string {
  return script.language === "autohotkey" ? "AutoHotkey" : "Python";
}

function ScriptRow({ script, onEdit, onToggle, onDelete }: {
  script: Script;
  onEdit: () => void;
  onToggle: () => void;
  onDelete: () => void;
}) {
  return (
    <div className={`step-row${script.enabled ? "" : " step-row--muted"}`}>
      <span className="overlay-type-badge overlay-type-badge--text">{script.name || "Unnamed"} - script</span>
      <span className="overlay-item-desc">{scriptTriggerDesc(script)} · {scriptLanguageLabel(script)} · {script.source === "path" ? "file" : "inline"}</span>
      <div className="step-row__btns">
        <button className="icon-btn" title={script.enabled ? "Disable" : "Enable"} onClick={onToggle}>{script.enabled ? "◉" : "○"}</button>
        <button className="icon-btn" title="Edit" onClick={onEdit}>✏</button>
        <button className="icon-btn icon-btn--danger" title="Delete" onClick={onDelete}>✕</button>
      </div>
    </div>
  );
}

function ScriptModal({ initial, onSave, onClose }: {
  initial: Script;
  onSave: (script: Script) => void;
  onClose: () => void;
}) {
  const [name, setName] = useState(initial.name);
  const [enabled, setEnabled] = useState(initial.enabled);
  const [trigger, setTrigger] = useState(initial.trigger);
  const [hotkey, setHotkey] = useState(initial.hotkey);
  const [language, setLanguage] = useState(initial.language ?? "python");
  const [source, setSource] = useState(initial.source);
  const [code, setCode] = useState(initial.code);
  const [path, setPath] = useState(initial.path);

  function build(): Script {
    return { id: initial.id, name: name.trim(), enabled, trigger, hotkey, language, source, code, path: path.trim() };
  }

  async function browse() {
    const isAhk = language === "autohotkey";
    const selected = await openDialog({
      title: `Select ${isAhk ? "AutoHotkey" : "Python"} Script`,
      filters: [{ name: isAhk ? "AutoHotkey" : "Python", extensions: [isAhk ? "ahk" : "py"] }],
      multiple: false,
      directory: false,
    });
    if (selected) setPath(selected as string);
  }

  return (
    <div className="modal-overlay">
      <div className="modal" onClick={e => e.stopPropagation()}>
        <h2>{initial.name ? "Edit Script" : "Add Script"}</h2>

        <input className="modal-name" value={name} onChange={e => setName(e.target.value)} placeholder="Script name" autoFocus />

        <select value={trigger} onChange={e => setTrigger(e.target.value as Script["trigger"])}>
          <option value="hotkey">When a hotkey is pressed</option>
          <option value="launch">When the app is launched</option>
        </select>

        {trigger === "hotkey" && (
          <KeyInput value={hotkey} onChange={setHotkey} placeholder="Hotkey" />
        )}

        <select value={language} onChange={e => setLanguage(e.target.value as Script["language"])}>
          <option value="python">Python</option>
          <option value="autohotkey">AutoHotkey v2</option>
        </select>

        <select value={source} onChange={e => setSource(e.target.value as Script["source"])}>
          <option value="code">Inline code</option>
          <option value="path">Path to a .{language === "autohotkey" ? "ahk" : "py"} file</option>
        </select>

        {source === "code" ? (
          <textarea value={code} onChange={e => setCode(e.target.value)} rows={12} spellCheck={false}
            style={{ fontFamily: "monospace", resize: "vertical" }}
            placeholder={language === "autohotkey" ? '#Requires AutoHotkey v2.0\nMsgBox "Hello"' : "print('hello')"} />
        ) : (
          <div className="input-row">
            <input value={path} onChange={e => setPath(e.target.value)}
              placeholder={`C:\\path\\to\\script.${language === "autohotkey" ? "ahk" : "py"}`} />
            <button className="btn btn--ghost" onClick={browse}>Browse…</button>
          </div>
        )}

        <label className="checkbox-row">
          <input type="checkbox" checked={enabled} onChange={e => setEnabled(e.target.checked)} />
          <span>Enabled</span>
        </label>

        <div className="modal__actions">
          <button className="btn btn--ghost" onClick={onClose}>Cancel</button>
          <button className="btn btn--primary" onClick={() => onSave(build())} disabled={!name.trim()}>Save</button>
        </div>
      </div>
    </div>
  );
}

// ── Game detail view ──────────────────────────────────────────────────────────

function ProfileEditor({ folder, profile, showStates, onExitStates, onContext, onDb, onModal }: {
  folder: Scope;
  profile: Profile;
  showStates: boolean;
  onExitStates: () => void;
  onContext: (e: ReactMouseEvent) => void;
  onDb: (db: Database) => void;
  onModal: (m: Modal) => void;
}) {
  const profileId = profile.id;
  const profileExe = folder.exe;

  async function setArmed(armed: boolean) {
    try { onDb(await api.setProfileArmed(profile.id, armed)); } catch (e) { alert(String(e)); }
  }
  async function saveProfile(updated: Profile) {
    onDb(await api.upsertProfile(folder.id, updated));
  }
  async function deleteHotkey(index: number) {
    onDb(await api.upsertProfile(folder.id, { ...profile, hotkeys: profile.hotkeys.filter((_, i) => i !== index) }));
  }
  async function saveScripts(scripts: Script[]) {
    try { onDb(await api.upsertProfile(folder.id, { ...profile, scripts })); } catch (e) { alert(String(e)); }
  }

  const resolvedHotkeys = resolveHotkeys(folder.profiles, profile);
  const resolvedStates = resolveStates(folder.profiles, profile);
  const resolvedOverlayItems = resolveOverlayItems(folder.profiles, profile);
  const stateOptions = resolvedStates.map(({ state }) => state);

  return (
    <div className="editor">
      <div className="editor__header" onContextMenu={onContext}>
        <div className="editor__title-row">
          <div className="editor__title-wrap">
            <span className="editor__title">{profile.name || "Untitled profile"}</span>
            <TargetChip exe={profileExe} />
          </div>
          <div style={{ flex: 1 }} />
          <ArmSwitch armed={profile.armed} size="lg" onChange={setArmed} />
        </div>
      </div>

      <div className="editor__content">
        {showStates ? (
          <>
            <div className="section-head">
              <button className="btn btn--ghost btn--sm" onClick={onExitStates}>‹ Back</button>
              <h3 style={{ flex: 1 }}>States</h3>
              <button className="btn btn--primary btn--sm" onClick={() => onModal({ type: "profileState", gameId: folder.id, profileId, index: null, state: blankState() })}>+ State</button>
            </div>
            <div className="steps-list">
              {resolvedStates.map(({ state, own }) => {
                const ownIndex = own ? profile.states.findIndex(candidate => candidate.id === state.id) : -1;
                return (
                  <StateRow key={state.id} state={state} inherited={!own}
                    onEdit={own ? () => onModal({ type: "profileState", gameId: folder.id, profileId, index: ownIndex, state }) : undefined}
                    onCopy={() => onModal({ type: "copyState", sourceGameId: folder.id, sourceProfileId: profileId, state })}
                    onDelete={own ? async () => {
                      const remainingStates = profile.states.filter((_, idx) => idx !== ownIndex);
                      const updated = {
                        ...profile,
                        states: remainingStates,
                        hotkeys: profile.hotkeys.map(hotkey => hotkey.state_id === state.id ? { ...hotkey, state_id: null } : hotkey),
                        overlay_items: profile.overlay_items.map(item => item.type === "timer"
                          ? { ...item, state_id: item.state_id === state.id ? null : item.state_id, timer_state_id: item.timer_state_id === state.id ? null : item.timer_state_id }
                          : { ...item, state_id: item.state_id === state.id ? null : item.state_id }),
                      };
                      onDb(await api.upsertProfile(folder.id, updated));
                    } : undefined}
                    onOverride={!own ? async () => {
                      onDb(await api.upsertProfile(folder.id, { ...profile, states: [...profile.states, { ...state }] }));
                    } : undefined} />
                );
              })}
              {resolvedStates.length === 0 && <div className="steps-empty">No states yet</div>}
            </div>
          </>
        ) : profile.kind === "hotkeys" ? (
          <>
            <div className="section-head">
              <h3>Hotkeys</h3>
              <button className="btn btn--primary btn--sm"
                onClick={() => onModal({ type: "editHotkey", gameId: folder.id, profileId, index: null, hotkey: { name: "", trigger: "", behavior: "", state_id: null }, gameExe: profileExe, states: stateOptions })}>
                + Hotkey
              </button>
            </div>
            <div className="steps-list">
              {resolvedHotkeys.map(({ hotkey: hk, own }, i) => {
                const ownIndex = own ? profile.hotkeys.findIndex(h => h.trigger === hk.trigger) : -1;
                return (
                  <HotkeyRow key={i} hotkey={hk} states={stateOptions} inherited={!own}
                    onEdit={() => onModal({ type: "editHotkey", gameId: folder.id, profileId, index: ownIndex, hotkey: hk, gameExe: profileExe, states: stateOptions })}
                    onCopy={() => onModal({ type: "copyHotkey", sourceGameId: folder.id, sourceProfileId: profileId, hotkey: hk })}
                    onDelete={() => deleteHotkey(ownIndex)}
                    onOverride={() => saveProfile({ ...profile, hotkeys: [...profile.hotkeys, { ...hk }] })} />
                );
              })}
              {resolvedHotkeys.length === 0 && <div className="steps-empty">No hotkeys yet</div>}
            </div>
          </>
        ) : profile.kind === "scripts" ? (
          <>
            <div className="section-head">
              <h3>Scripts</h3>
              <button className="btn btn--primary btn--sm"
                onClick={() => onModal({ type: "script", gameId: folder.id, profileId, index: null, script: blankScript() })}>
                + Script
              </button>
            </div>
            <div className="steps-list">
              {profile.scripts.map((script, i) => (
                <ScriptRow key={script.id} script={script}
                  onEdit={() => onModal({ type: "script", gameId: folder.id, profileId, index: i, script })}
                  onToggle={() => saveScripts(profile.scripts.map((s, idx) => idx === i ? { ...s, enabled: !s.enabled } : s))}
                  onDelete={() => saveScripts(profile.scripts.filter((_, idx) => idx !== i))} />
              ))}
              {profile.scripts.length === 0 && <div className="steps-empty">No scripts yet — scripts run from their own folder.</div>}
            </div>
          </>
        ) : (
          <>
            <div className="section-head">
              <h3>Overlay</h3>
              <div style={{ display: "flex", gap: 6 }}>
                <button className="btn btn--ghost btn--sm" onClick={() => onModal({ type: "overlayItem", gameId: folder.id, profileId, index: null, item: blankTimer(), gameExe: profileExe, states: stateOptions, groups: profile.overlay_groups ?? [] })}>Timer</button>
                <button className="btn btn--ghost btn--sm" onClick={() => onModal({ type: "overlayItem", gameId: folder.id, profileId, index: null, item: blankIcon(),  gameExe: profileExe, states: stateOptions, groups: profile.overlay_groups ?? [] })}>Icon</button>
                <button className="btn btn--ghost btn--sm" onClick={() => onModal({ type: "overlayItem", gameId: folder.id, profileId, index: null, item: blankBar(),   gameExe: profileExe, states: stateOptions, groups: profile.overlay_groups ?? [] })}>Bar</button>
                <button className="btn btn--ghost btn--sm" onClick={() => onModal({ type: "overlayItem", gameId: folder.id, profileId, index: null, item: blankText(),  gameExe: profileExe, states: stateOptions, groups: profile.overlay_groups ?? [] })}>Text</button>
                <button className="btn btn--ghost btn--sm" onClick={async () => {
                  const name = prompt("Group name:");
                  if (!name?.trim()) return;
                  const group: OverlayGroup = { id: uid(), name: name.trim() };
                  onDb(await api.upsertProfile(folder.id, { ...profile, overlay_groups: [...(profile.overlay_groups ?? []), group] }));
                }}>+ Group</button>
              </div>
            </div>
            {(profile.overlay_groups ?? []).length > 0 && (
              <div className="steps-list">
                {(profile.overlay_groups ?? []).map(group => (
                  <div key={group.id} className="step-row">
                    <span className="overlay-type-badge overlay-type-badge--text">{group.name} - group</span>
                    <span className="overlay-item-desc">{resolvedOverlayItems.filter(({ item }) => item.group_id === group.id).length} items</span>
                    <div className="step-row__btns">
                      <button className="icon-btn icon-btn--danger" title="Delete group" onClick={async () => {
                        const updated = {
                          ...profile,
                          overlay_groups: (profile.overlay_groups ?? []).filter(g => g.id !== group.id),
                          overlay_items: profile.overlay_items.map(item => item.group_id === group.id ? { ...item, group_id: null } : item),
                        };
                        onDb(await api.upsertProfile(folder.id, updated));
                      }}>✕</button>
                    </div>
                  </div>
                ))}
              </div>
            )}
            <div className="steps-list" style={{ marginTop: 8 }}>
              {resolvedOverlayItems.map(({ item, own }) => {
                const ownIndex = own ? profile.overlay_items.findIndex(candidate => candidate.id === item.id) : -1;
                return (
                  <OverlayItemRow key={item.id} item={item} states={stateOptions} groups={profile.overlay_groups ?? []} inherited={!own}
                    onEdit={own ? () => onModal({ type: "overlayItem", gameId: folder.id, profileId, index: ownIndex, item, gameExe: profileExe, states: stateOptions, groups: profile.overlay_groups ?? [] }) : undefined}
                    onCopy={() => onModal({ type: "copyOverlayItem", sourceGameId: folder.id, sourceProfileId: profileId, item })}
                    onDelete={own ? async () => {
                      const updated = { ...profile, overlay_items: profile.overlay_items.filter((_, idx) => idx !== ownIndex) };
                      onDb(await api.upsertProfile(folder.id, updated));
                    } : undefined}
                    onOverride={!own ? async () => {
                      onDb(await api.upsertProfile(folder.id, { ...profile, overlay_items: [...profile.overlay_items, { ...item }] }));
                    } : undefined} />
                );
              })}
              {resolvedOverlayItems.length === 0 && <div className="steps-empty">No overlay items yet</div>}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

// ── Settings view ─────────────────────────────────────────────────────────────

function SettingsView({ db, onDb, onCheckUpdates }: {
  db: Database;
  onDb: (db: Database) => void;
  onCheckUpdates: () => Promise<string | null>;
}) {
  const [ahkExe, setAhkExe] = useState(db.settings.ahk_exe);
  const [pythonExe, setPythonExe] = useState(db.settings.python_exe ?? "");
  const [openToTray, setOpenToTray] = useState(db.settings.open_to_tray);
  const [closeToTray, setCloseToTray] = useState(db.settings.close_to_tray);
  const [launchOnStartup, setLaunchOnStartup] = useState(db.settings.launch_on_startup);
  const [saved, setSaved] = useState(false);
  const [checking, setChecking] = useState(false);
  const [updateStatus, setUpdateStatus] = useState<string | null>(null);

  async function checkForUpdates() {
    setChecking(true);
    setUpdateStatus(null);
    try {
      const latest = await onCheckUpdates();
      setUpdateStatus(latest ? `Update ${latest} is available.` : "You're up to date.");
    } catch {
      setUpdateStatus("Could not check for updates.");
    }
    setChecking(false);
  }

  async function browse() {
    const selected = await openDialog({
      title: "Select AutoHotkey Executable",
      filters: [{ name: "Executable", extensions: ["exe"] }],
      multiple: false,
      directory: false,
    });
    if (selected) setAhkExe(selected as string);
  }

  async function browsePython() {
    const selected = await openDialog({
      title: "Select Python Executable",
      filters: [{ name: "Executable", extensions: ["exe"] }],
      multiple: false,
      directory: false,
    });
    if (selected) setPythonExe(selected as string);
  }

  async function save() {
    try {
      const updated = await api.saveSettings({
        ahk_exe: ahkExe,
        python_exe: pythonExe,
        open_to_tray: openToTray,
        close_to_tray: closeToTray,
        launch_on_startup: launchOnStartup,
      });
      onDb(updated);
      setSaved(true);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) { alert(`Error saving settings: ${e}`); }
  }

  return (
    <div className="settings-view">
      <h2>Settings</h2>
      <div className="input-row">
        <input value={ahkExe} onChange={e => setAhkExe(e.target.value)} placeholder="AutoHotkey v2 executable path" />
        <button className="btn btn--ghost" onClick={browse}>Browse…</button>
      </div>
      <div className="input-row">
        <input value={pythonExe} onChange={e => setPythonExe(e.target.value)} placeholder="Python executable path" />
        <button className="btn btn--ghost" onClick={browsePython}>Browse…</button>
      </div>
      <label className="checkbox-row">
        <input type="checkbox" checked={openToTray} onChange={e => setOpenToTray(e.target.checked)} />
        <span>Open to tray</span>
      </label>
      <label className="checkbox-row">
        <input type="checkbox" checked={closeToTray} onChange={e => setCloseToTray(e.target.checked)} />
        <span>Close to tray</span>
      </label>
      <label className="checkbox-row">
        <input type="checkbox" checked={launchOnStartup} onChange={e => setLaunchOnStartup(e.target.checked)} />
        <span>Launch on startup</span>
      </label>
      <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
        <button className="btn btn--primary" onClick={save}>Save</button>
        {saved && <span style={{ color: "var(--success)", fontSize: "0.88rem" }}>✓ Saved</span>}
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
        <button className="btn btn--ghost" onClick={checkForUpdates} disabled={checking}>
          {checking ? "Checking…" : "Check for Updates"}
        </button>
        {updateStatus && <span style={{ fontSize: "0.88rem" }}>{updateStatus}</span>}
      </div>
    </div>
  );
}

// ── App ───────────────────────────────────────────────────────────────────────

function semverGt(a: string, b: string): boolean {
  const parse = (v: string) => v.replace(/^v/, "").split(".").map(Number);
  const [aMaj, aMin, aPat] = parse(a);
  const [bMaj, bMin, bPat] = parse(b);
  if (aMaj !== bMaj) return aMaj > bMaj;
  if (aMin !== bMin) return aMin > bMin;
  return aPat > bPat;
}

export default function App() {
  const [db, setDb] = useState<Database | null>(null);
  const [selection, setSelection] = useState<Selection>({ kind: "empty" });
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [openExes, setOpenExes] = useState<Set<string>>(new Set());
  const [ctx, setCtx] = useState<{ x: number; y: number; items: CtxItem[] } | null>(null);
  const [statesFor, setStatesFor] = useState<string | null>(null);
  const collapseInited = useRef(false);
  const [modal, setModal] = useState<Modal | null>(null);
  const [updateInfo, setUpdateInfo] = useState<{ version: string; downloadUrl: string | null; notesUrl: string } | null>(null);
  const [updating, setUpdating] = useState(false);
  // The version the user dismissed, so the periodic re-check doesn't keep re-surfacing it.
  const dismissedUpdate = useRef<string | null>(null);
  // The version already announced via system notification, so the periodic re-check
  // doesn't toast the same release every 30 minutes.
  const notifiedUpdate = useRef<string | null>(null);
  const launchUpdateCheckStarted = useRef(false);
  const { w: libW, onPointerDown: onResize } = useLibraryWidth();

  const loadDb = useCallback(async () => {
    const data = await api.getDatabase();
    setDb(data);
  }, []);

  // Check for updates against the latest GitHub release. Returns the new version (and shows
  // the banner) if one is available, null when up to date; throws on network failure. A
  // manual check (the Settings button) ignores a previous dismissal.
  const checkUpdate = useCallback(async (manual = false): Promise<string | null> => {
    const current = await api.getAppVersion();
    const data = await (await fetch("https://api.github.com/repos/facufierro/MacroToolbox/releases/latest")).json();
    const latest: string = data.tag_name ?? "";
    if (!latest || !semverGt(latest, current) || (!manual && dismissedUpdate.current === latest)) return null;
    const asset = (data.assets as { name: string; browser_download_url: string }[])
      ?.find(a => a.name.endsWith("-setup.exe") || a.name.endsWith(".exe"));
    setUpdateInfo({
      version: latest,
      downloadUrl: asset?.browser_download_url ?? null,
      notesUrl: data.html_url,
    });
    // The app usually sits in the tray, so also announce the update with a system
    // notification — the in-app banner is invisible until the window is opened. A manual
    // check skips the toast: the user is already looking at the banner.
    if (!manual && notifiedUpdate.current !== latest) {
      notifiedUpdate.current = latest;
      try {
        let granted = await isPermissionGranted();
        if (!granted) granted = (await requestPermission()) === "granted";
        if (granted) {
          sendNotification({
            title: "MacroToolbox update available",
            body: `Version ${latest} is ready. Open MacroToolbox to install it.`,
          });
        }
      } catch { /* ignore */ }
    }
    return latest;
  }, []);

  useEffect(() => {
    loadDb();
    const poll = async () => {
      try { setOpenExes(new Set((await api.listOpenExecutables()).map(e => e.toLowerCase()))); } catch { /* ignore */ }
    };
    poll();
    const id = setInterval(poll, 2000);

    // An available update takes precedence over "Open to tray" at launch so the install banner
    // is visible immediately. Later checks retain the normal tray behavior and use a notification.
    const backgroundCheck = () => { checkUpdate().catch(() => {}); };
    if (!launchUpdateCheckStarted.current) {
      launchUpdateCheckStarted.current = true;
      checkUpdate()
        .then(latest => { if (latest) return api.revealMainWindow(); })
        .catch(() => {});
    }

    // Keep checking periodically: the app can live in the tray for days, so a one-shot check
    // would miss any release published while it stays open.
    const updateId = setInterval(backgroundCheck, 30 * 60 * 1000);
    // WebView2 throttles timers while the window is hidden in the tray, so the periodic
    // check can lag by an hour or more. Re-check whenever the window becomes visible so
    // opening the app always reflects the latest release. visibilitychange alone is not
    // enough: WebView2 doesn't reliably fire it for native show()/hide(), so the backend
    // also emits main-window-shown whenever it restores the window.
    const onVisible = () => { if (!document.hidden) backgroundCheck(); };
    document.addEventListener("visibilitychange", onVisible);
    const unlistenShown = listen("main-window-shown", backgroundCheck);

    return () => {
      clearInterval(id);
      clearInterval(updateId);
      document.removeEventListener("visibilitychange", onVisible);
      unlistenShown.then(f => f());
    };
  }, [loadDb, checkUpdate]);

  // Suppress the browser's native right-click menu (this is a desktop app), but keep it in
  // text fields so paste still works.
  useEffect(() => {
    const onCtx = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("input, textarea, [contenteditable]")) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", onCtx);
    return () => document.removeEventListener("contextmenu", onCtx);
  }, []);

  function handleDb(updated: Database) {
    setDb(updated);
  }

  async function runUpdate() {
    if (!updateInfo?.downloadUrl) return;
    const ok = await ask(
      `MacroToolbox ${updateInfo.version} is available.\n\nDownload and install it now? The app will close to finish updating.`,
      { title: "Update available", kind: "info", okLabel: "Update now", cancelLabel: "Later" }
    );
    if (!ok) return;
    setUpdating(true);
    try {
      // On success the installer launches and the app exits, so control never
      // returns here; a rejection means the download or launch failed.
      await api.downloadAndInstallUpdate(updateInfo.downloadUrl);
    } catch (e) {
      setUpdating(false);
      alert(`Update failed: ${e}`);
    }
  }

  useEffect(() => {
    if (!db) return;
    // Every folder starts collapsed on first load.
    if (!collapseInited.current) {
      collapseInited.current = true;
      setCollapsed(new Set(db.scopes.map(s => s.id)));
    }
    // Heal a selection whose profile/folder was deleted; otherwise leave it (no auto-select).
    setSelection(sel => {
      if (sel.kind !== "profile") return sel;
      const folder = db.scopes.find(s => s.id === sel.folderId);
      if (folder?.profiles.some(p => p.id === sel.profileId)) return sel;
      if (folder?.profiles[0]) return { kind: "profile", folderId: folder.id, profileId: folder.profiles[0].id };
      return { kind: "empty" };
    });
  }, [db]);

  useEffect(() => {
    if (selection.kind === "profile") {
      setCollapsed(prev => { if (!prev.has(selection.folderId)) return prev; const n = new Set(prev); n.delete(selection.folderId); return n; });
    }
  }, [selection]);

  function toggleCollapse(id: string) {
    setCollapsed(prev => { const n = new Set(prev); n.has(id) ? n.delete(id) : n.add(id); return n; });
  }

  async function toggleArmed(profileId: string, armed: boolean) {
    try { handleDb(await api.setProfileArmed(profileId, armed)); } catch (e) { alert(String(e)); }
  }

  function selectProfile(sel: Selection) {
    setSelection(sel);
    setStatesFor(null);
  }

  function profileMenu(folderId: string, p: Profile): CtxItem[] {
    return [
      { label: "Settings", onClick: () => setModal({ type: "profileSettings", gameId: folderId, profile: p, isNew: false }) },
      { label: p.parent_id ? "Change parent" : "Inherit from…", onClick: () => setModal({ type: "setParent", gameId: folderId, profile: p }) },
      { label: "Copy to…", onClick: () => setModal({ type: "copyProfile", sourceGameId: folderId, profile: p }) },
      ...(p.kind !== "scripts" ? [{ label: "Edit states…", onClick: () => { setSelection({ kind: "profile", folderId, profileId: p.id }); setStatesFor(p.id); } }] : []),
      { label: "Export profile", onClick: () => exportProfile(p).catch(e => alert(String(e))) },
      { label: "Delete profile", danger: true, onClick: async () => { if (!confirm(`Delete profile "${p.name}"?`)) return; try { handleDb(await api.deleteProfile(folderId, p.id)); } catch (e) { alert(String(e)); } } },
    ];
  }

  function onProfileContext(e: ReactMouseEvent, folderId: string, p: Profile) {
    e.preventDefault();
    e.stopPropagation();
    setCtx({ x: e.clientX, y: e.clientY, items: profileMenu(folderId, p) });
  }

  function folderMenu(folder: Scope): CtxItem[] {
    const newProfile = (kind: ProfileKind) => setModal({ type: "profileSettings", gameId: folder.id, profile: { ...blankProfile(), kind }, isNew: true });
    const armAll = async (target: boolean) => {
      try { let db: Database | null = null; for (const p of folder.profiles) db = await api.setProfileArmed(p.id, target); if (db) handleDb(db); }
      catch (e) { alert(String(e)); }
    };
    const isGlobalFolder = folder.id === GLOBAL_FOLDER_ID;
    const targetExe = folder.exe.trim();
    const hasProcessTarget = targetExe !== "" && !isGlobalExe(targetExe);
    return [
      { label: "New hotkeys profile", onClick: () => newProfile("hotkeys") },
      { label: "New scripts profile", onClick: () => newProfile("scripts") },
      { label: "New overlay profile", onClick: () => newProfile("overlay") },
      { label: "Import profile…", onClick: async () => {
        try { const imported = await importProfile(); if (!imported) return; imported.name = imported.name || "Imported Profile"; handleDb(await api.upsertProfile(folder.id, imported)); }
        catch (e) { alert(String(e)); }
      } },
      { label: "Arm all", onClick: () => armAll(true) },
      { label: "Disarm all", onClick: () => armAll(false) },
      { label: "Folder settings…", onClick: () => setModal({ type: "editGame", game: folder }) },
      { label: "Export folder", onClick: () => exportScope(folder).catch(e => alert(String(e))) },
      ...(hasProcessTarget ? [
        { label: "Toggle borderless", onClick: async () => {
          try { await api.toggleBorderless(targetExe); } catch (e) { alert(String(e)); }
        } },
        { label: "Toggle stretch", onClick: async () => {
          try { await api.toggleStretch(targetExe); } catch (e) { alert(String(e)); }
        } },
        { label: "Kill process", onClick: async () => {
          if (!confirm(`Force-kill "${targetExe}"?`)) return;
          try { await api.killGame(targetExe); } catch (e) { alert(String(e)); }
        } },
      ] : []),
      ...(!isGlobalFolder ? [{ label: "Delete folder", danger: true, onClick: async () => {
        if (!confirm(`Delete folder "${folder.name}" and all its profiles?`)) return;
        try { handleDb(await api.deleteGame(folder.id)); } catch (e) { alert(String(e)); }
      } }] : []),
    ];
  }

  function onFolderContext(e: ReactMouseEvent, folder: Scope) {
    e.preventDefault();
    setCtx({ x: e.clientX, y: e.clientY, items: folderMenu(folder) });
  }

  // Modal handlers
  async function handleGameSave(game: Scope) {
    try {
      handleDb(await api.upsertGame(game));
      setModal(null);
    } catch (e) { alert(`Error saving folder: ${e}`); }
  }

  async function handleProfileSettingsSave(gameId: string, profile: Profile, isNew: boolean) {
    try {
      handleDb(await api.upsertProfile(gameId, profile));
      setModal(null);
      if (isNew) setSelection({ kind: "profile", folderId: gameId, profileId: profile.id });
    } catch (e) { alert(`Error saving profile: ${e}`); }
  }

  async function handleSetParent(gameId: string, profile: Profile, parentId: string | null) {
    try {
      const updated = await api.upsertProfile(gameId, { ...profile, parent_id: parentId });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error setting parent: ${e}`); }
  }

  async function handleOverlayItemSave(gameId: string, profileId: string, index: number | null, item: OverlayItem) {
    try {
      const game = db!.scopes.find(g => g.id === gameId)!;
      const profile = game.profiles.find(p => p.id === profileId)!;
      const items = [...profile.overlay_items];
      if (index === null) items.push(item);
      else items[index] = item;
      const updated = await api.upsertProfile(gameId, { ...profile, overlay_items: items });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error saving overlay item: ${e}`); }
  }

  async function handleStateSave(gameId: string, profileId: string, index: number | null, state: ProfileState) {
    try {
      const game = db!.scopes.find(g => g.id === gameId)!;
      const profile = game.profiles.find(p => p.id === profileId)!;
      const states = [...profile.states];
      if (index === null) states.push(state);
      else states[index] = state;
      const updated = await api.upsertProfile(gameId, { ...profile, states });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error saving state: ${e}`); }
  }

  async function handleCopyProfile(sourceGameId: string, profile: Profile, targetGameId: string, name: string) {
    void sourceGameId;
    try {
      const copy: Profile = {
        ...profile,
        id: uid(),
        name,
        armed: false,
        parent_id: null,
        scripts: profile.scripts.map(s => ({ ...s, id: uid() })),
      };
      const updated = await api.upsertProfile(targetGameId, copy);
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error copying profile: ${e}`); }
  }

  async function handleCopyHotkey(sourceGameId: string, sourceProfileId: string, hotkey: Hotkey, targetGameId: string, targetProfileId: string) {
    try {
      const game = db!.scopes.find(candidate => candidate.id === targetGameId)!;
      const profile = game.profiles.find(candidate => candidate.id === targetProfileId)!;
      const isSameProfile = sourceGameId === targetGameId && sourceProfileId === targetProfileId;
      const copy = isSameProfile
        ? { ...hotkey, name: copyName(hotkey.name, hotkey.trigger || "Behavior"), trigger: "" }
        : { ...hotkey };
      const updated = await api.upsertProfile(targetGameId, { ...profile, hotkeys: [...profile.hotkeys, copy] });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error copying hotkey: ${e}`); }
  }

  async function handleCopyOverlayItem(sourceGameId: string, sourceProfileId: string, item: OverlayItem, targetGameId: string, targetProfileId: string) {
    try {
      const game = db!.scopes.find(candidate => candidate.id === targetGameId)!;
      const profile = game.profiles.find(candidate => candidate.id === targetProfileId)!;
      const isSameProfile = sourceGameId === targetGameId && sourceProfileId === targetProfileId;
      const copy = {
        ...item,
        id: uid(),
        name: isSameProfile ? copyName(item.name, item.type) : item.name,
      };
      const updated = await api.upsertProfile(targetGameId, { ...profile, overlay_items: [...profile.overlay_items, copy] });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error copying overlay item: ${e}`); }
  }

  async function handleCopyState(sourceGameId: string, sourceProfileId: string, state: ProfileState, targetGameId: string, targetProfileId: string) {
    try {
      const game = db!.scopes.find(candidate => candidate.id === targetGameId)!;
      const profile = game.profiles.find(candidate => candidate.id === targetProfileId)!;
      const isSameProfile = sourceGameId === targetGameId && sourceProfileId === targetProfileId;
      const copy = {
        ...state,
        id: isSameProfile ? uid() : state.id,
        name: isSameProfile ? copyName(state.name, "State") : state.name,
      };
      const updated = await api.upsertProfile(targetGameId, { ...profile, states: [...profile.states, copy] });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error copying state: ${e}`); }
  }

  async function handleHotkeySave(gameId: string, profileId: string, index: number | null, hotkey: Hotkey) {
    try {
      const game = db!.scopes.find(g => g.id === gameId)!;
      const profile = game.profiles.find(p => p.id === profileId)!;
      const hotkeys = [...profile.hotkeys];
      if (index === null) hotkeys.push(hotkey);
      else hotkeys[index] = hotkey;
      const updated = await api.upsertProfile(gameId, { ...profile, hotkeys });
      handleDb(updated);
      setModal(null);
    } catch (e) { alert(`Error saving hotkey: ${e}`); }
  }

  if (!db) {
    return <div className="loading">Loading…</div>;
  }

  const selFolder = selection.kind === "profile" ? db.scopes.find(s => s.id === selection.folderId) ?? null : null;
  const selProfile = selection.kind === "profile" ? selFolder?.profiles.find(p => p.id === selection.profileId) ?? null : null;

  async function importFolder() {
    try {
      const scopes = await importScope();
      if (!scopes) return;
      let updated: Database | null = null;
      for (const scope of scopes) updated = await api.upsertGame(scope);
      if (updated) handleDb(updated);
    }
    catch (e) { alert(String(e)); }
  }

  return (
    <div className="layout">
      {updateInfo && (
        <div className="update-banner">
          <span>Update available: <strong>{updateInfo.version}</strong></span>
          {updateInfo.downloadUrl && (
            <button className="btn btn--primary btn--sm" disabled={updating} onClick={runUpdate}>
              {updating ? "Downloading…" : "Update Now"}
            </button>
          )}
          <button className="btn btn--ghost btn--sm" disabled={updating}
            onClick={() => openUrl(updateInfo.notesUrl).catch(() => {})}>Release Notes</button>
          <button className="btn btn--ghost btn--sm" disabled={updating} onClick={() => { dismissedUpdate.current = updateInfo.version; setUpdateInfo(null); }}>Dismiss</button>
        </div>
      )}
      <div className="layout__body">
        <Library db={db} openExes={openExes} selection={selection}
          query={query} collapsed={collapsed} width={libW}
          onQuery={setQuery} onToggleCollapse={toggleCollapse}
          onSelect={selectProfile} onToggleArmed={toggleArmed} onProfileContext={onProfileContext} onFolderContext={onFolderContext} onDb={handleDb} onModal={setModal} />
        <Resizer onPointerDown={onResize} />
        <div className="detail">
          {selection.kind === "settings" && (
            <div className="detail__scroll"><SettingsView db={db} onDb={handleDb} onCheckUpdates={() => checkUpdate(true)} /></div>
          )}
          {selProfile && selFolder && (
            <ProfileEditor key={selProfile.id} folder={selFolder} profile={selProfile}
              showStates={statesFor === selProfile.id} onExitStates={() => setStatesFor(null)}
              onContext={e => onProfileContext(e, selFolder.id, selProfile)}
              onDb={handleDb} onModal={setModal} />
          )}
          {selection.kind === "empty" && (
            <div className="detail__empty">
              {db.scopes.length === 0
                ? <EmptyState title="No folders yet"
                    hint="Each folder targets one app shared by its profiles, which hold the hotkeys, scripts, overlay, and states."
                    action={{ label: "New folder", onClick: () => setModal({ type: "addGame" }) }}
                    secondary={{ label: "Import folder", onClick: importFolder }} />
                : <EmptyState title="Select a profile"
                    hint="Pick a profile on the left to edit its hotkeys, scripts, overlay, and states." />}
            </div>
          )}
        </div>

      {/* Modals */}
      {modal?.type === "addGame" && (
        <FolderModal initial={blankGame()} onSave={handleGameSave} onClose={() => setModal(null)} />
      )}
      {modal?.type === "editGame" && (
        <FolderModal initial={modal.game} onSave={handleGameSave} onClose={() => setModal(null)} />
      )}
      {modal?.type === "profileSettings" && (() => {
        const { gameId, profile, isNew } = modal;
        return <ProfileSettingsModal initial={profile}
          onSave={p => handleProfileSettingsSave(gameId, p, isNew)} onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "editHotkey" && (() => {
        const { gameId, profileId: pid, index, hotkey, gameExe, states } = modal;
        return <HotkeyModal initial={hotkey} gameExe={gameExe} states={states}
          onSave={hk => handleHotkeySave(gameId, pid, index, hk)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "copyHotkey" && (() => {
        const { sourceGameId, sourceProfileId, hotkey } = modal;
        return <CopyToProfileModal games={db.scopes} title="Copy Behavior To Profile" sourceGameId={sourceGameId} sourceProfileId={sourceProfileId}
          onSave={(targetGameId, targetProfileId) => handleCopyHotkey(sourceGameId, sourceProfileId, hotkey, targetGameId, targetProfileId)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "setParent" && (() => {
        const { gameId, profile } = modal;
        const game = db.scopes.find(g => g.id === gameId)!;
        return <SetParentModal profiles={game.profiles} current={profile}
          onSave={parentId => handleSetParent(gameId, profile, parentId)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "copyProfile" && (() => {
        const { sourceGameId, profile } = modal;
        return <CopyProfileModal games={db.scopes} sourceProfile={profile}
          onSave={(targetGameId, name) => handleCopyProfile(sourceGameId, profile, targetGameId, name)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "overlayItem" && (() => {
        const { gameId, profileId, index, item, gameExe, states, groups } = modal;
        return <OverlayItemModal initial={item} gameExe={gameExe} states={states} groups={groups}
          onSave={it => handleOverlayItemSave(gameId, profileId, index, it)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "copyOverlayItem" && (() => {
        const { sourceGameId, sourceProfileId, item } = modal;
        return <CopyToProfileModal games={db.scopes} title="Copy Widget To Profile" sourceGameId={sourceGameId} sourceProfileId={sourceProfileId}
          onSave={(targetGameId, targetProfileId) => handleCopyOverlayItem(sourceGameId, sourceProfileId, item, targetGameId, targetProfileId)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "profileState" && (() => {
        const { gameId, profileId, index, state } = modal;
        return <StateModal initial={state}
          onSave={nextState => handleStateSave(gameId, profileId, index, nextState)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "copyState" && (() => {
        const { sourceGameId, sourceProfileId, state } = modal;
        return <CopyToProfileModal games={db.scopes} title="Copy State To Profile" sourceGameId={sourceGameId} sourceProfileId={sourceProfileId}
          onSave={(targetGameId, targetProfileId) => handleCopyState(sourceGameId, sourceProfileId, state, targetGameId, targetProfileId)}
          onClose={() => setModal(null)} />;
      })()}
      {modal?.type === "script" && (() => {
        const { gameId, profileId, index, script } = modal;
        return <ScriptModal initial={script}
          onSave={async next => {
            const game = db.scopes.find(g => g.id === gameId);
            const profile = game?.profiles.find(p => p.id === profileId);
            if (!profile) return;
            const scripts = index === null
              ? [...profile.scripts, next]
              : profile.scripts.map((s, i) => i === index ? next : s);
            try { handleDb(await api.upsertProfile(gameId, { ...profile, scripts })); }
            catch (e) { alert(String(e)); }
            setModal(null);
          }}
          onClose={() => setModal(null)} />;
      })()}
      </div>{/* layout__body */}
      {ctx && <ContextMenu x={ctx.x} y={ctx.y} items={ctx.items} onClose={() => setCtx(null)} />}
    </div>
  );
}
