use crate::{
    naming,
    run::{self, observed_at, text, RunFiles},
    state::DelegateRun,
    tail::Tail,
    view::{Delegated, Ending, Outcome, Step, StepKind, StepSource, Update},
};
use serde_json::Value;
use std::{
    env, fs,
    io::{self},
    path::{Path, PathBuf},
};

const OUTPUT_LINES: usize = 4;

/// A Codex delegate seen through the files its relay writes into the run directory.
pub struct CodexRun {
    files: RunFiles,
    tail: Tail,
    brief_sent: bool,
    brief: Option<String>,
    last_error: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    title: Option<String>,
}

impl CodexRun {
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
        }
    }

    fn updates(&mut self, event: &Value) -> Vec<Update> {
        let item = &event["item"];
        let item_type = item["type"].as_str().unwrap_or("");
        let step = |kind, text: String| {
            vec![Update::Step(Step {
                timestamp: observed_at(),
                kind,
                text,
            })]
        };
        match (event["type"].as_str().unwrap_or(""), item_type) {
            ("thread.started", _) => step(
                StepKind::Note,
                format!("session {}", event["thread_id"].as_str().unwrap_or("")),
            ),
            ("item.started", "command_execution") => step(
                StepKind::Tool,
                format!(
                    "$ {}",
                    short_command(item["command"].as_str().unwrap_or(""))
                ),
            ),
            ("item.completed", "command_execution") => {
                let output = tail_lines(item["aggregated_output"].as_str().unwrap_or(""));
                match item["exit_code"].as_i64().filter(|code| *code != 0) {
                    Some(code) => {
                        let command = short_command(item["command"].as_str().unwrap_or(""));
                        let detail = if output.is_empty() {
                            String::new()
                        } else {
                            format!(": {output}")
                        };
                        step(
                            StepKind::ToolResult,
                            format!("error: {command} (exit {code}){detail}"),
                        )
                    }
                    None if output.is_empty() => Vec::new(),
                    None => step(StepKind::ToolResult, format!("output: {output}")),
                }
            }
            ("item.completed", "file_change") => step(
                StepKind::Edit,
                item["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|change| {
                        let name = Path::new(change["path"].as_str().unwrap_or(""))
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        format!("{} {name}", change["kind"].as_str().unwrap_or("?"))
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            ("item.completed", "agent_message") => step(
                StepKind::Message,
                item["text"].as_str().unwrap_or("").to_owned(),
            ),
            ("item.completed", "reasoning") => {
                let reasoning = text(&item["text"]).map(str::to_owned).unwrap_or_else(|| {
                    item["summary"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                if reasoning.trim().is_empty() {
                    Vec::new()
                } else {
                    step(StepKind::Thought, reasoning)
                }
            }
            ("item.completed", "todo_list" | "plan_update" | "plan") => {
                let plan = [&item["items"], &item["plan"]]
                    .into_iter()
                    .find(|value| !value.is_null())
                    .unwrap_or(item);
                step(StepKind::Note, format!("plan: {plan}"))
            }
            ("item.completed", "mcp_tool_call" | "web_search") => {
                let what = text(&item["tool"])
                    .or_else(|| text(&item["query"]))
                    .unwrap_or("");
                step(StepKind::Tool, format!("{item_type}: {what}"))
            }
            ("error" | "turn.failed", _) => {
                let error = error_text(event);
                self.last_error = Some(error.clone());
                step(StepKind::Error, error)
            }
            ("turn.completed", _) => {
                let count = |key: &str| {
                    event["usage"][key]
                        .as_u64()
                        .map_or_else(|| "?".to_owned(), |count| count.to_string())
                };
                step(
                    StepKind::Note,
                    format!(
                        "turn done — {} in / {} out tokens",
                        count("input_tokens"),
                        count("output_tokens")
                    ),
                )
            }
            _ => Vec::new(),
        }
    }
}

impl Delegated for CodexRun {
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
        let model = from_result("model").or_else(|| self.model.clone());
        let effort = from_result("effort").or_else(|| self.effort.clone());
        naming::delegate_name(
            "codex",
            model.as_deref(),
            effort.as_deref(),
            self.title.as_deref(),
            self.brief.as_deref(),
        )
    }

    fn outcome(&self, result: &Value) -> Outcome {
        run::outcome(result, self.last_error.as_deref())
    }

    fn outcome_without_result(&self) -> Outcome {
        run::outcome_without_result(self.last_error.as_deref())
    }
}

impl StepSource for CodexRun {
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

fn error_text(event: &Value) -> String {
    [
        &event["error"]["message"],
        &event["error"],
        &event["message"],
    ]
    .into_iter()
    .find_map(|value| value.as_str())
    .map_or_else(|| event.to_string(), str::to_owned)
}

/// Codex runs commands as `/bin/zsh -lc '<script>'`; the script is what matters.
fn short_command(command: &str) -> String {
    let command = command.trim();
    let mut script = command;
    let rest = command.strip_prefix("/usr").unwrap_or(command);
    let rest = rest.strip_prefix("/bin/").unwrap_or(rest);
    if let Some(rest) = ["bash", "zsh", "sh"]
        .iter()
        .find_map(|shell| rest.strip_prefix(shell))
        .filter(|rest| rest.starts_with(char::is_whitespace))
    {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("-lc").or_else(|| rest.strip_prefix("-c")) {
            if rest.starts_with(char::is_whitespace) {
                script = rest.trim_start();
                let quoted = script.len() >= 2
                    && (script.starts_with('\'') || script.starts_with('"'))
                    && script.chars().next() == script.chars().last();
                if quoted {
                    script = &script[1..script.len() - 1];
                }
            }
        }
    }
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ")
}

fn tail_lines(output: &str) -> String {
    let lines: Vec<&str> = output.trim_end().lines().map(str::trim_end).collect();
    if lines.len() > OUTPUT_LINES {
        let hidden = lines.len() - OUTPUT_LINES;
        format!("… {hidden} more line(s) ⏎ {}", lines[hidden..].join(" ⏎ "))
    } else {
        lines.join(" ⏎ ")
    }
}

/// `model` and `model_reasoning_effort` from Codex's own config, for runs that name neither.
pub fn configured() -> (Option<String>, Option<String>) {
    let home = env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")));
    let Some(config) = home.and_then(|home| fs::read_to_string(home.join("config.toml")).ok())
    else {
        return (None, None);
    };
    let (mut model, mut effort) = (None, None);
    for line in config.lines().map(str::trim) {
        // Keys inside a table belong to something else, such as a profile.
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "model" => model = model.or_else(|| config_string(value)),
            "model_reasoning_effort" => effort = effort.or_else(|| config_string(value)),
            _ => (),
        }
    }
    (model, effort)
}

fn config_string(value: &str) -> Option<String> {
    let value = value.trim();
    let parsed = match value.chars().next()? {
        quote @ ('"' | '\'') => value[1..].split(quote).next()?.to_owned(),
        _ => value.split('#').next()?.trim().to_owned(),
    };
    (!parsed.is_empty()).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_wrappers_are_stripped_from_commands() {
        assert_eq!(short_command("/bin/zsh -lc 'cargo test'"), "cargo test");
        assert_eq!(short_command("/usr/bin/bash -c \"ls\n  -la\""), "ls ⏎ -la");
        assert_eq!(short_command("cargo test"), "cargo test");
        assert_eq!(short_command("/bin/shellcheck x"), "/bin/shellcheck x");
    }

    #[test]
    fn config_values_are_read_with_quotes_and_comments() {
        assert_eq!(
            config_string(" \"gpt-6.1-sol\" # fast").as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(config_string(" 'high'").as_deref(), Some("high"));
        assert_eq!(config_string(" high # c").as_deref(), Some("high"));
        assert_eq!(config_string(" \"\""), None);
    }
}
