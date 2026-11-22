//! Every `#[rustc_intrinsic]` the pinned library declares, sorted by how blaze builds it
//! (front2/intrinsic_rule.rs), so a toolchain bump that adds an intrinsic only the compiler can
//! supply fails HERE, by name, instead of a real run stopping at its first call (rule 82).
//!
//! The library read is the checkout's `library/` (its `DIGEST` is the one blaze is built with:
//! blaze-cargo toolchain/library.rs `BUILT_WITH`), its `core`, `alloc` and `std` sources. The
//! scan is plain text: each line that is exactly `#[rustc_intrinsic]`, the attributes and doc
//! lines around it, the `fn name` that follows, and whether `;` or `{` ends that signature
//! (outside parentheses and brackets: `-> [u8; 32];` is no body). A `;` declaration written
//! inside `intrinsic_dispatch_on_type! { .. }` (core/src/intrinsics/macros.rs) has a body: the
//! macro writes `fn name<T: name::Dispatch>(..) { <T as name::Dispatch>::dispatch(..) }`.
//!
//! The three lists:
//! 1. NAMED: blaze-build's table names it (`Which::named`): built as its row says.
//! 2. FALLBACK: not named, declared with a body, not `#[rustc_intrinsic_must_be_overridden]`:
//!    built from that body like any fn (rule 2 of intrinsic_rule.rs).
//! 3. UNBUILT: not named, and no body or must-be-overridden: refused when a run reaches it.
//!    Those marked `#[rustc_comptime]` are printed apart: rustc refuses a call of one outside
//!    a constant, so no run reaches one (blaze's constant evaluator answers them). Every other
//!    one must be in [`KNOWN_UNBUILT`] with its reason: a new one fails the test by name.
//!
//! Prints: `intrinsics of core: <n> declared; named <a>; fallback <b>; unbuilt <c> (<d>
//! comptime, <e> known)`, then each list with each declaration's `file:line`.

use std::path::{Path, PathBuf};

use blaze_build::intrinsics::Which;

