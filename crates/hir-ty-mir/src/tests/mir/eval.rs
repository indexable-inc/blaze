use hir_def::{HasModule, signatures::FunctionSignature};
use hir_expand::EditionedFileId;
use span::Edition;
use syntax::{TextRange, TextSize};
use hir_expand::fixture;

use crate::{
    db::HirDatabase, display::DisplayTarget, mir::MirLowerError,
    mir_pretty::MirEvalErrorPretty as _, next_solver::GenericArgs, setup_tracing, test_db::{TestDB, TestDBExt as _},
};

use crate::mir::{MirEvalError, MirOptLevel, MirOptions, interpret_mir, set_interpreter_mir_opt_level};

fn eval_main(db: &TestDB, file_id: EditionedFileId) -> Result<(String, String), MirEvalError<'_>> {
    let unoptimized = eval_main_at(db, file_id, MirOptLevel::Off);
    // rustc's MIR optimizations change nothing a program does.
    for level in [MirOptLevel::Debug, MirOptLevel::Release] {
        let optimized = eval_main_at(db, file_id, level);
        match (&unoptimized, &optimized) {
            (Ok(expected), Ok(got)) => {
                assert_eq!(expected, got, "at MIR optimization level {level:?}")
            }
            (Err(_), Err(_)) => (),
            (Ok(_), Err(e)) => panic!("at MIR optimization level {level:?}: {e:?}"),
            (Err(e), Ok(_)) => {
                panic!("only at MIR optimization level {level:?} does it run: {e:?}")
            }
        }
    }
    unoptimized
}

fn eval_main_at(
    db: &TestDB,
    file_id: EditionedFileId,
    level: MirOptLevel,
) -> Result<(String, String), MirEvalError<'_>> {
    crate::attach_db(db, || {
        let module_id = db.module_for_file(file_id.file_id(db));
        let def_map = module_id.def_map(db);
        let scope = &def_map.module(db, module_id).scope(db);
        let func_id = scope
            .declarations()
            .find_map(|x| match x {
                hir_def::ModuleDefId::FunctionId(x) => {
                    if FunctionSignature::of(db, x).name.display(db, Edition::CURRENT).to_string()
                        == "main"
                    {
                        Some(x)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .expect("no main function found");
        let env = crate::ParamEnvAndCrate {
            param_env: db.trait_environment(func_id.into()),
            krate: func_id.krate(db),
        }
        .store();
        let args = GenericArgs::empty().store();
        let body = match level {
            MirOptLevel::Off => db.monomorphized_runtime_mir_body(func_id.into(), args, env),
            level => db.monomorphized_optimized_mir_body(func_id.into(), MirOptions::at(level), args, env),
        }
        .map_err(|e| MirEvalError::MirLowerError(func_id, e))?;
        set_interpreter_mir_opt_level(level);
        let ran = interpret_mir(db, body, None);
        set_interpreter_mir_opt_level(MirOptLevel::Off);
        let (result, output) = ran?;
        result?;
        Ok((output.stdout().into_owned(), output.stderr().into_owned()))
    })
}

fn check_pass(#[rust_analyzer::rust_fixture] ra_fixture: &str) {
    check_pass_and_stdio(ra_fixture, "", "");
}

pub(super) fn check_pass_and_stdio(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    expected_stdout: &str,
    expected_stderr: &str,
) {
    let _tracing = setup_tracing();
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let mut db = fx.into_db();
    // As a build does, for `Location::caller` and `file!()`.
    crate::mir::location::name_sources(&mut db, "fixture", |_, _| Some("main.rs".to_owned()));
    crate::attach_db(&db, || {
        let file_id = *file_ids.last().unwrap();
        let x = eval_main(&db, file_id);
        match x {
            Err(e) => {
                let mut err = String::new();
                let line_index = |size: TextSize| {
                    let mut size = u32::from(size) as usize;
                    let lines = ra_fixture.lines().enumerate();
                    for (i, l) in lines {
                        if let Some(x) = size.checked_sub(l.len()) {
                            size = x;
                        } else {
                            return (i, size);
                        }
                    }
                    (usize::MAX, size)
                };
                let span_formatter = |file, range: TextRange| {
                    format!(
                        "{:?} {:?}..{:?}",
                        file,
                        line_index(range.start()),
                        line_index(range.end())
                    )
                };
                let krate = db.module_for_file(file_id.file_id(&db)).krate(&db);
                e.pretty_print(
                    &mut err,
                    &db,
                    span_formatter,
                    DisplayTarget::from_crate(&db, krate),
                )
                .unwrap();
                panic!("Error in interpreting: {err}");
            }
            Ok((stdout, stderr)) => {
                assert_eq!(stdout, expected_stdout);
                assert_eq!(stderr, expected_stderr);
            }
        }
    })
}

fn check_panic(#[rust_analyzer::rust_fixture] ra_fixture: &str, expected_panic: &str) {
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let file_id = *file_ids.last().unwrap();
        let e = eval_main(&db, file_id).unwrap_err();
        assert_eq!(
            e.is_panic().unwrap_or_else(|| panic!("unexpected error: {e:?}")),
            expected_panic
        );
    })
}

fn check_error_with(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    expect_err: impl FnOnce(MirEvalError<'_>) -> bool,
) {
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let file_id = *file_ids.last().unwrap();
        let e = eval_main(&db, file_id).unwrap_err();
        let shown = format!("{e:?}");
        assert!(expect_err(e), "unexpected error: {shown}");
    })
}

/// The error `e` is, under the functions it was raised in.
fn innermost<'a, 'db>(e: &'a MirEvalError<'db>) -> &'a MirEvalError<'db> {
    let mut err = e;
    while let MirEvalError::InFunction(inner, _) = err {
        err = inner;
    }
    err
}

#[test]
fn an_unresolved_weak_extern_static_is_null_when_run() {
    check_pass(
        r#"
//- minicore: sized
extern "C" {
    #[linkage = "extern_weak"]
    static __dso_handle: *mut u8;
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    if unsafe { __dso_handle } as usize != 0 {
        should_not_reach();
    }
}
        "#,
    );
}

#[test]
fn reading_a_strong_extern_static_when_run_is_refused_naming_it() {
    check_error_with(
        r#"
//- minicore: sized
extern "C" {
    static environ: *const *const u8;
}

fn main() {
    let _env = unsafe { environ };
}
        "#,
        |e| {
            matches!(innermost(&e), MirEvalError::NotSupported(it)
                if it.contains("extern static `environ`"))
        },
    );
}

#[test]
fn function_with_extern_c_abi() {
    check_pass(
        r#"
extern "C" fn foo(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let x = foo(2, 3);
}
        "#,
    );
}

#[test]
fn panic_fmt() {
    // panic!
    // -> panic_2021 (builtin macro redirection)
    //   -> #[lang = "panic_fmt"] core::panicking::panic_fmt (hooked by CTFE for redirection)
    //   -> core::panicking::const_panic_fmt
    //     -> #[rustc_const_panic_str] core::panicking::panic_display (hooked by CTFE for builtin panic)
    //       -> Err(ConstEvalError::Panic)
    check_panic(
        r#"
//- minicore: fmt, panic
fn main() {
    panic!("hello, world!");
}
    "#,
        "hello, world!",
    );
}

#[test]
fn panic_display() {
    // panic!
    // -> panic_2021 (builtin macro redirection)
    //   -> #[rustc_const_panic_str] core::panicking::panic_display (hooked by CTFE for builtin panic)
    //     -> Err(ConstEvalError::Panic)
    check_panic(
        r#"
//- minicore: fmt, panic

fn main() {
    panic!("{}", "hello, world!");
}
    "#,
        "hello, world!",
    );
}

#[test]
fn drop_basic() {
    check_pass(
        r#"
//- minicore: drop, add

struct X<'a>(&'a mut i32);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        *self.0 += 1;
    }
}

struct NestedX<'a> { f1: X<'a>, f2: X<'a> }

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn my_drop2(x: X<'_>) {
    return;
}

fn my_drop(x: X<'_>) {
    drop(x);
}

fn main() {
    let mut s = 10;
    let mut x = X(&mut s);
    my_drop(x);
    x = X(&mut s);
    my_drop2(x);
    X(&mut s); // dropped immediately
    let x = X(&mut s);
    NestedX { f1: x, f2: X(&mut s) };
    if s != 15 {
        should_not_reach();
    }
}
    "#,
    );
}

#[test]
fn drop_glue_for_type_without_drop_impl_does_not_recurse() {
    check_pass(
        r#"
//- minicore: drop, pin

struct HasDrop;
impl Drop for HasDrop {
    fn drop(&mut self) {}
}

struct WithDropGlue {
    field: HasDrop,
}

fn main() {
    let _c = WithDropGlue { field: HasDrop };
}
    "#,
    );
}

#[test]
fn drop_if_let() {
    check_pass(
        r#"
//- minicore: drop, add, option, cell, builtin_impls

use core::cell::Cell;

struct X<'a>(&'a Cell<i32>);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1)
    }
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

#[test]
fn main() {
    let s = Cell::new(0);
    let x = Some(X(&s));
    if let Some(y) = x {
        if s.get() != 0 {
            should_not_reach();
        }
        if s.get() != 0 {
            should_not_reach();
        }
    } else {
        should_not_reach();
    }
    if s.get() != 1 {
        should_not_reach();
    }
    let x = Some(X(&s));
    if let None = x {
        should_not_reach();
    } else {
        if s.get() != 1 {
            should_not_reach();
        }
    }
    if s.get() != 1 {
        should_not_reach();
    }
}
    "#,
    );
}

#[test]
fn drop_struct_field() {
    check_pass(
        r#"
//- minicore: drop, add, option, cell, builtin_impls

use core::cell::Cell;

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

struct X<'a>(&'a Cell<i32>);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1)
    }
}

struct Tuple<'a>(X<'a>, X<'a>, X<'a>);

