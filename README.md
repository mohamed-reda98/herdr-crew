# herdr-crew

A Linux plugin for Herdr 0.9+ that opens one read-only **view** for each Claude
Code **subagent** an **orchestrator** starts, and for each Codex **delegate** it
starts through `herdr-crew delegate`. Each view is a separate tab in the
orchestrator's workspace and opens without taking focus. It shows the **brief**
first, then live **steps**, then the result and status, and closes 10 seconds
after the child ends.

## Install

Run these commands from this repository **inside a Herdr pane**:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release
herdr plugin link .
./target/release/herdr-crew install-hooks
```

`install-hooks` merges `SubagentStart` and `SubagentStop` command hooks into
`~/.claude/settings.json`. It preserves other settings and existing hooks, keeps
an exact backup alongside the file as `settings.json.bak.<time>.<pid>`, and makes
no changes when the hooks are already installed. Invalid settings are rejected
without changing the file. To use a different settings file:

```sh
./target/release/herdr-crew install-hooks --settings /absolute/path/settings.json
```

Restart Claude Code after installing the hooks. The installed commands use the
absolute path to the built binary, including shell quoting for spaces. Keep this
checkout at that path and rebuild after updates. The linked plugin launches
`bin/herdr-crew`, which executes `target/release/herdr-crew` from this checkout.

Start a subagent from Claude Code inside Herdr. A tab named `<model> <effort>: <task>`
(for example `sonnet-5.5 high: find ledger query`) appears without switching away from the orchestrator. Each subagent gets its own
tab; there is no shared tab or configured concurrency limit. Switching to a view
does not send input to the subagent. Closing the view does not stop the subagent.

## Commands and lifecycle

- `herdr-crew hook subagent-start` reads Claude Code's hook JSON from stdin and
  detaches tab creation into a new Linux session, with all standard streams
  disconnected. It never waits for Herdr.
- `herdr-crew hook subagent-stop` detaches a stop record write. It does not display
  the final report or status.
- `herdr-crew view` follows the child selected by `HERDR_CREW_CHILD_DIR`. The
  plugin sets this automatically; it is not a general conversation browser.
- `herdr-crew install-hooks [--settings PATH]` installs the two hooks above.
- `herdr-crew delegate --agent codex [--title <short task>] <relay args...>` runs
  a Codex delegate; see [Delegates](#delegates).

Outside a Herdr pane the hooks exit 0 silently, while `install-hooks`, `--help`
and `delegate` work anywhere (`delegate` runs the relay unchanged, with no view).
Both `HERDR_ENV=1` and a nonempty `HERDR_PANE_ID` make a pane a Herdr pane. Hooks
also exit 0 silently on malformed input, filesystem failures,
missing executables, or Herdr failures. Their settings timeout is two seconds;
all filesystem writes and Herdr calls happen in detached processes. Nested
subagent transcripts are ignored.

The workspace comes from `HERDR_WORKSPACE_ID`, falling back to the part of
`HERDR_PANE_ID` before `:`. The detached start calls only the Herdr CLI:

```text
herdr plugin pane open --plugin herdr-crew --entrypoint view --placement tab \
  --workspace <workspace> --no-focus --env HERDR_CREW_CHILD_DIR=<directory>
herdr tab rename <tab_id> '<model> <effort>: <task>'
```

The view waits for
`<orchestrator transcript without .jsonl>/subagents/agent-<agent_id>.jsonl` to
appear, polls every 50 ms, and buffers incomplete lines, including split UTF-8.
Malformed complete lines and unrelated records are ignored. It renders the
first user message as a framed brief and assistant thoughts, tool calls, file
edits, messages, and subsequent tool results as timestamped steps. Thoughts and
tool results are dim, tools are cyan, edits are marked in green, and messages
are bold when output is a terminal (`NO_COLOR` disables styling). Step text is
flattened to one line and capped at 220 characters; the brief retains paragraphs,
wraps at 88 characters, and is capped at 12,000 characters. Terminal control
characters from transcripts are removed.

The stop record ends following; the view exits when the 10-second linger from
that record has elapsed. A sibling `agent-<agent_id>.meta.json` with
`"finished": true` supplies a fallback if the stop hook is missed (linger starts
when the view observes it). Herdr closes the plugin tab when its command exits.
If neither completion signal arrives, the view continues waiting; close it
manually.

## Delegates

```sh
herdr-crew delegate --agent codex --title "fix login redirect" --brief brief.txt --cd /path/to/repo
```

This starts the `codex-delegate` skill's relay (`node $CODEX_DELEGATE_RELAY`,
default `~/.claude/skills/codex-delegate/scripts/relay.mjs`) and opens a view for
the run. `--agent` and `--title` are consumed; everything else reaches the relay
untouched, and its stdin, stdout, stderr and exit code are the relay's own
(`128 + n` if a signal ended it). Only `--agent codex` exists; any other agent is
rejected with exit 2 before anything starts. The brief may be given with
`--brief` or on stdin.

Inside a Herdr pane the wrapper:

- forces `--out-dir <state>/delegates/<run>/out` unless the caller gave one (a
  valueless `--out-dir` is left for the relay to reject), so the view can follow
  `events.jsonl` live;
- opens a view tab once the relay has written `brief.txt`, so a run the relay
  rejects up front has no view. The view shows the brief, then Codex's commands,
  edits, thoughts, messages and errors from the event stream, then the result
  (`finalMessage`) and status. Codex events carry no times, so each step is
  stamped when the view sees it;
- names the tab `<model> <effort>: <task>`. The task is `--title` as given, else
  the first 3 words of the brief; model and effort come from `--model` /
  `--effort`, else `model` / `model_reasoning_effort` in
  `$CODEX_HOME/config.toml` (default `~/.codex`), else from the result when a
  lane set them. Unknown parts are dropped, and an unknown model reads `codex`;
- shows the delegate on the orchestrator's pane: a `codex ⇠ claude` label and the
  [sidebar summary](#sidebar-summary), plus a working agent state when Herdr
  detects no agent in the pane. The label and the agent state are cleared when
  the last delegate from the pane ends. A killed wrapper is covered too: a
  detached watchdog recomputes once the wrapper and the relay are both gone;
- stops the run when the orchestrator's session closes. A detached guard watches
  the nearest `claude` ancestor process; when it ends the relay gets SIGTERM
  (so it aborts Codex and writes an `aborted` result), and after a grace period
  the processes and process groups the run made are killed.

Status in the view: `completed` shows `finalMessage`; `failed`, `timeout` and
any other relay status show the error; `aborted`, or a relay that vanished
without `result.json` (and without an error event), is `killed`. A `result.json`
older than the run (a reused `--out-dir`) is ignored.

## Sidebar summary

While any child of an orchestrator is working, its pane carries a summary in the
`delegate` metadata token (`herdr pane report-metadata <pane> --token
delegate=<summary>`), for example `1 delegate working`, `1 sub working` or
`2 delegates · 1 sub working`. Delegates and subagents are counted apart. The
token is removed when nothing is working. Herdr shows it where a sidebar row
uses `$delegate`; this plugin does not edit your Herdr config:

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["agent", "$delegate"]]
```

