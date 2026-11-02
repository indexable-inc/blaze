//! The checker end to end: programs run through `run_test`. Each kind of undefined behavior has
//! a program ported from Miri's `tests/fail/` (named after it) with Miri's verdict; the
//! programs Miri passes pass; and a run that finds nothing is the evaluator's own run.

mod db;

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use base_db::relevant_crates;
use hir_def::{FunctionId, HasModule, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::EditionedFileId;
use hir_ty::{InferBodyId, ParamEnvAndCrate, db::HirDatabase, mir::MirSpan, next_solver::GenericArgs};
use hir_ty_mir::mir::{Plain, Site, print_mir_body, run_function};
use hir_ty_ub::{
    Config, Found, Outcome, Ub,
    check::{Checker, Sites},
    memory::MemError,
    run_test,
};
use hir_expand::fixture;

use crate::db::TestDB;

/// What every program may use: the global allocator's functions, a `Box`, threads, and the
/// atomics, as the evaluator shims them.
macro_rules! prelude {
    () => {
        r#"
extern "Rust" {
    #[rustc_allocator]
    fn __rust_alloc(size: usize, align: usize) -> *mut u8;
    #[rustc_deallocator]
    fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize);
}

extern "C" {
    fn pthread_create(
        thread: *mut usize,
        attr: *const u8,
        start: extern "C" fn(*mut u8) -> *mut u8,
        arg: *mut u8,
    ) -> i32;
    fn pthread_join(thread: usize, ret: *mut *mut u8) -> i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

#[rustc_intrinsic]
pub fn box_new<T>(_x: T) -> Box<T>;

#[lang = "owned_box"]
pub struct Box<T: ?Sized> {
    inner: *mut T,
}

#[rustc_intrinsic]
unsafe fn arith_offset<T>(ptr: *const T, count: isize) -> *const T;
#[rustc_intrinsic]
unsafe fn offset<T>(ptr: *const T, count: isize) -> *const T;
#[rustc_intrinsic]
unsafe fn atomic_load_acquire<T>(src: *const T) -> T;
#[rustc_intrinsic]
unsafe fn atomic_load_relaxed<T>(src: *const T) -> T;
#[rustc_intrinsic]
unsafe fn atomic_store_release<T>(dst: *mut T, val: T);
#[rustc_intrinsic]
unsafe fn atomic_store_relaxed<T>(dst: *mut T, val: T);
#[rustc_intrinsic]
unsafe fn atomic_xadd_seqcst<T>(dst: *mut T, src: T) -> T;

fn spawn(start: extern "C" fn(*mut u8) -> *mut u8, arg: *mut u8) -> usize {
    let mut thread = 0usize;
    unsafe { pthread_create(&mut thread, 0 as *const u8, start, arg) };
    thread
}

fn join(thread: usize) {
    unsafe { pthread_join(thread, 0 as *mut *mut u8) };
}
"#
    };
}

/// The target of every program: the evaluator's thread shims follow the target's
/// `pthread` ABI, and the prelude's `pthread_t` is Linux's.
macro_rules! target {
    () => {
        "//- /main.rs cfg:target_os=linux,target_arch=x86_64\n"
    };
}

macro_rules! program {
    ($minicore:literal, $body:literal) => {
        concat!("//- minicore: ", $minicore, "\n", target!(), prelude!(), $body)
    };
}

mod budget;

fn main_of(db: &TestDB, file: EditionedFileId) -> FunctionId {
    let file_id = file.file_id(db);
    for &krate in relevant_crates(db, file_id).iter() {
        let def_map = crate_def_map(db, krate);
        for (module_id, data) in def_map.modules(db) {
            if data.origin.file_id().map(|f| f.file_id(db)) != Some(file_id) {
                continue;
            }
            let main = def_map.module(db, module_id).scope(db).declarations().find_map(|def| match def {
                ModuleDefId::FunctionId(f) if FunctionSignature::of(db, f).name.as_str() == "main" => Some(f),
                _ => None,
            });
            if let Some(main) = main {
                return main;
            }
        }
    }
    panic!("the program has no `fn main`")
}

fn run_with(fixture: &str, config: &Config) -> Outcome {
    let fx = fixture::db(fixture);
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || run_test(&db, main_of(&db, file), config))
}

fn run(fixture: &str) -> Outcome {
    run_with(fixture, &Config::default())
}

/// Runs the program, which must stop at undefined behavior whose message contains `expect`.
#[track_caller]
fn ub(fixture: &str, expect: &str) -> (Found, String) {
    match run(fixture) {
        Outcome::Ub { found, report, .. } => {
            let message = found.ub.to_string();
            assert!(message.contains(expect), "expected `{expect}`, found:\n{report}");
            assert!(report.starts_with(&format!("error: {message}")), "{report}");
            (found, report)
        }
        other => panic!("expected undefined behavior `{expect}`, got {other:?}"),
    }
}

