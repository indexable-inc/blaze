//! rustc's unstable flags, `-Z name[=value]`, and what blaze does with each: the one table
//! every place that reads rustc's flags asks (the daemon's plan, the plan of a program linked
//! against rustc's libraries, the build scripts and proc macros blaze compiles, blaze's own
//! answers to rustc's questions), so that they all agree. rustc still gets every flag cargo
//! gives it, for what it compiles; the table says what blaze does in its own code.
//!
//! - **Ignored**: the flag changes how fast rustc is, what it prints, the files it writes
//!   beside its output or what rustc's own backend does inside a function, and nothing a
//!   program does or how blaze's code links against rustc's.
//! - **Modelled**: the flag changes what a program does, and blaze's code already does what
//!   it asks.
//! - **Refused**: the flag changes the code, its ABI, what rustc's libraries export or what a
//!   program does, and blaze does not do the same; a crate built with it is not blaze's,
//!   with a reason that names the flag. A flag the table does not have is refused too.
//!
//! The names are those of `rustc -Z help` of the toolchain in `harness/toolchain`.
//!
//! [`sanitize`] reads the sanitizer flags into the build's [`Sanitize`] for blaze's own code
//! generator; the table still refuses them until blaze's linker links an instrumented program.

use blaze_codegen::settings::{Coverage, Sanitize, Sanitizers};
use blaze_units::stdlib::target::{self, SANITIZERS, UNSANITIZED};

