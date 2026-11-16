//! D65: the build scripts that run native, decided from text before the run, and the policy of
//! the operating system's sandbox they run in (build-scripts.md, "Native scripts (D65)").
//!
//! A script whose dependency closure holds a crate of [`TOOLCHAIN_CLASS`] starts a C compiler
//! or loads libclang, which wasm cannot: it is built for the host and run confined, with the
//! pinned clang the only compiler it can see. Every other script stays wasm. The choice is part
//! of the run's key ([`crate::key::RunnerKey`]); a wasm failure is never retried native
//! (rule 3).
//!
//! The policy is text and lists made here, sans-IO: the macOS Seatbelt profile
//! ([`seatbelt`]) and the paths Linux's Landlock is given come from one [`Grant`]. The process
//! is started by blaze-proc-macro's `native_sandbox`.
#![blaze::sans_io]

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_unit_graph::{UnitGraph, UnitId};

/// The crates that make a build script start a compiler or load libclang: `cc` (a C or C++
/// compiler and `ar`), `cmake` (cmake, then the compiler), `bindgen` and `clang-sys` (libclang
/// in the script's own process), `pkg-config` (the pkg-config program, which only native
/// scripts can be refused by name). Sorted. `autocfg` is not here: it spawns `rustc`, which the
/// wasm runner's probe shim answers.
pub const TOOLCHAIN_CLASS: [&str; 5] = ["bindgen", "cc", "clang-sys", "cmake", "pkg-config"];

/// Where a build script runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runner {
    /// `wasm32-wasip1` in Wasmtime.
    Wasm,
    /// Native, in the OS sandbox: `because` is the unit of [`TOOLCHAIN_CLASS`] in its closure
    /// with the least id (one answer for one graph).
    Native { because: UnitId },
}

/// Where `script` (a build-script unit) runs: native when a unit of a package named in
/// [`TOOLCHAIN_CLASS`] is in its dependency closure, else wasm. The walk keeps its own stack (no
/// recursion) and visits each unit once.
pub fn runner(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> Runner {
    let count = graph.units.len();
    let mut seen: AVec<'_, bool> = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        seen.push(false);
    }
    let mut stack: AVec<'_, UnitId> = Vec::new_in(alloc);
    for dep in graph.units[script.0 as usize].deps.iter() {
        stack.push(dep.unit);
    }
    let mut because: Option<UnitId> = None;
    while let Some(at) = stack.pop() {
        let index = at.0 as usize;
        if seen[index] {
            continue;
        }
        seen[index] = true;
        let unit = &graph.units[index];
        if TOOLCHAIN_CLASS.contains(&unit.name.as_str()) {
            because = Some(match because {
                Some(found) if found.0 < at.0 => found,
                _ => at,
            });
        }
        for dep in unit.deps.iter() {
            stack.push(dep.unit);
        }
    }
    match because {
        Some(because) => Runner::Native { because },
        None => Runner::Wasm,
    }
}

/// What a native script's process may do. Every path is absolute and canonical (the caller
/// resolves links first: Seatbelt and Landlock both match the resolved path).
#[derive(Clone, Copy, Debug)]
pub struct Grant<'g> {
    /// The script's own binary: executed and read.
    pub program: &'g str,
    /// The only other programs it may start: the pinned `clang`, `clang++`, `llvm-ar`.
    pub exec: &'g [&'g str],
    /// Read with what is under them: the package folder, the dependencies' outputs, the
    /// pinned toolchain's folder.
    pub read: &'g [&'g str],
    /// Written (and read) with what is under them: `OUT_DIR` alone.
    pub write: &'g [&'g str],
}

/// What macOS's loader reads before `main` (found by narrowing a `(deny default)` profile on
/// macOS 27, arm64): the root folder itself (a literal, not what is under it), the system
/// libraries and the dyld shared cache. None holds a compiler; `/System/Volumes/Data` (the
/// user's files under another name), `/usr/bin`, `/Library` and `/Applications` are not read.
pub const MACOS_LOADER_READS: [&str; 3] = ["/usr/lib", "/System/Library", "/System/Volumes/Preboot/Cryptexes/OS"];

/// A grant that cannot be said, refused before anything runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GrantError<'g> {
    /// Not absolute, or with an empty, `.` or `..` part.
    NotCanonical { path: &'g str },
    /// A quote, backslash or control character: a profile string cannot hold it as given.
    Unsayable { path: &'g str },
}

impl core::fmt::Display for GrantError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GrantError::NotCanonical { path } => {
                write!(f, "{path}: a native script's grant takes only absolute paths without `.` or `..` (D65)")
            }
            GrantError::Unsayable { path } => {
                write!(f, "{path}: a quote, backslash or control character cannot be put in the sandbox's profile (D65)")
            }
        }
    }
}

/// `path` checked to be absolute, canonical and sayable.
pub fn checked(path: &str) -> Result<&str, GrantError<'_>> {
    if path.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
        return Err(GrantError::Unsayable { path });
    }
    let Some(rest) = path.strip_prefix('/') else { return Err(GrantError::NotCanonical { path }) };
    if path != "/" && rest.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return Err(GrantError::NotCanonical { path });
    }
    Ok(path)
}

fn quoted(out: &mut AString<'_>, filter: &str, path: &str) {
    out.push_str(" (");
    out.push_str(filter);
    out.push_str(" \"");
    out.push_str(path);
    out.push_str("\")");
}

/// The Seatbelt profile of `grant`: everything denied but what it names. No network (denied by
/// default), no Mach service, no write outside `write`, no read outside `read`, `write`, the
/// program and [`MACOS_LOADER_READS`], no program started but `program` and `exec`. A forked
/// child keeps the profile.
pub fn seatbelt<'a, 'g>(grant: &Grant<'g>, alloc: Alloc<'a>) -> Result<AString<'a>, GrantError<'g>> {
    let mut out: AString<'a> = String::new_in(alloc);
    out.push_str("(version 1)\n(deny default)\n(allow process-fork)\n(allow sysctl-read)\n");
    out.push_str("(allow signal (target same-sandbox))\n(allow process-exec");
    quoted(&mut out, "literal", checked(grant.program)?);
    for program in grant.exec {
        quoted(&mut out, "literal", checked(program)?);
    }
    out.push_str(")\n(allow file-read*");
    quoted(&mut out, "literal", "/");
    quoted(&mut out, "literal", grant.program);
    for program in grant.exec {
        quoted(&mut out, "literal", program);
    }
    for path in MACOS_LOADER_READS {
        quoted(&mut out, "subpath", path);
    }
    for path in grant.read.iter().chain(grant.write) {
        quoted(&mut out, "subpath", checked(path)?);
    }
    quoted(&mut out, "literal", "/dev/null");
    // The folders above each granted path, their metadata only (not what is in them): a
    // program finds its own real path by `realpath`, which looks at each one (clang finds its
    // resource headers, `lib/clang/<v>/include`, beside its real path so; without this it
    // finds none, D69).
    out.push_str(")\n(allow file-read-metadata");
    let named = [grant.program].into_iter().chain(grant.exec.iter().copied()).chain(grant.read.iter().copied()).chain(grant.write.iter().copied());
    for path in named {
        for (at, _) in path.match_indices('/').filter(|(at, _)| *at > 0) {
            quoted(&mut out, "literal", &path[..at]);
        }
    }
    out.push_str(")\n(allow file-write*");
    for path in grant.write {
        quoted(&mut out, "subpath", path);
    }
    quoted(&mut out, "literal", "/dev/null");
    out.push_str(")\n");
    Ok(out)
}