#[track_caller]
fn pass(fixture: &str) {
    match run(fixture) {
        Outcome::Passed { .. } => {}
        other => panic!("expected a pass, got {other:?}\n{}", main_mir(fixture)),
    }
}

/// `main`'s MIR as the evaluator runs it (`run_function`'s body), for a failure's message.
fn main_mir(fixture: &str) -> String {
    let fx = fixture::db(fixture);
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let main = main_of(&db, file);
        let env = ParamEnvAndCrate {
            param_env: db.trait_environment(main.into()),
            krate: main.module(&db).krate(&db),
        };
        match db.monomorphized_runtime_mir_body(main.into(), GenericArgs::empty().store(), env.store()) {
            Ok(body) => print_mir_body(&db, &body, "main"),
            Err(e) => format!("no MIR for `main`: {e:?}"),
        }
    })
}

// --- Memory: tests/fail/dangling_pointers/ ---

// dangling_primitive.rs
#[test]
fn dangling_pointers_use_after_scope() {
    let (found, report) = ub(
        program!(
            "sized",
            r#"
fn main() {
    let p = {
        let x = 42i32;
        &x as *const i32
    };
    let _y = unsafe { *p };
}
"#
        ),
        "has been freed, so this pointer is dangling",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::UseAfterFree { .. })));
    assert!(report.contains("was allocated here:"));
    assert!(report.contains("was deallocated here:"));
}

// modifying_constants.rs: what a constant points to is interned read-only, so a write through a
// pointer cast from it is refused before any borrow is checked.
#[test]
fn a_write_to_what_a_constant_points_to_is_read_only() {
    let (found, _) = ub(
        program!(
            "sized",
            r#"
const C: &i32 = &1;
fn main() {
    let p = C as *const i32 as *mut i32;
    unsafe { *p = 42 };
}
"#
        ),
        "which is read-only",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::WriteToReadOnly(_))));
}

// null_pointer_deref.rs
#[test]
fn dangling_pointers_null_deref() {
    ub(
        program!(
            "sized",
            r#"
fn main() {
    let p = 0 as *const i32;
    let _x = unsafe { *p };
}
"#
        ),
        "memory access failed: attempting to access 4 bytes, but got null pointer",
    );
}

// dangling_pointer_deref_int.rs (an address with no exposed allocation)
#[test]
fn dangling_pointers_int_deref() {
    ub(
        program!(
            "sized",
            r#"
fn main() {
    let p = 64 as *const i32;
    let _x = unsafe { *p };
}
"#
        ),
        "but got 0x40[noalloc] which is a dangling pointer (it has no provenance)",
    );
}

// out_of_bounds_read.rs
#[test]
fn dangling_pointers_out_of_bounds_read() {
    ub(
        program!(
            "sized",
            r#"
fn main() {
    let v = [0u8; 4];
    let p = unsafe { arith_offset(&v as *const [u8; 4] as *const u8, 5) };
    let _x = unsafe { *p };
}
"#
        ),
        "which is at or beyond the end of the allocation of size 4 bytes",
    );
}

// tests/fail/intrinsics/ptr_offset_out_of_bounds.rs
#[test]
fn offset_out_of_bounds() {
    ub(
        program!(
            "sized",
            r#"
fn main() {
    let v = [0u8; 4];
    let _p = unsafe { offset(&v as *const [u8; 4] as *const u8, 5) };
}
"#
        ),
        "in-bounds pointer arithmetic failed: attempting to offset pointer by 5 bytes",
    );
}

// --- tests/fail/uninit/ ---

// uninit_byte_read.rs: a union field never written, read as an integer.
#[test]
fn uninit_read_as_integer() {
    ub(
        program!(
            "sized, copy",
            r#"
#[derive(Clone, Copy)]
union U {
    a: u8,
    b: (),
}

fn main() {
    let u = U { b: () };
    let x = unsafe { u.a };
    let _y = x;
}
"#
        ),
        "encountered uninitialized memory, but expected an integer",
    );
}

// --- tests/fail/validity/ ---

// invalid_bool.rs
#[test]
fn validity_invalid_bool() {
    ub(
        program!(
            "transmute",
            r#"
fn main() {
    let _b: bool = unsafe { core::mem::transmute::<u8, bool>(2) };
}
"#
        ),
        "constructing invalid value of type bool: encountered 0x02, but expected a boolean",
    );
}

