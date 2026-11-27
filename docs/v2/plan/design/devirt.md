---
status: plan
summary: Whole-program devirtualization. A final binary's program lists every vtable it makes, so the set of types behind each object type is closed; a `dyn` call with one possible type becomes a direct call, with two to four a vtable compare per type, keyed by the facts each body read. What makes a set closed, the guarantees relied on, vtable identity and keys. Built - the profile-guided half (speculative devirtualization of hot indirect calls in crates/blaze-lir). Not built - the closed-world half.
date: 2026-11-02
---
# Whole-program devirtualization (devirt)

blaze compiles the whole program, the standard library included. For a final binary it can
therefore know every concrete type that is ever unsized to a given `dyn Trait`. A call through
such a `dyn` with one possible type becomes a direct call, which the inliner can then inline.
With two to four types it becomes a compare of the vtable pointer against each type's vtable,
each match a direct call.

## Built: the profile-guided half

`speculative_devirt` (`crates/blaze-lir/src/simplify/devirt.rs`) rewrites an indirect call
(`Op::CallIndirect`, a `dyn` method or a function pointer) whose profile (`BPG1`) shows one
dominant target (at least `min_devirt_hit_percent`, default 80) into a guarded direct call with
the indirect call kept on the cold path. The whole-module LTO pass (`crates/blaze-lir/src/lto.rs`)
runs it before its call-graph condensation, so the new direct edges are inlined bottom-up.

## Not built: the closed-world half

- **Facts, in the walk.** Every walked body records each vtable its code makes (the one maker
  of vtables), each upcast between principals (dyn to dyn), and each object type a signature at
  the program's edge carries: the function's own when its ABI is not Rust's, and every foreign
  function it calls. Statics add the vtables of their values.
- **The world.** Each object type is keyed as code generation keys a vtable: the principal trait
  reference with regions erased and projections normalized, without Self. For each it holds the
  target types by content id in order, an optional open reason, and a digest. The world's id is
  the hash of the sorted digests.
- **The pass**, before and after inlining (the run after catches a `Box<dyn Fn>` call, which is
  a `dyn` call only once `<Box<F> as Fn>::call` is inlined): closed with one type, a direct call;
  closed with two to four, a compare per type, each a direct call, with the `dyn` call kept last;
  open with a profile, the hottest type compared first (the built half); anything else stays
  virtual, with a `kept` event saying why. A type whose impl the body's crate cannot see, a
  `#[track_caller]` method behind a declaration without it, and a method a downstream impl may
  specialize are left to the `dyn` call.
- **Two walks.** The first walk is without facts; when it ends with no error the world is built
  from everything it reached, and the bodies with sites are walked again against it so their new
  direct callees are listed. A second walk that finds other facts is an error naming the program.

## What makes a set closed

An object's list is closed only when all of these hold:

- The program is an executable or a test. In a `cdylib` every object is open, because a
  program that loads it may make objects of its traits.
- No code outside the library's crates names a dynamic loader (`dlopen`, `dlmopen`,
  `LoadLibrary*`). If any does, every object is open.
- No foreign function the program calls, and no non-Rust-ABI function it defines, carries the
  object type in its signature.
- Every place that makes a vtable is visited: every reached instance (generic instances and std
  included) and every reached static.

An upcast `dyn Sub` to `dyn Super` hands Super's list Sub's types and Sub's openness,
propagated to a bounded fixpoint. The supertrait's pointer may be Sub's vtable, so the compare
against Super's vtable can miss. That only costs the slow path.

A vtable that blaze did not make is not callable by blaze code anyway: vtable layout is
unspecified, and blaze lays its own out. So the only outside source of a callable vtable is a
library blaze built and loaded into the program. **Known gap:** a program linked at load time
against a `cdylib` blaze built, handed a `Box<dyn T>` through a `*mut c_void`. The fix: a unit
that links a library this build makes (a build script's `rustc-link-lib=dylib=` naming one)
opens every object.

## Behaviour identical to rustc

- A trait object's metadata must be a valid vtable for its type (Rust Reference, behaviour
  considered undefined: invalid wide-pointer metadata). A `dyn T` whose data is of a type
  whose vtable the program never made therefore cannot exist in a program without UB.
- Pointer casts between trait objects need equal principals (rustc PR #120248, 1.81). Without
  that rule, a cast could put a vtable for one trait under another.
- The pass checks each target against the call's own projection bounds and the method's where
  clauses, and refuses methods with where clauses of their own.
- Calls go to the same method instance the vtable slot points to: the receiver is
  `&`/`&mut` to the dyn, and the data pointer is passed as `&T`.

## Vtable identity

A compare is only a fast path. In an exported image there is one vtable object per content.
That does not hold for code placed while a program runs, nor where per-origin settings key one
method twice. So the `dyn` call stays as the last arm even for a closed set, and in an exported
image it is never taken. Making vtables unique everywhere would let a closed two-to-four set
drop it.

## Keys (rule 18)

- The world is an input of the key of every body that takes it, and of nothing else: library
  bodies never take a world, so they stay one for every program. One rule says which body takes
  the world; a program whose only `dyn` sites are in bodies that do not take it builds none.
- A site's digest is the object's when the body's crate saw every target it looked up and each
  met the call's bounds; otherwise it also hashes what the body found of each target (out of the
  bounds, seen, or unseen). What the body's crate sees depends on its dependencies, so a feature
  or lockfile change that gives it another crate rekeys the body.
- A body lists each digest it read once, in the order first read; its code key folds in the
  sorted digests. A body with no site is keyed exactly as without devirtualization.
- A world given by an id with other rows than the one already given is refused, naming the
  program and the object that differs.

## Tests and benchmark

- Fixtures: one type becomes direct; three types are compared with the `dyn` call last; an open
  object stays virtual with its reason; an absent object is read as absent; the same bytes come
  out twice; a type of a crate the body's crate does not depend on stays virtual; a
  `#[track_caller]` method behind a declaration without it stays virtual, naming the rule.
- A benchmark of a one-type `dyn`, a three-type `dyn` and a `Box<dyn Fn>` in hot loops, before
  and after, on one tree, checking that the outputs are equal and the binaries differ.
