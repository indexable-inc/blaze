//! What a build script in the sandbox may tell cargo. A script is code from anywhere, and what
//! it prints goes into every rustc and linker run of its package and of what links it: a
//! `rustc-link-search` into the home directory, a `rustc-link-arg` that loads a plugin into
//! the linker, or a `rustc-env` that overrides `CARGO_PKG_VERSION` reaches past the sandbox it
//! ran in. So the directives of a sandboxed run are checked before cargo reads them
//! ([`refused`]), and one that breaks a rule fails the build, naming the script, the line and
//! the rule (`blaze explain sandbox:link-dir`, [`blaze_report::rules`]):
//!
//! - `rustc-link-search` and `rustc-flags`' `-L`: a directory inside the script's `OUT_DIR`,
//!   its package, the target directory's build directory (the `OUT_DIR`s of its dependencies,
//!   which `DEP_*` names), a system directory ([`SYSTEM`], and what the workspace adds in
//!   `build-script-link-dirs`), or a layout of blaze's own store of pinned native inputs
//!   ([`Ran::native`]).
//! - `rustc-link-arg` (and its `-bins`, `-bin=`, `-tests`, `-benches`, `-examples` and
//!   `-cdylib` forms): one of [`LINK_ARGS`], one of [`LINK_ARG_PREFIXES`] with one value that
//!   is no further linker word, or an rpath to a directory `rustc-link-search` may name.
//! - `rustc-env`: not a variable cargo gives a crate itself, nor one rustc, rustdoc or cargo
//!   reads when building it ([`given_by_cargo`]).
//!
//! What counting these rules over the bench corpus found (590 scripts, 2026-10-27) decided
//! their edges: no
//! `rustc-link-search` outside those directories, `-pthread` and an rpath into the Nix store
//! the only link arguments outside the list, and `CARGO_COMMIT_HASH` (cargo's own build) the
//! only `CARGO_*` a script set, which cargo never sets for a crate.
//!
//! A run outside the sandbox, which its workspace took out by name, is not checked: it can do
//! anything anyway.

use std::path::{Path, PathBuf};

/// The system's directories of libraries, which the linker searches anyway: naming one adds
/// nothing a script could not link already. The Nix store is the closure a script's tools
/// come from; Homebrew's prefixes and Apple's SDKs are where a Mac's libraries are.
pub const SYSTEM: [&str; 11] = [
    "/usr/lib",
    "/usr/lib64",
    "/usr/local/lib",
    "/lib",
    "/lib64",
    "/nix/store",
    "/opt/homebrew/lib",
    "/opt/homebrew/opt",
    "/usr/local/opt",
    "/Library/Developer/CommandLineTools/SDKs",
    "/Applications/Xcode.app/Contents/Developer/Platforms",
];

/// The link arguments a script may give, whole: what changes how the output is linked, never
/// which program links it or what it loads.
pub const LINK_ARGS: [&str; 20] = [
    "-pthread",
    "-rdynamic",
    "-nostartfiles",
    "-Wl,--as-needed",
    "-Wl,--no-as-needed",
    "-Wl,--gc-sections",
    "-Wl,--export-dynamic",
    "-Wl,-z,relro",
    "-Wl,-z,now",
    "-Wl,-z,noexecstack",
    "-Wl,-z,nodelete",
    "-Wl,-undefined,dynamic_lookup",
    "-Wl,-dead_strip",
    "-Wl,-headerpad_max_install_names",
    "-headerpad_max_install_names",
    "-Wl,-z,defs",
    // pyo3's `extension-module` on macOS: two arguments, `-undefined` then its value. Neither
    // names a path, and blaze's linker reads `dynamic_lookup` only after `-undefined`.
    "-undefined",
    "dynamic_lookup",
    // Apple's: load every member of an archive that defines an Objective-C class or category.
    "-ObjC",
    "-Wl,-ObjC",
];

