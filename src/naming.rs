use crate::{herdr, render, state};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

const TASK_WORDS: usize = 3;
const NESTED_MARK: &str = "↳";
const PART_WIDTH: usize = 80;

/// `claude-opus-5-5` -> `opus-5.5`; ids that are not `claude-*` are kept as they are.
pub fn short_model(id: &str) -> String {
    let id = id.trim();
    let id = match id.rfind('[') {
        Some(at) if id.ends_with(']') => id[..at].trim_end(),
        _ => id,
    };
    let Some(rest) = id.strip_prefix("claude-") else {
        return id.to_owned();
    };
    let mut parts = rest.split('-');
    let family = parts.next().unwrap_or("");
    if family.is_empty() || !family.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return rest.to_owned();
    }
    let parts: Vec<&str> = parts.collect();
    let version: Vec<&str> = parts
        .iter()
        .copied()
        .take_while(|part| is_version_part(part))
        .collect();
    // A trailing snapshot date adds nothing to a tab name; any other qualifier does.
    let qualifiers: Vec<&str> = parts[version.len()..]
        .iter()
        .copied()
        .filter(|part| !(part.len() == 8 && part.bytes().all(|byte| byte.is_ascii_digit())))
        .collect();
    let mut short = family.to_owned();
    if !version.is_empty() {
        short.push('-');
        short.push_str(&version.join("."));
    }
    for qualifier in qualifiers {
        short.push('-');
        short.push_str(qualifier);
    }
    short
}

fn is_version_part(part: &str) -> bool {
    (1..=2).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
}

/// `<model> <effort>: <task>`, dropping whatever is unknown without leaving gaps. A subagent of
/// a delegate (`nested`) reads as the delegate's child with a leading `↳`.
pub fn tab_name(
    model: Option<&str>,
    effort: Option<&str>,
    description: Option<&str>,
    agent_type: &str,
    nested: bool,
) -> String {
    let task = description
        .map(first_words)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| clean(agent_type));
    let name = compose(model, effort, &task, "sub");
    if nested {
        format!("{NESTED_MARK} {name}")
    } else {
        name
    }
}

/// A delegate's name: the title as given, else the first words of its brief. `agent` stands in
/// for an unknown model.
pub fn delegate_name(
    agent: &str,
    model: Option<&str>,
    effort: Option<&str>,
    title: Option<&str>,
    brief: Option<&str>,
) -> String {
    let title = title.map(clean).filter(|text| !text.is_empty());
    let task = title
        .or_else(|| brief.map(first_words).filter(|text| !text.is_empty()))
        .unwrap_or_default();
    compose(model, effort, &task, agent)
}

fn clean(text: &str) -> String {
    render::one_line(text, PART_WIDTH)
}

