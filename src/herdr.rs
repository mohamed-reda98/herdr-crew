use std::{env, process::Command, process::Stdio};

pub fn command() -> Command {
    // The view runs in a plugin pane, which gets HERDR_BIN_PATH rather than HERDR_BIN.
    let binary = ["HERDR_BIN", "HERDR_BIN_PATH"]
        .iter()
        .filter_map(env::var_os)
        .find(|path| !path.is_empty())
        .unwrap_or_else(|| "herdr".into());
    let mut command = Command::new(binary);
    command.stdin(Stdio::null()).stderr(Stdio::null());
    command
}
