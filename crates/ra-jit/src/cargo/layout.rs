//! Where cargo puts what it builds of a profile, in either of its two layouts of the build
//! directory (`core/compiler/layout.rs` and `build_runner/compilation_files.rs` of
//! rust-lang/cargo at cad07cf8e, the cargo of nightly-2026-10-25).
//!
//! The first, stable cargo's and nightly's with `CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT=false`,
//! has rustc write every unit to `<profile>/deps` (`lib<name>-<hash>.rlib`, `<test>-<hash>`),
//! examples to `<profile>/examples`, a build script to `<profile>/build/<package>-<hash>` and
//! its run beside its `OUT_DIR` in `<profile>/build/<package>-<hash>/out`, and keeps the
//! fingerprints in `<profile>/.fingerprint/<package>-<hash>`.
//!
//! The second, nightly cargo's default since 2026 (`-Zbuild-dir-new-layout`), gives each unit
//! a directory `<profile>/build/<package>/<hash>`: rustc writes to its `out` (a build script's
//! run has its `OUT_DIR` there too), its fingerprint is in `fingerprint` and the files of a
//! script's run (`stdout`, `stderr`, `root-output`) are in `run`. A unit's directory names it,
//! so a program, an example, a build script and a `dylib` get no `-C extra-filename` there:
//! `out/<name>`, `out/build_script_build`, `out/lib<name>.so`. The hash is cargo's unit hash,
//! the same in both layouts.

use std::path::Path;

/// The directories of a profile that cargo has rustc write to in the first layout.
const FIRST: [&str; 4] = ["deps", "examples", "build", ".fingerprint"];

/// The directories of a unit's directory in the second layout.
const OF_UNIT: [&str; 3] = ["out", "fingerprint", "run"];

/// A unit's directory in the second layout: `<profile>/build/<package>/<hash>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UnitDir<'a> {
    pub(crate) dir: &'a Path,
    pub(crate) package: &'a str,
    pub(crate) hash: &'a str,
}

impl<'a> UnitDir<'a> {
    /// The profile's directory, `target/debug`, the unit is built for.
    pub(crate) fn profile(&self) -> Option<&'a Path> {
        self.dir.parent()?.parent()?.parent()
    }

    /// The name cargo gives the unit in the first layout, and which ra-jit keeps it by:
    /// `<package>-<hash>`.
    pub(crate) fn name(&self) -> String {
        format!("{}-{}", self.package, self.hash)
    }
}

/// Whether `text` is a hash cargo names a unit by: sixteen hexadecimal digits.
pub(crate) fn is_unit_hash(text: &str) -> bool {
    text.len() == 16 && text.bytes().all(|it| it.is_ascii_hexdigit())
}

/// The unit directory of the second layout that `path` is, or is below one of the
/// directories of (`out`, `fingerprint`, `run`).
pub(crate) fn unit_dir(path: &Path) -> Option<UnitDir<'_>> {
    let mut below: Option<&Path> = None;
    for dir in path.ancestors() {
        let is_unit = below.is_none_or(|below| {
            below.file_name().and_then(|it| it.to_str()).is_some_and(|it| OF_UNIT.contains(&it))
        });
        if is_unit && let Some(found) = as_unit_dir(dir) {
            return Some(found);
        }
        below = Some(dir);
    }
    None
}

fn as_unit_dir(dir: &Path) -> Option<UnitDir<'_>> {
    let hash = dir.file_name()?.to_str()?;
    let package_dir = dir.parent()?;
    let package = package_dir.file_name()?.to_str()?;
    let build = package_dir.parent()?;
    (is_unit_hash(hash) && build.file_name().is_some_and(|it| it == "build")).then_some(UnitDir {
        dir,
        package,
        hash,
    })
}

/// The directory of the profile (`target/debug`, `target/<triple>/release`) that `path`, a
/// file or directory cargo has rustc write to, is in, in either layout. `None` for one of the
/// profile's own directory, where cargo puts the programs it is asked for: that directory is
/// its parent.
pub(crate) fn profile_of(path: &Path) -> Option<&Path> {
    if let Some(unit) = unit_dir(path) {
        return unit.profile();
    }
    let first = path
        .ancestors()
        .find(|it| it.file_name().and_then(|it| it.to_str()).is_some_and(|it| FIRST.contains(&it)));
    first.and_then(Path::parent)
}

/// The hash cargo names the unit that rustc writes to `out_dir` by: the one of its
/// `-C extra-filename`, or, for one the second layout gives none, its directory's.
pub(crate) fn unit_hash<'a>(extra_filename: &'a str, out_dir: &'a Path) -> Option<&'a str> {
    match extra_filename.strip_prefix('-') {
        Some(hash) if !hash.is_empty() => Some(hash),
        _ => unit_dir(out_dir).map(|it| it.hash),
    }
}

/// The name a unit is kept by, `<crate>-<hash>` as its files are named in the first layout,
/// for the unit of crate `crate_name` that rustc writes to `out_dir` with `extra_filename`.
/// The second layout names a program or a build script after its crate alone, and every
/// package's build script is `build_script_build`: the hash of its directory tells them
/// apart.
pub(crate) fn unit_name(crate_name: &str, extra_filename: &str, out_dir: &Path) -> String {
    match (extra_filename.is_empty(), unit_dir(out_dir)) {
        (true, Some(unit)) => format!("{crate_name}-{}", unit.hash),
        _ => format!("{crate_name}{extra_filename}"),
    }
}

