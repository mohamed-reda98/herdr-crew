use serde_json::Value;
use std::{
    fs::{self, File},
    io::Write,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

const FAKE_HERDR: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HERDR_TEST_LOG"
case "$1 $2" in
    "agent list") cat "$HERDR_TEST_AGENTS" 2>/dev/null || echo '{"result":{"agents":[]}}' ;;
    "plugin pane") echo '{"result":{"plugin_pane":{"pane":{"tab_id":"tab-test"}}}}' ;;
esac
"#;

// Stands in for the codex-delegate relay: echoes its arguments, takes the brief from --brief or
// stdin, streams events into --out-dir and writes result.json, as the real one does.
const FAKE_RELAY: &str = r#"
printf 'ARGS:'
for arg in "$@"; do printf '[%s]' "$arg"; done
printf '\n'
echo "relay-stderr" >&2
out=""
brief=""
while [ $# -gt 0 ]; do
    case "$1" in
        --out-dir)
            [ $# -ge 2 ] || { echo "relay: --out-dir requires a value" >&2; exit 2; }
            out="$2"; shift ;;
        --brief) brief="$2"; shift ;;
    esac
    shift
done
[ -n "$FAKE_PIDFILE" ] && echo $$ > "$FAKE_PIDFILE"
write_result() {
    printf '{"status":"%s","exitCode":%s,"model":%s,"effort":%s,"finalMessage":"%s","error":%s}\n' \
        "$1" "${FAKE_EXIT:-0}" "${FAKE_MODEL_JSON:-null}" "${FAKE_EFFORT_JSON:-null}" \
        "${FAKE_FINAL:-All done}" "${FAKE_ERROR_JSON:-null}" > "$out/result.json.tmp"
    mv "$out/result.json.tmp" "$out/result.json"
}
mkdir -p "$out"
if [ -n "$brief" ]; then cat "$brief" > "$out/brief.txt"; else cat > "$out/brief.txt"; fi
: > "$out/events.jsonl"
[ -n "$FAKE_EVENTS_1" ] && cat "$FAKE_EVENTS_1" >> "$out/events.jsonl"
if [ -n "$FAKE_ABORT_ON_TERM" ]; then trap 'write_result aborted; exit 143' TERM; fi
if [ -n "$FAKE_IGNORE_TERM" ]; then trap '' TERM; fi
if [ -n "$FAKE_GRANDCHILD" ]; then setsid sleep 60 & echo $! > "$FAKE_GRANDCHILD"; fi
while [ -e "$FAKE_HOLD" ]; do sleep 0.05; done
[ -n "$FAKE_EVENTS_2" ] && cat "$FAKE_EVENTS_2" >> "$out/events.jsonl"
if [ -z "$FAKE_NO_RESULT" ]; then write_result "${FAKE_STATUS:-completed}"; fi
exit "${FAKE_EXIT:-0}"
"#;

struct Fixture {
    root: PathBuf,
    cleanup: Vec<u32>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-crew-delegate-{}-{}-{}",
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
        Self {
            root,
            cleanup: Vec::new(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-crew"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", "w1D:p1")
            .env("HERDR_CREW_STATE_DIR", self.path("state"))
            .env("HERDR_BIN", self.path("herdr"))
            .env("HERDR_TEST_LOG", self.path("herdr.log"))
            .env("HERDR_TEST_AGENTS", self.path("agents.json"))
            .env("CODEX_DELEGATE_RELAY", self.path("relay.sh"))
            .env("CLAUDE_DELEGATE_RELAY", self.path("relay.sh"))
            .env("HERDR_CREW_NODE", "/bin/sh")
            .env("HERDR_CREW_ORCHESTRATOR_PID", "0")
            .env("HERDR_CREW_KILL_GRACE", "1")
            .env("HERDR_CREW_LINGER_MS", "200")
            // A bug that signals a process group must not reach the test runner.
            .process_group(0);
        command
    }