// invalid_bool_field.rs: the path to a field inside an enum variant. The enum is `repr(u8)`, a
// tag of its own: an `Option` of a `bool` takes the `bool` as its niche, where a 3 is a bad tag.
#[test]
fn validity_invalid_bool_in_a_variant() {
    ub(
        program!(
            "transmute",
            r#"
#[repr(u8)]
enum E {
    A(u8, bool),
    B,
}

fn main() {
    let _e: E = unsafe { core::mem::transmute::<[u8; 3], E>([0, 1, 3]) };
}
"#
        ),
        "at .<enum-variant(A)>.1, encountered 0x03, but expected a boolean",
    );
}

// invalid_char.rs
#[test]
fn validity_invalid_char() {
    ub(
        program!(
            "transmute",
            r#"
fn main() {
    let _c: char = unsafe { core::mem::transmute::<u32, char>(0xD800) };
}
"#
        ),
        "encountered 0x0000d800, but expected a valid unicode scalar value",
    );
}

// invalid_enum_tag.rs
#[test]
fn validity_invalid_enum_tag() {
    ub(
        program!(
            "transmute",
            r#"
#[repr(u8)]
enum E {
    A = 0,
    B = 1,
}

fn main() {
    let _e: E = unsafe { core::mem::transmute::<u8, E>(7) };
}
"#
        ),
        "encountered 0x07, but expected a valid enum tag",
    );
}

// null_reference.rs (tests/fail/validity/ref_to_uninhabited, dangling_ref1 are its siblings)
#[test]
fn validity_null_reference() {
    ub(
        program!(
            "transmute",
            r#"
fn main() {
    let _r: &i32 = unsafe { core::mem::transmute::<usize, &i32>(0) };
}
"#
        ),
        "encountered a null reference",
    );
}

// unaligned_ref.rs
#[test]
fn validity_unaligned_reference() {
    ub(
        program!(
            "transmute",
            r#"
fn main() {
    let x = [0u16; 2];
    let p = unsafe { arith_offset(&x as *const [u16; 2] as *const u8, 1) };
    let _r: &u16 = unsafe { core::mem::transmute::<*const u8, &u16>(p) };
}
"#
        ),
        "encountered an unaligned reference (required 2 byte alignment but found 1)",
    );
}

// tests/fail/validity/uninhabited_enum.rs
#[test]
fn validity_zero_variant_enum() {
    ub(
        program!(
            "transmute",
            r#"
enum Void {}

fn main() {
    let _v: Void = unsafe { core::mem::transmute::<(), Void>(()) };
}
"#
        ),
        "encountered a value of zero-variant enum `ra_test_fixture::Void`",
    );
}

// --- tests/fail/alloc/ ---

// deallocate_twice.rs
#[test]
fn alloc_deallocate_twice() {
    ub(
        program!(
            "sized",
            r#"
fn main() {
    unsafe {
        let p = __rust_alloc(4, 4);
        __rust_dealloc(p, 4, 4);
        __rust_dealloc(p, 4, 4);
    }
}
"#
        ),
        "has been freed, so this pointer is dangling",
    );
}

// --- tests/fail/tree_borrows/ ---

// write_to_shr.rs: a write through a pointer derived from a shared reference.
#[test]
fn tree_borrows_write_through_shared() {
    let (found, report) = ub(
        program!(
            "sized",
            r#"
fn main() {
    let mut x = 0u8;
    let r = &mut x;
    let s = &*r;
    let p = s as *const u8 as *mut u8;
    unsafe { *p = 1 };
}
"#
        ),
        "write access through",
    );
    assert!(matches!(found.ub, Ub::Aliasing(_)));
    assert!(found.ub.to_string().contains("is forbidden"));
    assert!(report.contains("was created here"));
}

// protector-write-lazy.rs: a foreign write to a protected, written `&mut`.
#[test]
fn tree_borrows_protector() {
    ub(
        program!(
            "sized",
            r#"
fn f(x: &mut u8, y: *mut u8) {
    *x = 1;
    unsafe { *y = 2 };
}

fn main() {
    let mut v = 0u8;
    let p = &mut v as *mut u8;
    f(unsafe { &mut *p }, p);
}
"#
        ),
        "is forbidden",
    );
}

// --- tests/fail/both_borrows/ ---

// aliasing_mut1.rs
#[test]
fn both_borrows_aliasing_mut() {
    ub(
        program!(
            "sized",
            r#"
fn safe(x: &mut i32, y: &mut i32) {
    *x = 1;
    *y = 2;
}

fn main() {
    let mut x = 0;
    let xraw: *mut i32 = &mut x;
    unsafe { safe(&mut *xraw, &mut *xraw) };
}
"#
        ),
        "is forbidden",
    );
}

