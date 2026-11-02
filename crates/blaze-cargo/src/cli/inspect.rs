//! blaze's own looks at the daemon's last build of a checkout, which the proof harness reads
//! (`harness/v2/engine.py`): `blaze codegen-inputs --format=json` (each instance key's hex to
//! its code hash's hex), `blaze trace --out PATH` (the CBOR of `blaze_v2_proof::Trace`) and
//! `blaze tables --format=json` (`{"<name>": {"elements": N, "bytes": N}}`). Each takes a
//! build's package, target and profile flags, which name the build it asks about.
//!
//! `blaze pack -p PACKAGE --out PATH` is answered the same way: the daemon checks the package
//! for [`PACK_TARGET`] with debug assertions off, as `--release` does, and the CLI writes the
//! pack it answers (`blaze_wasm_pack::Pack`'s JSON) to `PATH`, for blaze's in-browser compiler.

use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
};

use blaze_api::{Request, Response, ResponseRef, Table, ViewRef};

use super::{
    FAILED, USAGE,
    args::{self, Command},
    daemon::{self, Api, Start},
    json, place, say,
};

/// The target `blaze pack` checks for: the in-browser compiler's.
pub const PACK_TARGET: &str = "wasm32-unknown-unknown";

/// Which look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inspected {
    CodegenInputs,
    Trace,
    Tables,
    Pack,
}

impl Inspected {
    pub fn named(word: &str) -> Option<Inspected> {
        match word {
            "codegen-inputs" => Some(Inspected::CodegenInputs),
            "trace" => Some(Inspected::Trace),
            "tables" => Some(Inspected::Tables),
            "pack" => Some(Inspected::Pack),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Inspected::CodegenInputs => "codegen-inputs",
            Inspected::Trace => "trace",
            Inspected::Tables => "tables",
            Inspected::Pack => "pack",
        }
    }
}

/// The command's own flags, and the build's words.
#[derive(Debug, PartialEq, Eq)]
pub struct Own {
    /// `trace`'s and `pack`'s `--out PATH`, as typed.
    pub out: Option<PathBuf>,
    pub build: Vec<OsString>,
}

/// Reads `--format FORMAT` (`json`, the one there is; `codegen-inputs` and `tables` only) and
/// `--out PATH` (`trace`'s and `pack`'s, which they need), each with `=` or as the next word;
/// every other word is the build's. `pack` names its own target and profile, so it refuses
/// `--target`, `--release` and `--profile`.
pub fn own(what: Inspected, words: Vec<OsString>) -> Result<Own, String> {
    let named = what.name();
    let mut own = Own { out: None, build: Vec::with_capacity(words.len()) };
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let Some(text) = word.to_str() else {
            own.build.push(word);
            continue;
        };
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (text, None),
        };
        if what == Inspected::Pack && matches!(flag, "--target" | "--release" | "-r" | "--profile") {
            return Err(format!(
                "`blaze pack` checks for {PACK_TARGET} with debug assertions off, as `--release`: it takes no `{flag}`"
            ));
        }
        if !matches!(flag, "--format" | "--out") {
            own.build.push(word);
            continue;
        }
        let flag = flag.to_owned();
        let value = match inline {
            Some(value) => value,
            None => match words.next().map(|it| it.into_string()) {
                Some(Ok(value)) => value,
                Some(Err(value)) if flag == "--out" => {
                    own.out = Some(PathBuf::from(value));
                    continue;
                }
                Some(Err(_)) => return Err(format!("`blaze {named} {flag}` takes a UTF-8 value")),
                None => return Err(format!("`blaze {named} {flag}` takes a value")),
            },
        };
        match (what, flag.as_str()) {
            (Inspected::CodegenInputs | Inspected::Tables, "--format") if value == "json" => {}
            (Inspected::CodegenInputs | Inspected::Tables, "--format") => {
                return Err(format!("`blaze {named}` prints JSON only (`--format=json`), not `{value}`"));
            }
            (Inspected::Trace | Inspected::Pack, "--out") => own.out = Some(PathBuf::from(value)),
            (_, flag) => return Err(format!("`blaze {named}` takes no `{flag}`")),
        }
    }
    if what == Inspected::Trace && own.out.is_none() {
        return Err("`blaze trace` writes the trace to `--out PATH`, which it was not given".to_owned());
    }
    if what == Inspected::Pack && own.out.is_none() {
        return Err("`blaze pack` writes the pack to `--out PATH`, which it was not given".to_owned());
    }
    Ok(own)
}

