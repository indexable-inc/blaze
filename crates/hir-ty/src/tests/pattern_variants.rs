//! blaze: pattern types over enum variants (`#![blaze::pattern_types]`,
//! docs/v2/plan/design/edition.md, section 8): `Conn is Conn::Open { .. }` lowers to a
//! refinement of `Conn`, widens to `Conn` and to a larger set for free and never narrows
//! implicitly, a plain local is narrowed in a match arm, an `if let` and after a `let else`, its
//! one variant's fields read directly, and its whole value is never written through `&mut`.

use base_db::{Db, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ModuleDefId, VariantId, expr_store::Body, hir::Expr,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature},
};
use hir_expand::fixture;
use rustc_type_ir::inherent::IntoKind;

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult, PatternTypeUnsupported,
    TyLoweringDiagnostic,
    db::HirDatabase,
    next_solver::{Ty, TyKind},
    pattern_types,
};

const GATED: &str = r#"
//- minicore: sized
#![blaze::pattern_types]
enum Conn { Open { socket: u32 }, Closed, Pending(u8) }
enum Other { A, B }
struct S { x: u8 }

fn send(c: &(Conn is Conn::Open { .. })) -> u32 { c.socket }

fn in_the_arm(c: Conn) -> u32 {
    match c {
        Conn::Open { .. } => send(&c),
        _ => 0,
    }
}
fn in_an_if_let(c: Conn) -> u32 {
    if let Conn::Open { .. } = c { send(&c) } else { 0 }
}
fn after_a_let_else(c: Conn) -> u32 {
    let Conn::Open { .. } = c else { return 0 };
    send(&c)
}
fn through_a_shared_reference(c: &Conn) -> u32 {
    match c {
        Conn::Open { .. } => send(c),
        _ => 0,
    }
}
fn outside_the_arm(c: Conn) -> u32 {
    match c {
        Conn::Open { .. } => {}
        _ => {}
    }
    send(&c)
}
fn a_mut_local_is_not_narrowed(mut c: Conn) -> u32 {
    match c {
        Conn::Open { .. } => send(&c),
        _ => 0,
    }
}
fn widens_to_the_enum(c: Conn is Conn::Open { .. }) {
    let _w: Conn = c;
}
fn widens_to_a_larger_set(c: Conn is Conn::Open { .. }) {
    let _w: Conn is Conn::Open { .. } | Conn::Closed = c;
}
fn widens_behind_a_shared_reference(c: &(Conn is Conn::Open { .. })) {
    let _w: &Conn = c;
}
fn never_narrows_implicitly(c: Conn is Conn::Open { .. } | Conn::Closed) {
    let _n: Conn is Conn::Open { .. } = c;
}
fn a_unique_reference_does_not_widen(r: &mut (Conn is Conn::Open { .. })) {
    let _w: &mut Conn = r;
}
fn a_field_is_written(r: &mut (Conn is Conn::Open { .. })) {
    r.socket = 1;
}
fn the_whole_value_is_not_written(
    r: &mut (Conn is Conn::Open { .. }),
    s: Conn is Conn::Open { .. },
) {
    *r = s;
}
fn the_canonical_form(
    a: Conn is Conn::Closed | Conn::Open { .. },
    b: Conn is Conn::Open { .. } | Conn::Closed | Conn::Open { .. },
) {
    let _same: Conn is Conn::Open { .. } | Conn::Closed = a;
    let _again: Conn is Conn::Closed | Conn::Open { .. } = b;
}

struct Refused {
    not_a_variant: Conn is Other::A,
    a_field_pattern: Conn is Conn::Pending(1),
    a_wrong_shape: Conn is Conn::Closed(..),
    of_a_struct: S is S { .. },
    every_variant: Conn is Conn::Open { .. } | Conn::Closed | Conn::Pending(_),
}
"#;

const UNGATED: &str = r#"
//- minicore: sized
enum Conn { Open { socket: u32 }, Closed, Pending(u8) }

fn in_the_arm(c: Conn) -> u32 {
    match c {
        Conn::Open { socket } => { let _d = &c; socket }
        _ => 0,
    }
}

