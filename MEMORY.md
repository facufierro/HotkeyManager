# MacroToolbox — project memory

Tauri v2 app (React/TS frontend, Rust backend) that manages per-app hotkey/overlay/script profiles and runs them through one always-on AutoHotkey v2 process.

## Structure
- [src/App.tsx](src/App.tsx) — main UI: profiles, hotkey binding recorder (records the typed character for letters/digits, `e.code` position as fallback; AltGr's synthetic LCtrl is dropped so it records as plain ralt), settings. Key semantics are character-based end to end: triggers resolve via the active layout in `layout_scancode` (ahk.rs), sends via `GetKeySC` in `PhysKey`, both with US-position fallback for layouts missing the character. Update check: on mount, every 30 min, and on the backend's `main-window-shown` event (WebView2 doesn't fire visibilitychange on native show/hide).
- [src/OverlayApp.tsx](src/OverlayApp.tsx) — in-game overlay window UI.
- [src-tauri/src/ahk.rs](src-tauri/src/ahk.rs) — generates the combined AHK script from armed profiles (`#HotIf` per exe), launches/kills AutoHotkey64 in a kill-on-close Job Object; behavior engine (press/hold/repeat/goto/state/send) lives in `BEHAVIOR_ENGINE`.
- [src-tauri/src/config.rs](src-tauri/src/config.rs) — profile/config model, hotkey inheritance via `parent_id`.
- [src-tauri/src/scripts.rs](src-tauri/src/scripts.rs) — user script launching.
- [src-tauri/src/lib.rs](src-tauri/src/lib.rs) — Tauri setup, commands, localhost backend (port 17823) the AHK script calls for overlay sync/events.

## Features
- Per-app profiles gated by `WinActive("ahk_exe …")`; "Any app" profile (exe `*`) always exists and must never be deleted.
- Hotkey behaviors: press/hold (true remap)/repeat (hold-to-repeat)/goto/sleep/send/state; trigger modifiers are excluded only during behavior output so they cannot leak into remapped keys and remain held for subsequent hotkeys; toggle keys per profile.
- Layout-independent hotkeys: triggers and sent single-char keys are emitted as US-QWERTY scancodes (`us_scancode` in ahk.rs, `PhysKey` in the AHK engine) so non-English layouts work.
- The Copilot key → Left Ctrl remap and profile hotkeys share one persistent AHK process, so keyboard-hook recovery and synthesized modifier state have a single owner.
- Keyboard-hook health check + reinstall on wake so hotkeys survive sleep/tray idling.
- Overlay follows the focused app's armed overlay profile (200ms poll → backend show/hide/focus).
- Update check (launch + every 30min + on window becoming visible + manual "Check for Updates" button in Settings, GitHub latest release): in-app banner + Windows toast via tauri-plugin-notification (`checkUpdate` in App.tsx).
- Releases: `scripts/package-release.ps1` + `.github/workflows/release.yml`; both require `changelog/v<version>.md` (see AGENTS.md rules). Installer is NSIS perMachine (app runs elevated + autostarts).
