//! `blaze-ci serve <config.json>`: the VM as CI, on the project's warm VM.
//! `blaze-ci standby <config.json>`: on another machine, takes over from the newest snapshot
//! when the primary stops answering.

#![forbid(unsafe_code)]

use std::{path::Path, process::ExitCode};

use blaze_ci::{config::Config, runtime, standby};

const USAGE: &str = "usage: blaze-ci serve <config.json>\n       blaze-ci standby <config.json>";

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect(); // alloc: once, at start
    let (command, path) = match args.as_slice() {
        [command, path] if command == "serve" || command == "standby" => (command.as_str(), path),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config = match Config::read(Path::new(path)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("blaze-ci: {error}");
            return ExitCode::FAILURE;
        }
    };
    match command {
        "serve" => match runtime::serve(&config) {
            Ok(never) => match never {},
            Err(why) => {
                eprintln!("blaze-ci: stopped: {why}");
                ExitCode::FAILURE
            }
        },
        _ => match standby::run(&config.standby) {
            Ok((code, why)) => {
                eprintln!("blaze-ci standby: {why}");
                ExitCode::from(u8::try_from(code).unwrap_or(1))
            }
            Err(error) => {
                eprintln!("blaze-ci standby: {error}");
                ExitCode::FAILURE
            }
        },
    }
}
