use crate::{
    naming::Namer, process, render::Renderer, state, transcript::ClaudeTranscript, Result,
};
use std::{
    env, io,
    path::PathBuf,
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

// Keeping the source independent lets the view follow other child formats later.
pub trait StepSource {
    fn poll(&mut self) -> io::Result<Vec<Update>>;
    fn ending(&self) -> Option<Ending>;
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
    let mut namer = Namer::new(&child_dir, child.agent_type);
    let mut source = ClaudeTranscript::new(child.transcript, child.agent_id);
    let mut renderer = Renderer::new(io::stdout());
    let linger = Duration::from_millis(
        env::var("HERDR_CREW_LINGER_MS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(10_000),
    );
    let mut ended_since: Option<Instant> = None;
    loop {
        let stop = state::read_json::<state::Stop>(&child_dir.join("stop.json")).ok();
        if let Some(transcript) = stop.as_ref().and_then(|stop| stop.transcript.as_ref()) {
            source.use_transcript(transcript);
        }
        // Completion must be observed before draining so its last steps are included.
        let finished = source.finished();
        let gone = child
            .orchestrator
            .as_ref()
            .is_some_and(|orchestrator| !process::alive(orchestrator));
        for update in source.poll()? {
            renderer.write(update)?;
        }
        namer.refresh(&mut source);
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