/// The link arguments a script may give with a value of its own after the prefix: a symbol
/// or a pattern of symbols to export, a library's own name, a version script. The value is
/// one word of the linker's ([`one_value`]). `--export-dynamic-symbol` only adds names the
/// image already defines to its dynamic symbol table, as `--export-dynamic` adds them all:
/// it loads nothing and names no file (evm2-jit gives `__revmc_builtin_*` so, for the code
/// its JIT makes to find them).
const LINK_ARG_PREFIXES: [&str; 6] = [
    "-Wl,--export-dynamic-symbol=",
    "-Wl,--export-dynamic-symbol,",
    "-Wl,-soname,",
    "-Wl,-install_name,@rpath/",
    "-Wl,-exported_symbol,",
    "-Wl,--version-script=",
];

/// Where a script ran, which decides what its directives may name.
pub struct Ran<'a> {
    pub package_dir: &'a Path,
    pub out_dir: &'a Path,
    /// The system directories the workspace allows besides [`SYSTEM`].
    pub link_dirs: &'a [PathBuf],
    /// blaze's store of pinned native inputs, `<BLAZE_HOME>/native`, when the daemon has a
    /// home: each `sha256-<digest>` directly under it is the read-only unpacking of the
    /// archive of that digest, which blaze laid out whole (docs/cargo.md, "Pinned native
    /// inputs"), so one may be searched and be an rpath as a directory of `/nix/store` may,
    /// whichever workspace pinned it: a machine whose only LLVM is the one blaze fetched
    /// points `LLVM_SYS_231_PREFIX` at it.
    pub native: Option<&'a Path>,
}

impl Ran<'_> {
    /// The target directory's build directory, whose subdirectories are every package's
    /// `OUT_DIR`: `<build>/<package>-<hash>/out` or `<build>/<package>/<hash>/out`.
    fn build_dir(&self) -> Option<&Path> {
        let run_dir = self.out_dir.parent()?;
        let above = run_dir.parent()?;
        match above.file_name().is_some_and(|it| it == "build") {
            true => Some(above),
            false => above.parent().filter(|it| it.file_name().is_some_and(|it| it == "build")),
        }
    }

    /// Whether `rustc-link-search` may name `dir`.
    fn may_search(&self, dir: &str) -> bool {
        let dir = Path::new(dir);
        if !dir.is_absolute() {
            // A relative one is relative to where rustc runs, the package's directory.
            return !dir.components().any(|it| it == std::path::Component::ParentDir);
        }
        let dir = blaze_manifest::paths::normalize_path(dir);
        if self.in_native_layout(&dir) {
            return true;
        }
        [self.out_dir, self.package_dir]
            .into_iter()
            .chain(self.build_dir())
            .chain(SYSTEM.iter().map(Path::new))
            .chain(self.link_dirs.iter().map(PathBuf::as_path))
            .any(|it| dir.starts_with(it))
    }

    /// Whether `dir`, normalized, is a layout of [`Self::native`] or inside one: under a
    /// `sha256-` and 64 lowercase hex digits directly below the store, never the store itself
    /// or what a layout under way leaves beside the layouts.
    fn in_native_layout(&self, dir: &Path) -> bool {
        let Some(below) = self.native.and_then(|it| dir.strip_prefix(it).ok()) else {
            return false;
        };
        below.components().next().is_some_and(|it| {
            it.as_os_str().to_str().and_then(|it| it.strip_prefix("sha256-")).is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'))
            })
        })
    }

    /// Why the link argument `arg` is refused, `None` when it is not.
    fn link_arg_refused(&self, arg: &str) -> Option<String> {
        let rpath =
            ["-Wl,-rpath,", "-Wl,-rpath=", "-Wl,-R"].iter().find_map(|it| arg.strip_prefix(it));
        if let Some(dir) = rpath {
            let relative = ["$ORIGIN", "@loader_path", "@executable_path"];
            return match relative.iter().any(|it| dir.starts_with(it)) || self.may_search(dir) {
                true => None,
                false => Some(format!("an rpath to `{dir}`, which is none of {}", self.places())),
            };
        }
        let known = LINK_ARGS.contains(&arg)
            || LINK_ARG_PREFIXES.iter().any(|it| arg.strip_prefix(it).is_some_and(one_value))
            || arg.strip_prefix("-l").is_some_and(|name| {
                !name.is_empty()
                    && name.chars().all(|it| it.is_ascii_alphanumeric() || "_+.-".contains(it))
            });
        (!known).then(|| {
            format!("a link argument that is not one a build script may give ({})", allowed_args())
        })
    }

    fn places(&self) -> String {
        "the script's OUT_DIR, its package, the build directory, a system directory, a layout \
         of blaze's pinned native inputs or one `build-script-link-dirs` names"
            .to_owned()
    }
}

