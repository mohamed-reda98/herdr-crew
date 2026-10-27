use crate::{
    herdr,
    state::{Kind, Reporting},
};
use serde_json::Value;
use std::{env, process::Stdio};

// Distinct from a bare agent name so a pane the user opened by hand is never mistaken for it.
/// The label for the delegates working from a pane, by the kinds of agent they run.
pub fn label(kinds: &[Kind]) -> Option<&'static str> {
    let first = kinds.first()?;
    if kinds.iter().any(|kind| kind != first) {
        return Some("delegates ⇠ claude");
    }
    Some(match first {
        Kind::Claude => "claude ⇠ claude",
        _ => "codex ⇠ claude",
    })
}

fn call(args: &[&str]) {
    // Fire and forget: whatever Herdr prints is not the delegate's output.
    let _ = herdr::command().args(args).stdout(Stdio::null()).status();
}

/// The agent Herdr detects in the pane, if any.
pub fn detected_agent(pane: &str) -> Option<String> {
    let output = herdr::command().args(["agent", "list"]).output().ok()?;
    let reply: Value = serde_json::from_slice(&output.stdout).ok()?;
    reply
        .pointer("/result/agents")?
        .as_array()?
        .iter()
        .find(|agent| agent["pane_id"] == pane)
        .and_then(|agent| agent["agent"].as_str())
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned)
}

/// The source Herdr attributes the reports to.
pub fn source() -> String {
    env::var("HERDR_REPORT_SOURCE")
        .ok()
        .filter(|source| !source.is_empty())
        .unwrap_or_else(|| "herdr-crew".to_owned())
}

/// Shows that a delegate is working. Herdr only lets a source set the lifecycle state of a pane
/// that has no detected agent; elsewhere the label and token are all it honours.
pub fn announce(report: &Reporting, kind: Kind) {
    if report.lifecycle {
        call(&[
            "pane",
            "report-agent",
            &report.pane,
            "--source",
            &report.source,
            "--agent",
            &report.agent,
            "--state",
            "working",
            "--message",
            message(kind),
        ]);
    }
}

fn message(kind: Kind) -> &'static str {
    match kind {
        Kind::Claude => "claude -p (delegated)",
        _ => "codex exec (delegated)",
    }
}

/// Gives back the lifecycle state `announce` claimed.
pub fn release(report: &Reporting) {
    if report.lifecycle {
        call(&[
            "pane",
            "report-agent",
            &report.pane,
            "--source",
            &report.source,
            "--agent",
            &report.agent,
            "--state",
            "idle",
        ]);
        call(&[
            "pane",
            "release-agent",
            &report.pane,
            "--source",
            &report.source,
            "--agent",
            &report.agent,
        ]);
    }
}

/// Puts the summary in the `delegate` token, which the user's sidebar rows render, and the label
/// beside it while a delegate runs. `None` removes both.
pub fn show(pane: &str, source: &str, label: Option<&str>, summary: Option<&str>) {
    let mut args = vec!["pane", "report-metadata", pane, "--source", source];
    let token = summary.map(|summary| format!("delegate={summary}"));
    if let Some(label) = label {
        args.extend(["--display-agent", label]);
    } else {
        args.push("--clear-display-agent");
    }
    match &token {
        Some(token) => args.extend(["--token", token]),
        None => args.extend(["--clear-token", "delegate"]),
    }
    call(&args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_the_kind_of_delegate_working() {
        assert_eq!(label(&[]), None);
        assert_eq!(label(&[Kind::Codex, Kind::Codex]), Some("codex ⇠ claude"));
        assert_eq!(label(&[Kind::Claude]), Some("claude ⇠ claude"));
        assert_eq!(
            label(&[Kind::Codex, Kind::Claude]),
            Some("delegates ⇠ claude")
        );
    }
}