fn first_words(text: &str) -> String {
    clean(
        &text
            .split_whitespace()
            .take(TASK_WORDS)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

// `unnamed_head` stands in for the model when it is unknown.
fn compose(model: Option<&str>, effort: Option<&str>, task: &str, unnamed_head: &str) -> String {
    let model = model.map(short_model).map(|model| clean(&model));
    let effort = effort.map(clean);
    let head = match model.filter(|model| !model.is_empty()) {
        Some(model) => match effort.filter(|effort| !effort.is_empty()) {
            Some(effort) => format!("{model} {effort}"),
            None => model,
        },
        None => unnamed_head.to_owned(),
    };
    if task.is_empty() {
        head
    } else {
        format!("{head}: {task}")
    }
}

/// The description the orchestrator gave a subagent, from the file beside its conversation.
pub fn read_description(transcript: &Path) -> Option<String> {
    state::read_json::<serde_json::Value>(&transcript.with_extension("meta.json"))
        .ok()?
        .get("description")?
        .as_str()
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())
}

pub fn rename(tab_id: &str, name: &str) -> bool {
    herdr::command()
        .args(["tab", "rename", tab_id, name])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Renames a view's tab each time more of its name becomes known.
pub struct Namer {
    tab_file: PathBuf,
    applied: Option<String>,
    tab_id: Option<String>,
}

impl Namer {
    pub fn new(child_dir: &Path) -> Self {
        Self {
            tab_file: child_dir.join("tab.json"),
            applied: None,
            tab_id: None,
        }
    }

    /// `name` is only evaluated once the tab is known to be the view's own.
    pub fn refresh(&mut self, name: impl FnOnce() -> String) {
        // The opener records the tab after its own rename, so waiting for it keeps that rename
        // from landing on top of a newer name.
        if self.tab_id.is_none() {
            let Ok(tab) = state::read_json::<state::Tab>(&self.tab_file) else {
                return;
            };
            self.tab_id = Some(tab.tab_id);
            self.applied = Some(tab.name);
        }
        let name = name();
        if self.applied.as_ref() != Some(&name) {
            // A failed rename is not retried: it would repeat on every poll.
            if let Some(tab_id) = &self.tab_id {
                rename(tab_id, &name);
            }
            self.applied = Some(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_model_names() {
        for (id, short) in [
            ("claude-opus-5-5", "opus-5.5"),
            ("claude-sonnet-5-5", "sonnet-5.5"),
            ("claude-haiku-5-5", "haiku-5.5"),
            ("claude-fable-5-1", "fable-5.1"),
            ("claude-opus-5-5[1m]", "opus-5.5"),
            ("claude-sonnet-5-5-20260101", "sonnet-5.5"),
            ("claude-opus-5", "opus-5"),
            ("claude-opus", "opus"),
            ("claude-3-5-sonnet-20241022", "3-5-sonnet-20241022"),
            ("gpt-6.1-sol", "gpt-6.1-sol"),
            ("gpt-6.1-sol[1m]", "gpt-6.1-sol"),
        ] {
            assert_eq!(short_model(id), short, "{id}");
        }
    }

    #[test]
    fn delegate_names_use_the_title_whole_and_the_brief_by_its_first_words() {
        let name = delegate_name;
        assert_eq!(
            name(
                "codex",
                Some("gpt-6.1-sol"),
                Some("high"),
                Some("fix the login redirect bug"),
                Some("ignored")
            ),
            "gpt-6.1-sol high: fix the login redirect bug"
        );
        assert_eq!(
            name(
                "codex",
                Some("gpt-6.1-sol"),
                None,
                None,
                Some("Port the\nwrapper into Rust")
            ),
            "gpt-6.1-sol: Port the wrapper"
        );
        assert_eq!(
            name("codex", None, None, Some(" "), Some("a b c d")),
            "codex: a b c"
        );
        assert_eq!(name("codex", None, Some("high"), None, None), "codex");
        assert_eq!(
            name("claude", None, None, Some("port it"), Some("ignored")),
            "claude: port it"
        );
    }

    #[test]
    fn names_cut_the_task_and_fill_gaps() {
        let name = |model, effort, description, agent_type| {
            tab_name(model, effort, description, agent_type, false)
        };
        assert_eq!(
            name(
                Some("claude-opus-5-5"),
                Some("high"),
                Some("research ledger"),
                "general-purpose"
            ),
            "opus-5.5 high: research ledger"
        );
        assert_eq!(
            name(
                Some("claude-sonnet-5-5"),
                Some("medium"),
                Some("Research:   hydraulic\npress brief"),
                "x"
            ),
            "sonnet-5.5 medium: Research: hydraulic press"
        );
        assert_eq!(
            name(
                Some("claude-opus-5-5"),
                None,
                Some("research ledger"),
                "general-purpose"
            ),
            "opus-5.5: research ledger"
        );
        assert_eq!(
            name(Some("claude-opus-5-5"), Some(" "), Some("a"), "x"),
            "opus-5.5: a"
        );
        assert_eq!(
            name(None, None, Some("research ledger"), "general-purpose"),
            "sub: research ledger"
        );
        assert_eq!(
            name(None, Some("high"), Some("research ledger"), "x"),
            "sub: research ledger"
        );
        assert_eq!(name(None, None, None, "Explore"), "sub: Explore");
        assert_eq!(name(None, None, Some("  "), "Explore"), "sub: Explore");
        assert_eq!(
            name(Some("claude-opus-5-5"), Some("high"), None, "Explore"),
            "opus-5.5 high: Explore"
        );
        assert_eq!(name(None, None, None, ""), "sub");
        assert_eq!(
            name(Some("claude-opus-5-5"), Some("high"), None, " "),
            "opus-5.5 high"
        );
        assert_eq!(name(Some(""), None, None, "x"), "sub: x");
        assert_eq!(
            tab_name(
                Some("claude-sonnet-5-5"),
                Some("high"),
                Some("find ledger query now"),
                "x",
                true
            ),
            "↳ sonnet-5.5 high: find ledger query"
        );
        assert_eq!(
            tab_name(None, None, None, "Explore", true),
            "↳ sub: Explore"
        );
    }
}
