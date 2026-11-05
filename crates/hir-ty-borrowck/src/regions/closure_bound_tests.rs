//! A closure given where a higher-ranked bound whose argument implies an outlives bound is
//! expected (`F: FnOnce(&&'t T)`, which is `Option::<&'t T>::filter`'s `P: FnOnce(&T) -> bool`)
//! keeps only the loans its signature makes it keep. Its signature `for<'a> fn(&'a &'t T)`
//! assumes `'t: 'a` for the placeholder `'a`; a body region made to outlive `'a` outlives `'t`
//! and no more (`inference::answer_placeholders`), as in rustc, which gives a placeholder no
//! point of the body. Before that, the borrow behind `'t` held every point of the body, and
//! `let Some(&(len, rights)) = objects.get(i).filter(|_| ..) else { .. }` in
//! blaze-inproc's `Grant::decode` was refused with E0597 (`objects` dropped while borrowed)
//! and E0505 (`objects` moved out of while borrowed), where rustc accepts it.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode};

const FIXTURE: &str = r#"
//- minicore: sized, copy, fn, option
struct W(u64);
/// Not `Copy`: moving it out while it is borrowed is a conflict.
struct Objects([u64; 2]);
struct Grant { objects: Objects }

fn eat<T>(_: T) {}

fn takes<'t>(it: &'t W, f: impl FnOnce(&&'t W) -> bool) -> bool {
    f(&it)
}

// `Option::filter`'s body.
fn filter<T, P: FnOnce(&T) -> bool>(it: Option<T>, p: P) -> Option<T> {
    if let Some(x) = it {
        if p(&x) {
            return Some(x);
        }
    }
    None
}

fn get<'t>(it: &'t Objects, i: usize) -> Option<&'t u64> {
    match i {
        0 => Some(&it.0[0]),
        1 => Some(&it.0[1]),
        _ => None,
    }
}

fn a_closure_to_a_higher_ranked_bound() -> bool {
    let local = W(1);
    takes(&local, |_| true)
}

fn let_else_over_filter(i: usize) -> Option<Grant> {
    let objects = Objects([1, 2]);
    let Some(&len) = filter(get(&objects, i), |_| i != 1) else {
        return None;
    };
    if len == 0 {
        return None;
    }
    Some(Grant { objects })
}

fn let_else_over_filter_in_a_loop(count: usize) -> Option<Grant> {
    let objects = Objects([1, 2]);
    let mut i = 0;
    while i < count {
        let Some(&len) = filter(get(&objects, i), |_| i != 1) else {
            return None;
        };
        if len == 0 {
            return None;
        }
        i += 1;
    }
    Some(Grant { objects })
}

fn match_over_filter(i: usize) -> Option<Grant> {
    let objects = Objects([1, 2]);
    let len = match filter(get(&objects, i), |_| true) {
        Some(&len) => len,
        None => return None,
    };
    if len == 0 {
        return None;
    }
    Some(Grant { objects })
}

fn the_filtered_borrow_used_after_the_move(i: usize) -> u64 {
    let objects = Objects([1, 2]);
    let kept = filter(get(&objects, i), |_| true);
    eat(Grant { objects });
    match kept {
        Some(&len) => len,
        None => 0,
    }
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}`")
}

fn checked(name: &str) -> [(Mode, BorrowckResult); 3] {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius, Mode::PoloniusFlow]
            .map(|mode| (mode, borrowck_in_mode(&db, function.into(), mode)))
    })
}

fn accepted(name: &str) {
    for (mode, result) in checked(name) {
        assert_eq!(result.not_checked, None, "`{name}` in {mode:?}");
        assert_eq!(result.skipped, None, "`{name}` in {mode:?}");
        assert!(result.diagnostics.is_empty(), "`{name}` in {mode:?}: {:?}", result.diagnostics);
    }
}

#[test]
fn a_closure_given_a_higher_ranked_bound_keeps_no_loan_past_its_call() {
    accepted("a_closure_to_a_higher_ranked_bound");
}

#[test]
fn let_else_over_option_filter_keeps_no_loan_past_the_statement() {
    accepted("let_else_over_filter");
    accepted("let_else_over_filter_in_a_loop");
    accepted("match_over_filter");
}

/// What the filtered `Option` still borrows stays borrowed: moving its owner while it is used
/// later is refused.
#[test]
fn a_filtered_borrow_used_after_its_owner_moves_is_refused() {
    let name = "the_filtered_borrow_used_after_the_move";
    for (mode, result) in checked(name) {
        assert_eq!(result.not_checked, None, "`{name}` in {mode:?}");
        assert!(
            result.diagnostics.iter().any(|it| it.code == "E0505"),
            "`{name}` in {mode:?}: {:?}",
            result.diagnostics
        );
    }
}
