use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "herdr-crew-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("calls")).unwrap();
        fs::write(
            root.join("herdr"),
            r#"#!/bin/sh
printf '%s\n' "$@" > "$HERDR_TEST_CALLS/$$"
if [ "$1" = plugin ]; then
    sleep "${HERDR_TEST_DELAY:-0}"
    if [ "${HERDR_TEST_FAIL:-0}" = 1 ]; then
        echo 'synthetic Herdr failure' >&2
        exit 3
    fi
    if [ "${HERDR_TEST_BAD_REPLY:-0}" = 1 ]; then
        printf '%s\n' '{bad-reply'
        exit 0
    fi
    printf '%s\n' '{"result":{"plugin_pane":{"pane":{"tab_id":"tab-test"}}}}'
fi
"#,
        )
        .unwrap();
        fs::set_permissions(root.join("herdr"), fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-crew"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", "w1D:p1")
            .env("HERDR_CREW_STATE_DIR", self.root.join("state"))
            .env("HERDR_BIN", self.root.join("herdr"))
            .env("HERDR_TEST_CALLS", self.root.join("calls"))
            .env("HERDR_CREW_LINGER_MS", "200");
        command
    }

    fn input(&self, agent: &str, event: &str) -> Value {
        json!({
            "session_id": "session-1", "transcript_path": self.root.join("conversation.jsonl"),
            "cwd": self.root, "hook_event_name": event, "agent_id": agent,
            "agent_type": "general-purpose"
        })
    }

    fn hook(&self, action: &str, input: &Value) -> Output {
        invoke_hook(self.command(), action, &input.to_string())
    }

    fn child_dir(&self, agent: &str) -> PathBuf {
        self.root.join("state/session-1").join(agent)
    }

    fn transcript(&self, agent: &str) -> PathBuf {
        self.root
            .join("conversation/subagents")
            .join(format!("agent-{agent}.jsonl"))
    }

    fn calls(&self) -> Vec<Vec<String>> {
        fs::read_dir(self.root.join("calls"))
            .unwrap()
            .map(|entry| {
                fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }

    fn prepare_child(&self, agent: &str) {
        fs::create_dir_all(self.child_dir(agent)).unwrap();
        fs::create_dir_all(self.transcript(agent).parent().unwrap()).unwrap();
        fs::write(self.child_dir(agent).join("child.json"), json!({
            "agent_id": agent, "agent_type": "general-purpose", "transcript": self.transcript(agent)
        }).to_string()).unwrap();
    }

    fn view(&self, agent: &str) -> View {
        let output = self.root.join(format!("view-{agent}.txt"));
        let errors = self.root.join(format!("view-{agent}.err"));
        let child = self
            .command()
            .arg("view")
            .env("HERDR_CREW_CHILD_DIR", self.child_dir(agent))
            .stdin(Stdio::null())
            .stdout(File::create(&output).unwrap())
            .stderr(File::create(&errors).unwrap())
            .spawn()
            .unwrap();
        View {
            child,
            output,
            errors,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct View {
    child: Child,
    output: PathBuf,
    errors: PathBuf,
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
        assert!(
            self.child.wait().unwrap().success(),
            "{}",
            fs::read_to_string(&self.errors).unwrap()
        );
        assert_eq!(fs::read_to_string(&self.errors).unwrap(), "");
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for detached work"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn invoke_hook(mut command: Command, action: &str, input: &str) -> Output {
    let mut child = command
        .args(["hook", action])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn assert_silent_success(output: &Output) {
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn line(agent: &str, kind: &str, content: Value) -> String {
    format!(
        "{}\n",
        json!({
            "type": kind, "isSidechain": true, "agentId": agent,
            "timestamp": "2026-10-27T10:11:12.000Z", "message": {"content": content}
        })
    )
}

fn assistant(agent: &str, extra: Value) -> String {
    let mut line: Value = serde_json::from_str(&line(
        agent,
        "assistant",
        json!([{"type":"text", "text":"A step"}]),
    ))
    .unwrap();
    for (key, value) in extra.as_object().unwrap() {
        line[key] = value.clone();
    }
    format!("{line}\n")
}

fn start(fixture: &Fixture, agent: &str) {
    assert_silent_success(&fixture.hook("subagent-start", &fixture.input(agent, "SubagentStart")));
    wait_until(|| fixture.child_dir(agent).join("tab.json").exists());
}

fn renames(fixture: &Fixture) -> Vec<String> {
    fixture
        .calls()
        .into_iter()
        .filter(|args| args[..2] == ["tab", "rename"])
        .map(|args| {
            assert_eq!(args[2], "tab-test");
            args[3].clone()
        })
        .collect()
}

fn finish(fixture: &Fixture, agent: &str) {
    assert_silent_success(&fixture.hook("subagent-stop", &fixture.input(agent, "SubagentStop")));
    wait_until(|| fixture.child_dir(agent).join("stop.json").exists());
}

fn start_conversation(fixture: &Fixture, agent: &str) {
    fixture.prepare_child(agent);
    append(
        &fixture.transcript(agent),
        line(agent, "user", json!("A brief")).as_bytes(),
    );
}

fn stop_with(fixture: &Fixture, agent: &str, message: Option<&str>) {
    let mut stop = fixture.input(agent, "SubagentStop");
    if let Some(message) = message {
        stop["last_assistant_message"] = json!(message);
    }
    assert_silent_success(&fixture.hook("subagent-stop", &stop));
    wait_until(|| fixture.child_dir(agent).join("stop.json").exists());
}

fn start_time(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn start_opens_a_tab_without_focus_and_does_not_wait_for_herdr() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command
        .env("HERDR_WORKSPACE_ID", "workspace-explicit")
        .env("HERDR_TEST_DELAY", "1");
    let started = Instant::now();
    let output = invoke_hook(
        command,
        "subagent-start",
        &fixture.input("child-1", "SubagentStart").to_string(),
    );
    assert_silent_success(&output);
    assert!(started.elapsed() < Duration::from_millis(500));
    wait_until(|| {
        fixture
            .calls()
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("tab"))
    });
    let calls = fixture.calls();
    let opened = calls.iter().find(|args| args[0] == "plugin").unwrap();
    assert_eq!(
        &opened[..14],
        [
            "plugin",
            "pane",
            "open",
            "--plugin",
            "herdr-crew",
            "--entrypoint",
            "view",
            "--placement",
            "tab",
            "--workspace",
            "workspace-explicit",
            "--no-focus",
            "--env",
            &format!(
                "HERDR_CREW_CHILD_DIR={}",
                fixture.child_dir("child-1").display()
            )
        ]
    );
    assert!(calls
        .iter()
        .any(|args| args == &["tab", "rename", "tab-test", "sub: general-purpose"]));
    let record: Value =
        serde_json::from_slice(&fs::read(fixture.child_dir("child-1").join("child.json")).unwrap())
            .unwrap();
    assert_eq!(
        record["transcript"],
        fixture.transcript("child-1").to_str().unwrap()
    );
}

#[test]
fn workspace_fallback_and_overlapping_subagents_are_independent() {
    let fixture = Fixture::new();
    let mut hooks = Vec::new();
    for index in 0..12 {
        let agent = format!("child-{index}");
        let mut child = fixture
            .command()
            .args(["hook", "subagent-start"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                fixture
                    .input(&agent, "SubagentStart")
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        hooks.push(child);
    }
    for child in hooks {
        assert_silent_success(&child.wait_with_output().unwrap());
    }
    wait_until(|| {
        fixture
            .calls()
            .iter()
            .filter(|args| args.first().map(String::as_str) == Some("tab"))
            .count()
            == 12
    });
    let opened: Vec<_> = fixture
        .calls()
        .into_iter()
        .filter(|args| args[0] == "plugin")
        .collect();
    assert_eq!(opened.len(), 12);
    let mut child_paths = std::collections::HashSet::new();
    for args in opened {
        assert_eq!(args[10], "w1D");
        child_paths.insert(args[13].clone());
    }
    assert_eq!(child_paths.len(), 12);
    for index in 0..12 {
        assert_silent_success(&fixture.hook(
            "subagent-start",
            &fixture.input(&format!("child-{index}"), "SubagentStart"),
        ));
    }
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|args| args[0] == "plugin")
            .count(),
        12
    );
}

#[test]
fn hooks_outside_herdr_are_silent_no_ops() {
    let fixture = Fixture::new();
    for (key, setting) in [
        ("HERDR_ENV", None),
        ("HERDR_ENV", Some("0")),
        ("HERDR_PANE_ID", Some("")),
    ] {
        for action in ["subagent-start", "subagent-stop"] {
            let mut command = fixture.command();
            match setting {
                Some(setting) => {
                    command.env(key, setting);
                }
                None => {
                    command.env_remove(key);
                }
            }
            assert_silent_success(&invoke_hook(command, action, "malformed"));
        }
    }
    assert!(fixture.calls().is_empty());
    assert!(!fixture.root.join("state").exists());
}

#[test]
fn user_commands_work_outside_herdr() {
    let fixture = Fixture::new();
    let help = fixture
        .command()
        .env_remove("HERDR_ENV")
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("install-hooks"));

    let settings = fixture.root.join("settings.json");
    let installed = fixture
        .command()
        .env_remove("HERDR_ENV")
        .args(["install-hooks", "--settings"])
        .arg(&settings)
        .output()
        .unwrap();
    assert!(installed.status.success());
    assert!(fs::read_to_string(&settings)
        .unwrap()
        .contains("SubagentStart"));
}

#[test]
fn malformed_and_nested_hooks_are_silent_no_ops() {
    let fixture = Fixture::new();
    for input in ["", "{broken", "null", "{}"] {
        assert_silent_success(&invoke_hook(fixture.command(), "subagent-start", input));
    }
    for agent in ["../escape", "", "a/b"] {
        assert_silent_success(
            &fixture.hook("subagent-start", &fixture.input(agent, "SubagentStart")),
        );
    }
    let mut nested = fixture.input("child", "SubagentStart");
    nested["transcript_path"] = json!(fixture.transcript("orchestrator"));
    assert_silent_success(&fixture.hook("subagent-start", &nested));
    assert_silent_success(&fixture.hook("subagent-start", &fixture.input("child", "SubagentStop")));
    assert!(fixture.calls().is_empty());
    assert!(!fixture.root.join("state").exists());
}

#[test]
fn herdr_and_state_failures_do_not_escape_the_hook() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command.env("HERDR_TEST_FAIL", "1");
    assert_silent_success(&invoke_hook(
        command,
        "subagent-start",
        &fixture.input("child", "SubagentStart").to_string(),
    ));
    wait_until(|| {
        fixture.child_dir("child").join("child.json").exists()
            && !fixture.child_dir("child").join("open.lock").exists()
    });
    assert_silent_success(
        &fixture.hook("subagent-start", &fixture.input("child", "SubagentStart")),
    );
    wait_until(|| fixture.calls().iter().any(|args| args[0] == "tab"));
    fs::write(fixture.root.join("blocked-state"), "not a directory").unwrap();
    let mut command = fixture.command();
    command.env("HERDR_CREW_STATE_DIR", fixture.root.join("blocked-state"));
    assert_silent_success(&invoke_hook(
        command,
        "subagent-start",
        &fixture.input("other-child", "SubagentStart").to_string(),
    ));
}

#[test]
fn view_waits_for_transcript_and_follows_partial_lines_live() {
    let fixture = Fixture::new();
    fixture.prepare_child("child");
    let mut view = fixture.view("child");
    thread::sleep(Duration::from_millis(80));
    assert!(view.child.try_wait().unwrap().is_none());
    assert_eq!(view.text(), "");
    append(&fixture.transcript("child"), line("child", "user", json!([
        {"type":"text", "text":"Synthetic brief"}, {"type":"text", "text":"Keep both paragraphs"}
    ])).as_bytes());
    view.expect("Keep both paragraphs");
    let partial = line(
        "child",
        "assistant",
        json!([
            {"type":"thinking", "thinking":"Consider the alternatives"},
            {"type":"tool_use", "name":"Bash", "input":{"command":"printf 猫"}},
            {"type":"tool_use", "name":"Edit", "input":{"file_path":"src/example.rs", "old_string":"a", "new_string":"b"}},
            {"type":"text", "text":"A live message"}
        ]),
    );
    let split = partial.find('猫').unwrap() + 1;
    append(&fixture.transcript("child"), &partial.as_bytes()[..split]);
    thread::sleep(Duration::from_millis(100));
    assert!(!view.text().contains("Consider the alternatives"));
    append(&fixture.transcript("child"), &partial.as_bytes()[split..]);
    view.expect("A live message");
    append(&fixture.transcript("child"), line("child", "user", json!([
        {"type":"tool_result", "tool_use_id":"synthetic-tool", "content":[{"type":"text", "text":"A tool failed"}], "is_error":true}
    ])).as_bytes());
    view.expect("error: A tool failed");
    let rendered = view.text();
    assert!(rendered.find("Brief").unwrap() < rendered.find("Synthetic brief").unwrap());
    assert!(
        rendered.find("Synthetic brief").unwrap()
            < rendered.find("Consider the alternatives").unwrap()
    );
    assert!(rendered.contains("10:11:12 thought"));
    assert!(rendered.contains("Bash: printf 猫"));
    assert!(rendered.contains("✎ edit  Edit: src/example.rs"));
    let stopped = Instant::now();
    finish(&fixture, "child");
    assert!(view.child.try_wait().unwrap().is_none());
    view.expect_exit();
    assert!(stopped.elapsed() >= Duration::from_millis(180));
    assert!(stopped.elapsed() < Duration::from_secs(2));
    assert_eq!(view.text().matches("A live message").count(), 1);
}

#[test]
fn metadata_closes_a_view_when_the_stop_hook_is_missed() {
    let fixture = Fixture::new();
    fixture.prepare_child("child");
    append(
        &fixture.transcript("child"),
        line("child", "user", json!("A string brief")).as_bytes(),
    );
    let mut view = fixture.view("child");
    view.expect("A string brief");
    let stopped = Instant::now();
    fs::write(
        fixture.transcript("child").with_extension("meta.json"),
        r#"{"finished":true}"#,
    )
    .unwrap();
    view.expect_exit();
    assert!(stopped.elapsed() >= Duration::from_millis(180));
    assert!(!fixture.child_dir("child").join("stop.json").exists());
}

#[test]
fn stop_before_start_preserves_the_stop_and_can_supply_the_transcript_path() {
    let fixture = Fixture::new();
    let actual = fixture.root.join("alternate.jsonl");
    fs::write(&actual, line("child", "user", json!("Alternate brief"))).unwrap();
    let mut input = fixture.input("child", "SubagentStop");
    input["agent_transcript_path"] = json!(actual);
    assert_silent_success(&fixture.hook("subagent-stop", &input));
    wait_until(|| fixture.child_dir("child").join("stop.json").exists());
    assert_silent_success(
        &fixture.hook("subagent-start", &fixture.input("child", "SubagentStart")),
    );
    wait_until(|| fixture.calls().iter().any(|args| args[0] == "tab"));
    let mut view = fixture.view("child");
    view.expect_exit();
    assert!(view.text().contains("Alternate brief"));
}

#[test]
fn simultaneous_views_stop_independently_and_ignore_unrelated_lines() {
    let fixture = Fixture::new();
    for agent in ["one", "two"] {
        fixture.prepare_child(agent);
        append(&fixture.transcript(agent), b"{broken}\n");
        append(
            &fixture.transcript(agent),
            line("unrelated", "user", json!("Private unrelated brief")).as_bytes(),
        );
        append(
            &fixture.transcript(agent),
            line(agent, "progress", json!("ignored progress")).as_bytes(),
        );
        append(
            &fixture.transcript(agent),
            line(agent, "user", json!(format!("Brief for {agent}"))).as_bytes(),
        );
    }
    let mut one = fixture.view("one");
    let mut two = fixture.view("two");
    one.expect("Brief for one");
    two.expect("Brief for two");
    finish(&fixture, "one");
    one.expect_exit();
    assert!(two.child.try_wait().unwrap().is_none());
    assert!(!one.text().contains("Private"));
    assert!(!one.text().contains("progress"));
    finish(&fixture, "two");
    two.expect_exit();
}

#[test]
fn install_hooks_preserves_settings_and_existing_hooks_and_is_idempotent() {
    let fixture = Fixture::new();
    let settings = fixture.root.join("settings.json");
    let existing = json!({"theme":"dark", "permissions":{"allow":["Read"]}, "hooks":{
        "PreToolUse":[{"matcher":"Bash", "hooks":[{"type":"command", "command":"keep-this"}]}],
        "SubagentStart":[{"matcher":"Explore", "hooks":[{"type":"command", "command":"keep-start"}]}],
        "SubagentStop":[{"hooks":[{"type":"command", "command":"keep-stop"}]}]
    }});
    let original = serde_json::to_vec_pretty(&existing).unwrap();
    fs::write(&settings, &original).unwrap();
    let output = fixture
        .command()
        .args(["install-hooks", "--settings"])
        .arg(&settings)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let installed = fs::read(&settings).unwrap();
    let merged: Value = serde_json::from_slice(&installed).unwrap();
    assert_eq!(merged["theme"], existing["theme"]);
    assert_eq!(merged["permissions"], existing["permissions"]);
    assert_eq!(
        merged["hooks"]["PreToolUse"],
        existing["hooks"]["PreToolUse"]
    );
    for (event, action) in [
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
    ] {
        assert_eq!(merged["hooks"][event][0], existing["hooks"][event][0]);
        assert_eq!(merged["hooks"][event][1]["matcher"], "");
        let hook = &merged["hooks"][event][1]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert_eq!(hook["timeout"], 2);
        let command = hook["command"].as_str().unwrap();
        assert!(command.starts_with('\''));
        assert!(command.contains(env!("CARGO_BIN_EXE_herdr-crew")));
        assert!(command.ends_with(&format!(" hook {action}")));
    }
    let backups = || {
        fs::read_dir(&fixture.root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("settings.json.bak.")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(backups().len(), 1);
    assert_eq!(fs::read(&backups()[0]).unwrap(), original);
    assert_silent_success(
        &fixture
            .command()
            .args(["install-hooks", "--settings"])
            .arg(&settings)
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read(&settings).unwrap(), installed);
    assert_eq!(backups().len(), 1);
}

#[test]
fn install_hooks_creates_new_settings_and_rejects_invalid_settings_without_changes() {
    let fixture = Fixture::new();
    let settings = fixture.root.join("nested/settings.json");
    assert!(fixture
        .command()
        .args(["install-hooks", "--settings"])
        .arg(&settings)
        .output()
        .unwrap()
        .status
        .success());
    let installed: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(
        installed["hooks"]["SubagentStart"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for original in [
        "{broken",
        "[]",
        r#"{"hooks":[]}"#,
        r#"{"hooks":{"SubagentStop":{}}}"#,
    ] {
        fs::write(&settings, original).unwrap();
        assert!(!fixture
            .command()
            .args(["install-hooks", "--settings"])
            .arg(&settings)
            .output()
            .unwrap()
            .status
            .success());
        assert_eq!(fs::read_to_string(&settings).unwrap(), original);
    }
    assert_eq!(fs::read_dir(settings.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn a_successful_open_keeps_its_claim_when_the_reply_is_malformed() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command.env("HERDR_TEST_BAD_REPLY", "1");
    assert_silent_success(&invoke_hook(
        command,
        "subagent-start",
        &fixture.input("child", "SubagentStart").to_string(),
    ));
    let opens = |fixture: &Fixture| {
        fixture
            .calls()
            .iter()
            .filter(|args| args[0] == "plugin")
            .count()
    };
    wait_until(|| opens(&fixture) == 1);
    assert_silent_success(
        &fixture.hook("subagent-start", &fixture.input("child", "SubagentStart")),
    );
    thread::sleep(Duration::from_millis(150));
    assert_eq!(opens(&fixture), 1);
    assert!(fixture.child_dir("child").join("open.lock").exists());
}

#[test]
fn large_transcript_lines_are_complete_and_bounded_when_rendered() {
    let fixture = Fixture::new();
    fixture.prepare_child("child");
    append(
        &fixture.transcript("child"),
        line("child", "user", json!("Large line brief")).as_bytes(),
    );
    let mut view = fixture.view("child");
    view.expect("Large line brief");
    let large = line(
        "child",
        "assistant",
        json!([
            {"type":"thinking", "thinking":format!("Large thought {}", "猫".repeat(200_000))},
            {"type":"tool_use", "name":"CustomTool", "input":{"count":3}},
            {"type":"text", "text":"After the large line"}
        ]),
    );
    append(&fixture.transcript("child"), large.as_bytes());
    view.expect("After the large line");
    let rendered = view.text();
    assert!(rendered.contains("Large thought"));
    assert!(rendered.contains('…'));
    assert!(rendered.contains("CustomTool: {\"count\":3}"));
    assert!(rendered.len() < 5000);
    finish(&fixture, "child");
    view.expect_exit();
}

#[test]
fn stop_closes_a_view_even_if_the_transcript_never_appears() {
    let fixture = Fixture::new();
    fixture.prepare_child("child");
    let mut view = fixture.view("child");
    let mut stop = fixture.input("child", "SubagentStop");
    stop["agent_transcript_path"] = json!("");
    stop["last_assistant_message"] = json!("The report from the stop hook");
    stop["stop_hook_active"] = json!(false);
    assert_silent_success(&fixture.hook("subagent-stop", &stop));
    view.expect_exit();
    assert!(view.text().contains("status: completed"));
    assert!(view.text().contains("The report from the stop hook"));
}

#[test]
fn state_uses_xdg_then_home_and_creates_private_records() {
    let fixture = Fixture::new();
    for use_xdg in [true, false] {
        let agent = if use_xdg { "xdg-child" } else { "home-child" };
        let mut command = fixture.command();
        command.env_remove("HERDR_CREW_STATE_DIR");
        let base = if use_xdg {
            let xdg = fixture.root.join("xdg");
            command.env("XDG_STATE_HOME", &xdg);
            xdg
        } else {
            fixture.root.join(".local/state")
        };
        assert_silent_success(&invoke_hook(
            command,
            "subagent-start",
            &fixture.input(agent, "SubagentStart").to_string(),
        ));
        let directory = base.join("herdr-crew/session-1").join(agent);
        wait_until(|| directory.join("child.json").exists());
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.join("child.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    wait_until(|| {
        fixture
            .calls()
            .iter()
            .filter(|args| args[0] == "tab")
            .count()
            == 2
    });
}

#[test]
fn view_renames_the_tab_once_model_and_effort_are_known() {
    let fixture = Fixture::new();
    let transcript = fixture.transcript("child");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(
        transcript.with_extension("meta.json"),
        json!({"agentType":"general-purpose", "description":"Research: hydraulic press brief"})
            .to_string(),
    )
    .unwrap();
    start(&fixture, "child");
    assert_eq!(renames(&fixture), ["sub: Research: hydraulic press"]);
    let view = fixture.view("child");
    append(
        &transcript,
        line("child", "user", json!("Synthetic brief")).as_bytes(),
    );
    view.expect("Synthetic brief");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(renames(&fixture).len(), 1);
    append(
        &transcript,
        assistant(
            "child",
            json!({"effort":"high", "perTurnEffort":"low", "requestedModel":"claude-sonnet-5-5",
                "message":{"model":"claude-opus-5-5[1m]", "content":[{"type":"text", "text":"A step"}]}}),
        )
        .as_bytes(),
    );
    wait_until(|| renames(&fixture).len() == 2);
    assert!(renames(&fixture).contains(&"opus-5.5 high: Research: hydraulic press".to_owned()));
    // Later steps with the same details must not rename again.
    append(
        &transcript,
        assistant("child", json!({"effort":"high"})).as_bytes(),
    );
    view.expect("A step");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(renames(&fixture).len(), 2);
}

#[test]
fn details_missing_at_first_are_filled_in_when_they_appear() {
    let fixture = Fixture::new();
    let transcript = fixture.transcript("child");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    start(&fixture, "child");
    assert_eq!(renames(&fixture), ["sub: general-purpose"]);
    let mut view = fixture.view("child");
    append(
        &transcript,
        line("child", "user", json!("Synthetic brief")).as_bytes(),
    );
    view.expect("Synthetic brief");
    // Only the requested model is known, and no effort.
    append(
        &transcript,
        assistant("child", json!({"requestedModel":"claude-haiku-5-5"})).as_bytes(),
    );
    wait_until(|| renames(&fixture).contains(&"haiku-5.5: general-purpose".to_owned()));
    // The description arrives after the child started.
    fs::write(
        transcript.with_extension("meta.json"),
        json!({"description":"find ledger query now please"}).to_string(),
    )
    .unwrap();
    wait_until(|| renames(&fixture).contains(&"haiku-5.5: find ledger query".to_owned()));
    // The effort falls back to the per-turn one when it shows up.
    append(
        &transcript,
        assistant("child", json!({"perTurnEffort":"medium"})).as_bytes(),
    );
    wait_until(|| renames(&fixture).contains(&"haiku-5.5 medium: find ledger query".to_owned()));
    thread::sleep(Duration::from_millis(150));
    assert_eq!(renames(&fixture).len(), 4);
    finish(&fixture, "child");
    view.expect_exit();
}

#[test]
fn a_view_without_a_recorded_tab_never_renames() {
    let fixture = Fixture::new();
    fixture.prepare_child("child");
    let mut view = fixture.view("child");
    append(
        &fixture.transcript("child"),
        assistant(
            "child",
            json!({"effort":"high", "requestedModel":"claude-opus-5-5"}),
        )
        .as_bytes(),
    );
    thread::sleep(Duration::from_millis(150));
    assert!(renames(&fixture).is_empty());
    finish(&fixture, "child");
    view.expect_exit();
}

#[test]
fn completed_child_shows_the_stop_hook_report_after_the_steps() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "child");
    append(
        &fixture.transcript("child"),
        line(
            "child",
            "assistant",
            json!([{"type":"text", "text":"Conversation text"}]),
        )
        .as_bytes(),
    );
    let mut view = fixture.view("child");
    view.expect("Conversation text");
    let long = format!("Final report\n{}\x1b[31mtail", "x".repeat(10_000));
    stop_with(&fixture, "child", Some(&long));
    view.expect_exit();
    let text = view.text();
    assert!(text.rfind("Conversation text").unwrap() < text.find("status: completed").unwrap());
    assert!(text.find("status: completed").unwrap() < text.find("Final report").unwrap());
    // The stop hook's message wins over the conversation, long reports are cut, escapes are not passed on.
    assert!(!text.contains("tail") && !text.contains('\x1b') && text.contains("…"));
}

#[test]
fn completed_child_falls_back_to_the_conversation_for_its_report() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "child");
    append(
        &fixture.transcript("child"),
        line(
            "child",
            "assistant",
            json!([
                {"type":"tool_use", "name":"Bash", "input":{"command":"ls"}}
            ]),
        )
        .as_bytes(),
    );
    append(
        &fixture.transcript("child"),
        line("child", "assistant", json!([
            {"type":"thinking", "thinking":"Done"}, {"type":"text", "text":"Report one"}, {"type":"text", "text":"Report two"}
        ])).as_bytes(),
    );
    let mut view = fixture.view("child");
    view.expect("Report two");
    stop_with(&fixture, "child", None);
    view.expect_exit();
    let text = view.text();
    assert!(text.contains("status: completed"));
    assert!(text.ends_with("Report one\nReport two\n"), "{text}");
}

#[test]
fn child_that_ends_on_an_api_error_shows_failed_with_the_error() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "child");
    let mut error = serde_json::from_str::<Value>(&line(
        "child",
        "assistant",
        json!([{"type":"text", "text":"API Error: 529 overloaded"}]),
    ))
    .unwrap();
    error["isApiErrorMessage"] = json!(true);
    append(
        &fixture.transcript("child"),
        format!("{error}\n").as_bytes(),
    );
    let mut view = fixture.view("child");
    view.expect("API Error: 529 overloaded");
    stop_with(&fixture, "child", Some("API Error: 529 overloaded"));
    view.expect_exit();
    let text = view.text();
    assert!(text.contains("status: failed"));
    assert!(!text.contains("status: completed"));
}

#[test]
fn child_stopped_without_a_result_is_unknown_and_an_interrupt_is_killed() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "empty");
    let mut view = fixture.view("empty");
    view.expect("A brief");
    stop_with(&fixture, "empty", None);
    view.expect_exit();
    assert!(view.text().contains("status: unknown"));

    start_conversation(&fixture, "stopped");
    append(
        &fixture.transcript("stopped"),
        line(
            "stopped",
            "user",
            json!([{"type":"text", "text":"[Request interrupted by user]"}]),
        )
        .as_bytes(),
    );
    let mut view = fixture.view("stopped");
    view.expect("A brief");
    stop_with(&fixture, "stopped", Some("partial"));
    view.expect_exit();
    assert!(view.text().contains("status: killed"));
}

#[test]
fn view_ends_as_killed_when_the_orchestrator_is_gone_without_a_stop_record() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "child");
    let mut orchestrator = Command::new("sleep").arg("30").spawn().unwrap();
    let started = start_time(orchestrator.id());
    fs::write(
        fixture.child_dir("child").join("child.json"),
        json!({
            "agent_id": "child", "agent_type": "general-purpose", "transcript": fixture.transcript("child"),
            "orchestrator": {"pid": orchestrator.id(), "started": started}
        })
        .to_string(),
    )
    .unwrap();
    let mut view = fixture.view("child");
    view.expect("A brief");
    thread::sleep(Duration::from_millis(1800));
    assert!(view.child.try_wait().unwrap().is_none());
    orchestrator.kill().unwrap();
    orchestrator.wait().unwrap();
    view.expect_exit();
    assert!(view.text().contains("status: killed"));
    assert!(!fixture.child_dir("child").join("stop.json").exists());
}

