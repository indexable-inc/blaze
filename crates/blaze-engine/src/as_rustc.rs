//! blaze as the `RUSTC` a build script runs (docs/no-rustc.md, step I): what a script asks
//! of rustc is answered here and no rustc runs. A question (`-vV`, `--print ...`) is answered
//! from what blaze knows of the toolchain ([`crate::answers`]), and a probe, a crate on
//! standard input or in a file that the script has "rustc" check (`autocfg`, `anyhow`,
//! `thiserror`, `rustix`), is given blaze's verdict on it, the one `blaze check` gives
//! ([`crate::check::once`]): it compiles, and the files asked for are written, or it does
//! not, and the errors are said and the run fails, as rustc's would. What can't be answered
//! that way (a flag blaze does not know, a probe that links or names a library, a feature
//! blaze does not know the toolchain has) fails with an error that names the command, and
//! the script sees rustc fail.
//!
//! A script is given this as `RUSTC` by [`crate::cargo::script_sandbox`], which links blaze
//! as `rustc` in the run's directory, with the toolchain's rustc in [`TOOLCHAIN`] and no
//! wrappers: blaze runs as this whenever it is started under the name `rustc`.
//!
//! What the files written are: the metadata and the IR of a probe are only seen to be there
//! (`autocfg` removes the `.ll` it asked for, `anyhow` the whole directory, `rustix` never
//! reads its file), so they are empty, and the dep-info names the probe's source.

pub(crate) use blaze_cargo::as_rustc::{NAME, TOOLCHAIN};

use std::{
    ffi::{OsStr, OsString},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, bail};

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
        Ok(code) => code,
        Err(why) => {
            let shown: Vec<_> = args.iter().map(|it| it.to_string_lossy()).collect();
            let shown = shown.join(" ");
            eprintln!("error: blaze can't answer `rustc {shown}` for a build script: {why:#}");
            1
        }
    };
    std::process::exit(code)
}

fn run(args: &[OsString]) -> anyhow::Result<i32> {
    let rustc = std::env::var_os(TOOLCHAIN)
        .with_context(|| format!("`{TOOLCHAIN}` does not name blaze's rustc"))?;
    let sysroot = Path::new(&rustc).parent().and_then(Path::parent);
    let sysroot = sysroot.with_context(|| format!("`{TOOLCHAIN}` names no toolchain's rustc"))?;
    crate::cargo::toolchain::known_at(sysroot.to_owned());
    // The cargo the load asks of the standard library asks blaze's wrapper, not this.
    // SAFETY: nothing else runs yet in this process.
    unsafe { std::env::set_var("RUSTC", &rustc) };
    let mut stdin = Vec::new();
    if args.iter().any(|it| it == "-") {
        std::io::stdin().read_to_end(&mut stdin).context("reading standard input")?;
    }
    if crate::answers::is_question(args) {
        let answer = crate::answers::ours_told(args, &stdin)
            .context("blaze has no answer to this question")?;
        std::io::stderr().write_all(&answer.stderr)?;
        std::io::stdout().write_all(&answer.stdout)?;
        return Ok(0);
    }
    let args: Vec<&str> = args
        .iter()
        .map(|it| it.to_str())
        .collect::<Option<_>>()
        .context("an argument that is not text")?;
    let probe = Probe::of(&args)?;
    probe.answer(sysroot, &stdin)
}

/// What a script asked rustc to check: one crate, by itself.
#[derive(Debug, Default, PartialEq)]
struct Probe<'a> {
    /// The file, or `None` for standard input.
    input: Option<&'a str>,
    crate_name: Option<&'a str>,
    edition: Option<&'a str>,
    /// What rustc is told that decides the crate's cfgs, asked again as `--print cfg`.
    cfg_flags: Vec<&'a str>,
    /// Each kind asked for, and the path it is asked at.
    emit: Vec<(&'a str, Option<&'a str>)>,
    out_dir: Option<&'a str>,
    output: Option<&'a str>,
    extra_filename: &'a str,
}

/// The kinds of output a probe is given: what rustc writes of a crate it checks.
const EMITTED: [&str; 3] = ["metadata", "dep-info", "llvm-ir"];

