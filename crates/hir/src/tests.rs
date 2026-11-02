mod api;
mod budget;
mod bodies;
mod const_eval;
mod db;
mod infer;
mod lower;
mod trait_imports;
mod types;
mod visibilities;

use base_db::{RunOrder, all_crates, set_run_order};
use blaze_format::{Aligned, Persist, Record, Rel, Writer};
use hir_def::{
    AdtId, AssocItemId, ConstId, DefWithBodyId, FunctionId, ImplId, ModuleDefId, TraitId,
    VariantId,
    nameres::crate_def_map,
    signatures::{ConstSignature, FunctionSignature, StructSignature},
};
use hir_ty::{
    ColdBatch, CrateFieldTypes, CrateImplPredicates, CrateImplSelfTys, CrateImplTraits,
    GenericPredicates, InferBodyId, InferenceResult,
    consteval::{AllocationRec, read_allocation, write_allocation},
    db::HirDatabase,
    infer::persist::InferenceRecord,
    next_solver::{
        DbInterner, GenericArg, GenericArgs, ParamEnv, Ty, TypingMode,
        infer::{DbInternerInferExt, select::EvaluationResult},
        persist::{TyTableReader, TyTableWriter},
    },
};
use hir_ty_borrowck::{Checked, NotChecked, Refused};
use hir_expand::fixture;

use self::db::TestDB;

fn function(db: &TestDB, name: &str) -> DefWithBodyId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return FunctionId::into(it);
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

fn count(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| *it == query).count()
}

/// Linking hir is what links hir-ty-mir's table, which panics unless exactly one is linked.
#[test]
fn the_mir_providers_are_linked() {
    let mir = hir_ty::providers::mir();
    assert!(std::ptr::eq(mir, hir_ty::providers::mir()));
}

#[test]
fn check_infers_only_workspace_bodies() {
    let db = fixture::db(
        r#"
//- /dep.rs crate:dep library
pub struct S;
impl S {
    pub fn method(&self) -> u32 { let x = 3; x }
}
pub fn called() -> u32 { let x = 1; x }
pub fn not_called() -> u32 { let x = 2; x }
//- /main.rs crate:main deps:dep
fn main() {
    let _ = dep::called();
    let _ = dep::S.method();
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let workspace = crate::check::workspace_crates(&db);
        assert_eq!(workspace.len(), 1, "only `main` is a workspace member");
        let executed = db.log_executed(|| {
            crate::check::check_workspace(&db, false);
        });
        assert_eq!(count(&executed, "infer_body_query"), 1, "{executed:#?}");
    });
}

/// A crate names the dependencies its paths look up in the extern prelude, at the level of
/// items, in bodies and in what macros expand to, and those it names with `extern crate`:
/// not one a false `#[cfg]` leaves out, a local of the same name, or one never written.
#[test]
fn a_crate_names_the_dependencies_rustc_loads_for_it() {
    let db = fixture::db(
        r#"
//- /main.rs crate:main deps:imported,called,externed,mac,expanded,tokens,cfgd,value,unused
use imported::Item;
extern crate externed;
#[cfg(any())]
use cfgd as _;
mac::make!();
macro_rules! local {
    () => { tokens::T };
}
fn f() -> u32 {
    let value = called::g();
    value
}
//- /imported.rs crate:imported
pub struct Item;
//- /called.rs crate:called
pub fn g() -> u32 { 0 }
//- /externed.rs crate:externed
//- /mac.rs crate:mac
#[macro_export]
macro_rules! make {
    () => { pub use expanded::E; };
}
//- /expanded.rs crate:expanded
pub struct E;
//- /tokens.rs crate:tokens
//- /cfgd.rs crate:cfgd
//- /value.rs crate:value
//- /unused.rs crate:unused
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let krate = all_crates(&db)
            .iter()
            .map(|&it| crate::Crate::from(it))
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture's crate `main`");
        let mut named: Vec<String> = crate::named_dependencies(&db, krate)
            .into_iter()
            .map(|it| it.display_name(&db).expect("a fixture's crate has a name").to_string())
            .collect();
        named.sort();
        assert_eq!(named, ["called", "expanded", "externed", "imported", "mac", "tokens"]);
    });
}

const MODULES: &str = r#"
//- minicore: sized, copy
//- /main.rs crate:main
mod a;
mod b;
struct S;
fn main() {
    let _: u32 = S;
}
//- /a.rs
pub fn f() -> u32 {
    super::S
}
mod inner {
    fn g() {
        let _: bool = 1u8;
    }
}
//- /b.rs
pub fn h() {
    let _: () = 2u32;
    let _: u8 = super::S;
}
"#;

