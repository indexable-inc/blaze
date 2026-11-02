//! Threads in the evaluator, through the shims programs call: `pthread_create` and
//! `pthread_join` with a return value, two threads adding to one atomic, a futex wait another
//! thread wakes (and one that times out), a thread that outlives its `pthread_detach`, a key's
//! value per thread with its destructor run at the thread's exit, and a condition variable.
//! Each program runs on Linux x86-64 under [`Plain`], whose scheduler keeps the running thread
//! until it blocks or ends, so what each prints comes in one order.

use hir_ty_mir::mir::{Plain, run_function};
use hir_expand::fixture;

use crate::function;

/// What every program may use: the pthread, futex, clock and `errno` shims, `write`, and the
/// atomics.
macro_rules! prelude {
    () => {
        r#"
extern "C" {
    fn pthread_create(
        thread: *mut usize,
        attr: *const u8,
        start: extern "C" fn(*mut u8) -> *mut u8,
        arg: *mut u8,
    ) -> i32;
    fn pthread_join(thread: usize, ret: *mut *mut u8) -> i32;
    fn pthread_detach(thread: usize) -> i32;
    fn pthread_key_create(key: *mut u32, dtor: Option<unsafe extern "C" fn(*mut u8)>) -> i32;
    fn pthread_getspecific(key: u32) -> *mut u8;
    fn pthread_setspecific(key: u32, value: *const u8) -> i32;
    fn pthread_mutex_lock(mutex: *mut [u64; 5]) -> i32;
    fn pthread_mutex_unlock(mutex: *mut [u64; 5]) -> i32;
    fn pthread_cond_wait(cond: *mut [u64; 6], mutex: *mut [u64; 5]) -> i32;
    fn pthread_cond_signal(cond: *mut [u64; 6]) -> i32;
    fn syscall(
        num: i64,
        addr: *const u32,
        op: i32,
        val: u32,
        timeout: *const [i64; 2],
        addr2: *const u32,
        val3: u32,
    ) -> i64;
    fn clock_gettime(clock: i32, out: *mut [i64; 2]) -> i32;
    fn __errno_location() -> *mut i32;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

#[rustc_intrinsic]
unsafe fn atomic_load_seqcst<T>(src: *const T) -> T;
#[rustc_intrinsic]
unsafe fn atomic_store_seqcst<T>(dst: *mut T, val: T);
#[rustc_intrinsic]
unsafe fn atomic_xadd_seqcst<T>(dst: *mut T, src: T) -> T;

const SYS_FUTEX: i64 = 202;
const FUTEX_WAIT_PRIVATE: i32 = 128;
const FUTEX_WAKE_PRIVATE: i32 = 129;

fn put(byte: u8) {
    unsafe { write(1, &byte as *const u8, 1) };
}

fn spawn(start: extern "C" fn(*mut u8) -> *mut u8, arg: *mut u8) -> usize {
    let mut thread = 0usize;
    unsafe { pthread_create(&mut thread, 0 as *const u8, start, arg) };
    thread
}

fn futex_wait(word: *const u32, expected: u32) -> i64 {
    let none = 0 as *const [i64; 2];
    unsafe { syscall(SYS_FUTEX, word, FUTEX_WAIT_PRIVATE, expected, none, 0 as *const u32, 0) }
}

fn futex_wake(word: *const u32) -> i64 {
    let none = 0 as *const [i64; 2];
    unsafe { syscall(SYS_FUTEX, word, FUTEX_WAKE_PRIVATE, 1, none, 0 as *const u32, 0) }
}
"#
    };
}

macro_rules! program {
    ($body:literal) => {
        concat!(
            "//- minicore: sized, option, index, slice\n",
            "//- /main.rs cfg:target_os=linux,target_arch=x86_64\n",
            prelude!(),
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

#[test]
fn spawn_and_join_with_a_return_value() {
    prints(
        program!(
            r#"
extern "C" fn child(arg: *mut u8) -> *mut u8 {
    put(b'c');
    (arg as usize + 1) as *mut u8
}

fn main() {
    let t = spawn(child, 41 as *mut u8);
    put(b'm');
    let mut ret = 0 as *mut u8;
    let code = unsafe { pthread_join(t, &mut ret) };
    put(b'0' + code as u8);
    put(ret as usize as u8);
}
"#
        ),
        // `main` goes on after the spawn until the join blocks it; 42 is `*`.
        "mc0*",
    );
}

#[test]
fn two_threads_add_to_one_atomic() {
    prints(
        program!(
            r#"
static mut COUNT: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    let mut i = 0;
    while i < 100 {
        unsafe { atomic_xadd_seqcst(&raw mut COUNT, 1) };
        i += 1;
    }
    0 as *mut u8
}

fn main() {
    let a = spawn(child, 0 as *mut u8);
    let b = spawn(child, 0 as *mut u8);
    unsafe { pthread_join(a, 0 as *mut *mut u8) };
    unsafe { pthread_join(b, 0 as *mut *mut u8) };
    let total = unsafe { atomic_load_seqcst(&raw const COUNT) };
    put(if total == 200 { b'y' } else { b'n' });
}
"#
        ),
        "y",
    );
}

#[test]
fn a_futex_wait_another_thread_wakes() {
    prints(
        program!(
            r#"
static mut FLAG: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    put(b'c');
    unsafe { atomic_store_seqcst(&raw mut FLAG, 1) };
    let woken = futex_wake(&raw const FLAG);
    put(b'0' + woken as u8);
    0 as *mut u8
}

fn main() {
    // A wait for a value the word does not hold returns at once with `EAGAIN`.
    let code = futex_wait(&raw const FLAG, 7);
    let errno = unsafe { *__errno_location() };
    put(if code == -1 && errno == 11 { b'e' } else { b'?' });
    let t = spawn(child, 0 as *mut u8);
    while unsafe { atomic_load_seqcst(&raw const FLAG) } == 0 {
        let code = futex_wait(&raw const FLAG, 0);
        put(b'0' + code as u8);
    }
    put(b'm');
    unsafe { pthread_join(t, 0 as *mut *mut u8) };
}
"#
        ),
        // `main` blocks on the futex; the child wakes it (one woken) and ends; the wait gives 0.
        "ec10m",
    );
}

#[test]
fn a_timed_futex_wait_nobody_wakes_times_out() {
    prints(
        program!(
            r#"
static mut WORD: u32 = 0;

fn main() {
    let mut before = [0i64; 2];
    let mut after = [0i64; 2];
    unsafe { clock_gettime(1, &mut before) };
    let timeout = [1i64, 0];
    let code = unsafe {
        syscall(SYS_FUTEX, &raw const WORD, FUTEX_WAIT_PRIVATE, 0, &timeout, 0 as *const u32, 0)
    };
    let errno = unsafe { *__errno_location() };
    unsafe { clock_gettime(1, &mut after) };
    put(if code == -1 && errno == 110 { b't' } else { b'?' });
    let waited = (after[0] - before[0]) * 1_000_000_000 + (after[1] - before[1]);
    put(if waited >= 1_000_000_000 { b's' } else { b'?' });
}
"#
        ),
        "ts",
    );
}

#[test]
fn a_detached_thread_outlives_the_detach() {
    prints(
        program!(
            r#"
static mut DONE: u32 = 0;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    put(b'd');
    unsafe { atomic_store_seqcst(&raw mut DONE, 1) };
    futex_wake(&raw const DONE);
    0 as *mut u8
}

fn main() {
    let t = spawn(child, 0 as *mut u8);
    let code = unsafe { pthread_detach(t) };
    put(b'0' + code as u8);
    while unsafe { atomic_load_seqcst(&raw const DONE) } == 0 {
        futex_wait(&raw const DONE, 0);
    }
    put(b'm');
}
"#
        ),
        "0dm",
    );
}

#[test]
fn joining_a_detached_thread_is_an_error() {
    let (result, _) = run(program!(
        r#"
extern "C" fn child(_: *mut u8) -> *mut u8 {
    0 as *mut u8
}

fn main() {
    let t = spawn(child, 0 as *mut u8);
    unsafe { pthread_detach(t) };
    unsafe { pthread_join(t, 0 as *mut *mut u8) };
}
"#
    ));
    let error = result.expect_err("the join of a detached thread");
    assert!(error.contains("which is detached"), "{error}");
}

#[test]
fn a_key_holds_a_value_per_thread_and_runs_its_destructor_at_exit() {
    prints(
        program!(
            r#"
static mut KEY: u32 = 0;
static A: u8 = b'a';
static B: u8 = b'b';

unsafe extern "C" fn destructor(value: *mut u8) {
    put(unsafe { *value });
}

extern "C" fn child(_: *mut u8) -> *mut u8 {
    let key = unsafe { KEY };
    let initial = unsafe { pthread_getspecific(key) };
    put(if initial as usize == 0 { b'n' } else { b'?' });
    unsafe { pthread_setspecific(key, &B) };
    put(unsafe { *pthread_getspecific(key) });
    0 as *mut u8
}

fn main() {
    unsafe { pthread_key_create(&raw mut KEY, Some(destructor)) };
    let key = unsafe { KEY };
    unsafe { pthread_setspecific(key, &A) };
    let t = spawn(child, 0 as *mut u8);
    unsafe { pthread_join(t, 0 as *mut *mut u8) };
    put(unsafe { *pthread_getspecific(key) });
}
"#
        ),
        // The child starts with no value, sets its own, and its destructor prints it as the
        // child exits; `main` still has its own. The main thread runs no key destructors.
        "nbba",
    );
}

#[test]
fn a_condition_variable_waiter_wakes_holding_the_mutex() {
    prints(
        program!(
            r#"
static mut MUTEX: [u64; 5] = [0; 5];
static mut COND: [u64; 6] = [0; 6];
static mut READY: bool = false;

extern "C" fn child(_: *mut u8) -> *mut u8 {
    unsafe {
        pthread_mutex_lock(&raw mut MUTEX);
        READY = true;
        pthread_cond_signal(&raw mut COND);
        put(b's');
        pthread_mutex_unlock(&raw mut MUTEX);
    }
    0 as *mut u8
}

fn main() {
    unsafe {
        pthread_mutex_lock(&raw mut MUTEX);
        let t = spawn(child, 0 as *mut u8);
        while !READY {
            let code = pthread_cond_wait(&raw mut COND, &raw mut MUTEX);
            put(b'0' + code as u8);
        }
        // Held again: unlocking it is no error.
        let code = pthread_mutex_unlock(&raw mut MUTEX);
        put(b'0' + code as u8);
        pthread_join(t, 0 as *mut *mut u8);
    }
}
"#
        ),
        "s00",
    );
}