// --- tests/fail/unaligned_pointers/ ---

// unaligned_ptr1.rs
#[test]
fn unaligned_pointer_read() {
    ub(
        program!(
            "sized",
            r#"
fn main() {
    let x = [0u16; 2];
    let p = unsafe { arith_offset(&x as *const [u16; 2] as *const u8, 1) } as *const u16;
    let _v = unsafe { *p };
}
"#
        ),
        "accessing memory based on pointer with alignment 1, but alignment 2 is required",
    );
}

// --- tests/fail/provenance/ ---

// ptr_int_unexposed.rs: an address taken without exposing, cast back.
#[test]
fn provenance_unexposed() {
    ub(
        program!(
            "transmute",
            r#"
fn main() {
    let x: i32 = 3;
    let addr: usize = unsafe { core::mem::transmute::<*const i32, usize>(&x as *const i32) };
    let p = addr as *const i32;
    let _y = unsafe { *p };
}
"#
        ),
        "which is a dangling pointer (it has no provenance)",
    );
}

// --- tests/fail/memleak.rs ---

#[test]
fn memleak() {
    let (found, report) = ub(
        program!(
            "sized",
            r#"
fn main() {
    let _p = unsafe { __rust_alloc(4, 4) };
}
"#
        ),
        "(Rust heap, size: 4, align: 4)",
    );
    assert!(matches!(found.ub, Ub::Leak(_)));
    assert!(report.contains("was allocated here:"));
}

#[test]
fn memleak_off() {
    let fixture = program!(
        "sized",
        r#"
fn main() {
    let _p = unsafe { __rust_alloc(4, 4) };
}
"#
    );
    let config = Config { leaks: false, ..Config::default() };
    assert_eq!(run_with(fixture, &config), Outcome::Passed { schedules: 1 });
}

// --- tests/fail/data_race/ ---

// read_write_race.rs
#[test]
fn data_race_read_write() {
    let (found, _) = ub(
        program!(
            "sized",
            r#"
static mut X: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    unsafe { X = 1 };
    0 as *mut u8
}

fn main() {
    let t = spawn(child, 0 as *mut u8);
    let _r = unsafe { X };
    join(t);
}
"#
        ),
        "Data race detected between (1)",
    );
    assert!(matches!(found.ub, Ub::Race(_)));
}

// tests/fail/concurrency/main_terminate_without_join (the leak check's thread rule).
#[test]
fn threads_left() {
    ub(
        program!(
            "sized",
            r#"
extern "C" fn child(_: *mut u8) -> *mut u8 {
    loop {}
}

fn main() {
    let _t = spawn(child, 0 as *mut u8);
}
"#
        ),
        "the main thread terminated without waiting for all remaining threads",
    );
}

// --- Programs Miri passes ---

#[test]
fn pass_box() {
    pass(program!(
        "sized, drop",
        r#"
#[rustc_intrinsic]
unsafe fn size_of_val<T: ?Sized>(ptr: *const T) -> usize;
#[rustc_intrinsic]
unsafe fn align_of_val<T: ?Sized>(ptr: *const T) -> usize;

// A box's drop glue drops what it holds, then runs this, which frees it, as std's does.
impl<T: ?Sized> Drop for Box<T> {
    fn drop(&mut self) {
        unsafe {
            let (size, align) = (size_of_val(self.inner), align_of_val(self.inner));
            __rust_dealloc(self.inner as *mut u8, size, align);
        }
    }
}

fn main() {
    let mut b = box_new(41i32);
    *b += 1;
    let r: &i32 = &*b;
    if *r != 42 {
        loop {}
    }
}
"#
    ));
}

#[test]
fn pass_vec() {
    pass(program!(
        "sized, drop",
        r#"
struct Vec {
    ptr: *mut u32,
    len: usize,
    cap: usize,
}

fn at(p: *mut u32, i: usize) -> *mut u32 {
    unsafe { offset(p as *const u32, i as isize) as *mut u32 }
}

impl Vec {
    fn new() -> Vec {
        Vec { ptr: 4 as *mut u32, len: 0, cap: 0 }
    }

    fn push(&mut self, v: u32) {
        if self.len == self.cap {
            let cap = if self.cap == 0 { 4 } else { self.cap * 2 };
            let new = unsafe { __rust_alloc(cap * 4, 4) } as *mut u32;
            let mut i = 0;
            while i < self.len {
                unsafe { *at(new, i) = *at(self.ptr, i) };
                i += 1;
            }
            if self.cap != 0 {
                unsafe { __rust_dealloc(self.ptr as *mut u8, self.cap * 4, 4) };
            }
            self.ptr = new;
            self.cap = cap;
        }
        unsafe { *at(self.ptr, self.len) = v };
        self.len += 1;
    }

    fn get(&self, i: usize) -> u32 {
        unsafe { *at(self.ptr, i) }
    }
}

impl Drop for Vec {
    fn drop(&mut self) {
        if self.cap != 0 {
            unsafe { __rust_dealloc(self.ptr as *mut u8, self.cap * 4, 4) };
        }
    }
}

fn main() {
    let mut v = Vec::new();
    let mut i = 0;
    while i < 10 {
        v.push(i);
        i += 1;
    }
    if v.get(9) != 9 {
        loop {}
    }
}
"#
    ));
}

