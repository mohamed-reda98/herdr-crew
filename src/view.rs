use crate::{naming::Namer, render::Renderer, state, transcript::ClaudeTranscript, Result};
use std::{env, io, path::PathBuf, thread, time::Duration};

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

// Keeping the source independent lets the view follow other child formats later.
pub trait StepSource {
    fn poll(&mut self) -> io::Result<Vec<Update>>;
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
    loop {
        let stop = state::read_json::<state::Stop>(&child_dir.join("stop.json")).ok();
        if let Some(transcript) = stop.as_ref().and_then(|stop| stop.transcript.as_ref()) {
            source.use_transcript(transcript);
        }
        // Completion must be observed before draining so its last steps are included.
        let finished = source.finished();
        for update in source.poll()? {
            renderer.write(update)?;
        }
        renderer.flush()?;
        namer.refresh(&mut source);
        if let Some(stop) = stop {
            let elapsed =
                Duration::from_millis(state::epoch_ms().saturating_sub(stop.stopped_at_ms));
            thread::sleep(linger.saturating_sub(elapsed));
            return Ok(());
        }
        if finished {
            thread::sleep(linger);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
