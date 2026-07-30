use std::path::Path;
use std::process::Command;

use crate::config::Script;

/// Interpreters to try, in order, when running a script. A configured path (from Settings)
/// wins; otherwise fall back to `python` and then the Windows `py` launcher.
fn python_candidates(configured: &str) -> Vec<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return vec![configured.to_string()];
    }
    vec!["python".to_string(), "py".to_string()]
}

/// Run a Python or AutoHotkey script from a file (source = "path") or from inline code written
/// under `scripts_path` (source = "code"). Returns the spawned child so the caller can track it
/// and terminate it when the app quits or the owning profile is disarmed.
pub fn run_script(
    python_exe: &str,
    ahk_exe: &str,
    script: &Script,
    scripts_path: &Path,
) -> Result<std::process::Child, String> {
    let (extension, interpreters, interpreter_name, setting_name) = match script.language.as_str() {
        "python" => (
            "py",
            python_candidates(python_exe),
            "Python",
            "Python executable path",
        ),
        "autohotkey" => (
            "ahk",
            vec![ahk_exe.to_string()],
            "AutoHotkey",
            "AutoHotkey executable path",
        ),
        language => return Err(format!("Unsupported script language '{language}'.")),
    };

    let target = if script.source == "path" {
        let path = script.path.trim();
        if path.is_empty() {
            return Err("Script has no file path set.".to_string());
        }
        std::path::PathBuf::from(path)
    } else {
        let file = scripts_path.join(format!("{extension}-{}.{extension}", script.id));
        std::fs::write(&file, &script.code)
            .map_err(|e| format!("Could not write the script file: {e}"))?;
        file
    };

    let mut last_err = String::new();
    for interpreter in interpreters {
        let mut command = Command::new(&interpreter);
        command.arg(&target);
        // Run from the script's own folder so relative paths in the script resolve next to it
        // (a path script → its file's directory; inline code → the scripts data folder).
        if let Some(dir) = target.parent() {
            command.current_dir(dir);
        }
        // Don't pop a console window each time a hotkey/launch fires the script.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(e) => last_err = format!("Failed to run '{interpreter}': {e}"),
        }
    }
    Err(format!(
        "Could not start {interpreter_name}. {last_err}. Set the {setting_name} in Settings."
    ))
}