/// Each diagnostic's kind, in order: what stays the same between two databases of one fixture.
fn kinds(found: &[crate::AnyDiagnostic<'_>]) -> Vec<String> {
    found
        .iter()
        .map(|it| format!("{it:?}").split(['(', ' ']).next().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_check_is_the_same_whatever_order_its_modules_ran_in() {
    let check = |db: &TestDB| {
        crate::attach_db(db, || {
            crate::check::check_workspace(db, false)
                .iter()
                .map(|(_, found)| kinds(found))
                .collect::<Vec<_>>()
        })
    };
    let in_order = fixture::db(MODULES).into_db();
    let reversed = fixture::db(MODULES).into_db();
    let before = set_run_order(RunOrder::InOrder);
    let found = check(&in_order);
    set_run_order(RunOrder::Reversed);
    let found_reversed = check(&reversed);
    set_run_order(before);
    assert_eq!(found.len(), 1, "one workspace crate");
    assert!(
        found[0].iter().filter(|it| *it == "TypeMismatch").count() >= 4,
        "every module's mismatches are there: {found:?}"
    );
    assert_eq!(found, found_reversed);
}

#[test]
fn a_check_of_a_crate_is_its_modules_checked_one_by_one() {
    let db = fixture::db(MODULES).into_db();
    let before = set_run_order(RunOrder::Reversed);
    crate::attach_db(&db, || {
        let krate = crate::check::workspace_crates(&db)[0];
        let modules = crate::check::file_modules(&db, krate);
        assert_eq!(modules.len(), 3, "`main`, `a` and `b`; `a::inner` goes with `a`");
        let one_by_one: Vec<_> = modules
            .iter()
            .flat_map(|&it| crate::check::check_module(&db, it, false, &crate::check::every_body))
            .collect();
        let batch = crate::check::check_crate(&db, krate, false, &crate::check::every_body);
        assert_eq!(kinds(&batch), kinds(&one_by_one));
        assert_eq!(format!("{batch:?}"), format!("{one_by_one:?}"));
    });
    set_run_order(before);
}

#[test]
fn a_check_of_some_bodies_leaves_the_others_out() {
    let db = fixture::db(MODULES).into_db();
    crate::attach_db(&db, || {
        let krate = crate::check::workspace_crates(&db)[0];
        let main = crate::DefWithBody::from(function(&db, "main"));
        let only_main = |it: crate::DefWithBody| it == main;
        let found = crate::check::check_crate(&db, krate, false, &only_main);
        assert_eq!(kinds(&found), ["TypeMismatch"], "{found:?}");
    });
}

const MOVED_TWICE: &str = r#"
//- minicore: sized, copy
struct S;
fn consume(_: S) {}
fn moved_twice() {
    let s = S;
    consume(s);
    consume(s);
}
fn moved_once() {
    let s = S;
    consume(s);
}
fn mut_unused() {
    let mut s = S;
    consume(s);
}
fn mismatched() -> u32 {
    S
}
"#;

#[test]
fn a_build_gets_no_mir_of_a_body_whose_borrow_check_failed() {
    let db = fixture::db(MOVED_TWICE).into_db();
    crate::attach_db(&db, || {
        let bad = function(&db, "moved_twice");
        match hir_ty_borrowck::checked(&db, bad) {
            Err(Refused::Errors { owner, errors }) => {
                assert_eq!(owner, bad);
                assert!(errors.iter().any(|it| it.code == "E0382"), "{errors:?}");
            }
            other => panic!("expected a borrowck error, got {other:?}"),
        }
        let good = function(&db, "moved_once");
        let checked = hir_ty_borrowck::checked(&db, good).unwrap();
        let mir = checked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(good))).unwrap();
        assert_eq!(mir.checked().owner(), good);
    });
}

/// A diagnostic of code with no syntax of its own is reported at the item whose body the code
/// is in, never dropped: the place `UnplacedDiagnostic` gets is the item's own node.
#[test]
fn an_unplaced_diagnostic_is_at_the_item_of_its_body() {
    use hir_def::{Lookup as _, src::HasSource as _};
    let db = fixture::db("fn f() -> u32 { 1 }").into_db();
    crate::attach_db(&db, || {
        let def = function(&db, "f");
        let DefWithBodyId::FunctionId(func) = def else { panic!("`f` is a function") };
        let (_, source_map) = hir_def::expr_store::Body::with_source_map(&db, def);
        let owner =
            crate::diagnostics::unplaced_owner(&db, InferBodyId::DefWithBodyId(def), source_map);
        let item: hir_expand::InFile<syntax::SyntaxNodePtr> =
            func.lookup(&db).ast_ptr(&db).map(Into::into);
        assert_eq!(owner, item);
    });
}

#[test]
fn a_lint_of_the_borrow_checker_lets_the_body_build() {
    let db = fixture::db(MOVED_TWICE).into_db();
    crate::attach_db(&db, || {
        let def = function(&db, "mut_unused");
        assert!(
            hir_ty_borrowck::borrowck(&db, def).diagnostics.iter().any(|it| !it.is_error()),
            "`unused_mut` is reported"
        );
        assert_eq!(hir_ty_borrowck::checked(&db, def).map(Checked::owner), Ok(def));
    });
}

#[test]
fn a_body_with_type_errors_is_refused_not_passed() {
    let db = fixture::db(MOVED_TWICE).into_db();
    crate::attach_db(&db, || {
        let def = function(&db, "mismatched");
        assert_eq!(
            hir_ty_borrowck::checked(&db, def),
            Err(Refused::NotChecked { owner: def, why: NotChecked::TypeErrors })
        );
    });
}

#[test]
fn a_body_match_checking_refused_gets_no_borrow_check_error() {
    let db = fixture::db(
        r#"
//- minicore: sized, copy
struct S;
fn consume(_: S) {}
fn refused(b: bool) {
    let s = S;
    consume(s);
    consume(s);
    match b {
        true => {}
    }
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let def = function(&db, "refused");
        let result = hir_ty_borrowck::borrowck(&db, def);
        assert_eq!(result.not_checked, Some(NotChecked::MatchErrors));
        assert!(result.diagnostics.is_empty(), "rustc reports E0004 alone: {:?}", result.diagnostics);
    });
}

#[test]
fn a_check_takes_no_mir_of_another_body() {
    let db = fixture::db(MOVED_TWICE).into_db();
    crate::attach_db(&db, || {
        let good = function(&db, "moved_once");
        let other = function(&db, "mut_unused");
        let checked = hir_ty_borrowck::checked(&db, good).unwrap();
        match checked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(other))) {
            Err(Refused::NotOf { owner, .. }) => assert_eq!(owner, good),
            other => panic!("expected the MIR to be refused, got {other:?}"),
        }
        let (unchecked, pending) = hir_ty_borrowck::speculate(good);
        assert!(matches!(
            unchecked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(other))),
            Err(Refused::NotOf { .. })
        ));
        assert_eq!(pending.finish(&db).map(Checked::owner), Ok(good));
    });
}