/// Whether a value after a prefix is one word of the linker's that names no path above where
/// it is. `-Wl,` splits at every comma, so a value with one is more words: after
/// `-Wl,-soname,x` a `,--plugin,/tmp/p.so` or a `,-rpath,/home/u` would be an option of its
/// own that no rule here read.
fn one_value(value: &str) -> bool {
    !value.is_empty() && !value.contains(',') && !value.split('/').any(|it| it == "..")
}

fn allowed_args() -> String {
    let mut all: Vec<String> = LINK_ARGS.iter().map(|it| format!("`{it}`")).collect();
    all.extend(LINK_ARG_PREFIXES.iter().map(|it| format!("`{it}...`")));
    all.push("`-lNAME`".to_owned());
    all.push("an rpath to `$ORIGIN` or to a directory `rustc-link-search` may name".to_owned());
    all.join(", ")
}

/// The variables, sorted, that cargo gives a crate's compilation or that rustc, rustdoc or cargo
/// read at run time when building a crate, from their source at the pinned nightly
/// (rust-lang/rust 16d1ffac17ad, its cargo at cad07cf8e597). Left out: what only bootstrap
/// (`src/bootstrap`), tests or tidy read, what the compiler's own build bakes in with
/// `option_env!` or `env!` (`RUSTC_VERIFY_LLVM_IR`, `RUSTC_INSTALL_BINDIR`), what only rustc's
/// build scripts read (`RUSTC_WINDOWS_RC`), and what cargo gives only build scripts
/// (`RUSTC_LINKER`).
pub const GIVEN: [&str; 47] = [
    // cargo `src/lib.rs` (`CARGO_ENV`), `src/compiler/compilation.rs`.
    "CARGO",
    "CARGO_BIN_NAME",
    "CARGO_CRATE_NAME",
    // The jobserver crate's `Client::configure`, which cargo calls on each rustc.
    "CARGO_MAKEFLAGS",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    // cargo `src/compiler/mod.rs`.
    "CARGO_PRIMARY_PACKAGE",
    // Unstable in cargo's "Environment variables Cargo sets for crates"; not set at this commit.
    "CARGO_RUSTC_CURRENT_DIR",
    // cargo `src/compiler/mod.rs`.
    "CARGO_SBOM_PATH",
    "CARGO_TARGET_TMPDIR",
    // cargo `src/compiler/mod.rs`.
    "OUT_DIR",
    "RUSTC",
    // `compiler/rustc_feature/src/lib.rs`, cargo `src/compiler/custom_build.rs`.
    "RUSTC_BOOTSTRAP",
    // `compiler/rustc_driver_impl/src/lib.rs`.
    "RUSTC_BREAK_ON_ICE",
    // `compiler/rustc_session/src/session.rs`.
    "RUSTC_CTFE_BACKTRACE",
    // `compiler/rustc_span/src/def_id.rs`, `compiler/rustc_incremental/src/persist/file_format.rs`.
    "RUSTC_FORCE_RUSTC_VERSION",
    // `compiler/rustc_session/src/config.rs`.
    "RUSTC_GRAPHVIZ_FONT",
    // `compiler/rustc_driver_impl/src/lib.rs`.
    "RUSTC_ICE",
    // `compiler/rustc_driver_impl/src/lib.rs` into `compiler/rustc_log/src/lib.rs`
    // (`LoggerConfig::from_env`), `compiler/rustc_session/src/session.rs`.
    "RUSTC_LOG",
    "RUSTC_LOG_BACKTRACE",
    "RUSTC_LOG_COLOR",
    "RUSTC_LOG_ENTRY_EXIT",
    "RUSTC_LOG_FORMAT_JSON",
    "RUSTC_LOG_LINES",
    "RUSTC_LOG_OUTPUT_TARGET",
    "RUSTC_LOG_THREAD_IDS",
    "RUSTC_LOG_WRAPTREE",
    // `compiler/rustc_driver_impl/src/lib.rs`, `compiler/rustc_ast/src/attr/version.rs`.
    "RUSTC_OVERRIDE_VERSION_STRING",
    // cargo `src/context/mod.rs` (`maybe_get_tool`).
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTC_WRAPPER",
    "RUSTDOC",
    // cargo `src/compiler/build_context/target_info.rs`.
    "RUSTDOCFLAGS",
    // `src/librustdoc/doctest.rs`, `src/librustdoc/doctest/runner.rs`.
    "RUSTDOC_DOCTEST_BIN_PATH",
    "RUSTDOC_DOCTEST_RUN_NB_TEST",
    // cargo `src/ops/cargo_compile/mod.rs`.
    "RUSTDOC_FLAGS",
    // `src/librustdoc/lib.rs` into `compiler/rustc_log/src/lib.rs` (`LoggerConfig::from_env`).
    "RUSTDOC_LOG",
    "RUSTDOC_LOG_BACKTRACE",
    "RUSTDOC_LOG_COLOR",
    "RUSTDOC_LOG_ENTRY_EXIT",
    "RUSTDOC_LOG_FORMAT_JSON",
    "RUSTDOC_LOG_LINES",
    "RUSTDOC_LOG_OUTPUT_TARGET",
    "RUSTDOC_LOG_THREAD_IDS",
    "RUSTDOC_LOG_WRAPTREE",
    "RUSTFLAGS",
    // `compiler/rustc_driver_impl/src/lib.rs`, `compiler/rustc_lint/src/non_local_def.rs`.
    "UNSTABLE_RUSTDOC_TEST_LINE",
    "UNSTABLE_RUSTDOC_TEST_PATH",
];

