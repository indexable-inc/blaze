//! `#![blaze::early_drop]` as the borrow checker sees it (docs/v2/plan/design/early-drop.md,
//! section 6): an early local's drop comes after its last use, so a borrow it holds ends
//! there and not where its scope ends. The three programs of the spec are accepted under the
//! attribute and refused without it; what is accepted without it is accepted with it; what
//! is an error for another reason stays one.

use base_db::{Db, all_crates};
use hir_def::{FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{Mode, borrowck_in_mode};

/// `Cursor` holds a unique borrow of a `Buf` and has a `Drop` impl, so Rust counts its drop
/// as a use of that borrow; it is marked early. `Lock` is the same with no mark: a guard.
const ITEMS: &str = r#"
struct Buf(u32);

#[blaze::drop(early, trusted = "writes its last byte through, which nothing reads before it")]
struct Cursor<'a> { buf: &'a mut Buf }
impl Drop for Cursor<'_> {
    fn drop(&mut self) {}
}
impl<'a> Cursor<'a> {
    fn new(buf: &'a mut Buf) -> Cursor<'a> { Cursor { buf } }
    fn put(&mut self, n: u32) { self.buf.0 = n; }
    fn first(&self) -> Option<u32> { Some(self.buf.0) }
    fn as_ptr(&self) -> *const u32 { &raw const self.buf.0 }
}

struct Lock<'a> { buf: &'a mut Buf }
impl Drop for Lock<'_> {
    fn drop(&mut self) {}
}
impl<'a> Lock<'a> {
    fn new(buf: &'a mut Buf) -> Lock<'a> { Lock { buf } }
    fn put(&mut self, n: u32) { self.buf.0 = n; }
}

fn len(buf: &Buf) -> u32 { buf.0 }
fn push(buf: &mut Buf, n: u32) { buf.0 = n; }
fn eat(_: Cursor<'_>) {}
fn pause() {}

// The three programs the attribute accepts.

fn straight() {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    c.put(1);
    len(&buf);
}

fn one_arm(flag: bool) {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    if flag {
        c.put(1);
        len(&buf);
    } else {
        c.put(2);
    }
}

fn scrutinee() {
    let mut buf = Buf(0);
    match Cursor::new(&mut buf).first() {
        Some(b) => push(&mut buf, b),
        None => {}
    }
}

// A guard is not early: its drop where the scope ends is still a use.

fn guarded() {
    let mut buf = Buf(0);
    let mut lock = Lock::new(&mut buf);
    lock.put(1);
    len(&buf);
}

// Accepted without the attribute, so accepted with it.

fn used_to_the_end() {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    c.put(1);
    pause();
    c.put(2);
}

fn reborrowed() {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    let r = &mut c;
    pause();
    r.put(1);
    pause();
}

fn in_a_loop() {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    let mut i = 0;
    while i < 3 {
        c.put(i);
        i += 1;
    }
    len(&buf);
    pause();
}

fn moved_on_one_arm(flag: bool) {
    let mut buf = Buf(0);
    let c = Cursor::new(&mut buf);
    if flag {
        eat(c);
    }
    pause();
}

fn captured() {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    let mut write = |n: u32| c.put(n);
    pause();
    write(1);
    pause();
    write(2);
}

fn two_cursors() {
    let mut a = Buf(0);
    let mut b = Buf(0);
    let mut first = Cursor::new(&mut a);
    let mut second = Cursor::new(&mut b);
    first.put(1);
    pause();
    second.put(2);
    pause();
    first.put(3);
}

fn pointer_kept() -> u32 {
    let mut buf = Buf(7);
    let c = Cursor::new(&mut buf);
    let p = c.as_ptr();
    pause();
    unsafe { *p }
}

// Errors for another reason, with the attribute or without.

fn used_after_move() {
    let mut buf = Buf(0);
    let c = Cursor::new(&mut buf);
    eat(c);
    eat(c);
}

fn used_while_borrowed() {
    let mut buf = Buf(0);
    let mut c = Cursor::new(&mut buf);
    len(&buf);
    c.put(1);
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
/// `#![blaze::early_drop]` or not.
fn codes(early: bool, name: &str) -> Vec<Vec<&'static str>> {
    let attr = if early { "#![blaze::early_drop]\n" } else { "" };
    let source = format!("//- minicore: sized, copy, drop, option, add, fn\n{attr}{ITEMS}");
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

/// The cause is the drop at the end of the scope: with the attribute it is gone and so is the
/// error; without it both are there.
#[test]
fn the_three_programs_are_accepted_under_the_attribute_and_refused_without_it() {
    for (name, code) in [("straight", "E0502"), ("one_arm", "E0502"), ("scrutinee", "E0499")] {
        for codes in codes(true, name) {
            assert!(codes.is_empty(), "`{name}` under the attribute: {codes:?}");
        }
        for codes in codes(false, name) {
            assert_eq!(codes, [code], "`{name}` without the attribute");
        }
    }
}

#[test]
fn a_guards_drop_is_still_a_use_where_its_scope_ends() {
    for early in [true, false] {
        for codes in codes(early, "guarded") {
            assert_eq!(codes, ["E0502"], "early drop {early}");
        }
    }
}

#[test]
fn nothing_accepted_without_the_attribute_is_refused_with_it() {
    for name in [
        "used_to_the_end",
        "reborrowed",
        "in_a_loop",
        "moved_on_one_arm",
        "captured",
        "two_cursors",
        "pointer_kept",
    ] {
        for early in [false, true] {
            for codes in codes(early, name) {
                assert!(codes.is_empty(), "`{name}`, early drop {early}: {codes:?}");
            }
        }
    }
}

#[test]
fn an_error_for_another_reason_stays_one() {
    for early in [true, false] {
        for codes in codes(early, "used_after_move") {
            assert_eq!(codes, ["E0382"], "early drop {early}");
        }
        for codes in codes(early, "used_while_borrowed") {
            assert_eq!(codes, ["E0502"], "early drop {early}");
        }
    }
}
