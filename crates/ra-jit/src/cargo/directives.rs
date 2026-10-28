//! What a build script in the sandbox may tell cargo. A script is code from anywhere, and what
//! it prints goes into every rustc and linker run of its package and of what links it: a
//! `rustc-link-search` into the home directory, a `rustc-link-arg` that loads a plugin into
//! the linker, or a `rustc-env` that overrides `CARGO_PKG_VERSION` reaches past the sandbox it
//! ran in. So the directives of a sandboxed run are checked before cargo reads them
//! ([`refused`]), and one that breaks a rule fails the build, naming the script and the line:
//!
//! - `rustc-link-search` and `rustc-flags`' `-L`: a directory inside the script's `OUT_DIR`,
//!   its package, the target directory's build directory (the `OUT_DIR`s of its dependencies,
//!   which `DEP_*` names), or a system directory ([`SYSTEM`], and what the workspace adds in
//!   `build-script-link-dirs`). Homebrew's versioned `Cellar` library paths are allowed when
//!   they resolve to the same directory through the installed formula's allowed `opt` link.
//! - `rustc-link-arg` (and its `-bins`, `-bin=`, `-tests`, `-benches`, `-examples` and
//!   `-cdylib` forms): one of [`LINK_ARGS`], or an rpath to a directory `rustc-link-search`
//!   may name.
//! - `rustc-env`: not a variable cargo or rustc gives a crate itself ([`given_by_cargo`]).
//!
//! What counting these rules over the bench corpus found (590 scripts,
//! `bench/results/2026-10-27-bscache/directives.md`) decided their edges: no
//! `rustc-link-search` outside those directories, `-pthread` and an rpath into the Nix store
//! the only link arguments outside the list, and `CARGO_COMMIT_HASH` (cargo's own build) the
//! only `CARGO_*` a script set, which cargo never sets for a crate.
//!
//! A run outside the sandbox, which its workspace took out by name, is not checked: it can do
//! anything anyway.

use std::path::{Path, PathBuf};

/// The keys of the old syntax (`cargo:KEY=VALUE`) that are directives: any other is metadata.
const RESERVED: [&str; 17] = [
    "rustc-flags",
    "rustc-link-lib",
    "rustc-link-search",
    "rustc-link-arg-cdylib",
    "rustc-cdylib-link-arg",
    "rustc-link-arg-bins",
    "rustc-link-arg-bin",
    "rustc-link-arg-tests",
    "rustc-link-arg-benches",
    "rustc-link-arg-examples",
    "rustc-link-arg",
    "rustc-cfg",
    "rustc-check-cfg",
    "rustc-env",
    "warning",
    "rerun-if-changed",
    "rerun-if-env-changed",
];

/// The system's directories of libraries, which the linker searches anyway: naming one adds
/// nothing a script could not link already. The Nix store is the closure a script's tools
/// come from; Homebrew's prefixes and Apple's SDKs are where a Mac's libraries are.
pub(crate) const SYSTEM: [&str; 11] = [
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
pub(crate) const LINK_ARGS: [&str; 14] = [
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
];

/// The link arguments a script may give with a value of its own after the prefix.
const LINK_ARG_PREFIXES: [&str; 5] = [
    "-Wl,--export-dynamic-symbol=",
    "-Wl,-soname,",
    "-Wl,-install_name,@rpath/",
    "-Wl,-exported_symbol,",
    "-Wl,--version-script=",
];

/// Where a script ran, which decides what its directives may name.
pub(crate) struct Ran<'a> {
    pub(crate) package_dir: &'a Path,
    pub(crate) out_dir: &'a Path,
    /// The system directories the workspace allows besides [`SYSTEM`].
    pub(crate) link_dirs: &'a [PathBuf],
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
        let dir = tidy(dir);
        [self.out_dir, self.package_dir]
            .into_iter()
            .chain(self.build_dir())
            .chain(SYSTEM.iter().map(Path::new))
            .chain(self.link_dirs.iter().map(PathBuf::as_path))
            .any(|it| dir.starts_with(it))
            || ["/opt/homebrew", "/usr/local"]
                .iter()
                .any(|prefix| homebrew_lib_alias(&dir, Path::new(prefix)))
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
            || LINK_ARG_PREFIXES.iter().any(|it| arg.strip_prefix(it).is_some_and(no_path_out))
            || arg.strip_prefix("-l").is_some_and(|name| {
                !name.is_empty()
                    && name.chars().all(|it| it.is_ascii_alphanumeric() || "_+.-".contains(it))
            });
        (!known).then(|| {
            format!("a link argument that is not one a build script may give ({})", allowed_args())
        })
    }

    fn places(&self) -> String {
        "the script's OUT_DIR, its package, the build directory, a system directory or one \
         `build-script-link-dirs` names"
            .to_owned()
    }
}

