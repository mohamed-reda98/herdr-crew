use crate::{
    naming,
    run::{self, observed_at, text, RunFiles},
    state::DelegateRun,
    tail::Tail,
    transcript::{assistant_steps, tool_results},
    view::{Delegated, Ending, Outcome, Status, Step, StepKind, StepSource, Update},
};
use serde_json::Value;
use std::io;

/// A Claude delegate seen through the files its relay writes into the run directory:
/// `events.jsonl` is the stream `claude -p --output-format stream-json` prints.
pub struct ClaudeRun {
    files: RunFiles,
    tail: Tail,
    brief_sent: bool,
    brief: Option<String>,
    last_error: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    title: Option<String>,
    // What the run itself reported, which the caller's arguments do not always name.
    initial_model: Option<String>,
}

impl ClaudeRun {
    pub fn new(run: DelegateRun) -> Self {
        Self {
            tail: Tail::new(run.out_dir.join("events.jsonl")),
            files: RunFiles::new(run.out_dir, run.started_ms, run.relay),
            brief_sent: false,
            brief: None,
            last_error: None,
            model: run.model,
            effort: run.effort,
            title: run.title,
            initial_model: None,
        }
    }

    fn updates(&mut self, event: &Value) -> Vec<Update> {
        // Lines of a subagent the delegate started belong to a child of their own.
        if !event["parent_tool_use_id"].is_null() {
            return Vec::new();
        }
        let content = &event["message"]["content"];
        let step = |kind, text: String| {
            vec![Update::Step(Step {
                timestamp: observed_at(),
                kind,
                text,
            })]
        };
        match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("system"), Some("init")) => {
                if self.initial_model.is_none() {
                    self.initial_model = text(&event["model"]).map(str::to_owned);
                }
                step(
                    StepKind::Note,
                    format!("session {}", event["session_id"].as_str().unwrap_or("")),
                )
            }
            // The stream has no times, and a thinking block may arrive with its text withheld.
            (Some("assistant"), _) => stamped(assistant_steps(event, content)),
            (Some("user"), _) => stamped(tool_results(event, content)),
            // The success result is the relay's `finalMessage`, which the view ends with.
            (Some("result"), _) if event["is_error"] == true => {
                let reason = text(&event["result"])
                    .map(str::to_owned)
                    .or_else(|| event["subtype"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "claude returned an error result".to_owned());
                self.last_error = Some(reason.clone());
                step(StepKind::Error, reason)
            }
            _ => Vec::new(),
        }
    }
}

fn stamped(updates: Vec<Update>) -> Vec<Update> {
    updates
        .into_iter()
        .filter_map(|update| match update {
            Update::Step(step) if step.text.trim().is_empty() => None,
            Update::Step(step) => Some(Update::Step(Step {
                timestamp: observed_at(),
                ..step
            })),
            brief => Some(brief),
        })
        .collect()
}

impl Delegated for ClaudeRun {
    fn relay_gone(&self) -> bool {
        self.files.relay_gone()
    }

    fn result(&mut self) -> Option<Value> {
        self.files.result()
    }

    fn tab_name(&self) -> String {
        let from_result = |key: &str| {
            self.files
                .last_result()
                .and_then(|result| text(&result[key]))
                .map(str::to_owned)
        };
        let model = self
            .model
            .clone()
            .or_else(|| self.initial_model.clone())
            .or_else(|| from_result("model"));
        let effort = self.effort.clone().or_else(|| from_result("effort"));
        naming::delegate_name(
            "claude",
            model.as_deref(),
            effort.as_deref(),
            self.title.as_deref(),
            self.brief.as_deref(),
        )
    }

    fn outcome(&self, result: &Value) -> Outcome {
        let mut outcome = run::outcome(result, self.last_error.as_deref());
        // The relay's own error only says that Claude's result was an error; what Claude said
        // about it is the part worth reading.
        if outcome.status == Status::Failed {
            if let (Some(error), Some(reported)) =
                (text(&result["error"]), self.last_error.as_deref())
            {
                if !error.contains(reported) {
                    outcome.text = format!("{error}: {reported}");
                }
            }
        }
        outcome
    }

    fn outcome_without_result(&self) -> Outcome {
        run::outcome_without_result(self.last_error.as_deref())
    }
}

impl StepSource for ClaudeRun {
    fn poll(&mut self) -> io::Result<Vec<Update>> {
        let mut updates = Vec::new();
        if !self.brief_sent {
            // The brief heads the view, so no step is shown before it.
            let Some(brief) = self.files.read_brief() else {
                return Ok(updates);
            };
            self.brief_sent = true;
            updates.push(Update::Brief(brief.clone()));
            self.brief = Some(brief);
        }
        for raw in self.tail.lines()? {
            if let Ok(event) = serde_json::from_slice::<Value>(&raw) {
                updates.extend(self.updates(&event));
            }
        }
        Ok(updates)
    }

    fn ending(&self) -> Option<Ending> {
        self.last_error.clone().map(Ending::Error)
    }
}
