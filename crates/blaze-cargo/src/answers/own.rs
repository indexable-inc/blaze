//! blaze's answers to rustc's questions, from what it knows of its own release of rustc and of
//! the targets that release has: no rustc runs. This reads rustc's command line; what the
//! release says of each target, and the cfgs that follow from the arguments, are
//! `blaze_units::stdlib::target`'s (its `rustc.json`, what the pinned rustc printed, written by
//! `harness/answers.py --write` when the pin moves). A question this does not understand whole
//! is answered `None`, and fails.

use std::{
    ffi::OsString,
    path::Path,
};

use blaze_units::stdlib::target;

/// blaze's own release, the one every rustc is answered as, on the host this blaze runs on.
/// Where its files are ([`sysroot`]) is looked for only when asked.
struct Toolchain {
    host: &'static str,
}

impl Toolchain {
    /// `None` when the data of targets is not of the pinned toolchain's rustc.
    fn ours() -> Option<Toolchain> {
        if target::rustc_version() != crate::toolchain::ours_says() {
            return None;
        }
        Some(Toolchain { host: crate::toolchain::host()? })
    }
}

/// Where blaze's release is, found with nothing run.
fn sysroot() -> Option<&'static Path> {
    crate::toolchain::sysroot()
}

/// What `cargo --version` says after `cargo ` for the toolchain at `sysroot`, from rustup's
/// channel manifest there: the release of its rustc (cargo's `CFG_RELEASE`) with the commit
/// and date of its cargo.
pub fn cargo_version(sysroot: &Path) -> Option<String> {
    let manifest = sysroot.join("lib/rustlib/multirust-channel-manifest.toml");
    let manifest = std::fs::read_to_string(manifest).ok()?;
    let version = |package: &str| {
        let (_, rest) = manifest.split_once(&format!("\n[pkg.{package}]\nversion = \""))?;
        rest.split_once('"').map(|(version, _)| version)
    };
    let (release, _) = version("rustc")?.split_once(' ')?;
    let (_, commit) = version("cargo")?.split_once(' ')?;
    Some(format!("{release} {commit}"))
}

/// How cfgs follow from `-C` and `--cfg`.
#[derive(Default)]
struct Given<'a> {
    target: Option<&'a str>,
    opt_level: Option<&'a str>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    panic: Option<&'a str>,
    /// The features of `-C target-feature`, on (`true`) or off, in order.
    features: Vec<(bool, &'a str)>,
    target_cpu: Option<&'a str>,
    cfgs: Vec<&'a str>,
    crate_name: Option<&'a str>,
    crate_types: Vec<&'a str>,
    prints: Vec<&'a str>,
    unstable_options: bool,
    from_stdin: bool,
    extra_filename: bool,
    /// Whether a flag changes which warnings rustc shows, or how: its warnings are answered
    /// only as they are shown with none, or when none are shown.
    diagnostics_set: bool,
    /// `--cap-lints allow`, or `-A warnings` last of what is said of `warnings`: rustc shows
    /// no warning at all.
    warnings_hidden: bool,
    lints_capped: bool,
}

/// The flags that change which warnings rustc shows, or how it shows them, but `-A` and
/// `-W` of `warnings` and `--cap-lints allow`.
const DIAGNOSTICS: [&str; 10] = [
    "-D",
    "-F",
    "--deny",
    "--forbid",
    "--force-warn",
    "--cap-lints",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
];

/// The prints rustc makes only as it links, so none when it compiles nothing.
pub(crate) const WHEN_LINKING: [&str; 2] = ["link-args", "native-static-libs"];

/// The flags that take a value and change no answer.
const IGNORED_VALUED: [&str; 12] = [
    "--edition",
    "--error-format",
    "--json",
    "--emit",
    "--out-dir",
    "-L",
    "--extern",
    "--check-cfg",
    "--cap-lints",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
];

/// The lint flags, whose value changes no answer.
const LINTS: [&str; 10] = [
    "-W",
    "-A",
    "-D",
    "-F",
    "--warn",
    "--allow",
    "--deny",
    "--forbid",
    "--force-warn",
    "--cap-lints",
];