/// The name the unit that wrote the file of `stem` (`lib<stem>.rlib`, `<stem>.d`, without
/// the `lib`) in `dir` is kept by, as [`unit_name`] gives it.
pub(crate) fn unit_of_file(stem: &str, dir: &Path) -> String {
    match unit_dir(dir) {
        Some(unit) if !stem.ends_with(&format!("-{}", unit.hash)) => {
            format!("{stem}-{}", unit.hash)
        }
        _ => stem.to_owned(),
    }
}

/// The directories of the second layout's units below `profile` that hold a dynamic library
/// with no hash in its name: a `dylib` (or `cdylib`), which a program cargo runs loads from
/// there. A proc macro's library has the hash of its unit in its name, and is the compiler's
/// to load. The first layout keeps them all in `deps`.
pub(crate) fn dylib_dirs(profile: &Path) -> Vec<std::path::PathBuf> {
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let read = |dir: &Path| std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut dirs = Vec::new();
    for package in read(&profile.join("build")) {
        for unit in read(&package.path()) {
            let out = unit.path().join("out");
            let Some(found) = unit_dir(&out) else { continue };
            let hashed = format!("-{}{suffix}", found.hash);
            let dylib = read(&out).any(|it| {
                it.file_name().to_str().is_some_and(|name| {
                    name.starts_with(prefix) && name.ends_with(suffix) && !name.ends_with(&hashed)
                })
            });
            if dylib {
                dirs.push(out);
            }
        }
    }
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "9088098a7e9115d7";

    #[test]
    fn a_profile_is_found_in_both_layouts() {
        let profile = Path::new("/t/debug");
        let second = format!("/t/debug/build/ct/{HASH}");
        for path in [
            "/t/debug/deps",
            "/t/debug/deps/libct-1.rlib",
            "/t/debug/examples/ex-1",
            "/t/debug/build/ct-1/out",
            "/t/debug/.fingerprint/ct-1",
            &second,
            &format!("{second}/out/libct-{HASH}.rlib"),
            &format!("{second}/fingerprint"),
            &format!("{second}/run/stdout"),
            // A package named as a directory of the first layout.
            &format!("/t/debug/build/build/{HASH}/out"),
            &format!("/t/debug/build/deps/{HASH}/out/libdeps-{HASH}.rlib"),
        ] {
            assert_eq!(profile_of(Path::new(path)), Some(profile), "{path}");
        }
        assert_eq!(profile_of(Path::new("/t/debug/app")), None);
        let triple = format!("/t/x86_64-unknown-linux-gnu/release/build/ct/{HASH}/out");
        assert_eq!(
            profile_of(Path::new(&triple)),
            Some(Path::new("/t/x86_64-unknown-linux-gnu/release"))
        );
    }

    #[test]
    fn a_unit_is_named_alike_in_both_layouts() {
        let first = Path::new("/t/debug/deps");
        let second = format!("/t/debug/build/ct/{HASH}/out");
        let second = Path::new(&second);
        let extra = format!("-{HASH}");
        assert_eq!(unit_name("ct", &extra, first), format!("ct-{HASH}"));
        assert_eq!(unit_name("ct", &extra, second), format!("ct-{HASH}"));
        assert_eq!(
            unit_name("build_script_build", "", second),
            format!("build_script_build-{HASH}")
        );
        assert_eq!(unit_name("app", "", first), "app");
        assert_eq!(unit_hash(&extra, first), Some(HASH));
        assert_eq!(unit_hash("", second), Some(HASH));
        assert_eq!(unit_hash("", first), None);
        assert_eq!(unit_of_file(&format!("ct-{HASH}"), second), format!("ct-{HASH}"));
        assert_eq!(unit_of_file("shared", second), format!("shared-{HASH}"));
        assert_eq!(unit_of_file("ct-1", first), "ct-1");
    }

    #[test]
    fn a_directory_is_a_unit_only_where_cargo_puts_one() {
        // A source directory that happens to look like one, below no `out`, is not.
        let source = format!("/ws/build/gen/{HASH}/src/lib.rs");
        assert_eq!(unit_dir(Path::new(&source)), None);
        let out = format!("/t/debug/build/ct/{HASH}/out/a/b.rs");
        let found = unit_dir(Path::new(&out)).unwrap();
        assert_eq!((found.package, found.hash), ("ct", HASH));
        assert_eq!(found.name(), format!("ct-{HASH}"));
        assert_eq!(unit_dir(Path::new("/t/debug/build/ct-1/out")), None);
    }

    #[test]
    fn the_dylibs_of_the_second_layout_are_found() {
        let dir = std::env::temp_dir().join(format!("ra-jit-layout-{}", std::process::id()));
        let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
        let put = |package: &str, hash: &str, file: String| {
            let out = dir.join("debug/build").join(package).join(hash).join("out");
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(out.join(file), b"").unwrap();
            out
        };
        let dylib = put("shared", HASH, format!("{prefix}shared{suffix}"));
        let other = "0123456789abcdef";
        put("macros", other, format!("{prefix}macros-{other}{suffix}"));
        put("lib", "fedcba9876543210", "liblib-fedcba9876543210.rlib".to_owned());
        assert_eq!(dylib_dirs(&dir.join("debug")), [dylib]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