/// Whether `name` is one a script's `rustc-env` may not set: one of [`GIVEN`], or what cargo
/// gives a crate under `CARGO_PKG_` and `CARGO_BIN_EXE_`. What cargo gives only build scripts
/// (`CARGO_CFG_*`, `CARGO_FEATURE_*`, `CARGO_ENCODED_RUSTFLAGS`, `CARGO_MANIFEST_LINKS`) is not
/// one: a crate has it only when its script forwards it, as rav1e's does. Nor is
/// `CARGO_COMMIT_HASH`, which cargo's build sets for cargo, or a `RUSTC_*` no tool reads, such
/// as rust-analyzer's `RUSTC_VERSION`.
pub fn given_by_cargo(name: &str) -> bool {
    const PREFIXES: [&str; 2] = ["CARGO_PKG_", "CARGO_BIN_EXE_"];
    GIVEN.binary_search(&name).is_ok() || PREFIXES.iter().any(|it| name.starts_with(it))
}

/// The directory of a search path, without its kind (`native=`, `framework=`, ...).
fn search_dir(value: &str) -> &str {
    match value.split_once('=') {
        Some(("native" | "crate" | "dependency" | "framework" | "all", dir)) => dir,
        _ => value,
    }
}

/// One line a sandboxed script printed that a rule refuses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Refused {
    /// The rule's id ([`blaze_report::rules`]): `sandbox:link-dir`, `sandbox:link-arg` or
    /// `sandbox:rustc-env`.
    pub rule: &'static str,
    /// The line, whole.
    pub line: String,
    /// Why, with code in backticks.
    pub why: String,
}