    fn brief(&self, text: &str) -> String {
        let path = self.path("brief-input.txt");
        fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn spawn(&self, args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>) -> Delegate {
        let stdout = self.path(&format!(
            "out-{}.txt",
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let stderr = stdout.with_extension("err");
        let mut command = self.command();
        command
            .arg("delegate")
            .args(args)
            .envs(envs.iter().copied())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let mut child = command.spawn().unwrap();
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(text.as_bytes()).unwrap();
        }
        Delegate {
            child,
            stdout,
            stderr,
        }
    }

    fn run(&self, args: &[&str], envs: &[(&str, &str)]) -> Finished {
        self.spawn(args, envs, None).finish()
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.path("herdr.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn calls_matching(&self, text: &str) -> usize {
        self.log().iter().filter(|line| line.contains(text)).count()
    }

    fn run_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = fs::read_dir(self.path("state/delegates"))
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        dirs.sort();
        dirs
    }

    fn run_dir(&self) -> PathBuf {
        wait_until(|| {
            self.run_dirs()
                .iter()
                .any(|dir| dir.join("tab.json").exists())
        });
        self.run_dirs().remove(0)
    }

    fn view(&self, run_dir: &Path) -> View {
        let output = self.path(&format!(
            "view-{}.txt",
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let child = self
            .command()
            .arg("view")
            .env("HERDR_CREW_CHILD_DIR", run_dir)
            .stdin(Stdio::null())
            .stdout(File::create(&output).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        View { child, output }
    }

    fn hold(&self, name: &str) -> String {
        let path = self.path(name);
        fs::write(&path, "").unwrap();
        path.to_string_lossy().into_owned()
    }

    fn events(&self, name: &str, lines: &[&str]) -> String {
        let path = self.path(name);
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path.to_string_lossy().into_owned()
    }

    fn track(&mut self, pid: u32) {
        self.cleanup.push(pid);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in &self.cleanup {
            unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
        }
        // Whatever a failed test left holding a relay open lets it finish.
        for name in ["hold", "hold-a", "hold-b"] {
            let _ = fs::remove_file(self.root.join(name));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Delegate {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Delegate {
    fn finish(mut self) -> Finished {
        wait_until(|| self.child.try_wait().unwrap().is_some());
        Finished {
            code: self.child.wait().unwrap().code(),
            stdout: fs::read_to_string(&self.stdout).unwrap(),
            stderr: fs::read_to_string(&self.stderr).unwrap(),
        }
    }
}

impl Drop for Delegate {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct View {
    child: Child,
    output: PathBuf,
}

impl View {
    fn text(&self) -> String {
        fs::read_to_string(&self.output).unwrap()
    }

    fn expect(&self, text: &str) {
        wait_until(|| self.text().contains(text));
    }

    fn expect_exit(&mut self) {
        wait_until(|| self.child.try_wait().unwrap().is_some());
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting");
        thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat[stat.rfind(')').unwrap() + 1..]
            .trim_start()
            .starts_with('Z')
    })
}

fn read_pid(path: &Path) -> u32 {
    wait_until(|| fs::read_to_string(path).is_ok_and(|text| text.trim().parse::<u32>().is_ok()));
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn index_of(log: &[String], text: &str) -> usize {
    log.iter()
        .position(|line| line.contains(text))
        .unwrap_or_else(|| panic!("no call containing {text:?} in {log:#?}"))
}

const SET: &str = "--display-agent";
const CLEARED: &str = "--clear-display-agent";

const STARTED: &str = r#"{"type":"thread.started","thread_id":"t-1"}"#;
const COMMAND_STARTED: &str = r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"/bin/zsh -lc 'cargo test'","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#;
const COMMAND_FAILED: &str = r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"/bin/zsh -lc 'cargo test'","aggregated_output":"boom\nline2","exit_code":101,"status":"failed"}}"#;
const THOUGHT: &str = r#"{"type":"item.completed","item":{"id":"i2","type":"reasoning","text":"Weighing the options"}}"#;
const EDIT: &str = r#"{"type":"item.completed","item":{"id":"i3","type":"file_change","changes":[{"path":"/repo/src/lib.rs","kind":"update"},{"path":"/repo/new.rs","kind":"add"}]}}"#;
const MESSAGE: &str =
    r#"{"type":"item.completed","item":{"id":"i4","type":"agent_message","text":"Halfway there"}}"#;
const TURN_DONE: &str =
    r#"{"type":"turn.completed","usage":{"input_tokens":120,"output_tokens":45}}"#;

#[test]
fn the_relay_gets_what_the_caller_gave_minus_agent_and_title_and_the_exit_code_comes_back() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    let out = fixture.path("mine");
    let out = out.to_str().unwrap();
    let args = [
        "--agent",
        "codex",
        "--title",
        "short task",
        "--brief",
        &brief,
        "--read-only",
        "--out-dir",
        out,
        "--cd",
        "/repo",
    ];
    let finished = fixture.run(&args, &[("FAKE_EXIT", "3")]);
    assert_eq!(finished.code, Some(3));
    assert_eq!(
        finished.stdout,
        format!("ARGS:[--brief][{brief}][--read-only][--out-dir][{out}][--cd][/repo]\n")
    );
    // The relay's own stderr is untouched and nothing of Herdr's reaches either stream.
    assert_eq!(finished.stderr, "relay-stderr\n");
    assert!(out.starts_with(fixture.root.to_str().unwrap()));
    assert!(fixture.path("mine/events.jsonl").exists());
    assert!(fixture
        .run_dirs()
        .iter()
        .all(|dir| !dir.join("out").exists()));
}

#[test]
fn without_an_out_dir_a_known_one_is_forced_and_a_valueless_one_is_left_for_the_relay_to_reject() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    let finished = fixture.run(&["--agent", "codex", "--brief", &brief], &[]);
    assert_eq!(finished.code, Some(0));
    let run_dir = fixture.run_dirs().remove(0);
    assert_eq!(
        finished.stdout,
        format!(
            "ARGS:[--out-dir][{}][--brief][{brief}]\n",
            run_dir.join("out").display()
        )
    );
    assert_eq!(
        fs::read_to_string(run_dir.join("out/brief.txt")).unwrap(),
        "Do the thing"
    );

    let fixture = Fixture::new();
    let finished = fixture.run(&["--agent", "codex", "--brief", &brief, "--out-dir"], &[]);
    assert_eq!(finished.code, Some(2));
    assert_eq!(
        finished.stdout,
        format!("ARGS:[--brief][{brief}][--out-dir]\n")
    );
    assert!(finished.stderr.contains("--out-dir requires a value"));
    // A relay that rejected its arguments leaves nothing to watch.
    assert_eq!(fixture.calls_matching("plugin pane open"), 0);
    assert_eq!(fixture.calls_matching(CLEARED), 1);
}

#[test]
fn a_brief_on_stdin_reaches_the_relay_and_names_the_tab() {
    let fixture = Fixture::new();
    let finished = fixture
        .spawn(
            &["--agent", "codex"],
            &[],
            Some("Port the wrapper into Rust\nmore"),
        )
        .finish();
    assert_eq!(finished.code, Some(0));
    let run_dir = fixture.run_dirs().remove(0);
    assert_eq!(
        fs::read_to_string(run_dir.join("out/brief.txt")).unwrap(),
        "Port the wrapper into Rust\nmore"
    );
    assert!(fixture
        .log()
        .contains(&"tab rename tab-test codex: Port the wrapper".to_owned()));
}

#[test]
fn outside_herdr_the_relay_runs_unchanged_with_no_view_and_no_herdr_calls() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    let out = fixture.path("mine");
    let args = [
        "--brief",
        &brief,
        "--out-dir",
        out.to_str().unwrap(),
        "--model",
        "gpt-x",
    ];
    let mut with_agent = vec!["--agent", "codex", "--title", "ignored"];
    with_agent.extend(args);
    let direct = Command::new("/bin/sh")
        .arg(fixture.path("relay.sh"))
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    for envs in [vec![("HERDR_ENV", "0")], vec![("HERDR_PANE_ID", "")]] {
        let finished = fixture.run(&with_agent, &envs);
        assert_eq!(finished.code, direct.status.code());
        assert_eq!(finished.stdout.as_bytes(), direct.stdout);
        assert_eq!(finished.stderr.as_bytes(), direct.stderr);
    }
    assert!(!fixture.path("herdr.log").exists());
    assert!(!fixture.path("state").exists());

    let finished = fixture.run(&with_agent, &[("HERDR_ENV", "0"), ("FAKE_EXIT", "7")]);
    assert_eq!(finished.code, Some(7));
}

#[test]
fn only_codex_and_claude_are_delegates_and_a_missing_agent_is_explained() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    for envs in [vec![], vec![("HERDR_ENV", "0")]] {
        let finished = fixture.run(&["--agent", "gemini"], &envs);
        assert_eq!(finished.code, Some(2));
        assert!(finished.stderr.contains("unknown agent"));
        let finished = fixture.run(&["--brief", &brief], &envs);
        assert_eq!(finished.code, Some(2));
        assert!(finished.stderr.contains("missing --agent"));
        let finished = fixture.run(&["--agent"], &envs);
        assert_eq!(finished.code, Some(2));
        assert!(finished.stderr.contains("--agent requires a value"));
    }
    assert!(!fixture.path("herdr.log").exists());
}

#[test]
fn the_view_opens_in_the_orchestrators_workspace_named_and_follows_the_run_to_its_result() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Implement the delegate command\nsecond line of the brief");
    let hold = fixture.hold("hold");
    let first = fixture.events(
        "first.jsonl",
        &[STARTED, COMMAND_STARTED, COMMAND_FAILED, THOUGHT],
    );
    let second = fixture.events("second.jsonl", &[EDIT, MESSAGE, TURN_DONE]);
    let delegate = fixture.spawn(
        &[
            "--agent",
            "codex",
            "--title",
            "fix login redirect bug",
            "--model",
            "gpt-6.1-sol",
            "--effort",
            "high",
            "--brief",
            &brief,
        ],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_EVENTS_1", &first),
            ("FAKE_EVENTS_2", &second),
            ("FAKE_FINAL", "Added the command and its tests."),
        ],
        None,
    );
    let run_dir = fixture.run_dir();
    let open = fixture
        .log()
        .into_iter()
        .find(|line| line.starts_with("plugin pane open"))
        .unwrap();
    assert!(open.contains(
        "--plugin herdr-crew --entrypoint view --placement tab --workspace w1D --no-focus"
    ));
    assert!(open.contains(&format!("HERDR_CREW_CHILD_DIR={}", run_dir.display())));
    assert!(fixture
        .log()
        .contains(&"tab rename tab-test gpt-6.1-sol high: fix login redirect bug".to_owned()));
    assert_eq!(json(&run_dir.join("child.json"))["kind"], "codex");

    let mut view = fixture.view(&run_dir);
    // The brief first, then the steps as they stream in.
    view.expect("Implement the delegate command");
    view.expect("second line of the brief");
    view.expect("tool  $ cargo test");
    view.expect("result  error: cargo test (exit 101): boom ⏎ line2");
    view.expect("thought  Weighing the options");
    let text = view.text();
    assert!(text.find("Brief").unwrap() < text.find("session t-1").unwrap());
    assert!(!text.contains("status:"));

    fs::remove_file(&hold).unwrap();
    view.expect("✎ edit  update lib.rs, add new.rs");
    view.expect("message  Halfway there");
    view.expect("note  turn done — 120 in / 45 out tokens");
    view.expect("status: completed");
    view.expect("Added the command and its tests.");
    let text = view.text();
    assert!(text.find("Halfway there").unwrap() < text.find("status: completed").unwrap());
    view.expect_exit();
    assert_eq!(delegate.finish().code, Some(0));
}

#[test]
fn the_name_falls_back_to_the_codex_config_and_the_first_words_of_the_brief() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path(".codex")).unwrap();
    fs::write(
        fixture.path(".codex/config.toml"),
        "model = \"gpt-6.1-sol\" # default\nmodel_reasoning_effort = \"medium\"\n[profiles.fast]\nmodel = \"other\"\n",
    )
    .unwrap();
    let brief = fixture.brief("Port the wrapper\ninto Rust");
    fixture.run(&["--agent", "codex", "--brief", &brief], &[]);
    assert!(fixture
        .log()
        .contains(&"tab rename tab-test gpt-6.1-sol medium: Port the wrapper".to_owned()));
}

