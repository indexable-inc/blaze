//! Which calls of const bodies are not const, through const traits' effects. Each fixture's
//! accept/refuse matches what nightly-2026-10-27 says of the same program.

use hir_def::{
    DefWithBodyId, ModuleDefId,
    signatures::{ConstSignature, FunctionSignature, TraitSignature},
};
use hir_expand::fixture;

use crate::{
    display::{DisplayTarget, HirDisplay},
    effects::{NonConst, non_const_calls},
    test_db::TestDBExt as _,
};

/// Checks the non-const calls of the crate root's fns and consts, one line per call:
/// `item: what`.
#[track_caller]
fn check(#[rust_analyzer::rust_fixture] ra_fixture: &str, expected: &str) {
    let fx = fixture::db(ra_fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let target = DisplayTarget::from_crate(&db, module_id.krate(&db));
        let mut lines = Vec::new();
        for decl in def_map.module(&db, module_id).scope(&db).declarations() {
            let (def, name): (DefWithBodyId, _) = match decl {
                ModuleDefId::FunctionId(it) => {
                    (it.into(), FunctionSignature::of(&db, it).name.as_str().to_owned())
                }
                ModuleDefId::ConstId(it) => match &ConstSignature::of(&db, it).name {
                    Some(name) => (it.into(), name.as_str().to_owned()),
                    None => continue,
                },
                _ => continue,
            };
            for (_, error) in non_const_calls(&db, def) {
                let what = match error {
                    NonConst::Fn(callee) => {
                        format!(
                            "`{}` is not const",
                            FunctionSignature::of(&db, callee).name.as_str()
                        )
                    }
                    NonConst::FnPointer => "fn pointer call".to_owned(),
                    NonConst::Closure => "call of a closure that is not const".to_owned(),
                    NonConst::Unsatisfied(callee, bounds) => {
                        let bounds = bounds
                            .iter()
                            .map(|bound| {
                                let bound = bound.skip_binder();
                                format!(
                                    "{}: [const] {}",
                                    bound.self_ty().display(&db, target),
                                    TraitSignature::of(&db, bound.def_id.0).name.as_str()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!(
                            "`{}` needs {bounds}",
                            FunctionSignature::of(&db, callee).name.as_str()
                        )
                    }
                };
                lines.push(format!("{name}: {what}"));
            }
        }
        lines.sort();
        assert_eq!(lines.join("\n"), expected.trim());
    });
}

#[test]
fn const_bound_makes_method_call_const() {
    check(
        r#"
const trait Tr { fn m(&self) -> u8; }
struct S;
const impl Tr for S { fn m(&self) -> u8 { 1 } }
struct U;
impl Tr for U { fn m(&self) -> u8 { 2 } }

const fn maybe<T: [const] Tr>(t: &T) -> u8 { t.m() }
const fn plain<T: Tr>(t: &T) -> u8 { t.m() }
const fn tilde<T: ~const Tr>(t: &T) -> u8 { t.m() }
fn runtime<T: Tr>(t: &T) -> u8 { t.m() }

const OF_S: u8 = S.m();
const OF_U: u8 = U.m();
const MAYBE_S: u8 = maybe(&S);
const MAYBE_U: u8 = maybe(&U);
"#,
        r#"
MAYBE_U: `maybe` needs U: [const] Tr
OF_U: `m` needs U: [const] Tr
plain: `m` needs T: [const] Tr
"#,
    );
}

#[test]
fn trait_that_is_not_const() {
    check(
        r#"
trait Tr { fn m(&self) -> u8; }
struct S;
impl Tr for S { fn m(&self) -> u8 { 1 } }
fn helper() -> u8 { 3 }
const fn f() -> u8 { helper() }
const C: u8 = S.m();
"#,
        r#"
C: `m` is not const
f: `helper` is not const
"#,
    );
}

#[test]
fn const_supertrait_through_subtrait_bound() {
    check(
        r#"
const trait Sup { fn up(&self) -> u8; }
const trait Sub: [const] Sup { fn down(&self) -> u8; }
const trait Loose: Sup {}
const fn f<T: [const] Sub>(t: &T) -> u8 { t.up() + t.down() }
const fn g<T: [const] Loose>(t: &T) -> u8 { t.up() }
"#,
        r#"
g: `up` needs T: [const] Sup
"#,
    );
}

#[test]
fn generic_const_impl_with_const_condition() {
    check(
        r#"
const trait Tr { fn m(&self) -> u8; }
struct S;
const impl Tr for S { fn m(&self) -> u8 { 1 } }
struct U;
impl Tr for U { fn m(&self) -> u8 { 2 } }
struct W<T>(T);
const impl<T: [const] Tr> Tr for W<T> { fn m(&self) -> u8 { self.0.m() } }
const OF_S: u8 = W(S).m();
const OF_U: u8 = W(U).m();
"#,
        r#"
OF_U: `m` needs W<U>: [const] Tr
"#,
    );
}

#[test]
fn const_closures() {
    check(
        r#"
//- minicore: fn, copy
const fn apply<F: [const] Fn(u8) -> u8>(f: F) -> u8 { f(1) }
const BARE: u8 = (const |x: u8| x + 1)(1);
const PASSED: u8 = apply(const |x| x * 2);
const NOT_CONST: u8 = apply(|x| x * 3);
const CALLED: u8 = (|x: u8| x)(1);
"#,
        r#"
CALLED: call of a closure that is not const
NOT_CONST: `apply` needs impl Fn(u8) -> u8: [const] Fn
"#,
    );
}

#[test]
fn fn_pointer_and_fn_item_calls() {
    check(
        r#"
//- minicore: fn
const fn one() -> u8 { 1 }
fn two() -> u8 { 2 }
const fn apply<F: [const] Fn() -> u8>(f: F) -> u8 { f() }
const fn through_pointer(f: fn() -> u8) -> u8 { f() }
const ITEM: u8 = apply(one);
const NOT_CONST_ITEM: u8 = apply(two);
"#,
        r#"
NOT_CONST_ITEM: `apply` needs fn two() -> u8: [const] Fn
through_pointer: fn pointer call
"#,
    );
}