#[test]
fn pass_rc() {
    pass(program!(
        "sized, drop, cell, copy",
        r#"
use core::cell::Cell;

struct RcBox<T> {
    count: Cell<usize>,
    value: T,
}

struct Rc<T> {
    ptr: *mut RcBox<T>,
}

impl<T> Rc<T> {
    fn new(value: T) -> Rc<T> {
        let ptr = unsafe { __rust_alloc(16, 8) } as *mut RcBox<T>;
        unsafe { core::ptr::write(ptr, RcBox { count: Cell::new(1), value }) };
        Rc { ptr }
    }

    fn get(&self) -> &T {
        unsafe { &(*self.ptr).value }
    }

    fn clone(&self) -> Rc<T> {
        let inner = unsafe { &*self.ptr };
        inner.count.set(inner.count.get() + 1);
        Rc { ptr: self.ptr }
    }
}

impl<T> Drop for Rc<T> {
    fn drop(&mut self) {
        let inner = unsafe { &*self.ptr };
        let n = inner.count.get() - 1;
        inner.count.set(n);
        if n == 0 {
            unsafe { __rust_dealloc(self.ptr as *mut u8, 16, 8) };
        }
    }
}

fn main() {
    let a = Rc::new(7u64);
    let b = a.clone();
    if *a.get() + *b.get() != 14 {
        loop {}
    }
}
"#
    ));
}

#[test]
fn pass_cell_through_shared_references() {
    pass(program!(
        "sized, cell, copy",
        r#"
use core::cell::Cell;

fn bump(a: &Cell<i32>, b: &Cell<i32>) {
    a.set(b.get() + 1);
    b.set(a.get() + 1);
}

fn main() {
    let c = Cell::new(0);
    bump(&c, &c);
    if c.get() != 2 {
        loop {}
    }
}
"#
    ));
}

// The core of `RefCell`: a flag in a `Cell` and the value in an `UnsafeCell`, borrowed `&mut`
// through the shared reference.
#[test]
fn pass_refcell() {
    pass(program!(
        "sized, cell, copy",
        r#"
use core::cell::{Cell, UnsafeCell};

struct RefCell<T> {
    borrowed: Cell<bool>,
    value: UnsafeCell<T>,
}

impl<T> RefCell<T> {
    fn borrow_mut(&self) -> &mut T {
        if self.borrowed.get() {
            loop {}
        }
        self.borrowed.set(true);
        unsafe { &mut *self.value.get() }
    }

    fn release(&self) {
        self.borrowed.set(false);
    }
}

fn main() {
    let c = RefCell { borrowed: Cell::new(false), value: UnsafeCell::new(1) };
    let r = &c;
    *r.borrow_mut() += 1;
    r.release();
    *r.borrow_mut() += 1;
    r.release();
}
"#
    ));
}

#[test]
fn pass_raw_pointer_round_trips() {
    pass(program!(
        "sized",
        r#"
fn main() {
    let mut x = 1u32;
    let p = &mut x as *mut u32;
    unsafe { *p += 1 };
    let r = unsafe { &mut *p };
    *r += 1;
    let q = r as *mut u32;
    let addr = q as usize;
    let back = addr as *mut u32;
    unsafe { *back += 1 };
    if x != 4 {
        loop {}
    }
}
"#
    ));
}

// `thread::scope`'s guarantee: a spawned thread borrows the parent's data until the join.
#[test]
fn pass_scoped_thread() {
    pass(program!(
        "sized",
        r#"
extern "C" fn child(arg: *mut u8) -> *mut u8 {
    unsafe { *(arg as *mut u32) += 1 };
    0 as *mut u8
}

fn main() {
    let mut data = 41u32;
    let t = spawn(child, &mut data as *mut u32 as *mut u8);
    join(t);
    if data != 42 {
        loop {}
    }
}
"#
    ));
}