#[test]
fn a_live_patch_has_code_before_the_check_that_refuses_it() {
    let db = fixture::db(MOVED_TWICE).into_db();
    crate::attach_db(&db, || {
        let bad = function(&db, "moved_twice");
        let (unchecked, pending) = hir_ty_borrowck::speculate(bad);
        let executed = db.log_executed(|| {
            let mir = unchecked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(bad))).unwrap();
            assert_eq!(mir.unchecked().owner(), bad);
            assert!(!mir.basic_blocks.is_empty());
        });
        assert_eq!(count(&executed, "borrowck"), 0, "the code comes before the check");
        match pending.finish(&db) {
            Err(Refused::Errors { owner, errors }) => {
                assert_eq!(owner, bad);
                assert!(errors.iter().any(|it| it.code == "E0382"), "{errors:?}");
            }
            other => panic!("the patch must be rolled back, got {other:?}"),
        }
    });
}

#[test]
fn dropped_results_keep_cutoff_and_come_back_equal() {
    let fx = fixture::db(
        r#"
//- minicore: sized, copy
//- /main.rs crate:main
struct S;
fn consume(_: S) {}
fn evicted() -> u32 {
    let s = S;
    consume(s);
    1
}
//- /other.rs crate:other
pub fn g() {}
"#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let other = files
        .iter()
        .map(|it| it.file_id(&db))
        .find(|&it| db.file_text(it).text(&db).contains("pub fn g"))
        .unwrap();
    let f = crate::attach_db(&db, || function(&db, "evicted"));
    let before = crate::attach_db(&db, || {
        hir_ty_borrowck::borrowck(&db, f);
        let mir = db.mir_body(InferBodyId::DefWithBodyId(f)).unwrap();
        (mir.basic_blocks.len(), mir.locals.len())
    });

    crate::evict::drop_after_store(&mut db, &[f]);
    db.set_file_text(other, "pub fn g() {}\npub fn h() {}\n");

    crate::attach_db(&db, || {
        let executed = db.log_executed(|| {
            hir_ty_borrowck::borrowck(&db, f);
        });
        assert_eq!(count(&executed, "borrowck"), 0, "{executed:#?}");
        assert_eq!(count(&executed, "infer_body_query"), 0, "{executed:#?}");

        let executed = db.log_executed(|| {
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(f));
        });
        assert_eq!(count(&executed, "infer_body_query"), 1, "{executed:#?}");

        let executed = db.log_executed(|| {
            let mir = db.mir_body(InferBodyId::DefWithBodyId(f)).unwrap();
            assert_eq!((mir.basic_blocks.len(), mir.locals.len()), before);
        });
        assert_eq!(count(&executed, "mir_body_query"), 1, "{executed:#?}");
    });
}

const SOLVER: &str = r#"
//- minicore: sized
trait Tr {}
struct A;
struct B<T>(T);
struct C;
impl Tr for A {}
impl<T: Tr> Tr for B<T> {}
"#;

