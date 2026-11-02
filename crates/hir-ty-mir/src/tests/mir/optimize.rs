//! rustc's MIR optimizations (`hir_ty_mir::mir::optimize`): what they make of a body, and that
//! a program does at each level what it does without them. `eval::check_pass_and_stdio`
//! runs `main` at every level and requires the same output of each.

use expect_test::{Expect, expect};
use hir_def::{ModuleDefId, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{
    db::HirDatabase,
    mir::{MirOptLevel, MirOptions, print_mir_body},
    setup_tracing,
    test_db::TestDBExt as _,
};

use super::eval::check_pass_and_stdio;

/// `out` writes a number and a space to stdout.
const OUT: &str = r#"
extern "C" {
    pub fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}

fn out(mut n: u64) {
    let mut digits = [0u8; 21];
    let mut at = 20;
    digits[at] = b' ';
    loop {
        at = at - 1;
        digits[at] = b'0' + (n % 10) as u8;
        n = n / 10;
        if n == 0 {
            break;
        }
    }
    unsafe { write(1, &digits[at], 21 - at) };
}
"#;

fn check_output(minicore: &str, program: &str, expected: &str) {
    let fixture = format!("//- minicore: add, builtin_impls, {minicore}\n{OUT}\n{program}");
    check_pass_and_stdio(&fixture, expected, "");
}

/// The body of the function `name` at `level`, as `-Zunpretty=mir` writes it.
fn check_mir(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    name: &str,
    level: MirOptLevel,
    expect: Expect,
) {
    let _tracing = setup_tracing();
    let fx = fixture::db(ra_fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let module = db.module_for_file(file_id.file_id(&db));
        let def_map = module.def_map(&db);
        let scope = &def_map.module(&db, module).scope(&db);
        let func = scope
            .declarations()
            .find_map(|it| match it {
                ModuleDefId::FunctionId(it)
                    if FunctionSignature::of(&db, it).name.as_str() == name =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("no such function");
        let body = db.optimized_mir_body(func.into(), MirOptions::at(level)).unwrap();
        expect.assert_eq(&print_mir_body(&db, body, name));
    })
}

#[test]
fn levels_agree_on_loops_and_arithmetic() {
    check_output(
        "slice, index, coerce_unsized",
        r#"
fn collatz(mut n: u64) -> u32 {
    let mut steps = 0;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        steps += 1;
    }
    steps
}

fn main() {
    let a = collatz(27);
    let b = a;
    let c = b;
    out(c as u64);
    let mut sum = 0u64;
    let mut i = 0;
    while i < 10 {
        if i % 3 == 0 {
            sum += i;
        } else if true {
            sum += 1;
        }
        i += 1;
    }
    out(sum);
}
"#,
        "111 24 ",
    );
}

#[test]
fn levels_agree_on_matches_and_options() {
    check_output(
        "option, slice, index, coerce_unsized, copy, clone",
        r#"
#[derive(Clone, Copy)]
enum Shape {
    Dot,
    Square(u32),
    Rect { w: u32, h: u32 },
}

fn area(shape: Shape) -> u64 {
    match shape {
        Shape::Dot => 0,
        Shape::Square(side) => (side * side) as u64,
        Shape::Rect { w, h } if w == h => 1000,
        Shape::Rect { w, h } => (w * h) as u64,
    }
}

fn first_even(xs: &[u32]) -> Option<u32> {
    let mut i = 0;
    while i < xs.len() {
        if xs[i] % 2 == 0 {
            return Some(xs[i]);
        }
        i += 1;
    }
    None
}

fn main() {
    let shapes = [Shape::Dot, Shape::Square(3), Shape::Rect { w: 2, h: 5 }, Shape::Rect { w: 4, h: 4 }];
    let mut i = 0;
    while i < shapes.len() {
        out(area(shapes[i]));
        i += 1;
    }
    match first_even(&[3, 5, 8, 9]) {
        Some(x) => out(x as u64),
        None => out(1),
    }
    let x = 7u32;
    let y = x.clone();
    out(y as u64);
    if let None = first_even(&[1]) {
        out(1);
    }
}
"#,
        "0 9 10 1000 8 7 1 ",
    );
}

#[test]
fn levels_agree_on_drops_and_closures() {
    check_output(
        "fn, drop, copy, slice, index, coerce_unsized",
        r#"
struct Loud(u64);

impl Drop for Loud {
    fn drop(&mut self) {
        out(self.0);
    }
}

fn pick(flag: bool) -> Loud {
    let a = Loud(1);
    let b = Loud(2);
    if flag { a } else { b }
}

fn main() {
    let kept = pick(true);
    out(10);
    let add = |x: u64| x + kept.0;
    out(add(5));
    let mut total = 0;
    let mut bump = |by: u64| total += by;
    bump(3);
    bump(4);
    out(total);
    {
        let _inner = Loud(3);
        out(20);
    }
    let unused = Loud(4);
    let moved = unused;
    out(30);
    drop(moved);
}
"#,
        "2 10 6 7 20 3 30 4 1 ",
    );
}

/// core's `drop_in_place` is an `#[inline(always)]` call of the `drop_glue` lang item, whose body
/// is a placeholder for the type's drop glue: inlined, it dropped nothing, and std's
/// `Vec::retain_mut` dropped none of what it removed (cc's build scripts leaked every job token
/// and hung, queue 01523).
#[test]
fn a_drop_in_place_that_inlines_still_drops() {
    check_output(
        "drop, manually_drop, deref, copy, slice, index, coerce_unsized",
        r#"
mod p {
    #[inline(always)]
    pub unsafe fn drop_in_place<T>(to_drop: *mut T) {
        unsafe { drop_glue(&mut *to_drop) }
    }

    #[lang = "drop_glue"]
    unsafe fn drop_glue<T>(_: &mut T) {}
}

struct Loud(u64);

impl Drop for Loud {
    fn drop(&mut self) {
        out(self.0);
    }
}

fn drop_at<T>(slot: *mut T) {
    unsafe { p::drop_in_place(slot) }
}

fn main() {
    let loud = core::mem::ManuallyDrop::new(Loud(5));
    drop_at(&*loud as *const Loud as *mut Loud);
    out(6);
}
"#,
        "5 6 ",
    );
}

#[test]
fn levels_agree_on_early_returns() {
    check_output(
        "option, slice, index, coerce_unsized",
        r#"
fn checked(a: u32, b: u32) -> Option<u32> {
    if b == 0 { return None; }
    Some(a / b)
}

fn main() {
    match checked(10, 2) {
        Some(it) => out(it as u64),
        None => out(0),
    }
    match checked(1, 0) {
        Some(it) => out(it as u64),
        None => out(0),
    }
}
"#,
        "5 0 ",
    );
}

#[test]
fn debug_level_drops_storage_and_copies() {
    check_mir(
        r#"
fn copies(a: u32) -> u32 {
    let b = a;
    let c = b;
    let d = c + 1;
    d
}
"#,
        "copies",
        MirOptLevel::Debug,
        expect![[r#"
            fn copies(_1: u32) -> u32 {
                debug a => _1;
                debug b => _1;
                debug c => _1;
                debug d => _0;
                let mut _0: u32;

                bb0: {
                    _0 = Add(copy _1, const 1_u32);
                    return;
                }
            }
        "#]],
    );
}

#[test]
fn constant_conditions_fold() {
    check_mir(
        r#"
fn folded() -> u32 {
    if true { 1 } else { 2 }
}
"#,
        "folded",
        MirOptLevel::Debug,
        expect![[r#"
            fn folded() -> u32 {
                let mut _0: u32;

                bb0: {
                    _0 = const 1_u32;
                    return;
                }
            }
        "#]],
    );
}

#[test]
fn release_level_removes_dead_stores() {
    check_mir(
        r#"
fn dead(a: u32) -> u32 {
    let _ = a + 0;
    a
}
"#,
        "dead",
        MirOptLevel::Release,
        expect![[r#"
            fn dead(_1: u32) -> u32 {
                debug a => _1;
                let mut _0: u32;

                bb0: {
                    _0 = move _1;
                    return;
                }
            }
        "#]],
    );
}