Each child registers itself under `panes/<pane>/` in the state directory, and
every start and end recomputes the summary from those records under a lock, so
children that start or finish together cannot leave an older count on screen.
A child counts as working only while it is verifiably so: a delegate while its
wrapper or relay runs, a subagent until its stop hook is recorded; either stops
counting once the orchestrator process is gone. A subagent whose hook could not
identify the orchestrator is never counted. A killed delegate is handled by its
watchdog, and a subagent whose orchestrator died by its start hook's detached
process, which stays until the subagent stops or the orchestrator goes away.

## State and environment

State lives under `$XDG_STATE_HOME/herdr-crew`, defaulting to
`~/.local/state/herdr-crew`:

```text
<session_id>/<agent_id>/
  child.json   subagent identity and transcript path
  open.lock    exclusive claim preventing duplicate start hooks from opening tabs
  stop.json    stop time and optional transcript path supplied by the stop hook
panes/<pane>/
  lock         held while the summary is recomputed and reported
  d.<run>.json, s.<session>.<agent>.json  one record per child that may be working
delegates/<run>/
  child.json   what the view follows: kind, output directory, relay, name inputs
  tab.json     the view's tab, recorded by the opener
  delegate.json  pane reporting, wrapper, relay and orchestrator processes
  out/         the relay's brief.txt, events.jsonl, final.txt, result.json (default)
```

New directories are private (0700), and new files are private (0600). State
contains transcript paths, not copied conversations. Records remain after a view
closes so duplicate start hooks remain harmless; you can remove old session
directories once their subagents have stopped. A failed tab-open attempt releases
its claim so a repeated start hook can retry.

Environment overrides:

| Variable | Meaning |
| --- | --- |
| `HERDR_BIN` | Herdr CLI executable; defaults to `herdr` on `PATH` |
| `HERDR_CREW_STATE_DIR` | State root (use an absolute path) |
| `HERDR_CREW_CHILD_DIR` | The view's per-child state directory, passed explicitly by the start hook |
| `HERDR_CREW_LINGER_MS` | Linger in milliseconds; defaults to `10000`, forwarded to the view |
| `CODEX_DELEGATE_RELAY` | The relay script `delegate` runs |
| `HERDR_CREW_NODE` | Interpreter for the relay; defaults to `node` |
| `HERDR_CREW_ORCHESTRATOR_PID` | Process whose end stops a delegate; `0` turns the guard off |
| `HERDR_CREW_KILL_GRACE` | Seconds the relay gets to stop before the leftovers are killed; defaults to `15` |
| `HERDR_REPORT_SOURCE` | `--source` for the pane reports; defaults to `herdr-crew` |
| `CODEX_HOME` | Where Codex's `config.toml` is read from; defaults to `~/.codex` |

## Development

`src/hooks.rs` owns detached hook dispatch and Herdr CLI calls; `src/state.rs`
owns state records; `src/view.rs` owns the view lifecycle and `StepSource`
interface; `src/transcript.rs` supplies Claude Code steps and `src/codex.rs`
Codex delegate steps (both follow files through `src/tail.rs`); `src/delegate.rs`
is the `delegate` wrapper, `src/pane.rs` its pane reports and `src/supervise.rs`
its detached watchdog and guard; `src/summary.rs` the per-pane records and the
sidebar summary; `src/naming.rs` builds tab names; `src/render.rs`
renders terminal output; `src/install.rs` merges settings; `src/main.rs`
dispatches commands and enforces the Herdr environment guard. A second delegate
kind adds a `state::Kind`, a relay in `Relay::of` and a step source beside
`CodexRun`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Tests use synthetic transcripts, temporary state/settings, and a fake Herdr CLI.
They never use live Herdr, real Claude settings, real conversations, or the
network; delegate tests use a fake relay (`CODEX_DELEGATE_RELAY` with
`HERDR_CREW_NODE=/bin/sh`). Claude delegates, nested children, and review logic
are outside this implementation.
