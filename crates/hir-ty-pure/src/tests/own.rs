//! A body's own effects (`crate::own`): the digest is the same in two databases of one source,
//! stays when only a callee's body changes (while the caller's transitive verdict moves), moves
//! when the body gains a call or an effect of its own, and the walk's summary is the fold of
//! the own parts over a cycle.

use base_db::Db;
use blaze_ids::ContentId;
use hir_ty::{facts::Proofs, next_solver::GenericArgs};
use hir_ty_mir::mir::dispatch::{self, Body};

use super::{
    fixture::{crate_named, db_with, give_build},
    fns::fn_named,
};
use crate::{
    Rule, instance_effects, own_effects,
    walk::{Cx, Def, Instance, fold, own_part, summary_in},
};

/// The database of `crates` with the build given, as a driver makes it.
fn db(crates: &str) -> Db {
    let mut db = db_with(crates);
    give_build(&mut db);
    db
}

/// The digest of the own effects of the non-generic `name` at the root of `kernel`, and
/// whether its transitive summary (the walk's, callees folded in) reads a variable.
#[track_caller]
fn digest_and_verdict(crates: &str, name: &str) -> (ContentId, bool) {
    let db = db(crates);
    blaze_memo::attach(&db, || {
        let krate = crate_named(&db, "kernel");
        let func = fn_named(&db, krate, name);
        let own = own_effects(&db, func).unwrap_or_else(|what| panic!("{what}"));
        let args = GenericArgs::new_from_slice(&[]).store();
        let reads = instance_effects(&db, krate, Body::Fn(func), args).has(Rule::Env);
        (own.digest(), reads)
    })
}

const CALLER: &str = r#"
//- /lib.rs crate:kernel deps:std
fn callee() -> u32 {
    1
}

pub fn f() -> u32 {
    callee() + 2
}
"#;

#[test]
fn two_databases_of_one_source_give_one_digest() {
    let (one, _) = digest_and_verdict(CALLER, "f");
    let (two, _) = digest_and_verdict(CALLER, "f");
    assert_eq!(one, two);
    // The control: another body in the same databases has another digest, so the equality
    // above is not of a constant.
    let (callee, _) = digest_and_verdict(CALLER, "callee");
    assert_ne!(one, callee, "`f` and `callee` have different own effects");
}

#[test]
fn a_callee_body_edit_leaves_the_caller_digest_while_its_verdict_moves() {
    let impure_callee =
        CALLER.replace("    1\n", "    match std::env::var(\"HOME\") {\n        Ok(_) => 1,\n        Err(_) => 0,\n    }\n");
    assert_ne!(impure_callee, CALLER, "the edit applies");
    let (before, read_before) = digest_and_verdict(CALLER, "f");
    let (after, read_after) = digest_and_verdict(&impure_callee, "f");
    // The control: the transitive summary sees the callee's new effect.
    assert!(!read_before && read_after, "the callee's env read reaches `f`'s summary");
    assert_eq!(before, after, "no part of the callee's body is in `f`'s own effects");
}

#[test]
fn the_body_gaining_a_call_or_an_effect_of_its_own_moves_the_digest() {
    let (before, _) = digest_and_verdict(CALLER, "f");
    let call = CALLER.replace("callee() + 2", "callee() + callee()");
    let effect = CALLER.replace(
        "callee() + 2",
        "callee() + match std::env::var(\"HOME\") {\n        Ok(_) => 1,\n        Err(_) => 0,\n    }",
    );
    assert_ne!(call, CALLER, "the call edit applies");
    assert_ne!(effect, CALLER, "the effect edit applies");
    assert_ne!(before, digest_and_verdict(&call, "f").0, "a second call is a new site");
    assert_ne!(before, digest_and_verdict(&effect, "f").0, "an env read of its own");
}

#[test]
fn the_summary_is_the_fold_of_the_own_parts_over_a_cycle() {
    // `ping` and `pong` call each other, `ping` alone reads a variable: `pong` has the read
    // only through `ping`, its own part has none of it.
    const CYCLE: &str = r#"
//- /lib.rs crate:kernel deps:std
pub fn ping(n: u32) {
    if n > 0 {
        pong(n - 1)
    } else {
        let _ = std::env::var("PING");
    }
}
pub fn pong(n: u32) {
    if n > 0 {
        ping(n - 1)
    }
}
"#;
    let db = db(CYCLE);
    blaze_memo::attach(&db, || {
        let krate = crate_named(&db, "kernel");
        for name in ["ping", "pong", "ping"] {
            let func = fn_named(&db, krate, name);
            let (env, args) = crate::function::generic(&db, func);
            let instance = Instance { def: Def::Body(Body::Fn(func)), args: args.clone() }; // clone: reference counts
            let walked = summary_in(&db, env.clone(), Proofs::Spent, instance); // clone: reference counts
            let cx = Cx::new(&db, env, Proofs::Spent);
            let body = dispatch::body_once(&db, &cx.stored, Body::Fn(func), &args)
                .unwrap_or_else(|it| panic!("`{name}` lowers: {it:?}"));
            let folded = fold(&cx, &body, own_part(&cx, &body, Def::Body(Body::Fn(func))));
            assert_eq!(*walked, folded, "`{name}`'s summary is the fold of its own part");
            // The verdicts the walk gave before it was cut at the own part: both have the read.
            assert!(walked.kinds.has(Rule::Env), "`{name}` reads a variable, itself or through the cycle");
            let own = own_effects(&db, func).unwrap_or_else(|what| panic!("{what}"));
            assert_eq!(own.kinds().has(Rule::Env), name == "ping", "only `ping` reads it itself");
            let callee = if name == "ping" { "pong" } else { "ping" };
            let callee = blaze_items::key::item_key(&db, blaze_items::ItemId::Function(fn_named(&db, krate, callee)));
            assert!(own.callees().contains(&callee), "`{name}` names its callee by key");
        }
    });
}

#[test]
fn a_function_with_no_body_has_no_own_effects() {
    const DECLARED: &str = r#"
//- /lib.rs crate:kernel deps:std
pub trait T {
    fn declared(&self);
}
pub fn f() {}
"#;
    let db = db(DECLARED);
    blaze_memo::attach(&db, || {
        let krate = crate_named(&db, "kernel");
        let root = hir_def::nameres::crate_def_map(&db, krate).root_module_id();
        let data = root.to_db(&db).def_map(&db).module(&db, root);
        let declared = data
            .scope(&db)
            .declarations()
            .find_map(|it| match it {
                hir_def::ModuleDefId::TraitId(it) => Some(it),
                _ => None,
            })
            .and_then(|it| {
                it.trait_items(&db).items.iter().find_map(|&(_, item)| match item {
                    hir_def::AssocItemId::FunctionId(it) => Some(it),
                    _ => None,
                })
            })
            .expect("the trait's method");
        assert_eq!(own_effects(&db, declared), Err(crate::own::NO_BODY));
        // The control: a function with a body has them.
        assert!(own_effects(&db, fn_named(&db, krate, "f")).is_ok());
    });
}
