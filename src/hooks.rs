use crate::{process::Process, render, state, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Deserialize, Serialize)]
struct Input {
    session_id: String,
    transcript_path: PathBuf,
    hook_event_name: String,
    agent_id: String,
    agent_type: String,
    #[serde(default)]
    agent_transcript_path: Option<PathBuf>,
    #[serde(default, deserialize_with = "report_text")]
    last_assistant_message: Option<String>,
    #[serde(default)]
    orchestrator: Option<Process>,
}

// The payload travels as one argument, which the kernel caps; a stop must not be lost to a long report.
const REPORT_LIMIT: usize = 16_000;

fn report_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .map(|text| text.chars().take(REPORT_LIMIT).collect()))
}

impl Input {
    fn validate(&self, action: &str) -> Result<()> {
        let expected = match action {
            "subagent-start" => "SubagentStart",
            "subagent-stop" => "SubagentStop",
            _ => return Err("unknown hook".into()),
        };
        if self.hook_event_name != expected
            || !safe_id(&self.session_id)
            || !safe_id(&self.agent_id)
            || !self.transcript_path.is_absolute()
            || self.transcript_path.parent().and_then(Path::file_name)
                == Some(std::ffi::OsStr::new("subagents"))
        {
            return Err("invalid or nested subagent hook".into());
        }
        Ok(())
    }

    fn child_dir(&self) -> Result<PathBuf> {
        Ok(state::root()?.join(&self.session_id).join(&self.agent_id))
    }

    fn transcript(&self) -> PathBuf {
        self.transcript_path
            .with_extension("")
            .join("subagents")
            .join(format!("agent-{}.jsonl", self.agent_id))
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
}

pub fn receive(args: &[String]) -> Result<()> {
    let [action] = args else {
        return Ok(());
    };
    let mut bytes = Vec::new();
    io::stdin().take(16 * 1024 * 1024).read_to_end(&mut bytes)?;
    let mut input: Input = serde_json::from_slice(&bytes)?;
    input.validate(action)?;
    input.orchestrator = crate::process::orchestrator();
    spawn_dispatch(action, &serde_json::to_string(&input)?)
}

fn spawn_dispatch(action: &str, payload: &str) -> Result<()> {
    let mut command = Command::new(env::current_exe()?);
    command
        .args(["dispatch", action, payload])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A new session survives the hook's exit and the orchestrator's process group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()?;
    Ok(())
}

pub fn dispatch(action: &str, payload: &str) -> Result<()> {
    let input: Input = serde_json::from_str(payload)?;
    input.validate(action)?;
    let child_dir = input.child_dir()?;
    state::create_dir(&child_dir)?;
    match action {
        "subagent-start" => open_view(&input, &child_dir),
        "subagent-stop" => state::write_json(
            &child_dir.join("stop.json"),
            &state::Stop {
                stopped_at_ms: state::epoch_ms(),
                transcript: input
                    .agent_transcript_path
                    .filter(|path| path.is_absolute()),
                message: input.last_assistant_message,
            },
        ),
        _ => unreachable!(),
    }
}

fn open_view(input: &Input, child_dir: &Path) -> Result<()> {
    let lock = child_dir.join("open.lock");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
    {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let opened = (|| {
        state::write_json(
            &child_dir.join("child.json"),
            &state::Child {
                agent_id: input.agent_id.clone(),
                agent_type: input.agent_type.clone(),
                transcript: input.transcript(),
                orchestrator: input.orchestrator,
            },
        )?;
        open_tab(input, child_dir)
    })();
    if opened.is_err() {
        let _ = fs::remove_file(lock);
    }
    opened
}

fn herdr() -> Command {
    let mut command = Command::new(env::var_os("HERDR_BIN").unwrap_or_else(|| "herdr".into()));
    command.stdin(Stdio::null()).stderr(Stdio::null());
    command
}

fn workspace() -> Result<String> {
    if let Ok(workspace) = env::var("HERDR_WORKSPACE_ID") {
        if !workspace.is_empty() {
            return Ok(workspace);
        }
    }
    let pane = env::var("HERDR_PANE_ID")?;
    Ok(pane
        .split(':')
        .next()
        .ok_or("missing workspace")?
        .to_owned())
}

fn open_tab(input: &Input, child_dir: &Path) -> Result<()> {
    let mut command = herdr();
    command.args([
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
        &workspace()?,
        "--no-focus",
        "--env",
    ]);
    let mut child_env = std::ffi::OsString::from("HERDR_CREW_CHILD_DIR=");
    child_env.push(child_dir);
    command.arg(child_env);
    // The Herdr server may have a different environment from the orchestrator.
    if let Some(linger) = env::var_os("HERDR_CREW_LINGER_MS") {
        let mut assignment = std::ffi::OsString::from("HERDR_CREW_LINGER_MS=");
        assignment.push(linger);
        command.arg("--env").arg(assignment);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err("Herdr could not open the view".into());
    }
    // A successful open owns its tab even if the reply or rename fails.
    if let Ok(reply) = serde_json::from_slice::<Value>(&output.stdout) {
        if let Some(tab_id) = reply
            .pointer("/result/plugin_pane/pane/tab_id")
            .and_then(Value::as_str)
        {
            let _ = herdr()
                .args(["tab", "rename", tab_id, &tab_name(&input.agent_type)])
                .stdout(Stdio::null())
                .status();
        }
    }
    Ok(())
}

fn tab_name(agent_type: &str) -> String {
    format!("⇢ sub: {}", render::one_line(agent_type, 80))
}
