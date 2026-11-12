//! What a test asks of blaze: from its directives, one of "run it like this and expect that",
//! "not for this target" (the test's own `only-`, `ignore-` and `needs-` say so) or "not
//! supported" with the directive or flag that has no blaze meaning yet. Pure: the test's text
//! and its auxiliaries' text in (through `read`), a [`Planned`] out. Nothing is dropped
//! quietly: a directive is in exactly one of the tables below, and one in none is `unknown
//! directive`, which the report lists.

use super::directives::{Annotation, Level, Parsed};
use blaze_script::regex::Regex;

/// The suites of rustc's `tests/` blaze runs (docs/compiletest.md says why these).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Suite {
    Ui,
    Incremental,
    Crashes,
}

impl Suite {
    pub(crate) const ALL: [Suite; 3] = [Suite::Ui, Suite::Incremental, Suite::Crashes];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Suite::Ui => "ui",
            Suite::Incremental => "incremental",
            Suite::Crashes => "crashes",
        }
    }

    pub(crate) fn of(name: &str) -> Option<Suite> {
        Suite::ALL.into_iter().find(|it| it.name() == name)
    }
}

/// The machine blaze builds for, in the words the conditions use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Host {
    /// `aarch64`, `x86_64`.
    pub arch: String,
    /// `linux`, `macos`.
    pub os: String,
}

impl Host {
    pub(crate) fn triple(&self) -> String {
        match self.os.as_str() {
            "macos" => format!("{}-apple-darwin", self.arch),
            _ => format!("{}-unknown-linux-gnu", self.arch),
        }
    }

    /// Every condition word that is true of it.
    fn truths(&self) -> Vec<String> {
        let mut out = vec![self.arch.clone(), self.os.clone(), self.triple(), "unix".to_owned(), "64bit".to_owned()];
        match self.os.as_str() {
            "macos" => out.push("apple".to_owned()),
            _ => out.extend(["gnu", "elf"].map(str::to_owned)),
        }
        out
    }
}

/// How the program must end, for a test that expects it to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ends {
    /// An exit status of 1 to 127.
    Fail,
    /// Anything else that is not success: a signal.
    Crash,
    Either,
}

/// What must come of the test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    CheckPass,
    BuildPass,
    RunPass,
    /// blaze must refuse it; with `build`, a build is tried where the check accepts it (an
    /// error only code generation or the link finds).
    CompileFail { build: bool },
    RunFail(Ends),
    /// `tests/crashes`, and `known-bug` tests: the input crashes rustc or shows a bug of it, so
    /// rustc's answer is no expectation. blaze may accept or refuse; it may not panic or hang.
    Survive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrateKind {
    Bin,
    Lib,
    ProcMacro,
    /// `dylib`, `cdylib`, `staticlib`: given to blaze as the manifest's `crate-type`.
    Other(String),
}

/// One package of a test: the test's own file, or an auxiliary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Crate {
    /// The crate's name, and the package's.
    pub name: String,
    /// Its source, as a path under the suite's directory.
    pub file: String,
    pub edition: String,
    pub kind: CrateKind,
    /// What goes to this package's `rustflags` (`--cfg`, `--check-cfg`, lint levels).
    pub rustflags: Vec<String>,
    /// Lines of `[profile.dev.package.<name>]`: `opt-level`, `debug`, `overflow-checks`,
    /// `debug-assertions`.
    pub profile: Vec<String>,
    /// The crates it depends on: (the name it uses, the package).
    pub deps: Vec<(String, String)>,
    /// Whether it names files beside it (`mod x;`, `#[path]`, `include!`), so they are placed
    /// beside its copy.
    pub siblings: bool,
}

/// What is checked of the program when it runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Run {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The path under the suite of the file its standard output must equal (`check-run-results`).
    pub stdout: Option<String>,
    /// Text its output must hold (`error-pattern` of a test whose program fails: the program's
    /// own words, a panic message of the standard library both compilers build).
    pub patterns: Vec<String>,
    pub regex_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub kind: Kind,
    pub main: Crate,
    /// The auxiliaries, each before any that depends on it.
    pub aux: Vec<Crate>,
    /// `-C panic=abort`: the profile's, so the whole build's.
    pub panic_abort: bool,
    pub run: Run,
    /// The `ERROR` and `WARN` annotations that hold for the revision.
    pub annotations: Vec<Annotation>,
    /// What the test also expects that the runner does not hold, by name (`run-rustfix`, `NOTE
    /// annotations`): counted in the report, so "pass" never claims them.
    pub not_held: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Planned {
    Run(Box<Plan>),
    /// The test's own conditions say it is not for this target.
    Ignored(String),
    /// A directive or flag with no blaze meaning yet, or one the runner has no place for.
    Unsupported(String),
}

/// Directives that change nothing blaze does or the runner checks: how rustc's output is
/// normalized and compared, how compiletest links auxiliaries, which of its own modes skip the
/// test.
const NO_MEANING: &[&str] = &[
    "revisions",
    "normalize-stderr",
    "normalize-stderr-32bit",
    "normalize-stderr-64bit",
    "dont-check-compiler-stderr",
    "dont-check-compiler-stdout",
    "dont-check-failure-status",
    "failure-status",
    "stderr-per-bitwidth",
    "compare-output-by-lines",
    "dont-require-annotations",
    "reference",
    "no-prefer-dynamic",
    "force-host",
    "forbid-output",
    "unused-revision-names",
    "no-pass-override",
    "ignore-pass",
    "incremental",
    "ignore-compare-mode-next-solver",
    "ignore-compare-mode-polonius",
    "ignore-parallel-frontend",
    "ignore-rustc-debug-assertions",
    "ignore-stage1",
    "ignore-stage2",
    "ignore-coverage-map",
    "ignore-coverage-run",
    "ignore-gdb",
    "ignore-lldb",
    "ignore-cdb",
    "ignore-gdb-version",
    "ignore-enzyme",
    "ignore-pauthtest",
    "ignore-cross-compile",
    "ignore-remote",
    "ignore-stable",
    "ignore-beta",
    "only-nightly",
    "needs-unwind",
    "needs-threads",
    "needs-subprocess",
    "needs-asm-support",
    "needs-asm-ret",
    "needs-dynamic-linking",
    "needs-crate-type",
    "needs-deterministic-layouts",
    "needs-relocation-model-pic",
    "needs-run-enabled",
    "needs-symlink",
    "needs-target-has-atomic",
    "needs-target-std",
    "only-wasm-proc-macros",
];

