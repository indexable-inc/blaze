//! D65: the build scripts that run native, decided from text before the run
//! (build-scripts.md, "Native scripts (D65)").
//!
//! A script whose dependency closure holds a crate of [`TOOLCHAIN_CLASS`] starts a C compiler
//! or loads libclang, which wasm cannot: it is built for the host and run confined, with the
//! pinned clang the only compiler it can see. Every other script stays wasm. The choice is part
//! of the run's key ([`crate::key::RunnerKey`]); a wasm failure is never retried native
//! (rule 3).
//!
//! Confinement itself lives in `bstd::procs::hermetic` (D177), whose plan renders both the
//! macOS Seatbelt profile and the Linux Landlock ruleset from one [`bstd::procs::hermetic::HermeticSpec`].
#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};
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
