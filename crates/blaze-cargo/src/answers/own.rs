//! blaze's answers to rustc's questions, from what it knows of its own release of rustc and of
//! the targets that release has: no rustc runs. The data of the release is what its rustc
//! printed for every target blaze has (`harness/answers.py --write`, when the pin moves), and
//! the cfgs that follow from the arguments are added here, as rustc adds them. A question this
//! does not understand whole is answered `None`, and fails.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde_derive::Deserialize;

/// What the rustc of the toolchain in `harness/toolchain` says (`harness/answers.py --write`
/// writes it when the toolchain moves).
const RELEASES: [&str; 1] = [include_str!("rustc.json")];

#[derive(Deserialize)]
struct Release {
    /// As the first line of `-V` has it, after `rustc `.
    version: String,
    /// What `-vV` prints, with `{host}` for the host's tuple.
    verbose: String,
    /// The warning of `-C target-feature=+{feature}` of a feature neither rustc nor LLVM has
    /// of the target, with `{feature}` for its name.
    #[serde(default)]
    unknown_feature: Option<String>,
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
struct Target {
    /// The cfgs of the target, but those the arguments decide.
    cfg: Vec<String>,
    /// The cfgs printed besides those when unstable features are allowed.
    nightly_cfg: Vec<String>,
    /// The strategy of `-C panic` when none is given.
    panic: String,
    split_debuginfo: Vec<String>,
    /// The name of the file of each kind of crate named `___`; `None` for a kind the target
    /// does not have.
    file_names: BTreeMap<String, Option<String>>,
    /// By the host rustc runs on, whose build decides some of it.
    target_spec_json: BTreeMap<String, String>,
    /// The `target_feature`s each feature rustc has turns on beside the target's own, with
    /// itself: `-C target-feature=+<name>` on the nightly. One rustc warns of is not here.
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    /// The same of the features only LLVM names, which `target-cpu=native` turns on.
    #[serde(default)]
    llvm_features: BTreeMap<String, Vec<String>>,
    /// Of each feature the target has, the features turning it off turns off: itself and
    /// those that need it.
    #[serde(default)]
    features_off: BTreeMap<String, Vec<String>>,
    /// Each CPU of `-C target-cpu`, by the index of its `target_feature`s in `cpu_features`.
    #[serde(default)]
    cpus: BTreeMap<String, usize>,
    #[serde(default)]
    cpu_features: Vec<Vec<String>>,
    /// The cfgs besides `target_feature` a feature has: wasm's `atomics`, `target_has_threads`.
    #[serde(default)]
    feature_cfgs: BTreeMap<String, Vec<String>>,
    /// The other features rustc or LLVM lists of the target (`--print target-features`).
    /// One of none of these turns nothing on or off, with a warning.
    #[serde(default)]
    other_features: Vec<String>,
}

impl Target {
    /// Whether `-C target-feature=crt-static` changes how the target links, as its spec says.
    fn respects_crt_static(&self) -> Option<bool> {
        let spec: serde_json::Value =
            serde_json::from_str(self.target_spec_json.values().next()?).ok()?;
        Some(spec.get("crt-static-respected").and_then(|it| it.as_bool()).unwrap_or(false))
    }

    /// The spec rustc on `host` prints: its own, or on a host not recorded (Windows) the one
    /// every recorded host prints. A spec the hosts differ on (the -linux-gnu ones, whose own
    /// rustc links with its lld) is the build's, so not known for another host.
    fn spec_on(&self, host: &str) -> Option<&str> {
        if let Some(spec) = self.target_spec_json.get(host) {
            return Some(spec);
        }
        let mut specs = self.target_spec_json.values();
        let spec = specs.next()?;
        specs.all(|it| it == spec).then_some(spec)
    }

    /// Whether rustc or LLVM lists `name` as a feature of the target.
    fn lists(&self, name: &str) -> bool {
        self.features.contains_key(name)
            || self.llvm_features.contains_key(name)
            || self.features_off.contains_key(name)
            || self.other_features.iter().any(|it| it == name)
    }

