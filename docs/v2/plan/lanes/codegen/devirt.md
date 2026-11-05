---
status: plan
summary: Whole-program devirtualization. A final binary's walk lists every vtable the program makes, and the program's facts are closed per object type. A MIR pass then turns `dyn` calls into direct calls (one type) or vtable compares (two to four), keyed by the facts each body read. A second walk lists the new direct callees. Covers what makes a set closed, the guarantees relied on, vtable identity, and the PGO producer still to build.
date: 2026-11-02
---
# Whole-program devirtualization (devirt)

blaze compiles the whole program, the standard library included. For a final binary it can
therefore know every concrete type that is ever unsized to a given `dyn Trait`. A call through
such a `dyn` with one possible type becomes a direct call, which the inliner can then inline.
With two to four types it becomes a compare of the vtable pointer against each type's vtable,
each match a direct call.

## Pieces

- **Facts, in the walk** (`crates/hir-ty-codegen/v2/refs.rs`, `crates/hir-ty-codegen/v2/devirt.rs`).
  Every walked body records three things beside its references:
  - Each vtable its IR makes. This is every `DataRequest::Vtable` the recorder sees, and the
    IR builder's `FunctionCx::vtable` is the only maker of vtables.
  - Each upcast between principals (`unsize_metadata`, dyn to dyn).
  - Each object type a signature at the program's edge carries: the function's own when its ABI
    is not Rust's, and every foreign function it calls.

  Statics add the vtables of their values (`compiled_static`).
- **The world** (`crates/hir-ty/src/mir/devirt.rs`). Each object type is keyed as codegen keys
  a vtable: the principal trait reference with regions erased and projections normalized,
  without Self (`object_key`). For each it holds the target types by content ID in order, an
  optional open reason, and a digest. The world's ID is the hash of the sorted digests. The
  code generator `give`s it, and the pass reads it by ID (`MirOptions::world`).
- **The pass** (`crates/hir-ty-mir/src/mir/optimize/devirt.rs`). It runs at MIR level 2 with
  the inliner on, before `Inline` and again after it. The run after is what catches a
  `Box<dyn Fn>` call, which only becomes a `dyn` call once alloc's `<Box<F> as Fn>::call` is
  inlined. The outcome depends on the object:
  - Closed with one type: an unconditional direct call.
  - Closed with two to four types: a compare per type, each a direct call, and the `dyn` call
    kept last (see vtable identity below).
  - Open, with a profile: the hottest type is compared first and the `dyn` call kept.
  - Anything else stays virtual. A `kept` event under the `devirtualize` span says why.

  A direct call names the trait's method on the type, which the code generator resolves in
  the body's crate. A type whose impl that crate can't see (a member's logger behind log's
  `&dyn Log`: impls are looked up in the crates a crate depends on) is left out of the
  compares and reaches the `dyn` call kept last. So does a type whose method is
  `#[track_caller]` while the trait's declaration is not (a `dyn` call reports the
  declaration's location, a direct one the call site's; `Specialize` refuses the same), and
  one whose method a downstream impl may specialize (a `default fn`, or an item of a `default
  impl`). So does a type that names a crate the body's crate does not load, even where a
  blanket impl or the compiler gives the method (hashbrown's `rehash_in_place` calling its
  `&dyn Fn` hasher, a closure over a member's key type): the direct callee is an instance of
  that type worked out in the body's crate (`home::env_of`: no crate that loads the type's
  crates stands for an upstream asker), where the type's own impls are not seen
  (`home::unloaded_crate`). A closed object with one such type is kept (`seen`). Resolving an unseen method in
  the crate that made the vtable, so that it becomes a direct call too, is open.

  Each site records the digest it read in `Inlined::devirtualized`, or `ABSENT` when the
  program makes no vtable of it.
