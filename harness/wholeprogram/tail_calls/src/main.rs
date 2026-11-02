//! `become` (`explicit_tail_calls`): a guaranteed tail call reuses its caller's frame, so a
//! chain of them of any length runs in the stack of one, and the caller's locals are dropped
//! before the callee runs. The cases are rustc's own (`tests/ui/explicit-tail-calls/`), with
//! an interpreter loop of ten million steps on a thread whose stack holds a few thousand
//! frames at most: a `become` made as a call and a return overflows it.
#![expect(incomplete_features)]
#![feature(explicit_tail_calls)]

use std::cell::RefCell;

/// An interpreter's handlers, each of which `become`s the next one's through the table, a
/// function pointer, or by name.
type Handler = fn(&[u8], usize, u64, u64) -> u64;

static HANDLERS: [Handler; 3] = [add, mix, count];

#[inline(never)]
fn step(code: &[u8], pc: usize, acc: u64, left: u64) -> u64 {
    become HANDLERS[code[pc] as usize](code, pc, acc, left)
}

fn add(code: &[u8], pc: usize, acc: u64, left: u64) -> u64 {
    become step(code, pc + 1, acc.wrapping_add(left), left)
}

fn mix(code: &[u8], pc: usize, acc: u64, left: u64) -> u64 {
    become step(code, pc + 1, (acc ^ 0x9e37_79b9_7f4a_7c15).rotate_left(7), left)
}

fn count(code: &[u8], _: usize, acc: u64, left: u64) -> u64 {
    if left == 0 {
        return acc;
    }
    become step(code, 0, acc, left - 1)
}

// drop-order.rs: a `become`'s caller drops its locals before the callee runs.
struct DropCounter<'a>(u8, &'a RefCell<Vec<u8>>);

impl Drop for DropCounter<'_> {
    #[track_caller]
    fn drop(&mut self) {
        self.1.borrow_mut().push(self.0);
    }
}

fn tail_recursive(n: u8, order: &RefCell<Vec<u8>>) {
    if n >= 128 {
        return;
    }
    let _local = DropCounter(n, order);
    become tail_recursive(n + 1, order)
}

fn simply_recursive(n: u8, order: &RefCell<Vec<u8>>) {
    if n >= 128 {
        return;
    }
    let _local = DropCounter(n, order);
    return simply_recursive(n + 1, order);
}

fn out_of_inner_scope(order: &RefCell<Vec<u8>>) {
    fn inner(order: &RefCell<Vec<u8>>) {
        let _7 = DropCounter(7, order);
        let _6 = DropCounter(6, order);
    }
    let _5 = DropCounter(5, order);
    let _4 = DropCounter(4, order);
    if true {
        let _3 = DropCounter(3, order);
        let _2 = DropCounter(2, order);
        loop {
            let _1 = DropCounter(1, order);
            let _0 = DropCounter(0, order);
            become inner(order);
        }
    }
}

// indirect.rs: an argument passed by reference is written to the memory the caller was handed
// for its own, never handed over from the caller's frame, which the callee's replaces.
#[repr(C)]
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Big([u64; 4]);

#[inline(never)]
fn update_in_caller(y: Big) -> u64 {
    #[inline(never)]
    fn helper(x: Big) -> u64 {
        x.0.iter().sum()
    }
    let x = Big([y.0[0], 2, 3, 4]);
    become helper(x)
}

#[inline(never)]
fn swapper<T>(a: T, b: T) -> (T, T) {
    #[inline(never)]
    fn helper<T>(a: T, b: T) -> (T, T) {
        (a, b)
    }
    become helper(b, a)
}

#[inline(never)]
fn swapper_derived(a: Big, _: (u64, u64), b: Big, _: (u64, u64)) -> ((u64, u64), (u64, u64)) {
    #[inline(never)]
    fn helper(_: Big, x: (u64, u64), _: Big, y: (u64, u64)) -> ((u64, u64), (u64, u64)) {
        (x, y)
    }
    become helper(b, (a.0[0], b.0[0]), a, (a.0[0], b.0[0]));
}

// callee_is_track_caller.rs: the callee is told no caller, as through a function pointer.
fn to_tracked(x: u32) -> u32 {
    become tracked(x);
}

#[track_caller]
fn tracked(x: u32) -> u32 {
    x + 42
}

/// A result returned through memory the caller was handed (`PassMode::Indirect`).
#[inline(never)]
fn countdown(n: u64, acc: Big) -> Big {
    if n == 0 {
        return acc;
    }
    become countdown(n - 1, Big([acc.0[1], acc.0[2], acc.0[3], acc.0[0].wrapping_add(n)]))
}

fn main() {
    let steps = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            let code = [0u8, 1, 0, 1, 2];
            (step(&code, 0, 1, 10_000_000), countdown(10_000_000, Big([1, 2, 3, 4])))
        })
        .expect("a thread")
        .join()
        .expect("the interpreter's thread");
    println!("interpreter {:016x}", steps.0);
    println!("countdown {:?}", steps.1);

    let tail_counter = Default::default();
    tail_recursive(0, &tail_counter);
    assert_eq!(tail_counter.into_inner(), (0..128).collect::<Vec<u8>>());
    let simply_counter = Default::default();
    simply_recursive(0, &simply_counter);
    assert_eq!(simply_counter.into_inner(), (0..128).rev().collect::<Vec<u8>>());
    let scope_counter = Default::default();
    out_of_inner_scope(&scope_counter);
    assert_eq!(scope_counter.into_inner(), (0..8).collect::<Vec<u8>>());
    println!("drop order ok");

    assert_eq!(update_in_caller(Big::default()), 2 + 3 + 4);
    assert_eq!(swapper(u8::MIN, u8::MAX), (u8::MAX, u8::MIN));
    assert_eq!(swapper(i128::MIN, i128::MAX), (i128::MAX, i128::MIN));
    assert_eq!(swapper(Big([1; 4]), Big([2; 4])), (Big([2; 4]), Big([1; 4])));
    assert_eq!(swapper_derived(Big([1; 4]), (0, 0), Big([2; 4]), (0, 0)), ((1, 2), (1, 2)));
    println!("indirect ok");

    assert_eq!(to_tracked(12), 54);
    println!("track_caller ok");
}
