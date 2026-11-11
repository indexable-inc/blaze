//! `#![blaze::early_drop]` (docs/v2/plan/design/early-drop.md): which types are dropped early,
//! and where the drops land when a body runs. `E` is an early type whose drop records itself
//! (asserted with `trusted`, since recording is an effect); `G` is a guard. Each `mark` and
//! each drop appends a digit to `TRACE`, so a run's order is one number.

use hir_expand::fixture;

use super::eval::{check_error_with, check_pass_and_stdio};
use crate::{
    db::HirDatabase,
    mir::{
        early_drop::{
            class::{DropClass, Scoped},
            local_classes,
        },
        print_mir_body,
    },
    test_db::TestDBExt as _,
};

const ITEMS: &str = r#"
static mut TRACE: u64 = 0;
fn mark(n: u64) { unsafe { TRACE = TRACE * 10 + n; } }
fn trace() -> u64 { unsafe { TRACE } }
fn reset() { unsafe { TRACE = 0; } }

#[blaze::drop(early, trusted = "the test reads when it runs")]
struct E(u64);
impl Drop for E {
    fn drop(&mut self) { mark(self.0); }
}

struct G(u64);
impl Drop for G {
    fn drop(&mut self) { mark(self.0); }
}

struct Pair(E, E);

fn read(e: &E) -> u64 { e.0 }
fn read_g(g: &G) -> u64 { g.0 }
fn take(_e: E) {}
fn make() -> E { E(7) }

fn straight() {
    let a = E(7);
    let g = G(8);
    mark(1);
    read(&a);
    mark(2);
    read_g(&g);
    mark(3);
}

fn branch(c: bool) {
    let a = E(7);
    mark(1);
    if c {
        read(&a);
        mark(2);
    } else {
        mark(3);
    }
    mark(4);
}

fn looped() {
    let a = E(7);
    let mut i = 0;
    while i < 2 {
        read(&a);
        mark(1);
        i += 1;
    }
    mark(2);
}

fn cond_move(c: bool) {
    let a = E(7);
    mark(1);
    if c {
        take(a);
        mark(2);
    } else {
        mark(3);
    }
    mark(4);
}

fn partial() {
    let p = Pair(E(7), E(8));
    mark(1);
    take(p.0);
    mark(2);
    read(&p.1);
    mark(3);
}

fn temporary() {
    mark(1);
    match read(&make()) {
        7 => { mark(2); }
        _ => { mark(3); }
    }
    mark(4);
}

fn captured() {
    let a = E(7);
    let f = || read(&a);
    mark(1);
    f();
    mark(2);
    mark(3);
}

fn reborrowed() {
    let a = E(7);
    let r = &a;
    mark(1);
    mark(2);
    read(r);
    mark(3);
}

fn guard_only() {
    let g = G(8);
    mark(1);
    read_g(&g);
    mark(2);
}

fn ran(what: &str, want: u64) {
    if trace() != want {
        panic!("{}", what);
    }
    reset();
}
"#;

/// Runs `main` over [`ITEMS`], under the attribute or not.
fn run(early: bool, main: &str) {
    let attr = if early { "#![blaze::early_drop]\n" } else { "" };
    let source =
        format!("//- minicore: sized, copy, drop, add, mul, fn, fmt, panic\n{attr}{ITEMS}{main}");
    check_pass_and_stdio(&source, "", "");
}