#[test]
fn the_tab_is_renamed_when_the_result_reveals_the_model_and_effort() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Port the wrapper");
    let hold = fixture.hold("hold");
    let delegate = fixture.spawn(
        &[
            "--agent",
            "codex",
            "--title",
            "port wrapper",
            "--brief",
            &brief,
        ],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_MODEL_JSON", "\"gpt-lane\""),
            ("FAKE_EFFORT_JSON", "\"low\""),
        ],
        None,
    );
    let run_dir = fixture.run_dir();
    let mut view = fixture.view(&run_dir);
    view.expect("Port the wrapper");
    fs::remove_file(&hold).unwrap();
    wait_until(|| {
        fixture
            .log()
            .contains(&"tab rename tab-test gpt-lane low: port wrapper".to_owned())
    });
    view.expect_exit();
    delegate.finish();
}

#[test]
fn every_way_a_run_can_end_has_its_status() {
    for (status, error, exit, expected, text) in [
        (
            "failed",
            "\"codex exited 1\"",
            "1",
            "status: failed",
            "codex exited 1",
        ),
        (
            "timeout",
            "\"did not finish within --timeout 1m\"",
            "124",
            "status: failed",
            "did not finish within",
        ),
        (
            "aborted",
            "null",
            "143",
            "status: killed",
            "Stopped before it finished",
        ),
    ] {
        let fixture = Fixture::new();
        let brief = fixture.brief("Do it");
        fixture.run(
            &["--agent", "codex", "--brief", &brief],
            &[
                ("FAKE_STATUS", status),
                ("FAKE_ERROR_JSON", error),
                ("FAKE_EXIT", exit),
            ],
        );
        let mut view = fixture.view(&fixture.run_dir());
        view.expect(expected);
        view.expect(text);
        view.expect_exit();
    }

    // A relay that vanished without a result is killed, unless Codex reported an error.
    let fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    fixture.run(
        &["--agent", "codex", "--brief", &brief],
        &[("FAKE_NO_RESULT", "1"), ("FAKE_EXIT", "1")],
    );
    let mut view = fixture.view(&fixture.run_dir());
    view.expect("status: killed");
    view.expect("The relay ended without writing a result");
    view.expect_exit();

    let fixture = Fixture::new();
    let failure = fixture.events(
        "failure.jsonl",
        &[r#"{"type":"turn.failed","error":{"message":"model overloaded"}}"#],
    );
    fixture.run(
        &["--agent", "codex", "--brief", &brief],
        &[
            ("FAKE_NO_RESULT", "1"),
            ("FAKE_EXIT", "1"),
            ("FAKE_EVENTS_1", &failure),
        ],
    );
    let mut view = fixture.view(&fixture.run_dir());
    view.expect("error  model overloaded");
    view.expect("status: failed");
    view.expect_exit();
}

#[test]
fn a_result_left_by_an_earlier_run_in_a_reused_out_dir_is_not_this_runs_result() {
    let fixture = Fixture::new();
    let out = fixture.path("reused");
    fs::create_dir_all(&out).unwrap();
    fs::write(
        out.join("result.json"),
        r#"{"status":"failed","error":"stale failure"}"#,
    )
    .unwrap();
    File::options()
        .write(true)
        .open(out.join("result.json"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let delegate = fixture.spawn(
        &[
            "--agent",
            "codex",
            "--brief",
            &brief,
            "--out-dir",
            out.to_str().unwrap(),
        ],
        &[("FAKE_HOLD", &hold)],
        None,
    );
    let run_dir = fixture.run_dir();
    let mut view = fixture.view(&run_dir);
    view.expect("Do it");
    thread::sleep(Duration::from_millis(400));
    assert!(!view.text().contains("status:"), "{}", view.text());
    assert_eq!(
        json(&run_dir.join("child.json"))["delegate"]["out_dir"],
        out.to_str().unwrap()
    );
    fs::remove_file(&hold).unwrap();
    view.expect("status: completed");
    view.expect_exit();
    delegate.finish();
}

#[test]
fn the_pane_shows_the_delegate_while_it_runs_and_is_cleared_when_it_ends() {
    // A pane without a detected agent is claimed as codex and given a working state.
    let fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let delegate = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[("FAKE_HOLD", &hold)],
        None,
    );
    wait_until(|| fixture.calls_matching(SET) == 1);
    let log = fixture.log();
    assert!(log.contains(&"pane report-agent w1D:p1 --source herdr-crew --agent codex --state working --message codex exec (delegated)".to_owned()));
    assert!(log.contains(&"pane report-metadata w1D:p1 --source herdr-crew --display-agent codex ⇠ claude --token delegate=1 delegate working".to_owned()));
    assert_eq!(fixture.calls_matching(CLEARED), 0);
    fs::remove_file(&hold).unwrap();
    delegate.finish();
    let log = fixture.log();
    assert_eq!(fixture.calls_matching(CLEARED), 1);
    assert!(index_of(&log, "--state working") < index_of(&log, SET));
    assert!(index_of(&log, SET) < index_of(&log, "--state idle"));
    assert!(
        index_of(&log, "--state idle")
            < index_of(
                &log,
                "release-agent w1D:p1 --source herdr-crew --agent codex"
            )
    );
    assert!(index_of(&log, "release-agent") < index_of(&log, CLEARED));
    assert!(log
        .last()
        .unwrap()
        .ends_with("--clear-display-agent --clear-token delegate"));

    // A pane that hosts a detected agent only gets the label and token.
    let fixture = Fixture::new();
    fs::write(
        fixture.path("agents.json"),
        r#"{"result":{"agents":[{"pane_id":"w9:p9","agent":"codex"},{"pane_id":"w1D:p1","agent":"claude"}]}}"#,
    )
    .unwrap();
    fixture.run(&["--agent", "codex", "--brief", &brief], &[]);
    assert_eq!(fixture.calls_matching(SET), 1);
    assert_eq!(fixture.calls_matching(CLEARED), 1);
    assert_eq!(fixture.calls_matching("report-agent"), 0);
    assert_eq!(fixture.calls_matching("release-agent"), 0);
}

#[test]
fn the_pane_is_cleared_after_the_wrapper_is_killed_but_only_once_the_relay_is_gone() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    let mut delegate = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
        ],
        None,
    );
    let relay = read_pid(&pidfile);
    fixture.track(relay);
    wait_until(|| {
        fixture
            .run_dirs()
            .iter()
            .any(|dir| dir.join("delegate.json").exists())
    });
    delegate.child.kill().unwrap();
    delegate.child.wait().unwrap();
    // The relay carries on unattended, and the pane keeps saying so.
    thread::sleep(Duration::from_millis(800));
    assert!(alive(relay));
    assert_eq!(fixture.calls_matching(CLEARED), 0);
    fs::remove_file(&hold).unwrap();
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
    assert!(!alive(relay));
    assert_eq!(fixture.calls_matching("--state idle"), 1);
    assert_eq!(fixture.calls_matching("release-agent"), 1);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(fixture.calls_matching(CLEARED), 1);
}

