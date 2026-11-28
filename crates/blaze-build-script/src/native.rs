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

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_unit_graph::{Unit, UnitGraph, UnitId};

/// The crates that make a build script start a compiler or load libclang: `cc` (a C or C++
/// compiler and `ar`), `cmake` (cmake, then the compiler), `bindgen` and `clang-sys` (libclang
/// in the script's own process), `pkg-config` (the pkg-config program, which only native
/// scripts can be refused by name). Sorted. `autocfg` is not here: it spawns `rustc`, which the
/// wasm runner's probe shim answers.
pub const TOOLCHAIN_CLASS: [&str; 5] = ["bindgen", "cc", "clang-sys", "cmake", "pkg-config"];

/// The crates.io crates that do not compile for `wasm32-wasip1` / `wasm64-blaze-wasip1`: a script
/// whose dependency closure holds one (qualified by crates.io registry source identity via
/// [`closure_with_source`]) has no wasm module in any mode, so it runs native in `check` too
/// ([`crate::toolchain::placement`]). `libloading` has no `Library` on wasi; `clang-sys` needs it
/// (its `runtime` feature, bindgen's default) or links libclang (`static`); `bindgen` holds
/// `clang-sys`. Protobuf/gRPC codegen build-deps (`prost-build`, `protobuf-build`,
/// `protoc-bin-vendored`, `tonic-build`, `tonic-prost-build`) spawn native `protoc` / ship host
/// binaries and require Tier-2 native execution. turso 2026-11-21: pg_query 18.1.0's and
/// turso_sdk_kit's scripts failed to build as wasm at clang-sys (`cannot find Library in
/// libloading`), though turso_sdk_kit's build.rs never calls bindgen on macOS: the class is what
/// the closure compiles, not what the script does. Sorted.
pub const NO_WASM_CLASS: [&str; 8] = [
    "bindgen",
    "clang-sys",
    "libloading",
    "prost-build",
    "protobuf-build",
    "protoc-bin-vendored",
    "tonic-build",
    "tonic-prost-build",
];

/// Source-identity class of a dependency package for [`closure_with_source`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackageSource {
    /// Official crates.io registry (`LockSource::is_crates_io()`).
    CratesIo,
    /// Workspace member, local path, git, or alternate registry package: never matched against
    /// crates.io-specific [`NO_WASM_CLASS`] entries by bare name alone.
    Other,
}

/// Why a build script's own Rust source text requires Tier-2 native host execution
/// ([`source_requires_native`], [`source_tree_requires_native`]). Decided deterministically
/// before compilation or execution (rules 3 and 54).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeSourceReason {
    /// Unconditional reference to host-OS-specific `std::os::{unix,linux,macos}` APIs
    /// (absent on `wasm32-wasip1` / `wasm64-blaze-wasip1`), not guarded by a WASI-excluding
    /// `#[cfg(...)]`. (`std::os::fd` is exposed on `target_os = "wasi"` and stays Wasm.)
    HostOsStd,
    /// Unconditional foreign-function block (`extern "C" { ... }`, `extern "system" { ... }`,
    /// `extern { ... }`, or `unsafe extern ... { ... }`), not guarded by a WASI-excluding
    /// `#[cfg(...)]`.
    ExternC,
    /// Crate-level `#![cfg(...)]` or runtime `cfg!(...)` predicate whose value diverges between
    /// the supported Unix host and WASI (e.g. `cfg!(target_family = "unix")`, `cfg!(unix)`,
    /// `cfg!(not(unix))`, `cfg!(target_os = "wasi")`, or crate-level `#![cfg(unix)]`).
    HostFamilyCfg,
}

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

/// What `script`'s dependency closure says when no lockfile source metadata is available:
/// treats every unit as [`PackageSource::CratesIo`]. Prefer [`closure_with_source`] whenever
/// lockfile package origins are available so local/path/git/alt-registry packages sharing a
/// crates.io name are not misclassified.
pub fn closure(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> Closure {
    closure_with_source(graph, script, alloc, |_| PackageSource::CratesIo)
}

/// Source-qualified version of [`closure`]: [`NO_WASM_CLASS`] matches a dependency unit ONLY when
/// `package_source(unit.package) == PackageSource::CratesIo`, so an unrelated local workspace,
/// path, git, or alternate-registry crate sharing a legal name such as `prost-build` or
/// `tonic-build` is never falsely forced onto the native runner.
///
/// The walk keeps its own stack (no recursion) and visits each unit once.
pub fn closure_with_source(
    graph: &UnitGraph<'_>,
    script: UnitId,
    alloc: Alloc<'_>,
    mut package_source: impl FnMut(&Unit<'_>) -> PackageSource,
) -> Closure {
    let count = graph.units.len();
    let mut seen: AVec<'_, bool> = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        seen.push(false);
    }
    let mut stack: AVec<'_, UnitId> = Vec::new_in(alloc);
    if let Some(root) = graph.units.get(script.index()) {
        for dep in root.deps.iter() {
            stack.push(dep.unit);
        }
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
        let index = at.index();
        let Some(already) = seen.get_mut(index) else { continue };
        if *already {
            continue;
        }
        *already = true;
        let Some(unit) = graph.units.get(index) else { continue };
        let name = unit.name.as_str();
        let is_crates_io = package_source(unit) == PackageSource::CratesIo;
        let cannot_be_wasm = is_crates_io && NO_WASM_CLASS.binary_search(&name).is_ok();
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

/// Single source of truth for deterministic pre-run build-script placement (rule 30) given an
/// already-computed [`NativeSourceReason`] from [`source_requires_native`] or
/// [`source_tree_requires_native`].
pub fn placement_with_source_reason(
    closure: Closure,
    mode: crate::toolchain::Mode,
    package: &str,
    script: UnitId,
    source_reason: Option<NativeSourceReason>,
) -> Runner {
    let base = crate::toolchain::placement(closure, mode, package);
    if matches!(base, Runner::Native { .. }) {
        return base;
    }
    if source_reason.is_some() {
        return Runner::Native { because: script };
    }
    base
}

/// Single source of truth for deterministic pre-run build-script placement (rule 30): combines
/// closure-based placement ([`crate::toolchain::placement`]) with static inspection of the build
/// script's own Rust source ([`source_requires_native`]).
///
/// When `source` is `Some(src)` and [`source_requires_native`] detects an unguarded host-OS API,
/// `extern "..." { ... }` block, crate-level `#![cfg(...)]`, or host-vs-WASI diverging
/// `cfg!(...)`, places the script on [`Runner::Native`] (`because: script`) in every mode before
/// any compilation or execution begins (rules 3 and 54).
pub fn placement_with_source(
    closure: Closure,
    mode: crate::toolchain::Mode,
    package: &str,
    script: UnitId,
    source: Option<&str>,
) -> Runner {
    placement_with_source_reason(closure, mode, package, script, source.and_then(source_requires_native))
}

/// Concrete host target and unit cfg facts for evaluating `#[cfg(...)]` and `cfg!(...)`
/// predicates against the supported host triple vs WASI (`wasm64-blaze-wasip1` / `wasm32-wasip1`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScriptCfg<'a> {
    /// Concrete host `target_os` (e.g. `"linux"`, `"macos"`), or `None` if unknown.
    pub host_os: Option<&'a str>,
    /// Concrete host `target_arch` (e.g. `"aarch64"`, `"x86_64"`), or `None` if unknown.
    pub host_arch: Option<&'a str>,
    /// Concrete host `target_env` (e.g. `"gnu"`, `"musl"`, `""`), or `None` if unknown.
    pub host_env: Option<&'a str>,
    /// Concrete host `target_vendor` (e.g. `"apple"`, `"unknown"`), or `None` if unknown.
    pub host_vendor: Option<&'a str>,
    /// Concrete host `target_pointer_width` (e.g. `"64"`), or `None` if unknown.
    pub host_pointer_width: Option<&'a str>,
    /// Active Cargo features for the build script unit (`unit.features`).
    /// When `Some(list)`, `feature = "x"` evaluates definitively (`True` iff in `list`, else
    /// `False`). When `None`, `feature = "x"` evaluates as `Unknown` on both host and WASI.
    pub features: Option<&'a [&'a str]>,
}

impl<'a> ScriptCfg<'a> {
    /// Host target facts for the machine compiling/running Blaze, with unknown unit features.
    pub const fn current_host() -> Self {
        let host_os = if cfg!(target_os = "macos") {
            Some("macos")
        } else if cfg!(target_os = "linux") {
            Some("linux")
        } else {
            None
        };
        let host_arch = if cfg!(target_arch = "aarch64") {
            Some("aarch64")
        } else if cfg!(target_arch = "x86_64") {
            Some("x86_64")
        } else {
            None
        };
        let host_env = if cfg!(target_os = "macos") {
            Some("")
        } else if cfg!(target_env = "gnu") {
            Some("gnu")
        } else if cfg!(target_env = "musl") {
            Some("musl")
        } else {
            None
        };
        let host_vendor = if cfg!(target_os = "macos") {
            Some("apple")
        } else if cfg!(target_os = "linux") {
            Some("unknown")
        } else {
            None
        };
        let host_pointer_width = if cfg!(target_pointer_width = "64") {
            Some("64")
        } else if cfg!(target_pointer_width = "32") {
            Some("32")
        } else {
            None
        };
        Self { host_os, host_arch, host_env, host_vendor, host_pointer_width, features: None }
    }

    /// Concrete host target facts parsed from a supported host triple (e.g.
    /// `"aarch64-apple-darwin"`, `"aarch64-unknown-linux-gnu"`, `"x86_64-unknown-linux-gnu"`)
    /// paired with the build script unit's active Cargo features.
    pub fn from_host_triple(triple: &str, features: Option<&'a [&'a str]>) -> Self {
        if triple.is_empty() {
            return Self { features, ..Self::current_host() };
        }
        let (host_arch, host_os, host_env, host_vendor, host_pointer_width) = match triple {
            "aarch64-apple-darwin" => (Some("aarch64"), Some("macos"), Some(""), Some("apple"), Some("64")),
            "x86_64-apple-darwin" => (Some("x86_64"), Some("macos"), Some(""), Some("apple"), Some("64")),
            "aarch64-unknown-linux-gnu" => (Some("aarch64"), Some("linux"), Some("gnu"), Some("unknown"), Some("64")),
            "x86_64-unknown-linux-gnu" => (Some("x86_64"), Some("linux"), Some("gnu"), Some("unknown"), Some("64")),
            "aarch64-unknown-linux-musl" => (Some("aarch64"), Some("linux"), Some("musl"), Some("unknown"), Some("64")),
            "x86_64-unknown-linux-musl" => (Some("x86_64"), Some("linux"), Some("musl"), Some("unknown"), Some("64")),
            _ => (None, None, None, None, None),
        };
        Self { host_os, host_arch, host_env, host_vendor, host_pointer_width, features }
    }
}

/// Maximum number of distinct files inspected by [`source_tree_requires_native`] for one build
/// script (cycle and pathological fan-out bound, rule 18).
pub const MAX_SCRIPT_FILES: usize = 256;

/// Maximum nesting depth when scanning nested `macro_rules!` transcriber bodies.
const MAX_MACRO_DEPTH: usize = 8;

