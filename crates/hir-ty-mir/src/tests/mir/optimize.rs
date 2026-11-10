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
    expect: &Expect,
) {
    let _tracing = setup_tracing();
    let fx = fixture::db(ra_fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
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
/// and hung).
///
/// The crate declares `drop_glue` itself, which takes `#![feature(lang_items)]` (rustc's E0658;
/// `crate_lang_items` collects none without it), so the root starts with it, before `out`.
#[test]
fn a_drop_in_place_that_inlines_still_drops() {
    let fixture = format!(
        "//- minicore: add, builtin_impls, drop, manually_drop, deref, copy, slice, index, \
         coerce_unsized\n#![feature(lang_items)]\n{OUT}\n{}",
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
"#
    );
    check_pass_and_stdio(&fixture, "5 6 ", "");
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
        &expect![[r#"
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
        &expect![[r#"
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
        &expect![[r#"
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

/// std's `Box::new` as the pinned nightly writes it (`alloc/src/boxed.rs`): `box_new_uninit` of
/// the layout, a write, a transmute. `BoxInPlace` takes its allocation from this body.
const BOX: &str = r#"
//- minicore: sized, drop, manually_drop, transmute, copy
#![feature(lang_items)]

use core::mem::ManuallyDrop;

pub struct Layout {
    size: usize,
    align: usize,
}

pub trait SizedTypeProperties: Sized {
    const LAYOUT: Layout = Layout { size: 0, align: 1 };
}
impl<T> SizedTypeProperties for T {}

#[lang = "maybe_uninit"]
pub union MaybeUninit<T> {
    uninit: (),
    value: ManuallyDrop<T>,
}

pub struct Global;

#[lang = "owned_box"]
pub struct Box<T: ?Sized, A = Global>(*mut T, A);

impl<T: ?Sized, A> Drop for Box<T, A> {
    fn drop(&mut self) {}
}

#[rustc_no_mir_inline]
fn box_new_uninit(layout: Layout) -> *mut u8 {
    loop {}
}

#[rustc_intrinsic]
pub unsafe fn write_via_move<T>(ptr: *mut T, value: T);

impl<T> Box<T> {
    pub fn new(x: T) -> Self {
        let ptr = box_new_uninit(<T as SizedTypeProperties>::LAYOUT) as *mut T;
        unsafe { write_via_move(ptr, x) };
        unsafe { core::mem::transmute(ptr) }
    }
}

pub struct Big([u64; 1024]);

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {}
}

#[inline(never)]
fn make() -> Big {
    loop {}
}

fn array() -> Box<[u8; 65536]> {
    Box::new([0u8; 65536])
}

fn nested() -> Box<Big> {
    Box::new(Big([0u64; 1024]))
}

fn guarded() -> Box<Big> {
    let _guard = Guard;
    Box::new(make())
}
"#;

/// The optimized bodies of `names` in [`BOX`] at `level`, given to `check` with the database.
fn with_box_bodies(
    level: MirOptLevel,
    check: impl FnOnce(&base_db::Db, &[&crate::mir::MirBody<'_>]),
) {
    let _tracing = setup_tracing();
    let fx = fixture::db(BOX);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let module = db.module_for_file(file_id.file_id(&db));
        let def_map = module.def_map(&db);
        let scope = &def_map.module(&db, module).scope(&db);
        let func = |name: &str| {
            scope
                .declarations()
                .find_map(|it| match it {
                    ModuleDefId::FunctionId(it)
                        if FunctionSignature::of(&db, it).name.as_str() == name =>
                    {
                        Some(it)
                    }
                    _ => None,
                })
                .expect("no such function")
        };
        let options = MirOptions::at(level);
        let bodies = ["array", "guarded", "nested"]
            .map(|name| db.optimized_mir_body(func(name).into(), options).unwrap());
        check(&db, &bodies);
    })
}

/// The name of the function `func` calls, if it names one.
fn called(db: &base_db::Db, func: &crate::mir::Operand) -> Option<String> {
    let (func, _) = func.fn_item()?;
    Some(FunctionSignature::of(db, func).name.as_str().to_owned())
}

/// `Box::new([0u8; 65536])` allocates first and builds the array in the box: no `Box::new` call,
/// no 64 KiB local, the repeat written through the box's pointer. At both levels.
#[test]
fn box_new_of_a_64k_array_builds_it_in_the_box() {
    use crate::mir::{ProjectionElem, Rvalue, StatementKind, TerminatorKind};
    use rustc_type_ir::inherent::IntoKind as _;
    for level in [MirOptLevel::Debug, MirOptLevel::Release] {
        with_box_bodies(level, |db, bodies| {
            let body = bodies[0];
            let shown = print_mir_body(db, body, "array");
            let calls: Vec<String> = body
                .basic_blocks
                .iter()
                .filter_map(|(_, it)| match &it.terminator.as_ref()?.kind {
                    TerminatorKind::Call { func, .. } => called(db, func),
                    _ => None,
                })
                .collect();
            assert_eq!(calls, ["box_new_uninit"], "{level:?}:\n{shown}");
            let repeats: Vec<bool> = body
                .basic_blocks
                .iter()
                .flat_map(|(_, it)| it.statements.iter())
                .filter_map(|it| match &it.kind {
                    StatementKind::Assign(place, Rvalue::Repeat(..)) => {
                        Some(place.projection.as_slice() == [ProjectionElem::Deref])
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                repeats,
                [true],
                "the array is built through the box's pointer, {level:?}:\n{shown}"
            );
            let big_locals = body
                .locals
                .iter()
                .filter(|(_, it)| {
                    matches!(it.ty.as_ref().kind(), crate::next_solver::TyKind::Array(..))
                })
                .count();
            assert_eq!(big_locals, 0, "no local holds the array, {level:?}:\n{shown}");
            // `Big([0; 1024])`: the array is built in the box's field, no `Big` or array local.
            let body = bodies[2];
            let shown = print_mir_body(db, body, "nested");
            let large = body
                .locals
                .iter()
                .filter(|(_, it)| match it.ty.as_ref().kind() {
                    crate::next_solver::TyKind::Array(..) => true,
                    crate::next_solver::TyKind::Adt(adt, _) => matches!(adt.def_id(),
                        hir_def::AdtId::StructId(it) if hir_def::signatures::StructSignature::of(db, it).name.as_str() == "Big"),
                    _ => false,
                })
                .count();
            assert_eq!(large, 0, "no local holds `Big` or its array, {level:?}:\n{shown}");
            let into_field = body
                .basic_blocks
                .iter()
                .flat_map(|(_, it)| it.statements.iter())
                .any(|it| matches!(&it.kind, StatementKind::Assign(place, Rvalue::Repeat(..))
                    if place.projection.as_slice() == [ProjectionElem::Deref, ProjectionElem::Field(crate::mir::FieldIndex(0))]));
            assert!(into_field, "the array is built in `(*box).0`, {level:?}:\n{shown}");
        });
    }
}

/// `Box::new(make())` with a guard to drop: the box is allocated before `make` runs, `make`
/// returns into it, and when `make` unwinds the box is freed (dropped as a
/// `Box<MaybeUninit<Big>>`, which deallocates and drops nothing) before the guard is dropped,
/// where `make` unwound to before. The allocation unwinds to the guard's drop too.
#[test]
fn a_panicking_argument_frees_the_box_and_unwinds_the_same() {
    use crate::mir::{ProjectionElem, StatementKind, TerminatorKind};
    use crate::next_solver::TyKind;
    use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
    for level in [MirOptLevel::Debug, MirOptLevel::Release] {
        with_box_bodies(level, |db, bodies| {
            let body = bodies[1];
            let shown = print_mir_body(db, body, "guarded");
            let adt_name = |ty: crate::next_solver::Ty<'_>| match ty.kind() {
                TyKind::Adt(adt, _) => Some(match adt.def_id() {
                    hir_def::AdtId::StructId(it) => {
                        hir_def::signatures::StructSignature::of(db, it).name.as_str().to_owned()
                    }
                    hir_def::AdtId::UnionId(it) => {
                        hir_def::signatures::UnionSignature::of(db, it).name.as_str().to_owned()
                    }
                    hir_def::AdtId::EnumId(it) => {
                        hir_def::signatures::EnumSignature::of(db, it).name.as_str().to_owned()
                    }
                }),
                _ => None,
            };
            let calls: Vec<_> = body
                .basic_blocks
                .iter()
                .filter_map(|(_, it)| match &it.terminator.as_ref()?.kind {
                    TerminatorKind::Call { func, destination, cleanup, .. } => {
                        Some((called(db, func)?, destination.clone(), *cleanup))
                    }
                    _ => None,
                })
                .collect();
            let names: Vec<&str> = calls.iter().map(|it| it.0.as_str()).collect();
            assert_eq!(names, ["box_new_uninit", "make"], "{level:?}:\n{shown}");
            let (_, into, free) = &calls[1];
            assert_eq!(
                into.projection.as_slice(),
                [ProjectionElem::Deref],
                "`make` returns into the box, {level:?}:\n{shown}"
            );
            let free = free.expect("`make` unwinds to a cleanup");
            let free = &body.basic_blocks[free];
            assert!(free.is_cleanup, "{level:?}:\n{shown}");
            let TerminatorKind::Drop { place, target, .. } =
                &free.terminator.as_ref().unwrap().kind
            else {
                panic!("the cleanup of `make` frees the box first, {level:?}:\n{shown}");
            };
            let boxed = body.locals[place.local].ty.as_ref();
            let TyKind::Adt(_, args) = boxed.kind() else { panic!("{level:?}:\n{shown}") };
            assert_eq!(adt_name(boxed).as_deref(), Some("Box"), "{level:?}:\n{shown}");
            assert_eq!(
                adt_name(args.type_at(0)).as_deref(),
                Some("MaybeUninit"),
                "the box is freed without dropping its value, {level:?}:\n{shown}"
            );
            // Then on to the guard's drop, where `make` unwound before.
            let guard = &body.basic_blocks[*target];
            let TerminatorKind::Drop { place, .. } = &guard.terminator.as_ref().unwrap().kind
            else {
                panic!("after the free, the guard is dropped, {level:?}:\n{shown}");
            };
            let guard_local = place.local;
            assert!(
                body.binding_locals.values().any(|it| *it == guard_local),
                "the drop after the free is of `_guard`, {level:?}:\n{shown}"
            );
            // The allocation unwinds to the same drop of the guard, with no box to free.
            let (_, _, alloc_cleanup) = &calls[0];
            assert_eq!(*alloc_cleanup, Some(*target), "{level:?}:\n{shown}");
            let free_reads_box = free.statements.iter().any(|it| matches!(&it.kind, StatementKind::Assign(p, _) if p.local == place_local_of(free)));
            assert!(free_reads_box, "{level:?}:\n{shown}");
        });
    }
}

/// The local the drop that ends `block` drops.
fn place_local_of(block: &crate::mir::BasicBlock) -> crate::mir::LocalId {
    match &block.terminator.as_ref().unwrap().kind {
        crate::mir::TerminatorKind::Drop { place, .. } => place.local,
        _ => panic!("not a drop"),
    }
}

/// Two builds of the same source give the same bytes: the rewritten bodies print the same.
#[test]
fn box_in_place_is_deterministic() {
    let print = || {
        let mut out = String::new();
        for level in [MirOptLevel::Debug, MirOptLevel::Release] {
            with_box_bodies(level, |db, bodies| {
                out.push_str(&print_mir_body(db, bodies[0], "array"));
                out.push_str(&print_mir_body(db, bodies[1], "guarded"));
                out.push_str(&print_mir_body(db, bodies[2], "nested"));
            });
        }
        out
    };
    let first = print();
    assert!(first.contains("box_new_uninit"), "{first}");
    assert_eq!(first, print());
}

/// `HeapToStack`: an `alloc` crate (a lang crate, as std's is) whose allocations count
/// themselves, and a program that reads the count only before and after the calls it measures.
/// The counter is the library's own entry point, as std's `System` is, not a
/// `#[global_allocator]` of the program's: this is the program whose allocations are not part
/// of what it does, the one the pass is for. One that the program's `#[global_allocator]` counts
/// keeps every allocation; `hir-ty-codegen`'s `allocations_unobserved` decides that, before the
/// pass is asked.
const ESCAPE: &str = r#"
//- minicore: sized, drop, manually_drop, transmute, copy, size_of, deref, index, slice, add, builtin_impls
//- /alloc.rs crate:alloc
#![feature(lang_items)]

use core::mem::{ManuallyDrop, size_of, transmute};
use core::ops::{Deref, Index};

extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(p: *mut u8);
}

pub static mut ALLOCS: u64 = 0;

pub fn allocs() -> u64 {
    unsafe { ALLOCS }
}

fn alloc_bytes(size: usize) -> *mut u8 {
    unsafe {
        ALLOCS = ALLOCS + 1;
        malloc(size)
    }
}

#[lang = "maybe_uninit"]
pub union MaybeUninit<T> {
    uninit: (),
    value: ManuallyDrop<T>,
}

pub struct Global;

#[lang = "owned_box"]
pub struct Box<T: ?Sized, A = Global>(*mut T, A);

impl<T: ?Sized, A> Drop for Box<T, A> {
    fn drop(&mut self) {
        unsafe { free(self.0 as *mut u8) }
    }
}

#[rustc_intrinsic]
pub unsafe fn write_via_move<T>(ptr: *mut T, value: T);

impl<T> Box<T> {
    pub fn new(x: T) -> Self {
        let ptr = alloc_bytes(size_of::<T>()) as *mut T;
        unsafe { write_via_move(ptr, x) };
        unsafe { transmute(ptr) }
    }

    pub fn new_uninit() -> Box<MaybeUninit<T>> {
        let ptr = alloc_bytes(size_of::<T>()) as *mut MaybeUninit<T>;
        unsafe { transmute(ptr) }
    }
}

pub fn write_box_via_move<T>(b: Box<MaybeUninit<T>>, x: T) -> Box<MaybeUninit<T>> {
    let ptr: *mut MaybeUninit<T> = unsafe { transmute(b) };
    unsafe { write_via_move(ptr as *mut T, x) };
    unsafe { transmute(ptr) }
}

pub fn box_assume_init_into_vec_unsafe<T, const N: usize>(b: Box<MaybeUninit<[T; N]>>) -> Vec<T> {
    let ptr: *mut MaybeUninit<[T; N]> = unsafe { transmute(b) };
    Vec { ptr: ptr as *mut T, cap: N, len: N }
}

pub struct Vec<T> {
    ptr: *mut T,
    cap: usize,
    len: usize,
}

#[repr(C)]
struct RawSlice<T> {
    ptr: *mut T,
    len: usize,
}

impl<T> Vec<T> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_slice(&self) -> &[T] {
        unsafe { transmute(RawSlice { ptr: self.ptr, len: self.len }) }
    }
}

impl<T> Index<usize> for Vec<T> {
    type Output = T;
    fn index(&self, at: usize) -> &T {
        &self.as_slice()[at]
    }
}

impl<T> Drop for Vec<T> {
    fn drop(&mut self) {
        unsafe { free(self.ptr as *mut u8) }
    }
}

struct RcInner<T> {
    strong: usize,
    value: T,
}

pub struct Rc<T> {
    ptr: *mut RcInner<T>,
}

impl<T> Rc<T> {
    pub fn new(value: T) -> Rc<T> {
        let ptr = alloc_bytes(size_of::<RcInner<T>>()) as *mut RcInner<T>;
        unsafe { write_via_move(ptr, RcInner { strong: 1, value }) };
        Rc { ptr }
    }
}

impl<T> Deref for Rc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &(*self.ptr).value }
    }
}

impl<T> Drop for Rc<T> {
    fn drop(&mut self) {
        unsafe { free(self.ptr as *mut u8) }
    }
}

#[macro_export]
macro_rules! vec {
    ($($x:expr),+ $(,)?) => (
        $crate::box_assume_init_into_vec_unsafe(
            $crate::write_box_via_move($crate::Box::new_uninit(), [$($x),+])
        )
    );
}
//- /main.rs crate:main deps:alloc
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

struct Pair(u64, u64);

fn sum_small() -> u64 {
    let v = alloc::vec![1u64, 2, 3, 4];
    v[0] + v[1] + v[2] + v[3] + v.len() as u64
}

fn box_sum() -> u64 {
    let b = alloc::Box::new(Pair(5, 6));
    b.0 + b.1
}

fn moved_out() -> u64 {
    let b = alloc::Box::new(Pair(7, 8));
    let p = *b;
    p.0 + p.1
}

fn rc_sum() -> u64 {
    let r = alloc::Rc::new(Pair(3, 4));
    r.0 + r.1
}

fn boxed(x: u64) -> alloc::Box<u64> {
    alloc::Box::new(x)
}

fn measured(f: fn() -> u64) {
    let before = alloc::allocs();
    let got = f();
    let after = alloc::allocs();
    out(got);
    out(after - before);
}

fn main() {
    measured(sum_small);
    measured(box_sum);
    measured(moved_out);
    measured(rc_sum);
    let before = alloc::allocs();
    let b = boxed(9);
    let after = alloc::allocs();
    out(*b);
    out(after - before);
}
"#;

/// What [`ESCAPE`]'s `main` prints at `level`: each result, then how many allocations it made.
fn escape_stdout(level: MirOptLevel) -> String {
    let _tracing = setup_tracing();
    let fx = fixture::db(ESCAPE);
    let file_id = *fx.files().last().unwrap();
    let db = fx.into_db();
    match super::eval::eval_main_at(&db, file_id, level) {
        Ok((stdout, _)) => stdout,
        Err(e) => panic!("at {level:?}: {e:?}"),
    }
}

/// A small `vec!` built and summed, a `Box` and an `Rc` read through, and a `Box` moved out of
/// allocate nothing once optimized, and each once without; the sums are the same at every
/// level; a returned `Box` allocates at every level.
#[test]
fn allocations_that_never_leave_their_function_move_to_the_stack() {
    expect!["14 1 11 1 15 1 7 1 9 1 "].assert_eq(&escape_stdout(MirOptLevel::Off));
    for level in [MirOptLevel::Debug, MirOptLevel::Release] {
        expect!["14 0 11 0 15 0 7 0 9 1 "].assert_eq(&escape_stdout(level));
    }
}

/// The optimized bodies of `names` in [`ESCAPE`]'s `main` crate with `options`: each printed,
/// with the names of the functions it calls and the bytes it moved into its frame.
fn escape_bodies(options: MirOptions, names: &[&str]) -> Vec<(String, Vec<String>, u32)> {
    escape_bodies_of(ESCAPE, options, names)
}

/// [`escape_bodies`] of `text`, a variant of [`ESCAPE`].
fn escape_bodies_of(
    text: &str,
    options: MirOptions,
    names: &[&str],
) -> Vec<(String, Vec<String>, u32)> {
    let _tracing = setup_tracing();
    let fx = fixture::db(text);
    let file_id = *fx.files().last().unwrap();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let module = db.module_for_file(file_id.file_id(&db));
        let def_map = module.def_map(&db);
        let scope = &def_map.module(&db, module).scope(&db);
        let func = |name: &str| {
            scope
                .declarations()
                .find_map(|it| match it {
                    ModuleDefId::FunctionId(it)
                        if FunctionSignature::of(&db, it).name.as_str() == name =>
                    {
                        Some(it)
                    }
                    _ => None,
                })
                .expect("no such function")
        };
        names
            .iter()
            .map(|&name| {
                let body = db.optimized_mir_body(func(name).into(), options).unwrap();
                let calls = body
                    .basic_blocks
                    .iter()
                    .filter_map(|(_, block)| match &block.terminator.as_ref()?.kind {
                        crate::mir::TerminatorKind::Call { func, .. } => called(&db, func),
                        _ => None,
                    })
                    .collect();
                (print_mir_body(&db, body, name), calls, body.inlined.moved_to_frame)
            })
            .collect()
    })
}

/// Whether a body calls an allocating constructor: `new` (`Box::new`, `Rc::new`) or
/// `new_uninit` (`vec!`'s box).
fn calls_new(calls: &[String]) -> bool {
    calls.iter().any(|it| it == "new" || it == "new_uninit")
}

/// The moved bodies call no constructor, with the UB checks too (they run after the pass, so
/// a check of a box's pointer does not keep it); the returned box keeps its `Box::new`; with
/// `heap_to_stack` off, every body keeps its allocation.
#[test]
fn a_returned_box_stays_on_the_heap_and_the_option_turns_the_pass_off() {
    let names = ["sum_small", "box_sum", "moved_out", "rc_sum", "boxed"];
    for level in [MirOptLevel::Debug, MirOptLevel::Release] {
        for options in
            [MirOptions::at(level), MirOptions { ub_checks: true, ..MirOptions::at(level) }]
        {
            let on = escape_bodies(options, &names);
            for (name, (shown, calls, moved)) in names.iter().zip(&on) {
                assert_eq!(calls_new(calls), *name == "boxed", "{name} with {options:?}:\n{shown}");
                assert_eq!(*moved == 0, *name == "boxed", "{name} with {options:?}:\n{shown}");
            }
        }
        let off =
            escape_bodies(MirOptions { heap_to_stack: false, ..MirOptions::at(level) }, &names);
        for (name, (shown, calls, moved)) in names.iter().zip(&off) {
            assert!(calls_new(calls), "{name} at {level:?} with heap_to_stack off:\n{shown}");
            assert_eq!(*moved, 0, "{name} at {level:?} with heap_to_stack off:\n{shown}");
        }
    }
}

/// A body the MIR inliner builds into its caller allocates again there: the inliner's callees
/// are built with `heap_to_stack` off (`inline.rs` `callee_options`), so a moved slot never
/// lands in a caller's frame. `main`, inlining an `#[inline]` `box_sum` at level 2, makes its
/// box (by `Box::new`, or the allocation it inlines to) and moves nothing into its own frame;
/// `box_sum`, compiled on its own, still moves it.
#[test]
fn an_inlined_body_allocates_again_in_its_caller() {
    let text = ESCAPE
        .replace("fn box_sum() -> u64 {", "#[inline]\nfn box_sum() -> u64 {")
        .replace("    measured(box_sum);\n", "    out(box_sum());\n");
    assert!(
        text.contains("#[inline]\nfn box_sum") && text.contains("out(box_sum());"),
        "ESCAPE changed:\n{text}"
    );
    let options = MirOptions { inline: true, ..MirOptions::at(MirOptLevel::Release) };
    let bodies = escape_bodies_of(&text, options, &["main", "box_sum"]);
    let (shown, calls, moved) = &bodies[0];
    assert!(
        !calls.iter().any(|it| it == "box_sum"),
        "the premise: `main` inlines `box_sum`:\n{shown}"
    );
    let allocates = calls.iter().any(|it| matches!(it.as_str(), "new" | "alloc_bytes" | "malloc"));
    assert!(allocates, "`main` makes `box_sum`'s box on the heap:\n{shown}");
    assert_eq!(*moved, 0, "`main` moved something into its frame:\n{shown}");
    let (shown, calls, moved) = &bodies[1];
    assert!(
        !calls_new(calls) && *moved != 0,
        "`box_sum` on its own keeps its box on the heap:\n{shown}"
    );
}

/// Two runs of the pass over the same source print the same bodies.
#[test]
fn heap_to_stack_is_deterministic() {
    let names = ["sum_small", "box_sum", "moved_out", "rc_sum"];
    let first = escape_bodies(MirOptions::at(MirOptLevel::Release), &names);
    assert_eq!(first, escape_bodies(MirOptions::at(MirOptLevel::Release), &names));
}