#[test]
fn a_view_still_ends_with_the_result_when_the_wrapper_was_killed() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    let mut delegate = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
        ],
        None,
    );
    fixture.track(read_pid(&pidfile));
    let run_dir = fixture.run_dir();
    let mut view = fixture.view(&run_dir);
    view.expect("Do it");
    delegate.child.kill().unwrap();
    delegate.child.wait().unwrap();
    thread::sleep(Duration::from_millis(300));
    assert!(!view.text().contains("status:"));
    fs::remove_file(&hold).unwrap();
    view.expect("status: completed");
    view.expect_exit();
}

#[test]
fn a_second_delegate_on_the_same_pane_keeps_the_label_until_the_last_one_ends() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let (a, b) = (fixture.hold("hold-a"), fixture.hold("hold-b"));
    let first = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[("FAKE_HOLD", &a)],
        None,
    );
    wait_until(|| {
        fixture
            .run_dirs()
            .iter()
            .any(|dir| dir.join("delegate.json").exists())
    });
    thread::sleep(Duration::from_millis(5));
    let second = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[("FAKE_HOLD", &b)],
        None,
    );
    wait_until(|| fixture.run_dirs().len() == 2);
    wait_until(|| {
        fixture
            .run_dirs()
            .iter()
            .all(|dir| dir.join("delegate.json").exists())
    });
    fs::remove_file(&a).unwrap();
    first.finish();
    thread::sleep(Duration::from_millis(500));
    assert_eq!(fixture.calls_matching(CLEARED), 0);
    fs::remove_file(&b).unwrap();
    second.finish();
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
}