/// `blaze codegen-inputs|trace|tables|pack WORDS`: its exit code.
pub fn main(what: Inspected, words: Vec<OsString>, color: bool) -> i32 {
    let refused = |what: &str, code: i32| {
        eprint!("{}", say::error(what, color));
        code
    };
    let own = match own(what, words) {
        Ok(own) => own,
        Err(what) => return refused(&what, USAGE),
    };
    let mut words = own.build;
    let command = match what {
        Inspected::Pack => {
            words.extend(["--target", PACK_TARGET, "--release"].map(OsString::from));
            Command::Check
        }
        Inspected::CodegenInputs | Inspected::Trace | Inspected::Tables => Command::Build,
    };
    let build = match args::command_line(command, words) {
        Ok(build) => build,
        Err(usage) => {
            eprint!("{}", args::refusal(&usage, color));
            return USAGE;
        }
    };
    let (place, cargo) = match super::prepared_cargo(&build, color) {
        Ok(prepared) => prepared,
        Err(what) => return refused(&what, FAILED),
    };
    let view = ViewRef::Head(cargo.root.clone());
    // alloc: error path, the refusal said once.
    let targets = |cargo| place::targets(&build, cargo).map_err(|it| refused(&it.to_string(), FAILED));
    let request = match what {
        Inspected::CodegenInputs => match targets(cargo) {
            Ok(targets) => Request::CodegenInputs { view, targets },
            Err(code) => return code,
        },
        Inspected::Trace => match targets(cargo) {
            Ok(targets) => Request::Trace { view, targets },
            Err(code) => return code,
        },
        Inspected::Tables => Request::Tables { view },
        Inspected::Pack => {
            let [program] = &build.flags.packages[..] else {
                return refused("`blaze pack` packs one program: name its package with `-p PACKAGE`", USAGE);
            };
            let program = program.clone(); // clone: the request owns it, once
            match targets(cargo) {
                Ok(targets) => Request::Pack { view, targets, program },
                Err(code) => return code,
            }
        }
    };
    let mut client = match daemon::connect(Start::IfMissing) {
        Ok(Some(client)) => client,
        Ok(None) => return refused("no daemon answers, and none could be started", FAILED),
        Err(error) => return refused(&format!("{error:#}"), FAILED),
    };
    let out = own.out.map(|out| place.cwd.join(out));
    let (mut stdout, mut stderr) = (io::stdout().lock(), io::stderr().lock());
    answer(&mut client, what, request, out.as_deref(), &mut stdout, &mut stderr, color).unwrap_or(FAILED)
}

/// `request` sent and its answer printed on `out` (or, for `trace` and `pack`, written to `to`):
/// the exit code. A pack's check's diagnostics are said on `err` as rustc renders them.
pub fn answer<A: Api>(
    api: &mut A,
    what: Inspected,
    request: Request,
    to: Option<&Path>,
    out: &mut dyn Write, // dyn: the CLI's output writer, written once per answer.
    err: &mut dyn Write, // dyn: the CLI's output writer, written once per error said.
    color: bool,
) -> io::Result<i32> {
    let mut answered = None;
    let mut said = Vec::new();
    let mut packed = None;
    let mut rendered = String::new();
    // The answers are read owned in place too (`ResponseRef`'s once-a-request responses): each
    // is moved out whole. A pack is written from the frame's bytes where they lie.
    let called = api.call(request, |response, _| match response {
        ResponseRef::CodegenInputs(it) => answered = Some(Response::CodegenInputs(it)),
        ResponseRef::Trace(it) => answered = Some(Response::Trace(it)),
        ResponseRef::Tables(it) => answered = Some(Response::Tables(it)),
        ResponseRef::Packed { pack } => packed = Some(to.map_or(Ok(()), |to| std::fs::write(to, pack))),
        ResponseRef::Diagnostic(it) if what == Inspected::Pack => rendered.push_str(it.rendered),
        // alloc: error path, the daemon's errors, said once it is done.
        ResponseRef::Error(error) => said.push(error.what.to_owned()),
        _ => {}
    });
    err.write_all(rendered.as_bytes())?;
    for what in &said {
        err.write_all(say::error(what, color).as_bytes())?;
    }
    // dyn: the CLI's output writer, written once per error said.
    let failed = |err: &mut dyn Write, what: &str| -> io::Result<i32> {
        err.write_all(say::error(what, color).as_bytes())?;
        Ok(FAILED)
    };
    match called {
        Ok(true) => {}
        Ok(false) => return Ok(FAILED),
        Err(error) => return failed(err, &error.to_string()),
    }
    match (what, answered) {
        (Inspected::CodegenInputs, Some(Response::CodegenInputs(inputs))) => {
            out.write_all(blaze_ui::api::codegen_inputs_json(&inputs).as_bytes())?;
        }
        (Inspected::Tables, Some(Response::Tables(tables))) => out.write_all(tables_json(&tables).as_bytes())?,
        (Inspected::Trace, Some(Response::Trace(traced))) => {
            let Some(to) = to else { return failed(err, "`blaze trace` was given no `--out PATH`") };
            let bytes = match blaze_ui::api::trace_cbor(&traced) {
                Ok(bytes) => bytes,
                Err(error) => return failed(err, &format!("the trace's CBOR: {error}")),
            };
            if let Err(error) = std::fs::write(to, bytes) {
                return failed(err, &format!("writing the trace to {}: {error}", to.display()));
            }
        }
        (Inspected::Pack, _) => {
            let Some(to) = to else { return failed(err, "`blaze pack` was given no `--out PATH`") };
            match packed {
                Some(Ok(())) => {}
                Some(Err(error)) => return failed(err, &format!("writing the pack to {}: {error}", to.display())),
                None => return failed(err, "the daemon answered `blaze pack` with no pack"),
            }
        }
        _ => return failed(err, &format!("the daemon answered `blaze {}` with no answer of it", what.name())),
    }
    out.flush()?;
    Ok(0)
}

/// `{"<name>": {"elements": N, "bytes": N}, ...}`, newline ended, in the response's order.
pub fn tables_json(tables: &[Table]) -> String {
    let mut out = String::with_capacity(2 + tables.len() * 64);
    out.push('{');
    for (at, table) in tables.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        json::quote(&mut out, &table.name);
        out.push_str(": {\"elements\": ");
        out.push_str(&table.elements.to_string());
        out.push_str(", \"bytes\": ");
        out.push_str(&table.bytes.to_string());
        out.push('}');
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests;