- **Two walks** (`crates/blaze-codegen/src/driver.rs`, `Frontend::closed`). The first walk is
  without facts. When it ends with no error, the front end builds the world from everything
  it reached. If some reached body has a site (`has_sites`), the driver walks again. In that
  second walk, the bodies with sites are walked and described against the world, so their new
  direct callees are listed. A second walk that finds other facts is an error naming the
  program.
- **Units** (`crates/hir-ty-codegen/v2/backend.rs`). A unit builds an instance's body against
  the world exactly when its references were walked so (`Walking`). Callees built in by the IR
  builder and by the MIR inliner are never devirtualized (`callee_options`, `built_in_body`).
  Their world-free walk lists them.

## What makes a set closed

An object's list is closed only when all of these hold:

- The program is an executable or a test. In a `cdylib` every object is open, because a
  program that loads it may make objects of its traits.
- No code outside the library's crates names a dynamic loader (`dlopen`, `dlmopen`,
  `LoadLibrary*`). If any does, every object is open.
- No foreign function the program calls, and no non-Rust-ABI function it defines, carries the
  object type in its signature.
- Every place that makes a vtable is visited. Every reached instance (generic instances and
  std included) and every reached static is walked by the IR builder's own resolution, and
  that resolution is the only thing that makes vtables.

An upcast `dyn Sub` to `dyn Super` hands Super's list Sub's types and Sub's openness,
propagated to a bounded fixpoint. The supertrait's pointer may be Sub's vtable
(`Upcast::Same`), so the compare against Super's vtable can miss. That only costs the slow
path.

A vtable that blaze did not make is not callable by blaze code anyway: vtable layout is
unspecified, and blaze lays its own out. So the only outside source of a callable vtable is a
library blaze built and loaded into the program. **Known gap:** a program linked at load time
against a `cdylib` blaze built, handed a `Box<dyn T>` through a `*mut c_void`, is not
detected. The fix: blaze-cargo tells the front end when a unit links a library this build
makes (a build script's `rustc-link-lib=dylib=` naming one), and that opens every object.

## Behaviour identical to rustc

- A trait object's metadata must be a valid vtable for its type (Rust Reference, behaviour
  considered undefined: invalid wide-pointer metadata). A `dyn T` whose data is of a type
  whose vtable the program never made therefore cannot exist in a program without UB.
- Pointer casts between trait objects need equal principals (rustc PR #120248, 1.81). Without
  that rule, a cast could put a vtable for one trait under another.
- The pass checks each target against the call's own projection bounds and the method's where
  clauses, and refuses methods with where clauses of their own (`bounds_hold`).
- Calls go to the same method instance the vtable slot points to: the receiver is
  `&`/`&mut` to the dyn, and the data pointer is passed as `&T`.

## Vtable identity

A compare is only a fast path. In a staged image there is one vtable object per content
(`crates/blaze-link/src/stage.rs`, data merged by content). That does not hold in a lazy run,
nor where per-origin settings key one method twice. So the `dyn` call stays as the last arm
even for a closed set, and in a staged image it is never taken. Making vtables unique
everywhere (one key per `(ty, trait_ref)` in places, or the image's symbol preferred in the
lazy loader) would let a closed two-to-four set drop it.

## Keys (rule 18)

- `MirOptions::world` is in the body's query key and in the identity hash
  (`crates/hir-ty-identity/src/lib.rs`). Library bodies never take a world, so they stay one
  for every program, and neither do the dependency image's (`Owner::Dependency`): built
  against the world, a dependency's generic (hashbrown's `resize_inner::<Global>`) calls the
  program's one closure directly, which is the workspace's when it names a member's type, and
  the image would change with workspace code. One rule says which body takes the world,
  `devirt::takes_world`: both a body's walk and whether a program builds a world at all read
  it, so a program whose only `dyn` sites are in the image's code builds none.
- Only a build that links a dependency image splits its code: a dev executable over the
  daemon's code memory, as blaze-cargo's `links_image` decides once for both the front end
  (`HirFrontend::with_image`) and the link. A build that links none (a file: an executable, a
  test binary, a release build) makes every crate but the library's the workspace's, so every
  such body may take the world and the whole program is devirtualized as before the split.