    /// The `target_feature`s of `cpu` with `features` turned on (`true`) and off in order,
    /// starting from `have`, the target's own, and the features of `features` neither rustc
    /// nor LLVM has, which change nothing and which rustc warns of, in order; `None` for a
    /// CPU or feature rustc warns of otherwise or does not have, and where what is listed is
    /// not recorded.
    fn tuned<'a>(
        &self,
        have: Vec<String>,
        cpu: Option<(&str, Option<&str>)>,
        features: &[(bool, &'a str)],
    ) -> Option<(Vec<String>, Vec<&'a str>)> {
        let own: Vec<String> = have.clone();
        let mut have: std::collections::BTreeSet<String> = match cpu {
            Some((name, _)) => {
                self.cpu_features.get(*self.cpus.get(name)?)?.iter().cloned().collect()
            }
            None => have.into_iter().collect(),
        };
        // `target-cpu=native`: LLVM's features of the host, which name what rustc does not too.
        let host: Vec<(bool, String)> = cpu
            .and_then(|(_, host)| host)
            .map(|it| {
                it.split(',')
                    .filter_map(|it| Some((it.starts_with('+'), it.get(1..)?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let on = |name: &str| self.features.get(name).or_else(|| self.llvm_features.get(name));
        let mut apply = |enable: bool, name: &str, host: bool| -> Option<()> {
            match (enable, on(name)) {
                (true, Some(turned)) => {
                    have.extend(turned.iter().cloned());
                    // The target's own features it needs, should they be off.
                    have.extend(
                        self.features_off
                            .iter()
                            .filter(|(_, off)| off.iter().any(|it| it == name))
                            .map(|(it, _)| it.clone()),
                    );
                }
                (false, _) if self.features_off.contains_key(name) => {
                    self.features_off[name].iter().for_each(|it| _ = have.remove(it));
                }
                (false, Some(turned)) if !own.iter().any(|it| it == name) => {
                    // Off with it go the features that turn it on.
                    let needs = |other: &Vec<String>| turned.iter().all(|it| other.contains(it));
                    if !turned.is_empty() {
                        have.retain(|it| !self.features.get(it).is_some_and(needs));
                    }
                }
                // A feature only LLVM has, of no `target_feature`, changes none of them.
                _ if host => {}
                _ => return None,
            }
            Some(())
        };
        for (enable, name) in &host {
            apply(*enable, name, true)?;
        }
        let mut unknown = Vec::new();
        for &(enable, name) in features {
            if !self.other_features.is_empty() && !self.lists(name) {
                unknown.push(name);
                continue;
            }
            apply(enable, name, false)?;
        }
        Some((have.into_iter().collect(), unknown))
    }
}

fn releases() -> &'static [Release] {
    static RELEASES_READ: OnceLock<Vec<Release>> = OnceLock::new();
    RELEASES_READ.get_or_init(|| {
        RELEASES
            .iter()
            .map(|it| serde_json::from_str(it).expect("the answers of a release"))
            .collect()
    })
}

/// blaze's own release, the one every rustc is answered as: its data, and the host this blaze
/// runs on. Where its files are ([`sysroot`]) is looked for only when asked.
struct Toolchain {
    host: &'static str,
    release: &'static Release,
}

impl Toolchain {
    fn ours() -> Option<Toolchain> {
        let version = crate::cargo::toolchain::ours_says();
        let release = releases().iter().find(|it| it.version == version)?;
        Some(Toolchain { host: crate::cargo::toolchain::host()?, release })
    }
}

/// Where blaze's release is, found with nothing run.
fn sysroot() -> Option<PathBuf> {
    crate::cargo::toolchain::sysroot()
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

    fn debug_assertions(&self) -> bool {
        self.debug_assertions.unwrap_or(matches!(self.opt_level, None | Some("0")))
    }
}

/// The architecture of a target's tuple: what is before its first `-`.
fn arch(tuple: &str) -> &str {
    tuple.split('-').next().unwrap_or(tuple)
}

/// The variables rustc reads for any question, which blaze answers only when they are unset.
const UNSET: [&str; 2] = ["RUSTC_OVERRIDE_VERSION_STRING", "RUST_TARGET_PATH"];

/// The cfgs rustc sets itself, which it refuses as `--cfg`, by name or by what it starts with.
const BUILTIN_CFGS: [&str; 13] = [
    "debug_assertions",
    "overflow_checks",
    "ub_checks",
    "contract_checks",
    "fmt_debug",
    "relocation_model",
    "panic",
    "unix",
    "windows",
    "test",
    "emscripten_wasm_eh",
    "sanitize",
    "sanitizer_",
];

/// Whether `cfg` is `name` or `name="value"` as rustc prints it back: a plain name of no
/// builtin cfg, a value with nothing to escape.
fn is_plain_cfg(cfg: &str) -> bool {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (cfg, None),
    };
    let plain_name = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let builtin = name.starts_with("target_")
        || BUILTIN_CFGS.iter().any(|it| name == *it || (it.ends_with('_') && name.starts_with(it)));
    let plain_value = value.is_none_or(|value| {
        let inner = value.strip_prefix('"').and_then(|it| it.strip_suffix('"'));
        inner.is_some_and(|it| {
            it.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ')
        })
    });
    plain_name && !builtin && plain_value
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

/// What rustc prints, answering.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub stdout: Vec<u8>,
    /// Its warnings, as it shows them to a pipe.
    pub stderr: Vec<u8>,
}

/// What rustc of blaze's release prints to standard output for `args` with `stdin`, with
/// the variables `var` reads; `None` for what this does not answer, and for an answer with
/// warnings, which only [`answered`] tells.
pub fn answer(
    args: &[OsString],
    stdin: &[u8],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<Vec<u8>> {
    answered(args, stdin, var).filter(|it| it.stderr.is_empty()).map(|it| it.stdout)
}

/// What rustc of blaze's release prints for `args` with `stdin`, with the variables `var`
/// reads; `None` for what this does not answer.
pub fn answered(
    args: &[OsString],
    stdin: &[u8],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<Answer> {
    if UNSET.iter().any(|it| var(it).is_some()) {
        return None;
    }
    let args = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>()?;
    let toolchain = Toolchain::ours()?;
    let release = toolchain.release;
    if !args.is_empty()
        && args.iter().all(|it| matches!(*it, "-V" | "-v" | "-vV" | "--version" | "--verbose"))
    {
        let verbose = args.iter().any(|it| matches!(*it, "-v" | "-vV" | "--verbose"));
        let text = if verbose {
            release.verbose.replace("{host}", toolchain.host)
        } else {
            format!("rustc {}\n", release.version)
        };
        return Some(Answer { stdout: text.into_bytes(), stderr: Vec::new() });
    }
    let given = Given::of(&args)?;
    let channel_nightly = release.version.contains("-nightly") || release.version.contains("-dev");
    let nightly = unstable_allowed(var("RUSTC_BOOTSTRAP"), given.crate_name, channel_nightly)?;
    if given.prints.is_empty() || (given.from_stdin && !stdin.trim_ascii().is_empty()) {
        return None;
    }
    if given.prints.iter().all(|it| WHEN_LINKING.contains(it)) {
        return None;
    }
    let tuple = given.target.unwrap_or(toolchain.host);
    let target = release.targets.get(tuple)?;
    let mut out = String::new();
    let mut dropped: Vec<&str> = Vec::new();
    let mut unknown: Vec<&str> = Vec::new();
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
                target.split_debuginfo.iter().for_each(|it| out += &format!("{it}\n"))
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
                    match target.file_names.get(*kind)? {
                        Some(file) => out += &format!("{}\n", file.replace("___", name)),
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
                let mut cfgs: Vec<String> = target.cfg.clone();
                if nightly {
                    cfgs.extend(target.nightly_cfg.iter().cloned());
                }
                // Whether a target respects `crt-static` is its spec's, so only asking for
                // what it has already is answered.
                let static_default = cfgs.iter().any(|it| it == "target_feature=\"crt-static\"");
                // Only a target's own `crt-static`, asked for again, changes nothing.
                let as_it_is = given
                    .features
                    .iter()
                    .all(|&(enable, name)| name == "crt-static" && enable == static_default);
                if given.target_cpu.is_some() || !as_it_is {
                    // The features rustc has without unstable ones allowed are not recorded.
                    if !nightly {
                        return None;
                    }
                    let cpu = match given.target_cpu {
                        // LLVM's host CPU, for any target of the host's architecture.
                        Some("native") if arch(tuple) == arch(toolchain.host) => {
                            let (name, features) = hir_ty_codegen::host_cpu()?;
                            Some((name, Some(features)))
                        }
                        Some("native") => return None,
                        Some(name) => Some((name.to_owned(), None)),
                        None => None,
                    };
                    let feature = |it: &String| {
                        let name = it.strip_prefix("target_feature=\"")?.strip_suffix('"')?;
                        Some(name.to_owned())
                    };
                    let own = cfgs.iter().filter_map(feature).collect();
                    let cpu = cpu.as_ref().map(|(name, host)| (name.as_str(), host.as_deref()));
                    let (tuned, unlisted) = target.tuned(own, cpu, &given.features)?;
                    unknown = unlisted;
                    cfgs.retain(|it| feature(it).is_none());
                    for feature in &tuned {
                        cfgs.extend(
                            target.feature_cfgs.get(feature).into_iter().flatten().cloned(),
                        );
                    }
                    cfgs.extend(tuned.iter().map(|it| format!("target_feature=\"{it}\"")));
                }
                let debug_assertions = given.debug_assertions();
                if debug_assertions {
                    cfgs.push("debug_assertions".to_owned());
                }
                if nightly && given.overflow_checks.unwrap_or(debug_assertions) {
                    cfgs.push("overflow_checks".to_owned());
                }
                if nightly && debug_assertions {
                    cfgs.push("ub_checks".to_owned());
                }
                let panic = given.panic.unwrap_or(&target.panic);
                if panic != target.panic && panic != "abort" {
                    return None;
                }
                cfgs.push(format!("panic=\"{panic}\""));
                if given.crate_types.contains(&"proc-macro") {
                    cfgs.push("proc_macro".to_owned());
                    // A proc macro is a dynamic library, which a target that respects
                    // `crt-static` builds without it unless it is asked for.
                    if !given.features.iter().any(|&(_, name)| name == "crt-static")
                        && target.respects_crt_static()?
                    {
                        cfgs.retain(|it| it != "target_feature=\"crt-static\"");
                    }
                }
                for cfg in &given.cfgs {
                    if !is_plain_cfg(cfg) {
                        return None;
                    }
                    cfgs.push((*cfg).to_owned());
                }
                cfgs.sort();
                cfgs.dedup();
                cfgs.iter().for_each(|it| out += &format!("{it}\n"));
            }
            // Apple's SDKROOT and *_DEPLOYMENT_TARGET, which a shell of nixpkgs on macOS sets,
            // are read to link: an Apple target's spec names no deployment target
            // (`"llvm-target": "arm64-apple-macosx"`).
            "target-spec-json" if given.unstable_options && nightly => {
                out += target.spec_on(toolchain.host)?;
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
            warned += &release.unknown_feature.as_ref()?.replace("{feature}", feature);
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

    #[test]
    fn cfgs_given() {
        assert!(is_plain_cfg("foo"));
        assert!(is_plain_cfg("feature=\"serde derive\""));
        assert!(!is_plain_cfg("feature=serde"));
        assert!(!is_plain_cfg("feature=\"a\\\"b\""));
        assert!(!is_plain_cfg("debug_assertions"));
        assert!(!is_plain_cfg("target_os=\"linux\""));
        assert!(!is_plain_cfg("sanitizer_cfi_generalize_pointers"));
        assert!(!is_plain_cfg("9lives"));
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
        assert!(!given.debug_assertions());
        assert!(Given::of(&["-O"]).is_some_and(|it| !it.debug_assertions()));
        assert!(Given::of(&["-C", "opt-level=0"]).is_some_and(|it| it.debug_assertions()));
        // The features and the CPU, in order, which `Target::tuned` answers from rustc's.
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
        answered(&args, &[], &|_| None)
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
        assert_eq!(answer(&args, &[], &|_| None), None);
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
            let var = |name: &str| {
                (sdk && matches!(name, "SDKROOT" | "MACOSX_DEPLOYMENT_TARGET"))
                    .then(|| OsString::from(if name == "SDKROOT" { "/sdk" } else { "14.0" }))
            };
            answer(&args, &[], &var)
        };
        let wasm = spec("wasm32-wasip1", true).expect("wasm32-wasip1's spec under SDKROOT");
        assert!(String::from_utf8(wasm).unwrap().contains("\"arch\": \"wasm32\""));
        let apple = spec("aarch64-apple-darwin", true).expect("an Apple spec under SDKROOT");
        assert_eq!(Some(apple), spec("aarch64-apple-darwin", false));
    }

    /// servo's `[env]` has `RUSTC_BOOTSTRAP` a list of its crates, which every build script
    /// that asks `rustc --version` (libc's, rustversion's) is run with.
    #[test]
    fn a_list_of_crates_bootstrapped_is_rustcs() {
        let told = |args: &str, bootstrap: &str| {
            let args: Vec<OsString> = args.split_whitespace().map(OsString::from).collect();
            answered(&args, &[], &|name| (name == "RUSTC_BOOTSTRAP").then(|| bootstrap.into()))
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
        let release = &releases()[0];
        let wasm = &release.targets["wasm32-wasip1"];
        let windows = wasm.spec_on("x86_64-pc-windows-msvc").expect("wasm32-wasip1's spec");
        assert_eq!(Some(windows), wasm.spec_on("aarch64-apple-darwin"));
        let linux = &release.targets["x86_64-unknown-linux-gnu"];
        assert_eq!(linux.spec_on("x86_64-pc-windows-msvc"), None);
    }
}