/// Maximum path-qualifier segments tracked for qualified macro calls (`crate::a::b::m!()`).
const MAX_QUAL_SEGS: usize = 4;

/// Failure while inspecting a build script's source tree ([`source_tree_requires_native`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScriptSourceError<'a, E> {
    /// Underlying file read / decode error returned by the caller's reader callback.
    Read(E),
    /// The reachable module/`include!` graph exceeded [`MAX_SCRIPT_FILES`]; refused rather than
    /// treating an incomplete scan as proof that no native requirement exists (rules 18 and 54).
    TooManyFiles { limit: usize, path: AString<'a> },
}

/// Inspects a build script's root file and every active submodule (`mod foo;`,
/// `#[path = "..."] mod foo;`) and static `include!("...")` / `include!(r#"..."#)` file
/// reachable from it, evaluating `#[cfg(...)]` and `cfg!(...)` against `cfg` (concrete host
/// triple + unit features) vs WASI, and resolving `macro_rules!` invocations in lexical scope.
///
/// `read_file(rel, is_root, visit)` calls `visit(source_text)` and returns `Ok(true)` when the
/// file exists, `Ok(false)` when absent (used to probe `foo.rs` before `foo/mod.rs`), or `Err(e)`
/// on any read/decode failure (propagated as [`ScriptSourceError::Read`]). Source trees that
/// exceed [`MAX_SCRIPT_FILES`] are refused with [`ScriptSourceError::TooManyFiles`] rather than
/// silently treated as Wasm-safe.
pub fn source_tree_requires_native<'a, E, F>(
    root_rel: &str,
    cfg: ScriptCfg<'_>,
    alloc: Alloc<'a>,
    mut read_file: F,
) -> Result<Option<NativeSourceReason>, ScriptSourceError<'a, E>>
where
    F: FnMut(&str, bool, &mut dyn FnMut(&str)) -> Result<bool, E>,
{
    enum Pending<'a> {
        Root { path: AString<'a>, scope: usize },
        Exact { path: AString<'a>, scope: usize, origin_order: Option<usize> },
        Module { primary: AString<'a>, fallback: AString<'a>, scope: usize },
    }

    let Some(norm_root) = normalize_rel_path("", root_rel, alloc) else {
        return Ok(None);
    };

    let mut visited: AVec<'_, AString<'_>> = Vec::new_in(alloc);
    let mut queue: AVec<'_, Pending<'_>> = Vec::new_in(alloc);
    queue.push(Pending::Root { path: norm_root, scope: 0 });

    let mut sink = TreeSink::new(alloc);
    let mut head: usize = 0;

    while let Some(item) = queue.get(head) {
        head += 1;
        let (is_root, primary_str, fallback_str, file_scope, origin_order) = match item {
            Pending::Root { path, scope } => (true, path.as_str(), None, *scope, None),
            Pending::Exact { path, scope, origin_order } => (false, path.as_str(), None, *scope, *origin_order),
            Pending::Module { primary, fallback, scope } => (false, primary.as_str(), Some(fallback.as_str()), *scope, None),
        };

        let p_copy = String::from_str_in(primary_str, alloc);
        let fb_copy = fallback_str.map(|fb| String::from_str_in(fb, alloc));

        let mut scanned_ok = false;
        let mut reason_hit: Option<NativeSourceReason> = None;

        if !visited.iter().any(|seen| seen.as_str() == p_copy.as_str()) {
            if visited.len() >= MAX_SCRIPT_FILES {
                return Err(ScriptSourceError::TooManyFiles { limit: MAX_SCRIPT_FILES, path: p_copy });
            }
            sink.set_origin_order(origin_order);
            let existed = read_file(p_copy.as_str(), is_root, &mut |text| {
                reason_hit = scan_source_core(text, is_root, p_copy.as_str(), file_scope, cfg, 0, &mut sink);
            })
            .map_err(ScriptSourceError::Read)?;
            if existed {
                scanned_ok = true;
                visited.push(p_copy);
            }
        } else {
            scanned_ok = true;
        }

        if let Some(reason) = reason_hit {
            return Ok(Some(reason));
        }

        if !scanned_ok
            && let Some(fb) = fb_copy
            && !visited.iter().any(|seen| seen.as_str() == fb.as_str())
        {
            if visited.len() >= MAX_SCRIPT_FILES {
                return Err(ScriptSourceError::TooManyFiles { limit: MAX_SCRIPT_FILES, path: fb });
            }
            sink.set_origin_order(origin_order);
            let existed = read_file(fb.as_str(), false, &mut |text| {
                reason_hit = scan_source_core(text, false, fb.as_str(), file_scope, cfg, 0, &mut sink);
            })
            .map_err(ScriptSourceError::Read)?;
            if existed {
                visited.push(fb);
            }
            if let Some(reason) = reason_hit {
                return Ok(Some(reason));
            }
        }

        if let Some(reason) = sink.propagate_invoked_macros() {
            return Ok(Some(reason));
        }

        for child in sink.take_top_children() {
            let already_seen = match &child {
                ChildRef::Exact { path, .. } => {
                    visited.iter().any(|seen| seen.as_str() == path.as_str())
                        || queue.get(head..).unwrap_or(&[]).iter().any(|pending| match pending {
                            Pending::Root { path: p, .. } | Pending::Exact { path: p, .. } => p.as_str() == path.as_str(),
                            Pending::Module { .. } => false,
                        })
                }
                ChildRef::Module { primary, fallback, .. } => {
                    visited.iter().any(|seen| seen.as_str() == primary.as_str() || seen.as_str() == fallback.as_str())
                        || queue.get(head..).unwrap_or(&[]).iter().any(|pending| match pending {
                            Pending::Module { primary: qp, fallback: qf, .. } => {
                                qp.as_str() == primary.as_str() && qf.as_str() == fallback.as_str()
                            }
                            _ => false,
                        })
                }
            };
            if already_seen {
                continue;
            }
            if visited.len() + queue.len().saturating_sub(head) >= MAX_SCRIPT_FILES * 2 {
                let path = match child {
                    ChildRef::Exact { path, .. } => path,
                    ChildRef::Module { primary, .. } => primary,
                };
                return Err(ScriptSourceError::TooManyFiles { limit: MAX_SCRIPT_FILES, path });
            }
            match child {
                ChildRef::Exact { path, scope, origin_order } => queue.push(Pending::Exact { path, scope, origin_order }),
                ChildRef::Module { primary, fallback, scope } => queue.push(Pending::Module { primary, fallback, scope }),
            }
        }
    }

    Ok(None)
}

/// Inspects a single build script's Rust source text using [`ScriptCfg::current_host`] (zero
/// allocations, sans-IO, exact Rust lexical scanning) and returns `Some(reason)` when the source
/// unconditionally requires Tier-2 native host execution rather than `wasm32-wasip1` /
/// `wasm64-blaze-wasip1`.
pub fn source_requires_native(source: &str) -> Option<NativeSourceReason> {
    source_requires_native_with_cfg(source, ScriptCfg::current_host())
}

/// Like [`source_requires_native`], evaluating `#[cfg(...)]` and `cfg!(...)` against explicit
/// `cfg` facts (concrete host triple + active build-script unit features).
pub fn source_requires_native_with_cfg(source: &str, cfg: ScriptCfg<'_>) -> Option<NativeSourceReason> {
    scan_source_no_alloc(source, cfg)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrLit<'a> {
    /// Ordinary `"..."` string literal whose Rust escapes must be decoded.
    Cooked(&'a str),
    /// Raw `r"..."` / `r#"..."#` string literal whose contents are verbatim.
    Raw(&'a str),
}

enum ChildRef<'a> {
    Exact { path: AString<'a>, scope: usize, origin_order: Option<usize> },
    Module { primary: AString<'a>, fallback: AString<'a>, scope: usize },
}

#[derive(Clone, Copy)]
struct CallSite<'s> {
    name: &'s str,
    qualifier: [&'s str; MAX_QUAL_SEGS],
    qual_len: usize,
    leading_global: bool,
    scope: usize,
}

trait ScanSink<'s> {
    fn enter_scope(&mut self, parent: usize, mod_name: Option<&'s str>, macro_use: bool, is_module: bool) -> usize;
    fn mark_scope_macro_use(&mut self, scope: usize);
    fn push_external_mod(
        &mut self,
        is_root: bool,
        current_rel: &str,
        current_scope: usize,
        inline_mods: &[Option<&str>],
        mod_name: &'s str,
        path_attr: Option<StrLit<'s>>,
        macro_use: bool,
    );
    fn push_include(&mut self, current_rel: &str, current_scope: usize, inc_path: StrLit<'s>);
    fn push_invocation(&mut self, call: CallSite<'s>);
    fn enter_macro(&mut self, name: &'s str, scope: usize, macro_export: bool) -> Option<(usize, Option<usize>)>;
    fn leave_macro(&mut self, def_id: usize, prev_active: Option<usize>, reason: Option<NativeSourceReason>);
}

const MAX_STACK_SCOPES: usize = 128;
const MAX_STACK_MACRO_DEFS: usize = 32;
const MAX_STACK_MACRO_CALLEES: usize = 8;
const MAX_STACK_CALLS: usize = 64;

#[derive(Clone, Copy)]
struct StackScope<'s> {
    parent: Option<usize>,
    mod_name: Option<&'s str>,
    macro_use: bool,
    is_module: bool,
    decl_pos: (usize, usize),
}

impl<'s> StackScope<'s> {
    const ROOT: Self = Self { parent: None, mod_name: None, macro_use: false, is_module: true, decl_pos: (0, 0) };
}

#[derive(Clone, Copy)]
struct StackCall<'s> {
    name: &'s str,
    qualifier: [&'s str; MAX_QUAL_SEGS],
    qual_len: usize,
    leading_global: bool,
    scope: usize,
    pos: (usize, usize),
}

impl<'s> StackCall<'s> {
    const EMPTY: Self = Self {
        name: "",
        qualifier: [""; MAX_QUAL_SEGS],
        qual_len: 0,
        leading_global: false,
        scope: 0,
        pos: (0, 0),
    };
}

#[derive(Clone, Copy)]
struct StackMacroDef<'s> {
    name: &'s str,
    scope: usize,
    macro_export: bool,
    pos: (usize, usize),
    seq: usize,
    reason: Option<NativeSourceReason>,
    callees: [StackCall<'s>; MAX_STACK_MACRO_CALLEES],
    callees_len: usize,
    invoked: bool,
}

impl<'s> StackMacroDef<'s> {
    const EMPTY: Self = Self {
        name: "",
        scope: 0,
        macro_export: false,
        pos: (0, 0),
        seq: 0,
        reason: None,
        callees: [StackCall::EMPTY; MAX_STACK_MACRO_CALLEES],
        callees_len: 0,
        invoked: false,
    };
}

struct StackSink<'s> {
    scopes: [StackScope<'s>; MAX_STACK_SCOPES],
    scopes_len: usize,
    defs: [StackMacroDef<'s>; MAX_STACK_MACRO_DEFS],
    defs_len: usize,
    top_calls: [StackCall<'s>; MAX_STACK_CALLS],
    top_calls_len: usize,
    top_calls_overflow: bool,
    active_def: Option<usize>,
    matching_only: bool,
    seq: usize,
}

impl<'s> StackSink<'s> {
    fn new() -> Self {
        Self {
            scopes: [StackScope::ROOT; MAX_STACK_SCOPES],
            scopes_len: 1,
            defs: [StackMacroDef::EMPTY; MAX_STACK_MACRO_DEFS],
            defs_len: 0,
            top_calls: [StackCall::EMPTY; MAX_STACK_CALLS],
            top_calls_len: 0,
            top_calls_overflow: false,
            active_def: None,
            matching_only: false,
            seq: 0,
        }
    }

    fn next_pos(&mut self) -> ((usize, usize), usize) {
        self.seq += 1;
        ((self.seq, self.seq), self.seq)
    }

    fn effective_def_scope(&self, def: &StackMacroDef<'s>) -> (usize, (usize, usize), bool) {
        if def.macro_export {
            return (0, (0, 0), true);
        }
        let mut s = def.scope;
        let mut pos = def.pos;
        let mut steps: usize = 0;
        while steps < MAX_STACK_SCOPES {
            steps += 1;
            let Some(node) = self.scopes.get(s) else { break };
            if node.is_module && node.macro_use
                && let Some(parent) = node.parent
            {
                pos = node.decl_pos;
                s = parent;
                continue;
            }
            break;
        }
        (s, pos, false)
    }

    fn enclosing_module(&self, mut scope: usize) -> usize {
        let mut steps: usize = 0;
        while steps < MAX_STACK_SCOPES {
            steps += 1;
            let Some(node) = self.scopes.get(scope) else { return 0 };
            if node.is_module {
                return scope;
            }
            match node.parent {
                Some(p) => scope = p,
                None => return 0,
            }
        }
        0
    }

    fn find_child_module(&self, parent: usize, name: &str) -> Option<usize> {
        for (idx, node) in self.scopes.get(..self.scopes_len).unwrap_or(&[]).iter().enumerate() {
            if node.is_module && node.parent == Some(parent) && node.mod_name == Some(name) {
                return Some(idx);
            }
        }
        None
    }

    fn resolve_qualified_module(&self, call: &StackCall<'s>) -> Option<usize> {
        if call.leading_global {
            return None;
        }
        let segs = call.qualifier.get(..call.qual_len).unwrap_or(&[]);
        let (&first, rest) = segs.split_first()?;
        let (mut cur, tail) = match first {
            "crate" => (0usize, rest),
            "self" => (self.enclosing_module(call.scope), rest),
            "super" => {
                let mut m = self.enclosing_module(call.scope);
                let mut idx = 0usize;
                while segs.get(idx) == Some(&"super") {
                    m = self.scopes.get(m).and_then(|n| n.parent).map(|p| self.enclosing_module(p)).unwrap_or(0);
                    idx += 1;
                }
                (m, segs.get(idx..).unwrap_or(&[]))
            }
            _ => {
                let enc = self.enclosing_module(call.scope);
                if let Some(child) = self.find_child_module(enc, first) {
                    (child, rest)
                } else if enc != 0
                    && let Some(child) = self.find_child_module(0, first)
                {
                    (child, rest)
                } else {
                    return None;
                }
            }
        };
        for &seg in tail {
            cur = self.find_child_module(cur, seg)?;
        }
        Some(cur)
    }

    fn resolve_call_target(&self, call: &StackCall<'s>) -> Option<usize> {
        if call.qual_len > 0 || call.leading_global {
            let target_mod = self.resolve_qualified_module(call)?;
            let mut best: Option<(usize, usize)> = None;
            for (idx, def) in self.defs.get(..self.defs_len).unwrap_or(&[]).iter().enumerate() {
                if def.name != call.name {
                    continue;
                }
                let (eff_scope, _, _) = self.effective_def_scope(def);
                if def.scope == target_mod || eff_scope == target_mod {
                    if best.is_none_or(|(_, seq)| def.seq >= seq) {
                        best = Some((idx, def.seq));
                    }
                }
            }
            return best.map(|(idx, _)| idx);
        }

        let mut scope = call.scope;
        let mut steps: usize = 0;
        while steps < MAX_STACK_SCOPES {
            steps += 1;
            let mut best_textual: Option<(usize, (usize, usize), usize)> = None;
            let mut best_export: Option<(usize, usize)> = None;
            for (idx, def) in self.defs.get(..self.defs_len).unwrap_or(&[]).iter().enumerate() {
                if def.name != call.name {
                    continue;
                }
                let (eff_scope, eff_pos, is_export) = self.effective_def_scope(def);
                if eff_scope != scope {
                    continue;
                }
                if is_export {
                    if best_export.is_none_or(|(_, seq)| def.seq >= seq) {
                        best_export = Some((idx, def.seq));
                    }
                } else if eff_pos <= call.pos {
                    if best_textual.is_none_or(|(_, bpos, bseq)| (eff_pos, def.seq) >= (bpos, bseq)) {
                        best_textual = Some((idx, eff_pos, def.seq));
                    }
                }
            }
            if let Some((idx, _, _)) = best_textual {
                return Some(idx);
            }
            if let Some((idx, _)) = best_export {
                return Some(idx);
            }
            match self.scopes.get(scope).and_then(|n| n.parent) {
                Some(parent) => scope = parent,
                None => break,
            }
        }
        None
    }

    fn resolve_invoked_macros(&mut self) -> Option<NativeSourceReason> {
        for idx in 0..self.top_calls_len {
            let Some(call) = self.top_calls.get(idx).copied() else { continue };
            if let Some(target) = self.resolve_call_target(&call)
                && let Some(def) = self.defs.get_mut(target)
            {
                def.invoked = true;
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.defs_len {
                let Some(def) = self.defs.get(i).copied() else { continue };
                if !def.invoked {
                    continue;
                }
                if let Some(reason) = def.reason {
                    return Some(reason);
                }
                for callee in def.callees.get(..def.callees_len).unwrap_or(&[]) {
                    if let Some(target) = self.resolve_call_target(callee)
                        && let Some(other) = self.defs.get_mut(target)
                        && !other.invoked
                    {
                        other.invoked = true;
                        changed = true;
                    }
                }
            }
        }
        None
    }
}

impl<'s> ScanSink<'s> for StackSink<'s> {
    fn enter_scope(&mut self, parent: usize, mod_name: Option<&'s str>, macro_use: bool, is_module: bool) -> usize {
        if self.matching_only {
            let (decl_pos, _) = self.next_pos();
            for (idx, node) in self.scopes.get(..self.scopes_len).unwrap_or(&[]).iter().enumerate() {
                if node.parent == Some(parent) && node.mod_name == mod_name && node.is_module == is_module && node.decl_pos == decl_pos {
                    return idx;
                }
            }
            return parent;
        }
        let (decl_pos, _) = self.next_pos();
        let id = self.scopes_len;
        let Some(slot) = self.scopes.get_mut(id) else { return parent };
        *slot = StackScope { parent: Some(parent), mod_name, macro_use, is_module, decl_pos };
        self.scopes_len += 1;
        id
    }

    fn mark_scope_macro_use(&mut self, scope: usize) {
        if let Some(node) = self.scopes.get_mut(scope) {
            node.macro_use = true;
        }
    }

    fn push_external_mod(
        &mut self,
        _is_root: bool,
        _current_rel: &str,
        _current_scope: usize,
        _inline_mods: &[Option<&str>],
        _mod_name: &'s str,
        _path_attr: Option<StrLit<'s>>,
        _macro_use: bool,
    ) {
    }

    fn push_include(&mut self, _current_rel: &str, _current_scope: usize, _inc_path: StrLit<'s>) {}

    fn push_invocation(&mut self, call: CallSite<'s>) {
        let (pos, _) = self.next_pos();
        let sc = StackCall {
            name: call.name,
            qualifier: call.qualifier,
            qual_len: call.qual_len,
            leading_global: call.leading_global,
            scope: call.scope,
            pos,
        };
        if self.matching_only {
            if let Some(target) = self.resolve_call_target(&sc)
                && let Some(def) = self.defs.get_mut(target)
            {
                def.invoked = true;
            }
            return;
        }
        if let Some(id) = self.active_def {
            if let Some(def) = self.defs.get_mut(id)
                && let Some(slot) = def.callees.get_mut(def.callees_len)
            {
                *slot = sc;
                def.callees_len += 1;
            }
            return;
        }
        if let Some(slot) = self.top_calls.get_mut(self.top_calls_len) {
            *slot = sc;
            self.top_calls_len += 1;
        } else {
            self.top_calls_overflow = true;
        }
    }

    fn enter_macro(&mut self, name: &'s str, scope: usize, macro_export: bool) -> Option<(usize, Option<usize>)> {
        let (pos, seq) = self.next_pos();
        if self.matching_only {
            return None;
        }
        let id = self.defs_len;
        let slot = self.defs.get_mut(id)?;
        *slot = StackMacroDef { name, scope, macro_export, pos, seq, ..StackMacroDef::EMPTY };
        self.defs_len += 1;
        let prev = self.active_def;
        self.active_def = Some(id);
        Some((id, prev))
    }

    fn leave_macro(&mut self, def_id: usize, prev_active: Option<usize>, reason: Option<NativeSourceReason>) {
        if let Some(def) = self.defs.get_mut(def_id) {
            def.reason = reason;
        }
        self.active_def = prev_active;
    }
}

struct TreeScope<'a> {
    parent: Option<usize>,
    mod_name: Option<AString<'a>>,
    macro_use: bool,
    is_module: bool,
    decl_pos: (usize, usize),
}

struct TreeCall<'a> {
    name: AString<'a>,
    qualifier: AVec<'a, AString<'a>>,
    leading_global: bool,
    scope: usize,
    pos: (usize, usize),
}

struct TreeMacroDef<'a> {
    name: AString<'a>,
    scope: usize,
    macro_export: bool,
    pos: (usize, usize),
    seq: usize,
    reason: Option<NativeSourceReason>,
    callees: AVec<'a, TreeCall<'a>>,
    children: AVec<'a, ChildRef<'a>>,
    resolved: bool,
}

struct TreeSink<'a> {
    alloc: Alloc<'a>,
    scopes: AVec<'a, TreeScope<'a>>,
    defs: AVec<'a, TreeMacroDef<'a>>,
    calls: AVec<'a, TreeCall<'a>>,
    top_children: AVec<'a, ChildRef<'a>>,
    active_def: Option<usize>,
    origin_order: Option<usize>,
    seq: usize,
}

impl<'a> TreeSink<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        let mut scopes = Vec::new_in(alloc);
        scopes.push(TreeScope {
            parent: None,
            mod_name: None,
            macro_use: false,
            is_module: true,
            decl_pos: (0, 0),
        });
        Self {
            alloc,
            scopes,
            defs: Vec::new_in(alloc),
            calls: Vec::new_in(alloc),
            top_children: Vec::new_in(alloc),
            active_def: None,
            origin_order: None,
            seq: 0,
        }
    }

    fn set_origin_order(&mut self, origin_order: Option<usize>) {
        self.origin_order = origin_order;
    }

    fn next_pos(&mut self) -> ((usize, usize), usize) {
        self.seq += 1;
        let pos = match self.origin_order {
            Some(base) => (base, self.seq),
            None => (self.seq, self.seq),
        };
        (pos, self.seq)
    }

    fn take_top_children(&mut self) -> AVec<'a, ChildRef<'a>> {
        core::mem::replace(&mut self.top_children, Vec::new_in(self.alloc))
    }

    fn effective_def_scope(&self, def: &TreeMacroDef<'a>) -> (usize, (usize, usize), bool) {
        if def.macro_export {
            return (0, (0, 0), true);
        }
        let mut s = def.scope;
        let mut pos = def.pos;
        let max_steps = self.scopes.len();
        let mut steps: usize = 0;
        while steps < max_steps {
            steps += 1;
            let Some(node) = self.scopes.get(s) else { break };
            if node.is_module && node.macro_use
                && let Some(parent) = node.parent
            {
                pos = node.decl_pos;
                s = parent;
                continue;
            }
            break;
        }
        (s, pos, false)
    }

    fn enclosing_module(&self, mut scope: usize) -> usize {
        let max_steps = self.scopes.len();
        let mut steps: usize = 0;
        while steps < max_steps {
            steps += 1;
            let Some(node) = self.scopes.get(scope) else { return 0 };
            if node.is_module {
                return scope;
            }
            match node.parent {
                Some(p) => scope = p,
                None => return 0,
            }
        }
        0
    }

    fn find_child_module(&self, parent: usize, name: &str) -> Option<usize> {
        for (idx, node) in self.scopes.iter().enumerate() {
            if node.is_module
                && node.parent == Some(parent)
                && node.mod_name.as_ref().is_some_and(|m| m.as_str() == name)
            {
                return Some(idx);
            }
        }
        None
    }

    fn resolve_qualified_module(&self, call: &TreeCall<'a>) -> Option<usize> {
        if call.leading_global {
            return None;
        }
        let first = call.qualifier.first()?.as_str();
        let (mut cur, start_idx) = match first {
            "crate" => (0usize, 1usize),
            "self" => (self.enclosing_module(call.scope), 1usize),
            "super" => {
                let mut m = self.enclosing_module(call.scope);
                let mut idx = 0usize;
                while call.qualifier.get(idx).is_some_and(|s| s.as_str() == "super") {
                    m = self.scopes.get(m).and_then(|n| n.parent).map(|p| self.enclosing_module(p)).unwrap_or(0);
                    idx += 1;
                }
                (m, idx)
            }
            _ => {
                let enc = self.enclosing_module(call.scope);
                if let Some(child) = self.find_child_module(enc, first) {
                    (child, 1usize)
                } else if enc != 0
                    && let Some(child) = self.find_child_module(0, first)
                {
                    (child, 1usize)
                } else {
                    return None;
                }
            }
        };
        for seg in call.qualifier.get(start_idx..).unwrap_or(&[]) {
            cur = self.find_child_module(cur, seg.as_str())?;
        }
        Some(cur)
    }

    fn resolve_call_target(&self, call: &TreeCall<'a>) -> Option<usize> {
        if !call.qualifier.is_empty() || call.leading_global {
            let target_mod = self.resolve_qualified_module(call)?;
            let mut best: Option<(usize, usize)> = None;
            for (idx, def) in self.defs.iter().enumerate() {
                if def.name.as_str() != call.name.as_str() {
                    continue;
                }
                let (eff_scope, _, _) = self.effective_def_scope(def);
                if def.scope == target_mod || eff_scope == target_mod {
                    if best.is_none_or(|(_, seq)| def.seq >= seq) {
                        best = Some((idx, def.seq));
                    }
                }
            }
            return best.map(|(idx, _)| idx);
        }

        let mut scope = call.scope;
        let max_steps = self.scopes.len();
        let mut steps: usize = 0;
        while steps < max_steps {
            steps += 1;
            let mut best_textual: Option<(usize, (usize, usize), usize)> = None;
            let mut best_export: Option<(usize, usize)> = None;
            for (idx, def) in self.defs.iter().enumerate() {
                if def.name.as_str() != call.name.as_str() {
                    continue;
                }
                let (eff_scope, eff_pos, is_export) = self.effective_def_scope(def);
                if eff_scope != scope {
                    continue;
                }
                if is_export {
                    if best_export.is_none_or(|(_, seq)| def.seq >= seq) {
                        best_export = Some((idx, def.seq));
                    }
                } else if eff_pos <= call.pos {
                    if best_textual.is_none_or(|(_, bpos, bseq)| (eff_pos, def.seq) >= (bpos, bseq)) {
                        best_textual = Some((idx, eff_pos, def.seq));
                    }
                }
            }
            if let Some((idx, _, _)) = best_textual {
                return Some(idx);
            }
            if let Some((idx, _)) = best_export {
                return Some(idx);
            }
            match self.scopes.get(scope).and_then(|n| n.parent) {
                Some(parent) => scope = parent,
                None => break,
            }
        }
        None
    }

    fn propagate_invoked_macros(&mut self) -> Option<NativeSourceReason> {
        let mut changed = true;
        while changed {
            changed = false;
            let call_count = self.calls.len();
            for ci in 0..call_count {
                let target = {
                    let Some(call) = self.calls.get(ci) else { continue };
                    self.resolve_call_target(call)
                };
                let Some(def_idx) = target else { continue };
                let already_resolved = self.defs.get(def_idx).is_none_or(|d| d.resolved);
                if already_resolved {
                    continue;
                }
                let Some(def) = self.defs.get_mut(def_idx) else { continue };
                def.resolved = true;
                changed = true;
                if let Some(reason) = def.reason {
                    return Some(reason);
                }
                let callees = core::mem::replace(&mut def.callees, Vec::new_in(self.alloc));
                let children = core::mem::replace(&mut def.children, Vec::new_in(self.alloc));
                for callee in callees {
                    self.calls.push(callee);
                }
                for child in children {
                    self.top_children.push(child);
                }
            }
        }
        None
    }
}

impl<'s, 'a> ScanSink<'s> for TreeSink<'a> {
    fn enter_scope(&mut self, parent: usize, mod_name: Option<&'s str>, macro_use: bool, is_module: bool) -> usize {
        let (decl_pos, _) = self.next_pos();
        let id = self.scopes.len();
        self.scopes.push(TreeScope {
            parent: Some(parent),
            mod_name: mod_name.map(|m| String::from_str_in(m, self.alloc)),
            macro_use,
            is_module,
            decl_pos,
        });
        id
    }

    fn mark_scope_macro_use(&mut self, scope: usize) {
        if let Some(node) = self.scopes.get_mut(scope) {
            node.macro_use = true;
        }
    }

    fn push_external_mod(
        &mut self,
        is_root: bool,
        current_rel: &str,
        current_scope: usize,
        inline_mods: &[Option<&str>],
        mod_name: &'s str,
        path_attr: Option<StrLit<'s>>,
        macro_use: bool,
    ) {
        let child_scope = self.enter_scope(current_scope, Some(mod_name), macro_use, true);
        let decoded_path = match path_attr {
            Some(lit) => match decode_str_lit(lit, self.alloc) {
                Some(s) => Some(s),
                None => return,
            },
            None => None,
        };
        let out = match self.active_def.and_then(|id| self.defs.get_mut(id)) {
            Some(def) => &mut def.children,
            None => &mut self.top_children,
        };
        push_external_mod_ref(
            out,
            is_root,
            current_rel,
            child_scope,
            inline_mods,
            mod_name,
            decoded_path.as_ref().map(|s| s.as_str()),
            self.alloc,
        );
    }

    fn push_include(&mut self, current_rel: &str, current_scope: usize, inc_path: StrLit<'s>) {
        let (pos, _) = self.next_pos();
        let Some(decoded) = decode_str_lit(inc_path, self.alloc) else { return };
        if let Some(resolved) = normalize_rel_path(dir_of(current_rel), decoded.as_str(), self.alloc) {
            let out = match self.active_def.and_then(|id| self.defs.get_mut(id)) {
                Some(def) => &mut def.children,
                None => &mut self.top_children,
            };
            out.push(ChildRef::Exact { path: resolved, scope: current_scope, origin_order: Some(pos.0) });
        }
    }

    fn push_invocation(&mut self, call: CallSite<'s>) {
        let (pos, _) = self.next_pos();
        let mut qual = Vec::with_capacity_in(call.qual_len, self.alloc);
        for &seg in call.qualifier.get(..call.qual_len).unwrap_or(&[]) {
            qual.push(String::from_str_in(seg, self.alloc));
        }
        let tc = TreeCall {
            name: String::from_str_in(call.name, self.alloc),
            qualifier: qual,
            leading_global: call.leading_global,
            scope: call.scope,
            pos,
        };
        if let Some(id) = self.active_def {
            if let Some(def) = self.defs.get_mut(id) {
                def.callees.push(tc);
            }
            return;
        }
        self.calls.push(tc);
    }

    fn enter_macro(&mut self, name: &'s str, scope: usize, macro_export: bool) -> Option<(usize, Option<usize>)> {
        let (pos, seq) = self.next_pos();
        let id = self.defs.len();
        self.defs.push(TreeMacroDef {
            name: String::from_str_in(name, self.alloc),
            scope,
            macro_export,
            pos,
            seq,
            reason: None,
            callees: Vec::new_in(self.alloc),
            children: Vec::new_in(self.alloc),
            resolved: false,
        });
        let prev = self.active_def;
        self.active_def = Some(id);
        Some((id, prev))
    }

    fn leave_macro(&mut self, def_id: usize, prev_active: Option<usize>, reason: Option<NativeSourceReason>) {
        if let Some(def) = self.defs.get_mut(def_id) {
            def.reason = reason;
        }
        self.active_def = prev_active;
    }
}

fn dir_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

fn normalize_rel_path<'a>(base_dir: &str, rel: &str, alloc: Alloc<'a>) -> Option<AString<'a>> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let mut segs: AVec<'_, &str> = Vec::new_in(alloc);
    for seg in base_dir.split('/') {
        if !seg.is_empty() && seg != "." {
            segs.push(seg);
        }
    }
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop()?;
            }
            part => segs.push(part),
        }
    }
    if segs.is_empty() {
        return None;
    }
    let mut out = String::new_in(alloc);
    for (idx, seg) in segs.iter().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        out.push_str(seg);
    }
    Some(out)
}