#[test]
fn the_delegate_is_stopped_when_the_orchestrators_session_closes() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let (pidfile, grandchild) = (fixture.path("relay.pid"), fixture.path("grandchild.pid"));
    let mut orchestrator = Command::new("sleep").arg("60").spawn().unwrap();
    // A relay that ignores the polite stop, with a child that left its process group.
    let delegate = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[
            (
                "HERDR_CREW_ORCHESTRATOR_PID",
                &orchestrator.id().to_string(),
            ),
            ("FAKE_HOLD", &hold),
            ("FAKE_IGNORE_TERM", "1"),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
            ("FAKE_GRANDCHILD", grandchild.to_str().unwrap()),
        ],
        None,
    );
    let relay = read_pid(&pidfile);
    let leftover = read_pid(&grandchild);
    fixture.track(relay);
    fixture.track(leftover);
    thread::sleep(Duration::from_millis(600));
    assert!(alive(relay) && alive(leftover));
    orchestrator.kill().unwrap();
    orchestrator.wait().unwrap();
    wait_until(|| !alive(relay) && !alive(leftover));
    let finished = delegate.finish();
    assert_eq!(finished.code, Some(128 + 9));
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
}

#[test]
fn a_relay_that_stops_politely_reports_aborted_to_the_view() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    let mut orchestrator = Command::new("sleep").arg("60").spawn().unwrap();
    let delegate = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[
            (
                "HERDR_CREW_ORCHESTRATOR_PID",
                &orchestrator.id().to_string(),
            ),
            ("FAKE_HOLD", &hold),
            ("FAKE_ABORT_ON_TERM", "1"),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
        ],
        None,
    );
    fixture.track(read_pid(&pidfile));
    let run_dir = fixture.run_dir();
    let mut view = fixture.view(&run_dir);
    view.expect("Do it");
    orchestrator.kill().unwrap();
    orchestrator.wait().unwrap();
    view.expect("status: killed");
    view.expect_exit();
    assert_eq!(delegate.finish().code, Some(143));
}

#[test]
fn the_orchestrator_is_the_nearest_claude_ancestor() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    // A process named claude that runs the wrapper as a child, as Claude Code's shell tool does.
    std::os::unix::fs::symlink("/bin/sh", fixture.path("claude")).unwrap();
    let mut session = Command::new(fixture.path("claude"))
        .arg("-c")
        .arg(format!(
            "'{}' delegate --agent codex --brief '{brief}'; true",
            env!("CARGO_BIN_EXE_herdr-crew")
        ))
        .env_clear()
        .envs(
            fixture
                .command()
                .get_envs()
                .filter_map(|(k, v)| Some((k, v?))),
        )
        .env_remove("HERDR_CREW_ORCHESTRATOR_PID")
        .env("FAKE_HOLD", &hold)
        .env("FAKE_PIDFILE", &pidfile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let relay = read_pid(&pidfile);
    fixture.track(relay);
    wait_until(|| {
        fixture
            .run_dirs()
            .iter()
            .any(|dir| dir.join("delegate.json").exists())
    });
    thread::sleep(Duration::from_millis(600));
    assert!(alive(relay));
    session.kill().unwrap();
    session.wait().unwrap();
    wait_until(|| !alive(relay));
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
}

#[test]
fn a_termination_sent_to_the_wrapper_reaches_the_relay_and_its_exit_code_comes_back() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    let delegate = fixture.spawn(
        &["--agent", "codex", "--brief", &brief],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_ABORT_ON_TERM", "1"),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
        ],
        None,
    );
    fixture.track(read_pid(&pidfile));
    let run_dir = fixture.run_dir();
    unsafe { libc::kill(delegate.child.id() as i32, libc::SIGTERM) };
    let finished = delegate.finish();
    assert_eq!(finished.code, Some(143));
    assert_eq!(json(&run_dir.join("out/result.json"))["status"], "aborted");
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(fixture.calls_matching(CLEARED), 1);
}

