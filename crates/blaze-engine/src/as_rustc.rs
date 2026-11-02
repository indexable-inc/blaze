//! blaze as the `RUSTC` a build script runs (docs/no-rustc.md): what a script asks of rustc is
//! answered here and no rustc runs. The daemon links blaze's executable as `rustc` and `rustdoc`
//! in its `tools` directory and gives a script those as `RUSTC` and `RUSTDOC`
//! (`blaze-cargo/src/serve/unit.rs`, `stand_ins`); blaze runs as this whenever it is started
//! under the name `rustc`, and as [`rustdoc`] under that one.
//!
//! A question (`-vV`, `--print ...`) is answered from what blaze knows of its release
//! ([`blaze_cargo::answers`]). A probe, a crate on standard input or in a file that the script
//! has "rustc" check (`autocfg`, `anyhow`, `thiserror`, `rustix`), is given the verdict `blaze
//! check` gives it ([`probe`]): it compiles, and the files asked for are written, or it does
//! not, and its errors are said and the run fails, as rustc's would. What can't be answered that
//! way (a flag blaze does not know, a probe that links or names a library) fails with an error
//! that names the command, and the script sees rustc fail.
//!
//! What the files written are: the metadata and the IR of a probe are only seen to be there
//! (`autocfg` removes the `.ll` it asked for, `anyhow` the whole directory, `rustix` never
//! reads its file), so they are empty, and the dep-info names the probe's source.

mod probe;
pub(crate) mod rustdoc;
#[cfg(test)]
mod tests;

pub(crate) use blaze_cargo::as_rustc::NAME;

use std::{
    ffi::{OsStr, OsString},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, anyhow, bail};
use blaze_diagnostics::emit;
use blaze_units::stdlib::target;

/// Whether this process was started as `rustc`, which is to be this.
pub(crate) fn is_me() -> bool {
    let Some(first) = std::env::args_os().next() else { return false };
    Path::new(&first).file_stem().is_some_and(|it| it == NAME)
}

/// Answers what the script asked rustc with the arguments of this process, and ends as rustc
/// would have.
pub(crate) fn command() -> ! {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let code = match run(&args) {
        Ok(outcome) => {
            let said = std::io::stderr()
                .write_all(&outcome.stderr)
                .and_then(|()| std::io::stdout().write_all(&outcome.stdout))
                .and_then(|()| std::io::stdout().flush());
            match said {
                Ok(()) => outcome.code,
                Err(error) => {
                    eprintln!("error: writing rustc's answer: {error}");
                    1
                }
            }
        }
        Err(why) => {
            let shown: Vec<_> = args.iter().map(|it| it.to_string_lossy()).collect();
            let shown = shown.join(" ");
            eprintln!("error: blaze can't answer `rustc {shown}` for a build script: {why:#}");
            1
        }
    };
    std::process::exit(code)
}

/// What a script asks of rustc that blaze does not check as rustc would.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// A crate that is not a library: `--crate-type` naming another kind, or none, for which
    /// rustc builds a binary (and fails one with no `main`).
    NotALibrary { crate_type: Option<String> },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::NotALibrary { crate_type: Some(kind) } => {
                write!(f, "blaze checks a library, not a `--crate-type {kind}`")
            }
            Refused::NotALibrary { crate_type: None } => f.write_str(
                "blaze checks a library, and with no `--crate-type` rustc builds a binary: \
                 `--crate-type lib` asks for a library",
            ),
        }
    }
}

impl std::error::Error for Refused {}

