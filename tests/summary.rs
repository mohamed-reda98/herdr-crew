use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

// A report takes a pid-dependent moment before it is logged, so reports made without the lock
// would land in the log out of the order they were computed in.
const FAKE_HERDR: &str = r#"#!/bin/sh
[ "$2" = report-metadata ] && sleep "0.0$(($$ % 7))"
printf '%s\n' "$*" >> "$HERDR_TEST_LOG"
case "$1 $2" in
    "agent list") echo '{"result":{"agents":[]}}' ;;
    "plugin pane") echo '{"result":{"plugin_pane":{"pane":{"tab_id":"tab-test"}}}}' ;;
esac
"#;

// Writes the brief where the wrapper looks for it and holds until the hold file goes.
const FAKE_RELAY: &str = r#"
while [ $# -gt 0 ]; do
    case "$1" in
        --out-dir) out="$2"; shift ;;
        --brief) brief="$2"; shift ;;
    esac
    shift
done
echo $$ > "$FAKE_PIDFILE"
mkdir -p "$out"
cat "$brief" > "$out/brief.txt"
: > "$out/events.jsonl"
while [ -e "$FAKE_HOLD" ]; do sleep 0.05; done
"#;

struct Fixture {
    root: PathBuf,
    cleanup: Vec<u32>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-crew-summary-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("herdr"), FAKE_HERDR).unwrap();
        fs::set_permissions(root.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("relay.sh"), FAKE_RELAY).unwrap();
        fs::write(root.join("brief.txt"), "Do it").unwrap();
        fs::write(root.join("hold"), "").unwrap();
        // A process that is not a shell by name, so the hook records it as the orchestrator.
        std::os::unix::fs::symlink("/bin/sh", root.join("claude")).unwrap();
        Self {
            root,
            cleanup: Vec::new(),
        }
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", "w1D:p1")
            .env("HERDR_CREW_STATE_DIR", self.root.join("state"))
            .env("HERDR_BIN", self.root.join("herdr"))
            .env("HERDR_TEST_LOG", self.root.join("herdr.log"))
            .env("CODEX_DELEGATE_RELAY", self.root.join("relay.sh"))
            .env("HERDR_CREW_NODE", "/bin/sh")
            .env("HERDR_CREW_ORCHESTRATOR_PID", "0")
            .env("HERDR_CREW_KILL_GRACE", "1")
            .env("HERDR_CREW_LINGER_MS", "200")
            .env("FAKE_HOLD", self.root.join("hold"))
            .env("FAKE_PIDFILE", self.root.join("relay.pid"))
            // A bug that signals a process group must not reach the test runner.
            .process_group(0);
        command
    }

    fn input(&self, agent: &str, event: &str) -> Value {
        json!({
            "session_id": "session-1", "transcript_path": self.root.join("conversation.jsonl"),
            "cwd": self.root, "hook_event_name": event, "agent_id": agent,
            "agent_type": "general-purpose"
        })
    }

    /// A stand-in for the orchestrator that runs a start hook for each agent, then stays alive.
    fn orchestrator(&self, agents: &[&str]) -> Orchestrator {
        let mut script = String::new();
        for agent in agents {
            let path = self.root.join(format!("start-{agent}.json"));
            fs::write(&path, self.input(agent, "SubagentStart").to_string()).unwrap();
            script += &format!(
                "'{}' hook subagent-start < '{}'\n",
                env!("CARGO_BIN_EXE_herdr-crew"),
                path.display()
            );
        }
        script += "exec sleep 60\n";
        let child = self
            .command(self.root.join("claude"))
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Orchestrator { child }
    }

    fn stop_command(&self, agent: &str) -> Child {
        let mut child = self
            .command(env!("CARGO_BIN_EXE_herdr-crew"))
            .args(["hook", "subagent-stop"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(self.input(agent, "SubagentStop").to_string().as_bytes())
            .unwrap();
        child
    }

    fn stop(&self, agent: &str) {
        assert!(self.stop_command(agent).wait().unwrap().success());
    }

    fn delegate(&self) -> Child {
        self.command(env!("CARGO_BIN_EXE_herdr-crew"))
            .args(["delegate", "--agent", "codex", "--brief"])
            .arg(self.root.join("brief.txt"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn release_delegate(&self) {
        let _ = fs::remove_file(self.root.join("hold"));
    }

    /// What each report on the pane said, in order: the summary, or `cleared`.
    fn reports(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("herdr.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("pane report-metadata w1D:p1 --source herdr-crew"))
            .map(|line| match line.split_once("--token delegate=") {
                Some((_, text)) => text.to_owned(),
                None => {
                    assert!(line.ends_with("--clear-display-agent --clear-token delegate"));
                    "cleared".to_owned()
                }
            })
            .collect()
    }

    fn last_report(&self) -> Option<String> {
        self.reports().pop()
    }

    fn expect(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while self.last_report().as_deref() != Some(text) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {text:?}; reports so far: {:#?}",
                self.reports()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether the pane's label was reported with the last summary.
    fn label_shown(&self) -> bool {
        fs::read_to_string(self.root.join("herdr.log"))
            .unwrap()
            .lines()
            .rfind(|line| line.starts_with("pane report-metadata"))
            .unwrap()
            .contains("--display-agent codex ⇠ claude")
    }

    fn relay(&mut self) -> u32 {
        let path = self.root.join("relay.pid");
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(pid) = fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                self.cleanup.push(pid);
                return pid;
            }
            assert!(Instant::now() < deadline, "relay never started");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.release_delegate();
        for pid in &self.cleanup {
            unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Orchestrator {
    child: Child,
}

impl Orchestrator {
    fn die(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Orchestrator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn count(report: &str) -> usize {
    report
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
        .unwrap_or(0)
}

#[test]
fn a_subagent_shows_while_it_works_and_clears_when_it_stops() {
    let fixture = Fixture::new();
    let _orchestrator = fixture.orchestrator(&["a"]);
    fixture.expect("1 sub working");
    assert!(!fixture.label_shown());
    fixture.stop("a");
    fixture.expect("cleared");
}

#[test]
fn delegates_and_subagents_are_counted_apart_and_drop_out_as_they_finish() {
    let mut fixture = Fixture::new();
    let _orchestrator = fixture.orchestrator(&["a", "b"]);
    fixture.expect("2 subs working");
    let mut delegate = fixture.delegate();
    fixture.expect("1 delegate · 2 subs working");
    assert!(fixture.label_shown());
    fixture.relay();
    fixture.stop("a");
    fixture.expect("1 delegate · 1 sub working");
    fixture.release_delegate();
    assert!(delegate.wait().unwrap().success());
    fixture.expect("1 sub working");
    assert!(!fixture.label_shown());
    fixture.stop("b");
    fixture.expect("cleared");
}

#[test]
fn a_delegate_alone_reads_as_a_delegate_working() {
    let mut fixture = Fixture::new();
    let mut delegate = fixture.delegate();
    fixture.expect("1 delegate working");
    fixture.relay();
    fixture.release_delegate();
    delegate.wait().unwrap();
    fixture.expect("cleared");
    assert_eq!(
        fixture.reports(),
        ["1 delegate working".to_owned(), "cleared".to_owned()]
    );
}

#[test]
fn a_killed_delegate_stops_counting_while_a_subagent_still_does() {
    let mut fixture = Fixture::new();
    let _orchestrator = fixture.orchestrator(&["a"]);
    fixture.expect("1 sub working");
    let mut delegate = fixture.delegate();
    fixture.expect("1 delegate · 1 sub working");
    let relay = fixture.relay();
    delegate.kill().unwrap();
    delegate.wait().unwrap();
    // The relay carries on unattended, so the delegate still counts.
    thread::sleep(Duration::from_millis(600));
    assert_eq!(
        fixture.last_report().as_deref(),
        Some("1 delegate · 1 sub working")
    );
    unsafe { libc::kill(relay as i32, libc::SIGKILL) };
    fixture.expect("1 sub working");
    assert!(!fixture.label_shown());
    assert!(!fixture.reports().contains(&"cleared".to_owned()));
}

#[test]
fn a_killed_delegate_clears_the_pane_when_nothing_else_works() {
    let mut fixture = Fixture::new();
    let mut delegate = fixture.delegate();
    fixture.expect("1 delegate working");
    let relay = fixture.relay();
    delegate.kill().unwrap();
    delegate.wait().unwrap();
    unsafe { libc::kill(relay as i32, libc::SIGKILL) };
    fixture.expect("cleared");
}

#[test]
fn a_subagent_whose_orchestrator_died_stops_counting() {
    let fixture = Fixture::new();
    let mut first = fixture.orchestrator(&["a"]);
    let mut second = fixture.orchestrator(&["b"]);
    fixture.expect("2 subs working");
    first.die();
    fixture.expect("1 sub working");
    second.die();
    fixture.expect("cleared");
}

#[test]
fn children_starting_and_finishing_together_leave_the_right_state() {
    let fixture = Fixture::new();
    let agents = ["a", "b", "c", "d", "e", "f"];
    let _orchestrator = fixture.orchestrator(&agents);
    fixture.expect("6 subs working");
    let counts: Vec<usize> = fixture.reports().iter().map(|r| count(r)).collect();
    assert!(
        counts.windows(2).all(|pair| pair[0] <= pair[1]),
        "{counts:?}"
    );

    let stops: Vec<Child> = agents.iter().map(|a| fixture.stop_command(a)).collect();
    for mut stop in stops {
        assert!(stop.wait().unwrap().success());
    }
    fixture.expect("cleared");
    // Reports are made one at a time, so none reports a count older than an earlier one.
    let counts: Vec<usize> = fixture.reports().iter().map(|r| count(r)).collect();
    let from = counts.iter().position(|n| *n == 6).unwrap();
    assert!(
        counts[from..].windows(2).all(|pair| pair[0] >= pair[1]),
        "{counts:?}"
    );
    thread::sleep(Duration::from_millis(300));
    assert_eq!(fixture.last_report().as_deref(), Some("cleared"));
}
