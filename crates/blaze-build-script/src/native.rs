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
//! ([`seatbelt`]) and the paths Linux's Landlock is given come from one [`Grant`]. Since D177
//! the process is started by bstd's hermetic run (blaze-proc-macro native_script.rs), whose
//! plan renders its own profile and ruleset; here [`seatbelt_with`] is only the grant's check.
#![blaze::sans_io]

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_unit_graph::{UnitGraph, UnitId};

/// The crates that make a build script start a compiler or load libclang: `cc` (a C or C++
/// compiler and `ar`), `cmake` (cmake, then the compiler), `bindgen` and `clang-sys` (libclang
/// in the script's own process), `pkg-config` (the pkg-config program, which only native
/// scripts can be refused by name). Sorted. `autocfg` is not here: it spawns `rustc`, which the
/// wasm runner's probe shim answers.
pub const TOOLCHAIN_CLASS: [&str; 5] = ["bindgen", "cc", "clang-sys", "cmake", "pkg-config"];

/// The crates that do not compile for `wasm32-wasip1`: a script whose dependency closure holds
/// one has no wasm module in any mode, so it runs native in `check` too
/// ([`crate::toolchain::placement`]). `libloading` has no `Library` on wasi; `clang-sys` needs it
/// (its `runtime` feature, bindgen's default) or links libclang (`static`); `bindgen` holds
/// `clang-sys`. turso 2026-11-21: pg_query 18.1.0's and turso_sdk_kit's scripts failed to
/// build as wasm at clang-sys (`cannot find Library in libloading`), though turso_sdk_kit's
/// build.rs never calls bindgen on macOS: the class is what the closure compiles, not what the
/// script does. Sorted.
pub const NO_WASM_CLASS: [&str; 3] = ["bindgen", "clang-sys", "libloading"];

/// What a script's dependency closure says of where it can run ([`closure`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Closure {
    /// Native when the closure holds a crate of [`TOOLCHAIN_CLASS`] or [`NO_WASM_CLASS`].
    pub runner: Runner,
    /// The closure's unit of [`NO_WASM_CLASS`] with the least id: the script cannot be wasm.
    pub no_wasm: Option<UnitId>,
}

/// Where a build script runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runner {
    /// `wasm32-wasip1` in Wasmtime.
    Wasm,
    /// Native, in the OS sandbox: `because` is the unit of [`TOOLCHAIN_CLASS`] in its closure
    /// with the least id (one answer for one graph).
    Native { because: UnitId },
}

/// Where `script` (a build-script unit) runs: [`closure`]'s runner.
pub fn runner(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> Runner {
    closure(graph, script, alloc).runner
}

/// What `script`'s dependency closure says: native when a unit of a package named in
/// [`TOOLCHAIN_CLASS`] or [`NO_WASM_CLASS`] is in it, else wasm; and its least
/// [`NO_WASM_CLASS`] unit. The walk keeps its own stack (no recursion) and visits each unit once.
pub fn closure(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> Closure {
    let count = graph.units.len();
    let mut seen: AVec<'_, bool> = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        seen.push(false);
    }
    let mut stack: AVec<'_, UnitId> = Vec::new_in(alloc);
    for dep in graph.units[script.0 as usize].deps.iter() {
        stack.push(dep.unit);
    }
    let least = |held: Option<UnitId>, at: UnitId| {
        Some(match held {
            Some(found) if found.0 < at.0 => found,
            _ => at,
        })
    };
    let mut because: Option<UnitId> = None;
    let mut no_wasm: Option<UnitId> = None;
    while let Some(at) = stack.pop() {
        let index = at.0 as usize;
        if seen[index] {
            continue;
        }
        seen[index] = true;
        let unit = &graph.units[index];
        let name = unit.name.as_str();
        let cannot_be_wasm = NO_WASM_CLASS.binary_search(&name).is_ok();
        if cannot_be_wasm {
            no_wasm = least(no_wasm, at);
        }
        if cannot_be_wasm || TOOLCHAIN_CLASS.binary_search(&name).is_ok() {
            because = least(because, at);
        }
        for dep in unit.deps.iter() {
            stack.push(dep.unit);
        }
    }
    let runner = match because {
        Some(because) => Runner::Native { because },
        None => Runner::Wasm,
    };
    Closure { runner, no_wasm }
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
    seatbelt_with(grant, &[], alloc)
}

/// [`seatbelt`] of `grant`, and also any program under the folders `exec_under` started (and
/// read): the one home of the deny-by-default profile, which the interim builder of a native
/// script takes too (cargo starts rustc, the linking clang, `ld64.lld`, and the build scripts
/// and proc macros of the script's build-dependencies it has just written under its target
/// folder: `exec_under` names the toolchain's and the shelf's `bin` and that folder).
pub fn seatbelt_with<'a, 'g>(grant: &Grant<'g>, exec_under: &[&'g str], alloc: Alloc<'a>) -> Result<AString<'a>, GrantError<'g>> {
    let mut out: AString<'a> = String::new_in(alloc);
    out.push_str("(version 1)\n(deny default)\n(allow process-fork)\n(allow sysctl-read)\n");
    out.push_str("(allow signal (target same-sandbox))\n(allow process-exec");
    quoted(&mut out, "literal", checked(grant.program)?);
    for program in grant.exec {
        quoted(&mut out, "literal", checked(program)?);
    }
    for folder in exec_under {
        quoted(&mut out, "subpath", checked(folder)?);
    }
    out.push_str(")\n(allow file-read*");
    for folder in exec_under {
        quoted(&mut out, "subpath", folder);
    }
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
    let named = [grant.program]
        .into_iter()
        .chain(grant.exec.iter().copied())
        .chain(grant.read.iter().copied())
        .chain(grant.write.iter().copied())
        .chain(exec_under.iter().copied());
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
