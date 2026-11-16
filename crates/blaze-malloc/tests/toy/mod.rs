//! A control for Miri, nothing of blaze-malloc's: a few dozen lines of `#[global_allocator]` over
//! one static buffer, with our provenance exposed as blaze-malloc's is, and a last-in-first-out
//! free list of 64-byte blocks, so a freed block is the next one handed out. `V` picks what
//! `dealloc` does with the block's bytes:
//!
//! - `OWN` (1): writes the free list's link into the block through our provenance.
//! - `CALLER` (2): writes the link through the pointer `dealloc` was given; `alloc` reads it back
//!   through ours.
//! - `NOTHING` (3): writes nothing; the free list is a side array; the block is handed out again at
//!   once and its new owner writes it.
//!
//! Each variant is its own test binary (`toy_own.rs`, `toy_caller.rs`, `toy_nothing.rs`) running
//! the same three cases ([`cases`]).

#![allow(dead_code, reason = "each binary uses its variant's half")]

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

pub const OWN: u8 = 1;
pub const CALLER: u8 = 2;
pub const NOTHING: u8 = 3;

const BYTES: usize = 8 << 20;
const BLOCK: usize = 64;
const SIDE: usize = 4096;

#[repr(C, align(4096))]
struct Buf(UnsafeCell<[u8; BYTES]>);

/// The allocator's state, under one spin lock: the bump offset, the list's head (an address,
/// 0 for none), and for `NOTHING` the side array of free addresses.
struct State {
    bump: usize,
    head: usize,
    side: [usize; SIDE],
    len: usize,
}

pub struct Toy<const V: u8> {
    buf: Buf,
    lock: AtomicBool,
    state: UnsafeCell<State>,
}

// SAFETY: `state` is touched only under `lock`; the buffer's bytes are handed out disjointly.
unsafe impl<const V: u8> Sync for Toy<V> {}

impl<const V: u8> Toy<V> {
    pub const fn new() -> Toy<V> {
        Toy {
            buf: Buf(UnsafeCell::new([0; BYTES])),
            lock: AtomicBool::new(false),
            state: UnsafeCell::new(State { bump: 0, head: 0, side: [0; SIDE], len: 0 }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State, *mut u8) -> R) -> R {
        while self.lock.swap(true, Ordering::Acquire) {
            std::hint::spin_loop();
        }
        let base = self.buf.0.get().cast::<u8>();
        let _ = base.expose_provenance();
        // SAFETY: under the lock.
        let out = f(unsafe { &mut *self.state.get() }, base);
        self.lock.store(false, Ordering::Release);
        out
    }
}

/// Our pointer to `addr`: from the exposed provenance of the buffer, as blaze-malloc's.
fn ours(addr: usize) -> *mut u8 {
    ptr::with_exposed_provenance_mut(addr)
}

// SAFETY: blocks are disjoint ranges of the static buffer, each handed out once until freed.
unsafe impl<const V: u8> GlobalAlloc for Toy<V> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|s, base| {
            if layout.size() <= BLOCK && layout.align() <= BLOCK {
                if V == NOTHING && s.len > 0 {
                    s.len -= 1;
                    return ours(s.side[s.len]);
                }
                if V != NOTHING && s.head != 0 {
                    let at = s.head;
                    // SAFETY: a free block of ours: its first word is the link.
                    s.head = unsafe { ours(at).cast::<usize>().read() };
                    return ours(at);
                }
            }
            let size = layout.size().max(BLOCK).next_multiple_of(BLOCK);
            let start = (base.addr() + s.bump).next_multiple_of(layout.align().max(BLOCK));
            if start + size > base.addr() + BYTES {
                return ptr::null_mut();
            }
            s.bump = start + size - base.addr();
            base.wrapping_add(start - base.addr())
        })
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        if layout.size() > BLOCK || layout.align() > BLOCK {
            return; // Large: never reused.
        }
        self.with(|s, _| match V {
            // SAFETY (all three): the block is given up and is at least one word.
            OWN => unsafe {
                ours(block.addr()).cast::<usize>().write(s.head);
                s.head = block.addr();
            },
            CALLER => unsafe {
                block.cast::<usize>().write(s.head);
                s.head = block.addr();
            },
            _ => {
                if s.len < SIDE {
                    s.side[s.len] = block.addr();
                    s.len += 1;
                }
            }
        })
    }
}

/// `drop(b)` inside the function that took `b` by value: `b`'s memory is protected until it
/// returns.
#[inline(never)]
pub fn drop_inside(b: Box<u64>) {
    drop(b)
}

/// As [`drop_inside`], then a new box (the same block, last in first out) written inside.
#[inline(never)]
pub fn reuse_inside(b: Box<u64>) -> u64 {
    let old = b.as_ref() as *const u64 as usize;
    drop(b);
    let mut c = Box::new(7u64);
    *c += 1;
    let same = (&*c as *const u64 as usize) == old;
    *c + u64::from(same)
}

/// The three cases each variant runs.
macro_rules! cases {
    () => {
        #[test]
        fn box_dropped_inside_its_by_value_function() {
            toy::drop_inside(Box::new(7));
        }

        /// Expected to fail under Tree Borrows only (pass natively and under Stacked Borrows):
        /// the named limit (docs/v2/plan/design/malloc.md). Tree Borrows keeps the freed `Box`'s
        /// protector until `reuse_inside` returns, so handing its bytes out again inside counts as
        /// a foreign access to a protected tag, even for the `nothing` toy, which writes nothing
        /// into the freed block. No reusing allocator passes it; Miri has no cfg that names the
        /// model, so the test is left to fail there, with this as its explanation.
        #[test]
        fn box_dropped_then_reused_inside_its_by_value_function() {
            let got = toy::reuse_inside(Box::new(7));
            assert!(got == 8 || got == 9, "{got}");
        }

        #[test]
        fn thread_spawn_and_join() {
            let handle = std::thread::spawn(|| Box::new(5u64));
            assert_eq!(*handle.join().expect("the thread ends"), 5);
        }
    };
}