/// The flags with a value that change nothing a probe is answered with.
const IGNORED_VALUED: [&str; 8] =
    ["-L", "--cap-lints", "-A", "-W", "--allow", "--warn", "--error-format", "--color"];

impl<'a> Probe<'a> {
    fn of(args: &[&'a str]) -> anyhow::Result<Probe<'a>> {
        let mut probe = Probe::default();
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
                "-" => probe.input_is(None)?,
                "--crate-name" => probe.crate_name = Some(value()?),
                "--edition" => probe.edition = Some(value()?),
                "--out-dir" => probe.out_dir = Some(value()?),
                "-o" => probe.output = Some(value()?),
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
                            bail!("blaze checks a library, not a `--crate-type {kind}`");
                        }
                    }
                }
                "--cfg" | "--target" => {
                    probe.cfg_flags.extend([flag, value()?]);
                }
                "-O" | "-g" => probe.cfg_flags.push(flag),
                "-C" | "--codegen" => {
                    let option = value()?;
                    if let Some(extra) = option.strip_prefix("extra-filename=") {
                        probe.extra_filename = extra;
                    } else {
                        probe.cfg_flags.extend(["-C", option]);
                    }
                }
                "-Z" => probe.cfg_flags.extend(["-Z", value()?]),
                _ if IGNORED_VALUED.contains(&flag) => _ = value()?,
                "-D" | "-F" | "--deny" | "--forbid" | "--force-warn" => {
                    bail!("blaze can't tell which lints rustc would find, which `{flag}` denies")
                }
                _ if !flag.starts_with('-') => probe.input_is(Some(word))?,
                _ => bail!("blaze does not know rustc's `{flag}`"),
            }
        }
        if probe.emit.is_empty() {
            bail!("rustc would link the crate, and blaze checks it");
        }
        if probe.output.is_some() && probe.emit.len() > 1 {
            bail!("`-o` with more than one `--emit`");
        }
        Ok(probe)
    }

    fn input_is(&mut self, input: Option<&'a str>) -> anyhow::Result<()> {
        if self.input.is_some() {
            bail!("more than one input");
        }
        self.input = Some(input.unwrap_or("-"));
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
            None => bail!("no input"),
        }
    }

    /// Checks the crate with blaze's release at `sysroot`, its source on `stdin` when that is
    /// the input: what rustc would end with, the files asked for written when it compiles.
    fn answer(&self, sysroot: &Path, stdin: &[u8]) -> anyhow::Result<i32> {
        let name = self.name()?;
        // The cfgs the crate is checked with are those rustc would print for the same flags,
        // which blaze answers or does not.
        let mut asked: Vec<OsString> = self.cfg_flags.iter().map(OsString::from).collect();
        asked.extend(["--print", "cfg"].map(OsString::from));
        crate::answers::ours(&asked, &[])
            .context("blaze can't tell the cfgs rustc would check the crate with")?;
        let cfg_args: Vec<String> = self.cfg_flags.iter().map(|it| (*it).to_owned()).collect();
        let cfg = crate::cargo::own_script::cfg_of(&cfg_args);
        let target = self.cfg_flags.windows(2).find(|it| it[0] == "--target").map(|it| it[1]);
        let dir = Scratch::new()?;
        let root = match self.input.context("no input")? {
            "-" => {
                let root = dir.path().join(format!("{name}.rs"));
                std::fs::write(&root, stdin)?;
                root
            }
            file => std::path::absolute(file)?,
        };
        let text = std::fs::read_to_string(&root)
            .with_context(|| format!("reading {}", root.display()))?;
        let edition = self.edition.unwrap_or("2015");
        if let Some(unknown) = features(&text, edition).into_iter().find(|it| !known_feature(it)) {
            bail!("blaze does not know whether the toolchain has the feature `{unknown}`");
        }
        let project = dir.path().join("rust-project.json");
        let described = serde_json::json!({
            "sysroot": sysroot,
            "sysroot_src": crate::cargo::toolchain::library::dir(),
            "crates": [{
                "display_name": name,
                "root_module": root,
                "edition": edition,
                "deps": [],
                "cfg": cfg,
                "target": target,
                "env": {},
                "is_workspace_member": true,
                "is_proc_macro": false,
            }],
        });
        std::fs::write(&project, described.to_string())?;
        if let Some(refused) = crate::check::once(&project, &name)? {
            eprintln!("{refused}");
            return Ok(1);
        }
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
            let text = match *kind {
                "dep-info" => {
                    let root = root.display();
                    format!("{}: {root}\n\n{root}:\n", path.display())
                }
                _ => String::new(),
            };
            // `-o -` is standard output to rustc (rustix's probes), not a file named `-`.
            if path == Path::new("-") {
                std::io::stdout().write_all(text.as_bytes())?;
                continue;
            }
            std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(0)
    }
}