fn main() {
    let s = Cell::new(0);
    {
        let x0 = X(&s);
        let xt = Tuple(x0, X(&s), X(&s));
        let x1 = xt.1;
        if s.get() != 0 {
            should_not_reach();
        }
        drop(xt.0);
        if s.get() != 1 {
            should_not_reach();
        }
    }
    // FIXME: this should be 3
    if s.get() != 2 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn drop_glue_of_a_long_list_of_boxes() {
    // A level of native stack a node overflowed it: smoke's const_heap drops a list of 1000.
    check_pass(
        r#"
//- minicore: drop, add, cell
#![feature(lang_items)]

use core::cell::Cell;

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

#[rustc_intrinsic]
#[rustc_intrinsic_must_be_overridden]
pub fn box_new<T>(_x: T) -> Box<T>;

#[lang = "owned_box"]
pub struct Box<T: ?Sized> {
    inner: *mut T,
}

struct X<'a>(&'a Cell<i32>);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1)
    }
}

enum List<'a> {
    Cons(X<'a>, Box<List<'a>>),
    Nil,
}

fn main() {
    let s = Cell::new(0);
    {
        let mut list = List::Nil;
        let mut i = 0;
        while i < 100000 {
            list = List::Cons(X(&s), box_new(list));
            i += 1;
        }
    }
    if s.get() != 100000 {
        should_not_reach();
    }
}
        "#,
    );
}

#[test]
fn write_box_via_move() {
    // `vec!`'s intrinsic: the value is moved into the box, which is given back.
    check_pass(
        r#"
//- minicore: drop
#![feature(lang_items)]

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

#[rustc_intrinsic]
#[rustc_intrinsic_must_be_overridden]
pub fn box_new<T>(_x: T) -> Box<T>;

#[rustc_intrinsic]
pub fn write_box_via_move<T>(b: Box<T>, x: T) -> Box<T>;

#[lang = "owned_box"]
pub struct Box<T: ?Sized> {
    inner: *mut T,
}

fn main() {
    let b = write_box_via_move(box_new(1u64), 7u64);
    if unsafe { *b.inner } != 7 {
        should_not_reach();
    }
}
        "#,
    );
}

#[test]
fn drop_glue_of_closures_tuples_arrays_and_enums() {
    // `Vec::extend_trusted` writes the length back in the `Drop` of a guard the closure it
    // passes to `for_each` captures: a closure's captures are dropped with it.
    check_pass(
        r#"
//- minicore: drop, add, option, cell, builtin_impls, fn

use core::cell::Cell;

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

struct X<'a>(&'a Cell<i32>);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1)
    }
}

fn twice<F: FnMut()>(mut f: F) {
    f();
    f();
}

fn main() {
    let s = Cell::new(0);
    let x = X(&s);
    twice(move || {
        let _ = &x;
    });
    if s.get() != 1 {
        should_not_reach();
    }
    {
        let _tuple = (X(&s), 5, X(&s));
    }
    if s.get() != 3 {
        should_not_reach();
    }
    {
        let _array = [X(&s), X(&s), X(&s)];
    }
    if s.get() != 6 {
        should_not_reach();
    }
    {
        let _some = Some(X(&s));
        let _none: Option<X<'_>> = None;
    }
    if s.get() != 7 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn drop_glue_of_projections_consumed_closures_and_signed_tags() {
    // Each drop appends its digit, so the order is checked too: a `Fn` closure called by value
    // through `FnOnce::call_once` is dropped after the call (rustc's `ClosureOnceShim`), a field
    // whose type is a projection is dropped as what it normalizes to, a `#[repr(i8)]` variant
    // `= -1` is found by its signed tag, and fields go in order.
    check_pass(
        r#"
//- minicore: drop, add, option, cell, builtin_impls, fn

use core::cell::Cell;

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

struct X<'a>(&'a Cell<i32>, i32);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        // The value times ten, then the digit (minicore has no `Mul`).
        let v = self.0.get();
        let v2 = v + v;
        let v8 = v2 + v2 + v2 + v2;
        self.0.set(v8 + v2 + self.1)
    }
}

fn once<F: FnOnce()>(f: F) {
    f()
}

trait Tr<'a> {
    type A;
}
impl<'a> Tr<'a> for () {
    type A = X<'a>;
}
struct H<'a, T: Tr<'a>>(<T as Tr<'a>>::A);

#[repr(i8)]
enum E<'a> {
    A(X<'a>) = -1,
    B = 0,
}

fn main() {
    let s = Cell::new(0);
    let x = X(&s, 1);
    once(move || {
        let _ = &x;
    });
    if s.get() != 1 {
        should_not_reach();
    }
    {
        let _h: H<'_, ()> = H(X(&s, 2));
    }
    {
        let _e = E::A(X(&s, 3));
        let _b = E::B;
    }
    {
        let _t = (X(&s, 4), X(&s, 5));
    }
    if s.get() != 12345 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn drop_in_place() {
    check_pass(
        r#"
//- minicore: drop, add, coerce_unsized
use core::ptr::drop_in_place;

struct X<'a>(&'a mut i32);
impl<'a> Drop for X<'a> {
    fn drop(&mut self) {
        *self.0 += 1;
    }
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let mut s = 2;
    let x = X(&mut s);
    drop_in_place(&mut x);
    drop(x);
    if s != 4 {
        should_not_reach();
    }
    let p: &mut [X] = &mut [X(&mut 2)];
    drop_in_place(p);
}
    "#,
    );
}

#[test]
fn drop_glue_for_trait_object() {
    check_panic(
        r#"
//- minicore: manually_drop, coerce_unsized, fmt, panic, drop, pin
use core::mem::ManuallyDrop;
use core::ptr::drop_in_place;

struct X;
impl Drop for X {
    fn drop(&mut self) {
        panic!("concrete drop ran");
    }
}

trait T {}
impl T for X {}

fn main() {
    let mut x = ManuallyDrop::new(X);
    let p = &mut x as *mut ManuallyDrop<X> as *mut X;
    let obj: &mut dyn T = unsafe { &mut *p };
    drop_in_place(obj);
}
    "#,
        "concrete drop ran",
    );
}

#[test]
fn manually_drop() {
    check_pass(
        r#"
//- minicore: manually_drop
use core::mem::ManuallyDrop;

struct X;
impl Drop for X {
    fn drop(&mut self) {
        should_not_reach();
    }
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let x = ManuallyDrop::new(X);
}
    "#,
    );
}

#[test]
fn generic_impl_for_trait_with_generic_method() {
    check_pass(
        r#"
//- minicore: drop
struct S<T>(T);

trait Tr {
    fn f<F>(&self, x: F);
}

impl<T> Tr for S<T> {
    fn f<F>(&self, x: F) {
    }
}

fn main() {
    let s = S(1u8);
    s.f(5i64);
}
        "#,
    );
}

#[test]
fn region_infer_var_does_not_escape_impl_lookup() {
    check_pass(
        r#"
//- minicore: fn, sized

trait Bound<T> {}
trait Trait { fn f(self); }

struct S;

impl<'a> Bound<&'a ()> for S {}
impl<'a, T: Bound<&'a ()>> Trait for T {
    fn f(self) { make::<'a>(); }
}

fn make<'a>() -> impl Fn(&'a ()) { |_| {} }

fn main() { S.f(); }
"#,
    );
}

#[test]
fn index_of_slice_should_preserve_len() {
    check_pass(
        r#"
//- minicore: index, slice, coerce_unsized

struct X;

impl core::ops::Index<X> for [i32] {
    type Output = i32;

    fn index(&self, _: X) -> &i32 {
        if self.len() != 3 {
            should_not_reach();
        }
        &self[0]
    }
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let x: &[i32] = &[1, 2, 3];
    &x[X];
}
        "#,
    );
}

#[test]
fn memcmp() {
    check_pass(
        r#"
//- minicore: slice, coerce_unsized, index

fn should_not_reach() -> bool {
    _ // FIXME: replace this function with panic when that works
}

extern "C" {
    fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32;
}

fn my_cmp(x: &[u8; 3], y: &[u8; 3]) -> i32 {
    memcmp(x as *const u8, y as *const u8, x.len())
}

fn main() {
    if my_cmp(&[1, 2, 3], &[1, 2, 3]) != 0 {
        should_not_reach();
    }
    if my_cmp(&[1, 20, 3], &[1, 2, 3]) <= 0 {
        should_not_reach();
    }
    if my_cmp(&[1, 2, 3], &[1, 20, 3]) >= 0 {
        should_not_reach();
    }
}
    "#,
    );
}

#[test]
fn unix_write_stdout() {
    check_pass_and_stdio(
        r#"
//- minicore: slice, index, coerce_unsized

type pthread_key_t = u32;
type c_void = u8;
type c_int = i32;

extern "C" {
    pub fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}

fn main() {
    let stdout = b"stdout";
    let stderr = b"stderr";
    write(1, &stdout[0], 6);
    write(2, &stderr[0], 6);
}
        "#,
        "stdout",
        "stderr",
    );
}

#[test]
fn closure_layout_in_rpit() {
    check_pass(
        r#"
//- minicore: fn

fn f<F: Fn()>(x: F) {
    fn g(x: impl Fn()) -> impl FnOnce() {
        move || {
            x();
        }
    }
    g(x)();
}

fn main() {
    f(|| {});
}
        "#,
    );
}

#[test]
fn from_fn() {
    check_pass(
        r#"
//- minicore: fn, iterator, sized
struct FromFn<F>(F);

impl<T, F: FnMut() -> Option<T>> Iterator for FromFn<F> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        (self.0)()
    }
}

fn main() {
    let mut tokenize = {
        FromFn(move || Some(2))
    };
    let s = tokenize.next();
}
        "#,
    );
}

#[test]
fn for_loop() {
    check_pass(
        r#"
//- minicore: iterator, add, builtin_impls
fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

struct X;
struct XIter(i32);

impl IntoIterator for X {
    type Item = i32;

    type IntoIter = XIter;

    fn into_iter(self) -> Self::IntoIter {
        XIter(0)
    }
}

impl Iterator for XIter {
    type Item = i32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0 == 5 {
            None
        } else {
            self.0 += 1;
            Some(self.0)
        }
    }
}

fn main() {
    let mut s = 0;
    for x in X {
        s += x;
    }
    if s != 15 {
        should_not_reach();
    }
}
        "#,
    );
}

#[test]
fn field_with_associated_type() {
    check_pass(
        r#"
//- /b/mod.rs crate:b
pub trait Tr {
    fn f(self);
}

pub trait Tr2 {
    type Ty: Tr;
}

pub struct S<T: Tr2> {
    pub t: T::Ty,
}

impl<T: Tr2> S<T> {
    pub fn g(&self) {
        let k = (self.t, self.t);
        self.t.f();
    }
}

//- /a/mod.rs crate:a deps:b
use b::{Tr, Tr2, S};

struct A(i32);
struct B(u8);

impl Tr for A {
    fn f(&self) {
    }
}

impl Tr2 for B {
    type Ty = A;
}

#[test]
fn main() {
    let s: S<B> = S { t: A(2) };
    s.g();
}
    "#,
    );
}

