use crate::{herdr, state::Reporting};
use serde_json::Value;
use std::process::Stdio;

// Distinct from a bare agent name so a pane the user opened by hand is never mistaken for it.
const LABEL: &str = "codex ⇠ claude";
const TOKEN: &str = "delegate=codex running";

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

/// Shows that a delegate is working. Herdr only lets a source set the lifecycle state of a pane
/// that has no detected agent; elsewhere the label and token are all it honours.
pub fn announce(report: &Reporting) {
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
            "codex exec (delegated)",
        ]);
    }
    call(&[
        "pane",
        "report-metadata",
        &report.pane,
        "--source",
        &report.source,
        "--display-agent",
        LABEL,
        "--token",
        TOKEN,
    ]);
}

pub fn clear(report: &Reporting) {
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
    call(&[
        "pane",
        "report-metadata",
        &report.pane,
        "--source",
        &report.source,
        "--clear-display-agent",
        "--clear-token",
        "delegate",
    ]);
}
