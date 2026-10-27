//! What the orchestrator's pane says about its working children.
//!
//! Every child leaves a small member record under `panes/<pane>/`; each start and end recomputes
//! the summary from the records that still describe a working child and reports it, so a crash
//! can leave a stale record behind but never a stale count.

use crate::{
    pane, process,
    process::Process,
    state::{self, Delegate, Reporting},
    supervise, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
};

/// A child that can make the summary say it is working.
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Member {
    /// Any delegate, whatever agent program it runs: `delegate.json` in its run directory says
    /// whether it is still going.
    Delegate { dir: PathBuf },
    /// Runs inside the orchestrator, so the orchestrator's process and the stop record are the only
    /// signs of life. One whose orchestrator was not identified cannot be known to be working.
    Subagent {
        dir: PathBuf,
        orchestrator: Option<Process>,
    },
}

impl Member {
    pub fn delegate(run_dir: &Path) -> Self {
        Self::Delegate {
            dir: run_dir.to_owned(),
        }
    }

    pub fn subagent(child_dir: &Path, orchestrator: Option<Process>) -> Self {
        Self::Subagent {
            dir: child_dir.to_owned(),
            orchestrator,
        }
    }

    fn file_name(&self) -> String {
        let name = |path: &Path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        // '.' is not in a session or agent id, so two ids cannot run together into another pair.
        match self {
            Self::Delegate { dir } => format!("d.{}.json", name(dir)),
            Self::Subagent { dir, .. } => format!(
                "s.{}.{}.json",
                dir.parent().map(name).unwrap_or_default(),
                name(dir)
            ),
        }
    }

    pub fn working(&self) -> bool {
        match self {
            Self::Delegate { dir } => state::read_json::<Delegate>(&dir.join("delegate.json"))
                .is_ok_and(|delegate| {
                    supervise::live(&delegate)
                        && delegate.orchestrator.as_ref().is_none_or(process::alive)
                }),
            Self::Subagent { dir, orchestrator } => {
                dir.is_dir()
                    && !dir.join("stop.json").exists()
                    && orchestrator.as_ref().is_some_and(process::alive)
            }
        }
    }
}

/// A child began: count it, and show the pane's summary.
pub fn started(pane: &str, source: &str, member: &Member) {
    refresh(pane, source, Some(member), None);
}

/// Waits out a subagent and corrects the summary if it ended without a stop record, which is
/// what the orchestrator's death looks like: nothing else would ever recompute.
pub fn settle(pane: &str, source: &str, member: &Member) {
    while member.working() {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if let Member::Subagent { dir, .. } = member {
        if dir.is_dir() && !dir.join("stop.json").exists() {
            changed(pane, source);
        }
    }
}

/// A child's stop was recorded, or something else about the pane's children may have changed.
pub fn changed(pane: &str, source: &str) {
    refresh(pane, source, None, None);
}

/// A delegate run ended, in its wrapper or in the watchdog after the wrapper was killed. The run
/// itself no longer counts even while the process reporting it is still alive.
pub fn ended(report: &Reporting) {
    refresh(&report.pane, &report.source, None, Some(report));
}

fn refresh(pane: &str, source: &str, join: Option<&Member>, ending: Option<&Reporting>) {
    if locked_refresh(pane, source, join, ending).is_err() {
        // Without the state there is no count to trust, but an ended delegate must not leave its
        // label behind.
        if let Some(report) = ending {
            pane::release(report);
            pane::show(pane, source, 0, None);
        }
    }
}

fn locked_refresh(
    pane: &str,
    source: &str,
    join: Option<&Member>,
    ending: Option<&Reporting>,
) -> Result<()> {
    let dir = state::root()?.join("panes").join(encode(pane));
    state::create_dir(&dir)?;
    // Two children starting or ending together would otherwise report in either order and leave
    // the older count on screen. The lock goes with the process that holds it.
    let _lock = lock(&dir.join("lock"))?;
    if let Some(member) = join {
        state::write_json(&dir.join(member.file_name()), member)?;
    }
    let skip = ending.map(|report| format!("d.{}.json", report.run_id));
    let (mut delegates, mut subagents) = (0, 0);
    for entry in fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| name.ends_with(".json")) else {
            continue;
        };
        let member = state::read_json::<Member>(&entry.path())
            .ok()
            .filter(|member| skip.as_deref() != Some(name) && member.working());
        match member {
            Some(Member::Delegate { .. }) => delegates += 1,
            Some(Member::Subagent { .. }) => subagents += 1,
            None => {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    if delegates == 0 {
        if let Some(report) = ending {
            pane::release(report);
        }
    }
    pane::show(
        pane,
        source,
        delegates,
        text(delegates, subagents).as_deref(),
    );
    Ok(())
}

fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

/// A pane id may hold characters a file name should not, so anything unusual is spelled out.
fn encode(pane: &str) -> String {
    pane.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' => (byte as char).to_string(),
            _ => format!("_{byte:02x}"),
        })
        .collect()
}

fn text(delegates: usize, subagents: usize) -> Option<String> {
    let mut parts = Vec::new();
    if delegates > 0 {
        parts.push(format!(
            "{delegates} delegate{}",
            if delegates == 1 { "" } else { "s" }
        ));
    }
    if subagents > 0 {
        parts.push(format!(
            "{subagents} sub{}",
            if subagents == 1 { "" } else { "s" }
        ));
    }
    (!parts.is_empty()).then(|| format!("{} working", parts.join(" · ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_counts_each_kind_apart() {
        assert_eq!(text(0, 0), None);
        assert_eq!(text(1, 0).unwrap(), "1 delegate working");
        assert_eq!(text(2, 0).unwrap(), "2 delegates working");
        assert_eq!(text(0, 1).unwrap(), "1 sub working");
        assert_eq!(text(0, 3).unwrap(), "3 subs working");
        assert_eq!(text(2, 1).unwrap(), "2 delegates · 1 sub working");
    }

    #[test]
    fn a_pane_id_becomes_one_plain_file_name() {
        assert_eq!(encode("w1D:p1"), "w1D_3ap1");
        assert_eq!(encode("../x"), "_2e_2e_2fx");
    }
}