/// The item named `name` in the type namespace of a module of the fixture, and its crate.
fn type_item(db: &TestDB, name: &str) -> (base_db::Crate, ModuleDefId) {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for (it, per_ns) in module.scope(db).entries() {
                if it.as_str() == name
                    && let Some(def) = per_ns.take_types()
                {
                    return (krate, def);
                }
            }
        }
    }
    panic!("no type `{name}` in the fixture")
}

/// Whether each of `goals` implements `tr`, all asked of one inference context of `interner`.
fn implements<'db>(
    interner: DbInterner<'db>,
    tr: TraitId,
    goals: &[(Ty<'db>, bool)],
) -> Vec<EvaluationResult> {
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    goals.iter().map(|&(ty, _)| infcx.type_implements_trait(tr, [ty], ParamEnv::empty())).collect()
}

const HELD_ACROSS_AWAIT: &str = r#"
//- minicore: sized, send, future
struct NotSend(*const u8);
fn not_send() -> NotSend { loop {} }
fn consume(_: NotSend) {}
fn held() {
    let fut = async {
        let it = not_send();
        async {}.await;
        consume(it);
    };
}
fn moved_before() {
    let fut = async {
        consume(not_send());
        async {}.await;
    };
}
"#;

/// The type inference gave the binding `name` in the body of `def`.
fn binding_ty<'db>(db: &'db TestDB, def: DefWithBodyId, name: &str) -> Ty<'db> {
    let body = hir_def::expr_store::Body::of(db, def);
    let (binding, _) = body
        .bindings()
        .find(|(_, it)| it.name.as_str() == name)
        .unwrap_or_else(|| panic!("no binding `{name}`"));
    InferenceResult::of(db, InferBodyId::DefWithBodyId(def)).binding_ty(binding)
}

#[test]
fn a_future_is_send_only_when_what_it_holds_across_an_await_is() {
    let db = fixture::db(HELD_ACROSS_AWAIT).into_db();
    crate::attach_db(&db, || {
        let (_, ModuleDefId::TraitId(send)) = type_item(&db, "Send") else {
            panic!("`Send` is a trait")
        };
        for (name, expected) in [
            ("held", EvaluationResult::EvaluatedToErr),
            ("moved_before", EvaluationResult::EvaluatedToOk),
        ] {
            let def = function(&db, name);
            let interner = DbInterner::new_with(&db, hir_def::HasModule::krate(&def, &db));
            let fut = binding_ty(&db, def, "fut");
            assert_eq!(implements(interner, send, &[(fut, false)]), [expected], "{name}");
        }
    });
}

const SEND_REQUIRED_IN_ITS_BODY: &str = r#"
//- minicore: sized, send, future
struct NotSend(*const u8);
fn not_send() -> NotSend { loop {} }
fn consume(_: NotSend) {}
fn require_send<T: Send>(_: T) {}
fn held() {
    require_send(async {
        let it = not_send();
        async {}.await;
        consume(it);
    });
}
fn moved_before() {
    require_send(async {
        consume(not_send());
        async {}.await;
    });
}
"#;

/// rustc's `check_coroutine_obligations`: `Send` of a future the body defines stalls in
/// inference, on the future's witness, and is proved after it. Inference reports nothing; the
/// check refutes it when a `NotSend` is held across the `.await` and proves it when it is not.
#[test]
fn a_goal_stalled_on_the_body_s_own_future_is_checked_after_inference() {
    let db = fixture::db(SEND_REQUIRED_IN_ITS_BODY).into_db();
    crate::attach_db(&db, || {
        for (name, errors) in [("held", 1), ("moved_before", 0)] {
            let body = InferBodyId::DefWithBodyId(function(&db, name));
            let infer = InferenceResult::of(&db, body);
            assert!(!infer.coroutine_stalled_predicates().is_empty(), "{name}: `{{async}}: Send`");
            assert!(infer.diagnostics().is_empty(), "{name}: {:?}", infer.diagnostics());
            let found = hir_ty::coroutine_obligations::coroutine_obligation_errors(&db, body);
            assert_eq!(found.len(), errors, "{name}: {found:?}");
        }
    });
}

