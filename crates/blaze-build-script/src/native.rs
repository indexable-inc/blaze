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
    /// Unconditional reference to host-OS-specific `std::os::{unix,linux,macos,fd}` APIs
    /// (absent on `wasm32-wasip1` / `wasm64-blaze-wasip1`), not guarded by a WASI-excluding
    /// `#[cfg(...)]`.
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

/// Maximum number of distinct files inspected by [`source_tree_requires_native`] for one build
/// script (cycle and pathological fan-out bound, rule 18).
const MAX_SCRIPT_FILES: usize = 256;

/// Inspects a build script's root file and every active submodule (`mod foo;`,
/// `#[path = "..."] mod foo;`) and static `include!("...")` file reachable from it, using
/// `read_file` to load each package-relative path.
///
/// `read_file(rel, is_root, visit)` calls `visit(source_text)` and returns `Ok(true)` when the
/// file exists, `Ok(false)` when absent (used to probe `foo.rs` before `foo/mod.rs`), or `Err(e)`
/// on any read/decode failure (which is propagated immediately rather than assuming Wasm).
pub fn source_tree_requires_native<E, F>(
    root_rel: &str,
    alloc: Alloc<'_>,
    mut read_file: F,
) -> Result<Option<NativeSourceReason>, E>
where
    F: FnMut(&str, bool, &mut dyn FnMut(&str)) -> Result<bool, E>,
{
    enum Pending<'a> {
        Root(AString<'a>),
        Exact(AString<'a>),
        Module { primary: AString<'a>, fallback: AString<'a> },
    }

    let Some(norm_root) = normalize_rel_path("", root_rel, alloc) else {
        return Ok(None);
    };

    let mut visited: AVec<'_, AString<'_>> = Vec::new_in(alloc);
    let mut queue: AVec<'_, Pending<'_>> = Vec::new_in(alloc);
    queue.push(Pending::Root(norm_root));

    let mut head: usize = 0;
    while let Some(item) = queue.get(head) {
        head += 1;
        if visited.len() >= MAX_SCRIPT_FILES {
            break;
        }
        let (is_root, primary_str, fallback_str) = match item {
            Pending::Root(p) => (true, p.as_str(), None),
            Pending::Exact(p) => (false, p.as_str(), None),
            Pending::Module { primary, fallback } => (false, primary.as_str(), Some(fallback.as_str())),
        };

        let p_copy = String::from_str_in(primary_str, alloc);
        let fb_copy = fallback_str.map(|fb| String::from_str_in(fb, alloc));

        let mut scanned_ok = false;
        let mut discovered: AVec<'_, ChildRef<'_>> = Vec::new_in(alloc);
        let mut reason_hit: Option<NativeSourceReason> = None;

        if !visited.iter().any(|seen| seen.as_str() == p_copy.as_str()) {
            let existed = read_file(p_copy.as_str(), is_root, &mut |text| {
                reason_hit = scan_source(text, is_root, p_copy.as_str(), Some(&mut discovered), alloc);
            })?;
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
            let existed = read_file(fb.as_str(), false, &mut |text| {
                reason_hit = scan_source(text, false, fb.as_str(), Some(&mut discovered), alloc);
            })?;
            if existed {
                visited.push(fb);
            }
            if let Some(reason) = reason_hit {
                return Ok(Some(reason));
            }
        }

        for child in discovered {
            if visited.len() + queue.len().saturating_sub(head) >= MAX_SCRIPT_FILES * 2 {
                break;
            }
            match child {
                ChildRef::Exact(path) => queue.push(Pending::Exact(path)),
                ChildRef::Module { primary, fallback } => queue.push(Pending::Module { primary, fallback }),
            }
        }
    }

    Ok(None)
}

/// Inspects a single build script's Rust source text (zero allocations, sans-IO, exact Rust
/// lexical scanning) and returns `Some(reason)` when the source unconditionally requires Tier-2
/// native host execution rather than `wasm32-wasip1` / `wasm64-blaze-wasip1`.
pub fn source_requires_native(source: &str) -> Option<NativeSourceReason> {
    scan_source_no_alloc(source)
}

enum ChildRef<'a> {
    Exact(AString<'a>),
    Module { primary: AString<'a>, fallback: AString<'a> },
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

fn scan_source_no_alloc(source: &str) -> Option<NativeSourceReason> {
    scan_source_core(source, true, "", None)
}

fn scan_source<'a>(
    source: &str,
    is_root: bool,
    current_rel: &str,
    children: Option<&mut AVec<'a, ChildRef<'a>>>,
    alloc: Alloc<'a>,
) -> Option<NativeSourceReason> {
    scan_source_core(source, is_root, current_rel, children.map(|out| (out, alloc)))
}

fn scan_source_core<'a>(
    source: &str,
    is_root: bool,
    current_rel: &str,
    mut child_sink: Option<(&mut AVec<'a, ChildRef<'a>>, Alloc<'a>)>,
) -> Option<NativeSourceReason> {
    let mut lex = Lexer::new(source);
    let mut brace_stack = [false; 64];
    let mut mod_stack: [Option<&str>; 64] = [None; 64];
    let mut brace_len: usize = 0;
    let mut brace_overflow: usize = 0;
    let mut excluded_depth: usize = 0;
    let mut paren_depth: usize = 0;
    let mut pending_excluded = false;
    let mut pending_depth: usize = 0;
    let mut stmt_needs_semi = false;
    let mut pending_path_attr: Option<&str> = None;
    let mut pending_path_depth: usize = 0;
    let mut pending_inline_mod: Option<&str> = None;
    let mut prev: Option<Token<'_>> = None;
    let mut prev_prev: Option<Token<'_>> = None;

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
                        continue;
                    }
                }
                Some(Token::LBracket) => false,
                _ => {
                    prev_prev = prev;
                    prev = Some(tok);
                    continue;
                }
            };
            lex = look;
            let attr = parse_attr(&mut lex);
            if let Some(cfg) = attr.cfg {
                if inner {
                    if brace_depth == 0 {
                        // Crate-level `#![cfg(...)]`: if false on WASI but not known-false on
                        // Unix host (e.g. `#![cfg(unix)]`, `#![cfg(target_os = "linux")]`,
                        // `#![cfg(not(target_family = "wasm"))]`), compiling on WASI strips
                        // `main()` and fails -> must run native! If false on BOTH host and WASI
                        // (e.g. `#![cfg(windows)]`), the crate is inactive on both -> None.
                        if cfg.wasi == TriBool::False {
                            return if cfg.host != TriBool::False {
                                Some(NativeSourceReason::HostFamilyCfg)
                            } else {
                                None
                            };
                        }
                    } else if cfg.wasi == TriBool::False
                        && brace_overflow == 0
                        && let Some(slot) = brace_len.checked_sub(1).and_then(|idx| brace_stack.get_mut(idx))
                        && !*slot
                    {
                        *slot = true;
                        excluded_depth += 1;
                    }
                } else if cfg.wasi == TriBool::False && excluded_depth == 0 {
                    pending_excluded = true;
                    pending_depth = brace_depth;
                }
            }
            if !inner && let Some(path_val) = attr.path {
                pending_path_attr = Some(path_val);
                pending_path_depth = brace_depth;
            }
            prev_prev = prev;
            prev = Some(Token::RBracket);
            continue;
        }

        // Skip unexpanded `macro_rules! name { ... }` / `(...)` / `[...]` token trees completely
        // so tokens inside macro definitions are never misclassified as compiled code.
        if tok == Token::Ident("macro_rules") {
            let mut look = lex;
            if look.next_token() == Some(Token::Bang)
                && matches!(look.next_token(), Some(Token::Ident(_)))
                && let Some(open) = look.next_token()
                && let Some(close) = closing_delim(open)
            {
                skip_delimited(&mut look, open, close);
                if open != Token::LBrace {
                    let mut after = look;
                    if after.next_token() == Some(Token::Semi) {
                        look = after;
                    }
                }
                lex = look;
                if pending_excluded && brace_depth == pending_depth && !stmt_needs_semi {
                    pending_excluded = false;
                }
                pending_path_attr = None;
                prev_prev = prev;
                prev = Some(close);
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
                if let Some(slot) = brace_stack.get_mut(brace_len) {
                    *slot = this_excluded;
                    if let Some(mod_slot) = mod_stack.get_mut(brace_len) {
                        *mod_slot = pending_inline_mod.take();
                    }
                    brace_len += 1;
                } else {
                    pending_inline_mod = None;
                    brace_overflow += 1;
                }
                if this_excluded {
                    excluded_depth += 1;
                }
                if pending_path_depth == brace_depth {
                    pending_path_attr = None;
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
            }
            Token::Semi => {
                if brace_depth == pending_path_depth {
                    pending_path_attr = None;
                }
                pending_inline_mod = None;
            }
            _ => {}
        }

        if !is_excluded {
            match tok {
                Token::Ident("std") if is_root_std_path(prev, prev_prev) => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::ColonColon)
                        && look.next_token() == Some(Token::Ident("os"))
                        && look.next_token() == Some(Token::ColonColon)
                    {
                        match look.next_token() {
                            Some(Token::Ident("unix" | "linux" | "macos" | "fd")) => {
                                return Some(NativeSourceReason::HostOsStd);
                            }
                            Some(Token::LBrace) if brace_group_mentions_host_os(&mut look) => {
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
                        Some(Token::Str(_)) if look.next_token() == Some(Token::LBrace) => {
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
                        let ev = eval_cfg_pair(&mut body, 0);
                        if ev.diverges_host_vs_wasi() {
                            return Some(NativeSourceReason::HostFamilyCfg);
                        }
                        skip_delimited(&mut look, open, close);
                        lex = look;
                        prev_prev = prev;
                        prev = Some(close);
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
                        match look.next_token() {
                            Some(Token::Semi) => {
                                if let Some((ref mut out, alloc)) = child_sink {
                                    push_external_mod_ref(
                                        out,
                                        is_root,
                                        current_rel,
                                        mod_stack.get(..brace_len).unwrap_or(&[]),
                                        mod_name,
                                        attr_path,
                                        alloc,
                                    );
                                }
                            }
                            Some(Token::LBrace) => {
                                pending_inline_mod = Some(attr_path.unwrap_or(mod_name));
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
                        if let Some(Token::Str(inc_path)) = body.next_token()
                            && body.next_token() == Some(close)
                            && let Some((ref mut out, alloc)) = child_sink
                            && let Some(resolved) = normalize_rel_path(dir_of(current_rel), inc_path, alloc)
                        {
                            out.push(ChildRef::Exact(resolved));
                        }
                        skip_delimited(&mut look, open, close);
                        lex = look;
                        prev_prev = prev;
                        prev = Some(close);
                        continue;
                    }
                }
                _ => {}
            }
        } else if tok == Token::Ident("mod") {
            let mut look = lex;
            if let Some(Token::Ident(mod_name)) = look.next_token()
                && look.next_token() == Some(Token::LBrace)
            {
                pending_inline_mod = Some(mod_name);
            }
        }

        prev_prev = prev;
        prev = Some(tok);
    }

    None
}

fn push_external_mod_ref<'a>(
    out: &mut AVec<'a, ChildRef<'a>>,
    is_root: bool,
    current_rel: &str,
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
            out.push(ChildRef::Exact(resolved));
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
        out.push(ChildRef::Module { primary, fallback });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Ident(&'a str),
    Str(&'a str),
    RawStr,
    ColonColon,
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

    fn try_scan_raw_string(&mut self, prefix_len: usize) -> bool {
        let mut off = prefix_len;
        let mut hashes: usize = 0;
        while self.peek_byte(off) == Some(b'#') {
            hashes += 1;
            off += 1;
        }
        if self.peek_byte(off) != Some(b'"') {
            return false;
        }
        self.at += off + 1;
        while let Some(b) = self.peek_byte(0) {
            self.at += 1;
            if b == b'"' {
                let mut matched = 0;
                while matched < hashes && self.peek_byte(matched) == Some(b'#') {
                    matched += 1;
                }
                if matched == hashes {
                    self.at += hashes;
                    break;
                }
            }
        }
        true
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
                if self.try_scan_raw_string(1) {
                    return Some(Token::RawStr);
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
                    return Some(Token::RawStr);
                }
                if b == b'b' && self.peek_byte(1) == Some(b'\'') {
                    self.scan_byte_char_literal();
                    return Some(Token::Other);
                }
                if self.peek_byte(1) == Some(b'r') && self.try_scan_raw_string(2) {
                    return Some(Token::RawStr);
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
                self.at += 1;
                Some(Token::Eq)
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
    let mut depth: usize = 1;
    while let Some(tok) = lex.next_token() {
        if tok == open {
            depth += 1;
        } else if tok == close {
            depth -= 1;
            if depth == 0 {
                break;
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

/// Inspects a grouped `use std::os::{ ... }` list right after `{` for `unix`, `linux`, `macos`,
/// or `fd` at top level of the group.
fn brace_group_mentions_host_os(lex: &mut Lexer<'_>) -> bool {
    let mut depth: usize = 1;
    let mut at_item_start = true;
    while let Some(tok) = lex.next_token() {
        match tok {
            Token::LBrace => {
                depth += 1;
                at_item_start = false;
            }
            Token::RBrace => {
                depth -= 1;
                if depth == 0 {
                    return false;
                }
            }
            Token::Comma if depth == 1 => {
                at_item_start = true;
            }
            Token::Ident("unix" | "linux" | "macos" | "fd") if depth == 1 && at_item_start => {
                return true;
            }
            _ => {
                at_item_start = false;
            }
        }
    }
    false
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

/// Simultaneous evaluation of a `cfg` predicate on supported Unix hosts (`unix = true`,
/// `windows = false`, `target_family = "unix"`, `target_os != "wasi"`) and on WASI
/// (`unix = false`, `windows = false`, `target_family = "wasm"`, `target_os = "wasi"`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct CfgEval {
    host: TriBool,
    wasi: TriBool,
    /// True when the predicate depends on a host-vs-WASI family/OS distinction (`unix`,
    /// `target_family = "unix"`, `target_family = "wasm"`, `target_os = "wasi"`,
    /// `target_arch = "wasm32" | "wasm64"`) that is not short-circuited to the same truth value
    /// on both targets.
    ///
    /// Deliberately `false` for `target_os = "linux"` / `"macos"`, which pure-Rust Wasm scripts
    /// (`anyhow`, `proc-macro2`, `thiserror`) use for best-effort host cleanup quirks and evaluate
    /// safely under WASI.
    family_div: bool,
}

impl CfgEval {
    const UNKNOWN: Self = Self { host: TriBool::Unknown, wasi: TriBool::Unknown, family_div: false };

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
    path: Option<&'a str>,
}

/// Consumes the body of an attribute `[...]` (starting immediately after `[`) and extracts any
/// `cfg(...)` evaluation or `path = "..."` value.
fn parse_attr<'a>(lex: &mut Lexer<'a>) -> ParsedAttr<'a> {
    let mut look = *lex;
    let mut out = ParsedAttr { cfg: None, path: None };
    match look.next_token() {
        Some(Token::Ident("cfg")) if look.next_token() == Some(Token::LParen) => {
            out.cfg = Some(eval_cfg_pair(&mut look, 0));
        }
        Some(Token::Ident("path")) if look.next_token() == Some(Token::Eq) => {
            if let Some(Token::Str(val)) = look.next_token() {
                out.path = Some(val);
            }
        }
        _ => {}
    }
    skip_delimited(lex, Token::LBracket, Token::RBracket);
    out
}

fn eval_cfg_pair(lex: &mut Lexer<'_>, depth: usize) -> CfgEval {
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
                        let sub = eval_cfg_pair(lex, depth + 1);
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
                        let sub = eval_cfg_pair(lex, depth + 1);
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
                    let sub = eval_cfg_pair(lex, depth + 1);
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
            let Some(Token::Str(val)) = lex.next_token() else { return CfgEval::UNKNOWN };
            match (name, val) {
                ("target_family", "unix") => {
                    CfgEval { host: TriBool::True, wasi: TriBool::False, family_div: true }
                }
                ("target_family", "wasm") => {
                    CfgEval { host: TriBool::False, wasi: TriBool::True, family_div: true }
                }
                ("target_family", _) => {
                    CfgEval { host: TriBool::False, wasi: TriBool::False, family_div: false }
                }
                ("target_os", "wasi") => {
                    CfgEval { host: TriBool::False, wasi: TriBool::True, family_div: true }
                }
                ("target_os", "windows") => {
                    CfgEval { host: TriBool::False, wasi: TriBool::False, family_div: false }
                }
                ("target_os", _) => {
                    CfgEval { host: TriBool::Unknown, wasi: TriBool::False, family_div: false }
                }
                ("target_arch", "wasm32" | "wasm64") => {
                    CfgEval { host: TriBool::False, wasi: TriBool::Unknown, family_div: true }
                }
                ("target_arch", _) => {
                    CfgEval { host: TriBool::Unknown, wasi: TriBool::False, family_div: false }
                }
                ("target_env", "gnu" | "msvc" | "musl" | "sgx" | "ohos") => {
                    CfgEval { host: TriBool::Unknown, wasi: TriBool::False, family_div: false }
                }
                _ => CfgEval::UNKNOWN,
            }
        }
        _ => match name {
            "unix" => CfgEval { host: TriBool::True, wasi: TriBool::False, family_div: true },
            "windows" => CfgEval { host: TriBool::False, wasi: TriBool::False, family_div: false },
            _ => CfgEval::UNKNOWN,
        },
    }
}