/// What rustc would have printed and ended with.
#[derive(Debug)]
struct Outcome {
    code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run(args: &[OsString]) -> anyhow::Result<Outcome> {
    let mut stdin = Vec::new();
    if args.iter().any(|it| it == "-") {
        std::io::stdin().read_to_end(&mut stdin).context("reading standard input")?;
    }
    answer(args, &stdin, found_library)
}

/// The standard library the daemon builds against: where blaze found it, checked to be the
/// release's.
fn found_library() -> anyhow::Result<probe::Library> {
    let found = blaze_cargo::toolchain::library::found()
        .map_err(|why| anyhow!("blaze's standard library, which a probe is checked against: {why}"))?;
    probe::Library::at(found.dir.clone())
}

/// What rustc says to `args` with `stdin`: a question answered, or a probe checked against the
/// library `library` gives.
fn answer(
    args: &[OsString],
    stdin: &[u8],
    library: impl FnOnce() -> anyhow::Result<probe::Library>,
) -> anyhow::Result<Outcome> {
    if blaze_cargo::answers::is_question(args) {
        let answer = blaze_cargo::answers::ours_told(args, stdin).context("blaze has no answer to this question")?;
        return Ok(Outcome { code: 0, stdout: answer.stdout, stderr: answer.stderr });
    }
    let args: Vec<&str> = args.iter().map(|it| it.to_str()).collect::<Option<_>>().context("an argument that is not text")?;
    let probe = Probe::of(&args)?;
    if std::env::var_os("RUSTC_BOOTSTRAP").is_some_and(|it| it == "-1") {
        bail!("blaze's rustc is a nightly, and `RUSTC_BOOTSTRAP=-1` asks for a stable one, which blaze does not check as");
    }
    let library = library()?;
    probe.answer(&args, stdin, &library)
}

/// What a script asked rustc to check: one crate, by itself.
#[derive(Debug, Default, PartialEq)]
struct Probe<'a> {
    /// The file, or `-` for standard input.
    input: Option<&'a str>,
    crate_name: Option<&'a str>,
    edition: Option<&'a str>,
    target: Option<&'a str>,
    /// What rustc is told that decides the crate's cfgs, as `--print cfg` is asked with it.
    cfg_flags: Vec<&'a str>,
    /// Each kind asked for, and the path it is asked at.
    emit: Vec<(&'a str, Option<&'a str>)>,
    out_dir: Option<&'a str>,
    output: Option<&'a str>,
    extra_filename: &'a str,
    error_format: Option<&'a str>,
    color: Option<&'a str>,
    width: Option<&'a str>,
}

/// The kinds of output a probe is given: what rustc writes of a crate it checks.
const EMITTED: [&str; 3] = ["metadata", "dep-info", "llvm-ir"];

/// The lint flags, and `--check-cfg`: read by the check from the whole command line.
const LINTS: [&str; 12] = [
    "-A",
    "-W",
    "-D",
    "-F",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--force-warn",
    "--cap-lints",
    "--check-cfg",
    "-L",
];

