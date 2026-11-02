//! Frames and unwinding in the evaluator: a panic runs the cleanup of each frame it leaves
//! and stops at a `catch_unwind`, an uncaught one is the run's error after its cleanups, a
//! panic out of a function that can't unwind or out of a destructor during cleanup aborts,
//! and drops and closures run as frames: an ADT's fields are dropped as its `Drop` impl left
//! them, and a closure `FnOnce::call_once` consumed is dropped after its call. Each program
//! runs on Linux x86-64 under [`Plain`] with minicore, which has no `#[panic_handler]`, so a
//! panic is the evaluator's own (a failed bounds check here).

use hir_ty_mir::mir::{Plain, run_function};
use hir_expand::fixture;

use crate::function;

macro_rules! program {
    ($body:literal) => {
        concat!(
            "//- minicore: sized, drop, fn, copy\n",
            "//- /main.rs cfg:target_os=linux,target_arch=x86_64\n",
            r#"
extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn pthread_create(
        thread: *mut usize,
        attr: *const u8,
        start: extern "C" fn(*mut u8) -> *mut u8,
        arg: *mut u8,
    ) -> i32;
    fn pthread_join(thread: usize, ret: *mut *mut u8) -> i32;
}

#[rustc_intrinsic]
unsafe fn catch_unwind(try_fn: fn(*mut u8), data: *mut u8, catch_fn: fn(*mut u8, *mut u8)) -> i32;

fn put(byte: u8) {
    unsafe { write(1, &byte as *const u8, 1) };
}

struct Noisy(u8);

impl Drop for Noisy {
    fn drop(&mut self) {
        put(self.0);
    }
}

fn index() -> usize {
    5
}

/// Panics: the bounds check of an index past the end.
fn fail() {
    let array = [0u8; 2];
    let _ = array[index()];
}
"#,
            $body
        )
    };
}

/// Runs the program's `main` and gives how it ended and what it printed.
fn run(fixture: &str) -> (Result<(), String>, String) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let main = function(&db, "main");
        let (result, output, Plain) = run_function(&db, main, Plain);
        (result.map_err(|e| format!("{e:?}")), output.stdout().into_owned())
    })
}

#[track_caller]
fn prints(fixture: &str, expected: &str) {
    let (result, stdout) = run(fixture);
    assert!(result.is_ok(), "{result:?}, printed {stdout:?}");
    assert_eq!(stdout, expected);
}

#[track_caller]
fn fails(fixture: &str, error: &str, expected: &str) {
    let (result, stdout) = run(fixture);
    let Err(found) = result else { panic!("ran to its end, printed {stdout:?}") };
    assert!(found.contains(error), "{found}");
    assert_eq!(stdout, expected);
}

#[test]
fn catch_unwind_stops_a_panic_after_the_cleanups() {
    prints(
        program!(
            r#"
fn inner() {
    let _b = Noisy(b'b');
    fail();
    put(b'x');
}

fn body(_: *mut u8) {
    let _a = Noisy(b'a');
    inner();
    put(b'y');
}

fn caught(_: *mut u8, _: *mut u8) {
    put(b'c');
}

fn main() {
    let code = unsafe { catch_unwind(body, 0 as *mut u8, caught) };
    put(b'0' + code as u8);
    let code = unsafe { catch_unwind(|_| put(b'n'), 0 as *mut u8, caught) };
    put(b'0' + code as u8);
}
"#
        ),
        // The innermost frame's cleanup first; `catch_fn` runs once the `try` frame is left.
        "bac1n0",
    );
}

#[test]
fn an_uncaught_panic_is_the_error_after_its_cleanups() {
    fails(
        program!(
            r#"
fn main() {
    let _a = Noisy(b'a');
    put(b'm');
    fail();
}
"#
        ),
        "index out of bounds: the len is 2 but the index is 5",
        "ma",
    );
}

#[test]
fn a_panic_in_a_destructor_during_cleanup_aborts() {
    fails(
        program!(
            r#"
struct Bad;

impl Drop for Bad {
    fn drop(&mut self) {
        put(b'd');
        fail();
    }
}

fn body(_: *mut u8) {
    let _bad = Bad;
    fail();
}

fn caught(_: *mut u8, _: *mut u8) {
    put(b'c');
}

fn main() {
    unsafe { catch_unwind(body, 0 as *mut u8, caught) };
}
"#
        ),
        "panic in a destructor during cleanup",
        "d",
    );
}

#[test]
fn a_panic_out_of_a_function_that_cannot_unwind_aborts() {
    fails(
        program!(
            r#"
extern "C" fn child(_: *mut u8) -> *mut u8 {
    let _a = Noisy(b'a');
    fail();
    0 as *mut u8
}

fn main() {
    let mut thread = 0usize;
    unsafe { pthread_create(&mut thread, 0 as *const u8, child, 0 as *mut u8) };
    put(b'm');
    let mut ret = 0 as *mut u8;
    unsafe { pthread_join(thread, &mut ret) };
}
"#
        ),
        // The thread's own cleanup runs before it leaves `child`, which is `extern "C"`.
        "panic in a function that cannot unwind",
        "ma",
    );
}

#[test]
fn fields_are_dropped_as_the_drop_impl_left_them() {
    prints(
        program!(
            r#"
enum Slot {
    Full(Noisy),
    Empty,
}

struct Holder(Slot, Noisy);

impl Drop for Holder {
    fn drop(&mut self) {
        put(b'h');
        // Drops the `Full` it held: its glue then sees `Empty`.
        self.0 = Slot::Empty;
    }
}

fn main() {
    let _holder = Holder(Slot::Full(Noisy(b'f')), Noisy(b'n'));
}
"#
        ),
        "hfn",
    );
}

#[test]
fn a_closure_call_once_consumed_is_dropped_after_its_call() {
    prints(
        program!(
            r#"
fn call<F: FnOnce()>(f: F) {
    f();
    put(b'r');
}

fn main() {
    let noisy = Noisy(b'd');
    call(move || {
        let _ = &noisy;
        put(b'f');
    });
    put(b'e');
}
"#
        ),
        // The closure is `Fn`: `call_once` borrows it for the call, then drops it.
        "fdre",
    );
}
