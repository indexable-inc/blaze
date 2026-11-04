//! `deep_drop_at_exit` (in `blaze::pedantic_perf`, allowed by default): in `fn main`, a local
//! collection whose elements own heap memory themselves (`Vec<String>`, `HashMap<String,
//! Vec<u8>>`) that is still alive when `main` returns. Dropping it walks every element and
//! frees each one, though the process is about to give all of its memory back at once. With
//! millions of elements that is a visible pause at exit. Goose frees a whole region at once;
//! at exit, the operating system does that for the process.
//!
//! Found: a plain `let` of `main`'s own block (the crate root's `main`, with no parameters)
//! whose type is std's `Vec`, `VecDeque`, `HashMap`, `HashSet`, `BTreeMap` or `BTreeSet` of
//! element types that own heap memory (`String`, `Vec`, `Box`, the same collections), every
//! type in it from std (so dropping only frees memory: no `Drop` of the crate's flushes a file
//! or prints), and every use only borrowing it (so it is dropped where `main` ends). A `main`
//! that calls `std::process::exit` or `std::mem::forget` is left alone.
//!
//! Allowed by default: leaking at exit hides real leaks from leak checkers (Valgrind, LSan),
//! and the time saved is worth it only for large collections, which the lint can't see. No
//! fix: where to flush and exit is the author's choice.

use hir_def::DefWithBodyId;

use super::{Finding, Lint, exempt, facts, general_facts, goose_locals};
use crate::check::Cx;

const LINT: Lint = Lint::DeepDropAtExit;

/// How deep the element types are read.
const DEPTH: u32 = 4;

/// The std collections whose drop walks their elements.
const OUTER: &[&str] = &["Vec", "VecDeque", "HashMap", "HashSet", "BTreeMap", "BTreeSet"];

/// The std types that own heap memory.
const HEAP: &[&str] =
    &["Vec", "VecDeque", "HashMap", "HashSet", "BTreeMap", "BTreeSet", "String", "Box"];

fn std_named(db: &base_db::Db, adt: hir::Adt, names: &[&str]) -> bool {
    names.contains(&adt.name(db).as_str())
        && general_facts::is_std_crate(db, adt.module(db).krate(db))
}

/// Whether every type in `ty` is a scalar, a tuple or array of such, or a std collection,
/// `String`, `Box` or `Option` of such, and whether one of them owns heap memory.
fn plain(db: &base_db::Db, ty: &hir::Type<'_>, depth: u32) -> Option<bool> {
    if depth == 0 {
        return None;
    }
    if ty.is_scalar() || ty.is_unit() {
        return Some(false);
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return plain(db, &inner, depth - 1);
    }
    if ty.is_tuple() {
        let mut heap = false;
        for it in ty.tuple_fields(db) {
            heap |= plain(db, &it, depth - 1)?;
        }
        return Some(heap);
    }
    let adt = ty.as_adt()?;
    let owns = std_named(db, adt, HEAP);
    if !owns && !std_named(db, adt, &["Option"]) {
        return None;
    }
    let mut heap = owns;
    for arg in ty.type_arguments() {
        // The hasher of a map: a std `RandomState` or a unit-like hasher, no heap.
        if arg.as_adt().is_some_and(|it| it.name(db).as_str() == "RandomState") {
            continue;
        }
        heap |= plain(db, &arg, depth - 1)?;
    }
    Some(heap)
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let hir::DefWithBody::Function(f) = analysis.def() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    if f.name(db).as_str() != "main"
        || !f.module(db).is_crate_root(db)
        || !f.assoc_fn_params(db).is_empty()
    {
        return found;
    }
    if general_facts::calls_std(db, analysis, "process", "exit")
        || general_facts::calls_std(db, analysis, "mem", "forget")
    {
        return found;
    }
    let body = analysis.body();
    let parents = facts::Parents::of(body);
    for made in goose_locals::lets(body, body.root_expr()) {
        let ty = analysis.ty(analysis.infer().binding_ty(made.local));
        let Some(adt) = ty.as_adt() else { continue };
        if !std_named(db, adt, OUTER) {
            continue;
        }
        let mut nested = false;
        let mut ok = true;
        for arg in ty.type_arguments() {
            if arg.as_adt().is_some_and(|it| it.name(db).as_str() == "RandomState") {
                continue;
            }
            match plain(db, &arg, DEPTH) {
                Some(heap) => nested |= heap,
                None => ok = false,
            }
        }
        if !ok || !nested {
            continue;
        }
        if !goose_locals::uses(db, def, analysis, &parents, made.local).only_borrowed {
            continue;
        }
        let Some(at) = analysis.expr_range(made.init) else { continue };
        if exempt::exempt(cx.text(), at, LINT) {
            continue;
        }
        let name = body[made.local].name.as_str();
        let target = cx.krate().to_display_target(db);
        let message = format!(
            "`{name}`, a `{}` whose elements own heap memory, is dropped where `main` ends: one free per element just \
             before the process exits",
            hir::HirDisplay::display(&ty, db, target)
        );
        let note = format!(
            "the operating system takes back all the memory at once: `std::mem::forget({name})` at the end of `main`, or \
             flushing what must be written and calling `std::process::exit(0)`, skips the per-element frees (Goose frees \
             a region at once). Leak checkers then report it"
        );
        facts::push(&mut found, LINT, at, message, Some(note));
    }
    found
}