#[test]
fn finished_metadata_without_a_stop_record_ends_the_view_as_unknown() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "child");
    let mut view = fixture.view("child");
    view.expect("A brief");
    fs::write(
        fixture.transcript("child").with_extension("meta.json"),
        r#"{"finished":true}"#,
    )
    .unwrap();
    view.expect_exit();
    assert!(view.text().contains("status: unknown"));
}

#[test]
fn a_late_stop_record_beats_the_finished_metadata() {
    let fixture = Fixture::new();
    start_conversation(&fixture, "child");
    let mut view = fixture.view("child");
    view.expect("A brief");
    fs::write(
        fixture.transcript("child").with_extension("meta.json"),
        r#"{"finished":true}"#,
    )
    .unwrap();
    thread::sleep(Duration::from_millis(300));
    stop_with(&fixture, "child", Some("Late report"));
    view.expect_exit();
    assert!(view.text().contains("status: completed"));
}

#[test]
fn start_hook_records_the_orchestrator_and_the_stop_hook_keeps_the_report() {
    let fixture = Fixture::new();
    assert_silent_success(
        &fixture.hook("subagent-start", &fixture.input("child", "SubagentStart")),
    );
    wait_until(|| fixture.child_dir("child").join("child.json").exists());
    let child: Value =
        serde_json::from_slice(&fs::read(fixture.child_dir("child").join("child.json")).unwrap())
            .unwrap();
    let pid = std::process::id();
    assert_eq!(child["orchestrator"]["pid"], pid);
    assert_eq!(child["orchestrator"]["started"], start_time(pid));
    let mut stop = fixture.input("child", "SubagentStop");
    stop["last_assistant_message"] = json!("é".repeat(100_000));
    assert_silent_success(&fixture.hook("subagent-stop", &stop));
    wait_until(|| fixture.child_dir("child").join("stop.json").exists());
    let stop: Value =
        serde_json::from_slice(&fs::read(fixture.child_dir("child").join("stop.json")).unwrap())
            .unwrap();
    assert_eq!(stop["message"].as_str().unwrap().chars().count(), 16_000);
}