/// The codegen options that change no answer. `linker-features` is how
/// rustc links (cargo's `-Clinker-features=-lld`), not the target it prints;
/// `symbol-mangling-version` how it names symbols (zed's `-C symbol-mangling-version=v0`),
/// `save-temps` which of its files it keeps (uitest.py's RUSTFLAGS).
const IGNORED_CODEGEN: [&str; 19] = [
    "embed-bitcode",
    "debuginfo",
    "metadata",
    "incremental",
    "strip",
    "codegen-units",
    "prefer-dynamic",
    "lto",
    "linker",
    "linker-features",
    "link-arg",
    "link-args",
    "split-debuginfo",
    "link-dead-code",
    "rpath",
    "force-frame-pointers",
    "force-unwind-tables",
    "symbol-mangling-version",
    "save-temps",
];

fn yes(value: Option<&str>) -> Option<bool> {
    match value {
        None | Some("y" | "yes" | "on" | "true") => Some(true),
        Some("n" | "no" | "off" | "false") => Some(false),
        Some(_) => None,
    }
}

impl<'a> Given<'a> {
    fn of(args: &'a [&'a str]) -> Option<Given<'a>> {
        let mut given = Given::default();
        let mut words = args.iter().copied();
        while let Some(word) = words.next() {
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
                _ => match word.as_bytes() {
                    // `-Cname=value`, `-Zname`, `-Wlint`, `-Ldir`.
                    [b'-', b'C' | b'Z' | b'W' | b'A' | b'D' | b'F' | b'L', _, ..] => {
                        (&word[..2], Some(&word[2..]))
                    }
                    _ => (word, None),
                },
            };
            let mut value = || joined.or_else(|| words.next());
            match flag {
                "-" => given.from_stdin = true,
                "--print" => given.prints.push(value()?),
                "--target" => given.target = Some(value()?),
                "--crate-name" => given.crate_name = Some(value()?),
                "--crate-type" => given.crate_types.extend(value()?.split(',')),
                "--cfg" => given.cfgs.push(value()?),
                "-O" => given.opt_level = Some("2"),
                "-g" => {}
                "-C" | "--codegen" => {
                    let option = value()?;
                    let (name, value) = match option.split_once('=') {
                        Some((name, value)) => (name, Some(value)),
                        None => (option, None),
                    };
                    match name.replace('_', "-").as_str() {
                        "opt-level" => given.opt_level = Some(value?),
                        "debug-assertions" => given.debug_assertions = Some(yes(value)?),
                        "overflow-checks" => given.overflow_checks = Some(yes(value)?),
                        "panic" => given.panic = Some(value?),
                        "extra-filename" => given.extra_filename = true,
                        "target-feature" => {
                            for feature in value?.split(',').filter(|it| !it.is_empty()) {
                                let enable = match feature.as_bytes()[0] {
                                    b'+' => true,
                                    b'-' => false,
                                    _ => return None,
                                };
                                given.features.push((enable, &feature[1..]));
                            }
                        }
                        "target-cpu" => given.target_cpu = Some(value?),
                        name if IGNORED_CODEGEN.contains(&name) => {}
                        _ => return None,
                    }
                }
                "-Z" => match crate::unstable::flag(value()?) {
                    flag if flag.name == "unstable-options" => given.unstable_options = true,
                    // One that changes only how rustc works changes no answer, but may how
                    // it warns.
                    flag if matches!(flag.treatment(), crate::unstable::Treatment::Ignored(_)) => {
                        given.diagnostics_set = true;
                    }
                    _ => return None,
                },
                _ if LINTS.contains(&flag) || IGNORED_VALUED.contains(&flag) => {
                    given.lint(flag, value()?);
                }
                // An input rustc does not read for what it is asked.
                _ if !flag.starts_with('-') => {}
                _ => return None,
            }
        }
        Some(given)
    }

    fn lint(&mut self, flag: &str, value: &str) {
        match (flag, value) {
            ("--cap-lints", "allow") => self.lints_capped = true,
            ("-A" | "--allow", "warnings") => self.warnings_hidden = true,
            ("-W" | "--warn", "warnings") => self.warnings_hidden = false,
            _ if DIAGNOSTICS.contains(&flag) => self.diagnostics_set = true,
            _ => {}
        }
    }
}

/// The variables rustc reads for any question, which blaze answers only when they are unset.
const UNSET: [&str; 2] = ["RUSTC_OVERRIDE_VERSION_STRING", "RUST_TARGET_PATH"];