/// A directory of this process's own for the probe's description, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> anyhow::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!("blaze-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
        Ok(Scratch(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The features the crate root `text` names in a `#![feature(...)]`, under a `cfg_attr` or
/// not: one that is off still has to be one blaze knows.
fn features(text: &str, edition: &str) -> Vec<String> {
    use syntax::{AstNode as _, ast::HasAttrs as _};
    let edition = edition.parse().unwrap_or(syntax::Edition::CURRENT);
    let file = syntax::SourceFile::parse(text, edition).tree();
    let mut named = Vec::new();
    for attr in file.attrs() {
        let attr = attr.syntax().text().to_string();
        if !attr.starts_with("#!") {
            continue;
        }
        let mut rest = attr.as_str();
        while let Some(at) = rest.find("feature") {
            rest = rest[at + "feature".len()..].trim_start();
            let Some(inside) = rest.strip_prefix('(') else { continue };
            let end = inside.find(')').unwrap_or(inside.len());
            let words = inside[..end].split(',').map(str::trim).filter(|it| !it.is_empty());
            named.extend(words.map(str::to_owned));
            rest = &inside[end..];
        }
    }
    named
}

/// Whether the toolchain has the unstable feature `name`, as far as blaze knows: the features
/// of the unstable book rust-analyzer was made with.
fn known_feature(name: &str) -> bool {
    ide_db::generated::lints::FEATURES.iter().any(|it| it.label == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_read() {
        let anyhow = [
            "--cfg=anyhow_build_probe",
            "--edition=2018",
            "--crate-name=anyhow",
            "--crate-type=lib",
            "--cap-lints=allow",
            "--emit=dep-info,metadata",
            "--out-dir",
            "/o/probe",
            "src/nightly.rs",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-Clinker-features=-lld",
        ];
        let probe = Probe::of(&anyhow).unwrap();
        assert_eq!(probe.input, Some("src/nightly.rs"));
        assert_eq!(probe.emit, [("dep-info", None), ("metadata", None)]);
        assert_eq!(
            probe.cfg_flags,
            [
                "--cfg",
                "anyhow_build_probe",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "linker-features=-lld"
            ]
        );
        let autocfg = "--crate-name autocfg_0 --crate-type=lib --out-dir /o --emit=llvm-ir -";
        let autocfg: Vec<&str> = autocfg.split(' ').collect();
        let probe = Probe::of(&autocfg).unwrap();
        assert_eq!((probe.input, probe.name().unwrap().as_str()), (Some("-"), "autocfg_0"));
        let rustix = ["--crate-type=rlib", "--emit=metadata", "-o", "/o/f", "-"];
        assert_eq!(Probe::of(&rustix).unwrap().name().unwrap(), "rust_out");
        assert!(Probe::of(&["--crate-type=bin", "--emit=metadata", "-"]).is_err());
        assert!(Probe::of(&["--emit=link", "-"]).is_err());
        assert!(Probe::of(&["-Dwarnings", "--emit=metadata", "-"]).is_err());
        assert!(Probe::of(&["--extern", "a=b", "--emit=metadata", "-"]).is_err());
        assert!(Probe::of(&["a.rs", "b.rs", "--emit=metadata"]).is_err());
    }

    #[test]
    fn features_named() {
        let text = "//! x\n#![cfg_attr(probe, feature(error_generic_member_access))]\n\
                    #![feature(a, b)]\n#![allow(stable_features)]\nfn feature() {}\n";
        assert_eq!(features(text, "2018"), ["error_generic_member_access", "a", "b"]);
        assert!(known_feature("error_generic_member_access"));
        assert!(!known_feature("no_such_feature_at_all"));
    }
}