const CLAUDE_INIT: &str = r#"{"type":"system","subtype":"init","model":"claude-sonnet-5-5","session_id":"s-1","tools":[]}"#;
const CLAUDE_HOOK: &str = r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup","session_id":"s-1"}"#;
const CLAUDE_RATE_LIMIT: &str =
    r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
const CLAUDE_THOUGHT_AND_COMMAND: &str = r#"{"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[{"type":"thinking","thinking":"Weighing the options"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#;
const CLAUDE_WITHHELD_THOUGHT: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#;
const CLAUDE_COMMAND_FAILED: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}]}}"#;
const CLAUDE_EDIT: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/repo/src/lib.rs"}}]}}"#;
const CLAUDE_MESSAGE: &str =
    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Halfway there"}]}}"#;
const CLAUDE_NESTED: &str = r#"{"type":"assistant","parent_tool_use_id":"t9","message":{"content":[{"type":"text","text":"from a nested subagent"}]}}"#;
const CLAUDE_DONE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"Added the command and its tests.","num_turns":3}"#;
const CLAUDE_FAILED: &str = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"Hit the turn limit","num_turns":9}"#;

#[test]
fn a_claude_delegate_gets_the_relay_args_output_and_exit_code_unchanged() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    let out = fixture.path("mine");
    let out = out.to_str().unwrap();
    let args = [
        "--agent",
        "claude",
        "--title",
        "short task",
        "--brief",
        &brief,
        "--read-only",
        "--max-turns",
        "5",
        "--out-dir",
        out,
        "--cd",
        "/repo",
    ];
    let finished = fixture.run(&args, &[("FAKE_EXIT", "3")]);
    assert_eq!(finished.code, Some(3));
    assert_eq!(
        finished.stdout,
        format!(
            "ARGS:[--brief][{brief}][--read-only][--max-turns][5][--out-dir][{out}][--cd][/repo]\n"
        )
    );
    assert_eq!(finished.stderr, "relay-stderr\n");
    assert!(fixture.path("mine/events.jsonl").exists());
    assert_eq!(
        json(&fixture.run_dirs().remove(0).join("child.json"))["kind"],
        "claude"
    );

    // Without an out dir one is forced, and a valueless option is left for the relay to reject.
    let fixture = Fixture::new();
    let finished = fixture.run(
        &["--agent", "claude", "--brief", &brief, "--max-budget-usd"],
        &[],
    );
    let run_dir = fixture.run_dirs().remove(0);
    assert_eq!(
        finished.stdout,
        format!(
            "ARGS:[--out-dir][{}][--brief][{brief}][--max-budget-usd]\n",
            run_dir.join("out").display()
        )
    );
}

#[test]
fn the_claude_relay_is_found_through_its_own_variable_and_default_location() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    // The Codex relay's variable has no say over a Claude delegate.
    let finished = fixture.run(
        &["--agent", "claude", "--brief", &brief],
        &[("CODEX_DELEGATE_RELAY", "/nonexistent/relay.mjs")],
    );
    assert_eq!(finished.code, Some(0));

    let missing = fixture.run(
        &["--agent", "claude", "--brief", &brief],
        &[("CLAUDE_DELEGATE_RELAY", "/nonexistent/relay.mjs")],
    );
    assert_eq!(missing.code, Some(2));
    assert!(missing
        .stderr
        .contains("relay not found: /nonexistent/relay.mjs"));

    let scripts = fixture.path(".claude/skills/claude-delegate/scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(scripts.join("relay.mjs"), "echo default-relay\n").unwrap();
    let finished = fixture.run(
        &["--agent", "claude", "--brief", &brief],
        &[("CLAUDE_DELEGATE_RELAY", "")],
    );
    assert_eq!(finished.stdout, "default-relay\n");
}

#[test]
fn outside_herdr_a_claude_relay_runs_unchanged_with_no_view_and_no_herdr_calls() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do the thing");
    let out = fixture.path("mine");
    let args = [
        "--brief",
        &brief,
        "--out-dir",
        out.to_str().unwrap(),
        "--model",
        "opus",
    ];
    let mut with_agent = vec!["--agent", "claude", "--title", "ignored"];
    with_agent.extend(args);
    let direct = Command::new("/bin/sh")
        .arg(fixture.path("relay.sh"))
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    for envs in [vec![("HERDR_ENV", "0")], vec![("HERDR_PANE_ID", "")]] {
        let finished = fixture.run(&with_agent, &envs);
        assert_eq!(finished.code, direct.status.code());
        assert_eq!(finished.stdout.as_bytes(), direct.stdout);
        assert_eq!(finished.stderr.as_bytes(), direct.stderr);
    }
    assert!(!fixture.path("herdr.log").exists());
    assert!(!fixture.path("state").exists());

    let finished = fixture.run(&with_agent, &[("HERDR_ENV", "0"), ("FAKE_EXIT", "7")]);
    assert_eq!(finished.code, Some(7));
}

