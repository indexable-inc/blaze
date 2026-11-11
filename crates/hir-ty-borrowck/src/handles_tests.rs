//! `#![blaze::handles]` as the borrow checker sees it (docs/v2/plan/design/edition.md, change
//! 1): a move of a handle that is used again is a copy, so it is no E0382; a move of anything
//! else still is; `#[blaze::norefcount]` makes each copy an error; and without the attribute a
//! handle used after a move is E0382 as before.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{Mode, borrowck_in_mode};

/// What every fixture has: a handle `Cfg` (a `Share` type, as `Arc` is), a value `Vec` that is
/// not one, and what takes and reads them.
const ITEMS: &str = r#"
struct Cfg;
impl Clone for Cfg {
    fn clone(&self) -> Cfg { Cfg }
}
impl core::clone::Share for Cfg {}

/// Not a handle: moved, it is gone.
struct Vec(u32);

fn f(_: Cfg) {}
fn eat(_: Vec) {}
fn report(_: &Cfg) {}
fn spawn<F: FnOnce() + 'static>(task: F) { task() }

fn spawns() {
    let cfg = Cfg;
    spawn(|| f(cfg));
    spawn(|| f(cfg));
    report(&cfg);
}

fn vec_moved() {
    let v = Vec(1);
    eat(v);
    eat(v);
}

fn looped() {
    let cfg = Cfg;
    loop {
        f(cfg);
    }
}

fn twice() {
    let cfg = Cfg;
    f(cfg);
    f(cfg);
}

#[blaze::norefcount]
fn counted() {
    let cfg = Cfg;
    f(cfg);
    f(cfg);
}

#[blaze::norefcount]
fn counted_last_use() {
    let cfg = Cfg;
    report(&cfg);
    f(cfg);
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

/// The codes of the errors `name`'s check gives, in each mode, with the crate under
/// `#![blaze::handles]` or not.
fn codes(handles: bool, name: &str) -> Vec<Vec<&'static str>> {
    let attr = if handles { "#![blaze::handles]\n" } else { "" };
    let source = format!("//- minicore: sized, copy, clone, handle, fn\n{attr}{ITEMS}");
    let fx = fixture::db(&source);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, name);
        [Mode::Nll, Mode::Polonius]
            .into_iter()
            .map(|mode| {
                let result = borrowck_in_mode(&db, function.into(), mode);
                assert_eq!(result.skipped, None, "`{name}` in {mode:?}");
                assert_eq!(result.not_checked, None, "`{name}` in {mode:?}");
                result.diagnostics.iter().filter(|it| it.is_error()).map(|it| it.code).collect()
            })
            .collect()
    })
}

/// (b) Each closure that owns `cfg` takes a copy, and `report(&cfg)` reads the original.
#[test]
fn closures_that_own_a_handle_used_after_them_take_copies() {
    for codes in codes(true, "spawns") {
        assert!(codes.is_empty(), "{codes:?}");
    }
}

/// (c) A value that is not a handle is moved, and using it again is E0382.
#[test]
fn a_value_that_is_not_a_handle_is_still_moved() {
    for codes in codes(true, "vec_moved") {
        assert_eq!(codes, ["E0382"]);
    }
}

/// (d) The next turn of a loop uses the handle again, so each turn moves a copy.
#[test]
fn a_move_in_a_loop_copies() {
    for codes in codes(true, "looped") {
        assert!(codes.is_empty(), "{codes:?}");
    }
}

/// (e) `#[blaze::norefcount]` refuses the copy the first `f(cfg)` would make, and the last use,
/// a move, is no error.
#[test]
fn norefcount_refuses_an_implied_copy() {
    for codes in codes(true, "counted") {
        assert_eq!(codes, ["NOREFCOUNT"]);
    }
    for codes in codes(true, "counted_last_use") {
        assert!(codes.is_empty(), "{codes:?}");
    }
}

/// (f) Without `#![blaze::handles]` nothing is copied: a handle used after a move is E0382,
/// and `#[blaze::norefcount]` has no copy to refuse.
#[test]
fn without_the_attribute_a_moved_handle_is_e0382() {
    for codes in codes(false, "twice") {
        assert_eq!(codes, ["E0382"]);
    }
    for codes in codes(false, "counted") {
        assert_eq!(codes, ["E0382"]);
    }
    for codes in codes(false, "spawns") {
        assert!(codes.contains(&"E0382"), "{codes:?}");
    }
    for codes in codes(true, "twice") {
        assert!(codes.is_empty(), "{codes:?}");
    }
}