#[test]
fn a_solved_goal_answers_the_same_from_the_cache_and_on_another_thread() {
    let db = fixture::db(SOLVER).into_db();
    crate::attach_db(&db, || {
        let (krate, ModuleDefId::TraitId(tr)) = type_item(&db, "Tr") else {
            panic!("`Tr` is a trait")
        };
        let adt = |name| match type_item(&db, name).1 {
            ModuleDefId::AdtId(it) => it,
            _ => panic!("`{name}` is a struct"),
        };
        let (a, b, c) = (adt("A"), adt("B"), adt("C"));
        let interner = DbInterner::new_with(&db, krate);
        let leaf = |adt| Ty::new_adt(interner, adt, GenericArgs::empty());
        let wrap = |ty| Ty::new_adt(interner, b, GenericArgs::new_from_slice(&[GenericArg::from(ty)]));
        let goals = [
            (leaf(a), true),
            (leaf(c), false),
            (wrap(leaf(a)), true),
            (wrap(leaf(c)), false),
            (wrap(wrap(leaf(a))), true),
            (wrap(wrap(leaf(c))), false),
            (wrap(wrap(wrap(leaf(a)))), true),
        ];

        let cold: Vec<EvaluationResult> = goals
            .iter()
            .map(|goal| {
                implements(DbInterner::new_with(&db, krate), tr, std::slice::from_ref(goal))[0]
            })
            .collect();
        for (at, (&(_, holds), &answer)) in goals.iter().zip(&cold).enumerate() {
            let expected = if holds {
                EvaluationResult::EvaluatedToOk
            } else {
                EvaluationResult::EvaluatedToErr
            };
            assert_eq!(answer, expected, "goal {at}");
        }

        let warm = DbInterner::new_with(&db, krate);
        assert_eq!(implements(warm, tr, &goals), cold, "first asked");
        assert_eq!(implements(warm, tr, &goals), cold, "answered from the cache");
        let elsewhere = std::thread::scope(|scope| {
            scope.spawn(|| crate::attach_db(&db, || implements(warm, tr, &goals))).join().unwrap()
        });
        assert_eq!(elsewhere, cold, "answered on another thread");
    });
}

#[test]
fn a_goal_reused_from_another_query_reruns_it_when_what_the_goal_read_changes() {
    let fx = fixture::db(
        r#"
//- minicore: sized
//- /main.rs crate:main
mod imp;
pub trait Tr {}
pub trait Other {}
pub struct A;
pub struct B<T>(T);
fn needs<T: Tr>(_: T) {}
fn first() { needs(B(A)); }
fn second() { needs(B(A)); }
fn unrelated() { let x = 1; let _ = x; }
//- /imp.rs
use super::*;
impl Tr for A {}
impl<T> Tr for B<T> where T: Tr {}
"#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let imp = files
        .iter()
        .map(|it| it.file_id(&db))
        .find(|&it| db.file_text(it).text(&db).contains("impl Tr for A"))
        .unwrap();
    let [first, second, unrelated] =
        crate::attach_db(&db, || ["first", "second", "unrelated"].map(|it| function(&db, it)));
    crate::attach_db(&db, || {
        // `B<A>: Tr` is solved for `first`, then answered for `second` from the shared cache.
        for body in [first, second, unrelated] {
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(body));
        }
    });

    // Only the impl's where clause changes: no item, name or body `second` reads directly.
    db.set_file_text(imp, "use super::*;\nimpl Tr for A {}\nimpl<T> Tr for B<T> where T: Other {}\n");

    crate::attach_db(&db, || {
        let executed = db.log_executed(|| {
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(unrelated));
        });
        assert_eq!(count(&executed, "infer_body_query"), 0, "{executed:#?}");
        // `second` took the goal's reads with its answer, so it reruns as `first` does.
        for body in [second, first] {
            let executed = db.log_executed(|| {
                InferenceResult::of(&db, InferBodyId::DefWithBodyId(body));
            });
            assert_eq!(count(&executed, "infer_body_query"), 1, "{executed:#?}");
        }
    });
}

const BORROWCK: &str = r#"
//- minicore: sized, copy
struct S;
fn consume(_: S) {}
fn read(_: &S) {}
fn make() -> S { S }

fn use_after_move() {
    let s = S;
    consume(s);
    consume(s);
}
fn two_mutable_borrows() {
    let mut s = S;
    let a = &mut s;
    let b = &mut s;
    let _ = (a, b);
}
fn mutable_while_shared() {
    let mut s = S;
    let a = &s;
    let b = &mut s;
    read(a);
    let _ = b;
}
fn move_while_borrowed() {
    let s = S;
    let a = &s;
    consume(s);
    read(a);
}
fn assign_while_borrowed() {
    let mut x = 1u32;
    let a = &x;
    x = 2;
    let _ = *a;
}
fn does_not_live_long_enough() {
    let r;
    {
        let s = S;
        r = &s;
    }
    read(r);
}
fn temporary_dropped() {
    let r: &S;
    r = &make();
    read(r);
}
fn needless_mut() {
    let mut s = S;
    consume(s);
}
fn in_order() {
    let mut s = S;
    let a = &s;
    let b = &mut s;
    read(a);
    let _ = b;
    consume(s);
    consume(s);
}
"#;

const MOVED: (&str, &str) = ("E0382", "use of moved value: `s`");
const MUTABLE_WHILE_SHARED: (&str, &str) =
    ("E0502", "cannot borrow `s` as mutable because it is also borrowed as immutable");

