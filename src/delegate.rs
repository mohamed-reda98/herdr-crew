use crate::{
    codex, hooks, naming, pane, process,
    state::{self, Child, Delegate, DelegateRun, Kind, Reporting},
    summary, supervise, Result,
};
use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicI32, Ordering},
    thread,
    time::Duration,
};

const USAGE: &str =
    "usage: herdr-crew delegate --agent codex [--title <short task>] <relay args...>

Runs the codex-delegate relay with the given arguments and opens a view for the run.
Everything but --agent and --title goes to the relay unchanged.";

// Options the relay reads a value for; the scan must not mistake a value for an option.
const RELAY_VALUE_OPTIONS: [&str; 10] = [
    "--brief",
    "--cd",
    "--lane",
    "--model",
    "--effort",
    "--service-tier",
    "--sandbox",
    "--session",
    "--keep-env",
    "--timeout",
];

struct Request {
    kind: Kind,
    title: Option<String>,
    relay_args: Vec<String>,
}

enum Parsed {
    Help,
    Run(Request),
}

/// The agent program a delegate runs as. A second kind adds its own relay and step source here.
fn agent_kind(name: &str) -> Result<Kind> {
    match name {
        "codex" => Ok(Kind::Codex),
        "claude" => {
            Err("--agent claude is not supported: only --agent codex starts a delegate".into())
        }
        other => Err(format!("unknown agent {other:?}: expected --agent codex").into()),
    }
}

fn parse(args: &[String]) -> Result<Parsed> {
    let mut agent = None;
    let mut title = None;
    let mut relay_args = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--agent" => agent = Some(rest.next().ok_or("--agent requires a value")?.as_str()),
            "--title" => title = Some(rest.next().ok_or("--title requires a value")?.clone()),
            _ => relay_args.push(arg.clone()),
        }
    }
    let Some(agent) = agent else {
        if relay_args.is_empty()
            || matches!(relay_args.as_slice(), [help] if help == "--help" || help == "-h")
        {
            return Ok(Parsed::Help);
        }
        return Err("missing --agent: expected --agent codex".into());
    };
    Ok(Parsed::Run(Request {
        kind: agent_kind(agent)?,
        title: title.filter(|title| !title.trim().is_empty()),
        relay_args,
    }))
}

/// How to start one kind of delegate's relay.
struct Relay {
    interpreter: OsString,
    script: PathBuf,
}

impl Relay {
    fn of(kind: Kind) -> Result<Self> {
        match kind {
            Kind::Codex => {
                let script =
                    match env::var_os("CODEX_DELEGATE_RELAY").filter(|path| !path.is_empty()) {
                        Some(path) => PathBuf::from(path),
                        None => PathBuf::from(env::var_os("HOME").ok_or("HOME is unset")?)
                            .join(".claude/skills/codex-delegate/scripts/relay.mjs"),
                    };
                Ok(Self {
                    interpreter: env::var_os("HERDR_CREW_NODE")
                        .filter(|path| !path.is_empty())
                        .unwrap_or_else(|| "node".into()),
                    script,
                })
            }
            Kind::Subagent => Err("a subagent has no relay".into()),
        }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut command = Command::new(&self.interpreter);
        command.arg(&self.script).args(args);
        command
    }
}

pub fn run(args: &[String]) -> Result<i32> {
    let request = match parse(args)? {
        Parsed::Help => {
            println!("{USAGE}");
            return Ok(0);
        }
        Parsed::Run(request) => request,
    };
    let relay = Relay::of(request.kind)?;
    if !relay.script.is_file() {
        return Err(format!("relay not found: {}", relay.script.display()).into());
    }
    if !crate::in_herdr() {
        // Replaces this process, so the caller sees the relay's own output, signals and exit code.
        let error = relay.command(&request.relay_args).exec();
        eprintln!("herdr-crew: could not run {:?}: {error}", relay.interpreter);
        return Ok(127);
    }
    supervised(&request, &relay)
}

struct Scan {
    // Some(None) is `--out-dir` without a value, which the relay must reject itself.
    out_dir: Option<Option<PathBuf>>,
    model: Option<String>,
    effort: Option<String>,
}

fn scan(args: &[String]) -> Scan {
    let mut found = Scan {
        out_dir: None,
        model: None,
        effort: None,
    };
    let mut at = 0;
    while at < args.len() {
        let value = args.get(at + 1);
        match args[at].as_str() {
            "--out-dir" => found.out_dir = Some(value.map(|dir| absolute(dir))),
            "--model" => found.model = value.cloned(),
            "--effort" => found.effort = value.cloned(),
            option if RELAY_VALUE_OPTIONS.contains(&option) => (),
            _ => {
                at += 1;
                continue;
            }
        }
        at += 2;
    }
    found
}

fn absolute(path: &str) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path))
}

static RELAY_PID: AtomicI32 = AtomicI32::new(0);
static PENDING: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(signal: libc::c_int) {
    PENDING.store(signal, Ordering::SeqCst);
    let pid = RELAY_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, signal) };
    }
}

// The relay turns a termination into an aborted result and stops Codex, so it must get it.
fn forward_terminations() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = forward as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