fn scan_source_no_alloc(source: &str, cfg: ScriptCfg<'_>) -> Option<NativeSourceReason> {
    let mut sink = StackSink::new();
    if let Some(reason) = scan_source_core(source, true, "", 0, cfg, 0, &mut sink) {
        return Some(reason);
    }
    if sink.defs_len == 0 {
        return None;
    }
    if sink.top_calls_overflow {
        sink.matching_only = true;
        sink.active_def = None;
        sink.seq = 0;
        let _ = scan_source_core(source, true, "", 0, cfg, 0, &mut sink);
    }
    sink.resolve_invoked_macros()
}

fn scan_macro_rules_transcribers<'s, S: ScanSink<'s>>(
    body_src: &'s str,
    current_rel: &str,
    def_scope: usize,
    cfg: ScriptCfg<'_>,
    macro_depth: usize,
    sink: &mut S,
) -> Option<NativeSourceReason> {
    let mut rule_lex = Lexer::new(body_src);
    let mut def_reason: Option<NativeSourceReason> = None;
    while let Some(tok) = rule_lex.next_token() {
        if tok == Token::Semi {
            continue;
        }
        let Some(m_close) = closing_delim(tok) else {
            break;
        };
        skip_delimited(&mut rule_lex, tok, m_close);
        if rule_lex.next_token() != Some(Token::FatArrow) {
            break;
        }
        let Some(t_open) = rule_lex.next_token() else { break };
        let Some(t_close) = closing_delim(t_open) else { break };
        let t_start = rule_lex.at;
        let t_end = skip_delimited_end(&mut rule_lex, t_open, t_close);
        if let Some(transcriber) = body_src.get(t_start..t_end)
            && let Some(reason) = scan_source_core(transcriber, false, current_rel, def_scope, cfg, macro_depth, sink)
        {
            def_reason = Some(reason);
            break;
        }
    }
    def_reason
}

