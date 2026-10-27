use crate::{state, Result};
use serde_json::{json, Value};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn run(settings: Option<PathBuf>) -> Result<()> {
    let settings = match settings {
        Some(path) => path,
        None => {
            PathBuf::from(env::var_os("HOME").ok_or("HOME is unset")?).join(".claude/settings.json")
        }
    };
    let settings = if settings.exists() {
        settings.canonicalize()?
    } else {
        settings
    };
    let original = match fs::read(&settings) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut document = match &original {
        Some(bytes) => serde_json::from_slice::<Value>(bytes)?,
        None => json!({}),
    };
    let executable = env::current_exe()?;
    let changed = merge(&mut document, &executable)?;
    if !changed {
        return Ok(());
    }
    if let Some(parent) = settings
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        state::create_dir(parent)?;
    }
    if let Some(bytes) = original {
        backup(&settings, &bytes)?;
    }
    replace(&settings, &document)?;
    println!("Installed subagent hooks in {}", settings.display());
    Ok(())
}

fn merge(document: &mut Value, executable: &Path) -> Result<bool> {
    let root = document
        .as_object_mut()
        .ok_or("settings must be a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks must be a JSON object")?;
    let executable = executable.to_str().ok_or("binary path is not UTF-8")?;
    let quoted = format!("'{}'", executable.replace('\'', "'\\''"));
    let mut changed = false;
    for (event, action) in [
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
    ] {
        let command = format!("{quoted} hook {action}");
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("hook entries must be arrays")?;
        if !contains_command(groups, &command) {
            groups.push(json!({"matcher": "", "hooks": [{"type": "command", "command": command, "timeout": 2}]}));
            changed = true;
        }
    }
    Ok(changed)
}

fn contains_command(groups: &[Value], command: &str) -> bool {
    groups.iter().any(|group| {
        group["matcher"].as_str().unwrap_or("").is_empty()
            && group["hooks"].as_array().is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|hook| hook["type"] == "command" && hook["command"] == command)
            })
    })
}

fn backup(settings: &Path, bytes: &[u8]) -> Result<()> {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut backup = settings.as_os_str().to_owned();
    backup.push(format!(".bak.{suffix}.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(backup)?;
    file.write_all(bytes)?;
    Ok(())
}

fn replace(settings: &Path, document: &Value) -> Result<()> {
    let mut pending = settings.as_os_str().to_owned();
    pending.push(format!(".{}.tmp", std::process::id()));
    let pending = PathBuf::from(pending);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let written = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut file, document)?;
        file.write_all(b"\n")?;
        if settings.exists() {
            file.set_permissions(fs::metadata(settings)?.permissions())?;
        }
        fs::rename(&pending, settings)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(pending);
    }
    written
}