/// The unbuilt intrinsics no run of this toolchain reaches yet, each with why it has no row.
/// A row added to the table (or a body added in core) removes its line here; a toolchain bump
/// that adds one adds a line, with its reason, in the same change.
const KNOWN_UNBUILT: &[(&str, &str)] = &[
    ("amdgpu_dispatch_ptr", "under `cfg(any(doc, target_arch = \"amdgpu\"))`: never declared for aarch64"),
    ("autodiff", "Enzyme's automatic differentiation: only the compiler's autodiff pass supplies it"),
    ("breakpoint", "a target trap instruction; no row yet"),
    ("fadd_algebraic", "float add the optimizer may reassociate; no row yet (its allowed results are not pinned here: rule 4)"),
    ("fadd_fast", "float add, UB on a non-finite input or result; no row yet"),
    ("fdiv_algebraic", "float div the optimizer may reassociate; no row yet"),
    ("fdiv_fast", "float div, UB on a non-finite input or result; no row yet"),
    ("fmul_algebraic", "float mul the optimizer may reassociate; no row yet"),
    ("fmul_fast", "float mul, UB on a non-finite input or result; no row yet"),
    ("frem_algebraic", "float rem the optimizer may reassociate; no row yet"),
    ("frem_fast", "float rem, UB on a non-finite input or result; no row yet"),
    ("fsub_algebraic", "float sub the optimizer may reassociate; no row yet"),
    ("fsub_fast", "float sub, UB on a non-finite input or result; no row yet"),
    ("gpu_launch_sized_workgroup_mem", "under `cfg(any(doc, target_arch = \"amdgpu\", target_arch = \"nvptx64\"))`: never declared for aarch64"),
    ("nontemporal_store", "a store with a non-temporal hint; no row yet"),
    ("offload", "GPU offload: only the compiler's offload backend supplies it"),
    ("offload_get_num_devices", "GPU offload: only the compiler's offload backend supplies it"),
    ("rustc_peek", "rustc's dataflow test hook: no program's run calls it"),
    ("simd_arith_offset", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_carryless_mul", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_cast_ptr", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_expose_provenance", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_extract_dyn", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_fcos", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_fexp", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_fexp2", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_flog", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_flog10", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_flog2", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_fsin", "a `simd_*` op intrinsics/simd.rs has no row for yet (libm per lane)"),
    ("simd_funnel_shl", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_funnel_shr", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_insert_dyn", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("simd_with_exposed_provenance", "a `simd_*` op intrinsics/simd.rs has no row for yet"),
    ("sve_tuple_create2", "SVE scalable vector tuples: blaze lays out no scalable vector"),
    ("sve_tuple_create3", "SVE scalable vector tuples: blaze lays out no scalable vector"),
    ("sve_tuple_create4", "SVE scalable vector tuples: blaze lays out no scalable vector"),
    ("sve_tuple_get", "SVE scalable vector tuples: blaze lays out no scalable vector"),
    ("sve_tuple_set", "SVE scalable vector tuples: blaze lays out no scalable vector"),
    ("va_arg", "C variadics: blaze-build has no row reading a `VaList` yet"),
    ("vtable_align", "a vtable's alignment word; no row yet"),
    ("vtable_size", "a vtable's size word; no row yet"),
];

/// One `#[rustc_intrinsic]` declaration as the scan read it.
struct Declaration {
    /// `core/src/intrinsics/mod.rs:1728`: the `fn` line, from the library's root.
    at: String,
    name: String,
    /// A body follows the signature (or the dispatch macro writes one).
    has_body: bool,
    must_be_overridden: bool,
    comptime: bool,
}

fn library() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library")
}

/// Every `.rs` file under `dir`, sorted by path (the scan's order is the files', never the
/// directory listing's).
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("listing {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap_or_else(|e| panic!("an entry of {}: {e}", dir.display())).path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// The name after the first `fn ` word of `line`.
fn fn_name(line: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(i) = line[from..].find("fn ") {
        let at = from + i;
        let starts_word = at == 0 || line.as_bytes()[at - 1] == b' ';
        if starts_word {
            let rest = &line[at + 3..];
            let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
            return (end > 0).then(|| &rest[..end]);
        }
        from = at + 3;
    }
    None
}

/// Whether the signature starting on `lines[from]` ends in `{` (a body) rather than `;`: the
/// first of the two outside `(..)` and `[..]`, line comments skipped. `None`: neither found
/// within 40 lines (the scan then fails naming the place).
fn ends_in_body(lines: &[&str], from: usize) -> Option<bool> {
    let mut depth = 0i32;
    for line in lines.iter().skip(from).take(40) {
        let code = line.find("//").map_or(*line, |i| &line[..i]);
        for c in code.chars() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ';' if depth == 0 => return Some(false),
                '{' if depth == 0 => return Some(true),
                _ => {}
            }
        }
    }
    None
}

fn is_attr_or_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("#[") || t.starts_with("//")
}

fn scan(root: &Path, file: &Path, out: &mut Vec<Declaration>) {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
    let shown = file.strip_prefix(root).unwrap_or(file).display().to_string();
    let lines: Vec<&str> = text.lines().collect();
    // Inside an `intrinsic_dispatch_on_type! { .. }` invocation: from its line to the next `}`
    // at the start of a line.
    let mut dispatch = vec![false; lines.len()];
    let mut inside = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("intrinsic_dispatch_on_type!") {
            inside = true;
        } else if inside && line.starts_with('}') {
            inside = false;
        }
        dispatch[i] = inside;
    }
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[rustc_intrinsic]" {
            continue;
        }
        // The item's attributes: the contiguous attribute and comment lines above, and those
        // below up to the `fn` line.
        let mut attrs: Vec<&str> = Vec::new();
        let mut up = i;
        while up > 0 && is_attr_or_comment(lines[up - 1]) {
            up -= 1;
            attrs.push(lines[up].trim());
        }
        let mut down = i + 1;
        while down < lines.len() && is_attr_or_comment(lines[down]) {
            attrs.push(lines[down].trim());
            down += 1;
        }
        let fn_line = lines.get(down).copied().unwrap_or("");
        let name = fn_name(fn_line).unwrap_or_else(|| panic!("{shown}:{}: `#[rustc_intrinsic]` is followed by no `fn name` but `{fn_line}`", i + 1));
        let ends = ends_in_body(&lines, down).unwrap_or_else(|| panic!("{shown}:{}: the signature of `{name}` ends in neither `;` nor `{{`", down + 1));
        let is_attr = |a: &str| attrs.iter().any(|l| l.starts_with(a));
        out.push(Declaration {
            at: format!("{shown}:{}", down + 1),
            name: name.to_string(),
            has_body: ends || dispatch[down],
            must_be_overridden: is_attr("#[rustc_intrinsic_must_be_overridden"),
            comptime: is_attr("#[rustc_comptime"),
        });
    }
}