/// What blaze does with one `-Z` flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Treatment {
    Ignored(&'static str),
    Modelled(&'static str),
    Refused(&'static str),
}

/// A row of the table: how a flag is treated, by its value where that decides it.
#[derive(Clone, Copy)]
enum Row {
    Ignored(&'static str),
    /// Modelled when the flag turns the thing off, which is what blaze's code does; refused
    /// when it turns it on.
    OffOnly(&'static str),
    Refused(&'static str),
}

use Row::{Ignored, OffOnly, Refused};

const SPEED: &str = "how fast rustc compiles";
const PRINTS: &str = "what rustc prints or writes beside its output";
const CHECKS: &str = "what rustc checks of itself while it compiles";
const ACCEPTS: &str = "which programs rustc's type checker accepts, not the code of one it accepts";
const RUSTC_BACKEND: &str =
    "how rustc's backend optimizes or lays out its own code; the program does the same";
const DEBUG: &str = "what rustc's own objects tell a debugger; blaze writes its own debug info";
const CODE: &str = "it changes the code rustc generates, which blaze's code generator does not do";
const ABI: &str = "it changes the ABI or symbols rustc's code has, which blaze's code must match";
const EXPORTS: &str =
    "it changes which functions rustc's libraries export, which blaze links against";
const SEMANTICS: &str = "it changes what the program does, which blaze does not model";
const LINKS: &str = "it changes how rustc links, which blaze's link does not do";
const NOT_BUILT: &str = "rustc does not build the crate's code with it";
const INSTRUMENTS: &str = "it instruments the code, which blaze's code generator does not do";

/// Every flag of `rustc -Z help`, sorted by name.
const TABLE: &[(&str, Row)] = &[
    ("allow-features", Refused("blaze does not check which features a crate enables")),
    ("allow-partial-mitigations", Refused(ABI)),
    ("always-encode-mir", Refused(EXPORTS)),
    ("annotate-moves", Ignored(DEBUG)),
    ("assert-incr-state", Ignored(CHECKS)),
    ("assume-incomplete-release", Refused("it changes what `cfg(version)` says")),
    ("assumptions-on-binders", Ignored(ACCEPTS)),
    ("async-panic", Refused(SEMANTICS)),
    ("autodiff", Refused(CODE)),
    ("autodiff-post-passes", Refused(CODE)),
    ("binary-dep-depinfo", Ignored(PRINTS)),
    ("box-noalias", Ignored(RUSTC_BACKEND)),
    ("branch-protection", Refused(CODE)),
    ("build-sdylib-interface", Refused(NOT_BUILT)),
    ("cache-proc-macros", Ignored(SPEED)),
    ("cf-protection", Refused(CODE)),
    ("check-cfg-all-expected", Ignored(PRINTS)),
    ("checksum-hash-algorithm", Ignored(PRINTS)),
    (
        "codegen-backend",
        Refused("blaze links against what rustc's LLVM backend builds, not another backend's"),
    ),
    ("codegen-emit-retag", Refused(INSTRUMENTS)),
    ("codegen-source-order", Ignored(RUSTC_BACKEND)),
    ("contract-checks", OffOnly("blaze compiles `contract_checks()` as false")),
    ("coverage-options", Refused(INSTRUMENTS)),
    ("crate-attr", Refused("blaze's front end does not see the attribute it adds")),
    ("cross-crate-inline-threshold", Refused(EXPORTS)),
    ("debug-info-type-line-numbers", Ignored(DEBUG)),
    ("debuginfo-compression", Ignored(DEBUG)),
    ("debuginfo-for-profiling", Ignored(DEBUG)),
    ("deduplicate-diagnostics", Ignored(PRINTS)),
    ("default-visibility", Refused(ABI)),
    ("deny-partial-mitigations", Refused(ABI)),
    ("dep-info-omit-d-target", Ignored(PRINTS)),
    ("direct-access-external-data", Refused(ABI)),
    ("disable-fast-paths", Ignored(SPEED)),
    ("disable-incr-comp-backend-caching", Ignored(SPEED)),
    ("disable-param-env-normalization-hack", Ignored(ACCEPTS)),
    ("dual-proc-macros", Refused(NOT_BUILT)),
    ("dump-dep-graph", Ignored(PRINTS)),
    ("dump-mir", Ignored(PRINTS)),
    ("dump-mir-dataflow", Ignored(PRINTS)),
    ("dump-mir-dir", Ignored(PRINTS)),
    ("dump-mir-exclude-alloc-bytes", Ignored(PRINTS)),
    ("dump-mir-exclude-pass-number", Ignored(PRINTS)),
    ("dump-mir-graphviz", Ignored(PRINTS)),
    ("dump-mono-stats", Ignored(PRINTS)),
    ("dump-mono-stats-format", Ignored(PRINTS)),
    ("dwarf-version", Ignored(DEBUG)),
    ("dylib-lto", Refused(EXPORTS)),
    ("eagerly-emit-delayed-bugs", Ignored(CHECKS)),
    ("ehcont-guard", Refused(CODE)),
    // Cargo on a nightly toolchain passes `-Z embed-metadata=no` to every crate by itself: the
    // metadata goes to the `.rmeta` file and not into the rlib, whose objects are the same.
    ("embed-metadata", Ignored("where rustc puts a library's metadata; the objects are the same")),
    ("embed-source", Ignored(DEBUG)),
    ("emit-stack-sizes", Refused(CODE)),
    ("enforce-type-length-limit", Ignored(ACCEPTS)),
    ("experimental-default-bounds", Refused(SEMANTICS)),
    ("export-executable-symbols", Refused(LINKS)),
    ("external-clangrt", Refused(LINKS)),
    ("extra-const-ub-checks", Ignored(CHECKS)),
    ("fewer-names", Ignored(RUSTC_BACKEND)),
    ("fixed-x18", Refused(ABI)),
    ("flatten-format-args", Refused(CODE)),
    ("fmt-debug", Refused("it changes what `#[derive(Debug)]` prints")),
    ("force-intrinsic-fallback", Refused(CODE)),
    ("force-unstable-if-unmarked", Refused(SEMANTICS)),
    ("function-return", Refused(CODE)),
    ("function-sections", Ignored(RUSTC_BACKEND)),
    ("future-incompat-test", Ignored(PRINTS)),
    ("graphviz-dark-mode", Ignored(PRINTS)),
    ("graphviz-font", Ignored(PRINTS)),
    ("has-thread-local", Refused("it changes what `cfg(target_thread_local)` says")),
    ("help", Refused(NOT_BUILT)),
    ("higher-ranked-assumptions", Ignored(ACCEPTS)),
    ("hint-mostly-unused", Refused(EXPORTS)),
    ("hint-msrv", Ignored(PRINTS)),
    ("human-readable-cgu-names", Ignored(RUSTC_BACKEND)),
    ("identify-regions", Ignored(PRINTS)),
    ("ignore-directory-in-diagnostics-source-blocks", Ignored(PRINTS)),
    ("implicit-sysroot-deps", Refused(LINKS)),
    ("incremental-ignore-spans", Ignored(SPEED)),
    ("incremental-info", Ignored(PRINTS)),
    ("incremental-verify-ich", Ignored(CHECKS)),
    ("indirect-branch-cs-prefix", Refused(CODE)),
    ("inline-llvm", Ignored(RUSTC_BACKEND)),
    ("inline-mir", Refused(EXPORTS)),
    ("inline-mir-forwarder-threshold", Refused(EXPORTS)),
    ("inline-mir-hint-threshold", Refused(EXPORTS)),
    ("inline-mir-preserve-debug", Ignored(DEBUG)),
    ("inline-mir-threshold", Refused(EXPORTS)),
    ("input-stats", Ignored(PRINTS)),
    ("instrument-mcount", Refused(INSTRUMENTS)),
    ("instrument-xray", Refused(INSTRUMENTS)),
    ("internal-testing-features", Refused(SEMANTICS)),
    ("large-data-threshold", Refused(CODE)),
    ("layout-seed", Refused("it changes the layout of types")),
    ("link-directives", Refused(LINKS)),
    ("link-native-libraries", Refused(LINKS)),
    ("link-only", Refused(LINKS)),
    ("lint-llvm-ir", Ignored(CHECKS)),
    ("lint-mir", Ignored(CHECKS)),
    ("llvm-module-flag", Refused(CODE)),
    ("llvm-plugins", Refused(CODE)),
    ("llvm-target-feature", Refused(ABI)),
    ("llvm-time-trace", Ignored(PRINTS)),
    ("llvm-writable", Ignored(RUSTC_BACKEND)),
    ("location-detail", Refused("it changes what `Location::caller()` says")),
    ("ls", Refused(NOT_BUILT)),
    ("macro-backtrace", Ignored(PRINTS)),
    ("macro-stats", Ignored(PRINTS)),
    ("maximal-hir-to-mir-coverage", Refused(INSTRUMENTS)),
    ("merge-functions", Ignored(RUSTC_BACKEND)),
    ("meta-stats", Ignored(PRINTS)),
    ("metadata-crate-hash", Refused(ABI)),
    ("metrics-dir", Ignored(PRINTS)),
    ("min-function-alignment", Refused(CODE)),
    ("min-recursion-limit", Ignored(ACCEPTS)),
    ("mir-enable-passes", Refused(CODE)),
    ("mir-include-spans", Ignored(PRINTS)),
    ("mir-move-elimination", Refused(CODE)),
    ("mir-opt-bisect-limit", Refused(CODE)),
    ("mir-opt-level", Refused(EXPORTS)),
    ("mir-preserve-ub", Refused(CODE)),
    ("mir-strip-debuginfo", Ignored(DEBUG)),
    ("move-size-limit", Ignored(PRINTS)),
    ("namespaced-crates", Refused(SEMANTICS)),
    ("next-solver", Ignored(ACCEPTS)),
    ("nll-facts", Ignored(PRINTS)),
    ("nll-facts-dir", Ignored(PRINTS)),
    ("no-analysis", Refused(NOT_BUILT)),
    ("no-codegen", Refused(NOT_BUILT)),
    ("no-generate-arange-section", Ignored(DEBUG)),
    ("no-implied-bounds-compat", Ignored(ACCEPTS)),
    ("no-leak-check", Refused(SEMANTICS)),
    ("no-link", Refused(LINKS)),
    ("no-parallel-backend", Ignored(SPEED)),
    ("no-profiler-runtime", Refused(LINKS)),
    ("no-steal-thir", Ignored(SPEED)),
    ("no-trait-vptr", Refused("it changes the layout of vtables")),
    ("no-unique-section-names", Ignored(RUSTC_BACKEND)),
    ("normalize-docs", Ignored("what rustdoc documents")),
    ("offload", Refused(CODE)),
    ("on-broken-pipe", Refused(SEMANTICS)),
    ("osx-rpath-install-name", Refused(LINKS)),
    ("packed-bundled-libs", Refused(LINKS)),
    ("packed-stack", Refused(ABI)),
    (
        "panic-abort-tests",
        Refused("blaze's test harness does not run tests built with panic=abort"),
    ),
    ("panic-in-drop", Refused(SEMANTICS)),
    ("parse-crate-root-only", Refused(NOT_BUILT)),
    ("patchable-function-entry", Refused(CODE)),
    ("plt", Ignored(RUSTC_BACKEND)),
    ("pointer-authentication", Refused(ABI)),
    ("polonius", Ignored(ACCEPTS)),
    ("pre-link-arg", Refused(LINKS)),
    ("pre-link-args", Refused(LINKS)),
    ("precise-enum-drop-elaboration", Refused(CODE)),
    ("print-codegen-stats", Ignored(PRINTS)),
    ("print-codegen-stats-json", Ignored(PRINTS)),
    ("print-llvm-passes", Ignored(PRINTS)),
    ("print-mono-items", Ignored(PRINTS)),
    ("print-type-sizes", Ignored(PRINTS)),
    ("proc-macro-backtrace", Ignored(PRINTS)),
    ("proc-macro-execution-strategy", Refused("blaze runs proc macros its own way")),
    ("profile-closures", Ignored(PRINTS)),
    ("profiler-runtime", Refused(LINKS)),
    ("query-dep-graph", Ignored(CHECKS)),
    ("randomize-layout", Refused("it changes the layout of types")),
    ("reg-struct-return", Refused(ABI)),
    ("regparm", Refused(ABI)),
    ("relax-elf-relocations", Ignored(RUSTC_BACKEND)),
    ("remap-cwd-prefix", Refused("it changes the paths `file!()` and panics name")),
    ("remark-dir", Ignored(PRINTS)),
    ("renormalize-rigid-aliases", Ignored(ACCEPTS)),
    ("retpoline", Refused(CODE)),
    ("retpoline-external-thunk", Refused(CODE)),
    ("sanitizer", Refused(INSTRUMENTS)),
    ("sanitizer-cfi-canonical-jump-tables", Refused(INSTRUMENTS)),
    ("sanitizer-cfi-diag", Refused(INSTRUMENTS)),
    ("sanitizer-cfi-generalize-pointers", Refused(INSTRUMENTS)),
    ("sanitizer-cfi-minimal-runtime", Refused(INSTRUMENTS)),
    ("sanitizer-cfi-normalize-integers", Refused(INSTRUMENTS)),
    ("sanitizer-cfi-recover", Refused(INSTRUMENTS)),
    ("sanitizer-dataflow-abilist", Refused(INSTRUMENTS)),
    ("sanitizer-ignorelist", Refused(INSTRUMENTS)),
    ("sanitizer-kcfi-arity", Refused(INSTRUMENTS)),
    ("sanitizer-memory-track-origins", Refused(INSTRUMENTS)),
    ("sanitizer-recover", Refused(INSTRUMENTS)),
    ("self-profile", Ignored(PRINTS)),
    ("self-profile-counter", Ignored(PRINTS)),
    ("self-profile-events", Ignored(PRINTS)),
    ("share-generics", Refused(EXPORTS)),
    ("shell-argfiles", Ignored("how rustc reads its own argument files")),
    ("simulate-remapped-rust-src-base", Refused("it changes the paths panics name")),
    ("small-data-threshold", Refused(CODE)),
    ("span-debug", Refused(SEMANTICS)),
    ("span-free-formats", Ignored(PRINTS)),
    ("split-dwarf-inlining", Ignored(DEBUG)),
    ("split-dwarf-kind", Ignored(DEBUG)),
    ("split-dwarf-out-dir", Ignored(DEBUG)),
    ("split-lto-unit", Refused(EXPORTS)),
    ("src-hash-algorithm", Ignored(DEBUG)),
    ("stack-protector", Refused(CODE)),
    ("staticlib-allow-rdylib-deps", Refused(LINKS)),
    ("staticlib-hide-internal-symbols", Refused(LINKS)),
    ("staticlib-prefer-dynamic", Refused(LINKS)),
    ("staticlib-rename-internal-symbols", Refused(LINKS)),
    ("strict-init-checks", Refused(SEMANTICS)),
    ("teach", Ignored(PRINTS)),
    ("temps-dir", Ignored(PRINTS)),
    ("terminal-urls", Ignored(PRINTS)),
    ("thinlto", Refused(EXPORTS)),
    ("threads", Ignored(SPEED)),
    ("time-llvm-passes", Ignored(PRINTS)),
    ("time-passes", Ignored(PRINTS)),
    ("time-passes-format", Ignored(PRINTS)),
    ("tiny-const-eval-limit", Refused(SEMANTICS)),
    ("tls-model", Refused(ABI)),
    ("trace-macros", Ignored(PRINTS)),
    ("track-diagnostics", Ignored(PRINTS)),
    ("translate-remapped-path-to-local-path", Refused("it changes the paths panics name")),
    ("trap-unreachable", Ignored(RUSTC_BACKEND)),
    ("treat-err-as-bug", Ignored(CHECKS)),
    ("trim-diagnostic-paths", Ignored(PRINTS)),
    ("tune-cpu", Ignored(RUSTC_BACKEND)),
    ("ub-checks", OffOnly("blaze compiles `ub_checks()` as false")),
    ("ui-testing", Ignored(PRINTS)),
    ("uninit-const-chunk-threshold", Refused(CODE)),
    ("unleash-the-miri-inside-of-you", Refused(SEMANTICS)),
    ("unpretty", Refused(NOT_BUILT)),
    ("unsound-mir-opts", Refused(CODE)),
    ("unstable-options", Ignored("it lets other unstable options be given, and changes nothing")),
    ("use-ctors-section", Refused(LINKS)),
    ("use-sync-unwind", Refused(CODE)),
    ("validate-mir", Ignored(CHECKS)),
    ("verbose-asm", Ignored(PRINTS)),
    ("verbose-internals", Ignored(PRINTS)),
    ("verify-llvm-ir", Ignored(CHECKS)),
    ("virtual-function-elimination", Refused(EXPORTS)),
    ("wasi-exec-model", Refused(LINKS)),
    ("wasm-c-abi", Refused(ABI)),
    ("wasm-proc-macros", Refused(NOT_BUILT)),
    ("write-long-types-to-disk", Ignored(PRINTS)),
];

/// What a flag the table does not have gets.
const UNKNOWN: &str = "blaze does not know the flag";

/// One `-Z` flag as given: its name with `_` spelled `-`, as rustc reads both, and its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag<'a> {
    pub name: String,
    pub value: Option<&'a str>,
    /// As it was given, after the `-Z`.
    pub given: &'a str,
}

impl Flag<'_> {
    /// What blaze does with it.
    pub fn treatment(&self) -> Treatment {
        let row = TABLE.binary_search_by(|(name, _)| name.cmp(&self.name.as_str()));
        match row.map(|at| TABLE[at].1) {
            Err(_) => Treatment::Refused(UNKNOWN),
            Ok(Ignored(why)) => Treatment::Ignored(why),
            Ok(Refused(why)) => Treatment::Refused(why),
            Ok(OffOnly(why)) => match off(self.value) {
                Some(true) => Treatment::Modelled(why),
                Some(false) => Treatment::Refused(why),
                None => Treatment::Refused("blaze does not know the value"),
            },
        }
    }

    /// Why blaze does not build code with it, naming it, when it does not.
    pub fn refusal(&self) -> Option<String> {
        match self.treatment() {
            Treatment::Refused(why) => Some(format!("-Z {}: {why}", self.given)),
            Treatment::Ignored(_) | Treatment::Modelled(_) => None,
        }
    }
}

/// Whether a boolean flag's value turns it off, as rustc reads one: no value is on.
fn off(value: Option<&str>) -> Option<bool> {
    match value {
        None | Some("y" | "yes" | "on" | "true") => Some(false),
        Some("n" | "no" | "off" | "false") => Some(true),
        Some(_) => None,
    }
}

/// The `-Z` flags among the words of a rustc command line, spelled `-Z name=value`,
/// `-Zname=value`, `-Z name` or `-Zname`.
pub fn flags<'a, S: AsRef<str> + 'a>(words: impl IntoIterator<Item = &'a S>) -> Vec<Flag<'a>> {
    let mut words = words.into_iter().map(AsRef::as_ref);
    let mut found = Vec::new();
    while let Some(word) = words.next() {
        let given = match word.strip_prefix("-Z") {
            Some("") => match words.next() {
                Some(next) => next,
                None => break,
            },
            Some(joined) => joined,
            None => continue,
        };
        found.push(flag(given));
    }
    found
}

/// The flag `given` is, after its `-Z`.
pub fn flag(given: &str) -> Flag<'_> {
    let (name, value) = match given.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (given, None),
    };
    Flag { name: name.replace('_', "-"), value, given }
}

/// Why blaze does not build code with the flags of a rustc command line, naming the first
/// flag it refuses, when it does not.
pub fn refusal<'a, S: AsRef<str> + 'a>(words: impl IntoIterator<Item = &'a S>) -> Option<String> {
    flags(words).iter().find_map(Flag::refusal)
}

/// Why a build whose [`sanitize`] is not empty stops, until blaze's linker links what an
/// instrumented program calls.
pub const UNLINKED: &str = "the build is sanitized (`-Z sanitizer`, or SanitizerCoverage by `-C \
    passes=sancov-module`), and blaze's linker has no sanitizer runtimes or SanitizerCoverage \
    sections yet, so the program can't be linked";

/// Checks that the toolchain at `sysroot` has the runtime of each of `sanitizers` for `target`,
/// as rustc's `link_sanitizer_runtime` names it in `lib/rustlib/<target>/lib/`: a dylib on
/// Apple targets, an archive elsewhere. An error names the sanitizer and the path looked at;
/// blaze's linker checks the same file when it links it.
pub fn runtimes(sanitizers: Sanitizers, target: &str, sysroot: &std::path::Path) -> Result<(), String> {
    let apple = target.contains("-apple-");
    let dir = sysroot.join("lib/rustlib").join(target).join("lib");
    for (one, short) in [(Sanitizers::ADDRESS, "asan"), (Sanitizers::THREAD, "tsan"), (Sanitizers::MEMORY, "msan")] {
        if !sanitizers.contains(one) {
            continue;
        }
        let name = sanitizer_name(one);
        if target.contains("-windows-") {
            return Err(format!("`-Zsanitizer={name}` on `{target}`: blaze links no sanitizer runtime for Windows"));
        }
        if apple && one == Sanitizers::MEMORY {
            return Err(format!("`-Zsanitizer={name}` on `{target}`: rustc has no memory sanitizer runtime for Apple targets"));
        }
        let file = dir.join(format!("librustc-nightly_rt.{short}.{}", if apple { "dylib" } else { "a" }));
        if !file.is_file() {
            return Err(format!("`-Zsanitizer={name}` needs its runtime `{}`, which blaze's toolchain does not have", file.display()));
        }
    }
    Ok(())
}

/// The set of each of [`SANITIZERS`], in its order.
const SANITIZER_SETS: [Sanitizers; 3] = [Sanitizers::ADDRESS, Sanitizers::MEMORY, Sanitizers::THREAD];

/// The pairs rustc refuses together (`SanitizerSet::MUTUALLY_EXCLUSIVE`), of those blaze has.
const EXCLUSIVE: [(Sanitizers, Sanitizers); 3] = [
    (Sanitizers::ADDRESS, Sanitizers::MEMORY),
    (Sanitizers::ADDRESS, Sanitizers::THREAD),
    (Sanitizers::MEMORY, Sanitizers::THREAD),
];

/// SanitizerCoverage's switches, `-sanitizer-coverage-<name>`, as LLVM names them.
const COVERAGE_SWITCHES: &[(&str, u16)] = &[
    ("control-flow", Coverage::CONTROL_FLOW),
    ("inline-8bit-counters", Coverage::INLINE_8BIT_COUNTERS),
    ("inline-bool-flag", Coverage::INLINE_BOOL_FLAG),
    ("pc-table", Coverage::PC_TABLE),
    ("stack-depth", Coverage::STACK_DEPTH),
    ("trace-compares", Coverage::TRACE_CMP),
    ("trace-divs", Coverage::TRACE_DIV),
    ("trace-geps", Coverage::TRACE_GEP),
    ("trace-loads", Coverage::TRACE_LOADS),
    ("trace-pc", Coverage::TRACE_PC),
    ("trace-pc-entry-exit", Coverage::TRACE_PC_ENTRY_EXIT),
    ("trace-pc-guard", Coverage::TRACE_PC_GUARD),
    ("trace-stores", Coverage::TRACE_STORES),
];

/// The name of one of [`SANITIZER_SETS`].
fn sanitizer_name(one: Sanitizers) -> &'static str {
    match SANITIZER_SETS.iter().position(|it| *it == one) {
        Some(at) => SANITIZERS[at],
        None => unreachable!("a set of one of blaze's sanitizers"),
    }
}

/// A name of `-Z sanitizer`'s list as rustc reads it ([`target::sanitizer`]).
fn sanitizer(name: &str, given: &str) -> Result<Sanitizers, String> {
    target::sanitizer(name, given).map(|at| SANITIZER_SETS[at])
}

/// The comma-separated names of a list flag; rustc refuses one with no value.
fn names<'a>(flag: &Flag<'a>) -> Result<std::str::Split<'a, char>, String> {
    flag.value
        .map(|value| value.split(','))
        .ok_or_else(|| format!("-Z {}: rustc wants a comma-separated list of sanitizers", flag.given))
}