#[test]
fn specialization_array_clone() {
    check_pass(
        r#"
//- minicore: copy, derive, slice, index, coerce_unsized, panic
#![feature(min_specialization)]

impl<T: Clone, const N: usize> Clone for [T; N] {
    #[inline]
    fn clone(&self) -> Self {
        SpecArrayClone::clone(self)
    }
}

trait SpecArrayClone: Clone {
    fn clone<const N: usize>(array: &[Self; N]) -> [Self; N];
}

impl<T: Clone> SpecArrayClone for T {
    #[inline]
    default fn clone<const N: usize>(array: &[T; N]) -> [T; N] {
        panic!("should go to the specialized impl")
    }
}

fn from_slice<T, const N: usize>(s: &[T]) -> [T; N] {
    [s[0]; N]
}

impl<T: Copy> SpecArrayClone for T {
    #[inline]
    fn clone<const N: usize>(array: &[T; N]) -> [T; N] {
        *array
    }
}

#[derive(Clone, Copy)]
struct X(i32);

fn main() {
    let ar = [X(1), X(2)];
    ar.clone();
}
        "#,
    );
}

#[test]
fn short_circuit_operator() {
    check_pass(
        r#"
fn should_not_reach() -> bool {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    if false && should_not_reach() {
        should_not_reach();
    }
    true || should_not_reach();

}
    "#,
    );
}

#[test]
fn closure_state() {
    check_pass(
        r#"
//- minicore: fn, add, copy, builtin_impls
fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let mut x = 2;
    let mut c = move || {
        x += 1;
        x
    };
    c();
    c();
    c();
    if x != 2 {
        should_not_reach();
    }
    if c() != 6 {
        should_not_reach();
    }
}
        "#,
    );
}

#[test]
fn closure_capture_array_const_generic() {
    check_pass(
        r#"
//- minicore: fn, add, copy
struct X(i32);

fn f<const N: usize>(mut x: [X; N]) { // -> impl FnOnce() {
    let c = || {
        x;
    };
    c();
}

fn main() {
    let s = f([X(1)]);
    //s();
}
        "#,
    );
}

#[test]
fn self_with_capital_s() {
    check_pass(
        r#"
//- minicore: fn, add, copy

struct S1;

impl S1 {
    fn f() {
        Self;
    }
}

struct S2 {
    f1: i32,
}

impl S2 {
    fn f() {
        Self { f1: 5 };
    }
}

struct S3(i32);

impl S3 {
    fn f() {
        Self(2);
        Self;
        let this = Self;
        this(2);
    }
}

fn main() {
    S1::f();
    S2::f();
    S3::f();
}
        "#,
    );
}

fn getrandom_syscall(arch: &str, number: i64) -> String {
    format!(
        r#"
//- minicore: option
//- /main.rs cfg:target_os=linux,target_arch={arch}

extern "C" {{
    pub unsafe extern "C" fn syscall(num: i64, ...) -> i64;
}}

const SYS_getrandom: i64 = {number};

fn should_not_reach() {{
    _ // FIXME: replace this function with panic when that works
}}

fn main() {{
    let mut x: i32 = 0;
    let r = syscall(SYS_getrandom, &mut x, 4usize, 0);
    if r != 4 {{
        should_not_reach();
    }}
}}
"#
    )
}

#[test]
fn syscalls() {
    check_pass(&getrandom_syscall("x86_64", 318));
    check_pass(&getrandom_syscall("aarch64", 278));
}

#[test]
fn syscall_numbers_are_the_targets() {
    check_error_with(
        &getrandom_syscall("aarch64", 318),
        |e| matches!(innermost(&e), MirEvalError::NotSupported(s) if s.contains("syscall id 318")),
    );
    check_error_with(
        r#"
//- minicore: option
//- /main.rs cfg:target_os=linux
extern "C" {
    pub unsafe extern "C" fn syscall(num: i64, ...) -> i64;
}
fn main() {
    let mut x: i32 = 0;
    syscall(318, &mut x, 4usize, 0);
}
"#,
        |e| matches!(innermost(&e), MirEvalError::NotSupported(s) if s.contains("target_arch")),
    );
}

#[test]
fn cc_random_generate_bytes() {
    check_pass(
        r#"
//- minicore: option
extern "C" {
    fn CCRandomGenerateBytes(bytes: *mut u8, count: usize) -> i32;
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let mut x = [0u8; 16];
    if unsafe { CCRandomGenerateBytes(&mut x as *mut [u8; 16] as *mut u8, 16) } != 0 {
        should_not_reach();
    }
}
"#,
    )
}