struct PendingInlineMod<'s> {
    ident: &'s str,
    dir_name: &'s str,
    macro_use: bool,
}

fn scan_source_core<'s, S: ScanSink<'s>>(
    source: &'s str,
    is_root: bool,
    current_rel: &str,
    initial_scope: usize,
    cfg: ScriptCfg<'_>,
    macro_depth: usize,
    sink: &mut S,
) -> Option<NativeSourceReason> {
    let mut lex = Lexer::new(source);
    let mut brace_stack = [false; 64];
    let mut mod_stack: [Option<&str>; 64] = [None; 64];
    let mut scope_stack: [usize; 64] = [initial_scope; 64];
    let mut brace_len: usize = 0;
    let mut brace_overflow: usize = 0;
    let mut current_scope: usize = initial_scope;
    let mut excluded_depth: usize = 0;
    let mut paren_depth: usize = 0;
    let mut pending_excluded = false;
    let mut pending_depth: usize = 0;
    let mut stmt_needs_semi = false;
    let mut pending_path_attr: Option<StrLit<'s>> = None;
    let mut pending_path_depth: usize = 0;
    let mut pending_macro_use = false;
    let mut pending_macro_use_depth: usize = 0;
    let mut pending_macro_export = false;
    let mut pending_macro_export_depth: usize = 0;
    let mut pending_inline_mod: Option<PendingInlineMod<'s>> = None;
    let mut prev: Option<Token<'s>> = None;
    let mut prev_prev: Option<Token<'s>> = None;
    let mut path_segs: [&'s str; MAX_QUAL_SEGS] = [""; MAX_QUAL_SEGS];
    let mut path_len: usize = 0;
    let mut path_overflow = false;
    let mut path_expect_ident = false;
    let mut path_leading_global = false;

    while let Some(tok) = lex.next_token() {
        let brace_depth = brace_len + brace_overflow;

        if tok == Token::Pound {
            let mut look = lex;
            let inner = match look.next_token() {
                Some(Token::Bang) => {
                    if look.next_token() == Some(Token::LBracket) {
                        true
                    } else {
                        prev_prev = prev;
                        prev = Some(tok);
                        path_len = 0;
                        path_overflow = false;
                        path_expect_ident = false;
                        path_leading_global = false;
                        continue;
                    }
                }
                Some(Token::LBracket) => false,
                _ => {
                    prev_prev = prev;
                    prev = Some(tok);
                    path_len = 0;
                    path_overflow = false;
                    path_expect_ident = false;
                    path_leading_global = false;
                    continue;
                }
            };
            lex = look;
            let attr = parse_attr(&mut lex, cfg);
            if let Some(attr_cfg) = attr.cfg {
                if inner {
                    if brace_depth == 0 {
                        // Crate-level `#![cfg(...)]` (when `is_root`): if false on WASI but not
                        // known-false on the concrete host, compiling on WASI strips `main()` and
                        // fails -> must run native. In a submodule (`!is_root`), a top-level
                        // `#![cfg(...)]` false on WASI disables only that submodule.
                        if attr_cfg.wasi == TriBool::False {
                            if !is_root {
                                return None;
                            }
                            return if attr_cfg.host != TriBool::False {
                                Some(NativeSourceReason::HostFamilyCfg)
                            } else {
                                None
                            };
                        }
                    } else if attr_cfg.wasi == TriBool::False
                        && brace_overflow == 0
                        && let Some(slot) = brace_len.checked_sub(1).and_then(|idx| brace_stack.get_mut(idx))
                        && !*slot
                    {
                        *slot = true;
                        excluded_depth += 1;
                    }
                } else if attr_cfg.wasi == TriBool::False && excluded_depth == 0 {
                    pending_excluded = true;
                    pending_depth = brace_depth;
                }
            }
            if !inner && let Some(path_val) = attr.path {
                pending_path_attr = Some(path_val);
                pending_path_depth = brace_depth;
            }
            if attr.macro_use {
                if inner {
                    if excluded_depth == 0 {
                        sink.mark_scope_macro_use(current_scope);
                    }
                } else {
                    pending_macro_use = true;
                    pending_macro_use_depth = brace_depth;
                }
            }
            if !inner && attr.macro_export {
                pending_macro_export = true;
                pending_macro_export_depth = brace_depth;
            }
            prev_prev = prev;
            prev = Some(Token::RBracket);
            path_len = 0;
            path_overflow = false;
            path_expect_ident = false;
            path_leading_global = false;
            continue;
        }

        // Handle `macro_rules! name { ... }` / `(...)` / `[...]`: never treat unexpanded tokens as
        // directly compiled code, but record active definitions in their lexical scope so macros
        // that ARE invoked in scope still propagate their native requirements and child files.
        if tok == Token::Ident("macro_rules") {
            let mut look = lex;
            if look.next_token() == Some(Token::Bang)
                && let Some(Token::Ident(macro_name)) = look.next_token()
                && let Some(open) = look.next_token()
                && let Some(close) = closing_delim(open)
            {
                let body_start = look.at;
                let body_end = skip_delimited_end(&mut look, open, close);
                if open != Token::LBrace {
                    let mut after = look;
                    if after.next_token() == Some(Token::Semi) {
                        look = after;
                    }
                }
                let is_excluded = excluded_depth > 0 || pending_excluded;
                let is_export = pending_macro_export && pending_macro_export_depth == brace_depth;
                if !is_excluded
                    && macro_depth < MAX_MACRO_DEPTH
                    && let Some(body_src) = lex.src.get(body_start..body_end)
                    && let Some((def_id, prev_active)) = sink.enter_macro(macro_name, current_scope, is_export)
                {
                    let def_reason = scan_macro_rules_transcribers(
                        body_src,
                        current_rel,
                        current_scope,
                        cfg,
                        macro_depth + 1,
                        sink,
                    );
                    sink.leave_macro(def_id, prev_active, def_reason);
                }
                lex = look;
                if pending_excluded && brace_depth == pending_depth && !stmt_needs_semi {
                    pending_excluded = false;
                }
                pending_path_attr = None;
                pending_macro_use = false;
                pending_macro_export = false;
                prev_prev = prev;
                prev = Some(close);
                path_len = 0;
                path_overflow = false;
                path_expect_ident = false;
                path_leading_global = false;
                continue;
            }
        }

        let is_excluded = excluded_depth > 0 || pending_excluded;

        if pending_excluded && brace_depth == pending_depth && paren_depth == 0 {
            match tok {
                Token::Ident("let" | "use" | "static" | "type" | "const") => {
                    stmt_needs_semi = true;
                }
                Token::Ident("fn") => {
                    stmt_needs_semi = false;
                }
                Token::Semi => {
                    pending_excluded = false;
                    stmt_needs_semi = false;
                }
                _ => {}
            }
        }

        match tok {
            Token::LParen | Token::LBracket => {
                paren_depth += 1;
            }
            Token::RParen | Token::RBracket => {
                paren_depth = paren_depth.saturating_sub(1);
            }
            Token::LBrace => {
                let this_excluded = pending_excluded && brace_depth == pending_depth;
                let inline_mod = pending_inline_mod.take();
                let next_scope = if !this_excluded && excluded_depth == 0 {
                    match &inline_mod {
                        Some(m) => sink.enter_scope(current_scope, Some(m.ident), m.macro_use, true),
                        None => sink.enter_scope(current_scope, None, false, false),
                    }
                } else {
                    current_scope
                };
                if let Some(slot) = brace_stack.get_mut(brace_len) {
                    *slot = this_excluded;
                    if let Some(mod_slot) = mod_stack.get_mut(brace_len) {
                        *mod_slot = inline_mod.map(|m| m.dir_name);
                    }
                    if let Some(scope_slot) = scope_stack.get_mut(brace_len) {
                        *scope_slot = current_scope;
                    }
                    current_scope = next_scope;
                    brace_len += 1;
                } else {
                    brace_overflow += 1;
                }
                if this_excluded {
                    excluded_depth += 1;
                }
                if pending_path_depth == brace_depth {
                    pending_path_attr = None;
                }
                if pending_macro_use_depth == brace_depth {
                    pending_macro_use = false;
                }
                if pending_macro_export_depth == brace_depth {
                    pending_macro_export = false;
                }
            }
            Token::RBrace => {
                if brace_overflow > 0 {
                    brace_overflow -= 1;
                } else if let Some(new_len) = brace_len.checked_sub(1) {
                    brace_len = new_len;
                    if brace_stack.get(brace_len).copied().unwrap_or(false) {
                        excluded_depth = excluded_depth.saturating_sub(1);
                    }
                    if let Some(mod_slot) = mod_stack.get_mut(brace_len) {
                        *mod_slot = None;
                    }
                    if let Some(&saved_scope) = scope_stack.get(brace_len) {
                        current_scope = saved_scope;
                    }
                }
                let new_depth = brace_len + brace_overflow;
                if pending_excluded {
                    if new_depth < pending_depth {
                        pending_excluded = false;
                        stmt_needs_semi = false;
                    } else if new_depth == pending_depth && !stmt_needs_semi {
                        let mut look = lex;
                        if look.next_token() != Some(Token::Ident("else")) {
                            pending_excluded = false;
                        }
                    }
                }
                if new_depth <= pending_path_depth {
                    pending_path_attr = None;
                }
                if new_depth <= pending_macro_use_depth {
                    pending_macro_use = false;
                }
                if new_depth <= pending_macro_export_depth {
                    pending_macro_export = false;
                }
            }
            Token::Semi => {
                if brace_depth == pending_path_depth {
                    pending_path_attr = None;
                }
                if brace_depth == pending_macro_use_depth {
                    pending_macro_use = false;
                }
                if brace_depth == pending_macro_export_depth {
                    pending_macro_export = false;
                }
                pending_inline_mod = None;
            }
            _ => {}
        }

        if !is_excluded {
            match tok {
                Token::Ident("std") if is_root_std_path(prev, prev_prev) => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::ColonColon) {
                        match look.next_token() {
                            Some(Token::Ident("os")) if look.next_token() == Some(Token::ColonColon) => {
                                match look.next_token() {
                                    Some(Token::Ident("unix" | "linux" | "macos")) => {
                                        return Some(NativeSourceReason::HostOsStd);
                                    }
                                    Some(Token::LBrace) if os_group_mentions_host_os(&mut look, 0) => {
                                        return Some(NativeSourceReason::HostOsStd);
                                    }
                                    _ => {}
                                }
                            }
                            Some(Token::LBrace) if std_group_mentions_host_os(&mut look, 0) => {
                                return Some(NativeSourceReason::HostOsStd);
                            }
                            _ => {}
                        }
                    }
                }
                Token::Ident("extern") => {
                    let mut look = lex;
                    match look.next_token() {
                        Some(Token::LBrace) => return Some(NativeSourceReason::ExternC),
                        Some(Token::Str(_) | Token::RawStr(_)) if look.next_token() == Some(Token::LBrace) => {
                            return Some(NativeSourceReason::ExternC);
                        }
                        _ => {}
                    }
                }
                Token::Ident("cfg") => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::Bang)
                        && let Some(open) = look.next_token()
                        && let Some(close) = closing_delim(open)
                    {
                        let mut body = look;
                        let ev = eval_cfg_pair(&mut body, cfg, 0);
                        if ev.diverges_host_vs_wasi() {
                            return Some(NativeSourceReason::HostFamilyCfg);
                        }
                        skip_delimited(&mut look, open, close);
                        lex = look;
                        prev_prev = prev;
                        prev = Some(close);
                        path_len = 0;
                        path_overflow = false;
                        path_expect_ident = false;
                        path_leading_global = false;
                        continue;
                    }
                }
                Token::Ident("mod") => {
                    let mut look = lex;
                    if let Some(Token::Ident(mod_name)) = look.next_token() {
                        let attr_path = if pending_path_depth == brace_depth {
                            pending_path_attr.take()
                        } else {
                            None
                        };
                        let is_macro_use = if pending_macro_use_depth == brace_depth {
                            let v = pending_macro_use;
                            pending_macro_use = false;
                            v
                        } else {
                            false
                        };
                        match look.next_token() {
                            Some(Token::Semi) => {
                                sink.push_external_mod(
                                    is_root,
                                    current_rel,
                                    current_scope,
                                    mod_stack.get(..brace_len).unwrap_or(&[]),
                                    mod_name,
                                    attr_path,
                                    is_macro_use,
                                );
                            }
                            Some(Token::LBrace) => {
                                let dir_name = match attr_path {
                                    Some(StrLit::Raw(s)) => s,
                                    Some(StrLit::Cooked(s)) if !s.as_bytes().contains(&b'\\') => s,
                                    _ => mod_name,
                                };
                                pending_inline_mod = Some(PendingInlineMod {
                                    ident: mod_name,
                                    dir_name,
                                    macro_use: is_macro_use,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Token::Ident("include") => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::Bang)
                        && let Some(open) = look.next_token()
                        && let Some(close) = closing_delim(open)
                    {
                        let mut body = look;
                        if let Some(inc_lit) = str_literal(body.next_token())
                            && body.next_token() == Some(close)
                        {
                            sink.push_include(current_rel, current_scope, inc_lit);
                        }
                        skip_delimited(&mut look, open, close);
                        lex = look;
                        prev_prev = prev;
                        prev = Some(close);
                        path_len = 0;
                        path_overflow = false;
                        path_expect_ident = false;
                        path_leading_global = false;
                        continue;
                    }
                }
                Token::Ident(name) if !is_rust_keyword(name) => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::Bang)
                        && let Some(open) = look.next_token()
                        && closing_delim(open).is_some()
                    {
                        let (qualifier, qual_len, leading_global) = if path_expect_ident {
                            if path_overflow {
                                ([""; MAX_QUAL_SEGS], 0, true)
                            } else {
                                (path_segs, path_len, path_leading_global)
                            }
                        } else {
                            ([""; MAX_QUAL_SEGS], 0, false)
                        };
                        sink.push_invocation(CallSite {
                            name,
                            qualifier,
                            qual_len,
                            leading_global,
                            scope: current_scope,
                        });
                    }
                }
                _ => {}
            }
        } else if tok == Token::Ident("mod") {
            let mut look = lex;
            if let Some(Token::Ident(mod_name)) = look.next_token()
                && look.next_token() == Some(Token::LBrace)
            {
                pending_inline_mod = Some(PendingInlineMod {
                    ident: mod_name,
                    dir_name: mod_name,
                    macro_use: false,
                });
            }
        }

        match tok {
            Token::Ident(name) => {
                if path_expect_ident {
                    if let Some(slot) = path_segs.get_mut(path_len) {
                        *slot = name;
                        path_len += 1;
                    } else {
                        path_overflow = true;
                    }
                } else {
                    if let Some(slot) = path_segs.get_mut(0) {
                        *slot = name;
                    }
                    path_len = 1;
                    path_overflow = false;
                    path_leading_global = false;
                }
                path_expect_ident = false;
            }
            Token::ColonColon => {
                if !matches!(prev, Some(Token::Ident(_))) {
                    path_len = 0;
                    path_overflow = false;
                    path_leading_global = true;
                }
                path_expect_ident = true;
            }
            _ => {
                path_len = 0;
                path_overflow = false;
                path_expect_ident = false;
                path_leading_global = false;
            }
        }

        prev_prev = prev;
        prev = Some(tok);
    }

    None
}

