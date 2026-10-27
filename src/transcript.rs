use crate::{
    state,
    view::{Step, StepKind, StepSource, Update},
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
}

impl ClaudeTranscript {
    pub fn new(path: PathBuf, agent_id: String) -> Self {
        Self {
            path,
            agent_id,
            reader: None,
            pending: Vec::new(),
            brief_seen: false,
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
        let content = &line["message"]["content"];
        match line["type"].as_str() {
            Some("user") if !self.brief_seen => {
                self.brief_seen = true;
                vec![Update::Brief(text_content(content))]
            }
            Some("assistant") if self.brief_seen => assistant_steps(line, content),
            Some("user") => tool_results(line, content),
            _ => Vec::new(),
        }
    }
}

impl StepSource for ClaudeTranscript {
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
