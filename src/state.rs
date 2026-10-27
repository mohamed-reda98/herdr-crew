use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Deserialize, Serialize)]
pub struct Child {
    pub agent_id: String,
    pub agent_type: String,
    pub transcript: PathBuf,
}

#[derive(Deserialize, Serialize)]
pub struct Stop {
    pub stopped_at_ms: u64,
    pub transcript: Option<PathBuf>,
}

pub fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn root() -> Result<PathBuf> {
    if let Some(path) = env::var_os("HERDR_CREW_STATE_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let base = match env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is unset")?).join(".local/state"),
    };
    Ok(base.join("herdr-crew"))
}

pub fn create_dir(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

pub fn write_json(path: &Path, record: &impl Serialize) -> Result<()> {
    let pending = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let written = (|| -> Result<()> {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        fs::rename(&pending, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(pending);
    }
    written
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
