use crate::{
    naming, state,
    view::{Ending, Step, StepKind, StepSource, Update},
};
use serde_json::Value;
use std::{
    fs::File,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
};

pub struct ClaudeTranscript {
    path: PathBuf,
    agent_id: String,
    reader: Option<BufReader<File>>,
    pending: Vec<u8>,
    brief_seen: bool,
    model: Option<String>,
    effort: Option<String>,
    description: Option<String>,
    ending: Option<Ending>,
}

impl ClaudeTranscript {
    pub fn new(path: PathBuf, agent_id: String) -> Self {
        Self {
            path,
            agent_id,
            reader: None,
            pending: Vec::new(),
            brief_seen: false,
            model: None,
            effort: None,
            description: None,
            ending: None,
        }
    }

    pub fn use_transcript(&mut self, path: &Path) {
        // A stop hook can locate a transcript that was absent at the derived path.
        if self.reader.is_none() && path != self.path {
            self.path = path.to_owned();
        }
    }

    pub fn finished(&self) -> bool {
        state::read_json::<Value>(&self.path.with_extension("meta.json"))
            .ok()
            .and_then(|meta| meta.get("finished").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// The orchestrator may write the description just after the child starts.
    pub fn load_description(&mut self) {
        if self.description.is_none() {
            self.description = naming::read_description(&self.path);
        }
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    fn note_details(&mut self, line: &Value) {
        let first = |keys: [&Value; 2]| {
            keys.into_iter()
                .filter_map(Value::as_str)
                .find(|text| !text.trim().is_empty())
                .map(str::to_owned)
        };
        if self.model.is_none() {
            self.model = first([&line["message"]["model"], &line["requestedModel"]]);
        }
        if self.effort.is_none() {
            self.effort = first([&line["effort"], &line["perTurnEffort"]]);
        }
    }

    fn open(&mut self) -> io::Result<()> {
        if self.reader.is_none() {
            match File::open(&self.path) {
                Ok(file) => self.reader = Some(BufReader::new(file)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn updates(&mut self, line: &Value) -> Vec<Update> {
        if line.get("isSidechain").and_then(Value::as_bool) != Some(true)
            || line.get("agentId").and_then(Value::as_str) != Some(&self.agent_id)
        {
            return Vec::new();
        }
        if line["type"] == "assistant" {
            self.note_details(line);
        }
        let content = &line["message"]["content"];
        match line["type"].as_str() {
            Some("user") if !self.brief_seen => {
                self.brief_seen = true;
                vec![Update::Brief(text_content(content))]
            }
            Some("assistant") if self.brief_seen => {
                self.ending = assistant_ending(line, content);
                assistant_steps(line, content)
            }
            Some("user") => {
                self.ending = user_ending(content);
                tool_results(line, content)
            }
            _ => Vec::new(),
        }
    }
}

impl StepSource for ClaudeTranscript {
    fn ending(&self) -> Option<Ending> {
        self.ending.clone()
    }

    fn poll(&mut self) -> io::Result<Vec<Update>> {
        self.open()?;
        let mut updates = Vec::new();
        while let Some(reader) = self.reader.as_mut() {
            if reader.read_until(b'\n', &mut self.pending)? == 0 {
                break;
            }
            // EOF is temporary: a later append may complete this JSON or UTF-8 code point.
            if self.pending.last() != Some(&b'\n') {
                break;
            }
            if let Ok(line) = serde_json::from_slice::<Value>(&self.pending) {
                updates.extend(self.updates(&line));
            }
            self.pending.clear();
        }
        Ok(updates)
    }
}

// Best effort: Claude Code marks a failed model call as an assistant line, not as a separate event.
fn assistant_ending(line: &Value, content: &Value) -> Option<Ending> {
    let text = text_content(content);
    if line["isApiErrorMessage"] == true || !line["error"].is_null() {
        let reason = line["error"].as_str().unwrap_or("the model call failed");
        let text = if text.trim().is_empty() {
            reason
        } else {
            &text
        };
        return Some(Ending::Error(text.to_owned()));
    }
    // A line that goes on to use a tool is mid-work, not a final report.
    let uses_tool = content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"));
    (!uses_tool && !text.trim().is_empty()).then_some(Ending::Report(text))
}

fn user_ending(content: &Value) -> Option<Ending> {
    text_content(content)
        .starts_with("[Request interrupted by user")
        .then_some(Ending::Interrupted)
}

fn text_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn step(line: &Value, kind: StepKind, text: String) -> Update {
    Update::Step(Step {
        timestamp: line["timestamp"].as_str().unwrap_or("").to_owned(),
        kind,
        text,
    })
}

fn assistant_steps(line: &Value, content: &Value) -> Vec<Update> {
    if let Some(text) = content.as_str() {
        return vec![step(line, StepKind::Message, text.to_owned())];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let (kind, text) = match block["type"].as_str()? {
                "thinking" => (StepKind::Thought, block["thinking"].as_str()?.to_owned()),
                "text" => (StepKind::Message, block["text"].as_str()?.to_owned()),
                "tool_use" => tool_step(block),
                _ => return None,
            };
            Some(step(line, kind, text))
        })
        .collect()
}

fn tool_step(block: &Value) -> (StepKind, String) {
    let name = block["name"].as_str().unwrap_or("tool");
    let input = &block["input"];
    let summary = [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "description",
    ]
    .iter()
    .find_map(|key| input[*key].as_str())
    .map(str::to_owned)
    .unwrap_or_else(|| input.to_string());
    let kind = match name {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => StepKind::Edit,
        _ => StepKind::Tool,
    };
    (kind, format!("{name}: {summary}"))
}

fn tool_results(line: &Value, content: &Value) -> Vec<Update> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            let label = if block["is_error"] == true {
                "error"
            } else {
                "output"
            };
            step(
                line,
                StepKind::ToolResult,
                format!("{label}: {}", text_content(&block["content"])),
            )
        })
        .collect()
}
