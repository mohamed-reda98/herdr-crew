use crate::{process::Process, Result};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// What a view follows. A delegate of another agent program adds a kind beside `Codex`
/// and `Claude`.
#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Subagent,
    Codex,
    Claude,
}

#[derive(Deserialize, Serialize)]
pub struct Child {
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default)]
    pub transcript: PathBuf,
    #[serde(default)]
    pub orchestrator: Option<Process>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub delegate: Option<DelegateRun>,
    /// The run id of the delegate this child was started inside, if any.
    #[serde(default)]
    pub parent: Option<String>,
}

/// What the view of a delegate needs to know about its run.
#[derive(Deserialize, Serialize)]
pub struct DelegateRun {
    pub out_dir: PathBuf,
    pub relay: Process,
    pub started_ms: u64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub title: Option<String>,
}

/// What a delegate run has told Herdr about the orchestrator's pane.
#[derive(Clone, Deserialize, Serialize)]
pub struct Reporting {
    pub run_id: String,
    pub pane: String,
    pub source: String,
    pub agent: String,
    pub lifecycle: bool,
}

/// A running delegate, for the helpers that outlive its wrapper.
#[derive(Deserialize, Serialize)]
pub struct Delegate {
    #[serde(flatten)]
    pub reporting: Reporting,
    pub wrapper: Process,
    pub wrapper_group: u32,
    pub relay: Process,
    pub orchestrator: Option<Process>,
    pub kill_grace_secs: u64,
}

#[derive(Deserialize, Serialize)]
pub struct Tab {
    pub tab_id: String,
    pub name: String,
}

#[derive(Deserialize, Serialize)]
pub struct Stop {
    pub stopped_at_ms: u64,
    pub transcript: Option<PathBuf>,
    #[serde(default)]
    pub message: Option<String>,
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