// Message passing: the release store of the flag orders the data's write before the read.
#[test]
fn pass_atomics_acquire_release() {
    let fixture = program!(
        "sized",
        r#"
static mut DATA: u32 = 0;
static mut READY: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    unsafe {
        DATA = 42;
        atomic_store_release(&raw mut READY, 1);
    }
    0 as *mut u8
}

fn main() {
    let t = spawn(child, 0 as *mut u8);
    unsafe {
        while atomic_load_acquire(&raw const READY) == 0 {}
        if DATA != 42 {
            loop {}
        }
    }
    join(t);
}
"#
    );
    let config = Config { schedules: 4, ..Config::default() };
    assert_eq!(run_with(fixture, &config), Outcome::Passed { schedules: 4 });
}

// The same program with relaxed orderings races on the data.
#[test]
fn data_race_relaxed_message_passing() {
    ub(
        program!(
            "sized",
            r#"
static mut DATA: u32 = 0;
static mut READY: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    unsafe {
        DATA = 42;
        atomic_store_relaxed(&raw mut READY, 1);
    }
    0 as *mut u8
}

fn main() {
    let t = spawn(child, 0 as *mut u8);
    unsafe {
        while atomic_load_relaxed(&raw const READY) == 0 {}
        let _d = DATA;
    }
    join(t);
}
"#
        ),
        "Data race detected between (1)",
    );
}

// --- The SIMD shim's memory events ---

