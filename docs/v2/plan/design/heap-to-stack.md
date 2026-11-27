---
status: plan
summary: HeapToStack, blaze's own pass: a Box::new, Rc::new or small vec! whose allocation never leaves its function lives in the function's frame instead, only in programs whose allocator is std's, capped per value and per frame, and never in a frame that can recur. A LIR-level escape pass exists, built, not wired, without the allocator gate or the recursion rule.
date: 2026-11-02
---
# HeapToStack: allocations that never leave their function

## What exists

`crates/blaze-lir/src/simplify/escape.rs`, `promote_heap_to_stack`: a fixed-size allocation
(a call classified `AllocRole::Alloc`) whose pointer and every pointer derived from it never
escape the function (never returned, stored as a value, passed to an escaping call, cast to an
integer or carried across loop iterations) becomes a frame slot, and its frees go; SROA and the
other simplify passes then fold the slot. Caps: 1 to 4096 bytes, alignment up to 64. Built, not
wired: nothing outside its own tests calls it. It does not yet hold to the rules below.

## What it moves

- `Box::new(value)` into a local used only through `*b`, dropped and freed: the local becomes
  the value, and the free goes.
- `Rc::new(value)` into a local only dereferenced and dropped: never cloned, counted,
  downgraded or moved, so its count is one when it drops. The local becomes the value.
- `vec![a, b, ..]` of elements with nothing to drop into a `Vec` only borrowed for `Deref`,
  `DerefMut`, `Index`, `IndexMut`, `IntoIterator` of `&Vec` and `&mut Vec`, `len`, `is_empty`,
  `capacity`, `as_slice` and `as_mut_slice`, and dropped. None of those can grow it, reallocate
  it or hand out the `Vec` itself (`as_ptr` could, through `ptr::read` of the `Vec`), so its
  capacity stays the array's.

A zero-sized `Box` or `vec!` stays where it is: it allocates nothing already, and its pointer,
the alignment, is one the program can read. Every candidate not moved is logged with the reason
under a `heap_to_stack` span.

## What it relies on

`GlobalAlloc`'s contract at the pinned nightly (`core/src/alloc/global.rs`): "you cannot rely
on global allocations actually happening ... Whether allocations happen or not is not part of
the program behavior, even if it could be detected via an allocator that tracks allocations by
printing or otherwise having side effects." rustc's LLVM already removes allocations it can
see through, even under a counting `#[global_allocator]`.

blaze holds itself to more than that. The pass runs only for the units of a program whose
global allocator is std's (no `#[global_allocator]`, or one that is `std::alloc::System`). It
never runs for a library unit, which serves programs with different allocators, nor for the
library's own bodies, nor for a body inlined into another (its slot would be in the caller's
frame, which may recur). A program that counts its allocations with its own allocator, and
reads the count, keeps every allocation; so does one whose allocator cannot be told (logged
with why, not an error). Two `#[global_allocator]`s are an error naming both crates in order
of name.

Keys: whether the pass runs is part of each function's settings, set only for functions that are
not std's, so one stored copy of each std body serves programs with and without a counting
allocator.

What it cannot see:
- A C `malloc` put in place under `System` (`LD_PRELOAD`, an unprefixed jemalloc) whose
  statistics the program reads. The contract already lets LLVM remove those calls.
- Code the program registered with std that std runs from inside a moved frame: a panic hook,
  the allocation-error hook, `atexit` handlers, signal handlers. Each is one more frame level
  per panic, failed allocation, exit or signal.

What stays the same: the value, the operand it is built from, and what drops when and in what
order, unwinding included. The constructors' own unwind edges go, because their one way to
unwind was a failed allocation.

## Stack

- At most 512 bytes per value and an alignment of at most 16.
- At most 2 KiB per body over every value it moves, taken in the order of their first
  constructor's block.
- One slot per allocation site, which a loop reuses.

Never across recursion: the pass moves nothing in a body that could call itself again. Every
call in such a body is a constructor, one of the calls the pass rewrites, or a library function
whose generic arguments and argument types are closed (made only of the library's types: no
parameter, closure, `dyn`, function pointer, program function or program type). By the orphan
rule such a call can reach only the library's impls; with no `asm!` and no tail call, a moved
frame is the deepest of the program's frames on its thread. `println!` makes a body open
(`fmt::Arguments` holds formatter function pointers), so a function that prints keeps its
allocations.

## Not done yet

- Wiring the pass into the pipeline, with the gate and the recursion rule above.
- `String` (`String::from`, `format!`), `Vec::with_capacity(N)` with a constant `N` and pushes
  bounded by it, and `vec![x; N]`.
- `Arc`: its drop's `Acquire` fence is a synchronization another thread can see. It needs a
  proof that the `Arc` was never shared before it can go.
- A call-graph SCC test in place of the closed-body rule.
- `Vec`s of elements with drop glue: the elements' drop has to stay while the free goes.