impl<'a> Probe<'a> {
    fn of(args: &[&'a str]) -> anyhow::Result<Probe<'a>> {
        let mut probe = Probe::default();
        let mut library = false;
        let mut words = args.iter().copied();
        while let Some(word) = words.next() {
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => match word.as_bytes() {
                    [b'-', b'C' | b'Z' | b'L' | b'A' | b'W' | b'D' | b'F', _, ..] => {
                        (&word[..2], Some(&word[2..]))
                    }
                    _ => (word, None),
                },
            };
            let mut value = || {
                joined.or_else(|| words.next()).with_context(|| format!("`{flag}` has no value"))
            };
            match flag {
                "-" => probe.input_is("-")?,
                "--crate-name" => probe.crate_name = Some(value()?),
                "--edition" => probe.edition = Some(value()?),
                "--target" => probe.target = Some(value()?),
                "--out-dir" => probe.out_dir = Some(value()?),
                "-o" => probe.output = Some(value()?),
                "--error-format" => probe.error_format = Some(value()?),
                "--color" => probe.color = Some(value()?),
                "--diagnostic-width" => probe.width = Some(value()?),
                "--emit" => {
                    for kind in value()?.split(',') {
                        let (kind, path) = match kind.split_once('=') {
                            Some((kind, path)) => (kind, Some(path)),
                            None => (kind, None),
                        };
                        if !EMITTED.contains(&kind) {
                            bail!("blaze checks a probe, and `--emit={kind}` asks for more");
                        }
                        probe.emit.push((kind, path));
                    }
                }
                "--crate-type" => {
                    for kind in value()?.split(',') {
                        if !matches!(kind, "lib" | "rlib") {
                            return Err(Refused::NotALibrary { crate_type: Some(kind.to_owned()) }.into());
                        }
                    }
                    library = true;
                }
                "--cfg" => probe.cfg_flags.extend([flag, value()?]),
                "-O" | "-g" => probe.cfg_flags.push(flag),
                "-C" | "--codegen" => {
                    let option = value()?;
                    match option.strip_prefix("extra-filename=") {
                        Some(extra) => probe.extra_filename = extra,
                        None => probe.cfg_flags.extend(["-C", option]),
                    }
                }
                "-Z" => probe.cfg_flags.extend(["-Z", value()?]),
                _ if LINTS.contains(&flag) => _ = value()?,
                _ if !flag.starts_with('-') => probe.input_is(word)?,
                _ => bail!("blaze does not know rustc's `{flag}`"),
            }
        }
        if probe.input.is_none() {
            bail!("no input filename given");
        }
        if !library {
            return Err(Refused::NotALibrary { crate_type: None }.into());
        }
        if probe.emit.is_empty() {
            bail!("rustc would link the crate, and blaze checks it");
        }
        if probe.output.is_some() && probe.emit.len() > 1 {
            bail!("`-o` with more than one `--emit`");
        }
        Ok(probe)
    }

    fn input_is(&mut self, input: &'a str) -> anyhow::Result<()> {
        if self.input.is_some() {
            bail!("more than one input");
        }
        self.input = Some(input);
        Ok(())
    }

    /// The crate's name: as given, or as rustc takes it of the file, `rust_out` for standard
    /// input.
    fn name(&self) -> anyhow::Result<String> {
        if let Some(name) = self.crate_name {
            return Ok(name.to_owned());
        }
        match self.input {
            Some("-") => Ok("rust_out".to_owned()),
            Some(file) => {
                let stem = Path::new(file).file_stem().and_then(OsStr::to_str);
                Ok(stem.context("an input with no name")?.replace('-', "_"))
            }
            None => bail!("no input filename given"),
        }
    }

    /// How rustc writes its diagnostics, and whether as JSON.
    fn style(&self) -> anyhow::Result<(emit::Style, bool)> {
        let mut style = emit::Style::default();
        let json = match self.error_format {
            None | Some("human") => false,
            Some("short") => {
                style.short = true;
                false
            }
            Some("json") => true,
            Some(other) => bail!("blaze writes no `--error-format={other}`"),
        };
        style.ansi = match self.color {
            None | Some("auto" | "never") => false,
            Some("always") => true,
            Some(other) => bail!("argument for `--color` must be auto, always or never (instead was `{other}`)"),
        };
        if let Some(width) = self.width {
            style.width = width.parse().with_context(|| format!("`--diagnostic-width={width}` is not a number"))?;
        }
        Ok((style, json))
    }

    /// The probe checked with blaze's front end over `library`, all of rustc's `args` read for
    /// its lints: what rustc would end with, the files asked for written when it compiles.
    fn answer(&self, args: &[&str], stdin: &[u8], library: &probe::Library) -> anyhow::Result<Outcome> {
        let name = self.name()?;
        let host = blaze_cargo::toolchain::host().context("blaze knows no target triple of this machine")?;
        let triple = self.target.unwrap_or(host);
        if triple.ends_with(".json") {
            bail!("blaze checks a probe for the targets it knows, and `--target {triple}` is a file of its own");
        }
        // The cfgs the crate is checked with are those rustc would print for the same flags.
        let flags: Vec<String> = self.cfg_flags.iter().map(|it| (*it).to_owned()).collect();
        let mut cfg_args = target::CfgArgs::from_flags(&flags).map_err(anyhow::Error::msg)?;
        if cfg_args.target_cpu.as_deref() == Some("native") {
            cfg_args.host_cpu = hir_ty_codegen::host_cpu().map(|(name, features)| target::HostCpu {
                arch: target::arch(host).to_owned(),
                name,
                features,
            });
        }
        let cfgs = target::target_cfg(triple, &cfg_args)
            .map_err(|why| anyhow!("blaze can't tell the cfgs rustc would check the crate with: {why}"))?;
        let (shown, source) = match self.input.context("no input filename given")? {
            "-" => ("<anon>", String::from_utf8(stdin.to_vec()).context("the probe on standard input is not UTF-8")?),
            file => (file, std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?),
        };
        let (style, json) = self.style()?;
        let all: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
        let asked = probe::Asked {
            name: &name,
            edition: self.edition.unwrap_or("2015"),
            triple,
            host,
            cfgs: &cfgs,
            args: &all,
            style,
            json,
        };
        let checked = probe::check(&asked, shown, &source, library)?;
        let stderr = checked.stderr.into_bytes();
        if checked.failed {
            return Ok(Outcome { code: 1, stdout: Vec::new(), stderr });
        }
        let mut stdout = Vec::new();
        for (kind, path) in &self.emit {
            let path = match (*path, self.output) {
                (Some(path), _) | (None, Some(path)) => PathBuf::from(path),
                (None, None) => {
                    let out_dir = Path::new(self.out_dir.unwrap_or("."));
                    let extra = self.extra_filename;
                    out_dir.join(match *kind {
                        "metadata" => format!("lib{name}{extra}.rmeta"),
                        "dep-info" => format!("{name}{extra}.d"),
                        _ => format!("{name}{extra}.ll"),
                    })
                }
            };
            let text = match (*kind, self.input) {
                ("dep-info", Some("-")) => format!("{}:\n", path.display()),
                ("dep-info", Some(file)) => format!("{}: {file}\n\n{file}:\n", path.display()),
                _ => String::new(),
            };
            // `-o -` is standard output to rustc (rustix's probes), not a file named `-`.
            if path == Path::new("-") {
                stdout.extend_from_slice(text.as_bytes());
                continue;
            }
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(Outcome { code: 0, stdout, stderr })
    }
}
