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

/// The running process with this pid, if any.
pub fn of(pid: u32) -> Option<Process> {
    Some(Process {
        pid,
        started: stat(pid)?.started,
    })
}

/// The nearest ancestor of this process whose command name is `name`.
pub fn ancestor_named(name: &str) -> Option<Process> {
    let mut pid = stat(std::process::id())?.parent;
    for _ in 0..64 {
        if pid <= 1 {
            return None;
        }
        let info = stat(pid)?;
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        if comm.trim() == name {
            return Some(Process {
                pid,
                started: info.started,
            });
        }
        pid = info.parent;
    }
    None
}

/// A process and everything below it, with the process group each belongs to.
pub fn tree(root: u32) -> Vec<(Process, u32)> {
    let mut children: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if let Some(info) = stat(pid) {
            children.entry(info.parent).or_default().push(pid);
        }
    }
    let mut found = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if let Some(info) = stat(pid) {
            found.push((
                Process {
                    pid,
                    started: info.started,
                },
                info.group,
            ));
            queue.extend(children.get(&pid).into_iter().flatten());
        }
    }
    found
}

/// Signals the process only while it is still the one that was recorded.
pub fn signal(process: &Process, signal: libc::c_int) {
    if alive(process) {
        unsafe { libc::kill(process.pid as libc::pid_t, signal) };
    }
}

pub fn signal_group(group: u32, signal: libc::c_int) {
    unsafe { libc::kill(-(group as libc::pid_t), signal) };
}

struct Stat {
    state: char,
    parent: u32,
    group: u32,
    started: u64,
}

fn stat(pid: u32) -> Option<Stat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and parentheses, so fields start after the last ')'.
    let mut fields = raw[raw.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    let started = fields.nth(16)?.parse().ok()?;
    Some(Stat {
        state,
        parent,
        group,
        started,
    })
}
