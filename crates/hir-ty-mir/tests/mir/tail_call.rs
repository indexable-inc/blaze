//! `become` in the evaluator: the caller's locals are dropped before the callee runs, the
//! callee runs in the caller's place and returns where the caller would have, a million-deep
//! chain of tail calls runs, a call through a function pointer works, and a bottom frame's
//! tail call returns to the evaluation. Each program runs on Linux x86-64 under [`Plain`] with minicore.

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
}

fn put(byte: u8) {
    unsafe { write(1, &byte as *const u8, 1) };
}

fn put_u64(mut n: u64) {
    let mut digits = [0u8; 20];
    let mut at = 20;
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    unsafe { write(1, &digits[at] as *const u8, 20 - at) };
}

struct Noisy(u8);

impl Drop for Noisy {
    fn drop(&mut self) {
        put(self.0);
    }
}
"#,
            $body
        )
    };
}

#[track_caller]
fn prints(fixture: &str, expected: &str) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    let (result, stdout) = hir_ty::attach_db(&db, || {
        let main = function(&db, "main");
        let (result, output, Plain) = run_function(&db, main, Plain);
        (result.map_err(|e| format!("{e:?}")), output.stdout().into_owned())
    });
    assert!(result.is_ok(), "{result:?}, printed {stdout:?}");
    assert_eq!(stdout, expected);
}

#[test]
fn become_drops_the_caller_then_runs_the_callee_in_its_place() {
    prints(
        program!(
            r#"
fn count(n: u64, acc: Noisy) -> u64 {
    let _local = Noisy(b'l');
    if n == 0 {
        put(acc.0);
        return 0;
    }
    become count(n - 1, Noisy(if n % 2 == 0 { b'e' } else { b'o' }));
}

fn main() {
    put_u64(count(3, Noisy(b's')));
}
"#
        ),
        // Each `become` drops `_local`, then the argument it was given; the last call prints
        // its argument and drops both as it returns, into `main`'s call.
        "lsloleolo0",
    );
}

#[test]
fn a_million_tail_calls_run() {
    prints(
        program!(
            r#"
fn sum(n: u64, acc: u64) -> u64 {
    if n == 0 { acc } else { become sum(n - 1, acc + n) }
}

fn main() {
    put_u64(sum(1_000_000, 0));
}
"#
        ),
        "500000500000",
    );
}

#[test]
fn become_through_a_function_pointer() {
    prints(
        program!(
            r#"
fn sum(n: u64, acc: u64) -> u64 {
    if n == 0 { acc } else { become sum(n - 1, acc + n) }
}

fn via_pointer(n: u64, acc: u64) -> u64 {
    let f: fn(u64, u64) -> u64 = sum;
    let _outer = Noisy(b'x');
    become f(n, acc);
}

fn main() {
    put_u64(via_pointer(10, 0));
}
"#
        ),
        "x55",
    );
}

#[test]
fn the_bottom_frame_tail_calls() {
    prints(
        program!(
            r#"
fn finish() {
    let _f = Noisy(b'f');
    put(b'z');
}

fn main() {
    let _m = Noisy(b'a');
    put(b'm');
    become finish();
}
"#
        ),
        "mazf",
    );
}
