//! The crate as `harness/scripts.py` drives it, cargo being the oracle there. `UNIT` is a
//! [`Unit`] and `CARGO_ENV` the environment cargo was started with, both JSON files.
//!
//! - `run UNIT CARGO_ENV` runs the script as blaze runs it and prints the environment it was
//!   given, its outputs and what cargo makes of them.
//! - `parse UNIT CARGO_ENV OUTPUT` reads a file of a script's output as blaze reads it.
//! - `inert SOURCE EDITION [ENV [ARGS]]` says whether the script of `SOURCE` is one blaze does
//!   not compile or run ([`blaze_build_script::inert`]), and with `ENV`, the environment a run
//!   was given, what it would print; with `ARGS`, the JSON list of what rustc was given for it,
//!   whether those let it be one.
//!
//! Under any other name it stands in for a script cargo runs: the harness puts it where rustc
//! wrote the script and the script's path in `harness-real` beside it. It writes what cargo
//! gave it to `harness-ran.json` beside `OUT_DIR` and becomes the script.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command line program: it answers on stdout and fails on stderr"
)]

use std::{collections::BTreeMap, ffi::OsString, path::Path, process::ExitCode};

use blaze_build_script::{
    Error, Unit, inert,
    output::{self, Context, Output, Severity},
    run::{self, Failed, Invocation},
};
use serde_derive::Serialize;

#[derive(Serialize)]
struct Ran {
    env: BTreeMap<String, String>,
    status: Option<i32>,
    stdout: String,
    stderr: String,
    output: Option<Output>,
    error: Option<String>,
    log_messages: Vec<(Severity, String)>,
}