#[test]
fn posix_getenv() {
    check_pass(
        r#"
//- minicore: sized
//- /main.rs env:foo=bar

type c_char = u8;

extern "C" {
    pub fn getenv(s: *const c_char) -> *mut c_char;
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let result = getenv(b"foo\0" as *const _);
    if *result != b'b' {
        should_not_reach();
    }
    let result = (result as usize + 1) as *const c_char;
    if *result != b'a' {
        should_not_reach();
    }
    let result = (result as usize + 1) as *const c_char;
    if *result != b'r' {
        should_not_reach();
    }
    let result = (result as usize + 1) as *const c_char;
    if *result != 0 {
        should_not_reach();
    }
    let result = getenv(b"not found\0" as *const _);
    if result as usize != 0 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn posix_tls() {
    check_pass(
        r#"
//- minicore: option
//- /main.rs cfg:target_os=linux,target_arch=x86_64

type pthread_key_t = u32;
type c_void = u8;
type c_int = i32;

extern "C" {
    pub fn pthread_key_create(
        key: *mut pthread_key_t,
        dtor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    pub fn pthread_key_delete(key: pthread_key_t) -> c_int;
    pub fn pthread_getspecific(key: pthread_key_t) -> *mut c_void;
    pub fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> c_int;
}

fn main() {
    let mut key = 2;
    pthread_key_create(&mut key, None);
}
        "#,
    );
}

#[test]
fn regression_14966() {
    check_pass(
        r#"
//- minicore: fn, copy, coerce_unsized, dispatch_from_dyn
trait A<T> {
    fn a(&self) {}
}
impl A<()> for () {}

struct B;
impl B {
    pub fn b<T>(s: &dyn A<T>) -> Self {
        B
    }
}
struct C;
impl C {
    fn c<T>(a: &dyn A<T>) -> Self {
        let mut c = C;
        let b = B::b(a);
        c.d(|| a.a());
        c
    }
    fn d(&mut self, f: impl FnOnce()) {}
}

fn main() {
    C::c(&());
}
"#,
    );
}

#[test]
fn long_str_eq_same_prefix() {
    check_pass_and_stdio(
        r#"
//- minicore: slice, index, coerce_unsized

type pthread_key_t = u32;
type c_void = u8;
type c_int = i32;

extern "C" {
    pub fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}

fn main() {
    // More than 16 bytes, the size of `i128`.
    let long_str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab";
    let output = match long_str {
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" => b"true" as &[u8],
        _ => b"false",
    };
    write(1, &output[0], output.len());
}
        "#,
        "false",
        "",
    );
}

#[test]
fn regression_19021() {
    check_pass(
        r#"
//- minicore: deref
use core::ops::Deref;

#[lang = "owned_box"]
struct Box<T>(T);

impl<T> Deref for Box<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct Foo;

fn main() {
    let x = Box(Foo);
    let y = &Foo;

    || match x {
        ref x => x,
        _ => y,
    };
}
"#,
    );
}

#[test]
fn regression_19177() {
    check_error_with(
        r#"
//- minicore: copy
trait Foo {}
trait Bar {}
trait Baz {}
trait Qux {
    type Assoc;
}

fn main<'a, T: Foo + Bar + Baz>(
    x: &T,
    y: (),
    z: &'a dyn Qux<Assoc = T>,
    w: impl Foo + Bar,
) {
}
"#,
        |e| matches!(e, MirEvalError::MirLowerError(_, MirLowerError::GenericArgNotProvided(..))),
    );
}

#[test]
fn derive_from() {
    // Nightly's `#[derive(From)]` (#600): `From` of the one field's type, for a tuple struct, a
    // struct with a named field, and a generic one.
    check_pass(
        r#"
//- minicore: derive_from, from, sized
use core::from::From;

#[derive(From)]
struct Meters(u32);

#[derive(From)]
struct Named { inner: i64 }

#[derive(From)]
struct Pair<T>((T, T));

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let meters: Meters = 5u32.into();
    let named = Named::from(-7);
    let pair: Pair<u8> = (1, 2).into();
    if meters.0 != 5 || named.inner != -7 || (pair.0).0 != 1 || (pair.0).1 != 2 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn statement_macro_in_the_parentheses_of_another() {
    // rustc parses `twice!(n += 1;)` with no `;` after it as an expression, so the outer
    // call's expansion, two calls with nothing between, does not parse (#154).
    check_error_with(
        r#"
macro_rules! twice {
    ($($body:tt)*) => {
        $($body)*
        $($body)*
    };
}

fn main() {
    let mut n = 0u32;
    twice!(twice!(n += 1;));
}
"#,
        |e| {
            matches!(
                e,
                MirEvalError::MirLowerError(_, MirLowerError::NotSupported(why))
                    if why.starts_with("macro call in parentheses or brackets with no `;` after it")
            )
        },
    );
    // Braced, each call is a statement of its own, and both compile it.
    check_pass(
        r#"
//- minicore: panic
macro_rules! twice {
    ($($body:tt)*) => {
        $($body)*
        $($body)*
    };
}

fn main() {
    let mut n = 0u32;
    twice! { twice! { n += 1; } }
    if n != 4 {
        panic!();
    }
}
"#,
    );
}

#[test]
fn expression_macro_of_one_expression_and_a_semicolon() {
    // rustc parses `bail!(1)` last in a block as an expression and takes the `;` after the
    // one expression it expands to: the call's value is that expression. It warns of it for
    // another crate's macro.
    check_pass(
        r#"
//- minicore: panic, result
//- /m.rs crate:m
#[macro_export]
macro_rules! val { () => { 5u32; } }
#[macro_export]
macro_rules! bail { ($e:expr) => { return Err($e); } }
#[macro_export]
macro_rules! inner { () => { $crate::val!() } }
//- /main.rs crate:main deps:m
fn f(c: bool) -> Result<u32, u32> {
    if c { m::bail!(1) } else { Ok(m::val!()) }
}

fn main() {
    let v: u32 = { m::val!() };
    let w: u32 = { m::inner!() };
    let ok = match f(false) { Ok(it) => it, Err(_) => 0 };
    let err = match f(true) { Ok(_) => 0, Err(it) => it };
    if v != 5 || w != 5 || ok != 5 || err != 1 {
        panic!();
    }
}
"#,
    );
    // A macro of the crate's own has the same value. rustc's `semicolon_in_expressions_from_macros`
    // denies it by default and `#[allow]` lets it build: a lint, reported by blaze-diagnostics
    // (`lints/syntax/early/macros.rs`), not a refusal of the evaluator.
    check_pass(
        r#"
//- minicore: panic
macro_rules! val { () => { 5u32; } }

fn main() {
    let v: u32 = { val!() };
    if v != 5 {
        panic!();
    }
}
"#,
    );
}

#[test]
fn statement_macro_last_in_another_statement_macro() {
    // A call last in another's expansion ends where the expansion does: it is a statement
    // macro where the outer call was one (`m!(..);`), and its expansion may be statements,
    // as rustc accepts.
    check_pass(
        r#"
//- minicore: panic
macro_rules! two { ($($t:tt)*) => { $($t)* $($t)* } }
macro_rules! outer { ($n:ident) => { two!($n += 1;) } }
macro_rules! bind { ($n:ident) => { let $n = 42u32; } }
macro_rules! binds { ($n:ident) => { bind!($n) } }
macro_rules! nop { () => {} }
macro_rules! nops { () => { let _x = 1; nop!() } }

fn main() {
    let mut n = 0u32;
    outer!(n);
    if n != 2 {
        panic!();
    }
    binds!(x);
    if x != 42 {
        panic!();
    }
    nops!();
}
"#,
    );
    // Where the outer call is an expression, so is one last in its expansion: rustc rejects
    // `outer!()` as the value of a block when it expands to a call of several statements, whose
    // expansion does not parse as the expression it is expanded as: the value is missing.
    check_error_with(
        r#"
macro_rules! two { ($($t:tt)*) => { $($t)* $($t)* } }
macro_rules! outer { () => { two!(n += 1;) } }

fn main() {
    let mut n = 0u32;
    let _ = { outer!() };
}
"#,
        |e| matches!(e, MirEvalError::MirLowerError(_, MirLowerError::IncompleteExpr)),
    );
}

#[test]
fn format_args_pass() {
    check_pass(
        r#"
//- minicore: fmt
fn main() {
    let x1 = format_args!("");
    let x2 = format_args!("{}", x1);
    let x3 = format_args!("{} {}", x1, x2);
}
"#,
    );
}

#[test]
fn fabs_intrinsic() {
    check_pass(
        r#"
//- minicore: copy, panic
pub unsafe trait FloatPrimitive: Sized + Copy {}
unsafe impl FloatPrimitive for f32 {}
unsafe impl FloatPrimitive for f64 {}

#[rustc_intrinsic]
fn fabs<T: FloatPrimitive>(x: T) -> T;

fn should_not_reach() { panic!() }

fn main() {
    if fabs(-3.5f32) != 3.5f32 {
        should_not_reach();
    }
    if fabs(3.5f32) != 3.5f32 {
        should_not_reach();
    }
    if fabs(-3.5f64) != 3.5f64 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn slice_get_unchecked_intrinsic() {
    check_pass(
        r#"
//- minicore: panic
#[rustc_intrinsic]
unsafe fn slice_get_unchecked<ItemPtr, SlicePtr, T>(
    slice_ptr: SlicePtr,
    index: usize,
) -> ItemPtr;

fn should_not_reach() { panic!() }

fn main() {
    let values = [10, 20, 30];
    let slice_ptr = &values as *const [i32];
    let item_ptr = unsafe {
        slice_get_unchecked::<*const i32, *const [i32], i32>(slice_ptr, 1)
    };
    if unsafe { *item_ptr } != 20 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn slice_get_unchecked_out_of_bounds() {
    check_error_with(
        r#"
#[rustc_intrinsic]
unsafe fn slice_get_unchecked<ItemPtr, SlicePtr, T>(
    slice_ptr: SlicePtr,
    index: usize,
) -> ItemPtr;

fn main() {
    let values = [()];
    let slice_ptr = &values as *const [()];
    let _item = unsafe {
        slice_get_unchecked::<*const (), *const [()], ()>(slice_ptr, 1)
    };
}
"#,
        |e| {
            let mut err = &e;
            while let MirEvalError::InFunction(inner, _) = err {
                err = inner;
            }
            matches!(err, MirEvalError::UndefinedBehavior(_))
        },
    );
}

#[test]
fn slice_get_unchecked_const_slice() {
    check_pass(
        r#"
//- minicore: panic
#[rustc_intrinsic]
unsafe fn slice_get_unchecked<ItemPtr, SlicePtr, T>(
    slice_ptr: SlicePtr,
    index: usize,
) -> ItemPtr;

struct Flag {
    name: &'static str,
    value: u16,
}

const PURE: &str = "PURE";
const NOMEM: &str = "NOMEM";
const READONLY: &str = "READONLY";
const PRESERVES_FLAGS: &str = "PRESERVES_FLAGS";
const NORETURN: &str = "NORETURN";
const NOSTACK: &str = "NOSTACK";
const ATT_SYNTAX: &str = "ATT_SYNTAX";
const FLAGS: &[Flag] = &[
    Flag { name: PURE, value: 1 },
    Flag { name: NOMEM, value: 2 },
    Flag { name: READONLY, value: 4 },
    Flag { name: PRESERVES_FLAGS, value: 8 },
    Flag { name: NORETURN, value: 16 },
    Flag { name: NOSTACK, value: 32 },
    Flag { name: ATT_SYNTAX, value: 64 },
];

fn should_not_reach() { panic!() }

fn main() {
    let flag = unsafe {
        slice_get_unchecked::<&Flag, &[Flag], Flag>(FLAGS, 6)
    };
    let name = flag.name as *const str as *const u8;
    if unsafe { *name } != b'A' || flag.value != 64 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn unreachable_intrinsic() {
    check_error_with(
        r#"
#[rustc_intrinsic]
fn unreachable() -> !;

fn main() {
    unreachable();
}
"#,
        |e| {
            let mut err = &e;
            while let MirEvalError::InFunction(inner, _) = err {
                err = inner;
            }
            matches!(err, MirEvalError::UndefinedBehavior(_))
        },
    );
}

#[test]
fn caller_location_intrinsic() {
    // `Location::caller` tracks its caller, so it is where `main` calls it, counted from 1.
    check_pass(
        r#"
//- minicore: panic_location
fn should_not_reach() {
    panic!()
}

fn main() {
    let loc = core::panic::Location::caller();
    if loc.line() != 6 || loc.column() != 15 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn copy_intrinsics() {
    // `copy` and `volatile_copy_memory` take overlapping ranges either way; the argument order
    // of the volatile ones is `dst, src`.
    check_pass(
        r#"
//- minicore: coerce_unsized, index, slice, panic
#[rustc_intrinsic]
pub unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize);
#[rustc_intrinsic]
pub unsafe fn copy_nonoverlapping<T>(src: *const T, dst: *mut T, count: usize);
#[rustc_intrinsic]
pub unsafe fn volatile_copy_memory<T>(dst: *mut T, src: *const T, count: usize);
#[rustc_intrinsic]
pub unsafe fn volatile_copy_nonoverlapping_memory<T>(dst: *mut T, src: *const T, count: usize);

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut x = [1i32, 2, 3, 4, 5];
        let p = &mut x as *mut [i32; 5] as *mut i32;
        copy((p as usize + 4) as *const i32, p, 3);
        if x[0] != 2 || x[1] != 3 || x[2] != 4 || x[3] != 4 || x[4] != 5 {
            should_not_reach();
        }
        copy(p as *const i32, (p as usize + 8) as *mut i32, 3);
        if x[0] != 2 || x[1] != 3 || x[2] != 2 || x[3] != 3 || x[4] != 4 {
            should_not_reach();
        }
        volatile_copy_memory((p as usize + 4) as *mut i32, p as *const i32, 4);
        if x[0] != 2 || x[1] != 2 || x[2] != 3 || x[3] != 2 || x[4] != 3 {
            should_not_reach();
        }

        let src = [(1u8, 2u16), (3, 4)];
        let mut dst = [(0u8, 0u16); 3];
        let s = &src as *const [(u8, u16); 2] as *const (u8, u16);
        let d = &mut dst as *mut [(u8, u16); 3] as *mut (u8, u16);
        copy_nonoverlapping(s, (d as usize + 4) as *mut (u8, u16), 2);
        if dst[0].0 != 0 || dst[0].1 != 0 || dst[1].0 != 1 || dst[1].1 != 2 {
            should_not_reach();
        }
        if dst[2].0 != 3 || dst[2].1 != 4 {
            should_not_reach();
        }
        copy_nonoverlapping(s, d, 0);
        if dst[0].0 != 0 || dst[0].1 != 0 {
            should_not_reach();
        }

        let mut y = 0u64;
        volatile_copy_nonoverlapping_memory(&mut y, &0x1122_3344_5566_7788u64, 1);
        if y != 0x1122_3344_5566_7788 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn write_bytes_intrinsics() {
    check_pass(
        r#"
//- minicore: coerce_unsized, index, slice, panic
#[rustc_intrinsic]
pub unsafe fn write_bytes<T>(dst: *mut T, val: u8, count: usize);
#[rustc_intrinsic]
pub unsafe fn volatile_set_memory<T>(dst: *mut T, val: u8, count: usize);

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut x = [0x1111_1111u32; 4];
        let p = &mut x as *mut [u32; 4] as *mut u32;
        write_bytes((p as usize + 4) as *mut u32, 0xab, 2);
        if x[0] != 0x1111_1111 || x[1] != 0xabab_abab || x[2] != 0xabab_abab {
            should_not_reach();
        }
        if x[3] != 0x1111_1111 {
            should_not_reach();
        }
        volatile_set_memory(p, 0, 1);
        if x[0] != 0 || x[1] != 0xabab_abab || x[3] != 0x1111_1111 {
            should_not_reach();
        }
        write_bytes(p, 0xff, 0);
        if x[0] != 0 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn read_and_write_intrinsics() {
    // The unaligned ones read and write a `u32` at an odd address.
    check_pass(
        r#"
//- minicore: coerce_unsized, index, slice, panic
#[rustc_intrinsic]
pub unsafe fn read_via_copy<T>(ptr: *const T) -> T;
#[rustc_intrinsic]
pub unsafe fn write_via_move<T>(ptr: *mut T, value: T);
#[rustc_intrinsic]
pub unsafe fn volatile_load<T>(src: *const T) -> T;
#[rustc_intrinsic]
pub unsafe fn volatile_store<T>(dst: *mut T, val: T);
#[rustc_intrinsic]
pub unsafe fn unaligned_volatile_load<T>(src: *const T) -> T;
#[rustc_intrinsic]
pub unsafe fn unaligned_volatile_store<T>(dst: *mut T, val: T);

struct Pair {
    a: u8,
    b: u32,
}

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut x = 0x1234_5678u32;
        if read_via_copy(&x) != 0x1234_5678 {
            should_not_reach();
        }
        write_via_move(&mut x, 7);
        if x != 7 {
            should_not_reach();
        }
        volatile_store(&mut x, 0xffff_fffe);
        if x != 0xffff_fffe || volatile_load(&x) != 0xffff_fffe {
            should_not_reach();
        }

        let mut p = Pair { a: 1, b: 2 };
        let q = read_via_copy(&p);
        if q.a != 1 || q.b != 2 {
            should_not_reach();
        }
        write_via_move(&mut p, Pair { a: 3, b: 4 });
        if p.a != 3 || p.b != 4 {
            should_not_reach();
        }
        volatile_store(&mut p, Pair { a: 5, b: 6 });
        let r = volatile_load(&p);
        if r.a != 5 || r.b != 6 {
            should_not_reach();
        }

        let mut bytes = [0u8; 8];
        let base = &mut bytes as *mut [u8; 8] as usize;
        unaligned_volatile_store((base + 1) as *mut u32, 0x0403_0201);
        if bytes[0] != 0 || bytes[1] != 1 || bytes[2] != 2 || bytes[3] != 3 || bytes[4] != 4 {
            should_not_reach();
        }
        if bytes[5] != 0 {
            should_not_reach();
        }
        if unaligned_volatile_load((base + 1) as *const u32) != 0x0403_0201 {
            should_not_reach();
        }
        if unaligned_volatile_load((base + 2) as *const u16) != 0x0302 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn typed_swap_nonoverlapping_intrinsic() {
    // Two halves of one array touch but do not overlap; a zero-sized value may swap with itself.
    check_pass(
        r#"
//- minicore: coerce_unsized, index, slice, panic
#[rustc_intrinsic]
pub unsafe fn typed_swap_nonoverlapping<T>(x: *mut T, y: *mut T);

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut a = (1u8, 0x0102_0304_0506_0708u64);
        let mut b = (9u8, 7u64);
        typed_swap_nonoverlapping(&mut a, &mut b);
        if a.0 != 9 || a.1 != 7 || b.0 != 1 || b.1 != 0x0102_0304_0506_0708 {
            should_not_reach();
        }

        let mut x = [1u32, 2, 3, 4];
        let p = &mut x as *mut [u32; 4] as *mut [u32; 2];
        typed_swap_nonoverlapping(p, (p as usize + 8) as *mut [u32; 2]);
        if x[0] != 3 || x[1] != 4 || x[2] != 1 || x[3] != 2 {
            should_not_reach();
        }

        let mut u = ();
        let pu = &mut u as *mut ();
        typed_swap_nonoverlapping(pu, pu);
    }
}
"#,
    );
}

#[test]
fn typed_swap_nonoverlapping_of_overlapping_ranges() {
    check_error_with(
        r#"
#[rustc_intrinsic]
pub unsafe fn typed_swap_nonoverlapping<T>(x: *mut T, y: *mut T);

fn main() {
    let mut x = [1u32, 2, 3];
    let p = &mut x as *mut [u32; 3] as *mut [u32; 2];
    unsafe {
        typed_swap_nonoverlapping(p, (p as usize + 4) as *mut [u32; 2]);
    }
}
"#,
        |e| {
            let mut err = &e;
            while let MirEvalError::InFunction(inner, _) = err {
                err = inner;
            }
            matches!(err, MirEvalError::UndefinedBehavior(_))
        },
    );
}

#[test]
fn raw_eq_intrinsic() {
    check_pass(
        r#"
//- minicore: panic
#[rustc_intrinsic]
pub unsafe fn raw_eq<T>(a: &T, b: &T) -> bool;

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        if !raw_eq(&[1u8, 2, 3], &[1u8, 2, 3]) {
            should_not_reach();
        }
        if raw_eq(&[1u8, 2, 3], &[1u8, 2, 4]) {
            should_not_reach();
        }
        if raw_eq(&0x0102u16, &0x0201u16) {
            should_not_reach();
        }
        if !raw_eq(&[7u64, 9], &[7u64, 9]) || raw_eq(&[7u64, 9], &[7u64, 8]) {
            should_not_reach();
        }
        if !raw_eq(&(), &()) {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn ptr_mask_intrinsic() {
    // A `[u32; 4]` is at an address aligned to 4, so masking the low two bits of an address
    // inside its third element gives that element.
    check_pass(
        r#"
//- minicore: panic
#[rustc_intrinsic]
pub fn ptr_mask<T>(ptr: *const T, mask: usize) -> *const T;

fn should_not_reach() { panic!() }

fn main() {
    let x = [10u32, 20, 30, 40];
    let p = &x as *const [u32; 4] as *const u32;
    unsafe {
        let masked = ptr_mask((p as usize + 9) as *const u32, !3);
        if masked as usize != p as usize + 8 || *masked != 30 {
            should_not_reach();
        }
        let same = ptr_mask(p, !0);
        if same as usize != p as usize || *same != 10 {
            should_not_reach();
        }
        if ptr_mask(p, 0) as usize != 0 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn compare_bytes_intrinsic() {
    // The bytes compare unsigned, and the order is -1, 0 or 1.
    check_pass(
        r#"
//- minicore: panic
#[rustc_intrinsic]
pub unsafe fn compare_bytes(left: *const u8, right: *const u8, bytes: usize) -> i32;

fn should_not_reach() { panic!() }

fn main() {
    let a = [1u8, 2, 3, 200];
    let b = [1u8, 2, 4, 0];
    let pa = &a as *const [u8; 4] as *const u8;
    let pb = &b as *const [u8; 4] as *const u8;
    unsafe {
        if compare_bytes(pa, pb, 4) != -1 || compare_bytes(pb, pa, 4) != 1 {
            should_not_reach();
        }
        if compare_bytes(pa, pb, 2) != 0 || compare_bytes(pa, pb, 0) != 0 {
            should_not_reach();
        }
        let a3 = (pa as usize + 3) as *const u8;
        let b3 = (pb as usize + 3) as *const u8;
        if compare_bytes(a3, b3, 1) != 1 || compare_bytes(b3, a3, 1) != -1 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn offset_and_arith_offset_intrinsics() {
    // `offset` by a negative `isize`, and `arith_offset` far past the end and back.
    check_pass(
        r#"
//- minicore: panic
#[rustc_intrinsic]
pub unsafe fn offset<Ptr, Delta>(dst: Ptr, offset: Delta) -> Ptr;
#[rustc_intrinsic]
pub unsafe fn arith_offset<T>(dst: *const T, offset: isize) -> *const T;

fn should_not_reach() { panic!() }

fn main() {
    let x = [10u16, 20, 30, 40, 50];
    let first = &x as *const [u16; 5] as *const u16;
    unsafe {
        let last = offset(first, 4usize);
        if *last != 50 || last as usize != first as usize + 8 {
            should_not_reach();
        }
        let back = offset(last, -3isize);
        if *back != 20 || back as usize != first as usize + 2 {
            should_not_reach();
        }
        if *offset(back, -1isize) != 10 {
            should_not_reach();
        }

        let far = arith_offset(first, 1000);
        if far as usize != first as usize + 2000 {
            should_not_reach();
        }
        let near = arith_offset(far, -997);
        if *near != 40 || near as usize != first as usize + 6 {
            should_not_reach();
        }
        let before = arith_offset(first, -5);
        if before as usize != first as usize - 10 || *arith_offset(before, 7) != 30 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn extern_c_memory_functions() {
    // `memcpy`, `memmove` and `memset` return their destination; `memset` writes the low byte
    // of its `c_int`.
    check_pass(
        r#"
//- minicore: coerce_unsized, index, slice, panic
type c_void = u8;
type c_int = i32;

extern "C" {
    fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
    fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void;
    fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
    fn strlen(s: *const u8) -> usize;
}

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let src = [1u8, 2, 3, 4];
        let mut dst = [0u8; 6];
        let s = &src as *const [u8; 4] as *const u8;
        let d = &mut dst as *mut [u8; 6] as *mut u8;
        let r = memcpy((d as usize + 1) as *mut u8, s, 4);
        if r as usize != d as usize + 1 {
            should_not_reach();
        }
        if dst[0] != 0 || dst[1] != 1 || dst[2] != 2 || dst[3] != 3 || dst[4] != 4 || dst[5] != 0 {
            should_not_reach();
        }

        let mut buf = [1u8, 2, 3, 4, 5, 6];
        let b = &mut buf as *mut [u8; 6] as *mut u8;
        let r = memmove((b as usize + 2) as *mut u8, b, 4);
        if r as usize != b as usize + 2 {
            should_not_reach();
        }
        if buf[0] != 1 || buf[1] != 2 || buf[2] != 1 || buf[3] != 2 || buf[4] != 3 || buf[5] != 4 {
            should_not_reach();
        }
        let r = memmove(b, (b as usize + 1) as *const u8, 5);
        if r as usize != b as usize {
            should_not_reach();
        }
        if buf[0] != 2 || buf[1] != 1 || buf[2] != 2 || buf[3] != 3 || buf[4] != 4 || buf[5] != 4 {
            should_not_reach();
        }

        let mut z = [7u8; 5];
        let p = &mut z as *mut [u8; 5] as *mut u8;
        let r = memset((p as usize + 1) as *mut u8, 0x1ab, 3);
        if r as usize != p as usize + 1 {
            should_not_reach();
        }
        if z[0] != 7 || z[1] != 0xab || z[2] != 0xab || z[3] != 0xab || z[4] != 7 {
            should_not_reach();
        }
        let r = memset(p, 0, 0);
        if r as usize != p as usize || z[0] != 7 {
            should_not_reach();
        }

        let other = [1u8, 2, 9, 0];
        let o = &other as *const [u8; 4] as *const u8;
        if memcmp(s, o, 4) >= 0 || memcmp(o, s, 3) <= 0 {
            should_not_reach();
        }
        if memcmp(s, o, 2) != 0 || memcmp(s, o, 0) != 0 {
            should_not_reach();
        }

        let hello = b"hello\0";
        if strlen(hello as *const _) != 5 || strlen(b"\0" as *const _) != 0 {
            should_not_reach();
        }
        if strlen((hello as *const [u8; 6] as usize + 2) as *const u8) != 3 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn suffix_atomic_loads_stores_exchanges_and_fences() {
    check_pass(
        r#"
//- minicore: copy, panic
#[rustc_intrinsic]
pub unsafe fn atomic_load_relaxed<T: Copy>(src: *const T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_load_acquire<T: Copy>(src: *const T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_load_seqcst<T: Copy>(src: *const T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_store_relaxed<T: Copy>(dst: *mut T, val: T);
#[rustc_intrinsic]
pub unsafe fn atomic_store_release<T: Copy>(dst: *mut T, val: T);
#[rustc_intrinsic]
pub unsafe fn atomic_store_seqcst<T: Copy>(dst: *mut T, val: T);
#[rustc_intrinsic]
pub unsafe fn atomic_xchg_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xchg_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xchg_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xchg_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xchg_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_fence_acquire();
#[rustc_intrinsic]
pub unsafe fn atomic_fence_release();
#[rustc_intrinsic]
pub unsafe fn atomic_fence_acqrel();
#[rustc_intrinsic]
pub unsafe fn atomic_fence_seqcst();
#[rustc_intrinsic]
pub unsafe fn atomic_singlethreadfence_acquire();
#[rustc_intrinsic]
pub unsafe fn atomic_singlethreadfence_release();
#[rustc_intrinsic]
pub unsafe fn atomic_singlethreadfence_acqrel();
#[rustc_intrinsic]
pub unsafe fn atomic_singlethreadfence_seqcst();

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut a = 0u8;
        atomic_store_relaxed(&mut a, 200);
        if a != 200 || atomic_load_relaxed(&a) != 200 {
            should_not_reach();
        }
        atomic_fence_acquire();
        if atomic_xchg_relaxed(&mut a, 7) != 200 || a != 7 {
            should_not_reach();
        }
        if atomic_xchg_seqcst(&mut a, 255) != 7 || atomic_load_seqcst(&a) != 255 {
            should_not_reach();
        }

        let mut b = 0u32;
        atomic_store_release(&mut b, 0xdead_beef);
        atomic_fence_release();
        if b != 0xdead_beef || atomic_load_acquire(&b) != 0xdead_beef {
            should_not_reach();
        }
        if atomic_xchg_acquire(&mut b, 1) != 0xdead_beef || b != 1 {
            should_not_reach();
        }

        let mut c = 0usize;
        atomic_store_seqcst(&mut c, !0usize);
        atomic_fence_acqrel();
        if c != !0usize || atomic_load_seqcst(&c) != !0usize {
            should_not_reach();
        }
        if atomic_xchg_release(&mut c, 12) != !0usize || atomic_load_relaxed(&c) != 12 {
            should_not_reach();
        }

        let mut d = 0i32;
        atomic_store_relaxed(&mut d, -9);
        atomic_fence_seqcst();
        atomic_singlethreadfence_acquire();
        atomic_singlethreadfence_release();
        atomic_singlethreadfence_acqrel();
        atomic_singlethreadfence_seqcst();
        if atomic_load_acquire(&d) != -9 {
            should_not_reach();
        }
        if atomic_xchg_acqrel(&mut d, 5) != -9 || d != 5 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn suffix_atomic_add_and_sub() {
    // Both wrap at the width of the type.
    check_pass(
        r#"
//- minicore: copy, panic
#[rustc_intrinsic]
pub unsafe fn atomic_xadd_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xadd_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xadd_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xadd_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xadd_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xsub_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xsub_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xsub_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xsub_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xsub_seqcst<T: Copy>(dst: *mut T, src: T) -> T;

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut a = 250u8;
        if atomic_xadd_relaxed(&mut a, 10) != 250 || a != 4 {
            should_not_reach();
        }
        if atomic_xsub_acquire(&mut a, 5) != 4 || a != 255 {
            should_not_reach();
        }
        if atomic_xsub_relaxed(&mut a, 255) != 255 || a != 0 {
            should_not_reach();
        }

        let mut b = 1u32;
        if atomic_xadd_acquire(&mut b, 0xffff_ffff) != 1 || b != 0 {
            should_not_reach();
        }
        if atomic_xsub_release(&mut b, 1) != 0 || b != 0xffff_ffff {
            should_not_reach();
        }

        let mut c = 100usize;
        if atomic_xadd_release(&mut c, 23) != 100 || c != 123 {
            should_not_reach();
        }
        if atomic_xsub_acqrel(&mut c, 123) != 123 || c != 0 {
            should_not_reach();
        }

        let mut d = -5i32;
        if atomic_xadd_acqrel(&mut d, 3) != -5 || d != -2 {
            should_not_reach();
        }
        if atomic_xsub_seqcst(&mut d, -10) != -2 || d != 8 {
            should_not_reach();
        }
        if atomic_xadd_seqcst(&mut d, 2147483647) != 8 || d != -2147483641 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn suffix_atomic_bitwise() {
    check_pass(
        r#"
//- minicore: copy, panic
#[rustc_intrinsic]
pub unsafe fn atomic_and_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_and_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_and_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_and_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_and_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_nand_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_nand_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_nand_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_nand_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_nand_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_or_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_or_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_or_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_or_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_or_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xor_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xor_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xor_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xor_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_xor_seqcst<T: Copy>(dst: *mut T, src: T) -> T;

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut a = 0b1100_1010u8;
        if atomic_and_relaxed(&mut a, 0b1010_0110) != 0b1100_1010 || a != 0b1000_0010 {
            should_not_reach();
        }
        if atomic_nand_acquire(&mut a, 0x80) != 0b1000_0010 || a != 0x7f {
            should_not_reach();
        }
        if atomic_or_release(&mut a, 0x80) != 0x7f || a != 0xff {
            should_not_reach();
        }
        if atomic_xor_acqrel(&mut a, 0x0f) != 0xff || a != 0xf0 {
            should_not_reach();
        }
        if atomic_and_seqcst(&mut a, 0x3c) != 0xf0 || a != 0x30 {
            should_not_reach();
        }
        if atomic_nand_relaxed(&mut a, 0x30) != 0x30 || a != 0xcf {
            should_not_reach();
        }
        if atomic_or_acquire(&mut a, 0x30) != 0xcf || a != 0xff {
            should_not_reach();
        }
        if atomic_xor_release(&mut a, 0xaa) != 0xff || a != 0x55 {
            should_not_reach();
        }

        let mut b = 0xffff_0000u32;
        if atomic_and_acquire(&mut b, 0x0ff0_0ff0) != 0xffff_0000 || b != 0x0ff0_0000 {
            should_not_reach();
        }
        if atomic_nand_release(&mut b, 0x0f00_0000) != 0x0ff0_0000 || b != 0xf0ff_ffff {
            should_not_reach();
        }
        if atomic_or_acqrel(&mut b, 0x0f00_0000) != 0xf0ff_ffff || b != 0xffff_ffff {
            should_not_reach();
        }
        if atomic_xor_seqcst(&mut b, 0xffff_ffff) != 0xffff_ffff || b != 0 {
            should_not_reach();
        }

        let mut c = 6usize;
        if atomic_and_release(&mut c, 3) != 6 || c != 2 {
            should_not_reach();
        }
        if atomic_nand_acqrel(&mut c, 3) != 2 || c != !2usize {
            should_not_reach();
        }
        if atomic_or_seqcst(&mut c, 2) != !2usize || c != !0usize {
            should_not_reach();
        }
        if atomic_xor_relaxed(&mut c, !0usize) != !0usize || c != 0 {
            should_not_reach();
        }

        let mut d = -1i32;
        if atomic_and_acqrel(&mut d, 2147483647) != -1 || d != 2147483647 {
            should_not_reach();
        }
        if atomic_nand_seqcst(&mut d, -1) != 2147483647 || d != !2147483647 {
            should_not_reach();
        }
        if atomic_or_relaxed(&mut d, 1) != !2147483647 || d != -2147483647 {
            should_not_reach();
        }
        if atomic_xor_acquire(&mut d, -1) != -2147483647 || d != 2147483646 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn suffix_atomic_max_and_min() {
    // `max` and `min` compare as signed numbers (on `i32`), `umax` and `umin` as unsigned ones:
    // each step's operand is one the other comparison would order the other way.
    check_pass(
        r#"
//- minicore: copy, panic
#[rustc_intrinsic]
pub unsafe fn atomic_max_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_max_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_max_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_max_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_max_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_min_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_min_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_min_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_min_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_min_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umax_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umax_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umax_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umax_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umax_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umin_relaxed<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umin_acquire<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umin_release<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umin_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
#[rustc_intrinsic]
pub unsafe fn atomic_umin_seqcst<T: Copy>(dst: *mut T, src: T) -> T;

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut d = -5i32;
        if atomic_max_relaxed(&mut d, -10) != -5 || d != -5 {
            should_not_reach();
        }
        if atomic_max_acquire(&mut d, 3) != -5 || d != 3 {
            should_not_reach();
        }
        if atomic_max_release(&mut d, -1) != 3 || d != 3 {
            should_not_reach();
        }
        if atomic_max_acqrel(&mut d, 2147483647) != 3 || d != 2147483647 {
            should_not_reach();
        }
        if atomic_max_seqcst(&mut d, !2147483647) != 2147483647 || d != 2147483647 {
            should_not_reach();
        }
        if atomic_min_relaxed(&mut d, 2147483646) != 2147483647 || d != 2147483646 {
            should_not_reach();
        }
        if atomic_min_acquire(&mut d, -1) != 2147483646 || d != -1 {
            should_not_reach();
        }
        if atomic_min_release(&mut d, 0) != -1 || d != -1 {
            should_not_reach();
        }
        if atomic_min_acqrel(&mut d, !2147483647) != -1 || d != !2147483647 {
            should_not_reach();
        }
        if atomic_min_seqcst(&mut d, 5) != !2147483647 || d != !2147483647 {
            should_not_reach();
        }

        let mut a = 100u8;
        if atomic_umax_relaxed(&mut a, 200) != 100 || a != 200 {
            should_not_reach();
        }
        if atomic_umin_acquire(&mut a, 255) != 200 || a != 200 {
            should_not_reach();
        }
        if atomic_umin_relaxed(&mut a, 7) != 200 || a != 7 {
            should_not_reach();
        }
        if atomic_umax_seqcst(&mut a, 6) != 7 || a != 7 {
            should_not_reach();
        }

        let mut b = 1u32;
        if atomic_umax_acquire(&mut b, 0x8000_0000) != 1 || b != 0x8000_0000 {
            should_not_reach();
        }
        if atomic_umin_release(&mut b, 0xffff_ffff) != 0x8000_0000 || b != 0x8000_0000 {
            should_not_reach();
        }
        if atomic_umin_acqrel(&mut b, 2) != 0x8000_0000 || b != 2 {
            should_not_reach();
        }

        let mut c = 5usize;
        if atomic_umax_release(&mut c, !0usize) != 5 || c != !0usize {
            should_not_reach();
        }
        if atomic_umax_acqrel(&mut c, 1) != !0usize || c != !0usize {
            should_not_reach();
        }
        if atomic_umin_seqcst(&mut c, 9) != !0usize || c != 9 {
            should_not_reach();
        }
    }
}
"#,
    );
}

#[test]
fn suffix_atomic_compare_exchange() {
    // Every success and failure ordering of `cxchg` and `cxchgweak`, each type with a success
    // and a failure; the evaluator's weak exchange never fails spuriously.
    check_pass(
        r#"
//- minicore: copy, panic
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_relaxed_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_relaxed_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_relaxed_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_acquire_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_acquire_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_acquire_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_release_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_release_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_release_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_acqrel_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_acqrel_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_acqrel_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_seqcst_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_seqcst_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchg_seqcst_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_relaxed_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_relaxed_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_relaxed_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_acquire_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_acquire_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_acquire_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_release_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_release_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_release_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_acqrel_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_acqrel_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_acqrel_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_seqcst_relaxed<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_seqcst_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
#[rustc_intrinsic]
pub unsafe fn atomic_cxchgweak_seqcst_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);

fn should_not_reach() { panic!() }

fn main() {
    unsafe {
        let mut a = 1u8;
        let (old, ok) = atomic_cxchg_relaxed_relaxed(&mut a, 1, 2);
        if old != 1 || !ok || a != 2 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_relaxed_acquire(&mut a, 1, 3);
        if old != 2 || ok || a != 2 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_relaxed_seqcst(&mut a, 2, 255);
        if old != 2 || !ok || a != 255 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_acquire_relaxed(&mut a, 254, 0);
        if old != 255 || ok || a != 255 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_relaxed_relaxed(&mut a, 255, 0);
        if old != 255 || !ok || a != 0 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_relaxed_acquire(&mut a, 1, 9);
        if old != 0 || ok || a != 0 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_relaxed_seqcst(&mut a, 0, 9);
        if old != 0 || !ok || a != 9 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_acquire_relaxed(&mut a, 0, 1);
        if old != 9 || ok || a != 9 {
            should_not_reach();
        }

        let mut b = 0xffff_ffffu32;
        let (old, ok) = atomic_cxchg_acquire_acquire(&mut b, 0xffff_ffff, 0);
        if old != 0xffff_ffff || !ok || b != 0 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_acquire_seqcst(&mut b, 0xffff_ffff, 1);
        if old != 0 || ok || b != 0 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_release_relaxed(&mut b, 0, 0x1234_5678);
        if old != 0 || !ok || b != 0x1234_5678 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_release_acquire(&mut b, 0x1234_5679, 5);
        if old != 0x1234_5678 || ok || b != 0x1234_5678 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_acquire_acquire(&mut b, 0x1234_5678, 7);
        if old != 0x1234_5678 || !ok || b != 7 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_acquire_seqcst(&mut b, 8, 1);
        if old != 7 || ok || b != 7 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_release_relaxed(&mut b, 7, 8);
        if old != 7 || !ok || b != 8 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_release_acquire(&mut b, 7, 9);
        if old != 8 || ok || b != 8 {
            should_not_reach();
        }

        let mut c = 0usize;
        let (old, ok) = atomic_cxchg_release_seqcst(&mut c, 0, !0usize);
        if old != 0 || !ok || c != !0usize {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_acqrel_relaxed(&mut c, 0, 1);
        if old != !0usize || ok || c != !0usize {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_acqrel_acquire(&mut c, !0usize, 3);
        if old != !0usize || !ok || c != 3 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_acqrel_seqcst(&mut c, 4, 5);
        if old != 3 || ok || c != 3 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_release_seqcst(&mut c, 3, 4);
        if old != 3 || !ok || c != 4 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_acqrel_relaxed(&mut c, 3, 5);
        if old != 4 || ok || c != 4 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_acqrel_acquire(&mut c, 4, 6);
        if old != 4 || !ok || c != 6 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_acqrel_seqcst(&mut c, 4, 7);
        if old != 6 || ok || c != 6 {
            should_not_reach();
        }

        let mut d = -1i32;
        let (old, ok) = atomic_cxchg_seqcst_relaxed(&mut d, -1, 7);
        if old != -1 || !ok || d != 7 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_seqcst_acquire(&mut d, -1, 8);
        if old != 7 || ok || d != 7 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchg_seqcst_seqcst(&mut d, 7, !2147483647);
        if old != 7 || !ok || d != !2147483647 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_seqcst_relaxed(&mut d, 2147483647, 0);
        if old != !2147483647 || ok || d != !2147483647 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_seqcst_acquire(&mut d, !2147483647, -2);
        if old != !2147483647 || !ok || d != -2 {
            should_not_reach();
        }
        let (old, ok) = atomic_cxchgweak_seqcst_seqcst(&mut d, 2, 3);
        if old != -2 || ok || d != -2 {
            should_not_reach();
        }
    }
}
"#,
    );
}

/// The proc-macro bridge's `rpc.rs` shape: impls whose headers leave lifetimes out
/// (`for &[u8]`, `DecodeMut<'a, '_, S>`), each a lifetime parameter of the impl, and
/// `Option<T>`'s decode needing `T: for<'s> DecodeMut<'a, 's, S>` of one. Indexing is by
/// `usize` only: minicore's `Index` impls, which a range index calls, are `loop {}`.
#[test]
fn impls_with_elided_header_lifetimes_evaluate() {
    check_pass(
        r#"
//- minicore: sized, copy, slice, index, option, panic, fmt
struct Reader<'a> { bytes: &'a [u8], at: usize }

struct Writer { buf: [u8; 8], len: usize }

impl Writer {
    fn push(&mut self, b: u8) {
        self.buf[self.len] = b;
        self.len += 1;
    }
}

trait Encode<S>: Sized {
    fn encode(self, w: &mut Writer, s: &mut S);
}

trait DecodeMut<'a, 's, S>: Sized {
    fn decode(r: &mut Reader<'a>, s: &'s mut S) -> Self;
}

impl<S> Encode<S> for u8 {
    fn encode(self, w: &mut Writer, _: &mut S) {
        w.push(self)
    }
}

impl<S> DecodeMut<'_, '_, S> for u8 {
    fn decode(r: &mut Reader<'_>, _: &mut S) -> Self {
        let x = r.bytes[r.at];
        r.at += 1;
        x
    }
}

impl<S> Encode<S> for &[u8] {
    fn encode(self, w: &mut Writer, s: &mut S) {
        (self.len() as u8).encode(w, s);
        let mut i = 0;
        while i < self.len() {
            w.push(self[i]);
            i += 1;
        }
    }
}

impl<'a, S> DecodeMut<'a, '_, S> for &'a u8 {
    fn decode(r: &mut Reader<'a>, _: &mut S) -> Self {
        let x = &r.bytes[r.at];
        r.at += 1;
        x
    }
}

impl<S, T: Encode<S>> Encode<S> for Option<T> {
    fn encode(self, w: &mut Writer, s: &mut S) {
        match self {
            None => 0u8.encode(w, s),
            Some(x) => {
                1u8.encode(w, s);
                x.encode(w, s);
            }
        }
    }
}

impl<'a, S, T: for<'s> DecodeMut<'a, 's, S>> DecodeMut<'a, '_, S> for Option<T> {
    fn decode(r: &mut Reader<'a>, s: &mut S) -> Self {
        match u8::decode(r, s) {
            0 => None,
            _ => Some(T::decode(r, s)),
        }
    }
}

fn main() {
    let mut w = Writer { buf: [0; 8], len: 0 };
    let data: &[u8] = &[7, 8, 9];
    Some(data).encode(&mut w, &mut ());
    if w.len != 5 {
        panic!("encoded wrong");
    }
    let mut r = Reader { bytes: &w.buf, at: 0 };
    // The tag, then the slice's length as a `&u8`, then its first byte.
    let back = <Option<&u8>>::decode(&mut r, &mut ());
    match back {
        Some(&3) => {}
        _ => panic!("decoded wrong"),
    }
    if u8::decode(&mut r, &mut ()) != 7 || r.at != 3 {
        panic!("read wrong");
    }
}
"#,
    );
}

/// The proc-macro bridge's `Decode` impls, each leaving its header's lifetimes out, and
/// `run_client`'s closure calling `<(ExpnGlobals<Span>, A)>::decode`, whose callee the 00:49
/// self-run's serde derives found no definition of (queue 01481). `S` counts the decodes, so
/// each call is seen in order.
#[test]
fn the_bridges_decode_impls_evaluate_for_run_client() {
    check_pass(
        r#"
//- minicore: sized, fn, copy, slice, panic, fmt
pub trait Decode<'a, 's, S>: Sized {
    fn decode(r: &mut &'a [u8], s: &'s mut S) -> Self;
}
impl<S> Decode<'_, '_, S> for () {
    fn decode(_: &mut &[u8], _: &mut S) -> Self {}
}
impl<'a, S, A: for<'s> Decode<'a, 's, S>, B: for<'s> Decode<'a, 's, S>> Decode<'a, '_, S> for (A, B) {
    fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
        (Decode::decode(r, s), Decode::decode(r, s))
    }
}
macro_rules! rpc_struct {
    (struct $name:ident <$T:ident> { $($field:ident),* }) => {
        pub struct $name<$T> { $(pub $field: $T),* }
        impl<'a, S, $T: for<'s> Decode<'a, 's, S>> Decode<'a, '_, S> for $name<$T> {
            fn decode(r: &mut &'a [u8], s: &mut S) -> Self {
                $name { $($field: Decode::decode(r, s)),* }
            }
        }
    };
}
pub struct Span(u32);
impl Decode<'_, '_, u32> for Span {
    fn decode(_: &mut &[u8], s: &mut u32) -> Self {
        *s += 1;
        Span(*s)
    }
}
rpc_struct!(struct ExpnGlobals<Span> { def_site, call_site });
pub fn run_client<A: for<'a, 's> Decode<'a, 's, u32>>(buf: &[u8]) -> A {
    let f = || {
        let mut bytes = buf;
        let reader = &mut bytes;
        let mut count = 0u32;
        let (globals, input) = <(ExpnGlobals<Span>, A)>::decode(reader, &mut count);
        if globals.def_site.0 != 1 || globals.call_site.0 != 2 {
            panic!("globals decoded wrong");
        }
        input
    };
    f()
}
fn main() {
    let input: Span = run_client(&[1, 2, 3]);
    if input.0 != 3 {
        panic!("input decoded wrong");
    }
    let () = run_client::<()>(&[]);
}
"#,
    );
}

#[test]
fn a_box_with_an_allocator_unsizes_a_closure_to_dyn_fn() {
    check_pass(
        r#"
//- minicore: fn, unsize, coerce_unsized, dispatch_from_dyn
use core::marker::Unsize;
use core::ops::CoerceUnsized;

pub struct Global;

pub struct Box<T: ?Sized, A = Global>(*const T, A);

impl<T: ?Sized + Unsize<U>, U: ?Sized, A> CoerceUnsized<Box<U, A>> for Box<T, A> {}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    let x = 5;
    let closure = move || x + 2;
    let boxed: Box<_, Global> = Box(&closure as *const _, Global);
    let unsized_: Box<dyn Fn() -> i32, Global> = boxed;
    let f: &dyn Fn() -> i32 = unsafe { &*unsized_.0 };
    if f() != 7 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn a_box_of_a_unique_with_phantom_data_unsizes_a_fn_to_dyn_fn() {
    check_pass(
        r#"
//- minicore: fn, unsize, coerce_unsized, dispatch_from_dyn, phantom_data
use core::marker::{PhantomData, Unsize};
use core::ops::CoerceUnsized;

pub struct Global;

pub struct Unique<T: ?Sized> {
    pointer: *const T,
    _marker: PhantomData<T>,
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Unique<U>> for Unique<T> {}

pub struct Box<T: ?Sized, A = Global>(Unique<T>, A);

impl<T: ?Sized + Unsize<U>, U: ?Sized, A> CoerceUnsized<Box<U, A>> for Box<T, A> {}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn default_hook(x: &i32) -> i32 {
    *x + 2
}

fn main() {
    let hook = default_hook;
    let boxed = Box(Unique { pointer: &hook as *const _, _marker: PhantomData }, Global);
    let unsized_: Box<dyn Fn(&i32) -> i32, Global> = boxed;
    let f: &dyn Fn(&i32) -> i32 = unsafe { &*unsized_.0.pointer };
    if f(&5) != 7 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn a_body_that_inherits_its_overflow_checks_takes_those_of_the_crate_the_run_is_compiled_as() {
    let fx = fixture::db(
        r#"
//- /std.rs crate:std
#[rustc_inherit_overflow_checks]
pub fn inc(x: u8) -> u8 {
    x + 1
}
//- /main.rs crate:main deps:std
fn main() {}
"#,
    );
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_ids[0].file_id(&db));
        let inc = module_id
            .def_map(&db)
            .module(&db, module_id)
            .scope(&db)
            .declarations()
            .find_map(|x| match x {
                hir_def::ModuleDefId::FunctionId(x)
                    if FunctionSignature::of(&db, x).name.display(&db, Edition::CURRENT).to_string() == "inc" =>
                {
                    Some(x)
                }
                _ => None,
            })
            .expect("no inc function found");
        let main_crate = db.module_for_file(file_ids[1].file_id(&db)).krate(&db);
        let env = crate::ParamEnvAndCrate { param_env: db.trait_environment(inc.into()), krate: main_crate };
        let run = |inherited: Option<bool>| {
            let mut eval = crate::mir::Evaluator::new(&db, inc.into(), Some(env)).unwrap();
            if let Some(checks) = inherited {
                eval.set_inherited_overflow_checks(checks);
            }
            eval.call(inc, GenericArgs::empty(), &[&[255]])
        };
        let panics = |ran: Result<Vec<u8>, MirEvalError<'_>>| match ran {
            Err(e) => matches!(innermost(&e), MirEvalError::Panic(_)),
            Ok(bytes) => panic!("255 + 1 ran to {bytes:?}"),
        };
        assert_eq!(run(Some(false)).unwrap(), [0], "a crate built without overflow checks wraps");
        assert!(panics(run(Some(true))), "a crate built with overflow checks panics");
        assert!(panics(run(None)), "unset, as in a constant, it checks");
    })
}

#[test]
fn a_box_of_dyn_fn_unsizes_to_dyn_fn_keeping_its_vtable() {
    check_pass(
        r#"
//- minicore: fn, unsize, coerce_unsized, dispatch_from_dyn, phantom_data, send, sync
use core::marker::{PhantomData, Send, Sync, Unsize};
use core::ops::CoerceUnsized;

pub struct Global;

pub struct NonNull<T: ?Sized> {
    pointer: *const T,
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<NonNull<U>> for NonNull<T> {}

pub struct Unique<T: ?Sized> {
    pointer: NonNull<T>,
    _marker: PhantomData<T>,
}

impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Unique<U>> for Unique<T> {}

unsafe impl<T: ?Sized + Send> Send for Unique<T> {}
unsafe impl<T: ?Sized + Sync> Sync for Unique<T> {}

pub struct Box<T: ?Sized, A = Global>(Unique<T>, A);

impl<T: ?Sized + Unsize<U>, U: ?Sized, A> CoerceUnsized<Box<U, A>> for Box<T, A> {}

fn new_box<T>(x: &T) -> Box<T> {
    Box(Unique { pointer: NonNull { pointer: x as *const T }, _marker: PhantomData }, Global)
}

fn get<T: ?Sized>(b: &Box<T>) -> &T {
    unsafe { &*b.0.pointer.pointer }
}

pub struct Info(i32);

enum Hook {
    Custom(Box<dyn Fn(&Info) -> i32 + Sync + Send>),
}

impl Hook {
    fn into_box(self) -> Box<dyn Fn(&Info) -> i32 + Sync + Send> {
        match self {
            Hook::Custom(hook) => hook,
        }
    }
}

// As std's: the value `into_box` returns is cast to the very type it has.
fn take_hook(hook: Hook) -> Box<dyn Fn(&Info) -> i32 + 'static + Sync + Send> {
    hook.into_box()
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn default_hook(info: &Info) -> i32 {
    info.0 + 2
}

fn main() {
    let f = default_hook;
    let boxed = new_box(&f);
    let hook: Box<dyn Fn(&Info) -> i32 + Sync + Send> = boxed;
    let prev = take_hook(Hook::Custom(hook));
    if get(&prev)(&Info(5)) != 7 {
        should_not_reach();
    }
    // As `maybe_install_panic_hook`'s: a closure holding the hook taken and a flag.
    let force_show = true;
    let wrapper = move |info: &Info| if force_show { get(&prev)(info) } else { 0 };
    let boxed = new_box(&wrapper);
    let installed: Box<dyn Fn(&Info) -> i32 + Sync + Send> = boxed;
    let fewer: Box<dyn Fn(&Info) -> i32> = take_hook(Hook::Custom(installed));
    if get(&fewer)(&Info(1)) != 3 {
        should_not_reach();
    }
}
"#,
    );
}

#[test]
fn the_c_heap_std_system_allocator_uses_allocates_grows_and_frees() {
    check_pass(
        r#"
//- minicore: sized, copy, slice, index, coerce_unsized
extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn calloc(count: usize, size: usize) -> *mut u8;
    fn realloc(ptr: *mut u8, size: usize) -> *mut u8;
    fn posix_memalign(out: *mut *mut u8, align: usize, size: usize) -> i32;
    fn free(ptr: *mut u8);
}

fn should_not_reach() {
    _ // FIXME: replace this function with panic when that works
}

fn main() {
    unsafe {
        let a = malloc(4);
        let small = a as *mut [u8; 4];
        (*small)[0] = 7;
        (*small)[3] = 9;
        let b = realloc(a, 64);
        let grown = b as *mut [u8; 64];
        if (*grown)[0] != 7 || (*grown)[3] != 9 {
            should_not_reach();
        }
        let c = calloc(8, 2) as *mut [u8; 16];
        if (*c)[0] != 0 || (*c)[15] != 0 {
            should_not_reach();
        }
        let mut aligned: *mut u8 = 0 as *mut u8;
        if posix_memalign(&mut aligned, 64, 10) != 0 || (aligned as usize) % 64 != 0 {
            should_not_reach();
        }
        let mut refused: *mut u8 = 0 as *mut u8;
        if posix_memalign(&mut refused, 3, 8) != 22 {
            should_not_reach();
        }
        if calloc(!0usize, 2) as usize != 0 {
            should_not_reach();
        }
        free(b);
        free(c as *mut u8);
        free(aligned);
        free(0 as *mut u8);
    }
}
"#,
    );
}