/// The borrow check of `BORROWCK`'s `name` gives exactly `expected`, rustc's codes and primary
/// messages in rustc's order, and the gate refuses it unless all of them are lints.
fn borrowck_gives(name: &str, expected: &[(&str, &str)]) {
    let db = fixture::db(BORROWCK).into_db();
    crate::attach_db(&db, || {
        let def = function(&db, name);
        let found: Vec<(&str, String)> = hir_ty_borrowck::borrowck(&db, def)
            .diagnostics
            .iter()
            .map(|it| (it.code, it.message.clone()))
            .collect();
        let expected: Vec<(&str, String)> =
            expected.iter().map(|&(code, message)| (code, message.to_owned())).collect();
        assert_eq!(found, expected, "{found:?}");
        let errors = expected.iter().any(|(code, _)| code.starts_with('E'));
        match hir_ty_borrowck::checked(&db, def) {
            Ok(_) => assert!(!errors, "`{name}` has errors but was let through"),
            Err(Refused::Errors { owner, .. }) => {
                assert!(errors, "`{name}` has only lints but was refused");
                assert_eq!(owner, def);
            }
            Err(other) => panic!("`{name}` must be refused for its errors, got {other:?}"),
        }
    });
}

#[test]
fn borrowck_use_after_move() {
    borrowck_gives("use_after_move", &[MOVED]);
}

#[test]
fn borrowck_two_mutable_borrows() {
    borrowck_gives(
        "two_mutable_borrows",
        &[("E0499", "cannot borrow `s` as mutable more than once at a time")],
    );
}

#[test]
fn borrowck_mutable_while_shared() {
    borrowck_gives("mutable_while_shared", &[MUTABLE_WHILE_SHARED]);
}

#[test]
fn borrowck_move_while_borrowed() {
    borrowck_gives(
        "move_while_borrowed",
        &[("E0505", "cannot move out of `s` because it is borrowed")],
    );
}

#[test]
fn borrowck_assign_while_borrowed() {
    borrowck_gives(
        "assign_while_borrowed",
        &[("E0506", "cannot assign to `x` because it is borrowed")],
    );
}

#[test]
fn borrowck_does_not_live_long_enough() {
    borrowck_gives("does_not_live_long_enough", &[("E0597", "`s` does not live long enough")]);
}

#[test]
fn borrowck_temporary_dropped() {
    borrowck_gives("temporary_dropped", &[("E0716", "temporary value dropped while borrowed")]);
}

#[test]
fn borrowck_needless_mut() {
    borrowck_gives("needless_mut", &[("unused_mut", "variable does not need to be mutable")]);
}

#[test]
fn borrowck_in_order() {
    borrowck_gives("in_order", &[MUTABLE_WHILE_SHARED, MOVED]);
}

/// The same signatures, written in `dep` (a library: batched) and in `main` (a query each).
const SIGNATURES: &str = "
pub trait Tr {}
pub struct P<T> { pub a: u32, pub b: T, pub c: (T, u8) }
pub struct Q<T>(pub T, pub bool);
pub enum E<T> { One(T), Two { x: u16, y: *const T } }
impl<T> P<T> { pub fn get(&self) -> u32 { self.a } }
impl<T> Tr for Q<T> where T: Tr {}
";

fn signatures_db() -> TestDB {
    fixture::db(&format!(
        "//- /dep.rs crate:dep library\n{SIGNATURES}//- /main.rs crate:main\n{SIGNATURES}"
    ))
    .into_db()
}

/// `dep` and `main` of [`signatures_db`].
fn dep_and_main(db: &TestDB) -> (base_db::Crate, base_db::Crate) {
    let crates = all_crates(db);
    let of =
        |local: bool| *crates.iter().find(|it| it.data(db).origin.is_local() == local).unwrap();
    (of(false), of(true))
}

/// The variants of `krate`, each with its name, and its impls, in def-map order.
fn signatures(db: &TestDB, krate: base_db::Crate) -> (Vec<(String, VariantId)>, Vec<ImplId>) {
    let (mut variants, mut impls) = (Vec::new(), Vec::new());
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    let name = StructSignature::of(db, it).name.as_str().to_owned();
                    variants.push((name, it.into()));
                }
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    for (name, &(variant, _)) in &it.enum_variants(db).variants {
                        variants.push((name.as_str().to_owned(), variant.into()));
                    }
                }
                _ => {}
            }
        }
        impls.extend(scope.impls());
    }
    (variants, impls)
}

/// What `variants` and `impls` lower to, printed: each variant's field types, then each impl's
/// self type, trait and predicates. ADTs and traits print by name, parameters by index.
fn lowered(db: &TestDB, variants: &[(String, VariantId)], impls: &[ImplId]) -> Vec<String> {
    let mut out = Vec::with_capacity(variants.len() + impls.len());
    for (name, variant) in variants {
        let fields: Vec<Ty<'_>> =
            db.field_types(*variant).values().map(|it| it.ty().skip_binder()).collect();
        out.push(format!("{name}: {fields:?}"));
    }
    for &it in impls {
        let predicates: Vec<_> =
            GenericPredicates::query_all(db, it.into()).skip_binder().collect();
        out.push(format!(
            "{:?} {:?} {predicates:?}",
            db.impl_self_ty(it).skip_binder(),
            db.impl_trait(it).map(|it| it.skip_binder()),
        ));
    }
    out
}