/// Each line of `stdout` a run `ran` printed that its rules refuse, with why.
pub fn refused(stdout: &[u8], ran: &Ran<'_>) -> Vec<Refused> {
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let line = line.trim();
        let data = match (line.strip_prefix("cargo::"), line.strip_prefix("cargo:")) {
            (Some(data), _) => data,
            (None, Some(data)) if blaze_build_script::output::has_reserved_prefix(data) => data,
            _ => continue,
        };
        let Some((key, value)) = data.split_once('=') else { continue };
        let value = value.trim_end();
        let (rule, why) = match key {
            "rustc-link-search" => (
                "sandbox:link-dir",
                (!ran.may_search(search_dir(value)))
                    .then(|| format!("a library directory that is none of {}", ran.places())),
            ),
            "rustc-flags" => {
                let mut flags = value.split_whitespace();
                let mut why = None;
                while let Some(flag) = flags.next() {
                    let Some(rest) = flag.strip_prefix("-L") else { continue };
                    let dir = match rest.is_empty() {
                        true => flags.next().unwrap_or_default(),
                        false => rest,
                    };
                    if !ran.may_search(search_dir(dir)) {
                        why = Some(format!("`-L {dir}`, which is none of {}", ran.places()));
                    }
                }
                ("sandbox:link-dir", why)
            }
            "rustc-link-arg-bin" => (
                "sandbox:link-arg",
                value.split_once('=').and_then(|(_, arg)| ran.link_arg_refused(arg)),
            ),
            "rustc-link-arg"
            | "rustc-link-arg-bins"
            | "rustc-link-arg-tests"
            | "rustc-link-arg-benches"
            | "rustc-link-arg-examples"
            | "rustc-link-arg-cdylib"
            | "rustc-cdylib-link-arg" => ("sandbox:link-arg", ran.link_arg_refused(value)),
            "rustc-env" => (
                "sandbox:rustc-env",
                value.split_once('=').filter(|(name, _)| given_by_cargo(name)).map(|(name, _)| {
                    format!("`{name}`, which cargo gives the crate or the toolchain acts on itself")
                }),
            ),
            _ => continue,
        };
        if let Some(why) = why {
            out.push(Refused { rule, line: line.to_owned(), why }); // alloc: a refusal's, once
        }
    }
    out
}

/// The report of a build a script of `package` broke the rules of, `at` naming the script:
/// each line refused and why, under the first's rule, and the ways out.
pub fn refused_report(
    package: &str,
    at: blaze_report::Text,
    refused: &[Refused],
) -> blaze_report::Report {
    use blaze_report::{Fix, Report, Text};
    let what = match refused.len() {
        1 => "a directive",
        _ => "directives",
    };
    let headline =
        Text::new().words("a build script printed ").words(what).words(" its sandbox refuses");
    let mut report = Report::error(headline).at(at);
    if let Some(first) = refused.first() {
        report = report.rule(first.rule);
    }
    for it in refused {
        let mut text = Text::new().code(&it.line).words(": ");
        text.0.extend(Text::marked(&it.why).0);
        report = report.fact("refused", text);
    }
    if refused.iter().any(|it| it.rule == "sandbox:link-dir") {
        report = report.fix(Fix::note(
            Text::new()
                .words("a directory of the system it links from goes in ")
                .code("build-script-link-dirs = [...]")
                .words(" under ")
                .code("[workspace.metadata.blaze.sandbox]")
                .words(" in the workspace's root manifest"),
        ));
    }
    report.fix(Fix::note(
        Text::new()
            .words("a script that needs more runs outside the sandbox: ")
            .code(&format!("unsandboxed-build-scripts = [\"{package}\"]")) // alloc: a refusal's, once
            .words(" there"),
    ))
}