/// Directives the runner does not hold but which change no verdict it gives: named in the
/// plan's `not_held`.
const NOT_HELD: &[&str] = &["run-rustfix", "rustfix-only-machine-applicable", "check-stdout", "normalize-stdout"];

/// Directives that mean the test is not for blaze as it runs here, with why.
const IGNORED: &[(&str, &str)] = &[
    ("ignore-test", "the test is switched off upstream"),
    ("ignore-nightly", "blaze is a nightly"),
    ("only-stable", "blaze is a nightly"),
    ("only-beta", "blaze is a nightly"),
    ("only-dist", "rustc's own distribution build"),
    ("ignore-wasm-proc-macros", "blaze's proc macros are wasm"),
    ("needs-rustc-debug-assertions", "rustc's own debug assertions"),
    ("only-cdb", "a debugger suite's"),
    ("needs-git-hash", "rustc's own build"),
];

/// Directives with no blaze meaning yet, or no place in the runner, with why.
const UNSUPPORTED: &[(&str, &str)] = &[
    ("should-fail", "a test of compiletest itself"),
    ("add-minicore", "a `no_core` test for a target blaze does not build for"),
    ("minicore-compile-flags", "a `no_core` test for a target blaze does not build for"),
    ("rustc-env", "the compiler's environment for one crate"),
    ("unset-rustc-env", "the compiler's environment for one crate"),
    ("unset-exec-env", "the runner gives the program a fixed environment"),
    ("aux-bin", "an auxiliary program"),
    ("aux-codegen-backend", "rustc's codegen backend interface"),
    ("build-aux-docs", "rustdoc"),
    ("doc-flags", "rustdoc"),
    ("pp-exact", "rustc's pretty printer"),
    ("pretty-mode", "rustc's pretty printer"),
    ("pretty-compare-only", "rustc's pretty printer"),
    ("assembly-output", "rustc's assembly output"),
    ("filecheck-flags", "FileCheck over rustc's IR"),
    ("llvm-cov-flags", "coverage"),
    ("remap-src-base", "rustc's path remapping"),
    ("act-as-stable", "rustc's release channel"),
    ("check-test-line-numbers-match", "libtest's output for rustdoc"),
    ("min-llvm-version", "an LLVM version condition"),
    ("min-system-llvm-version", "an LLVM version condition"),
    ("max-llvm-major-version", "an LLVM version condition"),
    ("exact-llvm-major-version", "an LLVM version condition"),
    ("ignore-llvm-version", "an LLVM version condition"),
    ("needs-llvm-components", "an LLVM target component, for a cross build"),
    ("needs-llvm-zstd", "LLVM built with zstd"),
    ("needs-profiler-runtime", "the profiler runtime"),
    ("needs-xray", "XRay"),
    ("needs-enzyme", "Enzyme"),
    ("needs-offload", "offload"),
    ("needs-rust-lld", "rust-lld"),
    ("needs-dlltool", "dlltool"),
    ("needs-wasmtime", "a wasm target"),
    ("needs-force-clang-based-tests", "clang"),
    ("needs-std-remap-debuginfo", "rustc's own build"),
    ("needs-std-debug-assertions", "whether blaze's standard library has debug assertions is its profile's"),
    ("ignore-std-debug-assertions", "whether blaze's standard library has debug assertions is its profile's"),
    ("needs-backends", "a rustc codegen backend by name"),
    ("ignore-backends", "a rustc codegen backend by name"),
];

/// The words an `only-` or `ignore-` may end in that name a target or a part of one. A word
/// that is not here, and is no directive of the tables above, is an unknown directive.
const TARGET_WORDS: &[&str] = &[
    "16bit", "32bit", "64bit", "aarch64", "aix", "android", "apple", "arm", "arm64ec", "avr", "bpf", "csky", "eabi", "eabihf", "elf",
    "emscripten", "endian-big", "freebsd", "fuchsia", "gnu", "haiku", "horizon", "i586", "illumos", "ios", "linux", "loongarch32",
    "loongarch64", "macabi", "macos", "mips", "mips64", "msp430", "msvc", "musl", "netbsd", "none", "nto", "nvptx64", "ohos",
    "openbsd", "powerpc", "powerpc64", "qnx", "riscv32", "riscv64", "s390x", "sgx", "sparc", "sparc64", "spirv", "thumb", "tvos",
    "uefi", "unix", "unknown", "uwp", "visionos", "vxworks", "wasi", "wasm", "wasm32", "wasm64", "watchos", "windows",
    "windows-gnu", "windows-msvc", "x32", "x86", "x86_64", "rustc_abi-x86-sse2",
];

/// Whether `word` is a target triple by its shape: an architecture, then two or three more
/// parts (`x86_64-unknown-linux-gnu`, `thumbv8m.main-none-eabi`).
fn is_triple(word: &str) -> bool {
    let parts: Vec<&str> = word.split('-').collect();
    const ARCHES: &[&str] = &["aarch64", "arm", "armv7", "i686", "i586", "x86_64", "riscv64gc", "loongarch64", "wasm32", "nvptx64", "thumbv8m.base", "thumbv8m.main"];
    (3..=4).contains(&parts.len()) && ARCHES.contains(&parts[0])
}