/// What `--print cfg` prints for `tuple` on `host` with unstable features allowed or not
/// (`nightly`), and the features of `-C target-feature` neither rustc nor LLVM has of it, which
/// change nothing and which rustc warns of, in order. `None` for what `target_cfg` refuses, and
/// for a feature rustc warns of in words blaze does not have.
fn cfg_printed(given: &Given<'_>, tuple: &str, host: &str, nightly: bool) -> Option<(Vec<String>, Vec<String>)> {
    let host_cpu = match given.target_cpu {
        // LLVM's host CPU, for any target of the host's architecture.
        Some("native") => {
            let (name, features) = hir_ty_codegen::host_cpu()?;
            Some(target::HostCpu { arch: target::arch(host).to_owned(), name, features })
        }
        _ => None,
    };
    let args = target::CfgArgs {
        opt_level: given.opt_level.map(str::to_owned),
        debug_assertions: given.debug_assertions,
        overflow_checks: given.overflow_checks,
        panic: given.panic.map(str::to_owned),
        target_cpu: given.target_cpu.map(str::to_owned),
        host_cpu,
        target_features: given.features.iter().map(|&(enable, name)| (enable, name.to_owned())).collect(),
        cfgs: given.cfgs.iter().map(|it| (*it).to_owned()).collect(),
        proc_macro: given.crate_types.contains(&"proc-macro"),
        sanitizer: None,
        stable: !nightly,
    };
    let (cfgs, skipped) = target::target_cfg_told(tuple, &args).ok()?;
    if !skipped.listed.is_empty() {
        return None;
    }
    let unknown = skipped.unknown.iter().map(|it| (*it).to_owned()).collect();
    Some((cfgs, unknown))
}

/// Whether rustc allows unstable features with `RUSTC_BOOTSTRAP` at `bootstrap`, for the crate
/// `--crate-name` names, on a release whose channel allows them or not: rustc's
/// `UnstableFeatures::from_environment`. `1`, or a list of crates that has this one, allows
/// them; `-1` does not; another value, or one that is not text, leaves the channel's default.
/// `None` for compiletest's `-allow-unstable-compiler-flags=`, which allows only the `-Z`
/// flags it names.
fn unstable_allowed(
    bootstrap: Option<OsString>,
    crate_name: Option<&str>,
    channel_nightly: bool,
) -> Option<bool> {
    let Some(bootstrap) = bootstrap.and_then(|it| it.into_string().ok()) else {
        return Some(channel_nightly);
    };
    let listed = |name: &str| bootstrap.split(',').any(|it| it == name);
    Some(match bootstrap.as_str() {
        it if it.starts_with("-allow-unstable-compiler-flags=") => return None,
        "1" => true,
        _ if crate_name.is_some_and(listed) => true,
        "-1" => false,
        _ => channel_nightly,
    })
}

/// The variables rustc is asked with.
#[derive(Clone, Copy)]
pub enum Vars<'a> {
    /// This process's.
    Process,
    /// What the command sets or removes, else this process's.
    Command(&'a std::process::Command),
    /// What the list sets (`Some`) or removes (`None`), the last for a name winning, else this
    /// process's.
    Over(&'a [(OsString, Option<OsString>)]),
    /// These, and no other.
    Only(&'a [(&'a str, &'a str)]),
}

impl Vars<'_> {
    pub fn get(self, name: &str) -> Option<OsString> {
        match self {
            Vars::Process => std::env::var_os(name),
            Vars::Command(command) => match command.get_envs().find(|(it, _)| *it == name) {
                Some((_, value)) => value.map(OsString::from),
                None => std::env::var_os(name),
            },
            Vars::Over(set) => match set.iter().rev().find(|(it, _)| it.as_os_str() == name) {
                Some((_, value)) => value.clone(),
                None => std::env::var_os(name),
            },
            Vars::Only(set) => set.iter().find(|(it, _)| *it == name).map(|(_, value)| OsString::from(*value)),
        }
    }
}

/// What rustc prints, answering.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub stdout: Vec<u8>,
    /// Its warnings, as it shows them to a pipe.
    pub stderr: Vec<u8>,
}

/// What rustc of blaze's release prints to standard output for `args` with `stdin`, with
/// the variables `vars`; `None` for what this does not answer, and for an answer with
/// warnings, which only [`answered`] tells.
pub fn answer(args: &[OsString], stdin: &[u8], vars: Vars<'_>) -> Option<Vec<u8>> {
    answered(args, stdin, vars).filter(|it| it.stderr.is_empty()).map(|it| it.stdout)
}