#[test]
fn an_early_local_is_dropped_after_its_last_use_and_a_guard_where_its_scope_ends() {
    run(true, r#"fn main() { straight(); ran("straight", 17238); }"#);
    run(false, r#"fn main() { straight(); ran("straight", 12387); }"#);
}

#[test]
fn each_arm_of_a_branch_drops_where_it_stops_using_the_local() {
    run(
        true,
        r#"fn main() { branch(true); ran("used arm", 1724); branch(false); ran("other arm", 1734); }"#,
    );
    run(
        false,
        r#"fn main() { branch(true); ran("used arm", 1247); branch(false); ran("other arm", 1347); }"#,
    );
}

#[test]
fn a_local_used_in_a_loop_is_dropped_where_the_loop_is_left() {
    run(true, r#"fn main() { looped(); ran("looped", 1172); }"#);
    run(false, r#"fn main() { looped(); ran("looped", 1127); }"#);
}

#[test]
fn a_conditionally_moved_local_is_dropped_once_on_each_path() {
    run(
        true,
        r#"fn main() { cond_move(true); ran("moved", 1724); cond_move(false); ran("kept", 1734); }"#,
    );
    run(
        false,
        r#"fn main() { cond_move(true); ran("moved", 1724); cond_move(false); ran("kept", 1347); }"#,
    );
}

#[test]
fn a_partially_moved_local_drops_what_is_left_after_its_last_use() {
    run(true, r#"fn main() { partial(); ran("partial", 17283); }"#);
    run(false, r#"fn main() { partial(); ran("partial", 17238); }"#);
}

#[test]
fn a_temporary_of_a_scrutinee_is_dropped_before_the_arms_run() {
    run(true, r#"fn main() { temporary(); ran("temporary", 1724); }"#);
    run(false, r#"fn main() { temporary(); ran("temporary", 1274); }"#);
}

#[test]
fn a_local_a_closure_borrows_lives_until_the_closure_is_last_called() {
    run(true, r#"fn main() { captured(); ran("captured", 1723); }"#);
    run(false, r#"fn main() { captured(); ran("captured", 1237); }"#);
}

#[test]
fn a_local_lives_while_a_borrow_of_it_is_read() {
    run(true, r#"fn main() { reborrowed(); ran("reborrowed", 1273); }"#);
    run(false, r#"fn main() { reborrowed(); ran("reborrowed", 1237); }"#);
}

/// The unwind path drops what is still there: a local dropped early is not dropped again by
/// the cleanup of a panic after it (its drop would panic "dropped twice", not "boom").
#[test]
fn a_panic_after_an_early_drop_does_not_drop_again() {
    check_error_with(
        r#"
//- minicore: sized, copy, drop, add, fmt, panic
#![blaze::early_drop]
static mut DROPS: u32 = 0;

#[blaze::drop(early, trusted = "counts its drops")]
struct Once;
impl Drop for Once {
    fn drop(&mut self) {
        unsafe {
            DROPS += 1;
            if DROPS > 1 {
                panic!("dropped twice");
            }
        }
    }
}

fn touch(_: &Once) {}
fn boom() { panic!("boom"); }

fn main() {
    let a = Once;
    touch(&a);
    boom();
}
"#,
        |e| e.is_panic().is_some_and(|it| it == "boom"),
    );
}

/// The raw-pointer rule: `as_ptr`'s result pins the buffer, so the read through it after the
/// buffer's last use by name finds it alive. `Buf`'s drop clears the byte, which the read
/// would see.
#[test]
fn a_local_a_raw_pointer_was_made_from_keeps_its_scope() {
    check_pass_and_stdio(
        r#"
//- minicore: sized, copy, drop, fmt, panic
#![blaze::early_drop]
static mut CELL: u8 = 0;

#[blaze::drop(early, trusted = "clears the cell the test reads")]
struct Buf;
impl Buf {
    fn new() -> Buf { unsafe { CELL = 7; } Buf }
    fn as_ptr(&self) -> *const u8 { &raw const CELL }
    fn len(&self) -> usize { 1 }
}
impl Drop for Buf {
    fn drop(&mut self) { unsafe { CELL = 0; } }
}

fn pause() {}

fn main() {
    let buf = Buf::new();
    let p = buf.as_ptr();
    pause();
    if unsafe { *p } != 7 {
        panic!("the buffer was dropped while a pointer into it was used");
    }
    let other = Buf::new();
    other.len();
    pause();
    if unsafe { CELL } != 0 {
        panic!("a buffer no pointer was made from was not dropped after its last use");
    }
}
"#,
        "",
        "",
    );
}

/// The printed MIR of the function `name` of the fixture's last file.
fn mir_of(source: &str, name: &str) -> String {
    let fx = fixture::db(source);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, *file_ids.last().unwrap(), name);
        let body = db.mir_body(function.into()).unwrap_or_else(|e| panic!("`{name}`: {e:?}"));
        print_mir_body(&db, body, name)
    })
}

fn function(
    db: &crate::test_db::TestDB,
    file: hir_expand::EditionedFileId,
    name: &str,
) -> hir_def::FunctionId {
    let module = db.module_for_file(file.file_id(db));
    let scope = module.def_map(db).module(db, module).scope(db);
    scope
        .declarations()
        .find_map(|it| match it {
            hir_def::ModuleDefId::FunctionId(it)
                if hir_def::signatures::FunctionSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

/// A body with no early local is the same MIR under the attribute: nothing of a scoped local
/// moves.
#[test]
fn a_body_with_only_guards_is_the_same_mir() {
    let source = |attr: &str| {
        format!("//- minicore: sized, copy, drop, add, mul, fn, fmt, panic\n{attr}{ITEMS}")
    };
    let with = mir_of(&source("#![blaze::early_drop]\n"), "guard_only");
    let without = mir_of(&source(""), "guard_only");
    assert_eq!(with, without);
    // And a body with an early local does change, so the comparison above compares something.
    let with = mir_of(&source("#![blaze::early_drop]\n"), "straight");
    let without = mir_of(&source(""), "straight");
    assert_ne!(with, without);
}

/// Building a body twice gives the same MIR: the drops of an edge are in scope order, not in
/// an order a hash gives.
#[test]
fn the_drops_land_the_same_on_every_build() {
    let source =
        format!("//- minicore: sized, copy, drop, add, mul, fn, fmt, panic\n#![blaze::early_drop]\n{ITEMS}");
    for name in ["straight", "branch", "partial", "captured"] {
        assert_eq!(mir_of(&source, name), mir_of(&source, name), "`{name}`");
    }
}

const TYPES: &str = r#"
//- minicore: sized, copy, drop
//- /alloc.rs crate:alloc
pub mod alloc {
    pub struct Global;
}
pub mod vec {
    pub struct Vec<T, A = crate::alloc::Global> { ptr: *mut T, len: usize, alloc: A }
    impl<T, A> Drop for Vec<T, A> {
        fn drop(&mut self) {}
    }
    pub struct Drain<'a, T> { vec: &'a mut Vec<T> }
    impl<T> Drop for Drain<'_, T> {
        fn drop(&mut self) {}
    }
}
pub mod string {
    pub struct String { vec: crate::vec::Vec<u8> }
}
pub mod boxed {
    pub struct Box<T: ?Sized, A = crate::alloc::Global>(*mut T, A);
    impl<T: ?Sized, A> Drop for Box<T, A> {
        fn drop(&mut self) {}
    }
}
pub mod rc {
    pub struct Rc<T> { ptr: *mut T }
    impl<T> Drop for Rc<T> {
        fn drop(&mut self) {}
    }
}
pub mod sync {
    pub struct Arc<T> { ptr: *mut T }
    impl<T> Drop for Arc<T> {
        fn drop(&mut self) {}
    }
}
//- /main.rs crate:main deps:alloc
use alloc::{boxed::Box, rc::Rc, string::String, sync::Arc, vec::{Drain, Vec}};

struct Plain { a: Vec<u32>, b: String }
struct Nested { p: Plain, q: (Vec<String>, u8) }

struct Printer;
impl Drop for Printer {
    fn drop(&mut self) {}
}

struct Guard<'a>(&'a u8);
impl Drop for Guard<'_> {
    fn drop(&mut self) {}
}

#[blaze::drop(early)]
struct Pooled(u32);
impl Drop for Pooled {
    fn drop(&mut self) {}
}

#[blaze::drop(scoped)]
struct Token { v: Vec<u8> }

struct Frame;

fn probe<T>(
    vec: Vec<u32>,
    string: String,
    boxed: Box<[u32]>,
    nested: Nested,
    printer: Printer,
    guard: Guard<'static>,
    generic: Vec<T>,
    rc: Rc<u32>,
    arc: Arc<Vec<u8>>,
    pooled: Pooled,
    token: Token,
    printers: Vec<Printer>,
    framed: Vec<u8, &'static Frame>,
    number: u32,
    pool: Vec<Pooled>,
    drain: Drain<'static, u8>,
    param: T,
    tokens: (u8, Token),
) {
}
"#;

fn short(class: DropClass) -> &'static str {
    match class {
        DropClass::None => "none",
        DropClass::Early => "early",
        DropClass::Scoped(Scoped::Marked(_)) => "scoped: marked",
        DropClass::Scoped(Scoped::OwnDrop(_)) => "scoped: own drop",
        DropClass::Scoped(Scoped::Shared(_)) => "scoped: shared",
        DropClass::Scoped(Scoped::NotListed(_)) => "scoped: not listed",
        DropClass::Scoped(Scoped::Allocator(_)) => "scoped: allocator",
        DropClass::Scoped(Scoped::Open) => "scoped: open",
    }
}

/// Which types are early: owning containers of plain data and structs of them; a `Drop` impl
/// of the crate's own only when marked; guards, shared handles, a custom allocator, a type
/// parameter and what `#[blaze::drop(scoped)]` marks are scoped.
#[test]
fn which_types_are_dropped_early() {
    let fx = fixture::db(TYPES);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let function = function(&db, *file_ids.last().unwrap(), "probe");
        let body = db.mir_body(function.into()).unwrap_or_else(|e| panic!("`probe`: {e:?}"));
        let classes = local_classes(&db, body);
        let found: Vec<&str> = body
            .param_locals
            .iter()
            .map(|&it| short(classes[it.into_raw().into_u32() as usize]))
            .collect();
        let want = [
            ("vec", "early"),
            ("string", "early"),
            ("boxed", "early"),
            ("nested", "early"),
            ("printer", "scoped: own drop"),
            ("guard", "scoped: own drop"),
            ("generic", "scoped: open"),
            ("rc", "scoped: shared"),
            ("arc", "scoped: shared"),
            ("pooled", "early"),
            ("token", "scoped: marked"),
            ("printers", "scoped: own drop"),
            ("framed", "scoped: allocator"),
            ("number", "none"),
            ("pool", "early"),
            ("drain", "scoped: not listed"),
            ("param", "scoped: open"),
            ("tokens", "scoped: marked"),
        ];
        assert_eq!(found.len(), want.len(), "{found:?}");
        for (found, (name, want)) in found.iter().zip(want) {
            assert_eq!(*found, want, "`{name}`");
        }
    })
}
