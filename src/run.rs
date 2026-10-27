use crate::{
    process::{self, Process},
    state,
    view::{Outcome, Status},
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

// The relay may stamp its files a little before the wrapper noted the start, on a coarse clock.
const CLOCK_SLACK_MS: u64 = 1_000;

/// What every delegate's view follows: the relay, and the files it leaves in the output directory.
pub struct RunFiles {
    out_dir: PathBuf,
    started_ms: u64,
    relay: Process,
    result: Option<Value>,
}

impl RunFiles {
    pub fn new(out_dir: PathBuf, started_ms: u64, relay: Process) -> Self {
        Self {
            out_dir,
            started_ms,
            relay,
            result: None,
        }
    }

    pub fn relay_gone(&self) -> bool {
        !process::alive(&self.relay)
    }

    fn fresh(&self, name: &str) -> Option<PathBuf> {
        let path = self.out_dir.join(name);
        written_since(&path, self.started_ms).then_some(path)
    }

    /// The relay's result record once it has written one for this run.
    pub fn result(&mut self) -> Option<Value> {
        if self.result.is_none() {
            let path = self.fresh("result.json")?;
            self.result = state::read_json(&path).ok();
        }
        self.result.clone()
    }

    /// The result as far as it has been read, for naming.
    pub fn last_result(&self) -> Option<&Value> {
        self.result.as_ref()
    }

    pub fn read_brief(&self) -> Option<String> {
        let bytes = fs::read(self.fresh("brief.txt")?).ok()?;
        // The relay creates the file before it fills it.
        if bytes.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Whether the relay wrote the file during this run: an output directory the caller reuses still
/// holds the previous run's files.
pub fn written_since(path: &Path, started_ms: u64) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .is_some_and(|modified| modified.as_millis() as u64 + CLOCK_SLACK_MS >= started_ms)
}

pub fn text(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.trim().is_empty())
}

/// How a relay's result record ends the run. `last_error` is what the agent itself reported, kept
/// for a failure whose record names no error.
pub fn outcome(result: &Value, last_error: Option<&str>) -> Outcome {
    let field = |key: &str| text(&result[key]).map(str::to_owned);
    let status = result["status"].as_str();
    let (kind, text) = match status {
        Some("completed") => (
            Status::Completed,
            field("finalMessage").unwrap_or_else(|| "Completed without a final message".into()),
        ),
        Some("aborted") => (
            Status::Killed,
            field("error").unwrap_or_else(|| "Stopped before it finished".into()),
        ),
        Some(status) => {
            let stderr = result["stderrTail"]
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .rev()
                        .take(8)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|lines| !lines.trim().is_empty());
            let text = field("error")
                .or_else(|| last_error.map(str::to_owned))
                .or(stderr)
                .or_else(|| field("finalMessage"))
                .unwrap_or_else(|| match result["exitCode"].as_i64() {
                    Some(code) => format!("{status} (exit {code})"),
                    None => status.to_owned(),
                });
            (Status::Failed, text)
        }
        None => (
            Status::Unknown,
            "The relay's result has no status".to_owned(),
        ),
    };
    Outcome { status: kind, text }
}

/// A relay that vanished without a result record.
pub fn outcome_without_result(last_error: Option<&str>) -> Outcome {
    match last_error {
        Some(error) => Outcome {
            status: Status::Failed,
            text: error.to_owned(),
        },
        None => Outcome {
            status: Status::Killed,
            text: "The relay ended without writing a result".into(),
        },
    }
}

// The agents' events carry no times; a step is stamped when the view sees it, which is when it
// happened for a view that follows live.
pub fn observed_at() -> String {
    let seconds = state::epoch_ms() / 1000 % 86_400;
    format!(
        "0000-00-00T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}
