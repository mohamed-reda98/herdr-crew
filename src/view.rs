use crate::{
    claude::ClaudeRun, codex::CodexRun, naming::Namer, process, render::Renderer, state,
    transcript::ClaudeTranscript, Result,
};
use serde_json::Value;
use std::{
    env, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

pub enum Update {
    Brief(String),
    Step(Step),
}

pub struct Step {
    pub timestamp: String,
    pub kind: StepKind,
    pub text: String,
}

pub enum StepKind {
    Thought,
    Tool,
    Edit,
    Message,
    ToolResult,
    Note,
    Error,
}

/// How a source's conversation ended, as far as it can tell.
#[derive(Clone)]
pub enum Ending {
    Report(String),
    Error(String),
    Interrupted,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    Completed,
    Failed,
    Killed,
    Unknown,
}

pub struct Outcome {
    pub status: Status,
    pub text: String,
}

/// What told the view the child is over.
#[derive(Clone, Copy)]
enum Cause {
    Stopped,
    Finished,
    OrchestratorGone,
}

// A missed stop hook is only believed after the one in flight has had time to land.
const STOP_GRACE: Duration = Duration::from_millis(1500);

pub trait StepSource {
    fn poll(&mut self) -> io::Result<Vec<Update>>;
    fn ending(&self) -> Option<Ending>;
}

/// A delegate's run, which its relay's result record or exit ends.
pub trait Delegated: StepSource {
    fn relay_gone(&self) -> bool;
    fn result(&mut self) -> Option<Value>;
    fn tab_name(&self) -> String;
    fn outcome(&self, result: &Value) -> Outcome;
    fn outcome_without_result(&self) -> Outcome;
}

fn outcome(cause: Cause, message: Option<&str>, ending: Option<Ending>) -> Outcome {
    let message = message.filter(|text| !text.trim().is_empty());
    let (status, text) = match (cause, ending) {
        (_, Some(Ending::Error(error))) => (Status::Failed, error),
        (_, Some(Ending::Interrupted)) => (Status::Killed, "Interrupted by the user".into()),
        (Cause::Stopped, ending) => match (message, ending) {
            (Some(message), _) => (Status::Completed, message.to_owned()),
            (None, Some(Ending::Report(report))) => (Status::Completed, report),
            _ => (Status::Unknown, "Ended without leaving a result".into()),
        },
        (Cause::Finished, Some(Ending::Report(report))) => (Status::Unknown, report),
        (Cause::Finished, _) => (Status::Unknown, "Ended without a stop record".into()),
        (Cause::OrchestratorGone, _) => (
            Status::Killed,
            "The orchestrator ended before the child reported".into(),
        ),
    };
    Outcome { status, text }
}

pub fn run() -> Result<()> {
    let child_dir =
        PathBuf::from(env::var_os("HERDR_CREW_CHILD_DIR").ok_or("missing child directory")?);
    let child: state::Child = state::read_json(&child_dir.join("child.json"))?;
    let linger = Duration::from_millis(
        env::var("HERDR_CREW_LINGER_MS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(10_000),
    );
    let renderer = Renderer::new(io::stdout());
    match (child.kind, child.delegate) {
        (state::Kind::Codex, Some(run)) => {
            follow_delegate(&child_dir, CodexRun::new(run), renderer, linger)
        }
        (state::Kind::Claude, Some(run)) => {
            follow_delegate(&child_dir, ClaudeRun::new(run), renderer, linger)
        }
        (state::Kind::Codex | state::Kind::Claude, None) => {
            Err("the delegate's run record is missing".into())
        }
        (state::Kind::Subagent, _) => follow_subagent(
            &child_dir,
            ClaudeTranscript::new(child.transcript, child.agent_id),
            &child.agent_type,
            child.orchestrator,
            renderer,
            linger,
        ),
    }
}

/// Follows a delegate until its run is over, which its relay's result record or exit tells.
fn follow_delegate(
    child_dir: &Path,
    mut source: impl Delegated,
    mut renderer: Renderer<io::Stdout>,
    linger: Duration,
) -> Result<()> {
    let mut namer = Namer::new(child_dir);
    loop {
        // The relay writes its result before it exits, so a relay seen gone has left it behind;
        // observing both before draining keeps the last steps in.
        let gone = source.relay_gone();
        let result = source.result();
        for update in source.poll()? {
            renderer.write(update)?;
        }
        namer.refresh(|| source.tab_name());
        if let Some(outcome) = result
            .map(|result| source.outcome(&result))
            .or_else(|| gone.then(|| source.outcome_without_result()))
        {
            renderer.result(&outcome)?;
            thread::sleep(linger);
            return Ok(());
        }
        renderer.flush()?;
        thread::sleep(Duration::from_millis(50));
    }
}

fn follow_subagent(
    child_dir: &Path,
    mut source: ClaudeTranscript,
    agent_type: &str,
    orchestrator: Option<process::Process>,
    mut renderer: Renderer<io::Stdout>,
    linger: Duration,
) -> Result<()> {
    let mut namer = Namer::new(child_dir);
    let mut ended_since: Option<Instant> = None;
    loop {
        let stop = state::read_json::<state::Stop>(&child_dir.join("stop.json")).ok();
        if let Some(transcript) = stop.as_ref().and_then(|stop| stop.transcript.as_ref()) {
            source.use_transcript(transcript);
        }
        // Completion must be observed before draining so its last steps are included.
        let finished = source.finished();
        let gone = orchestrator
            .as_ref()
            .is_some_and(|orchestrator| !process::alive(orchestrator));
        for update in source.poll()? {
            renderer.write(update)?;
        }
        namer.refresh(|| {
            source.load_description();
            crate::naming::tab_name(
                source.model(),
                source.effort(),
                source.description(),
                agent_type,
            )
        });
        if let Some(stop) = stop {
            renderer.result(&outcome(
                Cause::Stopped,
                stop.message.as_deref(),
                source.ending(),
            ))?;
            let elapsed =
                Duration::from_millis(state::epoch_ms().saturating_sub(stop.stopped_at_ms));
            thread::sleep(linger.saturating_sub(elapsed));
            return Ok(());
        }
        if finished || gone {
            let since = *ended_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= STOP_GRACE {
                let cause = if finished {
                    Cause::Finished
                } else {
                    Cause::OrchestratorGone
                };
                renderer.result(&outcome(cause, None, source.ending()))?;
                thread::sleep(linger);
                return Ok(());
            }
        }
        renderer.flush()?;
        thread::sleep(Duration::from_millis(50));
    }
}