#[test]
fn every_intrinsic_of_the_pinned_library_is_named_has_a_fallback_body_or_is_known() {
    let root = library();
    let mut files = Vec::new();
    for krate in ["core/src", "alloc/src", "std/src"] {
        rust_files(&root.join(krate), &mut files);
    }
    files.sort();
    let mut all = Vec::new();
    for file in &files {
        scan(&root, file, &mut all);
    }
    // The scan ran over the real library: core alone declares some hundreds (rule 61).
    assert!(all.len() >= 200, "the scan found {} `#[rustc_intrinsic]` declarations under {}", all.len(), root.display());

    let mut named: Vec<&Declaration> = Vec::new();
    let mut fallback: Vec<&Declaration> = Vec::new();
    let mut comptime: Vec<&Declaration> = Vec::new();
    let mut unbuilt: Vec<&Declaration> = Vec::new();
    for d in &all {
        if Which::named(&d.name).is_ok() {
            named.push(d);
        } else if d.has_body && !d.must_be_overridden {
            fallback.push(d);
        } else if d.comptime {
            comptime.push(d);
        } else {
            unbuilt.push(d);
        }
    }
    println!(
        "intrinsics of core: {} declared; named {}; fallback {}; unbuilt {} ({} comptime, {} known)",
        all.len(),
        named.len(),
        fallback.len(),
        comptime.len() + unbuilt.len(),
        comptime.len(),
        unbuilt.len()
    );
    let show = |title: &str, list: &[&Declaration]| {
        println!("{title} ({}):", list.len());
        for d in list {
            let why = if d.must_be_overridden { " [must be overridden]" } else if d.has_body { "" } else { " [no body]" };
            println!("  {} {}{why}", d.name, d.at);
        }
    };
    show("NAMED by the table", &named);
    show("FALLBACK: built from the body", &fallback);
    show("UNBUILT, `#[rustc_comptime]`: no run reaches one", &comptime);
    show("UNBUILT: a run stops at the first call", &unbuilt);

    // The stop this file was written for: `integer_min`/`integer_max` (core/src/cmp.rs, `Ord::min`
    // of the integers) are fallback bodies, built from their bodies.
    for want in ["integer_min", "integer_max"] {
        assert!(fallback.iter().any(|d| d.name == want), "`{want}` is not among the fallback bodies");
    }

    let mut got: Vec<&str> = unbuilt.iter().map(|d| d.name.as_str()).collect();
    got.sort_unstable();
    got.dedup();
    let known: Vec<&str> = KNOWN_UNBUILT.iter().map(|(n, _)| *n).collect();
    let new: Vec<String> = unbuilt.iter().filter(|d| !known.contains(&d.name.as_str())).map(|d| format!("{} at {}", d.name, d.at)).collect();
    let gone: Vec<&str> = known.iter().copied().filter(|n| !got.contains(n)).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "the unbuilt intrinsics moved.\nNEW (a run would stop at each: add a table row with its exact semantics, or a line to KNOWN_UNBUILT with why): {new:#?}\nGONE (now named or with a body: remove its line): {gone:#?}"
    );
}