- A site's digest is the object's when the body's crate saw every target it looked up (a
  closed object past `MAX_TARGETS`, or an open one with no profile counts, looks up none) and
  each met the call's bounds; otherwise it also hashes what the body found of each target
  (`devirt::site_digest`): out of the bounds, seen, or unseen (its method not resolvable in
  the body's crate, `#[track_caller]` where the trait's declaration is not, or specializable).
  What the body's crate sees depends on its dependencies, which its code hash leaves out, so
  a feature or lockfile change that gives it another crate rekeys the body.
- A body lists each digest it read once, in the order first read (`Inlined::read_devirt`):
  the pass runs before and after `Inline` and looks again at a call it kept, which reads the
  same facts again. An instance's code hash folds in the sorted digests its body read
  (`devirt::code`). A body
  with no site is keyed exactly as without devirtualization.
- A world is given process-wide by its ID (`devirt::give`), which is of the facts' content,
  while the world holds one database's interned rows. A world of the same ID with other rows
  (another database that interned the same program otherwise) is refused, naming the program
  and the object that differs, never handed the first one's rows.
- The walk runs with debug assertions and unwinding on, so the walk's body has at least the
  unit's sites. A unit that asks for a callee the walk did not list is the `unwalked` error.

## Profile-guided ordering

`Target::count` orders the compares, hottest first, and lets the last run speculate the
hottest type of an open object: only counted types are looked up, hottest first, up to the
first whose method the body's crate can name, so an open object with no counts costs no
lookup and keeps the object's digest. Both are in the digest. Nothing fills the counts yet. The
producer is to read the entry counts of each target's method (`<T as Trait>::m`) from the
`.profdata` of `Pgo::Use`, by its `blaze.pgo.` record name. Its tests are a profile that
reorders two types, and an open object speculated.

## Cost

The second walk describes every instance again, but only for a final binary in which some
reached body that takes the world has a site, and every walk without the world is a memo hit.
In a build that links no image every non-library body with a site is keyed per program, as
before the split. A build that links one costs nothing today: it is a live dev build, which
never inlines MIR (`mir_inline` is off for live code, `plan.rs`), so no body of it takes the
world at all, and an optimized live run is refused (`blaze_lazy::mode::allow`). The owner
test in `takes_world` is the guard for when live builds inline: then a dependency's instance
with a site is keyed per program when the workspace owns it (one of its type arguments is a
member's, or its crate reaches a local crate through its non-proc-macro dependencies,
`reaches_local`: a registry crate over a path or `[patch]`ed crate, whose non-generic
functions and statics are the workspace's too), and every other is the image's, shared and
never devirtualized.

## Tests and benchmark

- `crates/hir-ty-mir/src/mir/optimize/devirt/fixtures.rs` covers: one type becomes direct;
  three types are compared with the `dyn` call last; an open object stays virtual with its
  reason under the span; an absent object is read as `ABSENT`; the same bytes come out twice;
  a type of a crate the body's crate does not depend on stays virtual, even with a blanket
  impl's method the body's crate finds, and one beside a seen
  type is left to the `dyn` call after the seen one's compare, each with a site digest of its
  own; a `#[track_caller]` method behind a declaration without it stays virtual, naming the
  rule. What such a call reports when run (rustc: the method's own definition, through its
  `ReifyShim`) is an ignored fixture until the MIR evaluator models the shim: it passes the
  `dyn` call's location today. Each fixture's world IDs are of its text and names, as tests run in
  parallel with a database each and worlds are shared by ID.
- `tools/devirt/bench.py` over `tools/devirt/dyn_loop` covers a one-type `dyn`, a three-type
  `dyn` and a `Box<dyn Fn>` in hot loops. It times the merge base's blaze against this
  change's, on one tree, and checks that the outputs are equal and the binaries differ.