/// Whether a versioned Homebrew library directory is the same directory reached through
/// its already allowed `opt/<formula>/lib` path. pkg-config may print the Cellar spelling.
/// Only existing library directories of the linked formula qualify, and a nested symlink
/// must not lead outside that library directory.
fn homebrew_lib_alias(dir: &Path, prefix: &Path) -> bool {
    let Ok(relative) = dir.strip_prefix(prefix.join("Cellar")) else { return false };
    let mut parts = relative.components();
    let Some(std::path::Component::Normal(formula)) = parts.next() else { return false };
    let Some(std::path::Component::Normal(_version)) = parts.next() else { return false };
    if parts.next() != Some(std::path::Component::Normal(std::ffi::OsStr::new("lib"))) {
        return false;
    }
    let opt_lib = prefix.join("opt").join(formula).join("lib");
    let Ok(library) = opt_lib.canonicalize() else { return false };
    let Ok(directory) = dir.canonicalize() else { return false };
    directory.is_dir()
        && directory.starts_with(library)
        && opt_lib.join(parts.as_path()).canonicalize().is_ok_and(|alias| alias == directory)
}

/// Whether a value names no path above where it is.
fn no_path_out(value: &str) -> bool {
    !value.split('/').any(|it| it == "..")
}

fn allowed_args() -> String {
    let mut all: Vec<String> = LINK_ARGS.iter().map(|it| format!("`{it}`")).collect();
    all.extend(LINK_ARG_PREFIXES.iter().map(|it| format!("`{it}...`")));
    all.push("`-lNAME`".to_owned());
    all.push("an rpath to `$ORIGIN` or to a directory `rustc-link-search` may name".to_owned());
    all.join(", ")
}

/// Whether cargo or rustc gives a crate the variable `name` itself, which a script's
/// `rustc-env` may not set over: what cargo sets for a crate's compilation (cargo's
/// "Environment variables Cargo sets for crates") and what the toolchain reads from its own
/// environment (`RUSTC_BOOTSTRAP`, the jobserver's `CARGO_MAKEFLAGS`). What cargo gives only
/// build scripts (`CARGO_CFG_*`, `CARGO_FEATURE_*`, `CARGO_ENCODED_RUSTFLAGS`,
/// `CARGO_MANIFEST_LINKS`) is not one: a crate has it only when its script forwards it, as
/// rav1e's does. Nor is `CARGO_COMMIT_HASH`, which cargo's build sets for cargo.
pub(crate) fn given_by_cargo(name: &str) -> bool {
    const NAMES: [&str; 13] = [
        "CARGO",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_CRATE_NAME",
        "CARGO_BIN_NAME",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_TARGET_TMPDIR",
        "CARGO_RUSTC_CURRENT_DIR",
        "CARGO_MAKEFLAGS",
        "OUT_DIR",
        "RUSTC",
        "RUSTDOC",
        "RUSTFLAGS",
    ];
    const PREFIXES: [&str; 4] = ["CARGO_PKG_", "CARGO_BIN_EXE_", "RUSTC_", "RUSTDOC_"];
    NAMES.contains(&name) || PREFIXES.iter().any(|it| name.starts_with(it))
}

/// The directory of a search path, without its kind (`native=`, `framework=`, ...).
fn search_dir(value: &str) -> &str {
    match value.split_once('=') {
        Some(("native" | "crate" | "dependency" | "framework" | "all", dir)) => dir,
        _ => value,
    }
}

/// Each line of `stdout` a run `ran` printed that its rules refuse, with why.
pub(crate) fn refused(stdout: &[u8], ran: &Ran<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let line = line.trim();
        let data = match (line.strip_prefix("cargo::"), line.strip_prefix("cargo:")) {
            (Some(data), _) => data,
            (None, Some(data)) if RESERVED.iter().any(|it| data.starts_with(&format!("{it}="))) => {
                data
            }
            _ => continue,
        };
        let Some((key, value)) = data.split_once('=') else { continue };
        let value = value.trim_end();
        let why = match key {
            "rustc-link-search" => (!ran.may_search(search_dir(value)))
                .then(|| format!("a library directory that is none of {}", ran.places())),
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
                why
            }
            "rustc-link-arg-bin" => {
                value.split_once('=').and_then(|(_, arg)| ran.link_arg_refused(arg))
            }
            "rustc-link-arg"
            | "rustc-link-arg-bins"
            | "rustc-link-arg-tests"
            | "rustc-link-arg-benches"
            | "rustc-link-arg-examples"
            | "rustc-link-arg-cdylib"
            | "rustc-cdylib-link-arg" => ran.link_arg_refused(value),
            "rustc-env" => value
                .split_once('=')
                .filter(|(name, _)| given_by_cargo(name))
                .map(|(name, _)| format!("`{name}`, which cargo gives the crate itself")),
            _ => None,
        };
        if let Some(why) = why {
            out.push((line.to_owned(), why));
        }
    }
    out
}