/// The build's sanitizers as rustc takes them from the words of its command line:
/// `-Z sanitizer`, `-Z sanitizer-recover` and `-Z sanitizer-memory-track-origins`, each list
/// adding to the last as rustc's does, and SanitizerCoverage (`coverage`). Canonical, so two
/// builds whose code is the same have one key: what recovers is only what is on and can
/// recover (rustc passes LLVM nothing else), and origins are tracked only under `memory`.
pub fn sanitize<S: AsRef<str>>(words: &[S]) -> Result<Sanitize, String> {
    let mut sanitizers = Sanitizers::NONE;
    let mut recover = Sanitizers::NONE;
    let mut track_origins = 0;
    for flag in flags(words) {
        match flag.name.as_str() {
            "sanitizer" => {
                for name in names(&flag)? {
                    sanitizers = sanitizers.union(sanitizer(name, flag.given)?);
                }
            }
            "sanitizer-recover" => {
                for name in names(&flag)? {
                    recover = recover.union(match name {
                        "address" => Sanitizers::ADDRESS,
                        "memory" => Sanitizers::MEMORY,
                        "thread" => Sanitizers::NONE,
                        _ if UNSANITIZED.contains(&name) => Sanitizers::NONE,
                        _ => return Err(format!("-Z {}: rustc has no sanitizer `{name}`", flag.given)),
                    });
                }
            }
            "sanitizer-memory-track-origins" => {
                track_origins = match flag.value {
                    None | Some("2") => 2,
                    Some("1") => 1,
                    Some("0") => 0,
                    Some(_) => return Err(format!("-Z {}: rustc takes 0, 1 or 2", flag.given)),
                };
            }
            _ => {}
        }
    }
    if let Some((a, b)) = EXCLUSIVE.into_iter().find(|(a, b)| sanitizers.contains(*a) && sanitizers.contains(*b)) {
        return Err(format!(
            "`-Zsanitizer={}` is incompatible with `-Zsanitizer={}`",
            sanitizer_name(a),
            sanitizer_name(b)
        ));
    }
    Ok(Sanitize {
        sanitizers,
        recover: Sanitizers(recover.0 & sanitizers.0),
        track_origins: if sanitizers.contains(Sanitizers::MEMORY) { track_origins } else { 0 },
        coverage: coverage(words)?,
    })
}