const BATCHES: [&str; 4] =
    ["crate_field_types", "crate_impl_self_tys", "crate_impl_traits", "crate_impl_predicates"];

#[test]
fn a_dependency_batch_is_what_the_per_item_lowering_gives() {
    // Every write ends in the maintainers, and hir-ty's impls maintainer lowers each impl's
    // self type and trait there, so the log starts at the write that fills both crates.
    let fx = fixture::db("//- /dep.rs crate:dep library\n//- /main.rs crate:main\n");
    let files = [fx.file_at("/dep.rs"), fx.file_at("/main.rs")].map(|it| it.file_id(fx.db()));
    let mut db = fx.into_db();
    let written = db.log_mut(|db| {
        db.batch(|db| {
            for file in files {
                db.set_file_text(file, SIGNATURES);
            }
        })
    });
    let mut executed: Vec<String> = written
        .into_iter()
        .filter_map(|event| match event.kind {
            blaze_query::db::EventKind::WillExecute { database_key } => Some(
                blaze_query::db::Database::storage(&db)
                    .ingredient_name(database_key.ingredient_index())
                    .to_owned(),
            ),
            _ => None,
        })
        .collect();
    crate::attach_db(&db, || {
        let (dep, main) = dep_and_main(&db);
        let (mut batched, mut per_item) = (Vec::new(), Vec::new());
        let (mut dep_variants, mut dep_impls) = (Vec::new(), Vec::new());
        let (mut main_variants, mut main_impls) = (Vec::new(), Vec::new());
        executed.extend(db.log_executed(|| {
            (dep_variants, dep_impls) = signatures(&db, dep);
            (main_variants, main_impls) = signatures(&db, main);
            batched = lowered(&db, &dep_variants, &dep_impls);
            per_item = lowered(&db, &main_variants, &main_impls);
        }));
        let names = |variants: &[(String, VariantId)]| -> Vec<String> {
            variants.iter().map(|(name, _)| name.clone()).collect()
        };
        assert_eq!(names(&dep_variants), ["P", "Q", "One", "Two"]);
        assert_eq!(names(&dep_variants), names(&main_variants));
        assert_eq!(dep_impls.len(), 2);
        assert_eq!(main_impls.len(), 2);
        assert_eq!(batched, per_item);
        for batch in BATCHES {
            assert_eq!(count(&executed, batch), 1, "{batch}: {executed:#?}");
        }
        assert_eq!(
            count(&executed, "field_types_of_variant"),
            main_variants.len(),
            "{executed:#?}"
        );
        assert_eq!(count(&executed, "impl_self_ty_of_impl"), main_impls.len(), "{executed:#?}");
        assert_eq!(count(&executed, "impl_trait_of_impl"), main_impls.len(), "{executed:#?}");
    });
}

/// `cold` written twice to equal bytes, and read back from an aligned copy.
fn round_trip(cold: &ColdBatch, again: &ColdBatch) -> Aligned {
    let bytes = blaze_format::to_bytes(cold);
    assert_eq!(bytes, blaze_format::to_bytes(again), "equal batches are equal bytes");
    Aligned::copy_from(&bytes)
}

#[test]
fn a_cold_batch_round_trips_with_its_types() {
    let db = signatures_db();
    crate::attach_db(&db, || {
        let (dep, _) = dep_and_main(&db);
        let interner = DbInterner::new_with(&db, dep);

        let batch = CrateFieldTypes::of(&db, dep);
        assert!(!batch.is_empty());
        let buffer = round_trip(
            &ColdBatch::of_field_types(interner, batch),
            &ColdBatch::of_field_types(interner, batch),
        );
        assert_eq!(&ColdBatch::read(&buffer).unwrap().field_types(interner).unwrap(), batch);

        let batch = CrateImplSelfTys::of(&db, dep);
        let buffer = round_trip(
            &ColdBatch::of_impl_self_tys(interner, batch),
            &ColdBatch::of_impl_self_tys(interner, batch),
        );
        assert_eq!(&ColdBatch::read(&buffer).unwrap().impl_self_tys(interner).unwrap(), batch);

        let batch = CrateImplTraits::of(&db, dep);
        let buffer = round_trip(
            &ColdBatch::of_impl_traits(interner, batch),
            &ColdBatch::of_impl_traits(interner, batch),
        );
        assert_eq!(&ColdBatch::read(&buffer).unwrap().impl_traits(interner).unwrap(), batch);

        let batch = CrateImplPredicates::of(&db, dep);
        let buffer = round_trip(
            &ColdBatch::of_impl_predicates(interner, batch),
            &ColdBatch::of_impl_predicates(interner, batch),
        );
        assert_eq!(&ColdBatch::read(&buffer).unwrap().impl_predicates(interner).unwrap(), batch);
    });
}