#[test]
fn a_claude_view_shows_the_brief_then_its_steps_then_the_result() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Implement the delegate command\nsecond line of the brief");
    let hold = fixture.hold("hold");
    let first = fixture.events(
        "first.jsonl",
        &[
            CLAUDE_HOOK,
            CLAUDE_INIT,
            CLAUDE_RATE_LIMIT,
            CLAUDE_WITHHELD_THOUGHT,
            CLAUDE_THOUGHT_AND_COMMAND,
            CLAUDE_COMMAND_FAILED,
            CLAUDE_NESTED,
        ],
    );
    let second = fixture.events("second.jsonl", &[CLAUDE_EDIT, CLAUDE_MESSAGE, CLAUDE_DONE]);
    let delegate = fixture.spawn(
        &[
            "--agent",
            "claude",
            "--title",
            "fix login redirect bug",
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high",
            "--brief",
            &brief,
        ],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_EVENTS_1", &first),
            ("FAKE_EVENTS_2", &second),
            ("FAKE_FINAL", "Added the command and its tests."),
        ],
        None,
    );
    let run_dir = fixture.run_dir();
    let open = fixture
        .log()
        .into_iter()
        .find(|line| line.starts_with("plugin pane open"))
        .unwrap();
    assert!(open.contains(
        "--plugin herdr-crew --entrypoint view --placement tab --workspace w1D --no-focus"
    ));
    assert!(fixture
        .log()
        .contains(&"tab rename tab-test opus-5.5 high: fix login redirect bug".to_owned()));
    assert_eq!(json(&run_dir.join("child.json"))["kind"], "claude");

    let mut view = fixture.view(&run_dir);
    view.expect("Implement the delegate command");
    view.expect("second line of the brief");
    view.expect("note  session s-1");
    view.expect("thought  Weighing the options");
    view.expect("tool  Bash: cargo test");
    view.expect("result  error: boom");
    let text = view.text();
    assert!(text.find("Brief").unwrap() < text.find("session s-1").unwrap());
    assert!(!text.contains("status:"));
    assert!(!text.contains("from a nested subagent"));
    assert!(!text.contains("hook"));
    // A withheld thought is not a step, and steps carry the time the view saw them.
    assert_eq!(text.matches("thought").count(), 1);
    assert!(!text.contains("--:--:--"), "{text}");

    fs::remove_file(&hold).unwrap();
    view.expect("✎ edit  Edit: /repo/src/lib.rs");
    view.expect("message  Halfway there");
    view.expect("status: completed");
    view.expect("Added the command and its tests.");
    let text = view.text();
    assert!(text.find("Halfway there").unwrap() < text.find("status: completed").unwrap());
    // The success result is shown once, as the result.
    assert_eq!(text.matches("Added the command and its tests.").count(), 1);
    view.expect_exit();
    assert_eq!(delegate.finish().code, Some(0));
}

#[test]
fn a_claude_tab_takes_its_model_from_the_run_when_the_caller_named_none() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Port the wrapper");
    let hold = fixture.hold("hold");
    let events = fixture.events("events.jsonl", &[CLAUDE_INIT]);
    let delegate = fixture.spawn(
        &[
            "--agent",
            "claude",
            "--title",
            "port wrapper",
            "--brief",
            &brief,
        ],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_EVENTS_1", &events),
            ("FAKE_EFFORT_JSON", "\"low\""),
        ],
        None,
    );
    let run_dir = fixture.run_dir();
    assert!(fixture
        .log()
        .contains(&"tab rename tab-test claude: port wrapper".to_owned()));
    let mut view = fixture.view(&run_dir);
    view.expect("note  session s-1");
    wait_until(|| {
        fixture
            .log()
            .contains(&"tab rename tab-test sonnet-5.5: port wrapper".to_owned())
    });
    // A lane's effort is only known from the result.
    fs::remove_file(&hold).unwrap();
    wait_until(|| {
        fixture
            .log()
            .contains(&"tab rename tab-test sonnet-5.5 low: port wrapper".to_owned())
    });
    view.expect_exit();
    delegate.finish();

    // The first words of the brief name a run with neither title nor model.
    let fixture = Fixture::new();
    let brief = fixture.brief("Port the wrapper\ninto Rust");
    fixture.run(&["--agent", "claude", "--brief", &brief], &[]);
    assert!(fixture
        .log()
        .contains(&"tab rename tab-test claude: Port the wrapper".to_owned()));
}

#[test]
fn every_way_a_claude_run_can_end_has_its_status() {
    // An error result: the relay's error says so, and Claude says why.
    let fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let events = fixture.events("failure.jsonl", &[CLAUDE_INIT, CLAUDE_FAILED]);
    fixture.run(
        &["--agent", "claude", "--brief", &brief],
        &[
            ("FAKE_STATUS", "failed"),
            (
                "FAKE_ERROR_JSON",
                "\"claude returned an error result (error_max_turns)\"",
            ),
            ("FAKE_EXIT", "1"),
            ("FAKE_EVENTS_1", &events),
            ("FAKE_FINAL", "Hit the turn limit"),
        ],
    );
    let mut view = fixture.view(&fixture.run_dir());
    view.expect("error  Hit the turn limit");
    view.expect("status: failed");
    view.expect("claude returned an error result (error_max_turns): Hit the turn limit");
    view.expect_exit();

    for (status, error, exit, expected, text) in [
        (
            "timeout",
            "\"claude did not finish within --timeout 1m\"",
            "124",
            "status: failed",
            "claude did not finish within",
        ),
        (
            "claude_unavailable",
            "\"spawn claude ENOENT\"",
            "127",
            "status: failed",
            "spawn claude ENOENT",
        ),
        (
            "aborted",
            "\"the relay was killed by SIGTERM\"",
            "143",
            "status: killed",
            "the relay was killed by SIGTERM",
        ),
    ] {
        let fixture = Fixture::new();
        let brief = fixture.brief("Do it");
        fixture.run(
            &["--agent", "claude", "--brief", &brief],
            &[
                ("FAKE_STATUS", status),
                ("FAKE_ERROR_JSON", error),
                ("FAKE_EXIT", exit),
            ],
        );
        let mut view = fixture.view(&fixture.run_dir());
        view.expect(expected);
        view.expect(text);
        view.expect_exit();
    }

    // A relay that vanished without a result is killed, unless Claude reported an error.
    let fixture = Fixture::new();
    fixture.run(
        &["--agent", "claude", "--brief", &brief],
        &[("FAKE_NO_RESULT", "1"), ("FAKE_EXIT", "1")],
    );
    let mut view = fixture.view(&fixture.run_dir());
    view.expect("status: killed");
    view.expect("The relay ended without writing a result");
    view.expect_exit();

    let fixture = Fixture::new();
    fixture.run(
        &["--agent", "claude", "--brief", &brief],
        &[
            ("FAKE_NO_RESULT", "1"),
            ("FAKE_EXIT", "1"),
            ("FAKE_EVENTS_1", &events),
        ],
    );
    let mut view = fixture.view(&fixture.run_dir());
    view.expect("status: failed");
    view.expect("Hit the turn limit");
    view.expect_exit();
}