/// The `-C` flags among the words of a rustc command line, each `(name, value)` with `_`
/// spelled `-`: `-C name=value`, `-Cname=value`, `--codegen name=value`, `--codegen=name=value`.
fn codegen_flags<'a, S: AsRef<str> + 'a>(
    words: impl IntoIterator<Item = &'a S>,
) -> Vec<(String, Option<&'a str>)> {
    let mut words = words.into_iter().map(AsRef::as_ref);
    let mut found = Vec::new();
    while let Some(word) = words.next() {
        let given = match word.strip_prefix("-C").or_else(|| word.strip_prefix("--codegen")) {
            Some("") => match words.next() {
                Some(next) => next,
                None => break,
            },
            Some(joined) if word.starts_with("--codegen") => match joined.strip_prefix('=') {
                Some(given) => given,
                None => continue,
            },
            Some(joined) => joined,
            None => continue,
        };
        let (name, value) = match given.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (given, None),
        };
        found.push((name.replace('_', "-"), value));
    }
    found
}

/// A `cl::opt<bool>`'s value as LLVM reads one: none is on.
fn llvm_bool(value: Option<&str>, arg: &str) -> Result<bool, String> {
    match value {
        None | Some("true" | "TRUE" | "True" | "1") => Ok(true),
        Some("false" | "FALSE" | "False" | "0") => Ok(false),
        Some(_) => Err(format!("-C llvm-args={arg}: LLVM takes true or false")),
    }
}

