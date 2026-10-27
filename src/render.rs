use crate::view::{Outcome, Status, Step, StepKind, Update};
use std::io::{self, IsTerminal, Write};

const BRIEF_WIDTH: usize = 88;
const RESULT_CHARS: usize = 4_000;
const RESULT_LINES: usize = 60;

pub fn one_line(text: &str, limit: usize) -> String {
    let clean: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let compact = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let mut shortened: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        shortened.push('…');
    }
    shortened
}

pub struct Renderer<W> {
    output: W,
    color: bool,
}

impl<W: Write> Renderer<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            color: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub fn write(&mut self, update: Update) -> io::Result<()> {
        match update {
            Update::Brief(brief) => self.brief(&brief),
            Update::Step(step) => self.step(step),
        }
    }

    fn brief(&mut self, brief: &str) -> io::Result<()> {
        writeln!(self.output, "┌─ Brief {}┐", "─".repeat(BRIEF_WIDTH - 6))?;
        let clipped: String = brief.chars().take(12_000).collect();
        for line in clipped.split('\n') {
            let clean: Vec<char> = line
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            if clean.is_empty() {
                self.brief_line("")?;
            }
            for chunk in clean.chunks(BRIEF_WIDTH) {
                self.brief_line(&chunk.iter().collect::<String>())?;
            }
        }
        if brief.chars().count() > 12_000 {
            self.brief_line("…")?;
        }
        writeln!(self.output, "└{}┘\n", "─".repeat(BRIEF_WIDTH + 2))
    }

    fn brief_line(&mut self, line: &str) -> io::Result<()> {
        writeln!(self.output, "│ {line:<BRIEF_WIDTH$} │")
    }

    fn step(&mut self, step: Step) -> io::Result<()> {
        let (style, label) = match step.kind {
            StepKind::Thought => ("2", "thought"),
            StepKind::Tool => ("36", "tool"),
            StepKind::Edit => ("32", "✎ edit"),
            StepKind::Message => ("1", "message"),
            StepKind::ToolResult => ("2", "result"),
        };
        let stamp = step
            .timestamp
            .split('T')
            .nth(1)
            .and_then(|time| time.get(..8))
            .unwrap_or("--:--:--");
        let text = format!(
            "{} {label}  {}",
            one_line(stamp, 8),
            one_line(&step.text, 220)
        );
        if self.color {
            writeln!(self.output, "\x1b[{style}m{text}\x1b[0m")
        } else {
            writeln!(self.output, "{text}")
        }
    }

    pub fn result(&mut self, outcome: &Outcome) -> io::Result<()> {
        let (style, label) = match outcome.status {
            Status::Completed => ("1;32", "completed"),
            Status::Failed => ("1;31", "failed"),
            Status::Killed => ("1;33", "killed"),
            Status::Unknown => ("1;35", "unknown"),
        };
        let heading = format!("── status: {label} ──");
        writeln!(self.output)?;
        if self.color {
            writeln!(self.output, "\x1b[{style}m{heading}\x1b[0m")?;
        } else {
            writeln!(self.output, "{heading}")?;
        }
        let mut chars = outcome.text.chars();
        let clipped: String = chars.by_ref().take(RESULT_CHARS).collect();
        let mut lines: Vec<&str> = clipped.lines().collect();
        let mut more = chars.next().is_some();
        if lines.len() > RESULT_LINES {
            lines.truncate(RESULT_LINES);
            more = true;
        }
        for line in lines {
            // Reports are model output: control characters must not reach the terminal.
            let clean: String = line
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            writeln!(self.output, "{clean}")?;
        }
        if more {
            writeln!(self.output, "…")?;
        }
        self.flush()
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::one_line;

    #[test]
    fn truncation_preserves_unicode_and_removes_terminal_controls() {
        assert_eq!(one_line("", 2), "");
        assert_eq!(one_line("é猫", 2), "é猫");
        assert_eq!(one_line("é猫a", 2), "é猫…");
        assert_eq!(one_line("a\n\t\x1bb", 10), "a b");
    }
}
