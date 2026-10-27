# herdr-crew

A Linux plugin for Herdr 0.9+ that opens one read-only **view** for each Claude
Code **subagent** an **orchestrator** starts. Each view is a separate tab in the
orchestrator's workspace and opens without taking focus. It shows the **brief**
first, then live **steps**, and closes 10 seconds after the subagent stops.

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

Start a subagent from Claude Code inside Herdr. A tab named `⇢ sub: <agent type>`
appears without switching away from the orchestrator. Each subagent gets its own
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

Outside a Herdr pane, **every command** exits 0 silently, including hook
installation and help. Both `HERDR_ENV=1` and a nonempty `HERDR_PANE_ID` are
required. Hooks also exit 0 silently on malformed input, filesystem failures,
missing executables, or Herdr failures. Their settings timeout is two seconds;
all filesystem writes and Herdr calls happen in detached processes. Nested
subagent transcripts are ignored.

The workspace comes from `HERDR_WORKSPACE_ID`, falling back to the part of
`HERDR_PANE_ID` before `:`. The detached start calls only the Herdr CLI:

```text
herdr plugin pane open --plugin herdr-crew --entrypoint view --placement tab \
  --workspace <workspace> --no-focus --env HERDR_CREW_CHILD_DIR=<directory>
herdr tab rename <tab_id> '⇢ sub: <agent type>'
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
manually. There is no final report or status appended by the view.

## State and environment

State lives under `$XDG_STATE_HOME/herdr-crew`, defaulting to
`~/.local/state/herdr-crew`:

```text
<session_id>/<agent_id>/
  child.json   subagent identity and transcript path
  open.lock    exclusive claim preventing duplicate start hooks from opening tabs
  stop.json    stop time and optional transcript path supplied by the stop hook
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

## Development

`src/hooks.rs` owns detached hook dispatch and Herdr CLI calls; `src/state.rs`
owns state records; `src/view.rs` owns the view lifecycle and `StepSource`
interface; `src/transcript.rs` supplies Claude Code steps; `src/render.rs` renders
terminal output; `src/install.rs` merges settings; `src/main.rs` dispatches
commands and enforces the Herdr environment guard. The placeholder tab name is
kept in `tab_name` in `src/hooks.rs`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Tests use synthetic transcripts, temporary state/settings, and a fake Herdr CLI.
They never use live Herdr, real Claude settings, real conversations, or the
network. Delegates, nested children, sidebar summaries, and review logic are
outside this implementation.