#[test]
fn a_workspace_crate_has_no_batch() {
    let db = signatures_db();
    crate::attach_db(&db, || {
        let (_, main) = dep_and_main(&db);
        let (variants, impls) = signatures(&db, main);
        let executed = db.log_executed(|| {
            lowered(&db, &variants, &impls);
        });
        for batch in BATCHES {
            assert_eq!(count(&executed, batch), 0, "{batch}: {executed:#?}");
        }
    });
}

blaze_format::record! {
    /// A buffer of one allocation and the type table its types are in.
    #[derive(Clone, Copy)]
    struct AllocationRoot: 1 {
        table: u64,
        allocation: Rel<AllocationRec>,
        reserved: u32,
    }
}

/// The constant named `name` in a module of the fixture.
fn constant(db: &TestDB, name: &str) -> ConstId {
    for &krate in all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::ConstId(it) = def
                    && ConstSignature::of(db, it)
                        .name
                        .as_ref()
                        .is_some_and(|found| found.as_str() == name)
                {
                    return it;
                }
            }
        }
    }
    panic!("no constant `{name}` in the fixture")
}

#[test]
fn allocations_read_back_as_written() {
    let db = fixture::db(
        r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait T { fn f(&self) -> u8; }
impl T for u8 { fn f(&self) -> u8 { *self } }
const S: &str = "text";
const D: &dyn T = &7u8;
const P: (&[u16], u8) = (&[1, 2, 3], 4);
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let interner = DbInterner::new_no_crate(&db);
        for name in ["S", "D", "P"] {
            let value = db
                .const_eval(constant(&db, name), GenericArgs::empty(), None)
                .unwrap_or_else(|error| panic!("`{name}` evaluates: {error:?}"));

            let mut out = Writer::new(<AllocationRoot as Record>::SCHEMA);
            let mut tys = TyTableWriter::new(interner);
            let allocation = write_allocation(&mut out, &mut tys, value);
            let table = tys.write(&mut out);
            let at = out.next::<AllocationRoot>();
            let field = at + std::mem::offset_of!(AllocationRoot, allocation) as u64;
            let root = out.push(&AllocationRoot {
                table,
                allocation: Rel::between(field, allocation),
                reserved: 0,
            });
            let buffer = Aligned::copy_from(&out.finish(root));

            let root = blaze_format::open::<AllocationRoot>(&buffer).unwrap();
            let record = root.allocation.follow(&buffer).unwrap().unwrap();
            let tys = TyTableReader::open(interner, &buffer, root.table).unwrap();
            let read_back = read_allocation(record, &buffer, &tys).unwrap();
            assert!(read_back == value, "`{name}` reads back as {read_back:?}, not {value:?}");
        }
    });
}

/// Every function body of the workspace crates, impl methods too, in def-map order.
fn bodies(db: &TestDB) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    for krate in crate::check::workspace_crates(db) {
        for (_, module) in crate_def_map(db, krate.id).modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                if let ModuleDefId::FunctionId(it) = def {
                    bodies.push(it.into());
                }
            }
            for impl_id in scope.impls() {
                for &(_, item) in &impl_id.impl_items(db).items {
                    if let AssocItemId::FunctionId(it) = item {
                        bodies.push(it.into());
                    }
                }
            }
        }
    }
    bodies
}

#[test]
fn an_inference_record_round_trips_through_its_type_table() {
    let db = fixture::db(
        r#"
//- minicore: deref, coerce_unsized, fn, sized, index, slice
struct S { a: u32, b: (u8, bool) }
impl S { fn get<T: Default>(&self) -> T { T::default() } }
trait Tr { fn m(&self) -> u32; }
impl Tr for u32 { fn m(&self) -> u32 { *self } }
fn opaque() -> impl Tr { 5u32 }
fn f(s: &S, v: &mut Vec<u32>) -> u32 {
    let x: u8 = s.get::<u8>();
    let (ref mut y, _) = s.b;
    let slice: &[u32] = &[1, 2, 3];
    let mut count = 0;
    let mut add = |n: u32| count += n + s.a;
    add(1);
    let moved = move || x;
    match &s.b { (0, flag) => { let _: &bool = flag; } _ => {} }
    let k = const { 3 + 4 };
    s.missing();
    let wrong: bool = 1u32;
    opaque().m() + slice[0] + moved() as u32 + k
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let interner = DbInterner::new_no_crate(&db);
        let bodies = bodies(&db);
        assert_eq!(bodies.len(), 4, "`get`, `m`, `opaque` and `f`");
        for def in bodies {
            let result = InferenceResult::of(&db, def);
            let record = result.to_record(interner);
            let bytes = Aligned::copy_from(&blaze_format::to_bytes(&record));
            let read = <InferenceRecord as Persist>::read(&bytes).unwrap();
            assert_eq!(read.to_result(interner).unwrap(), *result);
            assert_eq!(record.as_ref().to_result(interner).unwrap(), *result);
        }
    });
}