/// `-C` options that change nothing blaze does: how rustc splits and schedules its own work
/// and links its own libraries.
const C_NO_MEANING: &[&str] = &["codegen-units", "incremental", "prefer-dynamic", "strip"];
/// `-Z` options that change only how rustc words, lays out or schedules its output.
const Z_NO_MEANING: &[&str] = &[
    "unstable-options",
    "threads",
    "ui-testing",
    "deduplicate-diagnostics",
    "write-long-types-to-disk",
    "macro-backtrace",
    "verbose-internals",
    "terminal-urls",
    "teach",
    "diagnostic-width",
];
/// Long options that change only how rustc words its output.
const LONG_NO_MEANING: &[&str] = &["--error-format", "--json", "--diagnostic-width", "--color"];

/// The lints compiletest allows in every crate it builds: tests use internal and incomplete
/// features, and unused parentheses and braces are not what a test is about.
const ALLOWED: &[&str] = &["internal_features", "incomplete_features", "unused_parens", "unused_braces"];

/// `text` split as a shell splits words, with `'..'` and `".."` holding spaces.
pub(crate) fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut open = false;
    let mut quote = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                open = true;
            }
            None if c.is_whitespace() => {
                if open || !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                open = false;
            }
            None => word.push(c),
        }
    }
    if open || !word.is_empty() {
        out.push(word);
    }
    out
}

/// A crate's flags as the manifest says them.
#[derive(Debug, Default)]
struct Flags {
    edition: Option<String>,
    kind: Option<CrateKind>,
    name: Option<String>,
    rustflags: Vec<String>,
    profile: Vec<String>,
    panic_abort: bool,
}

fn crate_kind(word: &str) -> CrateKind {
    match word {
        "bin" => CrateKind::Bin,
        "lib" | "rlib" => CrateKind::Lib,
        "proc-macro" => CrateKind::ProcMacro,
        other => CrateKind::Other(other.to_owned()),
    }
}

fn on(value: Option<&str>) -> Result<bool, String> {
    match value {
        None | Some("yes" | "y" | "on" | "true") => Ok(true),
        Some("no" | "n" | "off" | "false") => Ok(false),
        Some(other) => Err(format!("a switch set to `{other}`")),
    }
}

/// Reads one `compile-flags` value into `flags`; `Err` names the first flag with no blaze
/// meaning (its name alone, so tests group by it).
fn take_flags(text: &str, flags: &mut Flags) -> Result<(), String> {
    let all = words(text);
    let mut at = 0;
    // The value of an option given as `--name value`, `--name=value` or, for one letter,
    // `-Nvalue`.
    let value = |at: &mut usize, name: &str| -> Option<String> {
        let word = &all[*at];
        let joined = word.strip_prefix(name)?;
        if let Some(rest) = joined.strip_prefix('=') {
            return Some(rest.to_owned());
        }
        if joined.is_empty() {
            *at += 1;
            return all.get(*at).cloned();
        }
        (!name.starts_with("--")).then(|| joined.to_owned())
    };
    while at < all.len() {
        let word = all[at].clone();
        let unsupported = |name: &str| Err(format!("flag {name}"));
        if word == "-O" {
            flags.profile.push("opt-level = 3".to_owned());
        } else if word == "-g" {
            flags.profile.push("debug = 2".to_owned());
        } else if word.starts_with("--cfg") {
            let it = value(&mut at, "--cfg").ok_or("flag --cfg with no value")?;
            flags.rustflags.extend(["--cfg".to_owned(), it]);
        } else if word.starts_with("--check-cfg") {
            let it = value(&mut at, "--check-cfg").ok_or("flag --check-cfg with no value")?;
            flags.rustflags.extend(["--check-cfg".to_owned(), it]);
        } else if word.starts_with("--edition") {
            flags.edition = Some(value(&mut at, "--edition").ok_or("flag --edition with no value")?);
        } else if word.starts_with("--crate-type") {
            let it = value(&mut at, "--crate-type").ok_or("flag --crate-type with no value")?;
            if it.contains(',') {
                return unsupported("--crate-type with several types");
            }
            flags.kind = Some(crate_kind(&it));
        } else if word.starts_with("--crate-name") {
            flags.name = Some(value(&mut at, "--crate-name").ok_or("flag --crate-name with no value")?);
        } else if let Some(long) = ["--allow", "--warn", "--deny", "--forbid", "--force-warn", "--cap-lints"].into_iter().find(|it| word.starts_with(it)) {
            let it = value(&mut at, long).ok_or_else(|| format!("flag {long} with no value"))?;
            flags.rustflags.extend([long.to_owned(), it]);
        } else if let Some(short) = ["-A", "-W", "-D", "-F"].into_iter().find(|it| word.starts_with(it)) {
            let it = value(&mut at, short).ok_or_else(|| format!("flag {short} with no value"))?;
            flags.rustflags.extend([short.to_owned(), it]);
        } else if let Some(long) = LONG_NO_MEANING.iter().find(|it| word.starts_with(**it)) {
            // `--error-format=json` or `--error-format json`: the value goes with it.
            let _ = value(&mut at, *long);
        } else if word.starts_with("-C") || word.starts_with("--codegen") {
            let name = if word.starts_with("-C") { "-C" } else { "--codegen" };
            let it = value(&mut at, name).ok_or("flag -C with no option")?;
            let (option, set) = match it.split_once('=') {
                Some((option, set)) => (option, Some(set)),
                None => (it.as_str(), None),
            };
            let option = option.replace('_', "-");
            match option.as_str() {
                "opt-level" => {
                    let level = set.ok_or("flag -C opt-level with no value")?;
                    flags.profile.push(match level {
                        "s" | "z" => format!("opt-level = \"{level}\""),
                        "0" | "1" | "2" | "3" => format!("opt-level = {level}"),
                        other => return Err(format!("flag -C opt-level={other}")),
                    });
                }
                "debuginfo" => {
                    let level = set.ok_or("flag -C debuginfo with no value")?;
                    flags.profile.push(match level {
                        "0" | "1" | "2" => format!("debug = {level}"),
                        "none" | "limited" | "full" | "line-tables-only" | "line-directives-only" => format!("debug = \"{level}\""),
                        other => return Err(format!("flag -C debuginfo={other}")),
                    });
                }
                "debug-assertions" | "overflow-checks" => flags.profile.push(format!("{option} = {}", on(set).map_err(|why| format!("flag -C {option}: {why}"))?)),
                "panic" => match set {
                    Some("abort") => flags.panic_abort = true,
                    Some("unwind") => {}
                    other => return Err(format!("flag -C panic={}", other.unwrap_or(""))),
                },
                other if C_NO_MEANING.contains(&other) => {}
                other => return unsupported(&format!("-C {other}")),
            }
        } else if word.starts_with("-Z") {
            let it = value(&mut at, "-Z").ok_or("flag -Z with no option")?;
            let option = it.split('=').next().unwrap_or("").replace('_', "-");
            if !Z_NO_MEANING.contains(&option.as_str()) {
                return unsupported(&format!("-Z {option}"));
            }
        } else {
            // Its name alone: `--target=x` and `--target x` are one cause.
            return unsupported(word.split('=').next().unwrap_or(&word));
        }
        at += 1;
    }
    Ok(())
}