macro_rules! simd {
    ($body:literal) => {
        concat!(
            "//- minicore: sized, copy, index, slice\n",
            target!(),
            prelude!(),
            r#"
#[repr(simd)]
#[derive(Clone, Copy)]
struct P2([*const u8; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct U2([usize; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct I2([i64; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct B2([u8; 2]);
#[repr(simd)]
#[derive(Clone, Copy)]
struct M2([i8; 2]);

#[rustc_intrinsic]
unsafe fn simd_shuffle<T, U, V>(a: T, b: T, idx: U) -> V;
#[rustc_intrinsic]
unsafe fn simd_gather<T, U, V>(val: T, ptr: U, mask: V) -> T;
#[rustc_intrinsic]
unsafe fn simd_expose_provenance<T, U>(ptr: T) -> U;
#[rustc_intrinsic]
unsafe fn simd_with_exposed_provenance<T, U>(addr: T) -> U;
#[rustc_intrinsic]
unsafe fn simd_add<T>(a: T, b: T) -> T;

fn main() {
"#,
            $body,
            "}\n"
        )
    };
}

#[test]
fn simd_shuffled_pointers_keep_their_provenance() {
    pass(simd!(
        r#"
    let a = 1u8;
    let b = 2u8;
    let v = P2([&a, &b]);
    let s: P2 = unsafe { simd_shuffle(v, v, [1u32, 0]) };
    let _x = unsafe { *s.0[0] };
"#
    ));
}

#[test]
fn simd_gather_out_of_bounds() {
    ub(
        simd!(
            r#"
    let v = [0u8; 4];
    let p = unsafe { arith_offset(&v as *const [u8; 4] as *const u8, 5) };
    let _g: B2 = unsafe { simd_gather(B2([0, 0]), P2([p, p]), M2([-1, 0])) };
"#
        ),
        "beyond the end of the allocation",
    );
}

#[test]
fn simd_with_exposed_provenance_of_an_address_never_exposed() {
    ub(
        simd!(
            r#"
    let p: P2 = unsafe { simd_with_exposed_provenance(U2([64, 64])) };
    let _x = unsafe { *p.0[0] };
"#
        ),
        "no provenance",
    );
}

#[test]
fn simd_exposed_lanes_get_their_provenance_back() {
    pass(simd!(
        r#"
    let a = 1u8;
    let u: U2 = unsafe { simd_expose_provenance(P2([&a, &a])) };
    let p: P2 = unsafe { simd_with_exposed_provenance(u) };
    let _x = unsafe { *p.0[0] };
"#
    ));
}

/// The copy of `u.v` (validity) or `simd_add`'s `require_init` stops it, whichever the
/// evaluator reaches first: both name uninitialized bytes.
#[test]
fn simd_add_of_an_uninitialized_lane() {
    ub(
        simd!(
            r#"
    #[derive(Clone, Copy)]
    union U {
        v: I2,
        b: (),
    }
    let u = U { b: () };
    let x = unsafe { u.v };
    let _s = unsafe { simd_add(x, I2([1, 1])) };
"#
        ),
        "uninitialized",
    );
}

// --- Outcomes other than UB ---

#[test]
fn panic_is_a_failure() {
    match run(program!(
        "sized, panic",
        r#"
fn main() {
    panic!("boom");
}
"#
    )) {
        Outcome::Failed { message, .. } => assert!(message.contains("boom"), "{message}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn unsupported_is_not_a_pass() {
    let outcome = run(program!(
        "sized",
        r#"
extern "C" {
    fn a_function_the_evaluator_does_not_know();
}

fn main() {
    unsafe { a_function_the_evaluator_does_not_know() };
}
"#
    ));
    assert!(matches!(outcome, Outcome::Unsupported { .. }), "{outcome:?}");
    assert!(!outcome.passed());
}

// --- The checker changes nothing a program does ---

#[test]
fn plain_and_checker_agree() {
    let fixture = program!(
        "sized, cell, copy, drop",
        r#"
use core::cell::Cell;

#[rustc_intrinsic]
unsafe fn size_of_val<T: ?Sized>(ptr: *const T) -> usize;
#[rustc_intrinsic]
unsafe fn align_of_val<T: ?Sized>(ptr: *const T) -> usize;

// The box is freed, as std's is: the checker would report it leaked.
impl<T: ?Sized> Drop for Box<T> {
    fn drop(&mut self) {
        unsafe {
            let (size, align) = (size_of_val(self.inner), align_of_val(self.inner));
            __rust_dealloc(self.inner as *mut u8, size, align);
        }
    }
}

fn next(c: &Cell<u8>) -> u8 {
    c.set(c.get() + 1);
    c.get()
}

fn main() {
    let c = Cell::new(b'a');
    let buf = [next(&c), next(&c), next(&c), next(&c)];
    let b = box_new(buf);
    unsafe { write(1, &*b as *const [u8; 4] as *const u8, 4) };
}
"#
    );
    let fx = fixture::db(fixture);
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let main = main_of(&db, file);
        let (plain, plain_out, _) = run_function(&db, main, Plain);
        let checker: Checker<'_> = Checker::new(8, &Config::default(), 0);
        let (checked, checked_out, checker) = run_function(&db, main, checker);
        assert!(plain.is_ok(), "{plain:?}");
        assert!(checked.is_ok(), "{checked:?}, the checker found {:?}", checker.found());
        assert!(checker.found().is_none(), "{:?}", checker.found());
        assert_eq!(plain_out.stdout(), checked_out.stdout());
        assert_eq!(plain_out.stdout(), "bcde");
        assert_eq!(plain_out.stderr(), checked_out.stderr());
    });
}

// --- The evaluator's atomics and memory shims under the checker ---

// A pointer through an atomic store and load keeps its provenance.
#[test]
fn pass_atomic_pointer_keeps_provenance() {
    pass(program!(
        "sized, copy",
        r#"
fn main() {
    let x = 7u32;
    let mut slot = 0 as *const u32;
    unsafe {
        atomic_store_release(&raw mut slot, &x as *const u32);
        let p = atomic_load_acquire(&raw const slot);
        let _v = *p;
    }
}
"#
    ));
}

#[test]
fn atomic_rmw_of_uninitialized_memory() {
    let (found, _) = ub(
        program!(
            "sized, copy",
            r#"
union U {
    a: u32,
    b: (),
}

fn main() {
    let mut u = U { b: () };
    unsafe { atomic_xadd_seqcst(&raw mut u.a, 1) };
}
"#
        ),
        "uninitialized",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::Uninit(_))));
}

// copy_nonoverlapping.rs
#[test]
fn copy_nonoverlapping_of_overlapping_ranges() {
    let (found, _) = ub(
        program!(
            "sized",
            r#"
#[rustc_intrinsic]
unsafe fn copy_nonoverlapping<T>(src: *const T, dst: *mut T, count: usize);

fn main() {
    let mut a = [1u8, 2, 3, 4];
    let p = &raw mut a as *mut u8;
    unsafe { copy_nonoverlapping(p as *const u8, arith_offset(p as *const u8, 1) as *mut u8, 2) };
}
"#
        ),
        "`copy_nonoverlapping` called on overlapping ranges",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::CopyOverlapping)));
}

#[test]
fn pass_copy_of_overlapping_ranges() {
    pass(program!(
        "sized",
        r#"
#[rustc_intrinsic]
unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize);

fn main() {
    let mut a = [1u8, 2, 3, 4];
    let p = &raw mut a as *mut u8;
    unsafe { copy(p as *const u8, arith_offset(p as *const u8, 1) as *mut u8, 2) };
    let _b = a;
}
"#
    ));
}

#[test]
fn memcpy_from_freed_memory() {
    let (found, _) = ub(
        program!(
            "sized",
            r#"
extern "C" {
    fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
}

fn main() {
    unsafe {
        let p = __rust_alloc(4, 4);
        __rust_dealloc(p, 4, 4);
        let mut b = [0u8; 4];
        memcpy(&raw mut b as *mut u8, p as *const u8, 4);
    }
}
"#
        ),
        "has been freed, so this pointer is dangling",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::UseAfterFree { .. })));
}

