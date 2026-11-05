---
status: plan
summary: HeapToStack, blaze's own MIR pass: a Box::new, Rc::new or small vec! whose allocation never leaves its function lives in the function's frame instead, only in programs whose allocator is std's, capped per value and per frame, and never in a frame that can recur.
date: 2026-11-02
---
# HeapToStack: allocations that never leave their function

Lane escape. The pass is
[`crates/hir-ty-mir/src/mir/optimize/escape.rs`](../../../../crates/hir-ty-mir/src/mir/optimize/escape.rs);
its module doc is the reference for each shape it takes and why. This page is what the pass
relies on, what it does not do yet, and how to measure it.

## What it moves

At MIR levels 1 and 2 (`MirOptions::heap_to_stack`), before the UB checks and before
`BoxInPlace`. A value it moves is read through no pointer, and the alignment and null checks of
a box's pointer would name the box, which the pass then refuses. A zero-sized `Box` or `vec!`
stays where it is: it allocates nothing already, and its pointer, the alignment, is one the
program can read.

It moves:

- `Box::new(value)` into a local used only through `*b`, dropped and freed: the local becomes
  the value, and the free goes.
- `Rc::new(value)` into a local only dereferenced (`Deref::deref` of a borrow read once) and
  dropped: never cloned, counted, downgraded or moved, so its count is one when it drops. The
  local becomes the value.
- `vec![a, b, ..]` of elements with nothing to drop, std's chain
  `box_assume_init_into_vec_unsafe(write_box_via_move(Box::new_uninit(), [..]))`, into a `Vec`
  only borrowed for `Deref`, `DerefMut`, `Index`, `IndexMut`, `IntoIterator` of `&Vec` and
  `&mut Vec`, `len`, `is_empty`, `capacity`, `as_slice` and `as_mut_slice`, and dropped. None
  of those can grow it, reallocate it or hand out the `Vec` itself (`as_ptr` could, through
  `ptr::read` of the `Vec`), so its capacity stays the array's. `Box::new_uninit()` becomes a
  pointer to a slot in the frame, and the rest of the chain runs as before.

Every candidate it does not move is logged, with the reason, under the `heap_to_stack` span
(target `blaze`).

## What it relies on

`GlobalAlloc`'s contract at the pinned nightly (`core/src/alloc/global.rs`): "you cannot rely
on global allocations actually happening ... Whether allocations happen or not is not part of
the program behavior, even if it could be detected via an allocator that tracks allocations by
printing or otherwise having side effects." rustc's LLVM already removes allocations it can
see through, even under a counting `#[global_allocator]`.

blaze holds itself to more than that. `heap_to_stack` is set only for the units of a program
whose global allocator is std's (no `#[global_allocator]`, or one that is
`std::alloc::System`), as `hir-ty-codegen`'s `allocations_unobserved` decides. It is never set
for a library unit, which serves programs with different allocators, and never for the
library's own bodies, which the pass also skips by crate. It is never set for a body built into
another: the inliner's callees and the code generator's always-inlined bodies use
`Build::built_in`. A body that keeps a moved allocation records its bytes in
`Inlined::moved_to_frame`. The code generator then makes it a function LLVM never imports
(`v2/release.rs`: not importable, summary hint `Never`), because ThinLTO import plus LLVM's
inliner would put the slot in the importer's frame, and that frame may recur. The field exists
only in optimized MIR: the MIR store refuses a body with it set and reads it back as 0, as it
does `specialized`. So a program that counts its allocations with its own allocator, and reads
the count, keeps every allocation. So does one whose allocator's static `allocations_unobserved`
can't tell (its `__blaze_global_alloc` names no static or more than one): that is `false`,
logged under target `blaze` with why, not an error. Two `#[global_allocator]`s are an error
naming both crates in order of name.