#[derive(Serialize)]
struct Inert {
    inert: bool,
    why: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

#[derive(Serialize)]
struct Parsed {
    output: Option<Output>,
    error: Option<String>,
}

fn main() -> ExitCode {
    let exe = std::env::current_exe().unwrap_or_default();
    if exe.file_stem().is_some_and(|it| it != "blaze-build-script") {
        return record(&exe);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let printed = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["run", unit, cargo_env] => ran(unit, cargo_env),
        ["parse", unit, cargo_env, output] => parsed(unit, cargo_env, output),
        ["inert", source, edition] => inert(source, edition, None, None),
        ["inert", source, edition, env] => inert(source, edition, Some(env), None),
        ["inert", source, edition, env, args] => inert(source, edition, Some(env), Some(args)),
        _ => Err(Error(
            "usage: blaze-build-script run UNIT CARGO_ENV | parse UNIT CARGO_ENV OUTPUT | inert \
             SOURCE EDITION [ENV [ARGS]]"
                .to_owned(),
        )),
    };
    match printed {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("blaze-build-script: {err}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Serialize)]
struct Recorded {
    env: BTreeMap<String, String>,
    cwd: String,
    args: Vec<String>,
}

#[cfg(unix)]
fn record(exe: &Path) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let dir = exe.parent().unwrap_or(Path::new("."));
    let real = match std::fs::read_to_string(dir.join("harness-real")) {
        Ok(real) => real,
        Err(err) => {
            eprintln!("blaze-build-script: {}: {err}", dir.join("harness-real").display());
            return ExitCode::FAILURE;
        }
    };
    let recorded = Recorded {
        env: std::env::vars_os()
            .map(|(name, value)| {
                (name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())
            })
            .collect(),
        cwd: std::env::current_dir().unwrap_or_default().display().to_string(),
        args: std::env::args_os().map(|it| it.to_string_lossy().into_owned()).collect(),
    };
    if let Some(out_dir) = std::env::var_os("OUT_DIR")
        && let Some(run_dir) = Path::new(&out_dir).parent()
        && let Ok(json) = json(&recorded)
    {
        let _ = std::fs::write(run_dir.join("harness-ran.json"), json);
    }
    let mut args = std::env::args_os();
    #[expect(clippy::disallowed_methods, reason = "it becomes the script, where cargo ran it")]
    let err = std::process::Command::new(real.trim())
        .arg0(args.next().unwrap_or_default())
        .args(args)
        .exec();
    eprintln!("blaze-build-script: {}: {err}", real.trim());
    ExitCode::FAILURE
}

#[cfg(not(unix))]
fn record(exe: &Path) -> ExitCode {
    eprintln!("blaze-build-script: {}: the harness records scripts on unix only", exe.display());
    ExitCode::FAILURE
}

fn read<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, Error> {
    let text = std::fs::read(path).map_err(|err| Error(format!("{path}: {err}")))?;
    serde_json::from_slice(&text).map_err(|err| Error(format!("{path}: {err}")))
}

fn inputs(unit: &str, cargo_env: &str) -> Result<(Unit, Vec<(OsString, OsString)>), Error> {
    let cargo_env: BTreeMap<String, String> = read(cargo_env)?;
    let cargo_env: Vec<_> =
        cargo_env.into_iter().map(|(name, value)| (name.into(), value.into())).collect();
    let unit: Unit = read(unit)?;
    let cargo_env = blaze_build_script::env::cargo_started(&cargo_env, &unit.host);
    Ok((unit, cargo_env))
}

fn json(value: &impl serde::Serialize) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|err| Error(err.to_string()))
}

fn ran(unit: &str, cargo_env: &str) -> Result<String, Error> {
    let (unit, cargo_env) = inputs(unit, cargo_env)?;
    let env = blaze_build_script::env(&unit, &cargo_env)?;
    let shown = env
        .vars
        .iter()
        .map(|(name, value)| {
            (name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())
        })
        .collect();
    let mut command = Invocation::of(&unit, env).command();
    let jobserver = match unit.jobserver {
        Some(_) => {
            Some(jobserver::Client::new(unit.jobs as usize).map_err(|err| Error(err.to_string()))?)
        }
        None => None,
    };
    if let Some(jobserver) = &jobserver {
        jobserver.configure(&mut command);
    }
    let ran = run::run(&unit, command)?;
    let (output, error, log_messages) = match ran.finish(&unit, &Context::of(&unit, &cargo_env)) {
        Ok(output) => (Some(output), None, Vec::new()),
        Err(Failed { error, log_messages }) => (None, Some(error.0), log_messages),
    };
    json(&Ran {
        env: shown,
        status: ran.status.code(),
        stdout: String::from_utf8_lossy(&ran.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
        output,
        error,
        log_messages,
    })
}

fn parsed(unit: &str, cargo_env: &str, output: &str) -> Result<String, Error> {
    let (unit, cargo_env) = inputs(unit, cargo_env)?;
    let printed =
        std::fs::read(Path::new(output)).map_err(|err| Error(format!("{output}: {err}")))?;
    let parsed = output::parse(&printed, &Context::of(&unit, &cargo_env));
    json(&match parsed {
        Ok(output) => Parsed { output: Some(output), error: None },
        Err(error) => Parsed { output: None, error: Some(error.0) },
    })
}

fn inert(
    source: &str,
    edition: &str,
    env: Option<&str>,
    args: Option<&str>,
) -> Result<String, Error> {
    let text = std::fs::read_to_string(source).map_err(|err| Error(format!("{source}: {err}")))?;
    let edition = edition.parse().map_err(|err| Error(format!("{err}")))?;
    let env: Option<BTreeMap<String, String>> = env.map(read).transpose()?;
    let args: Vec<String> = args.map(read).transpose()?.unwrap_or_default();
    json(&match inert::args(&args).and_then(|()| inert::of(&text, edition)) {
        Ok(script) => {
            let printed = env.map(|env| script.run(|name| env.get(name).map(String::as_str)));
            let text = |it: &[u8]| String::from_utf8_lossy(it).into_owned();
            Inert {
                inert: true,
                why: None,
                stdout: printed.as_ref().map(|it| text(&it.stdout)),
                stderr: printed.as_ref().map(|it| text(&it.stderr)),
            }
        }
        Err(why) => Inert { inert: false, why: Some(why.0), stdout: None, stderr: None },
    })
}