/// SanitizerCoverage as rustc gets it: `-C passes` naming `sancov-module`, with
/// `-sanitizer-coverage-level=N` and the switches of `-C llvm-args`, the last of each winning as
/// in LLVM's options. LLVM instruments nothing at a level other than 1 to 4, and without the
/// pass nothing reads the options: both are `Coverage::default()`, a plain build's key. An
/// option blaze does not instrument with is an error only when the pass runs.
fn coverage<S: AsRef<str>>(words: &[S]) -> Result<Coverage, String> {
    let mut pass = false;
    let mut level: i64 = 0;
    let mut flags = 0u16;
    let mut unknown: Option<&str> = None;
    for (name, value) in codegen_flags(words) {
        let Some(value) = value else { continue };
        match name.as_str() {
            "passes" => pass |= value.split_whitespace().any(|it| it == "sancov-module"),
            "llvm-args" => {
                for arg in value.split_whitespace() {
                    let option = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'));
                    let Some(option) = option.and_then(|it| it.strip_prefix("sanitizer-coverage-")) else {
                        continue;
                    };
                    let (option, given) = match option.split_once('=') {
                        Some((option, given)) => (option, Some(given)),
                        None => (option, None),
                    };
                    match option {
                        "level" => {
                            level = given.and_then(|it| it.parse().ok()).ok_or_else(|| {
                                format!("-C llvm-args={arg}: LLVM takes a number for the level")
                            })?;
                        }
                        "prune-blocks" => match llvm_bool(given, arg)? {
                            true => flags &= !Coverage::NO_PRUNE,
                            false => flags |= Coverage::NO_PRUNE,
                        },
                        _ => match COVERAGE_SWITCHES.iter().find(|it| it.0 == option) {
                            Some(&(_, bit)) => match llvm_bool(given, arg)? {
                                true => flags |= bit,
                                false => flags &= !bit,
                            },
                            None => {
                                unknown.get_or_insert(arg);
                            }
                        },
                    }
                }
            }
            _ => {}
        }
    }
    if !pass || !(1..=4).contains(&level) {
        return Ok(Coverage::default());
    }
    if let Some(arg) = unknown {
        return Err(format!(
            "-C llvm-args={arg}: blaze's code generator does not instrument with that SanitizerCoverage option"
        ));
    }
    Ok(Coverage { level: level as u8, flags })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_has_each_name_once() {
        assert!(TABLE.windows(2).all(|it| it[0].0 < it[1].0), "TABLE must be sorted by name");
    }

    #[test]
    fn spellings() {
        let words =
            ["-Zthreads=8", "-Z", "time-passes", "--cfg", "x", "-Z", "ub_checks=no", "-Zfoo"];
        let found = flags(&words);
        let names: Vec<_> = found.iter().map(|it| (it.name.as_str(), it.value)).collect();
        assert_eq!(
            names,
            [
                ("threads", Some("8")),
                ("time-passes", None),
                ("ub-checks", Some("no")),
                ("foo", None)
            ]
        );
        assert_eq!(found[2].given, "ub_checks=no");
        // A `-Z` at the end has nothing to name.
        assert!(flags(&["-Z"]).is_empty());
    }

    #[test]
    fn treatments() {
        let of = |given| flag(given).treatment();
        assert!(matches!(of("threads=8"), Treatment::Ignored(_)));
        assert!(matches!(of("embed-metadata=no"), Treatment::Ignored(_)));
        assert!(matches!(of("ub-checks=no"), Treatment::Modelled(_)));
        assert!(matches!(of("ub-checks"), Treatment::Refused(_)));
        assert!(matches!(of("ub-checks=yes"), Treatment::Refused(_)));
        assert!(matches!(of("ub-checks=maybe"), Treatment::Refused(_)));
        assert!(matches!(of("share-generics=y"), Treatment::Refused(_)));
        assert_eq!(of("no-such-flag"), Treatment::Refused(UNKNOWN));
    }

    #[test]
    fn a_refusal_names_the_flag() {
        assert_eq!(refusal(&["-Zthreads=8", "-Z", "embed-metadata=no"]), None);
        let why = refusal(&["-Zthreads=8", "-Z", "no_such_flag=3"]).unwrap();
        assert_eq!(why, format!("-Z no_such_flag=3: {UNKNOWN}"));
    }

    #[test]
    fn a_sanitizer_runtime_the_toolchain_lacks_is_named() {
        let sysroot = std::env::temp_dir().join(format!("blaze-sanitizer-runtimes-{}", std::process::id()));
        let lib = sysroot.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("librustc-nightly_rt.asan.a"), b"").unwrap();
        let linux = "x86_64-unknown-linux-gnu";
        assert_eq!(runtimes(Sanitizers::ADDRESS, linux, &sysroot), Ok(()));
        assert_eq!(runtimes(Sanitizers::NONE, "aarch64-apple-darwin", &sysroot), Ok(()));
        let thread = runtimes(Sanitizers::THREAD, linux, &sysroot).unwrap_err();
        assert!(thread.starts_with("`-Zsanitizer=thread` needs its runtime `"), "{thread}");
        assert!(thread.contains(&lib.join("librustc-nightly_rt.tsan.a").display().to_string()), "{thread}");
        let apple = runtimes(Sanitizers::ADDRESS, "aarch64-apple-darwin", &sysroot).unwrap_err();
        assert!(apple.contains("librustc-nightly_rt.asan.dylib"), "{apple}");
        assert!(runtimes(Sanitizers::MEMORY, "aarch64-apple-darwin", &sysroot).unwrap_err().contains("Apple"));
        assert!(runtimes(Sanitizers::ADDRESS, "x86_64-pc-windows-msvc", &sysroot).unwrap_err().contains("Windows"));
        std::fs::remove_dir_all(&sysroot).unwrap();
    }

    #[test]
    fn the_sanitizer_flags_stay_refused_until_they_link() {
        for given in ["sanitizer=address", "sanitizer-recover=address", "sanitizer-memory-track-origins"] {
            assert!(matches!(flag(given).treatment(), Treatment::Refused(_)), "{given}");
        }
    }

    #[test]
    fn sanitizers_as_rustc_reads_them() {
        assert_eq!(sanitize(&["-Copt-level=0", "-Zthreads=8"]).unwrap(), Sanitize::default());
        let address = sanitize(&["-Zsanitizer=address", "-Z", "sanitizer_recover=address,thread,memory"]).unwrap();
        assert_eq!((address.sanitizers, address.recover), (Sanitizers::ADDRESS, Sanitizers::ADDRESS));
        assert!(!address.is_empty());
        let memory = sanitize(&["-Zsanitizer=memory", "-Zsanitizer-memory-track-origins"]).unwrap();
        assert_eq!((memory.sanitizers, memory.track_origins), (Sanitizers::MEMORY, 2));
        let origins = sanitize(&["-Zsanitizer=address", "-Zsanitizer-memory-track-origins=1"]).unwrap();
        assert_eq!(origins.track_origins, 0);
        // A recover of what is not on changes no code, as in rustc.
        assert_eq!(sanitize(&["-Zsanitizer-recover=address"]).unwrap(), Sanitize::default());
        // Each list adds to the last.
        let twice = sanitize(&["-Zsanitizer=thread", "-Zsanitizer=thread"]).unwrap();
        assert_eq!(twice.sanitizers, Sanitizers::THREAD);
        assert_eq!(
            sanitize(&["-Zsanitizer=address", "-Zsanitizer=thread"]).unwrap_err(),
            "`-Zsanitizer=address` is incompatible with `-Zsanitizer=thread`"
        );
        assert!(sanitize(&["-Zsanitizer=memory,address"]).unwrap_err().contains("incompatible"));
        assert!(sanitize(&["-Zsanitizer=memory,thread"]).unwrap_err().contains("incompatible"));
        assert!(sanitize(&["-Zsanitizer=cfi"]).unwrap_err().contains("the `cfi` sanitizer"));
        assert!(sanitize(&["-Zsanitizer=nope"]).unwrap_err().contains("no sanitizer `nope`"));
        assert!(sanitize(&["-Zsanitizer-recover=nope"]).unwrap_err().contains("no sanitizer `nope`"));
        assert!(sanitize(&["-Zsanitizer"]).is_err());
        assert!(sanitize(&["-Zsanitizer-memory-track-origins=3"]).is_err());
    }

    /// blaze-units' list of sanitizers is this one's, against the match this read them by.
    #[test]
    fn each_sanitizer_is_blaze_units_one() {
        let reference = |name: &str| match name {
            "address" => Some(Sanitizers::ADDRESS),
            "thread" => Some(Sanitizers::THREAD),
            "memory" => Some(Sanitizers::MEMORY),
            _ => None,
        };
        for name in ["address", "thread", "memory", "cfi", "leak", "shadow-call-stack", "nope", ""] {
            assert_eq!(sanitizer(name, name).ok(), reference(name), "{name}");
        }
        for (at, name) in SANITIZERS.iter().enumerate() {
            assert_eq!(sanitizer_name(SANITIZER_SETS[at]), *name);
        }
        assert!(sanitizer("leak", "sanitizer=leak").unwrap_err().contains("does not instrument"));
    }

    #[test]
    fn sanitizer_coverage_as_cargo_fuzz_passes_it() {
        let fuzz = [
            "-Cpasses=sancov-module",
            "-Cllvm-args=-sanitizer-coverage-level=4",
            "-C",
            "llvm-args=-sanitizer-coverage-inline-8bit-counters -sanitizer-coverage-pc-table",
            "--codegen=llvm-args=--sanitizer-coverage-trace-compares",
        ];
        let flags = Coverage::INLINE_8BIT_COUNTERS | Coverage::PC_TABLE | Coverage::TRACE_CMP;
        assert_eq!(sanitize(&fuzz).unwrap().coverage, Coverage { level: 4, flags });
        // Without the pass, or at level 0, LLVM instruments nothing: a plain build's key.
        assert_eq!(sanitize(&fuzz[1..]).unwrap(), Sanitize::default());
        let level0 = ["-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-trace-pc"];
        assert_eq!(sanitize(&level0).unwrap().coverage, Coverage::default());
        let level5 = ["-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-level=5"];
        assert_eq!(sanitize(&level5).unwrap().coverage, Coverage::default());
        // The last of a switch wins; `prune-blocks=0` is `NO_PRUNE`.
        let last = [
            "-C",
            "passes=sancov-module",
            "-Cllvm-args=-sanitizer-coverage-level=2 -sanitizer-coverage-trace-pc \
             -sanitizer-coverage-trace-pc=false -sanitizer-coverage-prune-blocks=0",
        ];
        assert_eq!(sanitize(&last).unwrap().coverage, Coverage { level: 2, flags: Coverage::NO_PRUNE });
        let unknown = ["-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-level=1 -sanitizer-coverage-drop-ctors"];
        assert!(sanitize(&unknown).unwrap_err().contains("-sanitizer-coverage-drop-ctors"));
        assert_eq!(sanitize(&unknown[1..]).unwrap(), Sanitize::default());
        assert!(sanitize(&["-Cllvm-args=-sanitizer-coverage-level=x"]).is_err());
        assert!(sanitize(&["-Cllvm-args=-sanitizer-coverage-pc-table=maybe"]).is_err());
    }
}