Keys: `Spec::heap_to_stack` is part of each function's settings, but `Policy::choose` sets it
only for functions that are not std's. std's bodies never move an allocation, so one stored
copy of each serves programs with and without a counting allocator. A member's or a
dependency's function still has two keys, one per setting, even when its body moves nothing.
Keying on whether the optimized body moved something would need that body before the store is
asked: the settings key is made before any MIR, and a non-generic function's code key is its
source's.

What it cannot see:
- A C `malloc` put in place under `System` (`LD_PRELOAD`, an unprefixed jemalloc) whose
  statistics the program reads. The contract already lets LLVM remove those calls.
- Code the program registered with std that std runs from inside a moved frame: a panic hook,
  the allocation-error hook, `atexit` handlers, signal handlers. Each is one more frame level
  per panic, failed allocation, exit or signal.

What stays the same: the value, the operand it is built from, and what drops when and in what
order, unwinding included. A drop of the box or `Rc` drops the value, as it did before the
free. The constructors' own unwind edges go, because their one way to unwind was a failed
allocation.

## Stack

- At most 512 bytes per value (`HEAP_TO_STACK_MAX`) and an alignment of at most 16.
- At most 2 KiB per body over every value it moves (`HEAP_TO_STACK_FRAME_MAX`), taken in the
  order of their first constructor's block.
- One slot per allocation site, which a loop reuses.

Never across recursion: the pass moves nothing in a body that could call itself again. Every
call in such a body is one of these:
- a constructor;
- one of the calls the pass rewrites;
- a library function whose generic arguments and argument types are closed. A closed type is
  made only of the library's types: no parameter, closure, `dyn`, function pointer, program
  function or program type.

By the orphan rule, such a call can reach only the library's impls. Every value such a body
drops is one whose drop runs only the library's code, and the body has no `asm!` and no tail
call. So a moved frame is the deepest of the program's frames on its thread: a thread holds at
most one of them, plus the hook and signal levels above.

Limitation: `println!` in such a body makes it open, because `fmt::Arguments` holds formatter
function pointers that can reach the program's `Display` impls. So a function that prints keeps
its allocations.

## Not done yet

- `String` (`String::from`, `format!`), `Vec::with_capacity(N)` with a constant `N` and pushes
  bounded by it, and `vec![x; N]`.
- `Arc`: its drop's `Acquire` fence is a synchronization another thread can see. It needs a
  proof that the `Arc` was never shared before it can go.
- A call-graph SCC test in place of the closed-body rule, so that a body calling the program's
  own non-recursive functions can move its allocations.
- Inlining: a body that `Inline` takes into its caller allocates again there, since its slot
  would be in a frame that can recur. A body that keeps its moves is never imported by LLVM,
  so LLVM can't inline it across units either; that can cost a small hot function the inlining
  it had. Moving allocations after `Inline`, where the caller is checked, would keep both.
- `Vec`s of elements with drop glue: the elements' drop has to stay while the free goes.

## Measure

The bench is `harness/escape`, its own workspace. In an ix VM, on the exact tree measured:

```
ix new --name escape --cpus 8
ix shell escape -- sh -c 'cd blaze-v2/harness/escape && blaze build --release'
ix shell escape -- hyperfine -w 3 \
  'blaze-v2/harness/escape/target/release/alloc_loop 10000000' \
  'blaze-v2/harness/escape/target/release/alloc_loop_counted 10000000'
ix shell escape -- sh -c 'cd blaze-v2/harness/escape && cargo +nightly-2026-10-27 build --release --target-dir target/rustc'
ix shell escape -- hyperfine -w 3 'blaze-v2/harness/escape/target/rustc/release/alloc_loop 10000000'
```

`alloc_loop` and `alloc_loop_counted` print the same `result`. `alloc_loop_counted` prints
`allocations 30000000`, three per step, under blaze; rustc at opt-level 0 makes the same count.
The time saved is `alloc_loop` against `alloc_loop_counted` under blaze. The pass's own record
is in the `heap_to_stack` spans of `<BLAZE_HOME>/v2/profile/*.spans.parquet`: `step` should
show three `moved` lines.
