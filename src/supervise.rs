use crate::{
    process,
    state::{self, Delegate},
    summary, Result,
};
use std::{
    collections::BTreeSet,
    io::{self, Error},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(200);
// What the relay's own children get to leave after it has been told to stop.
const SETTLE: Duration = Duration::from_secs(1);

pub fn runs_dir() -> Result<PathBuf> {
    Ok(state::root()?.join("delegates"))
}

/// Starts a helper that outlives the wrapper: a new session, and no stream shared with the
/// caller, who may be waiting for them to close.
pub fn spawn(role: &str, run_dir: &Path) -> io::Result<Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(role)
        .arg(run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

pub fn live(delegate: &Delegate) -> bool {
    process::alive(&delegate.wrapper) || process::alive(&delegate.relay)
}

/// Updates the pane once the wrapper and the relay are both gone, however the wrapper ended.
pub fn watchdog(run_dir: &Path) -> Result<()> {
    let delegate: Delegate = state::read_json(&run_dir.join("delegate.json"))?;
    while live(&delegate) {
        thread::sleep(POLL);
    }
    summary::ended(&delegate.reporting);
    Ok(())
}

/// Stops the run when the orchestrator that started it goes away: the relay would otherwise
/// leave Codex working unattended, since Codex runs in a process group of its own.
pub fn guard(run_dir: &Path) -> Result<()> {
    let delegate: Delegate = state::read_json(&run_dir.join("delegate.json"))?;
    let Some(orchestrator) = delegate.orchestrator else {
        return Ok(());
    };
    while process::alive(&delegate.relay) {
        if !process::alive(&orchestrator) {
            stop(&delegate);
            break;
        }
        thread::sleep(POLL);
    }
    Ok(())
}

fn stop(delegate: &Delegate) {
    // Looked up first: once the relay is told to stop its children are reparented.
    let members = process::tree(delegate.relay.pid);
    process::signal(&delegate.relay, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(delegate.kill_grace_secs);
    while process::alive(&delegate.relay) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(SETTLE);
    // The relay shares its group with the wrapper and so with the caller's shell, which must
    // survive; the groups the run made for itself are fair game.
    let own = unsafe { libc::getpgrp() } as u32;
    let groups: BTreeSet<u32> = members
        .iter()
        .map(|(_, group)| *group)
        .filter(|group| *group > 1 && *group != delegate.wrapper_group && *group != own)
        .collect();
    for group in groups {
        process::signal_group(group, libc::SIGKILL);
    }
    for (member, _) in &members {
        process::signal(member, libc::SIGKILL);
    }
}
