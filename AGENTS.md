# herdr-crew

A Herdr plugin, written in Rust, that opens a read-only **view** for every **child** an **orchestrator** starts, so you
can watch what each one was asked and what it is doing. Read `GLOSSARY.md` first and use its terms in code, docs and
messages. Work is tracked as GitHub issues on `mohamed-reda98/herdr-crew`.

## Gates

Rust is installed under `~/.cargo/bin`, which may not be on `PATH`: prefix commands with
`PATH="$HOME/.cargo/bin:$PATH"`. Every change must leave all of these green:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Rules

- Linux and Herdr 0.9+ only.
- Talk to Herdr only through the `herdr` CLI. The binary is found through `HERDR_BIN`, falling back to `herdr` on
  `PATH`, so tests can point it at a fake that records its arguments.
- Tests never touch the live Herdr session, `~/.claude`, `~/.codex` or the network. Use temp dirs, a fake `herdr`,
  and synthetic fixtures. Never copy a real conversation file into the repo: they hold private client data.
- Outside a Herdr pane (`HERDR_ENV` is not `1` or `HERDR_PANE_ID` is empty) every hook does nothing visible and
  exits 0, and `herdr-crew delegate` runs the relay unchanged with no view. User-facing commands (`install-hooks`,
  `--help`) work anywhere, and the view runs in a plugin pane, which may not have `HERDR_PANE_ID`.
- A hook must never fail or noticeably slow the orchestrator: swallow errors, return fast, do slow work in a
  detached process.
- No review logic: herdr-crew never starts a review or any other agent.
- Code comments explain why, not what. No ticket numbers or process words ("phase", "for now", "MVP") in code.
- Do not run `git add` or `git commit`; the orchestrator commits after review.