/// What a build that a script of `package` broke the rules of is told.
pub(crate) fn refused_error(package: &str, refused: &[(String, String)]) -> String {
    let lines: Vec<String> =
        refused.iter().map(|(line, why)| format!("  `{line}`: {why}")).collect();
    format!(
        "the build script of `{package}` printed {} a build script in the sandbox may not:\n{}\n\
         a directory of the system it links from goes in `build-script-link-dirs = [...]` \
         under `{}` in the workspace's root manifest; a script that needs more runs outside, \
         by name in `unsandboxed-build-scripts` there",
        match refused.len() {
            1 => "a directive",
            _ => "directives",
        },
        lines.join("\n"),
        super::sandboxed::WORKSPACE_TABLE,
    )
}

/// `path` with `.` left out and `..` taken back.
fn tidy(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => _ = out.pop(),
            other => out.push(other),
        }
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ran() -> Ran<'static> {
        static DIRS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
        let dirs = DIRS.get_or_init(|| vec!["/opt/vendor/lib".into()]);
        Ran {
            package_dir: Path::new("/src/pkg"),
            out_dir: Path::new("/t/debug/build/pkg/0123/out"),
            link_dirs: dirs,
        }
    }

    fn refused_lines(stdout: &str) -> Vec<String> {
        refused(stdout.as_bytes(), &ran()).into_iter().map(|it| it.0).collect()
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
                      cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib\n\
                      cargo:rustc-link-arg=-Wl,-rpath,/nix/store/abc-libva/lib\n\
                      cargo:rustc-cdylib-link-arg=-Wl,-soname,libx.so.1\n\
                      cargo:rustc-link-arg=-lstdc++\n\
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

    #[test]
    #[cfg(unix)]
    fn homebrew_cellar_search_requires_the_same_opt_library_directory() {
        use std::{fs, os::unix::fs::symlink, time::SystemTime};

        let nonce = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        // Use the logical temporary directory: the macOS sandbox's physical TMPDIR has
        // remapped ancestors that realpath cannot currently traverse.
        let root = Path::new("/tmp")
            .join(format!("ra-jit-directives-homebrew-{}-{nonce}", std::process::id()));
        let prefix = root.join("homebrew");
        let lib = prefix.join("Cellar/x264/r3222/lib");
        let old = prefix.join("Cellar/x264/r3000/lib");
        let unlinked = prefix.join("Cellar/unlinked/1.0/lib");
        let bin = prefix.join("Cellar/x264/r3222/bin");
        let outside = root.join("outside");
        for dir in [&lib.join("private"), &old, &unlinked, &bin, &outside, &prefix.join("opt")] {
            fs::create_dir_all(dir).unwrap();
        }
        symlink("../Cellar/x264/r3222", prefix.join("opt/x264")).unwrap();
        symlink(&outside, lib.join("escaped")).unwrap();
        fs::write(lib.join("not-a-directory"), b"").unwrap();

        assert!(homebrew_lib_alias(&lib, &prefix));
        assert!(homebrew_lib_alias(&lib.join("private"), &prefix));
        for dir in [
            old,
            unlinked,
            bin,
            prefix.join("Cellar/x264/r3222"),
            prefix.join("Cellarish/x264/r3222/lib"),
            prefix.join("opt/x264/lib"),
            lib.join("missing"),
            lib.join("not-a-directory"),
            lib.join("escaped"),
            outside,
        ] {
            assert!(!homebrew_lib_alias(&dir, &prefix), "{}", dir.display());
        }

        fs::remove_file(prefix.join("opt/x264")).unwrap();
        symlink("../Cellar/x264/not-installed", prefix.join("opt/x264")).unwrap();
        assert!(!homebrew_lib_alias(&lib, &prefix));
        fs::remove_dir_all(root).unwrap();
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
    fn the_error_names_the_package_and_each_line() {
        let refused = refused(b"cargo:rustc-link-search=/root/x\n", &ran());
        let error = refused_error("evil-sys", &refused);
        assert!(
            error.contains("`evil-sys`") && error.contains("`cargo:rustc-link-search=/root/x`")
        );
        assert!(error.contains("build-script-link-dirs"), "{error}");
    }
}