fn forward_to(pid: u32) {
    RELAY_PID.store(pid as i32, Ordering::SeqCst);
    // A signal that arrived before the relay existed is not lost.
    let pending = PENDING.load(Ordering::SeqCst);
    if pending != 0 {
        unsafe { libc::kill(pid as i32, pending) };
    }
}

/// The session whose end stops the run: the Claude Code process above this one. Zero in
/// `HERDR_CREW_ORCHESTRATOR_PID` turns that off.
fn orchestrator() -> Option<process::Process> {
    match env::var("HERDR_CREW_ORCHESTRATOR_PID") {
        Ok(pid) => pid
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .and_then(process::of),
        Err(_) => process::ancestor_named("claude"),
    }
}

fn supervised(request: &Request, relay: &Relay) -> Result<i32> {
    let run_id = format!("{}-{}", state::epoch_ms(), std::process::id());
    let run_dir = supervise::runs_dir()?.join(&run_id);
    state::create_dir(&run_dir)?;
    let scanned = scan(&request.relay_args);

    // The relay writes its files into --out-dir; a known one lets the view follow them live. It
    // goes first so a trailing option without a value is still rejected the way it would be alone.
    let mut args = request.relay_args.clone();
    let out_dir = match scanned.out_dir {
        Some(given) => given,
        None => {
            let dir = std::path::absolute(run_dir.join("out"))?;
            args.splice(
                0..0,
                ["--out-dir".to_owned(), dir.to_string_lossy().into_owned()],
            );
            Some(dir)
        }
    };

    let pane_id = env::var("HERDR_PANE_ID")?;
    let (agent, lifecycle) = match pane::detected_agent(&pane_id) {
        Some(agent) => (agent, false),
        None => ("codex".to_owned(), true),
    };
    let reporting = Reporting {
        run_id,
        pane: pane_id,
        source: pane::source(),
        agent,
        lifecycle,
    };
    pane::announce(&reporting);

    forward_terminations();
    let started_ms = state::epoch_ms();
    let mut child = match relay.command(&args).spawn() {
        Ok(child) => child,
        Err(error) => {
            summary::ended(&reporting);
            eprintln!("herdr-crew: could not run {:?}: {error}", relay.interpreter);
            return Ok(127);
        }
    };
    forward_to(child.id());

    let relay_process = process::of(child.id());
    let mut watchdog = None;
    if let Some(relay_process) = relay_process {
        let delegate = Delegate {
            reporting: reporting.clone(),
            wrapper: process::of(std::process::id()).ok_or("cannot identify this process")?,
            wrapper_group: unsafe { libc::getpgrp() } as u32,
            relay: relay_process,
            orchestrator: orchestrator(),
            kill_grace_secs: env::var("HERDR_CREW_KILL_GRACE")
                .ok()
                .and_then(|secs| secs.parse().ok())
                .unwrap_or(15),
        };
        if state::write_json(&run_dir.join("delegate.json"), &delegate).is_ok() {
            watchdog = supervise::spawn("delegate-watchdog", &run_dir).ok();
            if delegate.orchestrator.is_some() {
                let _ = supervise::spawn("delegate-guard", &run_dir);
            }
            summary::started(
                &reporting.pane,
                &reporting.source,
                &summary::Member::delegate(&run_dir),
            );
        }
    }

    // A view is worth opening once the relay has accepted its arguments, which it shows by
    // writing the brief; one it rejected leaves nothing to watch.
    let configured = codex::configured();
    let mut view_target = out_dir.zip(relay_process);
    let status = loop {
        // Looked at before the brief: a relay seen gone has already written it.
        let exited = child.try_wait()?;
        if let Some((dir, relay_process)) = &view_target {
            let brief = dir.join("brief.txt");
            // The relay creates the file before it fills it, and never accepts an empty brief.
            let written = fs::metadata(&brief).is_ok_and(|meta| meta.len() > 0);
            if written && codex::written_since(&brief, started_ms) {
                let run = DelegateRun {
                    out_dir: dir.clone(),
                    relay: *relay_process,
                    started_ms,
                    model: scanned.model.clone().or(configured.0.clone()),
                    effort: scanned.effort.clone().or(configured.1.clone()),
                    title: request.title.clone(),
                };
                let _ = open_view(&run_dir, &reporting.run_id, request.kind, run);
                view_target = None;
            }
        }
        match exited {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    // The watchdog is only for a wrapper that never got here.
    if let Some(mut watchdog) = watchdog {
        let _ = watchdog.kill();
        let _ = watchdog.wait();
    }
    summary::ended(&reporting);
    Ok(status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1))
}

fn open_view(run_dir: &Path, run_id: &str, kind: Kind, run: DelegateRun) -> Result<()> {
    let brief = fs::read(run.out_dir.join("brief.txt"))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let name = naming::delegate_name(
        run.model.as_deref(),
        run.effort.as_deref(),
        run.title.as_deref(),
        brief.as_deref(),
    );
    state::write_json(
        &run_dir.join("child.json"),
        &Child {
            agent_id: run_id.to_owned(),
            agent_type: "codex".to_owned(),
            transcript: PathBuf::new(),
            orchestrator: None,
            kind,
            delegate: Some(run),
        },
    )?;
    hooks::open_tab(run_dir, &name)
}