#[test]
fn the_pane_shows_a_claude_delegate_while_it_runs_and_is_cleared_when_it_ends() {
    let fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let delegate = fixture.spawn(
        &["--agent", "claude", "--brief", &brief],
        &[("FAKE_HOLD", &hold)],
        None,
    );
    wait_until(|| fixture.calls_matching(SET) == 1);
    let log = fixture.log();
    assert!(log.iter().any(|line| line.starts_with(
        "pane report-agent w1D:p1 --source herdr-crew --agent claude --state working"
    )));
    assert!(log.iter().any(|line| line.starts_with(
        "pane report-metadata w1D:p1 --source herdr-crew --display-agent claude ⇠ claude --token "
    )));
    assert_eq!(fixture.calls_matching(CLEARED), 0);
    fs::remove_file(&hold).unwrap();
    delegate.finish();
    assert_eq!(fixture.calls_matching(CLEARED), 1);
    assert_eq!(fixture.calls_matching("--state idle"), 1);
    assert!(fixture
        .log()
        .last()
        .unwrap()
        .ends_with("--clear-display-agent --clear-token delegate"));

    // A pane that hosts a detected agent only gets the label and token.
    let fixture = Fixture::new();
    fs::write(
        fixture.path("agents.json"),
        r#"{"result":{"agents":[{"pane_id":"w1D:p1","agent":"claude"}]}}"#,
    )
    .unwrap();
    fixture.run(&["--agent", "claude", "--brief", &brief], &[]);
    assert_eq!(fixture.calls_matching("display-agent claude ⇠ claude"), 1);
    assert_eq!(fixture.calls_matching(CLEARED), 1);
    assert_eq!(fixture.calls_matching("report-agent"), 0);

    // Codex delegates keep their own label.
    let fixture = Fixture::new();
    fixture.run(&["--agent", "codex", "--brief", &brief], &[]);
    assert_eq!(fixture.calls_matching("display-agent codex ⇠ claude"), 1);
    assert_eq!(fixture.calls_matching("claude ⇠ claude"), 0);
}

#[test]
fn a_claude_delegates_pane_is_cleared_after_the_wrapper_is_killed() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    let mut delegate = fixture.spawn(
        &["--agent", "claude", "--brief", &brief],
        &[
            ("FAKE_HOLD", &hold),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
        ],
        None,
    );
    let relay = read_pid(&pidfile);
    fixture.track(relay);
    let run_dir = fixture.run_dir();
    let mut view = fixture.view(&run_dir);
    view.expect("Do it");
    delegate.child.kill().unwrap();
    delegate.child.wait().unwrap();
    // The relay carries on unattended, and the pane keeps saying so.
    thread::sleep(Duration::from_millis(800));
    assert!(alive(relay));
    assert_eq!(fixture.calls_matching(CLEARED), 0);
    assert!(!view.text().contains("status:"));
    fs::remove_file(&hold).unwrap();
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
    assert!(!alive(relay));
    assert_eq!(fixture.calls_matching("release-agent"), 1);
    view.expect("status: completed");
    view.expect_exit();
    thread::sleep(Duration::from_millis(500));
    assert_eq!(fixture.calls_matching(CLEARED), 1);
}

#[test]
fn a_claude_delegate_is_stopped_when_the_orchestrators_session_closes() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let (pidfile, grandchild) = (fixture.path("relay.pid"), fixture.path("grandchild.pid"));
    let mut orchestrator = Command::new("sleep").arg("60").spawn().unwrap();
    let delegate = fixture.spawn(
        &["--agent", "claude", "--brief", &brief],
        &[
            (
                "HERDR_CREW_ORCHESTRATOR_PID",
                &orchestrator.id().to_string(),
            ),
            ("FAKE_HOLD", &hold),
            ("FAKE_IGNORE_TERM", "1"),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
            ("FAKE_GRANDCHILD", grandchild.to_str().unwrap()),
        ],
        None,
    );
    let relay = read_pid(&pidfile);
    let leftover = read_pid(&grandchild);
    fixture.track(relay);
    fixture.track(leftover);
    thread::sleep(Duration::from_millis(600));
    assert!(alive(relay) && alive(leftover));
    orchestrator.kill().unwrap();
    orchestrator.wait().unwrap();
    wait_until(|| !alive(relay) && !alive(leftover));
    assert_eq!(delegate.finish().code, Some(128 + 9));
    wait_until(|| fixture.calls_matching(CLEARED) == 1);
}

#[test]
fn a_claude_relay_that_stops_politely_reports_aborted_to_the_view() {
    let mut fixture = Fixture::new();
    let brief = fixture.brief("Do it");
    let hold = fixture.hold("hold");
    let pidfile = fixture.path("relay.pid");
    let mut orchestrator = Command::new("sleep").arg("60").spawn().unwrap();
    let delegate = fixture.spawn(
        &["--agent", "claude", "--brief", &brief],
        &[
            (
                "HERDR_CREW_ORCHESTRATOR_PID",
                &orchestrator.id().to_string(),
            ),
            ("FAKE_HOLD", &hold),
            ("FAKE_ABORT_ON_TERM", "1"),
            ("FAKE_PIDFILE", pidfile.to_str().unwrap()),
        ],
        None,
    );
    fixture.track(read_pid(&pidfile));
    let mut view = fixture.view(&fixture.run_dir());
    view.expect("Do it");
    orchestrator.kill().unwrap();
    orchestrator.wait().unwrap();
    view.expect("status: killed");
    view.expect_exit();
    assert_eq!(delegate.finish().code, Some(143));
}