/// What a source says of its own crate in attributes the runner must give the manifest:
/// `#![crate_type = ".."]` and `#![crate_name = ".."]`.
fn attributes(source: &str) -> (Option<CrateKind>, Option<String>) {
    let attribute = |name: &str| {
        let found = Regex::new(&format!(r#"(?m)^\s*#!\[\s*{name}\s*=\s*"([^"]+)"\s*\]"#)).expect("a regex that compiles");
        found.captures(source).map(|it| it[1].to_owned())
    };
    (attribute("crate_type").map(|it| crate_kind(&it)), attribute("crate_name"))
}

/// Whether `source` names files beside it.
fn names_siblings(source: &str) -> bool {
    let found = Regex::new(r"\bmod\s+[A-Za-z_][A-Za-z0-9_]*\s*;|#!?\[\s*path\b|\binclude(_str|_bytes)?!").expect("a regex that compiles");
    found.is_match(source)
}

/// A file's name as a crate name: its stem, `-` as `_`, never one of the standard library's
/// and never starting with a digit.
pub(crate) fn crate_name(file: &str) -> String {
    let stem = file.rsplit('/').next().unwrap_or(file).trim_end_matches(".rs");
    let mut name: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if name.starts_with(|c: char| c.is_ascii_digit()) || name.is_empty() {
        name.insert(0, '_');
    }
    if ["std", "core", "alloc", "test", "proc_macro", "build", "deps", "examples", "incremental"].contains(&name.as_str()) {
        name.push('_');
    }
    name
}

/// `//@ edition`'s value as one edition: a range's lower bound, as compiletest takes it.
fn edition(value: &str) -> Result<String, String> {
    let lower = value.split("..").next().unwrap_or("").trim();
    match lower {
        "2015" | "2018" | "2021" | "2024" => Ok(lower.to_owned()),
        other => Err(format!("edition {other}")),
    }
}

/// What an `only-`, `ignore-` or `needs-` directive, or one of the tables, decides.
enum Condition {
    /// Nothing: the test goes on.
    Holds,
    Ignored(String),
    Unsupported(String),
    /// Not a condition: another kind of directive.
    Other,
}

fn condition(name: &str, host: &Host) -> Condition {
    if NO_MEANING.contains(&name) {
        return Condition::Holds;
    }
    if let Some((_, why)) = IGNORED.iter().find(|it| it.0 == name) {
        return Condition::Ignored(format!("{name}: {why}"));
    }
    if let Some((_, why)) = UNSUPPORTED.iter().find(|it| it.0 == name) {
        return Condition::Unsupported(format!("{name}: {why}"));
    }
    if name.starts_with("needs-sanitizer-") {
        return Condition::Unsupported("needs-sanitizer-*: the sanitizer runtimes".to_owned());
    }
    let (only, word) = match (name.strip_prefix("only-"), name.strip_prefix("ignore-")) {
        (Some(word), _) => (true, word),
        (_, Some(word)) => (false, word),
        _ => return Condition::Other,
    };
    if !TARGET_WORDS.contains(&word) && !is_triple(word) {
        return Condition::Other;
    }
    let is = host.truths().iter().any(|it| it == word);
    match (only, is) {
        (true, true) | (false, false) => Condition::Holds,
        _ => Condition::Ignored(format!("{name}: this is {}", host.triple())),
    }
}

/// An auxiliary's package: `file` is its path under the suite; `as_proc_macro` for `proc-macro:`.
fn auxiliary(file: &str, as_proc_macro: bool, host: &Host, read: &dyn Fn(&str) -> Option<String>, into: &mut Vec<Crate>, depth: usize) -> Result<String, Planned> {
    let unsupported = |why: String| Planned::Unsupported(why);
    if depth > 8 {
        return Err(unsupported(format!("auxiliaries nested past 8 at {file}")));
    }
    let Some(source) = read(file) else {
        return Err(unsupported(format!("its auxiliary {file} is not there or not UTF-8")));
    };
    let parsed = super::directives::parse(&source);
    if let Some(why) = parsed.malformed.first() {
        return Err(unsupported(format!("auxiliary {file}: {why}")));
    }
    let (attr_kind, attr_name) = attributes(&source);
    let mut flags = Flags::default();
    let mut crate_edition = "2015".to_owned();
    let mut deps = Vec::new();
    let dir = file.rsplit_once('/').map_or("", |it| it.0);
    for it in parsed.of(None) {
        match it.name.as_str() {
            "edition" => crate_edition = edition(&it.value).map_err(|why| unsupported(format!("auxiliary {why}")))?,
            "compile-flags" => take_flags(&it.value, &mut flags).map_err(|why| unsupported(format!("auxiliary {why}")))?,
            "aux-build" => {
                let package = auxiliary(&format!("{dir}/{}", it.value), false, host, read, into, depth + 1)?;
                deps.push((package.clone(), package));
            }
            "aux-crate" => deps.push(aux_crate(&it.value, dir, host, read, into, depth)?),
            "ignore-auxiliary" => {}
            "proc-macro" => {
                let package = auxiliary(&format!("{dir}/{}", it.value), true, host, read, into, depth + 1)?;
                deps.push((package.clone(), package));
            }
            name => match condition(name, host) {
                Condition::Holds => {}
                Condition::Ignored(why) => return Err(Planned::Ignored(format!("auxiliary {why}"))),
                Condition::Unsupported(why) => return Err(unsupported(format!("auxiliary {why}"))),
                Condition::Other => return Err(unsupported(format!("auxiliary directive {name}"))),
            },
        }
    }
    if flags.panic_abort {
        return Err(unsupported("auxiliary flag -C panic".to_owned()));
    }
    let kind = match as_proc_macro {
        true => CrateKind::ProcMacro,
        false => flags.kind.or(attr_kind).unwrap_or(CrateKind::Lib),
    };
    // compiletest builds an auxiliary as a `dylib` unless it says otherwise, to keep its own
    // test binaries small: nothing a test states. Here it is a library.
    let kind = match kind {
        CrateKind::Other(it) if it == "dylib" => CrateKind::Lib,
        CrateKind::Bin => return Err(unsupported(format!("auxiliary {file} is a program"))),
        other => other,
    };
    let name = flags.name.or(attr_name).unwrap_or_else(|| crate_name(file));
    if let Some(same) = into.iter().find(|it| it.name == name) {
        return match same.file == file {
            true => Ok(name),
            false => Err(unsupported(format!("two auxiliaries named {name}"))),
        };
    }
    let mut rustflags: Vec<String> = ALLOWED.iter().flat_map(|lint| ["-A".to_owned(), (*lint).to_owned()]).collect();
    rustflags.extend(flags.rustflags);
    into.push(Crate {
        name: name.clone(),
        file: file.to_owned(),
        edition: flags.edition.unwrap_or(crate_edition),
        kind,
        rustflags,
        profile: flags.profile,
        deps,
        siblings: names_siblings(&source),
    });
    Ok(name)
}

/// `aux-crate`'s value, `[modifiers:]name=file.rs`: (the name the test uses, the package).
fn aux_crate(value: &str, dir: &str, host: &Host, read: &dyn Fn(&str) -> Option<String>, into: &mut Vec<Crate>, depth: usize) -> Result<(String, String), Planned> {
    let Some((name, file)) = value.split_once('=') else {
        return Err(Planned::Unsupported(format!("aux-crate with no `=` (`{value}`)")));
    };
    if name.contains(':') {
        return Err(Planned::Unsupported("aux-crate with `--extern` modifiers".to_owned()));
    }
    let package = auxiliary(&format!("{dir}/{}", file.trim()), false, host, read, into, depth + 1)?;
    Ok((name.trim().to_owned(), package))
}

/// The kind an incremental revision's name asks for (`rpass2`, `cfail1`).
fn incremental_kind(revision: &str) -> Option<Kind> {
    Some(match revision.trim_end_matches(|c: char| c.is_ascii_digit()) {
        "cpass" => Kind::CheckPass,
        "bpass" => Kind::BuildPass,
        "rpass" => Kind::RunPass,
        "cfail" => Kind::CompileFail { build: false },
        "bfail" => Kind::CompileFail { build: true },
        "rfail" => Kind::RunFail(Ends::Fail),
        _ => return None,
    })
}

/// The plan of the test at `file` (a path under the suite's directory) for `revision`.
/// `read` gives a file of the suite by such a path, `None` where it is not there or not text.
pub(crate) fn plan(suite: Suite, file: &str, source: &str, parsed: &Parsed, revision: Option<&str>, host: &Host, read: &dyn Fn(&str) -> Option<String>) -> Planned {
    if let Some(why) = parsed.malformed.first() {
        return Planned::Unsupported(format!("malformed: {why}"));
    }
    let dir = file.rsplit_once('/').map_or("", |it| it.0);
    let aux_dir = if dir.is_empty() { "auxiliary".to_owned() } else { format!("{dir}/auxiliary") };
    let mut kind = match suite {
        // A ui test with no mode must fail to compile.
        Suite::Ui => Kind::CompileFail { build: true },
        Suite::Crashes => Kind::Survive,
        Suite::Incremental => match revision.and_then(incremental_kind) {
            Some(kind) => kind,
            None => return Planned::Unsupported(format!("an incremental revision named `{}`", revision.unwrap_or(""))),
        },
    };
    let mut flags = Flags::default();
    let mut crate_edition = "2015".to_owned();
    let mut aux = Vec::new();
    let mut deps = Vec::new();
    let mut run = Run::default();
    let mut not_held: Vec<String> = Vec::new();
    let (mut known_bug, mut check_run_results, mut auto_check_cfg) = (false, false, true);
    let mut patterns: Vec<(bool, String)> = Vec::new();
    // A later mode directive replaces an earlier one, and a revision's own the test's.
    for it in parsed.of(revision) {
        let name = it.name.as_str();
        let mode = match name {
            "check-pass" => Some(Kind::CheckPass),
            "build-pass" => Some(Kind::BuildPass),
            "run-pass" => Some(Kind::RunPass),
            "check-fail" => Some(Kind::CompileFail { build: false }),
            "build-fail" => Some(Kind::CompileFail { build: true }),
            "run-fail" => Some(Kind::RunFail(Ends::Fail)),
            "run-crash" => Some(Kind::RunFail(Ends::Crash)),
            "run-fail-or-crash" => Some(Kind::RunFail(Ends::Either)),
            _ => None,
        };
        if let Some(mode) = mode {
            if suite == Suite::Ui {
                kind = mode;
            }
            continue;
        }
        let made = match name {
            "edition" => edition(&it.value).map(|it| crate_edition = it).map_err(Planned::Unsupported),
            "compile-flags" => take_flags(&it.value, &mut flags).map_err(Planned::Unsupported),
            "aux-build" => auxiliary(&format!("{aux_dir}/{}", it.value), false, host, read, &mut aux, 0).map(|package| deps.push((package.clone(), package))),
            "aux-crate" => aux_crate(&it.value, &aux_dir, host, read, &mut aux, 0).map(|dep| deps.push(dep)),
            "proc-macro" => auxiliary(&format!("{aux_dir}/{}", it.value), true, host, read, &mut aux, 0).map(|package| deps.push((package.clone(), package))),
            "known-bug" => {
                known_bug = true;
                Ok(())
            }
            "check-run-results" => {
                check_run_results = true;
                Ok(())
            }
            "no-auto-check-cfg" => {
                auto_check_cfg = false;
                Ok(())
            }
            "run-flags" => {
                run.args.extend(words(&it.value));
                Ok(())
            }
            "exec-env" => match it.value.split_once('=') {
                Some((name, value)) => {
                    run.env.push((name.trim().to_owned(), value.to_owned()));
                    Ok(())
                }
                None => Err(Planned::Unsupported(format!("exec-env with no `=` (`{}`)", it.value))),
            },
            "error-pattern" => {
                patterns.push((false, it.value.clone()));
                Ok(())
            }
            "regex-error-pattern" => {
                patterns.push((true, it.value.clone()));
                Ok(())
            }
            "ignore-auxiliary" => Err(Planned::Ignored("ignore-auxiliary: a file of another test".to_owned())),
            _ if NOT_HELD.contains(&name) => {
                not_held.push(name.to_owned());
                Ok(())
            }
            _ => match condition(name, host) {
                Condition::Holds => Ok(()),
                Condition::Ignored(why) => Err(Planned::Ignored(why)),
                Condition::Unsupported(why) => Err(Planned::Unsupported(why)),
                Condition::Other => Err(Planned::Unsupported(format!("unknown directive {name}"))),
            },
        };
        if let Err(stop) = made {
            return stop;
        }
    }
    if known_bug {
        kind = Kind::Survive;
    }
    let (attr_kind, attr_name) = attributes(source);
    let crate_kind = flags.kind.or(attr_kind).unwrap_or(CrateKind::Bin);
    let runs = matches!(kind, Kind::RunPass | Kind::RunFail(_));
    if runs && crate_kind != CrateKind::Bin {
        return Planned::Unsupported("a test that runs a crate that is no program".to_owned());
    }
    let name = flags.name.or(attr_name).unwrap_or_else(|| crate_name(file));
    if aux.iter().any(|it| it.name == name) {
        return Planned::Unsupported("an auxiliary with the test's own crate name".to_owned());
    }
    // Run output is the program's own, so its patterns are held; a compile's are rustc's words.
    for (regex, pattern) in patterns {
        match (matches!(kind, Kind::RunFail(_)), regex) {
            (true, false) => run.patterns.push(pattern),
            (true, true) => run.regex_patterns.push(pattern),
            (false, _) if !not_held.iter().any(|it| it == "error-pattern") => not_held.push("error-pattern".to_owned()),
            (false, _) => {}
        }
    }
    if check_run_results && runs {
        if not_held.iter().any(|it| it == "normalize-stdout") {
            return Planned::Unsupported("check-run-results with normalize-stdout".to_owned());
        }
        let stem = file.trim_end_matches(".rs");
        let named = match revision {
            Some(revision) => format!("{stem}.{revision}.run.stdout"),
            None => format!("{stem}.run.stdout"),
        };
        // No file: the program must print nothing, which the runner holds too.
        run.stdout = Some(named);
    }
    // compiletest's own flags for every ui test, before the test's: unused code is not what a
    // test is about, and tests use internal and incomplete features.
    let mut rustflags: Vec<String> = ["-A", "unused", "-W", "unused_attributes"].map(str::to_owned).to_vec();
    rustflags.extend(ALLOWED.iter().flat_map(|lint| ["-A".to_owned(), (*lint).to_owned()]));
    if auto_check_cfg {
        let mut names = vec!["test".to_owned(), "FALSE".to_owned()];
        names.extend(parsed.revisions());
        rustflags.extend(["--check-cfg".to_owned(), format!("cfg({})", names.join(","))]);
    }
    if let Some(revision) = revision {
        rustflags.extend(["--cfg".to_owned(), revision.to_owned()]);
    }
    rustflags.extend(flags.rustflags);
    let mut annotations = Vec::new();
    let mut others = 0;
    for it in parsed.annotations.iter().filter(|it| it.holds_for(revision)) {
        match it.level {
            Level::Error | Level::Warning => annotations.push(it.clone()),
            _ => others += 1,
        }
    }
    if others > 0 {
        not_held.push("NOTE, HELP and SUGGESTION annotations".to_owned());
    }
    Planned::Run(Box::new(Plan {
        kind,
        main: Crate {
            name,
            file: file.to_owned(),
            edition: flags.edition.unwrap_or(crate_edition),
            kind: crate_kind,
            rustflags,
            profile: flags.profile,
            deps,
            siblings: names_siblings(source),
        },
        aux,
        panic_abort: flags.panic_abort,
        run,
        annotations,
        not_held,
    }))
}

/// A package's manifest. `root`: the test's own package, which also holds the workspace and the
/// profile. `source` is the path of its root file from the manifest.
pub(crate) fn manifest(it: &Crate, source: &str, root: Option<&Plan>) -> String {
    let quoted = |flags: &[String]| flags.iter().map(|it| format!("{it:?}")).collect::<Vec<_>>().join(", ");
    let mut out = String::new();
    if root.is_some() {
        out.push_str("cargo-features = [\"profile-rustflags\"]\n\n");
    }
    out.push_str(&format!("[package]\nname = {:?}\nversion = \"0.0.0\"\nedition = {:?}\n\n", it.name, it.edition));
    match &it.kind {
        CrateKind::Bin => out.push_str(&format!("[[bin]]\nname = {:?}\npath = {source:?}\n", it.name)),
        CrateKind::Lib => out.push_str(&format!("[lib]\nname = {:?}\npath = {source:?}\n", it.name)),
        CrateKind::ProcMacro => out.push_str(&format!("[lib]\nname = {:?}\npath = {source:?}\nproc-macro = true\n", it.name)),
        CrateKind::Other(kind) => out.push_str(&format!("[lib]\nname = {:?}\npath = {source:?}\ncrate-type = [{kind:?}]\n", it.name)),
    }
    if !it.deps.is_empty() {
        out.push_str("\n[dependencies]\n");
        for (used, package) in &it.deps {
            let path = if root.is_some() { format!("aux/{package}") } else { format!("../{package}") };
            out.push_str(&format!("{used} = {{ path = {path:?}, package = {package:?} }}\n"));
        }
    }
    let Some(plan) = root else { return out };
    if plan.panic_abort {
        out.push_str("\n[profile.dev]\npanic = \"abort\"\n");
    }
    for each in std::iter::once(&plan.main).chain(&plan.aux) {
        if each.rustflags.is_empty() && each.profile.is_empty() {
            continue;
        }
        out.push_str(&format!("\n[profile.dev.package.{:?}]\n", each.name));
        for line in &each.profile {
            out.push_str(line);
            out.push('\n');
        }
        if !each.rustflags.is_empty() {
            out.push_str(&format!("rustflags = [{}]\n", quoted(&each.rustflags)));
        }
    }
    out.push_str("\n[workspace]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::super::directives::parse;
    use super::*;

    fn host() -> Host {
        Host { arch: "aarch64".to_owned(), os: "linux".to_owned() }
    }

    fn planned(suite: Suite, source: &str, revision: Option<&str>) -> Planned {
        let read = |path: &str| match path {
            "dir/auxiliary/helper.rs" => Some("//@ edition: 2021\n//@ aux-build: inner.rs\npub fn f() {}\n".to_owned()),
            "dir/auxiliary/inner.rs" => Some("#![crate_name = \"deep\"]\npub fn g() {}\n".to_owned()),
            "dir/auxiliary/mac.rs" => Some("extern crate proc_macro;\n".to_owned()),
            "dir/auxiliary/cross.rs" => Some("//@ compile-flags: --target wasm32-wasip1\n".to_owned()),
            _ => None,
        };
        plan(suite, "dir/a-test.rs", source, &parse(source), revision, &host(), &read)
    }

    fn run(suite: Suite, source: &str, revision: Option<&str>) -> Plan {
        match planned(suite, source, revision) {
            Planned::Run(plan) => *plan,
            other => panic!("not run: {other:?}"),
        }
    }

    #[test]
    fn a_ui_test_with_no_mode_must_fail_to_compile() {
        let plan = run(Suite::Ui, "fn main() { let x: u8 = 1u16; } //~ ERROR mismatched\n//~^ NOTE expected\n", None);
        assert_eq!(plan.kind, Kind::CompileFail { build: true });
        assert_eq!(plan.main.name, "a_test");
        assert_eq!(plan.main.kind, CrateKind::Bin);
        assert_eq!(plan.main.edition, "2015");
        assert_eq!(plan.annotations.len(), 1);
        assert_eq!(plan.not_held, ["NOTE, HELP and SUGGESTION annotations"]);
    }

    #[test]
    fn modes_flags_and_revisions_are_the_revisions_own() {
        let source = "//@ revisions: old new\n//@ check-pass\n//@[new] check-fail\n//@ edition: 2018..2024\n\
                      //@ compile-flags: --crate-type=lib -Dwarnings --cfg 'feature=\"x\"' -Zunstable-options -Ccodegen-units=1 -O\n\
                      //@[new] compile-flags: -C debug-assertions=no --error-format json\n\
                      //[new]~ ERROR only here\n";
        let old = run(Suite::Ui, source, Some("old"));
        assert_eq!(old.kind, Kind::CheckPass);
        assert_eq!(old.main.kind, CrateKind::Lib);
        assert_eq!(old.main.edition, "2018");
        assert_eq!(old.main.profile, ["opt-level = 3"]);
        assert!(old.annotations.is_empty());
        let tail: Vec<&str> = old.main.rustflags.iter().map(String::as_str).rev().take(8).rev().collect();
        assert_eq!(tail, ["--check-cfg", "cfg(test,FALSE,old,new)", "--cfg", "old", "-D", "warnings", "--cfg", "feature=\"x\""]);
        let new = run(Suite::Ui, source, Some("new"));
        assert_eq!(new.kind, Kind::CompileFail { build: false });
        assert_eq!(new.main.profile, ["opt-level = 3", "debug-assertions = false"]);
        assert_eq!(new.annotations.len(), 1);
    }

    #[test]
    fn what_has_no_blaze_meaning_is_named() {
        let unsupported = |source: &str| match planned(Suite::Ui, source, None) {
            Planned::Unsupported(why) => why,
            other => panic!("{other:?}"),
        };
        assert_eq!(unsupported("//@ compile-flags: -Znext-solver=globally\n"), "flag -Z next-solver");
        assert_eq!(unsupported("//@ compile-flags: --target x86_64-unknown-linux-gnu\n"), "flag --target");
        assert_eq!(unsupported("//@ compile-flags: -C target-feature=+neon\n"), "flag -C target-feature");
        assert_eq!(unsupported("//@ compile-flags: --emit=mir\n"), "flag --emit");
        assert_eq!(unsupported("//@ made-up-directive\n"), "unknown directive made-up-directive");
        assert_eq!(unsupported("//@ only-frobnicate\n"), "unknown directive only-frobnicate");
        assert_eq!(unsupported("//@ rustc-env: A=b\n"), "rustc-env: the compiler's environment for one crate");
        assert_eq!(unsupported("//@ aux-build: cross.rs\n"), "auxiliary flag --target");
        assert_eq!(unsupported("//@ aux-build: missing.rs\n"), "its auxiliary dir/auxiliary/missing.rs is not there or not UTF-8");
        assert_eq!(unsupported("//~| ERROR nothing before\n"), "malformed: line 1: `//~|` with no annotation before it");
        assert_eq!(unsupported("//@ run-pass\n//@ compile-flags: --crate-type lib\n"), "a test that runs a crate that is no program");
    }

    #[test]
    fn conditions_are_decided_for_the_host() {
        let ignored = |source: &str| matches!(planned(Suite::Ui, source, None), Planned::Ignored(_));
        assert!(ignored("//@ only-x86_64\n"));
        assert!(ignored("//@ ignore-linux\n"));
        assert!(ignored("//@ ignore-aarch64-unknown-linux-gnu\n"));
        assert!(ignored("//@ only-windows\n"));
        assert!(ignored("//@ ignore-test\n"));
        assert!(ignored("//@ ignore-64bit\n"));
        for holds in ["//@ only-aarch64\n", "//@ only-linux\n", "//@ ignore-windows\n", "//@ only-unix\n", "//@ ignore-x86\n", "//@ only-64bit\n", "//@ needs-unwind\n", "//@ only-elf\n"] {
            assert!(matches!(planned(Suite::Ui, holds, None), Planned::Run(_)), "{holds}");
        }
    }

    #[test]
    fn auxiliaries_become_packages_in_dependency_order() {
        let plan = run(Suite::Ui, "//@ run-pass\n//@ aux-build: helper.rs\n//@ proc-macro: mac.rs\n//@ aux-crate: other=helper.rs\nfn main() {}\n", None);
        let names: Vec<(&str, &CrateKind, &str)> = plan.aux.iter().map(|it| (it.name.as_str(), &it.kind, it.edition.as_str())).collect();
        assert_eq!(names, [("deep", &CrateKind::Lib, "2015"), ("helper", &CrateKind::Lib, "2021"), ("mac", &CrateKind::ProcMacro, "2015")]);
        assert_eq!(plan.aux[1].deps, [("deep".to_owned(), "deep".to_owned())]);
        assert_eq!(
            plan.main.deps,
            [("helper".to_owned(), "helper".to_owned()), ("mac".to_owned(), "mac".to_owned()), ("other".to_owned(), "helper".to_owned())]
        );
        let text = manifest(&plan.main, "t/a-test.rs", Some(&plan));
        assert!(text.contains("[[bin]]\nname = \"a_test\"\npath = \"t/a-test.rs\"\n"), "{text}");
        assert!(text.contains("other = { path = \"aux/helper\", package = \"helper\" }\n"), "{text}");
        assert!(text.contains("[profile.dev.package.\"a_test\"]\nrustflags = [\"-A\", \"unused\""), "{text}");
        assert!(text.ends_with("\n[workspace]\n"), "{text}");
        let helper = manifest(&plan.aux[1], "helper.rs", None);
        assert!(helper.contains("deep = { path = \"../deep\", package = \"deep\" }\n"), "{helper}");
        assert!(!helper.contains("[workspace]"), "{helper}");
    }

    #[test]
    fn a_program_that_runs_says_what_is_checked_of_it() {
        let plan = run(Suite::Ui, "//@ run-fail\n//@ error-pattern: explicit panic\n//@ exec-env: A=b c\n//@ run-flags: one 'two words'\n//@ check-run-results\nfn main() {}\n", None);
        assert_eq!(plan.kind, Kind::RunFail(Ends::Fail));
        assert_eq!(plan.run.patterns, ["explicit panic"]);
        assert_eq!(plan.run.env, [("A".to_owned(), "b c".to_owned())]);
        assert_eq!(plan.run.args, ["one", "two words"]);
        assert_eq!(plan.run.stdout.as_deref(), Some("dir/a-test.run.stdout"));
        let compile = run(Suite::Ui, "//@ error-pattern: rustc's words\nfn main() {}\n", None);
        assert!(compile.run.patterns.is_empty());
        assert_eq!(compile.not_held, ["error-pattern"]);
    }

    #[test]
    fn crashes_and_known_bugs_only_have_to_be_survived() {
        assert_eq!(run(Suite::Crashes, "//@ known-bug: #1\n//@ edition: 2021\nfn main() {}\n", None).kind, Kind::Survive);
        assert_eq!(run(Suite::Ui, "//@ check-pass\n//@ known-bug: #2\nfn main() {}\n", None).kind, Kind::Survive);
    }

    #[test]
    fn an_incremental_revisions_name_is_its_mode() {
        let source = "//@ revisions: rpass1 cfail2 bfail3 what4\n//@ check-pass\nfn main() {}\n";
        assert_eq!(run(Suite::Incremental, source, Some("rpass1")).kind, Kind::RunPass);
        assert_eq!(run(Suite::Incremental, source, Some("cfail2")).kind, Kind::CompileFail { build: false });
        assert_eq!(run(Suite::Incremental, source, Some("bfail3")).kind, Kind::CompileFail { build: true });
        assert!(matches!(planned(Suite::Incremental, source, Some("what4")), Planned::Unsupported(_)));
    }

    #[test]
    fn words_split_as_a_shell_does() {
        assert_eq!(words("a  'b c' --x=\"d e\" ''"), ["a", "b c", "--x=d e", ""]);
        assert_eq!(crate_name("x/issue-123.rs"), "issue_123");
        assert_eq!(crate_name("x/2229.rs"), "_2229");
        assert_eq!(crate_name("std.rs"), "std_");
    }
}