/// The workspace's `build-script-link-dirs` as a script started in `env` may name them: an
/// entry `${NAME}/rest` is the variable `NAME`'s value then `/rest` (a machine's install of
/// what the script links, which the manifest can't spell: `${LLVM_SYS_231_PREFIX}/lib`), any
/// other entry as it is. An error names a variable the script was not given, or one that is
/// no absolute path.
pub fn link_dirs(
    dirs: &[String],
    env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<Vec<PathBuf>, String> {
    dirs.iter()
        .map(|dir| {
            let Some((name, rest)) = dir.strip_prefix("${").and_then(|it| it.split_once('}')) else {
                return Ok(PathBuf::from(dir));
            };
            let Some((_, value)) = env.iter().find(|(it, _)| it == name) else {
                return Err(format!(
                    "`{dir}` of `build-script-link-dirs` names `{name}`, which the build script's environment does not set"
                ));
            };
            let value = Path::new(value);
            if !value.is_absolute() {
                return Err(format!(
                    "`{dir}` of `build-script-link-dirs`: `{name}` is `{}`, which is no absolute path",
                    value.display()
                ));
            }
            Ok(blaze_manifest::paths::normalize_path(&value.join(rest.trim_start_matches('/'))))
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ran() -> Ran<'static> {
        Ran {
            package_dir: Path::new("/src/pkg"),
            out_dir: Path::new("/t/debug/build/pkg/0123/out"),
            link_dirs: Vec::<PathBuf>::leak(vec!["/opt/vendor/lib".into()]), // alloc: a test's, leaked
            native: Some(Path::new("/home/u/.blaze/native")),
        }
    }

    fn refused_lines(stdout: &str) -> Vec<String> {
        refused(stdout.as_bytes(), &ran()).into_iter().map(|it| it.line).collect()
    }

    #[test]
    fn link_search_stays_in_the_build_the_package_and_the_system() {
        let stdout = "cargo:rustc-link-search=native=/t/debug/build/pkg/0123/out/lib\n\
                      cargo:rustc-link-search=/t/debug/build/zlib/4567/out/lib\n\
                      cargo::rustc-link-search=native=/usr/lib/x86_64-linux-gnu\n\
                      cargo:rustc-link-search=/nix/store/abc-openssl-3/lib\n\
                      cargo:rustc-link-search=/opt/vendor/lib/x\n\
                      cargo:rustc-link-search=native=vendored\n\
                      cargo:rustc-link-search=native=/home/u/.ssh\n\
                      cargo:rustc-link-search=/usr/lib/../../home/u\n\
                      cargo:rustc-link-search=../outside\n\
                      cargo:rustc-flags=-L /etc -lfoo\n";
        assert_eq!(
            refused_lines(stdout),
            [
                "cargo:rustc-link-search=native=/home/u/.ssh",
                "cargo:rustc-link-search=/usr/lib/../../home/u",
                "cargo:rustc-link-search=../outside",
                "cargo:rustc-flags=-L /etc -lfoo",
            ]
        );
    }

    #[test]
    fn link_args_are_the_list_and_rpaths_where_search_may_go() {
        let stdout = "cargo:rustc-link-arg=-pthread\n\
                      cargo:rustc-link-arg=-ObjC\n\
                      cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib\n\
                      cargo:rustc-link-arg=-Wl,-rpath,/nix/store/abc-libva/lib\n\
                      cargo:rustc-cdylib-link-arg=-Wl,-soname,libx.so.1\n\
                      cargo:rustc-link-arg=-lstdc++\n\
                      cargo:rustc-link-arg=-ObjC\n\
                      cargo:rustc-link-arg-bins=-Wl,-ObjC\n\
                      cargo:rustc-link-arg=-fuse-ld=/tmp/evil\n\
                      cargo:rustc-link-arg=-Wl,-rpath,/home/u/lib\n\
                      cargo:rustc-link-arg=-Wl,--version-script=../../etc/x\n\
                      cargo::rustc-link-arg-bin=tool=-Wl,-plugin,/tmp/p.so\n";
        assert_eq!(
            refused_lines(stdout),
            [
                "cargo:rustc-link-arg=-fuse-ld=/tmp/evil",
                "cargo:rustc-link-arg=-Wl,-rpath,/home/u/lib",
                "cargo:rustc-link-arg=-Wl,--version-script=../../etc/x",
                "cargo::rustc-link-arg-bin=tool=-Wl,-plugin,/tmp/p.so",
            ]
        );
    }

    /// evm2-jit's `-Wl,--export-dynamic-symbol,__revmc_builtin_*` is a pattern of symbols to
    /// export, in either spelling; a value that goes on into another linker word is refused
    /// whatever prefix it follows.
    #[test]
    fn a_prefixed_link_arg_has_one_value_and_no_further_linker_word() {
        let stdout = "cargo:rustc-link-arg=-Wl,--export-dynamic-symbol,__revmc_builtin_*\n\
                      cargo:rustc-link-arg=-Wl,--export-dynamic-symbol=__revmc_builtin_*\n\
                      cargo:rustc-cdylib-link-arg=-Wl,-soname,libx.so.1\n\
                      cargo:rustc-link-arg=-Wl,--version-script=exports.map\n\
                      cargo:rustc-link-arg=-Wl,--export-dynamic-symbol,x,--plugin,/tmp/p.so\n\
                      cargo:rustc-link-arg=-Wl,--export-dynamic-symbol=x,-rpath,/home/u\n\
                      cargo:rustc-cdylib-link-arg=-Wl,-soname,libx.so,-rpath,/home/u/lib\n\
                      cargo:rustc-link-arg=-Wl,--export-dynamic-symbol,\n";
        assert_eq!(
            refused_lines(stdout),
            [
                "cargo:rustc-link-arg=-Wl,--export-dynamic-symbol,x,--plugin,/tmp/p.so",
                "cargo:rustc-link-arg=-Wl,--export-dynamic-symbol=x,-rpath,/home/u",
                "cargo:rustc-cdylib-link-arg=-Wl,-soname,libx.so,-rpath,/home/u/lib",
                "cargo:rustc-link-arg=-Wl,--export-dynamic-symbol,",
            ]
        );
    }

    /// A layout of blaze's own store of pinned native inputs may be searched and be an rpath
    /// (llvm-sys, pointed at the LLVM blaze fetched, prints its `lib`); the store itself, a
    /// name beside the layouts and another home's store may not. With no store named, none is.
    #[test]
    fn a_layout_of_the_native_store_may_be_searched_and_nothing_beside_it() {
        let digest = "918ef5bdb006b29b96d8f87dbbbac422593c6abf916cb7ad9afed17672648087";
        let stdout = format!(
            "cargo:rustc-link-search=native=/home/u/.blaze/native/sha256-{digest}/lib\n\
             cargo:rustc-link-arg=-Wl,-rpath,/home/u/.blaze/native/sha256-{digest}/lib\n\
             cargo:rustc-link-search=native=/home/u/.blaze/native\n\
             cargo:rustc-link-search=native=/home/u/.blaze/native/sha256-{digest}.new/lib\n\
             cargo:rustc-link-search=native=/home/u/.blaze/native/sha256-{digest}/../../../.ssh\n\
             cargo:rustc-link-search=native=/home/other/.blaze/native/sha256-{digest}/lib\n"
        );
        assert_eq!(
            refused_lines(&stdout),
            [
                "cargo:rustc-link-search=native=/home/u/.blaze/native".to_owned(),
                format!("cargo:rustc-link-search=native=/home/u/.blaze/native/sha256-{digest}.new/lib"),
                format!("cargo:rustc-link-search=native=/home/u/.blaze/native/sha256-{digest}/../../../.ssh"),
                format!("cargo:rustc-link-search=native=/home/other/.blaze/native/sha256-{digest}/lib"),
            ]
        );
        let none = Ran { native: None, ..ran() };
        assert_eq!(refused(stdout.as_bytes(), &none).len(), 6, "with no store, each is refused");
    }

    #[test]
    fn rustc_env_may_not_set_what_cargo_gives() {
        let stdout = "cargo:rustc-env=CARGO_COMMIT_HASH=ea2d978\n\
                      cargo:rustc-env=GIT_HASH=abc\n\
                      cargo:rustc-env=CARGO_PKG_VERSION=9.9.9\n\
                      cargo::rustc-env=RUSTC_BOOTSTRAP=1\n\
                      cargo:rustc-env=CARGO_MAKEFLAGS=-j1\n\
                      cargo:rustc-env-like=CARGO_PKG_NAME\n\
                      cargo:CARGO_PKG_NAME=metadata, not a directive\n";
        assert_eq!(
            refused_lines(stdout),
            [
                "cargo:rustc-env=CARGO_PKG_VERSION=9.9.9",
                "cargo::rustc-env=RUSTC_BOOTSTRAP=1",
                "cargo:rustc-env=CARGO_MAKEFLAGS=-j1",
            ]
        );
    }

    #[test]
    fn rustc_env_may_forward_what_cargo_gives_only_scripts() {
        let stdout = "cargo:rustc-env=CARGO_CFG_TARGET_FEATURE=aes,crc,neon\n\
                      cargo:rustc-env=CARGO_ENCODED_RUSTFLAGS=\n\
                      cargo::rustc-env=CARGO_FEATURE_ASM=1\n\
                      cargo:rustc-env=CARGO_MANIFEST_LINKS=z\n";
        assert_eq!(refused_lines(stdout), Vec::<String>::new());
    }

    #[test]
    fn given_is_sorted_for_its_binary_search() {
        assert!(GIVEN.windows(2).all(|it| it[0] < it[1]), "{GIVEN:?}");
    }

    #[test]
    fn rustc_env_may_not_set_what_the_toolchain_reads() {
        for name in ["RUSTC_BOOTSTRAP", "RUSTC_LOG", "RUSTDOC_DOCTEST_RUN_NB_TEST"] {
            assert!(given_by_cargo(name), "{name}");
        }
        for name in GIVEN {
            assert!(given_by_cargo(name), "{name}");
        }
    }

    #[test]
    fn rustc_env_may_set_rustc_names_no_tool_reads() {
        for name in ["RUSTC_VERSION", "RUSTC_SEMVER", "RUSTDOC_FOO_BAR", "RUSTC_X"] {
            assert!(!given_by_cargo(name), "{name}");
        }
        assert!(refused(b"cargo::rustc-env=RUSTC_VERSION=1\n", &ran()).is_empty());
        assert_eq!(
            refused_lines("cargo::rustc-env=RUSTC_BOOTSTRAP=1\n"),
            ["cargo::rustc-env=RUSTC_BOOTSTRAP=1"]
        );
    }

    #[test]
    fn the_report_names_the_script_each_line_and_the_rule() {
        let refused =
            refused(b"cargo:rustc-link-search=/root/x\ncargo:rustc-env=RUSTFLAGS=-Cx\n", &ran());
        assert_eq!(
            refused.iter().map(|it| it.rule).collect::<Vec<_>>(),
            ["sandbox:link-dir", "sandbox:rustc-env"]
        );
        let at =
            blaze_report::Text::new().words("build script of ").package("evil-sys", Some("0.1.0"));
        let report = refused_report("evil-sys", at, &refused);
        assert_eq!(report.rule.as_deref(), Some("sandbox:link-dir"));
        assert_eq!(report.subject(), Some("evil-sys"));
        let flat = report.flat();
        assert!(
            flat.contains("`cargo:rustc-link-search=/root/x`") && flat.contains("`RUSTFLAGS`"),
            "{flat}"
        );
        assert!(
            flat.contains("build-script-link-dirs")
                && flat.contains("unsandboxed-build-scripts = [\"evil-sys\"]"),
            "{flat}"
        );
        assert!(
            blaze_report::rules::find("sandbox:link-dir").is_some()
                && blaze_report::rules::find("sandbox:rustc-env").is_some()
        );
    }

    /// A link dir under a variable is where the script's environment puts it, and the search it
    /// prints there is allowed; an unset or relative variable is an error naming it, never a
    /// dir left out.
    #[test]
    fn a_link_dir_under_a_variable_is_where_the_scripts_environment_puts_it() {
        use std::ffi::OsString;
        let env = |value: &str| -> Vec<(OsString, OsString)> {
            vec![("LLVM_SYS_231_PREFIX".into(), value.into())]
        };
        let dirs = ["${LLVM_SYS_231_PREFIX}/lib".to_owned(), "/opt/vendor/lib".to_owned()];
        let found = link_dirs(&dirs, &env("/m/llvm-23.1.4")).unwrap();
        assert_eq!(found, [PathBuf::from("/m/llvm-23.1.4/lib"), PathBuf::from("/opt/vendor/lib")]);
        let ran = Ran { link_dirs: &found, ..ran() };
        assert!(refused(b"cargo:rustc-link-search=native=/m/llvm-23.1.4/lib\n", &ran).is_empty());
        assert!(!refused(b"cargo:rustc-link-search=native=/m/other/lib\n", &ran).is_empty());

        let unset = link_dirs(&dirs, &[]).unwrap_err();
        assert!(
            unset.contains("`LLVM_SYS_231_PREFIX`") && unset.contains("does not set"),
            "{unset}"
        );
        let relative = link_dirs(&dirs, &env("llvm")).unwrap_err();
        assert!(relative.contains("no absolute path"), "{relative}");
    }
}