// unaligned_pointers/atomic_unaligned.rs
#[test]
fn atomic_load_unaligned() {
    let (found, _) = ub(
        program!(
            "sized",
            r#"
fn main() {
    let x = [0u32; 2];
    let p = unsafe { arith_offset(&x as *const [u32; 2] as *const u8, 1) } as *const u32;
    let _v = unsafe { atomic_load_relaxed(p) };
}
"#
        ),
        "but alignment 4 is required",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::Misaligned { .. })));
}

#[test]
fn pass_unaligned_volatile_load() {
    pass(program!(
        "sized",
        r#"
#[rustc_intrinsic]
unsafe fn unaligned_volatile_load<T>(src: *const T) -> T;

fn main() {
    let x = [1u8, 2, 3, 4, 5];
    let p = unsafe { arith_offset(&x as *const [u8; 5] as *const u8, 1) } as *const u16;
    let _v = unsafe { unaligned_volatile_load(p) };
}
"#
    ));
}

// A failed compare-exchange is a load with its failure ordering, which races with a
// non-atomic write on another thread.
#[test]
fn failed_cxchg_races_with_a_non_atomic_write() {
    let (found, _) = ub(
        program!(
            "sized, copy",
            r#"
#[rustc_intrinsic]
unsafe fn atomic_cxchg_seqcst_relaxed<T>(dst: *mut T, old: T, src: T) -> (T, bool);

static mut X: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    unsafe { X = 1 };
    0 as *mut u8
}

fn main() {
    let t = spawn(child, 0 as *mut u8);
    let _r = unsafe { atomic_cxchg_seqcst_relaxed(&raw mut X, 5, 6) };
    join(t);
}
"#
        ),
        "atomic load",
    );
    assert!(matches!(found.ub, Ub::Race(_)));
}

#[test]
fn sites_are_interned_densely() {
    let fx = fixture::db(program!("sized", "fn main() {}\n"));
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let owner = InferBodyId::DefWithBodyId(main_of(&db, file).into());
        let a = Site { owner, span: MirSpan::Unknown };
        let b = Site { owner, span: MirSpan::SelfParam };
        let mut sites = Sites::default();
        let ia = sites.intern(a);
        assert_eq!(sites.intern(a), ia);
        let ib = sites.intern(b);
        assert_ne!(ia, ib);
        assert_eq!(sites.intern(a), ia);
        assert_eq!(sites.len(), 2);
        assert_eq!(sites.get(ia), Some(a));
        assert_eq!(sites.get(ib), Some(b));
        assert_eq!(sites.get(hir_ty_ub::ids::SiteId::START), None);
    });
}

// --- C's heap, which std's `System` allocator uses ---

#[test]
fn calloc_memory_reads_as_zero_and_realloc_keeps_it() {
    pass(program!(
        "sized, slice, index, coerce_unsized",
        r#"
extern "C" {
    fn calloc(count: usize, size: usize) -> *mut u8;
    fn realloc(ptr: *mut u8, size: usize) -> *mut u8;
    fn malloc(size: usize) -> *mut u8;
    fn free(ptr: *mut u8);
}

fn main() {
    unsafe {
        let a = calloc(4, 2) as *mut [u8; 8];
        let _first = (*a)[0];
        let b = realloc(a as *mut u8, 16) as *mut [u8; 8];
        let _kept = (*b)[7];
        free(b as *mut u8);
        let none = malloc(0);
        if none as usize == 0 {
            let _ = *(0 as *const u8);
        }
        free(none);
    }
}
"#
    ));
}

#[test]
fn malloc_memory_is_uninitialized() {
    ub(
        program!(
            "sized",
            r#"
extern "C" {
    fn malloc(size: usize) -> *mut u8;
}

fn main() {
    let p = unsafe { malloc(4) } as *const u32;
    let _x = unsafe { *p };
}
"#
        ),
        "uninitialized",
    );
}

#[test]
fn free_of_rusts_allocation_is_ub() {
    let (found, _) = ub(
        program!(
            "sized",
            r#"
extern "C" {
    fn free(ptr: *mut u8);
}

fn main() {
    unsafe { free(__rust_alloc(8, 8)) };
}
"#
        ),
        "which is Rust heap memory, using C heap deallocation operation",
    );
    assert!(matches!(found.ub, Ub::Memory(MemError::FreeWrongKind { .. })));
}