struct Refused {
    needs_the_gate: Conn is Conn::Open { .. },
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

/// Of the fixture's function `name`: its inference diagnostics, whether it has a type
/// mismatch, and how many of its paths read a refinement.
fn infer(fixture: &str, name: &str) -> (Vec<InferenceDiagnostic>, bool, usize) {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, name));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        let refined = body
            .exprs()
            .filter(|(expr, it)| {
                matches!(it, Expr::Path(_))
                    && pattern_types::refinement(peel_ref(infer.expr_ty(*expr))).is_some()
            })
            .count();
        (infer.diagnostics().to_vec(), infer.has_type_mismatches(), refined)
    })
}

/// `ty` through one reference, where a narrowed `&Conn` holds its refinement.
fn peel_ref(ty: Ty<'_>) -> Ty<'_> {
    match ty.kind() {
        TyKind::Ref(_, inner, _) => inner,
        _ => ty,
    }
}

/// Why each field of the fixture's struct `Refused` did not lower.
fn refused(fixture: &str) -> Vec<PatternTypeUnsupported> {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let refused = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::StructId(it))
                    if StructSignature::of(&db, it).name.as_str() == "Refused" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `Refused`");
        db.field_types_with_diagnostics(VariantId::StructId(refused))
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                TyLoweringDiagnostic::PatternTypeNotLowered { reason, .. } => Some(*reason),
                _ => None,
            })
            .collect()
    })
}

#[track_caller]
fn passes(name: &str) {
    let (diagnostics, mismatch, _) = infer(GATED, name);
    assert!(diagnostics.is_empty() && !mismatch, "`{name}`: {diagnostics:?}, mismatch {mismatch}");
}

#[track_caller]
fn mismatches(name: &str) {
    let (_, mismatch, _) = infer(GATED, name);
    assert!(mismatch, "`{name}` is a type mismatch");
}

#[test]
fn a_narrowed_local_is_passed_with_no_second_match() {
    for name in ["in_the_arm", "in_an_if_let", "after_a_let_else", "through_a_shared_reference"] {
        passes(name);
        let (_, _, refined) = infer(GATED, name);
        assert!(refined > 0, "`{name}` reads its local narrowed");
    }
}

#[test]
fn a_local_outside_the_arm_or_declared_mut_is_not_narrowed() {
    mismatches("outside_the_arm");
    mismatches("a_mut_local_is_not_narrowed");
}

#[test]
fn a_refinement_reads_its_one_variants_field() {
    passes("send");
    passes("a_field_is_written");
}

#[test]
fn widening_is_free_and_narrowing_never_implicit() {
    passes("widens_to_the_enum");
    passes("widens_to_a_larger_set");
    passes("widens_behind_a_shared_reference");
    passes("the_canonical_form");
    mismatches("never_narrows_implicitly");
    mismatches("a_unique_reference_does_not_widen");
}

#[test]
fn the_whole_value_is_not_written_through_a_unique_reference() {
    let (diagnostics, _, _) = infer(GATED, "the_whole_value_is_not_written");
    assert!(
        matches!(&diagnostics[..], [InferenceDiagnostic::WholeValueWriteThroughRefinement { .. }]),
        "{diagnostics:?}"
    );
}

#[test]
fn a_pattern_that_is_not_a_set_of_variants_is_refused_naming_why() {
    assert_eq!(
        refused(GATED),
        [
            PatternTypeUnsupported::NotAVariant,
            PatternTypeUnsupported::VariantFieldNotWildcard,
            PatternTypeUnsupported::VariantShape,
            PatternTypeUnsupported::VariantsOfNonEnum,
        ],
        "every variant is the enum itself, with nothing refused"
    );
}

#[test]
fn a_crate_without_the_gate_is_unchanged() {
    let (diagnostics, mismatch, refined) = infer(UNGATED, "in_the_arm");
    assert!(diagnostics.is_empty() && !mismatch, "{diagnostics:?}");
    assert_eq!(refined, 0, "nothing is narrowed without `#![blaze::pattern_types]`");
    assert_eq!(refused(UNGATED), [PatternTypeUnsupported::VariantsNeedGate]);
}
