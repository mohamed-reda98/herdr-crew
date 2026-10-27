use serde::{Deserialize, Serialize};
use std::fs;

/// A process identified by pid and start time, so a recycled pid is not mistaken for it.
#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct Process {
    pub pid: u32,
    pub started: u64,
}

// The hook may be wrapped in a shell that exits with it, which says nothing about the orchestrator.
const SHELLS: [&str; 5] = ["sh", "bash", "dash", "zsh", "fish"];

/// The orchestrator that ran this hook: the nearest ancestor that is not a shell.
pub fn orchestrator() -> Option<Process> {
    let mut pid = unsafe { libc::getppid() } as u32;
    for _ in 0..4 {
        if pid <= 1 {
            return None;
        }
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        if !SHELLS.contains(&comm.trim()) {
            return Some(Process {
                pid,
                started: stat(pid)?.started,
            });
        }
        pid = stat(pid)?.parent;
    }
    None
}

pub fn alive(process: &Process) -> bool {
    stat(process.pid).is_some_and(|stat| stat.started == process.started && stat.state != 'Z')
}

struct Stat {
    state: char,
    parent: u32,
    started: u64,
}

fn stat(pid: u32) -> Option<Stat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses, so fields start after the last ')'.
    let mut fields = raw[raw.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent = fields.next()?.parse().ok()?;
    let started = fields.nth(17)?.parse().ok()?;
    Some(Stat {
        state,
        parent,
        started,
    })
}