/// What rustc of blaze's release prints for `args` with `stdin`, with the variables `vars`;
/// `None` for what this does not answer.
pub fn answered(args: &[OsString], stdin: &[u8], vars: Vars<'_>) -> Option<Answer> {
    if UNSET.iter().any(|it| vars.get(it).is_some()) {
        return None;
    }
    let args = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>()?;
    let toolchain = Toolchain::ours()?;
    if !args.is_empty()
        && args.iter().all(|it| matches!(*it, "-V" | "-v" | "-vV" | "--version" | "--verbose"))
    {
        let verbose = args.iter().any(|it| matches!(*it, "-v" | "-vV" | "--verbose"));
        let text = if verbose {
            target::rustc_verbose_version(toolchain.host)
        } else {
            format!("rustc {}\n", target::rustc_version())
        };
        return Some(Answer { stdout: text.into_bytes(), stderr: Vec::new() });
    }
    let given = Given::of(&args)?;
    let version = target::rustc_version();
    let channel_nightly = version.contains("-nightly") || version.contains("-dev");
    let nightly = unstable_allowed(vars.get("RUSTC_BOOTSTRAP"), given.crate_name, channel_nightly)?;
    if given.prints.is_empty() || (given.from_stdin && !stdin.trim_ascii().is_empty()) {
        return None;
    }
    if given.prints.iter().all(|it| WHEN_LINKING.contains(it)) {
        return None;
    }
    let tuple = given.target.unwrap_or(toolchain.host);
    target::known(tuple).ok()?;
    let mut out = String::new();
    let mut dropped: Vec<&str> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for print in &given.prints {
        match *print {
            // Beside another print rustc stops before it links.
            print if WHEN_LINKING.contains(&print) => {}
            "sysroot" => out += &format!("{}\n", sysroot()?.to_str()?),
            "target-libdir" => {
                let dir = sysroot()?.join("lib/rustlib").join(tuple).join("lib");
                out += &format!("{}\n", dir.to_str()?);
            }
            "host-tuple" => out += &format!("{}\n", toolchain.host),
            "split-debuginfo" => {
                target::split_debuginfo(tuple).ok()?.iter().for_each(|it| out += &format!("{it}\n"))
            }
            "crate-name" => out += &format!("{}\n", given.crate_name.unwrap_or("rust_out")),
            "file-names" => {
                if given.extra_filename || !given.from_stdin {
                    return None;
                }
                let name = given.crate_name.unwrap_or("rust_out");
                let mut kinds: Vec<&str> = Vec::new();
                for kind in
                    if given.crate_types.is_empty() { &["bin"][..] } else { &given.crate_types }
                {
                    if !kinds.contains(kind) {
                        kinds.push(kind);
                    }
                }
                // `lib` is an `rlib`, which rustc may not name twice.
                if kinds.contains(&"lib") && kinds.contains(&"rlib") {
                    return None;
                }
                // A kind the target does not have is dropped with a warning, which cargo reads
                // to tell it from the others.
                for kind in &kinds {
                    match target::file_name(tuple, kind, name).ok()? {
                        Some(file) => out += &format!("{file}\n"),
                        None => dropped.push(*kind),
                    }
                }
                if dropped.len() == kinds.len()
                    || dropped
                        .iter()
                        .any(|it| given.crate_types.iter().filter(|k| *k == it).count() > 1)
                {
                    return None;
                }
            }
            "cfg" => {
                let (cfgs, unlisted) = cfg_printed(&given, tuple, toolchain.host, nightly)?;
                unknown = unlisted;
                cfgs.iter().for_each(|it| out += &format!("{it}\n"));
            }
            // Apple's SDKROOT and *_DEPLOYMENT_TARGET, which a shell of nixpkgs on macOS sets,
            // are read to link: an Apple target's spec names no deployment target
            // (`"llvm-target": "arm64-apple-macosx"`).
            "target-spec-json" if given.unstable_options && nightly => {
                out += target::target_spec_json(tuple, toolchain.host).ok()??;
            }
            _ => return None,
        }
    }
    let mut warned = String::new();
    let count = unknown.len() + dropped.len();
    if count > 0 && !given.lints_capped && !given.warnings_hidden {
        if given.diagnostics_set || (!unknown.is_empty() && !dropped.is_empty()) {
            return None;
        }
        for feature in &unknown {
            warned += &target::unknown_feature_warning(feature)?;
        }
        for kind in &dropped {
            warned += &format!(
                "warning: dropping unsupported crate type `{kind}` for target `{tuple}`\n\n"
            );
        }
        warned +=
            &format!("warning: {count} warning{} emitted\n\n", if count == 1 { "" } else { "s" });
    }
    Some(Answer { stdout: out.into_bytes(), stderr: warned.into_bytes() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `--cfg` is printed back when it is a plain name of no builtin cfg, with a value with
    /// nothing to escape; another is not answered.
    #[test]
    fn cfgs_given() {
        let printed = |cfg: &str| {
            let args: Vec<OsString> = ["--print", "cfg", "--cfg", cfg].iter().map(OsString::from).collect();
            answered(&args, &[], Vars::Only(&[]))
                .map(|it| String::from_utf8(it.stdout).unwrap().lines().any(|line| line == cfg))
        };
        assert_eq!(printed("foo"), Some(true));
        assert_eq!(printed("feature=\"serde derive\""), Some(true));
        for refused in [
            "feature=serde",
            "feature=\"a\\\"b\"",
            "debug_assertions",
            "target_os=\"linux\"",
            "sanitizer_cfi_generalize_pointers",
            "9lives",
        ] {
            assert_eq!(printed(refused), None, "{refused}");
        }
    }

    /// A feature neither rustc nor LLVM has of the target changes nothing, with rustc's
    /// warning; one LLVM lists, whose warning blaze does not have, is not answered.
    #[test]
    fn a_feature_the_target_lacks_is_warned_of() {
        let plain = told("--print cfg --target x86_64-unknown-linux-gnu").expect("an answer");
        let unknown = told("--print cfg --target x86_64-unknown-linux-gnu -C target-feature=+blazeunlisted")
            .expect("an answer with a warning");
        assert_eq!(unknown.stdout, plain.stdout);
        let warning = target::unknown_feature_warning("blazeunlisted").unwrap();
        assert_eq!(String::from_utf8(unknown.stderr).unwrap(), format!("{warning}warning: 1 warning emitted\n\n"));
        assert!(told("--print cfg --target x86_64-unknown-linux-gnu -C target-feature=+64bit").is_none());
        let hidden = told("--print cfg --target x86_64-unknown-linux-gnu -C target-feature=+blazeunlisted -A warnings");
        assert_eq!(hidden.map(|it| (it.stdout, it.stderr)), Some((plain.stdout, Vec::new())));
    }

    #[test]
    fn arguments_read() {
        let args =
            ["-Copt-level=3", "-C", "panic=abort", "--print=cfg", "--cfg", "foo", "-Wwarnings"];
        let given = Given::of(&args).unwrap();
        assert_eq!(given.opt_level, Some("3"));
        assert_eq!(given.panic, Some("abort"));
        assert_eq!(given.prints, ["cfg"]);
        assert_eq!(given.cfgs, ["foo"]);
        assert_eq!(given.debug_assertions, None);
        assert!(Given::of(&["-O"]).is_some_and(|it| it.opt_level == Some("2")));
        assert!(Given::of(&["-C", "debug_assertions=off"]).is_some_and(|it| it.debug_assertions == Some(false)));
        assert!(Given::of(&["-C", "debug-assertions=maybe"]).is_none());
        // The features and the CPU, in order, which `target_cfg` answers from rustc's.
        let given =
            Given::of(&["-C", "target-cpu=x86-64-v3", "-Ctarget-feature=-crt-static,+avx2"]);
        let given = given.unwrap();
        assert_eq!(given.target_cpu, Some("x86-64-v3"));
        assert_eq!(given.features, [(false, "crt-static"), (true, "avx2")]);
        assert!(Given::of(&["-Ctarget-feature=avx2", "--print", "cfg"]).is_none());
        assert!(Given::of(&["-Z", "ub-checks", "--print", "cfg"]).is_none());
        assert!(Given::of(&["-Zthreads=8", "--print", "cfg"]).is_some());
        // cargo's `-Clinker-features=-lld` is how rustc links (rust-analyzer's `cargo rustc
        // --print cfg`, cargo's question of its targets).
        assert!(Given::of(&["-Clinker-features=-lld", "-O", "--print", "cfg"]).is_some());
        // zed's rustflags.
        let zed = ["-C", "link-arg=-fuse-ld=lld", "-C", "symbol-mangling-version=v0"];
        let zed = [&zed[..], &["--cfg", "tokio_unstable", "--print", "cfg"]].concat();
        let given = Given::of(&zed).unwrap();
        assert_eq!(given.cfgs, ["tokio_unstable"]);
    }

    fn told(args: &str) -> Option<Answer> {
        let args: Vec<OsString> = args.split_whitespace().map(OsString::from).collect();
        answered(&args, &[], Vars::Only(&[]))
    }

    /// uitest.py's RUSTFLAGS: a print of what is linked and kept files change nothing that is
    /// printed when nothing links; `--cfg` adds its cfg.
    #[test]
    fn flags_of_a_link_are_answered() {
        let plain = told("--target x86_64-unknown-linux-gnu --print cfg").expect("an answer");
        let ui = told(
            "--print link-args -C save-temps -C link-dead-code --cap-lints allow \
             -L native=/ui/native --cfg e2024 --target x86_64-unknown-linux-gnu --print cfg",
        )
        .expect("an answer under uitest.py's flags");
        let mut cfgs: Vec<&str> = std::str::from_utf8(&plain.stdout).unwrap().lines().collect();
        cfgs.push("e2024");
        cfgs.sort();
        assert_eq!(std::str::from_utf8(&ui.stdout).unwrap().lines().collect::<Vec<_>>(), cfgs);
        assert!(told("--print link-args").is_none());
    }

    /// cargo's probe of a target that has no `dylib` or `proc-macro`: rustc drops them with
    /// a warning, which cargo reads.
    #[test]
    fn kinds_a_target_lacks_are_dropped_with_a_warning() {
        let probe = "- --crate-name ___ --print=file-names --target wasm32-wasip1 \
                     --crate-type bin --crate-type rlib --crate-type dylib --crate-type cdylib \
                     --crate-type staticlib --crate-type proc-macro --print=crate-name -Wwarnings";
        let said = told(probe).expect("an answer of the probe");
        assert_eq!(
            std::str::from_utf8(&said.stdout).unwrap(),
            "___.wasm\nlib___.rlib\n___.wasm\nlib___.a\n___\n"
        );
        assert_eq!(
            std::str::from_utf8(&said.stderr).unwrap(),
            "warning: dropping unsupported crate type `dylib` for target `wasm32-wasip1`\n\n\
             warning: dropping unsupported crate type `proc-macro` for target `wasm32-wasip1`\n\n\
             warning: 2 warnings emitted\n\n"
        );
        let args: Vec<OsString> = probe.split_whitespace().map(OsString::from).collect();
        assert_eq!(answer(&args, &[], Vars::Only(&[])), None);
        for hidden in ["-A warnings", "--cap-lints allow", "-Wwarnings -Awarnings"] {
            let said = told(&format!("{probe} {hidden}")).expect("an answer with no warnings");
            assert_eq!(said.stderr, b"");
        }
        assert!(told(&format!("{probe} -D warnings")).is_none());
        assert!(
            told(&format!("{probe} -A warnings -W warnings"))
                .is_some_and(|it| !it.stderr.is_empty())
        );
        // A proc macro of a target that links statically by default has no `crt-static`.
        let static_of = |kinds: &str| {
            let said = told(&format!("--print cfg --target x86_64-unknown-linux-musl {kinds}"));
            String::from_utf8(said.unwrap().stdout).unwrap().contains("\"crt-static\"")
        };
        assert!(static_of("--crate-type bin"));
        assert!(!static_of("--crate-type bin --crate-type proc-macro"));
        assert!(static_of("--crate-type proc-macro -C target-feature=+crt-static"));
    }

    #[test]
    fn a_spec_is_answered_in_a_shell_of_the_sdk() {
        let spec = |target: &str, sdk: bool| {
            let args =
                ["-Z", "unstable-options", "--print", "target-spec-json", "--target", target];
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let vars: &[(&str, &str)] = match sdk {
                true => &[("SDKROOT", "/sdk"), ("MACOSX_DEPLOYMENT_TARGET", "14.0")],
                false => &[],
            };
            answer(&args, &[], Vars::Only(vars))
        };
        let wasm = spec("wasm32-wasip1", true).expect("wasm32-wasip1's spec under SDKROOT");
        assert!(String::from_utf8(wasm).unwrap().contains("\"arch\": \"wasm32\""));
        let apple = spec("aarch64-apple-darwin", true).expect("an Apple spec under SDKROOT");
        assert_eq!(Some(apple), spec("aarch64-apple-darwin", false));
    }

    #[test]
    fn each_kind_of_vars_reads_its_own_then_the_process() {
        let unset = format!("BLAZE_OWN_VARS_TEST_{}", std::process::id());
        let path = std::env::var_os("PATH");
        assert_eq!(Vars::Process.get("PATH"), path);
        assert_eq!(Vars::Process.get(&unset), None);
        let mut command = std::process::Command::new("rustc");
        command.env(&unset, "set").env_remove("PATH");
        assert_eq!(Vars::Command(&command).get(&unset), Some("set".into()));
        assert_eq!(Vars::Command(&command).get("PATH"), None);
        assert_eq!(Vars::Command(&std::process::Command::new("rustc")).get("PATH"), path);
        let over = [
            (OsString::from(&unset), Some(OsString::from("first"))),
            (OsString::from(&unset), Some(OsString::from("last"))),
            (OsString::from("PATH"), None),
        ];
        assert_eq!(Vars::Over(&over).get(&unset), Some("last".into()));
        assert_eq!(Vars::Over(&over).get("PATH"), None);
        assert_eq!(Vars::Over(&[]).get("PATH"), path);
        let only = [("A", "1")];
        assert_eq!(Vars::Only(&only).get("A"), Some("1".into()));
        assert_eq!(Vars::Only(&only).get("PATH"), None);
    }

    /// servo's `[env]` has `RUSTC_BOOTSTRAP` a list of its crates, which every build script
    /// that asks `rustc --version` (libc's, rustversion's) is run with.
    #[test]
    fn a_list_of_crates_bootstrapped_is_rustcs() {
        let told = |args: &str, bootstrap: &str| {
            let args: Vec<OsString> = args.split_whitespace().map(OsString::from).collect();
            answered(&args, &[], Vars::Only(&[("RUSTC_BOOTSTRAP", bootstrap)]))
        };
        let servo = "crown,script,mozjs_sys";
        for args in ["--version", "-vV"] {
            assert!(told(args, servo).is_some(), "{args}");
            assert_eq!(told(args, servo), told(args, "1"), "{args}");
        }
        let cfg = |name: &str, bootstrap: &str| {
            let asked = format!("--crate-name {name} --print cfg -C debug-assertions=on");
            let said = told(&asked, bootstrap).expect("--print cfg");
            String::from_utf8(said.stdout).unwrap()
        };
        assert_eq!(cfg("script", servo), cfg("script", "1"));
        assert_eq!(cfg("libc", servo), cfg("libc", "1"));
        assert_eq!(cfg("libc", "1,"), cfg("libc", "1"));
        assert!(cfg("libc", "-1").len() < cfg("libc", "1").len());
        assert_eq!(cfg("script", "-1,script"), cfg("script", "1"));
        let compiletest = told("--print cfg", "-allow-unstable-compiler-flags=threads");
        assert!(compiletest.is_none());

        assert_eq!(unstable_allowed(Some("script".into()), Some("script"), false), Some(true));
        assert_eq!(unstable_allowed(Some("script".into()), Some("libc"), false), Some(false));
        assert_eq!(unstable_allowed(Some("script".into()), None, true), Some(true));
        assert_eq!(unstable_allowed(Some("-1".into()), Some("-1"), true), Some(true));
        assert_eq!(unstable_allowed(None, None, false), Some(false));
    }

    #[test]
    fn a_spec_the_hosts_agree_on_is_answered_on_any_host() {
        let spec = |triple: &str, host: &str| target::target_spec_json(triple, host).unwrap();
        let windows = spec("wasm32-wasip1", "x86_64-pc-windows-msvc").expect("wasm32-wasip1's spec");
        assert_eq!(Some(windows), spec("wasm32-wasip1", "aarch64-apple-darwin"));
        assert_eq!(spec("x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"), None);
    }

    /// The print of a target blaze has no data of is not answered, even one that needs none.
    #[test]
    fn a_target_blaze_has_no_data_of_is_not_answered() {
        assert!(told("--print crate-name --target riscv64gc-unknown-plan9").is_none());
        assert!(told("--print crate-name --target x86_64-unknown-linux-gnu").is_some());
    }
}
