mod herdr;
mod hooks;
mod install;
mod naming;
mod render;
mod state;
mod transcript;
mod view;

use std::{env, path::PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn in_herdr() -> bool {
    env::var("HERDR_ENV").as_deref() == Ok("1")
        && env::var_os("HERDR_PANE_ID").is_some_and(|pane| !pane.is_empty())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    // Hooks fire in every Claude Code session, so outside Herdr they must stay invisible.
    let hook_side = args
        .first()
        .is_some_and(|arg| arg == "hook" || arg == "dispatch");
    if hook_side && !in_herdr() {
        return;
    }
    // Hook errors must never become feedback to the orchestrator.
    if args.first().is_some_and(|arg| arg == "hook") {
        let _ = hooks::receive(&args[1..]);
        return;
    }
    if args.first().is_some_and(|arg| arg == "dispatch") {
        if let [_, action, payload] = args.as_slice() {
            let _ = hooks::dispatch(action, payload);
        }
        return;
    }
    if let Err(error) = run(&args) {
        eprintln!("herdr-crew: {error}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["view"] => view::run(),
        ["install-hooks"] => install::run(None),
        ["install-hooks", "--settings", path] => install::run(Some(PathBuf::from(path))),
        [] | ["--help"] | ["-h"] => {
            println!("herdr-crew\n\n  hook subagent-start\n  hook subagent-stop\n  view\n  install-hooks [--settings PATH]");
            Ok(())
        }
        _ => Err("expected hook, view, or install-hooks; see --help".into()),
    }
}