#[allow(clippy::too_many_arguments)]
fn push_external_mod_ref<'a>(
    out: &mut AVec<'a, ChildRef<'a>>,
    is_root: bool,
    current_rel: &str,
    child_scope: usize,
    inline_mods: &[Option<&str>],
    mod_name: &str,
    path_attr: Option<&str>,
    alloc: Alloc<'a>,
) {
    // Base directory for child modules following Rust's standard module-file rules:
    // crate root (`build.rs`, `build/main.rs`) and `mod.rs` resolve relative to their directory;
    // any other module file (`foo.rs`) resolves relative to `foo/`.
    let file_base = if is_root || current_rel == "mod.rs" || current_rel.ends_with("/mod.rs") {
        dir_of(current_rel)
    } else {
        current_rel.strip_suffix(".rs").unwrap_or(current_rel)
    };
    let mut base = String::from_str_in(file_base, alloc);
    for seg in inline_mods.iter().flatten() {
        if !base.is_empty() {
            base.push('/');
        }
        base.push_str(seg);
    }
    if let Some(explicit) = path_attr {
        let explicit_base = if inline_mods.iter().any(|m| m.is_some()) {
            base.as_str()
        } else {
            dir_of(current_rel)
        };
        if let Some(resolved) = normalize_rel_path(explicit_base, explicit, alloc) {
            out.push(ChildRef::Exact { path: resolved, scope: child_scope, origin_order: None });
        }
        return;
    }
    let mut primary_rel = String::with_capacity_in(mod_name.len() + 3, alloc);
    primary_rel.push_str(mod_name);
    primary_rel.push_str(".rs");
    let mut fallback_rel = String::with_capacity_in(mod_name.len() + 7, alloc);
    fallback_rel.push_str(mod_name);
    fallback_rel.push_str("/mod.rs");
    if let (Some(primary), Some(fallback)) = (
        normalize_rel_path(base.as_str(), primary_rel.as_str(), alloc),
        normalize_rel_path(base.as_str(), fallback_rel.as_str(), alloc),
    ) {
        out.push(ChildRef::Module { primary, fallback, scope: child_scope });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Ident(&'a str),
    Str(&'a str),
    RawStr(&'a str),
    ColonColon,
    FatArrow,
    Pound,
    Bang,
    Eq,
    Comma,
    Semi,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Other,
}

fn str_literal<'a>(tok: Option<Token<'a>>) -> Option<StrLit<'a>> {
    match tok {
        Some(Token::Str(s)) => Some(StrLit::Cooked(s)),
        Some(Token::RawStr(s)) => Some(StrLit::Raw(s)),
        _ => None,
    }
}

enum Escaped {
    Char(char),
    Skip,
}

fn hex_val(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
        _ => None,
    }
}

fn parse_rust_escape(src: &str, at: &mut usize) -> Option<Escaped> {
    let bytes = src.as_bytes();
    let b = *bytes.get(*at)?;
    *at += 1;
    match b {
        b'n' => Some(Escaped::Char('\n')),
        b'r' => Some(Escaped::Char('\r')),
        b't' => Some(Escaped::Char('\t')),
        b'\\' => Some(Escaped::Char('\\')),
        b'0' => Some(Escaped::Char('\0')),
        b'\'' => Some(Escaped::Char('\'')),
        b'"' => Some(Escaped::Char('"')),
        b'x' => {
            let hi = hex_val(*bytes.get(*at)?)?;
            let lo = hex_val(*bytes.get(*at + 1)?)?;
            *at += 2;
            let code = (hi << 4) | lo;
            if code > 0x7f {
                return None;
            }
            char::from_u32(code).map(Escaped::Char)
        }
        b'u' => {
            if *bytes.get(*at)? != b'{' {
                return None;
            }
            *at += 1;
            let mut code: u32 = 0;
            let mut digits: usize = 0;
            while let Some(&ch) = bytes.get(*at) {
                *at += 1;
                if ch == b'}' {
                    if digits == 0 {
                        return None;
                    }
                    return char::from_u32(code).map(Escaped::Char);
                }
                if ch == b'_' {
                    if digits == 0 {
                        return None;
                    }
                    continue;
                }
                if digits >= 6 {
                    return None;
                }
                let d = hex_val(ch)?;
                code = (code << 4) | d;
                digits += 1;
            }
            None
        }
        b'\n' => {
            while let Some(&ws) = bytes.get(*at) {
                if matches!(ws, b' ' | b'\t' | b'\n' | b'\r') {
                    *at += 1;
                } else {
                    break;
                }
            }
            Some(Escaped::Skip)
        }
        b'\r' if bytes.get(*at) == Some(&b'\n') => {
            *at += 1;
            while let Some(&ws) = bytes.get(*at) {
                if matches!(ws, b' ' | b'\t' | b'\n' | b'\r') {
                    *at += 1;
                } else {
                    break;
                }
            }
            Some(Escaped::Skip)
        }
        _ => None,
    }
}

fn decode_str_lit<'a>(lit: StrLit<'_>, alloc: Alloc<'a>) -> Option<AString<'a>> {
    match lit {
        StrLit::Raw(s) => Some(String::from_str_in(s, alloc)),
        StrLit::Cooked(s) => {
            if !s.as_bytes().contains(&b'\\') {
                return Some(String::from_str_in(s, alloc));
            }
            let mut out = String::with_capacity_in(s.len(), alloc);
            let mut at: usize = 0;
            while let Some(rest) = s.get(at..) {
                if rest.is_empty() {
                    break;
                }
                if let Some(bs) = rest.find('\\') {
                    if let Some(prefix) = rest.get(..bs) {
                        out.push_str(prefix);
                    }
                    at += bs + 1;
                    match parse_rust_escape(s, &mut at)? {
                        Escaped::Char(ch) => out.push(ch),
                        Escaped::Skip => {}
                    }
                } else {
                    out.push_str(rest);
                    break;
                }
            }
            Some(out)
        }
    }
}

fn decode_str_lit_stack<'a>(lit: StrLit<'a>, buf: &'a mut [u8; 128]) -> Option<&'a str> {
    match lit {
        StrLit::Raw(s) => Some(s),
        StrLit::Cooked(s) => {
            if !s.as_bytes().contains(&b'\\') {
                return Some(s);
            }
            let mut len: usize = 0;
            let mut at: usize = 0;
            while let Some(rest) = s.get(at..) {
                if rest.is_empty() {
                    break;
                }
                if let Some(bs) = rest.find('\\') {
                    let prefix = rest.get(..bs)?.as_bytes();
                    let dest = buf.get_mut(len..len.checked_add(prefix.len())?)?;
                    dest.copy_from_slice(prefix);
                    len += prefix.len();
                    at += bs + 1;
                    match parse_rust_escape(s, &mut at)? {
                        Escaped::Char(ch) => {
                            let mut enc = [0u8; 4];
                            let bytes = ch.encode_utf8(&mut enc).as_bytes();
                            let dest = buf.get_mut(len..len.checked_add(bytes.len())?)?;
                            dest.copy_from_slice(bytes);
                            len += bytes.len();
                        }
                        Escaped::Skip => {}
                    }
                } else {
                    let tail = rest.as_bytes();
                    let dest = buf.get_mut(len..len.checked_add(tail.len())?)?;
                    dest.copy_from_slice(tail);
                    len += tail.len();
                    break;
                }
            }
            core::str::from_utf8(buf.get(..len)?).ok()
        }
    }
}

fn is_rust_keyword(word: &str) -> bool {
    matches!(
        word,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "yield"
    )
}

#[derive(Clone, Copy)]
struct Lexer<'a> {
    src: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, at: 0 }
    }

    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.src.as_bytes().get(self.at.checked_add(offset)?).copied()
    }

    fn rest(&self) -> &'a str {
        self.src.get(self.at..).unwrap_or("")
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(b) = self.peek_byte(0) {
                if b.is_ascii_whitespace() {
                    self.at += 1;
                } else if b >= 0x80 {
                    let Some(ch) = self.rest().chars().next() else { break };
                    if ch.is_whitespace() {
                        self.at += ch.len_utf8();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            match (self.peek_byte(0), self.peek_byte(1)) {
                (Some(b'/'), Some(b'/')) => {
                    self.at += 2;
                    while let Some(b) = self.peek_byte(0) {
                        self.at += 1;
                        if b == b'\n' {
                            break;
                        }
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    self.at += 2;
                    let mut depth: usize = 1;
                    while depth > 0 {
                        match (self.peek_byte(0), self.peek_byte(1)) {
                            (Some(b'/'), Some(b'*')) => {
                                self.at += 2;
                                depth += 1;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                self.at += 2;
                                depth -= 1;
                            }
                            (Some(_), _) => {
                                self.at += 1;
                            }
                            (None, _) => break,
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn try_scan_raw_string(&mut self, prefix_len: usize) -> Option<&'a str> {
        let mut off = prefix_len;
        let mut hashes: usize = 0;
        while self.peek_byte(off) == Some(b'#') {
            hashes += 1;
            off += 1;
        }
        if self.peek_byte(off) != Some(b'"') {
            return None;
        }
        self.at += off + 1;
        let start = self.at;
        let mut end = self.at;
        while let Some(b) = self.peek_byte(0) {
            if b == b'"' {
                let mut matched = 0;
                while matched < hashes && self.peek_byte(1 + matched) == Some(b'#') {
                    matched += 1;
                }
                if matched == hashes {
                    end = self.at;
                    self.at += 1 + hashes;
                    break;
                }
            }
            self.at += 1;
        }
        Some(self.src.get(start..end).unwrap_or(""))
    }

    fn scan_quoted_string(&mut self) -> &'a str {
        self.at += 1;
        let start = self.at;
        let mut end = self.at;
        while let Some(b) = self.peek_byte(0) {
            if b == b'\\' {
                self.at += 1;
                if self.peek_byte(0).is_some() {
                    self.at += 1;
                }
            } else if b == b'"' {
                end = self.at;
                self.at += 1;
                break;
            } else {
                self.at += 1;
            }
        }
        self.src.get(start..end).unwrap_or("")
    }

    fn scan_char_or_lifetime(&mut self) {
        self.at += 1;
        if self.peek_byte(0) == Some(b'\\') {
            self.at += 1;
            if self.peek_byte(0).is_some() {
                self.at += 1;
            }
            while let Some(b) = self.peek_byte(0) {
                self.at += 1;
                if b == b'\'' || b == b'\n' {
                    break;
                }
            }
            return;
        }
        if let Some(ch) = self.rest().chars().next() {
            let after = ch.len_utf8();
            if self.peek_byte(after) == Some(b'\'') {
                self.at += after + 1;
                return;
            }
            if ch == 'r' && self.peek_byte(1) == Some(b'#') {
                self.at += 2;
            }
            self.skip_ident_tail();
        }
    }

    fn scan_byte_char_literal(&mut self) {
        self.at += 2;
        while let Some(b) = self.peek_byte(0) {
            if b == b'\\' {
                self.at += 1;
                if self.peek_byte(0).is_some() {
                    self.at += 1;
                }
            } else {
                self.at += 1;
                if b == b'\'' || b == b'\n' {
                    break;
                }
            }
        }
    }

    fn skip_ident_tail(&mut self) {
        while let Some(b) = self.peek_byte(0) {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.at += 1;
            } else if b >= 0x80 {
                let Some(ch) = self.rest().chars().next() else { break };
                if ch.is_alphanumeric() || ch == '_' {
                    self.at += ch.len_utf8();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        self.skip_ws_and_comments();
        let b = self.peek_byte(0)?;
        match b {
            b'"' => Some(Token::Str(self.scan_quoted_string())),
            b'\'' => {
                self.scan_char_or_lifetime();
                Some(Token::Other)
            }
            b'r' => {
                if let Some(raw) = self.try_scan_raw_string(1) {
                    return Some(Token::RawStr(raw));
                }
                if self.peek_byte(1) == Some(b'#') {
                    self.at += 2;
                    self.skip_ident_tail();
                    return Some(Token::Other);
                }
                let start = self.at;
                self.at += 1;
                self.skip_ident_tail();
                Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")))
            }
            b'b' | b'c' => {
                if self.peek_byte(1) == Some(b'"') {
                    self.at += 1;
                    let _ = self.scan_quoted_string();
                    return Some(Token::Other);
                }
                if b == b'b' && self.peek_byte(1) == Some(b'\'') {
                    self.scan_byte_char_literal();
                    return Some(Token::Other);
                }
                if self.peek_byte(1) == Some(b'r') && self.try_scan_raw_string(2).is_some() {
                    return Some(Token::Other);
                }
                let start = self.at;
                self.at += 1;
                self.skip_ident_tail();
                Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")))
            }
            b':' if self.peek_byte(1) == Some(b':') => {
                self.at += 2;
                Some(Token::ColonColon)
            }
            b'#' => {
                self.at += 1;
                Some(Token::Pound)
            }
            b'!' => {
                self.at += 1;
                Some(Token::Bang)
            }
            b'=' => {
                if self.peek_byte(1) == Some(b'>') {
                    self.at += 2;
                    Some(Token::FatArrow)
                } else {
                    self.at += 1;
                    Some(Token::Eq)
                }
            }
            b',' => {
                self.at += 1;
                Some(Token::Comma)
            }
            b';' => {
                self.at += 1;
                Some(Token::Semi)
            }
            b'(' => {
                self.at += 1;
                Some(Token::LParen)
            }
            b')' => {
                self.at += 1;
                Some(Token::RParen)
            }
            b'[' => {
                self.at += 1;
                Some(Token::LBracket)
            }
            b']' => {
                self.at += 1;
                Some(Token::RBracket)
            }
            b'{' => {
                self.at += 1;
                Some(Token::LBrace)
            }
            b'}' => {
                self.at += 1;
                Some(Token::RBrace)
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = self.at;
                self.at += 1;
                self.skip_ident_tail();
                Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")))
            }
            _ if b >= 0x80 => {
                let start = self.at;
                if let Some(ch) = self.rest().chars().next() {
                    self.at += ch.len_utf8();
                    if ch.is_alphabetic() || ch == '_' {
                        self.skip_ident_tail();
                        return Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")));
                    }
                } else {
                    self.at += 1;
                }
                Some(Token::Other)
            }
            _ => {
                self.at += 1;
                Some(Token::Other)
            }
        }
    }
}

fn closing_delim(open: Token<'_>) -> Option<Token<'_>> {
    match open {
        Token::LParen => Some(Token::RParen),
        Token::LBracket => Some(Token::RBracket),
        Token::LBrace => Some(Token::RBrace),
        _ => None,
    }
}

fn skip_delimited(lex: &mut Lexer<'_>, open: Token<'_>, close: Token<'_>) {
    let _ = skip_delimited_end(lex, open, close);
}

fn skip_delimited_end(lex: &mut Lexer<'_>, open: Token<'_>, close: Token<'_>) -> usize {
    let mut depth: usize = 1;
    loop {
        lex.skip_ws_and_comments();
        let tok_start = lex.at;
        let Some(tok) = lex.next_token() else {
            return lex.src.len();
        };
        if tok == open {
            depth += 1;
        } else if tok == close {
            depth -= 1;
            if depth == 0 {
                return tok_start;
            }
        }
    }
}

/// Returns `true` when `std` is at the start of a root path (`std::...` or `::std::...`) rather
/// than a submodule of another path (`my_mod::std::...`).
fn is_root_std_path(prev: Option<Token<'_>>, prev_prev: Option<Token<'_>>) -> bool {
    if prev != Some(Token::ColonColon) {
        return true;
    }
    match prev_prev {
        Some(Token::Ident(word)) => matches!(
            word,
            "use"
                | "pub"
                | "return"
                | "let"
                | "mut"
                | "ref"
                | "in"
                | "if"
                | "else"
                | "match"
                | "while"
                | "for"
                | "loop"
                | "as"
                | "where"
                | "impl"
                | "fn"
                | "const"
                | "static"
                | "type"
                | "struct"
                | "enum"
                | "trait"
                | "unsafe"
                | "async"
                | "move"
                | "dyn"
                | "crate"
        ),
        _ => true,
    }
}

/// Skips the remainder of one comma-separated item inside a `{ ... }` `use` tree group.
/// Returns `true` when another item follows (stopped at `,`), or `false` when the enclosing
/// group ended (stopped at `}` or EOF).
fn skip_group_item(lex: &mut Lexer<'_>) -> bool {
    let mut nested: usize = 0;
    while let Some(tok) = lex.next_token() {
        match tok {
            Token::LBrace => nested += 1,
            Token::RBrace => {
                if nested == 0 {
                    return false;
                }
                nested -= 1;
            }
            Token::Comma if nested == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Inspects a grouped `use std::os::{ ... }` list right after `{` for `unix`, `linux`, or `macos`
/// at top level of the group (`std::os::fd` is exposed on WASI and stays Wasm).
fn os_group_mentions_host_os(lex: &mut Lexer<'_>, depth: usize) -> bool {
    if depth > 16 {
        skip_delimited(lex, Token::LBrace, Token::RBrace);
        return false;
    }
    loop {
        match lex.next_token() {
            None | Some(Token::RBrace) => return false,
            Some(Token::Comma) => continue,
            Some(Token::Ident("unix" | "linux" | "macos")) => return true,
            Some(Token::LBrace) => {
                skip_delimited(lex, Token::LBrace, Token::RBrace);
                if !skip_group_item(lex) {
                    return false;
                }
            }
            Some(_) => {
                if !skip_group_item(lex) {
                    return false;
                }
            }
        }
    }
}

/// Inspects a grouped `use std::{ ... }` list right after `{` for any `os::unix`, `os::linux`,
/// `os::macos`, or nested `os::{ ... }` branch referencing a host-only OS module.
fn std_group_mentions_host_os(lex: &mut Lexer<'_>, depth: usize) -> bool {
    if depth > 16 {
        skip_delimited(lex, Token::LBrace, Token::RBrace);
        return false;
    }
    loop {
        match lex.next_token() {
            None | Some(Token::RBrace) => return false,
            Some(Token::Comma) => continue,
            Some(Token::Ident("os")) => match lex.next_token() {
                Some(Token::ColonColon) => match lex.next_token() {
                    Some(Token::Ident("unix" | "linux" | "macos")) => return true,
                    Some(Token::LBrace) => {
                        if os_group_mentions_host_os(lex, depth + 1) {
                            return true;
                        }
                        if !skip_group_item(lex) {
                            return false;
                        }
                    }
                    Some(Token::RBrace) | None => return false,
                    Some(Token::Comma) => continue,
                    Some(_) => {
                        if !skip_group_item(lex) {
                            return false;
                        }
                    }
                },
                Some(Token::RBrace) | None => return false,
                Some(Token::Comma) => continue,
                Some(Token::LBrace) => {
                    skip_delimited(lex, Token::LBrace, Token::RBrace);
                    if !skip_group_item(lex) {
                        return false;
                    }
                }
                Some(_) => {
                    if !skip_group_item(lex) {
                        return false;
                    }
                }
            },
            Some(Token::LBrace) => {
                skip_delimited(lex, Token::LBrace, Token::RBrace);
                if !skip_group_item(lex) {
                    return false;
                }
            }
            Some(_) => {
                if !skip_group_item(lex) {
                    return false;
                }
            }
        }
    }
}

/// 3-valued truth value of a `cfg` predicate on a target class.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TriBool {
    True,
    False,
    Unknown,
}

impl TriBool {
    fn not(self) -> Self {
        match self {
            TriBool::True => TriBool::False,
            TriBool::False => TriBool::True,
            TriBool::Unknown => TriBool::Unknown,
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (TriBool::False, _) | (_, TriBool::False) => TriBool::False,
            (TriBool::True, TriBool::True) => TriBool::True,
            _ => TriBool::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (TriBool::True, _) | (_, TriBool::True) => TriBool::True,
            (TriBool::False, TriBool::False) => TriBool::False,
            _ => TriBool::Unknown,
        }
    }
}

/// Simultaneous evaluation of a `cfg` predicate on the concrete host target (`ScriptCfg`) and on
/// WASI (`unix = false`, `windows = false`, `target_family = "wasm"`, `target_os = "wasi"`,
/// `target_env = ""`, `target_vendor = "unknown"`, `target_pointer_width = "64"`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct CfgEval {
    host: TriBool,
    wasi: TriBool,
    /// True when the predicate depends on a host-vs-WASI target difference (`unix`,
    /// `target_family`, concrete host `target_os` / `target_arch` / `target_env` /
    /// `target_vendor`, `target_os = "wasi"`, `target_arch = "wasm32" | "wasm64"`) that is not
    /// short-circuited to the same truth value on both targets.
    family_div: bool,
}

impl CfgEval {
    const UNKNOWN: Self = Self { host: TriBool::Unknown, wasi: TriBool::Unknown, family_div: false };

    fn leaf(host: TriBool, wasi: TriBool) -> Self {
        let family_div = matches!(
            (host, wasi),
            (TriBool::True, TriBool::False) | (TriBool::False, TriBool::True)
        );
        Self { host, wasi, family_div }
    }

    fn diverges_host_vs_wasi(self) -> bool {
        self.family_div
            || matches!(
                (self.host, self.wasi),
                (TriBool::True, TriBool::False) | (TriBool::False, TriBool::True)
            )
    }
}

struct ParsedAttr<'a> {
    cfg: Option<CfgEval>,
    path: Option<StrLit<'a>>,
    macro_use: bool,
    macro_export: bool,
}

/// Consumes the body of an attribute `[...]` (starting immediately after `[`) and extracts any
/// `cfg(...)` evaluation, `path = "..."` value, `macro_use`, or `macro_export`.
fn parse_attr<'a>(lex: &mut Lexer<'a>, cfg: ScriptCfg<'_>) -> ParsedAttr<'a> {
    let mut look = *lex;
    let mut out = ParsedAttr { cfg: None, path: None, macro_use: false, macro_export: false };
    match look.next_token() {
        Some(Token::Ident("cfg")) if look.next_token() == Some(Token::LParen) => {
            out.cfg = Some(eval_cfg_pair(&mut look, cfg, 0));
        }
        Some(Token::Ident("path")) if look.next_token() == Some(Token::Eq) => {
            if let Some(val) = str_literal(look.next_token()) {
                out.path = Some(val);
            }
        }
        Some(Token::Ident("macro_use")) => {
            out.macro_use = true;
        }
        Some(Token::Ident("macro_export")) => {
            out.macro_export = true;
        }
        _ => {}
    }
    skip_delimited(lex, Token::LBracket, Token::RBracket);
    out
}

fn eq_opt(actual: Option<&str>, expected: &str) -> TriBool {
    match actual {
        Some(a) if a == expected => TriBool::True,
        Some(_) => TriBool::False,
        None => TriBool::Unknown,
    }
}

fn eval_cfg_pair(lex: &mut Lexer<'_>, cfg: ScriptCfg<'_>, depth: usize) -> CfgEval {
    if depth > 16 {
        return CfgEval::UNKNOWN;
    }
    let Some(Token::Ident(name)) = lex.next_token() else { return CfgEval::UNKNOWN };
    let mut look = *lex;
    match look.next_token() {
        Some(Token::LParen) => {
            *lex = look;
            match name {
                "all" => {
                    let mut host = TriBool::True;
                    let mut wasi = TriBool::True;
                    let mut any_div = false;
                    loop {
                        let mut peek = *lex;
                        if matches!(peek.next_token(), None | Some(Token::RParen)) {
                            *lex = peek;
                            break;
                        }
                        let sub = eval_cfg_pair(lex, cfg, depth + 1);
                        host = host.and(sub.host);
                        wasi = wasi.and(sub.wasi);
                        any_div |= sub.family_div;
                        let mut after = *lex;
                        match after.next_token() {
                            Some(Token::Comma) => *lex = after,
                            Some(Token::RParen) | None => {
                                *lex = after;
                                break;
                            }
                            _ => {
                                skip_delimited(lex, Token::LParen, Token::RParen);
                                break;
                            }
                        }
                    }
                    let same_known = matches!(
                        (host, wasi),
                        (TriBool::False, TriBool::False) | (TriBool::True, TriBool::True)
                    );
                    CfgEval { host, wasi, family_div: !same_known && any_div }
                }
                "any" => {
                    let mut host = TriBool::False;
                    let mut wasi = TriBool::False;
                    let mut any_div = false;
                    loop {
                        let mut peek = *lex;
                        if matches!(peek.next_token(), None | Some(Token::RParen)) {
                            *lex = peek;
                            break;
                        }
                        let sub = eval_cfg_pair(lex, cfg, depth + 1);
                        host = host.or(sub.host);
                        wasi = wasi.or(sub.wasi);
                        any_div |= sub.family_div;
                        let mut after = *lex;
                        match after.next_token() {
                            Some(Token::Comma) => *lex = after,
                            Some(Token::RParen) | None => {
                                *lex = after;
                                break;
                            }
                            _ => {
                                skip_delimited(lex, Token::LParen, Token::RParen);
                                break;
                            }
                        }
                    }
                    let same_known = matches!(
                        (host, wasi),
                        (TriBool::True, TriBool::True) | (TriBool::False, TriBool::False)
                    );
                    CfgEval { host, wasi, family_div: !same_known && any_div }
                }
                "not" => {
                    let sub = eval_cfg_pair(lex, cfg, depth + 1);
                    let mut after = *lex;
                    if after.next_token() == Some(Token::RParen) {
                        *lex = after;
                    } else {
                        skip_delimited(lex, Token::LParen, Token::RParen);
                    }
                    CfgEval { host: sub.host.not(), wasi: sub.wasi.not(), family_div: sub.family_div }
                }
                _ => {
                    skip_delimited(lex, Token::LParen, Token::RParen);
                    CfgEval::UNKNOWN
                }
            }
        }
        Some(Token::Eq) => {
            *lex = look;
            let Some(lit) = str_literal(lex.next_token()) else { return CfgEval::UNKNOWN };
            let mut buf = [0u8; 128];
            let Some(val) = decode_str_lit_stack(lit, &mut buf) else { return CfgEval::UNKNOWN };
            match (name, val) {
                ("feature", feat) => match cfg.features {
                    Some(active) => {
                        let present = if active.contains(&feat) { TriBool::True } else { TriBool::False };
                        CfgEval::leaf(present, present)
                    }
                    None => CfgEval::UNKNOWN,
                },
                ("target_family", "unix") => CfgEval::leaf(TriBool::True, TriBool::False),
                ("target_family", "wasm") => CfgEval::leaf(TriBool::False, TriBool::True),
                ("target_family", _) => CfgEval::leaf(TriBool::False, TriBool::False),
                ("target_os", "wasi") => CfgEval::leaf(TriBool::False, TriBool::True),
                ("target_os", "windows") => CfgEval::leaf(TriBool::False, TriBool::False),
                ("target_os", other) => CfgEval::leaf(eq_opt(cfg.host_os, other), TriBool::False),
                ("target_arch", "wasm32" | "wasm64") => {
                    CfgEval { host: TriBool::False, wasi: TriBool::Unknown, family_div: true }
                }
                ("target_arch", other) => CfgEval::leaf(eq_opt(cfg.host_arch, other), TriBool::False),
                ("target_env", "") => CfgEval::leaf(eq_opt(cfg.host_env, ""), TriBool::True),
                ("target_env", other) => CfgEval::leaf(eq_opt(cfg.host_env, other), TriBool::False),
                ("target_vendor", "unknown") => CfgEval::leaf(eq_opt(cfg.host_vendor, "unknown"), TriBool::True),
                ("target_vendor", other) => CfgEval::leaf(eq_opt(cfg.host_vendor, other), TriBool::False),
                ("target_pointer_width", "64") => {
                    CfgEval::leaf(eq_opt(cfg.host_pointer_width, "64"), TriBool::True)
                }
                ("target_pointer_width", other) => {
                    CfgEval::leaf(eq_opt(cfg.host_pointer_width, other), TriBool::False)
                }
                ("target_endian", "little") => CfgEval::leaf(TriBool::True, TriBool::True),
                ("target_endian", _) => CfgEval::leaf(TriBool::False, TriBool::False),
                _ => CfgEval::UNKNOWN,
            }
        }
        _ => match name {
            "unix" => CfgEval::leaf(TriBool::True, TriBool::False),
            "windows" | "test" | "doctest" => CfgEval::leaf(TriBool::False, TriBool::False),
            _ => CfgEval::UNKNOWN,
        },
    }
}
