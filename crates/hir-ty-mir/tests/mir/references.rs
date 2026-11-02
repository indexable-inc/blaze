//! Of the passes run only at `MirOptLevel::Release`, none adds a call, constant, static, drop
//! or vtable a body lacks at `MirOptLevel::Debug`. The code generator's walk of what a unit
//! references reads `Debug` bodies for every level and relies on it.

use std::collections::HashSet;

use hir_def::StaticId;
use hir_ty::{
    db::HirDatabase,
    next_solver::{StoredConst, StoredTy},
};
use hir_ty_mir::mir::{
    CastKind, MirBody, MirOptLevel, MirOptions, Operand, OperandKind, Rvalue, StatementKind,
    TerminatorKind,
};
use hir_expand::fixture;

use crate::function;

/// What a body makes the code generator emit or link to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Reference {
    /// A function item, a value or the size or alignment of a type, by its type.
    Constant(StoredConst, StoredTy),
    Static(StaticId),
    /// The drop glue of a local's type.
    Drop(StoredTy),
    /// The vtable or function pointer of a coercion, by its target type.
    Coerced(StoredTy),
}

fn operand(operand: &Operand, out: &mut HashSet<Reference>) {
    match &operand.kind {
        OperandKind::Constant { konst, ty } => {
            out.insert(Reference::Constant(*konst, *ty));
        }
        OperandKind::Static(it) => {
            out.insert(Reference::Static(*it));
        }
        OperandKind::Copy(_) | OperandKind::Move(_) | OperandKind::Allocation { .. } => {}
    }
}

fn references(body: &MirBody<'_>) -> HashSet<Reference> {
    let mut out = HashSet::new();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(_, Rvalue::Cast(CastKind::PointerCoercion(_), _, ty))
            | StatementKind::Assign(_, Rvalue::Cast(CastKind::DynStar, _, ty)) = &statement.kind
            {
                out.insert(Reference::Coerced(*ty));
            }
            statement.kind.for_each_operand(&mut |it| operand(it, &mut out));
        }
        let Some(terminator) = &block.terminator else { continue };
        if let TerminatorKind::Drop { place, .. } = &terminator.kind
            && place.projection.is_empty()
        {
            out.insert(Reference::Drop(body.locals[place.local].ty));
        }
        terminator.kind.for_each_operand(&mut |it| operand(it, &mut out));
    }
    out
}

fn at(level: MirOptLevel) -> MirOptions {
    MirOptions { ub_checks: true, lifetime_markers: true, ..MirOptions::at(level) }
}

/// Every function of `fixture` and the closures in them reference at `Release` only what they
/// do at `Debug`.
#[track_caller]
fn release_adds_nothing(fixture: &str, functions: &[&str]) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        for name in functions {
            let func = function(&db, name);
            let body = |level| {
                db.optimized_mir_body(func.into(), at(level))
                    .unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"))
            };
            let (debug, release) = (body(MirOptLevel::Debug), body(MirOptLevel::Release));
            let added: Vec<_> = references(release).difference(&references(debug)).copied().collect();
            assert!(added.is_empty(), "`{name}` at Release references {added:?}");
            assert_eq!(debug.closures, release.closures, "`{name}`");
            for &closure in &debug.closures {
                let body = |level| {
                    db.optimized_mir_body_for_closure(closure, at(level))
                        .unwrap_or_else(|error| panic!("a closure of `{name}`: {error:?}"))
                };
                let (debug, release) = (body(MirOptLevel::Debug), body(MirOptLevel::Release));
                let added: Vec<_> =
                    references(release).difference(&references(debug)).copied().collect();
                assert!(added.is_empty(), "a closure of `{name}` at Release references {added:?}");
            }
        }
    });
}

#[test]
fn calls_constants_and_statics() {
    release_adds_nothing(
        r#"
static TABLE: [u32; 4] = [1, 2, 3, 4];
const LIMIT: u32 = 3;

fn twice(x: u32) -> u32 { x * 2 }
fn pick<T: Copy>(a: T, b: T, first: bool) -> T { if first { a } else { b } }

fn calls(x: u32) -> u32 {
    let mut total = 0;
    let mut i = 0;
    while i < 4 {
        let dead = twice(i);
        total = total + TABLE[i as usize];
        if total > LIMIT { total = twice(total); }
        i = i + 1;
    }
    pick(total, x, x > LIMIT)
}

fn constant_branch() -> u32 {
    if LIMIT > 10 { twice(1) } else { pick(1, 2, true) }
}
"#,
        &["calls", "constant_branch"],
    );
}

#[test]
fn drops_and_coercions() {
    release_adds_nothing(
        r#"
//- minicore: sized, copy, drop, unsize, coerce_unsized, dispatch_from_dyn, fn
struct Noisy(u32);
impl Drop for Noisy { fn drop(&mut self) {} }

trait Speak { fn speak(&self) -> u32; }
impl Speak for Noisy { fn speak(&self) -> u32 { self.0 } }

fn plain(x: u32) -> u32 { x + 1 }

fn drops(flag: bool) -> u32 {
    let a = Noisy(1);
    let b = Noisy(2);
    let kept = if flag { a } else { b };
    let speaker: &dyn Speak = &kept;
    let f: fn(u32) -> u32 = plain;
    let g = |y: u32| y + speaker.speak();
    f(g(3))
}
"#,
        &["drops"],
    );
}
