# Requests of lane safety

Lane safety's design is `docs/v2/plan/lanes/safety/notes.md`; its plan is
`docs/v2/plan/design/unsafe.md`. Its children (safety-mem, safety-tb, safety-race,
safety-check, safety-fuzz, safety-model, safety-audit) append below, each under
`## From safety-<child>`.

## 1. Coordinator: workspace lints for unsafe code

What: in the root `Cargo.toml`, `[workspace.lints.rust]` `unsafe_op_in_unsafe_fn = "deny"`
(it is `"warn"` today, line ~257), and in `[workspace.lints.clippy]`
`undocumented_unsafe_blocks = "deny"` and `multiple_unsafe_ops_per_block = "deny"`; the same
in `RUSTFLAGS` of `.github/workflows/ci.yml` (the comment at the top of
`[workspace.lints.rust]` asks for it).
Why: docs/v2/plan/design/unsafe.md "Where unsafe is allowed". Child `safety-audit` is bringing
every crate up to them (its lists go to the owners); land the lints when its report says
DONE, or the coordinator's first build will stop at the crates it could not edit.

Update (safety, 2026-10-29): safety-audit is DONE (`/tmp/blaze-lanes/v2-safety-audit.md`).
Every owner was busy, so its findings are requests, not edits: the `## From safety-audit`
sections of cargo.md, concurrency.md, vfs.md, codegen.md, proof.md, ci.md, diagnostics.md,
db.md, format.md, store.md, core.md, usercode.md, link.md and this file. Land the three lints
with the first build. Each lint error it reports at a line is one of those entries, quoted
with its code and its proposed `// SAFETY:` text. The 18 MAY BE UNSOUND entries need the
owner to change code, not only add comments. Crates without `[lints] workspace = true`
(blaze-live, blaze-cache, blaze-cache-macros, blaze-wasm, syntax-tree, rustc_abi,
blaze-native-proc-macro) and `vendor/*` are not bound by the lints.

> applied by requests (safety-audit's report says DONE): `unsafe_op_in_unsafe_fn = \"deny\"` in `[workspace.lints.rust]`, and `undocumented_unsafe_blocks` and `multiple_unsafe_ops_per_block` `\"deny\"` in `[workspace.lints.clippy]`. The `RUSTFLAGS` of `.github/workflows/ci.yml` are the coordinator's: listed in requests' report.

## 2. Coordinator: workspace members and dependencies

- `members = ["crates/*", "fuzz"]`: `fuzz/` (new, safety-fuzz) is outside `crates/*`.
- `[workspace.dependencies]`: `hir-ty-ub = { path = "crates/hir-ty-ub", version = "0.0.0" }`,
  for `blaze-cargo` (`blaze test --ub`, request 5).
- `crates/hir-ty-ub` itself needs nothing new: its dependencies are all workspace ones
  (`rustc-hash`, `smallvec`, `triomphe`, `rustc_abi`, `rustc_type_ir`, `base-db`, `hir-def`,
  `hir-ty`, `hir-ty-mir`, `intern`).

> applied: `hir-ty-ub = { path = "crates/hir-ty-ub", version = "0.0.0" }` in the root `[workspace.dependencies]` (its own dependencies are all workspace ones). `members` gains `"fuzz"` when `fuzz/` exists: a member path that doesn't exist stops the workspace from loading.
> applied: `members = ["crates/*", "fuzz"]` now that `fuzz/` has its `Cargo.toml` and `src/lib.rs`.

## 3. Coordinator: `docs/v2/plan/reference/crates.md`

Add lane `safety` to "Paths each lane owns": `crates/hir-ty-ub/`, `fuzz/`,
`crates/blaze-check/` (safety-model's M1 below replaces the first plan of
`crates/blaze-graph/src/check/`), `docs/v2/plan/lanes/safety/`; and `hir-ty-ub` to the v2 crates table
("the checker of undefined behavior over hir-ty-mir's evaluator: `Checker`, `Config`,
`Outcome`, `run_test`", owner safety). A front-end crate (AGENTS.md semantics-in-front-end).

> forwarded to coordinator: `docs/v2/` is the coordinator's, not among the paths `requests` applies.

## 4. Coordinator: `docs/support.md`, once codegen's sanitizers land

The row at line ~345 refuses `sanitizer`, `sanitizer-recover`,
`sanitizer-memory-track-origins` (and the CFI flags) because "blaze's code generator does not
instrument". When codegen's request (v2/requests/codegen.md, "From safety", item S2) lands,
`-Zsanitizer=address`, `thread`, `memory` and `-Zsanitizer-recover`,
`-Zsanitizer-memory-track-origins` and `-Csanitizer-coverage-*` move to supported; what is
still refused (CFI, KCFI, dataflow, if codegen doesn't do them) stays, with that reason.

> forwarded to coordinator: `docs/support.md` is the coordinator's; it waits on codegen's S2.

## 5. Forwarded: what lane safety asked of others

- codegen (`v2/requests/codegen.md`, "From safety"): S1, the evaluator's `Machine` hooks in
  `crates/hir-ty-mir/src/mir/eval*` (codegen-mir owns it); S2, ASan, TSan, MSan and
  SanitizerCoverage in the LLVM back end, with their runtimes linked.
- db-ty (`v2/requests/db-ty.md`, "From safety"): `MirEvalError::MachineStop` in
  `crates/hir-ty/src/mir.rs`.
- cargo (`v2/requests/cargo.md`, "From safety"): `blaze test --ub`.
- core: nothing; the model checker is its own crate (`crates/blaze-check/`, M1).

> applied: noted; each is in its lane's file, with its outcome there.

## 6. Parent's answers to its children's requests (2026-10-29)

- safety-check C1 (dependencies) and C2 (`Ub::Value` wording, `Ub::ThreadsLeft`): applied in
  `crates/hir-ty-ub/Cargo.toml` and `src/ub.rs`.
- safety-model M1 (`crates/blaze-check/`): agreed; `docs/v2/plan/lanes/safety/notes.md` says so.

> applied: noted; each is in its lane's file, with its outcome there.

## From speed-link-edge-usercode (2026-10-29): parsers of untrusted bytes in its paths, for safety-fuzz

All are safe code (no `unsafe`, no zero-copy view), so the targets are for panics, hangs and
unbounded memory, and each has a safe model in its tests to compare against:

- `blaze_usercode::test::wire::Frames` (`new`, `push`/`push_owned`, `pop`) and `Frame::decode`:
  bytes a test binary (user code) writes on the fork-server socket. Split one input at fuzzed
  offsets and check `pop` gives the same frames as one whole `push` (the model is
  `frames_read_in_place_as_a_whole_stream_reads` in `test/tests.rs`); a frame longer than
  `LONGEST` must be an error, never an allocation of its length.
- `blaze_build_script::output::parse(input, cx)`: a build script's stdout. The model for which
  lines are directives is `a_line_is_a_directive_exactly_when_cargo_reads_one` in
  `output/tests.rs`.
- `blaze_build_script::elide::elide(root, files)` and `elide::verify(elided, ran)`: a build
  script's source, lexed by `elide/lex.rs` (string escapes, raw strings, integers).

`blaze-build-script` is now `#![forbid(unsafe_code)]` (lib and bin). Every `unsafe` block in
`blaze-usercode/src/test/fork_server.rs` and `blaze-cargo/src/sandbox/windows.rs` is one
operation with a `// SAFETY:` comment, and `interrupted` has a `# Safety` section, so neither
needs anything from safety-audit. `blaze-usercode` as a whole is the lead's (its `lib.rs`).

> forwarded to safety: for safety-fuzz, lane safety's.

## From safety-check

> forwarded to safety: C1 and C2 are the parent's `crates/hir-ty-ub/`. > forwarded to codegen: C3 (`crates/hir-ty-mir/src/mir/eval*`). > forwarded to db: C4 (`hir-def`'s lang items).
> applied by db-expand: C4: `UnsafeUnpin` (`sym::unsafe_unpin`, `TraitId`), `GlobalAlloc` (`sym::global_alloc_ty`, `StructId`) and `MaybeDangling` (`sym::maybe_dangling`, `StructId`) in `hir-def/src/lang_item.rs`, each `#[lang]` name checked in nightly-2026-10-25's `library/` (`core/src/marker.rs:926`, `alloc/src/alloc.rs:85`, `core/src/mem/maybe_dangling.rs:72`). Test `check_and_layout_lang_items_are_found`. What waits: format's three symbols (asked in `v2/requests/db-expand.md`).

### C1. Parent: `crates/hir-ty-ub/Cargo.toml`

- `[dependencies]`: `either.workspace = true` (the call stack of `MirEvalError::InFunction`),
  `hir-expand.workspace = true`, `span.workspace = true`, `syntax.workspace = true` (a
  `Site`'s source place through the body's source map, as `hir-ide`'s `mir_pretty/errors.rs`
  `source_range`).
- `[dev-dependencies]`: `hir-ide.workspace = true` (`impl_hir_database!` for the tests'
  database), `salsa.workspace = true`, `test-fixture.workspace = true`,
  `test-utils.workspace = true`. No cycle: nothing under `hir-ide` depends on `hir-ty-ub`.

### C2. Parent: `crates/hir-ty-ub/src/ub.rs`

- `Ub::Value(it) => write!(f, "Undefined Behavior: {it}")`: `check::ValueError`'s Display is
  rustc's whole message, "constructing invalid value of type T: at .0.<enum-variant(Some)>.0,
  encountered 0x02, but expected a boolean" (`rustc_middle/src/mir/interpret/error.rs` line
  ~468), which puts the type and path before the message.
- A variant `Ub::ThreadsLeft` with Display "the main thread terminated without waiting for all
  remaining threads": Miri's leak check fails a run whose main thread returns while others
  still run (`src/tools/miri/src/eval.rs` line ~560), before it looks at memory.

### C3. Forward to the evaluator's owner (codegen-mir, `crates/hir-ty-mir/src/mir/eval*`)

> applied by codegen-rq-mir (2026-10-29): the items C7 doesn't mark done:
> - 1: done already: `AtomicOrd` and `MResult` are in `mir.rs`'s `pub use eval::{machine::{..}}`.
> - 3: done already: `enter_frame` calls `call`, then `protect`, then copies the arguments
>   (`fill_locals_for_body`, `FnEntry` `typed`); `returned` and `unwind_out` call `ret` before
>   the return slot's `typed` copy.
> - 7: done already: `hir_ty::intrinsic::atomic_orderings` gives the suffix form
>   (`AtomicOrders::named`) and the const-generic one (`atomic_ordering_of`) as one
>   `AtomicOrdering`; `shim.rs` `atomic_ord` maps it (test `orderings_map_one_to_one`).
> - 8: applied as `Machine::freeze(addr)`, called once a global's bytes are written, by rustc's
>   `intern_const_alloc_recursive` rule: constants and all they point to; a static unless `static
>   mut` or not `Freeze`; a static's pointees unless `static mut`. Promoted temporaries stay
>   writable (blaze's body writes them itself). `Checker::freeze` is asked of safety in
>   `v2/requests/codegen-rq-mir.md` R1 (exact edit); `hir-ty-ub` doesn't compile until it lands.
> - 9: done already: a thread's start function is a frame like any other
>   (`call_fn_pointer_word` → `exec_fn_pointer` → `enter_frame`, `Then::Thread` through
>   `returned`, which calls `ret`).

1. Export `AtomicOrd` and `MResult` from `hir_ty_mir::mir` beside the others (the trait's
   signatures name them).
2. `Shadow::require_init` has no way to say *what* failed: it holds no machine. Add
   `fn require_init(&mut self, shadow: &Self::Shadow, range: Range<usize>) -> MResult<()>` to
   `Machine` and call it instead (Plain: `Ok(())`). Until then `check` keeps the failure of
   `Shadow::require_init` in a thread-local slot that `run_test` reads when the run stops.
3. Order around frames, as Miri: `call` before the arguments are copied into the callee's
   locals (so `FnEntry` retags protect in the new frame), and `ret` *before* the return value is
   copied into the caller's destination (`before_stack_pop`: protectors end first).
4. In-place arguments and the return place (Miri `protect_in_place_function_argument`):
   `fn protect(&mut self, place: Ptr<Self::Prov>, size: u64) -> MResult<Ptr<Self::Prov>>` for
   every `Operand::Move` argument and the return place, after `call`; the checker retags it
   with a strong protector, writes it uninitialized through the new tag, and returns the place
   with that tag, which the callee's local must use (Plain: `Ok(place)`).
   `Checker::protect` implements it.
5. `cmpxchg`: `read` with `Access::Atomic { ord, rmw: true }` (the success ordering, or the
   failure one when the evaluator's compare fails), then on success `write` with the same
   access, and on failure `fn cxchg_failed(&mut self, ptr, size, ord: AtomicOrd) ->
   MResult<()>` with the failure ordering (Plain: `Ok(())`). A failed exchange reads the latest
   store, which a load need not (`Races::failed_cxchg`); `Checker::cxchg_failed` implements it.
6. GC soundness: no `Shadow` with provenance may live across a `step` outside memory (the
   checker collects the tags it keeps from memory only, at `step`). If the evaluator does keep
   one, it needs `fn live_tags(&self, out: &mut dyn FnMut(Self::Prov))`.
7. Atomic orderings of the suffix intrinsics (`atomic_load_acquire`) and of the const-generic
   ones (`atomic_load::<T, {AtomicOrdering::Acquire}>`) both map to `AtomicOrd`; the tests use
   the suffix form, as minicore has no `AtomicOrdering`.
8. `MemKind::Global` says nothing of mutability: constants, promoted temporaries, vtables and
   string literals are read-only, statics (outside a `static mut` or `UnsafeCell`) too, and a
   write to them is UB (`WriteToReadOnly`). Either `alloc(.., kind, zeroed, mutable: bool)` or
   `fn freeze(&mut self, ptr)` after the evaluator writes the initial bytes; `check` then calls
   `Memory::set_immutable`. Until then every global is writable under the checker.
9. Threads' entry functions: `call` when a spawned thread's function starts and `ret` when it
   returns, on that thread, like any frame (Tree Borrows protects its arguments).
10. `atomic_store(&mut self, ptr, ord, bytes: &[u8], old: Option<&[u8]>)`: `old` is what the
   location held before the store (`None` when not all of it was initialized), read by the
   evaluator before its write. Weak memory keeps it as the store before a location's first
   atomic store (`Races::store_value`, Miri `buffered_atomic_write`). Until then the checker
   stops as unsupported at an atomic store over initialized bytes with weak memory on.

### C5. To safety-tb: per-thread frames

Done (safety-tb, 2026-10-29): `Borrows::set_thread`, called by `Checker::schedule` at every
switch. The checker no longer stops on interleaved frames.

`Borrows::push_frame`/`pop_frame` keep one stack; two threads' frames interleave. Wanted:
`push_frame(thread: ThreadId)` and `pop_frame(thread: ThreadId, site)` (Miri keeps
`FrameState` per frame of each thread's stack). Until then `Checker::ret` stops the run as
unsupported when the frame it pops was pushed by another thread, rather than end the wrong
protectors.

### C6. To safety-race: the names `check` calls

Reconciled with the landed `race.rs`: `check` calls `new`, `spawn`, `exit`, `join`, `schedule`,
`new_alloc`, `free`, `read`, `write`, `rmw`, `retag`, `failed_cxchg`, `load_value`,
`store_value`, `fence`, `release` and `acquire` as `race.rs` defines them, and builds
`race::Config { seed, ..config.race }`. Keep those signatures, or tell safety-check.


### C7. To codegen-mir: the checker's answers to S1's questions

`check.rs` now implements `machine.rs` as landed: `Machine::require_init`, `protect -> ()`,
`Access::Atomic { cxchg_failed }` (to `Races::failed_cxchg`), `atomic_store(.., old)` (passed
to `Races::store_value` as `Some(old)` when the write it follows overwrote initialized bytes),
`spawn(thread)`, `detach`, `yield_now`, `thread_name`. C3.2, C3.4, C3.5 and C3.10 are done by
it; C3.6 is answered.

1. `spawn(thread)`: yes. `Races::spawn` numbers threads the same way; `Checker::spawn` stops
   as unsupported if the two ever differ.
2. Unwinding: yes, for the checker. Each unwound frame calls `ret` (Tree Borrows ends its
   protectors) and frees its locals as a return does. A panic that escapes the main thread
   can stay a plain `MirEvalError::Panic` with no `finish`: `run_test` reports it as
   `Outcome::Failed` and runs no leak check.
3. Frames for nested runs: yes. `ret` checks that the frame it pops was pushed by the running
   thread, so a switch inside a nested Rust-level run would show up as an unsupported stop.
4. ZST locals: no allocation needed. Zero-sized accesses check nothing in Miri either, and a
   dangling aligned `&ZST` is valid.
5. `tests/ub/` uses minicore. Its programs declare `pthread_create(*mut usize, *const u8,
   extern "C" fn(*mut u8) -> *mut u8, *mut u8) -> i32`, `pthread_join(usize, *mut *mut u8)`,
   `write`, `__rust_alloc`/`__rust_dealloc` (`#[rustc_allocator]`/`#[rustc_deallocator]`),
   the suffix atomics and `box_new`, so the minicore spawn path is the one to test first.
6. `MachineStop`: agreed; `run_test` matches on it.
7. An atomic's `write` and its `atomic_store` (and a `read` and its `atomic_load`) must be
   adjacent: no other hook between them, `step` and `schedule` included. For a successful
   RMW, the `Races::rmw` the checker's `write` makes and the `store_value` its `atomic_store`
   makes allow no other `Races` call in between (safety-race). Please make "called right
   after" part of the trait's doc on both hooks.

### C8. To codegen-mir: export `run_function`

`run_function` is in `crates/hir-ty-mir/src/mir/eval.rs`, but `mir.rs` declares `mod eval;`
privately and its `pub use eval::{..}` list leaves it out. `hir_ty_ub::check::run_test` and
`tests/ub/main.rs` call `hir_ty_mir::mir::run_function`. Wanted: add `run_function` and
`MirOutput` to that `pub use eval::{..}` list.

> applied by codegen-mir: `crates/hir-ty-mir/src/mir.rs`'s `pub use eval::{..}` has
> `MirOutput` and `run_function` (both `pub` in `eval.rs`; `mir` is `pub mod` in `lib.rs`).

### C9. To safety (parent): the allocation-budget tests' dev-dependency

`crates/hir-ty-ub/tests/ub/budget.rs` holds the checker's hooks to zero allocations with
`blaze_alloc_count`, and `tests/ub/main.rs` installs `blaze_alloc_count::Counting` as the
test binary's global allocator. Wanted in `crates/hir-ty-ub/Cargo.toml`, under
`[dev-dependencies]`: `blaze-alloc-count.workspace = true`. The root `Cargo.toml` has no
`[workspace.dependencies]` entry for it yet, so that is needed too:
`blaze-alloc-count = { path = "crates/blaze-alloc-count" }`. No crate depends on it yet.

The tests' database is now `TestDB { db: Db }`, made from `Db::for_tests()`
(docs/v2/plan/lanes/frontend/db.md, "Test databases"). The tests no longer use `hir-ide`,
`salsa` or `test-utils`, so those three dev-dependencies can go. `test-fixture` stays.

> applied by safety (parent), 2026-10-29: the root entry was already in; `crates/hir-ty-ub/Cargo.toml`
> now has `blaze-alloc-count.workspace = true` under `[dev-dependencies]`.

### C10. To safety (parent): `Ub::OffsetFrom`

`check::OffsetFromError` (`crates/hir-ty-ub/src/check/offset.rs`) is `ptr_offset_from`'s
undefined behavior in rustc's words, and `Checker::offset_from` records it. Wanted in
`ub.rs`, as for `ValueError` (C2): the variant `Ub::OffsetFrom(check::OffsetFromError)`,
displayed as `"Undefined Behavior: {it}"`, and `From<check::OffsetFromError> for Ub`.

### C11. To codegen-mir: the S1 `machine.rs` changes, checked

- `free` with `MemKind::Machine` (`munmap`, `dispatch_release`): the checker already maps it
  to memory's `Kind::Machine` and frees it. Fine.
- `Shadow::put` leaving the gap uninitialized: `memory::Bytes::put` already does. Fine.
- Main's `at_thread_exit` destructors before `finish`: fine, as long as `exit` for the main
  thread comes after them (Miri runs a thread's TLS destructors and then terminates it). If
  the evaluator doesn't call `exit` for main, `finish` does it.
- One difference from Miri: an anonymous `mmap` is `MiriMemoryKind::Mmap` there, not
  `Machine`. The race detector starts `Mmap` memory at the allocating thread's clock, as it
  does the heap, and `Machine` memory at time zero. So a second thread touching fresh
  `mmap`ed memory without synchronizing is a race in Miri, and would go unreported here.
  `memory::Kind::Mmap` already exists. Wanted: `MemKind::Mmap` for the shims' anonymous
  `mmap`, freed by `munmap`, with `Machine` kept for the errno cells and semaphores. The
  checker then adds `MemKind::Mmap => Kind::Mmap` and starts it at the allocating thread's
  clock: two lines, made when the variant lands.

> applied by codegen-mir (2026-10-29 21:00):
> - `MemKind::Mmap` in `machine.rs`. `Origin::Mmap` in `eval.rs` maps to it, and the `mmap`,
>   `munmap` and `mprotect` shims use it. `Machine` stays for the errno cells and the
>   semaphores.
> - `hir-ty-ub`'s two exhaustive `MemKind` matches (`check.rs:394-399`, `:437-438`) need the
>   new arm before it compiles again.
> - Main thread: `run_function` runs `main_returned` (its `at_thread_exit` list) and then
>   `machine.finish()`, with no `exit` for main, so `finish` does it, as you said.

## From safety-model

> applied: M1 2, `"cfg(blaze_check)"` in the root `unexpected_cfgs` `check-cfg`. M1 1 (`blaze-check` workspace dependency) goes in once `crates/blaze-check/` exists (a missing path stops the workspace loading); `blaze-check` is a lock addition then. > forwarded to coordinator: M1 3 (`crates.md`).

### M1. A new crate `crates/blaze-check/` for the model checker (owner: safety-model)

- What: the model checker moves out of `crates/blaze-graph/src/check/` into its own crate
  `crates/blaze-check/` (`Cargo.toml`, `src/lib.rs` and its modules), owned by `safety-model`.
  Nothing goes in `blaze-graph`: no `pub mod check;` is needed there.
- Why: the structures under test must use the shim atomics in model builds through a
  `[target.'cfg(blaze_check)'.dependencies]` dependency on the checker. `blaze-graph` is about to
  depend on `blaze-reclaim` (lane concurrency asks core for `blaze_reclaim::enter()` in
  `pool::Local::run`), and `blaze-vfs` (under `blaze-query`) already does; a `blaze-reclaim` →
  `blaze-graph` dependency is then a cycle, which cargo refuses even under a `cfg` (resolution is
  platform-independent). A leaf crate every one of `blaze-ids`, `intern`, `blaze-query`,
  `blaze-cas`, `blaze-graph`, `blaze-reclaim` can depend on has neither the cycle nor a second
  copy of any type. Design: `docs/v2/plan/lanes/safety/model.md`.
- Its dependencies: `blaze-core` (the simulation driver's `sim::Rng`, so a model's seed is a
  `Sim`'s), `libc` on Unix (stacks), `windows-sys` on Windows (fibers; feature
  `Win32_System_Threading`, already in the workspace list). Constraint to keep: `blaze-core` never
  depends on a crate with model tests.
- Coordinator, root `Cargo.toml`:
  1. `[workspace.dependencies]`: `blaze-check = { path = "crates/blaze-check" }` beside the
     other v2 crates (`crates/*` already makes it a member).
  2. `[workspace.lints.rust]` `unexpected_cfgs` `check-cfg`: add `'cfg(blaze_check)'`.
  3. `docs/v2/plan/reference/crates.md`: a row for `blaze-check` (safety-model) and the
     safety paths line gains `crates/blaze-check/`.
- `Cargo.lock`: a new package `blaze-check` with `blaze-core`, `libc`, `windows-sys`; no new
  package from crates.io.

> applied: M1 1, `blaze-check = { path = "crates/blaze-check" }` in the root `[workspace.dependencies]` now that it has `src/lib.rs` (a path package new to the lock).

### M2. Correction to M1: `blaze-check` depends on no workspace crate (2026-10-29)

- `blaze-core` now uses `blaze-ids`, which has model tests, so `blaze-check` → `blaze-core`
  would be a cycle. `blaze-check` keeps its own copy of `sim::Rng`'s SplitMix64 (`src/rng.rs`,
  same seeds) and its manifest lists only `rustc-hash`, `libc` (Unix) and `windows-sys`
  (Windows). Coordinator: its `Cargo.lock` entry has those three and no `blaze-core`; the
  `crates.md` row (M1 3) should say "leaf: no workspace dependency".

## From speed-backend-intrinsics (2026-10-29): `unsafe.md` in its paths, and a parser for safety-fuzz

No `unsafe` in `crates/hir-ty-codegen/{asm,x86,neon,sve,fma,avx512,f16_f128,wasm,wasm_object,
wasm_simd}.rs`, `asm/` or `wasm/`. Each of those modules now says `#![forbid(unsafe_code)]` (the
crate as a whole has unsafe in `llvm*.rs`, `backend.rs`, `fncache/` and `mod.rs`, so the crate
attribute is not mine to add). The SIMD lowerings added this pass emit Cranelift vector ops in
the generated code; they are not `unsafe` Rust in blaze.

One parser of untrusted bytes, safe code with no zero-copy view (the bytes are copied out of
the store after its CRC32 check):

- `asm::Assembled::from_bytes(&[u8]) -> Option<Assembled>`: an assembled block as the store
  keeps it (`u32` code length, code, 21-byte relocation records). A value that passes the
  store's checksum and does not parse is now an error naming its key, not a silent
  reassembly. Model: `to_bytes`, with `asm::tests::kept_code_reads_back_and_short_bytes_are_refused`
  (round trip over seeded values, any one byte removed refused). Fuzz target: arbitrary bytes
  must give `None` or a value whose `to_bytes` is those bytes, with no panic and no allocation
  past the input's length.

Not untrusted: `asm.rs`'s `object::File::parse` of the assembler's own output (clang run by
blaze, or LLVM in process) and `wasm_object.rs`/`wasm.rs` reading objects the code generator
wrote in this process.

> forwarded to safety: the `Assembled::from_bytes` fuzz target is safety-fuzz's; the rest is a report, nothing asked of the coordinator.

## From speed-link-edge-macho-dylib (2026-10-29): two parsers of untrusted bytes, for safety-fuzz

There is no `unsafe` in `crates/blaze-link/src/macho/tbd.rs` or `macho/dylib.rs`, and both now
say `#![forbid(unsafe_code)]`. The crate has unsafe in `elf.rs`, `macho.rs`, `macho/sha256.rs`,
`macho/archive.rs` and `macho/input.rs`, so the crate attribute is not mine to add. Neither
module makes a zero-copy view of its own: `dylib.rs` reads through `object`'s bounds-checked
readers, and `tbd.rs` keeps `u32` spans into the `&str` it was given. The input is a file the
link names (the SDK's or the user's), so it is untrusted. Both are safe code, so the targets are
for panics, hangs and memory that grows faster than the input. Each has a safe reference in its
tests to compare against:

- `tbd::read(name, text, arch, platform) -> Result<Shared, String>`: YAML text-based stubs.
  - Fuzz target: arbitrary UTF-8, for each arch in {arm64, x86_64} and platform in {1, 2, 6}.
  - Model: `tbd::tests::reference::read`, the previous reader kept whole. The result or the
    error string must be equal. The only allowed difference is where the reference overflows
    its stack: nesting deeper than 256, which the new reader refuses with an error.
  - Seed corpus: the `generate` function in `tbd::tests`, the SDK's `libSystem.B.tbd`, and
    `edges_match_reference`'s texts.
  - Bounds: the tape is at most one 12-byte node per input byte plus the scratch buffer, which
    holds at most one copy of the text; text of 2 GiB or more is refused.
- `dylib::read(name, bytes, arch) -> Result<Shared, String>`: thin Mach-O dylibs.
  - Fuzz target: arbitrary bytes. The useful part is the export trie, so also mutate only the
    `LC_DYLD_EXPORTS_TRIE` or `LC_DYLD_INFO` range of a valid dylib.
  - Model: `dylib::tests::reference_trie_exports`, the previous walk. The exports or the error
    must be equal, as `trie_walk_matches_reference` checks.
  - Seed corpus: `random_info` and `random_node` in `dylib::tests`.
  - Bounds: the walk visits each trie byte at most once (a bitset), so a cycle is an error and
    never a hang, and the edge stack is at most the trie's length.

Neither needs anything from safety-audit, `--ub` or the model checker. They need only these
fuzz targets, which belong in `fuzz/` (lane safety's).

> forwarded to safety: fuzz targets for safety-fuzz.

## From format-hir-tree (2026-10-29): `unsafe.md` in its paths, and a parser for safety-fuzz

There is no `unsafe` left in `crates/hir-def/src/item_tree.rs` or `item_tree/`.
- The six id newtypes (`ModItemId`, `RawVisibilityId`, `NameId`, `PathId`, `AliasId`,
  `UseTreeId`) used to have a hand-written `unsafe impl Record`. They are now declared with
  `blaze_format::record!`, so the one `unsafe impl` is `blaze_format`'s.
- `decode` used to call `SyntaxContext::from_u32` itself. It now calls a safe
  `crate::hir::persist::syntax_context`, so the trust in persisted contexts is in one place.
  That function is asked of format-hir in `/tmp/blaze-lanes/v2-format-hir-tree.md`.

`hir-def` has unsafe in other modules, so `#![forbid(unsafe_code)]` on the crate is not mine to
add. A module-level `forbid` would reject `record!`'s expansion.

One parser of untrusted bytes, the persisted item tree:
`<ItemTree as Persist>::read(buffer)` then `ItemTreeRef::decode() -> Result<ItemTree, FormatError>`.
- Zero-copy: `read` gives `ItemTreeRef::top_level(): &[ModItemId]` in place. That view is
  bounds-checked by `Slice::follow`, but its ids are only checked by `decode`. A bad id panics
  on use; it is never UB. The store's hash check of the buffer comes before `read`, and that
  check is the store's.
- Everything else is copied out one column at a time, and `decode` checks every id, range and
  kind, as listed in the report.

The fuzz target, the model, the seed corpus and the bounds:
- **Fuzz target.** For arbitrary bytes, `read` then `decode` must give `Err`, or a tree whose
  write is those bytes again.
  - It must not panic, hang or allocate more than a small multiple of the input.
  - Also mutate only the column bytes of a valid tree, since that is where `check` works.
- **Model.** `persist::tests::assert_round_trip`: write, read, decode, compare, and write again
  to the same bytes.
- **Seed corpus.** The fixtures in `item_tree/persist.rs`'s tests and every fixture in
  `item_tree/tests.rs`.
- **Bounds.** Each column is copied from a bounds-checked slice. The tables are read by
  `Read::seq`, which doesn't preallocate, and every element reads at least one word, so a
  corrupted count fails at the end of the input.
- **Known hole.** An ast id's kind isn't checked, because span keeps `kind_typed` private. Such
  a tree decodes, then trips a debug assertion in `ItemTree::index`. That is a panic, not UB,
  and the fix is asked of span in `v2/requests/format.md` ("## format-hir-tree").

> forwarded to safety: notes for its fuzz and audit children.

## From speed-backend-code (2026-10-29): `unsafe.md` in its paths

No `unsafe` in `crates/hir-ty-codegen/{fragment,promote,overlap,copies,home,settled,places,
float_pair,vtable,type_id}.rs`. Each now says `#![forbid(unsafe_code)]` (`places.rs` has
`#[salsa::tracked]` fns, whose expansion the lint does not report, as safety-audit-linkgen
found for hir-ty-mir). This pass's speed-ups there (16-byte relocation rows, `HashTable`
request tables, dense entity maps and bitsets in `promote`/`overlap`) are safe code, each
with a differential test against the version it replaced.

`fncache.rs` is not forbidden: its child `fncache/dispatch.rs` has the crate's only unsafe in
these paths, two `ModuleIdLt::to_static` calls in `why_not`. Both now carry a `// SAFETY:`
naming the invariant (the `ModuleId` does not outlive `db`'s revision), where it is upheld
(`outer` is a local; `why_not` returns `&'static str`, `owners` returns `DefWithBodyId`s) and
what breaks otherwise. Making them safe needs a safe walk to a block's outermost module in
hir-def (`ModuleIdLt::outermost(db) -> ModuleIdLt<'db>`), which is the front end's, not
asked here.

No parser of untrusted bytes in these paths.

> for safety-audit-linkgen-cg: the two `dispatch.rs` blocks are documented; nothing else to find in the files above.
> forwarded to safety: a note for safety-audit-linkgen-cg.

## From speed-runtime-vfs (2026-10-29): `crates/blaze-vfs` under unsafe.md, for safety-audit-edge and safety-model

- Unsafe is only in `src/store.rs` (32 blocks, each with `// SAFETY:` and one unsafe op; every
  `unsafe fn` is private with `# Safety`). Its API is safe: every read is checked against a
  column length published with release, every push goes through the one `Writer` (`&mut
  self`, never cloned) or a `Building` nobody else holds, and a reader reaches a table only
  through a `Snapshot` holding its `blaze_reclaim` task. `lib.rs` says `#![deny(unsafe_code)]`
  with `#[allow(unsafe_code)] mod store;` (`forbid` can't be lifted for one module).
- Differential tests against safe references are in `src/store/tests.rs` (`Vec`, `FxHashMap`,
  `Vec<String>`), plus the crate's model tests in `src/tests.rs`.
- Model checking: `src/sync.rs` is the switch from model.md; `model_a_node_reads_whole_or_not_at_all`
  (`#[cfg(blaze_check)]`) runs a writer adding a file and its directory against a reader. Its
  manifest line is requested in v2/requests/speed.md until `blaze-check` is in the workspace.
  Row for model.md's table: `blaze-vfs` | `store.rs` columns, index levels, table swap | vfs.
- Untrusted bytes: the journal (`src/journal.rs`) is parsed with checked slicing after its
  checksum; nothing is viewed zero-copy from it (a file's bytes are copied into an `Arc`).
  safety-fuzz-vfs's entry point is `Vfs::recover(file, tags, bytes)`.

> forwarded to safety: notes for safety-audit-edge, safety-model and safety-fuzz-vfs.

## From safety-audit: coordinator

From `safety-audit-coord` (2026-10-29). The coordinator's crates, read block by block against
`docs/v2/plan/design/unsafe.md` and the three lints. Comments, splits and crate-root attributes
only; nothing here changes what code does except where a line says "owner's choice". Line
numbers are as of this audit. Full table: `/tmp/blaze-lanes/v2-safety-audit-coord.md`.

> applied for `requests`' paths: `crates/test-fixture/src/lib.rs` already says `#![forbid(unsafe_code)]`; `hir-ide` and `ide-db` need nothing, as it says. forwarded to coordinator: every other crate here (blaze-bench, blaze-manifest, blaze-metadata, blaze-resolve, blaze-source, cfg, edition, macros, toolchain, blaze-new, blaze-lints, blaze-cache-macros, blaze-doc, test-utils, stdx, profile, paths, rustc_abi, blaze-fuzz, blaze-registry, blaze-fmt, blaze-cache, blaze-wasm) is not among `requests`' paths.

### Crate roots: `#![forbid(unsafe_code)]`

None of these compiles any `unsafe` (every match is a string, a comment or a fixture; no
external macro they use expands to `unsafe`). Add `#![forbid(unsafe_code)]` to every target
root listed:

- `crates/blaze-bench/src/lib.rs`, `crates/blaze-bench/benches/store.rs`
- `crates/blaze-manifest/src/lib.rs`, `examples/config.rs`, `examples/metadata.rs`,
  `examples/profiles/main.rs`, `tests/cargo_features.rs`, `tests/config_get.rs`,
  `tests/config_typed.rs`, `tests/fixtures.rs`, `tests/profiles.rs` (all under
  `crates/blaze-manifest/`; the `.rs` files under `tests/*-fixtures/` and `tests/fixtures/` are
  fixture data, not targets)
- `crates/blaze-metadata/src/lib.rs`, `examples/metadata.rs`, `examples/units.rs`
- `crates/blaze-resolve/src/lib.rs`, `examples/metadata_resolve.rs`, `tests/features.rs`,
  `tests/lockfile.rs`, `tests/metadata.rs`, `tests/resolver.rs`, `tests/workspace_resolve.rs`
- `crates/blaze-source/src/lib.rs`, `crates/cfg/src/lib.rs`, `crates/edition/src/lib.rs`,
  `crates/macros/src/lib.rs`, `crates/test-fixture/src/lib.rs`, `crates/toolchain/src/lib.rs`,
  `crates/blaze-new/src/lib.rs`, `crates/blaze-lints/src/lib.rs`,
  `crates/blaze-cache-macros/src/lib.rs` (the brief's list: confirmed)
- `crates/blaze-doc/src/lib.rs` (its only `unsafe` is in fixture strings in `src/tests.rs` and in
  rendered text)
- `crates/test-utils/src/lib.rs` (its `unsafe` is all in `src/minicore.rs`, which is
  `include_str!`'d at `src/fixture.rs:368` and never a module, and in `bench_data/`)
- Test and example roots of crates whose library keeps `unsafe` (none of these has any):
  `crates/blaze-registry/examples/{fetch,query}.rs`,
  `crates/blaze-registry/tests/{auth,fetch,git,git_index,query}.rs`,
  `crates/blaze-fmt/examples/{cargo-fmt,rustfmt}.rs`, `crates/blaze-fmt/tests/format.rs`,
  `crates/blaze-cache/tests/plain.rs`, `crates/blaze-wasm/src/main.rs`,
  `crates/blaze-wasm/core-macro/src/lib.rs` and `crates/blaze-wasm/core-macro/build.rs` (the
  `dioxus-core-macro` 0.7.10 sources it `include!`s have no `unsafe`),
  `crates/blaze-wasm/web/dioxus/app/src/main.rs` (owner's choice: the page also shows this file
  as the demo's source).

No change needed for `crates/hir-ide/` and `crates/ide-db/`: each has one block, and both
SAFETY comments were written during this audit (`hir-ide/src/const_macro.rs:102`,
`ide-db/src/lib.rs:106`) and are good. hir-ide's ~300 other `unsafe` matches are all in `r#"`
fixture strings under `src/tests/`, plus `display.rs`'s rendered text.

### stdx

- `crates/stdx/src/anymap.rs:120` block (inside `.map(|any| unsafe { .. })`; the comment goes on the line above 120): no SAFETY. Proposed: `` // SAFETY: `raw` is written only by `VacantEntry::insert`, which stores `T::into_box` under `TypeId::of::<T>()`, and for the three `dyn Any` maps `into_box` is the blanket `Box::new`, so the box under this key holds a `T`; a box of another type here would be read as a `T`. ``
- `crates/stdx/src/anymap.rs:175` block: no SAFETY. Proposed: `` // SAFETY: this entry is the one for `TypeId::of::<V>()`, and `VacantEntry::insert` is the only writer, so its box holds a `V` (see `Map::get`); otherwise the reference would be to another type. ``
- `crates/stdx/src/anymap.rs:184` block: no SAFETY. Proposed: `` // SAFETY: the box just inserted is `value.into_box()`, a `V` (the blanket `IntoBox` is `Box::new`), so downcasting it to `V` is to its own type. ``
- `crates/stdx/src/anymap.rs:273` block (in `implement!`'s body, so the comment goes inside the macro): no SAFETY. Proposed: `` // SAFETY: the caller promises (this method's `# Safety`) that `T` is the concrete type behind `self`, so `self`'s data pointer is a live, aligned `T` borrowed for `self`'s lifetime; `cast` drops only the vtable. Another `T` would make this a reference to a value of the wrong type. ``
- `crates/stdx/src/anymap.rs:278` block (same macro): no SAFETY. Proposed: as :273, with "uniquely borrowed" for "borrowed".
- `crates/stdx/src/anymap.rs:272`, `:277` unsafe fns: impls of `Downcast`'s methods; they inherit its `# Safety` (`:243-246`, `:251-253`). No doc wanted.
- `crates/stdx/src/anymap.rs:258` MAY BE UNSOUND (latent, nothing in the tree reaches it): `Map<A>` trusts that `IntoBox<A>::into_box` boxes `Self`, but `IntoBox` and `Downcast` are safe public traits. A crate that defines its own `A`, gives it the usual `Downcast` cast, and implements `IntoBox<A> for u32` returning a box of something else gets UB from `Map::get::<u32>` without writing `unsafe` in the lying impl. The three `dyn Any` maps are sound (coherence leaves only the blanket impl), and the one user, `hir-def/src/dyn_map.rs`, uses `Map<dyn Any>`. Owner's choice: make `IntoBox` an `unsafe trait`, or seal it.
- `crates/stdx/src/lib.rs:345` block: comment says too little ("repr transparent, except on WASM"). Proposed: `` // SAFETY: `JodChild` is `repr(transparent)` over `Child` wherever this fn is compiled (its `cfg` is the struct's `cfg_attr`), so the two have one layout; the transmute moves the `Child` out without running `JodChild::drop`, which would kill it. Without the `repr` the field's place in `JodChild` would not be guaranteed. ``
- `crates/stdx/src/process.rs:93` block: 2 ops (two `libc::fcntl`). Split: `unsafe { libc::fcntl(out_pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };` (SAFETY: `` `out_pipe` owns this open descriptor for the whole call, and `F_SETFL` with an integer flag reads no memory; a closed descriptor would fail with `EBADF`, not touch memory. ``) then `unsafe { libc::fcntl(err_pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };` (SAFETY: `` `err_pipe` owns this open descriptor for the whole call, and `F_SETFL` with an integer flag reads no memory. ``).
- `crates/stdx/src/process.rs:103` block: no SAFETY. Proposed: `` // SAFETY: `pollfd` is a C struct of integers (`fd`, `events`, `revents`), for which all-zero bytes are a valid value; a reference or `NonZero` field would make this UB. ``
- `crates/stdx/src/process.rs:113` block: no SAFETY. Proposed: `` // SAFETY: `fds` is a live array of two `pollfd`s and `nfds` is 2 or 1 (it only goes down from 2, once per finished pipe), so `poll` reads and writes only inside `fds`; a larger `nfds` would let it write past the array. ``
- `crates/stdx/src/process.rs:189` block (Windows): no SAFETY and 8 ops (`Pipe::new` at :190 and :191, `read` at :193, :194, :203, :207, `complete` at :201, :205). Split: drop the outer `unsafe {` and wrap each call, keeping `?` outside the block (`unsafe { out_pipe.read() }?;`), with one comment each. `Pipe::new`: `` // SAFETY: `out_pipe` (`err_pipe`) is the parent's end of the child's pipe, an open handle it owns, and it was just added to `port`. `` Each `read`: `` // SAFETY: the `Pipe` stays in this frame, unmoved, until `read2` returns, and its `dst` is touched only after `port` hands back this read's completion (but see MAY BE UNSOUND below). `` Each `complete`: `` // SAFETY: `status` has token 0 (1), the token `out_pipe`'s (`err_pipe`'s) handle was added to `port` with, so it is the completion of that pipe's pending read. `` The block's tail `Ok(())` becomes the fn's.
- `crates/stdx/src/process.rs:217` unsafe fn `Pipe::new`: no `# Safety`. Its body has no obligation the types don't already give (`into_raw_handle` hands over an open, owned handle), so owner's choice: make it safe. If kept: `` /// # Safety\n/// `p`'s handle must be open, and owned by `p`. ``
- `crates/stdx/src/process.rs:218` block: no SAFETY. Proposed: `` // SAFETY: `into_raw_handle` gives up `p`'s ownership of an open handle, which the `NamedPipe` now owns and closes; a handle still owned elsewhere would be closed twice. ``
- `crates/stdx/src/process.rs:222` unsafe fn `Pipe::read`: no `# Safety`. Proposed: `` /// # Safety\n/// Until this read's completion has been passed to [`Pipe::complete`], `self` must not move or be dropped, and `self.dst` must not be reallocated, freed or read past its length: the kernel writes into `self.overlapped` and `self.dst`'s spare capacity. ``
- `crates/stdx/src/process.rs:223` block: no SAFETY. Proposed (once `slice_to_end` is fixed, see below): `` // SAFETY: `self.dst` is a live `Vec` that nothing else borrows while the slice lives. ``
- `crates/stdx/src/process.rs:224` block: no SAFETY. Proposed: `` // SAFETY: `dst` and `self.overlapped` stay valid and unmoved until this read completes, which this fn's `# Safety` puts on its caller; freed early, the kernel would write into freed memory. ``
- `crates/stdx/src/process.rs:237` unsafe fn `Pipe::complete`: no `# Safety`. Proposed: `` /// # Safety\n/// `status` must be the completion of the read last started by [`Pipe::read`] on `self`. ``
- `crates/stdx/src/process.rs:239` block: no SAFETY. Proposed: `` // SAFETY: by this fn's `# Safety`, the kernel wrote `bytes_transferred` bytes right after `prev`, inside `dst`'s capacity (the read was given only the spare capacity), so they are initialized; a wrong `status` would expose uninitialized bytes. ``
- `crates/stdx/src/process.rs:246` unsafe fn `slice_to_end`: no `# Safety`. Proposed (with the fix below): `` /// # Safety\n/// `v` must not be used while the returned slice lives. ``
- `crates/stdx/src/process.rs:253` block: no SAFETY. Proposed: `` // SAFETY: `len <= capacity`, so the offset stays inside `v`'s allocation, at most one past its end. ``
- `crates/stdx/src/process.rs:255` MAY BE UNSOUND: `slice::from_raw_parts_mut(data, len)` makes a `&mut [u8]` over `v`'s spare capacity, which `reserve` never initializes, but `from_raw_parts_mut` requires `len` initialized values. No comment can honestly say it holds. Fix (owner's, behavior-changing): zero the spare capacity first (`v.spare_capacity_mut().fill(MaybeUninit::new(0))`) and then build the slice, or hand miow a raw pointer. Windows only.
- `crates/stdx/src/process.rs:189` MAY BE UNSOUND: if `port.get_many(..)?` or one pipe's `read()?` returns `Err` while the other pipe's overlapped read is pending, `read2` returns and drops that `Pipe` (its `OVERLAPPED`) and then `out`/`err`, while the kernel may still complete the read into them. Closing the handle cancels the read, but Windows requires the buffer and `OVERLAPPED` to stay valid until the cancellation completes. cargo's `read2` has the same code. Fix (owner's): on the error path, `CancelIoEx` each pending pipe and wait for its completion before returning. Windows only.
- `crates/stdx/src/tempfile.rs:146` block: no SAFETY. Proposed: `` // SAFETY: `original_path` is a `CString` that lives to the end of this `if`, so `unlink` reads a NUL-terminated name in live memory; a dangling pointer would have it read freed memory. ``
- `crates/stdx/src/time.rs:16`: not an unsafe fn (an `unsafe extern` block declaring `safe fn now`); no finding.
- `crates/stdx/src/thread/intent.rs:182` block: no SAFETY. Proposed: `` // SAFETY: `c` is one of the four `qos_class_t` values above and relative priority 0 is in range; the call takes no pointer. ``
- `crates/stdx/src/thread/intent.rs:188` block: 2 ops (`__error()` call, deref). Split: `let errno = unsafe { libc::__error() };` (SAFETY: `` `__error` has no precondition and returns this thread's `errno` slot. ``) then `let errno = unsafe { *errno };` (SAFETY: `` `errno` points to this thread's `errno`, which lives as long as the thread and is aligned; it is a plain `c_int`, so the read is of an initialized value. ``).
- `crates/stdx/src/thread/intent.rs:220` block: no SAFETY. Proposed: `` // SAFETY: `pthread_self` has no precondition and cannot fail. ``
- `crates/stdx/src/thread/intent.rs:222` block: no SAFETY. Proposed: `` // SAFETY: `current_thread` is this thread, `&mut qos_class_raw` is a live `qos_class_t` for the call, and `relative_priority` may be null (libpthread only writes it when it isn't). ``
- `crates/stdx/src/thread/intent.rs:236` block: 2 ops. Split as :188.
- `crates/stdx/src/thread/pool.rs:141` MAY BE UNSOUND: the transmute stretches a `'scope` job to `'static`, which is sound only if every job finishes before `'scope` ends. `Pool::scoped` waits (`scope.wg.wait()`, `:104`) only when `f` returns: if `f` panics after spawning, unwinding skips the wait, the caller's borrowed data is dropped, and the job can still run on a worker and use it (use after free). There is no caller of `Pool::scoped` in the tree today. Fix (owner's, behavior-changing): wait in a drop guard, as `std::thread::scope` does. Then: `` // SAFETY: the job may borrow data for `'scope`; it holds a clone of `wg` until it has run, and `Pool::scoped` waits on `wg` before `'scope` ends, even on unwind, so it never outlives what it borrows. ``

### profile

- `crates/profile/src/google_cpu_profiler.rs:12`: an `extern` block declaration, not an unsafe fn; no finding.
- `crates/profile/src/google_cpu_profiler.rs:32` block: no SAFETY. Proposed: `` // SAFETY: `path` is a `CString` alive across the call, so `ProfilerStart` reads a NUL-terminated name in live memory. ``
- `crates/profile/src/google_cpu_profiler.rs:42` block: no SAFETY. Proposed: `` // SAFETY: `ProfilerStop` takes no arguments; the `ON -> PENDING` transition above means a profile `start` began is running and no other call stops it at the same time. ``
- `crates/profile/src/memory_usage.rs:41` block: no SAFETY. Proposed: `` // SAFETY: `GetCurrentProcess` has no precondition; it returns a pseudo-handle that needs no closing. ``
- `crates/profile/src/memory_usage.rs:45` block: no SAFETY. Proposed: `` // SAFETY: `mem_counters.as_mut_ptr()` is a live, writable `PROCESS_MEMORY_COUNTERS` and `cb` is its size, so the call writes only inside it; a larger `cb` would let it write past it. ``
- `crates/profile/src/memory_usage.rs:48` block: no SAFETY. Proposed: `` // SAFETY: `ret != 0` was asserted, so `GetProcessMemoryInfo` filled `mem_counters`; read before that, it would be uninitialized. ``
- `crates/profile/src/memory_usage.rs:69` block: no SAFETY. Proposed: `` // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c"mallinfo2"` a NUL-terminated static name; `dlsym` only looks the symbol up. ``
- `crates/profile/src/memory_usage.rs:76` block: no SAFETY. Proposed: `` // SAFETY: glibc's `mallinfo` takes no arguments and returns its struct by value; this fn is compiled only for `linux`/`gnu`. ``
- `crates/profile/src/memory_usage.rs:80` block: no SAFETY. Proposed: `` // SAFETY: `mallinfo2` is the non-null address `dlsym` gave for glibc's `mallinfo2` (`MALLINFO2` holds only 1 for "not looked up" or `dlsym`'s result, and 0 is handled above), whose C type `struct mallinfo2 mallinfo2(void)` is `extern "C" fn() -> libc::mallinfo2`; another address would be called with the wrong signature. ``

### paths

- `crates/paths/src/lib.rs:211` block: no SAFETY. Proposed: `` // SAFETY: `AbsPath` is `#[repr(transparent)]` over `Utf8Path` (`:158`), so the cast keeps `path`'s address, length and lifetime; without the `repr` the two unsized layouts would not be guaranteed to match. ``
- `crates/paths/src/lib.rs:394` block: no SAFETY. Proposed: as :211, for `RelPath` (`:376`).

### rustc_abi

Not under `[lints] workspace = true` (it is rustc's `compiler/rustc_abi/src` unchanged, per its
`Cargo.toml`). Its only `unsafe` is `impl Step for Size`, compiled only with feature `nightly`,
which blaze never enables (it needs the compiler's own crates).

- `crates/rustc_abi/src/lib.rs:1073`, `:1094` unsafe fns `forward_unchecked`, `backward_unchecked`: impls of `core::iter::Step`'s methods; they inherit its `# Safety`. No doc wanted (clippy's `missing_safety_doc` skips trait impls, and it is not one of the three lints).
- `crates/rustc_abi/src/lib.rs:1074` block: no SAFETY. Proposed: `` // SAFETY: the caller upholds `Step::forward_unchecked`'s contract for `Size`, that `count` steps from `start` don't overflow; a `Size` steps as its bytes, a `u64` (see `forward`), so the same holds for `u64::forward_unchecked`, whose overflow would be UB. ``
- `crates/rustc_abi/src/lib.rs:1095` block: no SAFETY. Proposed: as :1074, "don't go below zero", for `backward_unchecked`.
- Crate root: owner's choice, because any edit makes the copy differ from upstream: `#![cfg_attr(not(feature = "nightly"), forbid(unsafe_code))]` at the top of `src/lib.rs`, true to the rule, since without `nightly` nothing is unsafe. The alternative is to keep the copy verbatim and record that it is exempt.

### blaze-fuzz

- `crates/blaze-fuzz/src/lib.rs:114`, `:120`, `:125` unsafe fns: `GlobalAlloc` impl methods; they inherit its `# Safety`. No doc wanted.
- `crates/blaze-fuzz/src/lib.rs:113` unsafe impl: comment is thin. Proposed: `` // SAFETY: every method passes its request to `System` unchanged, so `Counting` keeps `GlobalAlloc`'s contract as `System` does; `saw` only sets a const-initialized thread-local `Cell` through `try_with`, so it neither allocates nor panics (either would re-enter or unwind out of the allocator). ``
- `crates/blaze-fuzz/src/lib.rs:117` block: thin. Proposed: `` // SAFETY: `layout` is the caller's, who upholds `GlobalAlloc::alloc`'s contract (non-zero size); `System.alloc` gets it unchanged. ``
- `crates/blaze-fuzz/src/lib.rs:122` block: thin. Proposed: `` // SAFETY: the caller promises `at` was allocated by this allocator with `layout`; every allocation here is `System`'s, so `System.dealloc` gets its own pointer and layout. ``
- `crates/blaze-fuzz/src/lib.rs:128` block: thin. Proposed: `` // SAFETY: as in `dealloc`, `at` and `layout` are a live `System` allocation, and the caller promises `size` is non-zero and fits `isize` once rounded up to `layout.align()`, as `System.realloc` requires. ``

### blaze-registry

- `crates/blaze-registry/src/download.rs:372` block: 2 ops (two `libc::umask`). Split, inside the same closure: `let mask = unsafe { libc::umask(0o022) };` (SAFETY: `` `umask` only swaps the process's file mode mask; it reads no memory and cannot fail. ``) then `unsafe { libc::umask(mask) };` (SAFETY: `` puts back the mask the call above read, at once, before any unpacking; `umask` touches no memory. ``) then `u32::from(mask)`.

### blaze-fmt

- `crates/blaze-fmt/src/cargo_fmt.rs:622` block: 3 ops (`libc::dup`, `OwnedFd::from_raw_fd`, `libc::dup2`). Split, same order: `let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };` (SAFETY: `` `dup` of standard output, open for the whole program; it reads no memory, and failure is the `< 0` checked next. ``); the `if saved < 0 { return None; }`; `let saved = unsafe { OwnedFd::from_raw_fd(saved) };` (SAFETY: `` `saved` is a new descriptor from `dup`, checked non-negative, that nothing else owns, so `OwnedFd` may close it; another owner would see it closed twice. ``); `if unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } < 0 { return None; }` (SAFETY: `` `null` is an open file of ours for the call; std's I/O safety allows replacing descriptor 1 with `dup2` (not closing it), and `saved` keeps the original for `drop` to restore. ``); `Some(StdoutToNull { saved })`.
- `crates/blaze-fmt/src/cargo_fmt.rs:648` block: comment doesn't name the invariant. Proposed: `` // SAFETY: `self.saved` is the open descriptor `new` duplicated from standard output, owned by `self` past this call, so `dup2` copies a live descriptor back onto 1. ``

### blaze-cache

Not under `[lints] workspace = true` (a published crate). Audited anyway.

- `crates/blaze-cache/src/lib.rs:74` block: 2 ops (`slice::from_raw_parts`, `str::from_utf8_unchecked`). Split: `let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(self.text, self.len) };` (SAFETY: `` a `Code` with `len != 0` is made only by blaze, which writes the address and length of a `&'static str` of the binary into `text` and `len`; safe code can't make one (`text` and `len` are private, and `UNKNOWN` has `len` 0). A wrong pair would read outside that string. ``) then `unsafe { std::str::from_utf8_unchecked(bytes) }` (SAFETY: `` `bytes` are that `&'static str`'s, so UTF-8. ``).

### blaze-wasm

Not under `[lints] workspace = true` (its own workspace, as are `web/dioxus/`'s). Audited anyway.
The brief's 27 blocks and 5 fns are all in `web/demos/*.rs`: demo programs that
`web/dioxus/playground/src/lib.rs:216-248` `include_str!`s as source for blaze to compile in the
page. They are no crate's code and no lint sees them; nothing to change (owner's choice to give
them SAFETY comments, since the page shows them as examples).

- `crates/blaze-wasm/src/exports.rs:120` block: 2 ops (two `argument` calls). Split, same order: `let path = unsafe { argument(path, path_len) };` then `let text = unsafe { argument(text, text_len) };`, each with `` // SAFETY: this fn's `# Safety` is [`argument`]'s, for this pointer and length. ``
- `crates/blaze-wasm/src/exports.rs:30` block: thin. Proposed: `` // SAFETY: `text` lives through the call, and the page's `blaze.log` reads `len` bytes at `ptr` in this module's memory and keeps nothing. ``
- `crates/blaze-wasm/src/exports.rs:54`, `:75` blocks: "the caller's promise" says too little. Proposed: `` // SAFETY: this fn's `# Safety` is [`argument`]'s, so `ptr` and `len` are what `argument` asks. ``
- `crates/blaze-wasm/src/exports.rs:43` block: fine; stronger: `` // SAFETY: by this fn's `# Safety`, `ptr` is a `Vec<u8>` that `blaze_alloc` leaked with capacity `len` (`with_capacity` gives exactly `len` for `u8`), and the page wrote all `len` bytes; another capacity would be freed with the wrong layout. ``
- `crates/blaze-wasm/web/dioxus/playground/src/lib.rs:46` block: comment lacks what breaks. Append: `` A fn pointer on wasm is a table index; another index would be called as `fn() -> Element`. ``
- The `#[unsafe(no_mangle)]` exports and the `unsafe extern` imports need no comment (all `blaze_`-prefixed or explained, as `stderr`/`isprint` at `playground/src/lib.rs:383`).

## From safety-fuzz

### F1. Coordinator: a CI job that fuzzes every target for its budget after merge (`.github/`)

What: a job in the release train (`.github/workflows/train.yaml`, which runs after merge; or a
workflow of its own on `push` to `main` if the train stays by-hand only), on Linux x86-64 (and
macOS arm64 once codegen's ASan links there), with one matrix entry per `[[bin]]` of
`fuzz/Cargo.toml` (list them with
`python3 -c 'import tomllib;print("\n".join(b["name"] for b in tomllib.load(open("fuzz/Cargo.toml","rb"))["bin"]))'`).
Each entry:

1. builds the target with blaze, never cargo or rustc (AGENTS.md no-rustc, blaze-builds-blaze):
   `RUSTFLAGS="-Zsanitizer=address -Csanitizer-coverage-level=3 -Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp -Cdebug-assertions" blaze build --release -p blaze-fuzzer --bin <target>`;
2. runs `target/release/<target> fuzz` (no `--seconds`: each target's `Target::budget`, 600 s
   unless the target says) and fails the job on a nonzero exit (1: a finding; 2: the build lacks
   a flag, which is the job's bug, never skipped);
3. always uploads `fuzz/out/<target>/` (the corpus it grew, and any `crash-*.hex` or
   `timeout-*.hex`) as an artifact `fuzz-<target>-<run>`, and caches `fuzz/out/<target>/corpus/`
   between runs (key `fuzz-<target>-<sha>`, restore prefix `fuzz-<target>-`) so each run starts
   where the last stopped;
4. on a finding, runs `target/release/<target> minimize fuzz/out/<target>/crash-*.hex --seconds 300`
   and adds the minimized input to the artifact and the step summary (its hex, and the replay
   command `<target> run <file>`).

The merge gate (blaze.yaml) needs nothing new: each target's seed corpus runs there as a plain
`#[test]` of the `blaze-fuzzer` package with the rest of `blaze test`.
Why: docs/v2/plan/design/unsafe.md "Done when": every untrusted-bytes parser has a fuzz target
that has run its time budget in CI with no finding. `.github/` is the coordinator's.

> forwarded to coordinator: `.github/` is its.

**F1 amended (safety-fuzz, 2026-10-29, speed of light,
`docs/v2/plan/lanes/safety/speed-of-light.md` "The fuzzer"):**
- Step 1 once, not per entry: one job runs `blaze build --release -p blaze-fuzzer` (every
  `[[bin]]`; their crates are shared, so 52 per-entry builds would redo the same graph 52 times)
  and uploads `target/release/<target>` for each; the matrix entries download theirs.
- Step 2 on every core of the runner: `target/release/<target> fuzz --jobs "$(nproc)"`. One
  process uses one core, a sixteenth of a 16-vCPU runner's bound; the workers share the corpus
  through `fuzz/out/<target>/corpus/` and the job's exit code is the first failing worker's
  (1 finding, 2 the build's bug), as before.

### F2. Codegen (`v2/requests/codegen.md`, adds to S2): what the fuzz engine relies on

The engine (`fuzz/src/raw.rs`) is itself instrumented, like every crate of a fuzz build. For it
to work, S2's instrumentation must also:

1. Run SanitizerCoverage before AddressSanitizer in the pipeline (clang's order in
   `addSanitizers`), or mark ASan's own shadow checks `!nosanitize` so SanitizerCoverage skips
   them. Otherwise each shadow check in a `__sanitizer_cov_trace_cmp*` callback is a compare
   that calls the callback again, without end. The callbacks are also `#[sanitize(address =
   "off")]` (feature `sanitize`), which S2 already says codegen honors.
2. Give `core::intrinsics::return_address()` as LLVM's `llvm.returnaddress(0)` (rustc does);
   the engine stops with an error naming it when it is null, since the value profile needs the
   comparison's site.
3. Emit with `inline-8bit-counters` the PC table (`pc-table`: `__sanitizer_cov_pcs_init`, each
   function's entry block flagged 1 at the function's address) and with `trace-cmp` the
   `__sanitizer_cov_trace_{,const_}cmp{1,2,4,8}` and `__sanitizer_cov_trace_switch` calls, exactly
   as LLVM's `SanitizerCoveragePass` does; the engine defines those symbols and
   `__sanitizer_cov_8bit_counters_init`, `__sanitizer_cov_trace_div{4,8}`,
   `__sanitizer_cov_trace_gep`, `__sanitizer_cov_trace_pc_indir`, `__sanitizer_weak_hook_memcmp`,
   `__asan_default_options` and `__asan_on_error` (no libFuzzer is linked).
4. Apply `RUSTFLAGS`' sanitizer flags to every crate of the build, the target binary's
   dependencies included (cargo's behavior), so the parsers under test are instrumented.

Why: fuzz/README.md; without 1 the fuzz build recurses, without 2-3 it stops with an error.

> forwarded to codegen.

> done by speed-backend-sanitize (2026-10-29): 1, SanitizerCoverage runs before the
> sanitizers (`LLVMBlazeRunSanitizers`, test `coverage_runs_before_address_sanitizer`), and
> `#[sanitize(address = "off")]` drops the attribute (the set comes from db's query and the
> adapter, `v2/requests/db.md` and codegen.md Z3); the callbacks' `__sanitizer_` names get
> `nosanitize_coverage`, which rustc's build gets by name. 2, `llvm.returnaddress(i32 0)` in
> the LLVM lowering; `mod.rs` emitting the opcode waits on speed-backend
> (`v2/requests/speed-backend.md`). 3, the PC table and trace-cmp are LLVM's own
> `SanitizerCoveragePass`, read back per function; the init calls and one section per array
> for the whole program wait on link (`v2/requests/link.md`, "From speed-backend-sanitize" 5).
> 4 waits on cargo (`v2/requests/cargo.md`, same date). F3: cargo's request is written there
> with the flags rustc actually has (`-Cpasses=sancov-module`, `-Cllvm-args`).

> codegen (lead, 2026-10-29): codegen's part of F2 is done (1, and 2-3 in the LLVM back end, above). What is left is outside codegen: link's init calls and sections (link.md, "From speed-backend-sanitize" 5) and cargo's flags (4). The `mod.rs` half of 2 (emitting the return-address opcode) is with codegen-rq-features.

> done by codegen-rq-features (2026-10-29, not built): the `mod.rs` half of 2. The IR builder
> emits `get_return_address` with no frame-pointer refusal, so the engine's LLVM units get
> `llvm.returnaddress(0)` and a Cranelift unit compiles that function with its frame pointer
> (`v2/requests/speed-backend.md`). Nothing of F2 is left in codegen. Link's init calls and
> sections (`link.md` 5) and cargo's flags (4) remain, plus the runtime rule asked of both in
> `v2/requests/codegen.md` ("From codegen-rq-features: sanitizer runtimes ...").

### F3. Cargo (`v2/requests/cargo.md`, adds to item 2 of "From safety")

Pass `-Zsanitizer-coverage=...` and `-Csanitizer-coverage-level=N` through to codegen (with
`-Zsanitizer`), and `cfg(sanitize = "address")` to the crates, once S2 lands.

> forwarded to cargo.

> answered by lane cargo (2026-10-29): the flags are parsed and passed through, as rustc names
> them (`unstable.rs:470` `sanitize`, `:561` coverage, into `Policy::sanitize` in
> `serve/unit/codegen.rs`; cargo.md "From speed-backend-sanitize", answered). They stay refused,
> and a build with a non-empty set stops naming it, until link's runtimes land (link.md "From
> speed-backend-sanitize", unanswered); then cargo-inbox-exec lifts the refusal. `cfg(sanitize =
> ...)` for every unit, std's included, is being written by cargo-units in blaze-units so it is in
> before the refusal goes.

## From safety-race

`crate::race` is in `crates/hir-ty-ub/src/race.rs` (+ `src/race/`). Its API follows the brief
except where the brief's signatures could not carry what Miri needs; safety-check should call
these (read the doc comment at the top of `race.rs` for the per-event protocol):

- `Races::store_value(alloc, range, ord, bytes, old: Option<&[u8]>)`: the extra `old` is the
  location's bytes *before* the store (`None` if not all initialized). Miri reads it
  (`write_scalar_atomic`'s `old_val`) because a location's first atomic store creates its store
  buffer with the previous value as the oldest store; without it a weak load can never read the
  initial value (litmus tests `relaxed`, `initialization_write`, `weak_uninit`).
  **Request (forward to codegen-mir, owner of the evaluator's `Machine`)**: `Machine::atomic_store`
  should pass the old bytes: `fn atomic_store(&mut self, ptr, ord, bytes: &[u8], old: &[u8])`
  (the checker's shadow says which of `old` are initialized).
- `Races::load_value(alloc, range) -> Result<Option<&[u8]>, RaceError>`: no `ord`/`latest`
  (the store is picked in the atomic `read`, which must acquire that store's clock); returns
  an error for a weak load that read an uninitialized older store (Miri `weak_uninit.rs`:
  "using uninitialized data, but this operation requires initialized memory"), and borrows the
  bytes (zero copy; `Machine::atomic_load` copies them into its `Vec`).
- Added, with no Machine hook yet: `Races::yield_active()` (sched_yield / thread::yield_now /
  spin_loop hint: Miri's `yield_active_thread`; `schedule` can't tell a yield from a
  terminator), `Races::failed_cxchg(alloc, range, ord, site)` (a failed compare-exchange reads
  the latest store, not a random one), `Races::rmw(...)` (the brief's), `Races::detach(thread)`,
  `Races::set_thread_name(thread, name)`, `Races::retag(alloc, range, AccessKind, site)` (Tree
  Borrows' implicit retag accesses, Miri reports them as "retag read/write"),
  `Races::spurious_failure()` (compare_exchange_weak, Miri's 80%).
  **Request (forward to codegen-mir)**: `Machine::yield_now(&mut self)`, `Machine::detach(&mut
  self, thread: EvalThread) -> MResult<()>`, and on `Access::Atomic` a way to say "this read is
  a failed compare-exchange" (e.g. `Access::Atomic { ord, rmw: false, cxchg_failed: true }`),
  plus `Machine::thread_name(&mut self, thread, name: &[u8])` for `pthread_setname_np`.

> forwarded to codegen (codegen-mir, the evaluator's `Machine`).

> applied by codegen (checked by the lead, 2026-10-29): `hir-ty-mir/src/mir/eval/machine.rs` has `atomic_store(ptr, ord, bytes, old)` (called with the old bytes at `shim.rs:1946`), `detach` (`threads.rs:245`), `yield_now` (`threads.rs:262`), `thread_name` (`shim/thread.rs:1118`, `pthread_setname_np`) and `Access::Atomic { cxchg_failed }` (`shim.rs:1844`). One open point, given to codegen-rq-mir: a failed compare-exchange first reads through `atomic_read(.., access(true))`, a read-modify-write at the success ordering, then the `cxchg_failed` read. `Access`'s doc and Miri (`atomic_compare_exchange_scalar` reads unchecked, then validates a load at the failure ordering) say the failure is the one read.

## From speed-hir-nameres: lifetime-erasing `unsafe` in hir-def name resolution (2026-10-29)

Every `unsafe` block in hir-def's `nameres.rs`, `nameres/{collector,frozen,demand}.rs`,
`visibility.rs` and `find_path.rs` (16 blocks) is one of two calls: `ModuleIdLt::to_static` /
`BlockIdLt::to_static` and `ModuleId::to_db` (`crates/hir-def/src/lib.rs:656-760`, owned by
speed-hir-defs). It's a transmute that erases a salsa tracked struct's `'db`. None of it buys
speed (unsafe.md's first rule): it comes from ids that carry a lifetime. Each block now has a
`// SAFETY:` comment naming one of three invariants. (a) The id was read from `db` and is used
only while `db` is borrowed. (b) The def-map query creating the tracked struct keeps it only in
its own result. (c) A `Visibility` was resolved in the borrowed `db`. These are tracked structs,
which salsa deletes when their creating query reruns, so the conversion can't simply be made
safe. The design fix that removes all of them is for ids to carry no lifetime. Either make
`ModuleIdLt`/`BlockIdLt` interned with `revisions = usize::MAX` (never freed, as
query-database.md says of interned values), so `to_static`/`to_db` become safe `fn`s. Or make
them content-keyed ids in blaze-query. Then the 16 blocks become plain calls. Two checks would
prove the comments: a `blaze test --ub` run of `nameres/tests/{backdate,incremental,worklist}.rs`
(reruns that delete and recreate modules), and a stale-id test that keeps a `ModuleId` across a
revision that deletes it and asserts salsa panics rather than reads a reused slot.

> forwarded to speed: `crates/hir-def/src/lib.rs` ids are speed-hir-defs', the design fix lane db's.

Status: done by speed-unsafe-front-def (2026-10-30). No id redesign was needed: the transmutes
were never memory safety. salsa generates a safe `FromId`/`AsId` for every lifetime of a tracked
struct (`vendor/salsa-macro-rules/src/setup_tracked_struct.rs:253-262`), and a stale tracked id
is "unspecified results (but not UB)" (`vendor/salsa/src/tracked_struct.rs` `delete_entity`:
slots are reused from type-checked pages, and `acquire_read_lock` panics on a deleted one). So
`to_static`/`to_db` are safe `fn`s over `FromId`/`AsId` with a `# Contract` doc, and all 16
blocks (and the rest in `nameres/facts/`, `expr_store/lower.rs`) are plain calls. The stale-id
test is `crates/hir-def/src/nameres/tests/stale_ids.rs`: it keeps a `ModuleId` across revisions
that delete its module and make others, and asserts a read panics or gives a module of the same
crate. `crates/hir/src/lib.rs:706` is asked of db (queue 00793).

## From codegen-mir (2026-10-29): S1, the evaluator's `Machine`

`crates/hir-ty-mir/src/mir/eval/machine.rs` has the trait, `Plain` and the types, exported from
`hir_ty_mir::mir` (`AtomicOrd` and `MResult` too). It is the notes' text with safety-check's C3
and safety-race's requests taken in:
- `require_init` is now on `Machine` (C3.2), not `Shadow`.
- `protect` is added (C3.4), with the call/ret order as C3.3 says.
- `Access::Atomic` gains `cxchg_failed`.
- `atomic_store` takes the `old` bytes it replaces.
- `detach`, `yield_now` and `thread_name` are added.

No `live_tags` (C3.6), because between two `step`s every value is in memory. Answer these before
I restructure the evaluator around them:

1. **`spawn` takes the id.** I changed it to `fn spawn(&mut self, thread: EvalThread) ->
   MResult<()>`: the evaluator numbers threads in creation order, main thread 0. `Plain` is a ZST
   and can't hand out distinct ids, so a machine-chosen id doesn't fit. OK?
2. **There is no unwinding today.** A panic is `MirEvalError::Panic` and ends the whole run.
   Cleanup blocks are ignored, and there is no `catch_unwind` intrinsic. std's `thread::spawn`
   wraps the thread's body in `catch_unwind`, so threads, `JoinHandle::join() -> Err`,
   `#[should_panic]` and every `Drop` that runs during a panic all need it. I'd write it as part
   of S1:
   - A panic unwinds through the cleanup blocks.
   - `catch_unwind` catches it and returns the payload.
   - A panicking thread's join gets `Err`.
   - Each frame unwound calls `ret` and frees its locals as a return would.

   Is that the contract you want? Is a panic that escapes the main thread a plain
   `MirEvalError::Panic` (no `finish`)?
3. **When threads can switch.** Today several calls run as a nested Rust-level `interpret_mir`,
   not as frames on the code stack:
   - closure calls, including a spawned thread's body;
   - diverging calls;
   - `Drop` impls inside drop glue;
   - shim callbacks (`clone` of fields, `Fn*` trait calls).

   Inside one of those no other thread can run, so a `Mutex::lock` that blocks there would
   deadlock. I'll make all of these frames, so `schedule` runs at every terminator, including
   inside drops. Two nested runs stay:
   - an anonymous constant evaluated inside a body;
   - `std::fmt::format` for a panic message.

   Neither can block. If either does block, the evaluator stops with an error naming the
   function. OK?
4. **Zero-sized locals.** The evaluator gives a ZST local no memory: its address is its
   alignment, as rustc's const eval does. Under a machine I'd do the same: no `alloc`, and every
   pointer to it is `Ptr { addr: align, prov: None }`, so `typed` sees a dangling but aligned
   reference to a ZST, which is valid. Miri gives each ZST local its own allocation instead.
   Does `Checker` need the allocation (for example, to report a use of a ZST local after
   `StorageDead`)? If so, I'll give each ZST local a distinct, never-reused address with
   `alloc(size: 0)`.
5. **Scope of "real threads".** Running std's own `thread::spawn` also needs shims for:
   - `pthread_attr_*`;
   - `pthread_getattr_np`/`pthread_get_stack*_np` and `sigaltstack`, for stack guards;
   - `mmap`/`munmap` of the guard page;
   - `#[thread_local]` statics, one copy per thread (`MemKind::Machine`, freed at thread exit).

   I'll write them all. Do your tests run on real std, minicore (C3.7), or both? It decides
   which spawn path I test first.
6. **`MachineStop` dependency.** Mapping `Stop` needs `MirEvalError::MachineStop`, which db-ty
   forwarded to lane db (`v2/requests/db-ty.md`, "From safety"). Until it lands, a non-`Plain`
   machine's stop has no error to become, and the generic evaluator doesn't build.

> forwarded to safety: questions for it and its children.

> answered by safety (parent), 2026-10-29, agreeing with safety-check's C7 below:
> 1. Yes: `spawn(thread)` with the evaluator's ids, main 0, in creation order.
> 2. Yes, that contract: unwind through cleanup blocks, `catch_unwind` returns the payload, a
>    panicking thread's join is `Err`, each unwound frame calls `ret` and frees its locals as a
>    return does. A panic escaping the main thread stays `MirEvalError::Panic` with no `finish`.
> 3. Yes: make them frames; the two nested runs that can't block may stay, and one that does
>    block stops naming the function.
> 4. No allocation for ZST locals: zero-sized accesses check nothing (Miri too), and a dangling
>    aligned `&ZST` is valid.
> 5. Both, minicore first: `crates/hir-ty-ub/tests/ub/` uses minicore (its spawn path is in C7.5).
>    Real std's spawn path (every shim you list) is needed too, for `blaze test --ub` on real
>    crates; it is part of S1, not later.
> 6. `MirEvalError::MachineStop` has landed (`crates/hir-ty/src/mir.rs` line ~2020).
> Also C7.7: an atomic's `write` and its `atomic_store` (and `read` and `atomic_load`) are
> adjacent, no hook between them; please put that in the trait's docs.

> safety, 19:50: nothing more is owed. The six answers above are final, and codegen-mir's
> third-run note in `/tmp/blaze-lanes/v2-codegen-mir.md` already takes them in. The
> `STATUS: BLOCKED` at the end of that report is from its second run. I read the third
> run's design and it agrees with these answers:
> - Threads switch only at terminators at nesting depth 1.
> - Cloning closure and tuple fields stays nested, and blocking there is an error.
> - Each unwound frame calls `ret`.
> - The panic payload goes through `__rust_start_panic`.

> safety, 20:05, on codegen-mir's note about the `MemKind::Machine` free, `Shadow::put` and the
> main thread's `at_thread_exit` destructors: the checker already handles all three. One
> small new request is S3 in `v2/requests/codegen.md`: split `MemKind::Mmap` out of
> `MemKind::Machine`, as Miri's `MiriMemoryKind::Mmap` is split out. The split comes with two
> exact one-line replacements in `hir-ty-ub/src/check.rs` that codegen-mir may make.
> `Kind::Mmap` and its test are in already. Nothing blocks S1 on it.
>
> codegen-rq-mir (2026-10-30), the failed compare-exchange (raised by the codegen lead): in
> `eval/shim.rs`, a compare-exchange now compares the latest store, read unchecked
> (`peek_memory`, as Miri does), and then tells the machine of one read. That is
> `Atomic { ord, rmw: true }` when it exchanges (then the write and `atomic_store`), and else
> `Atomic { ord: failure, rmw: true, cxchg_failed: true }` and no write. Before, a failed one was
> an RMW read at the success ordering and then the failed read. Under `Plain` it peeks once, as
> before. Tests in `tests/mir/machine.rs`: `a_failed_compare_exchange_is_one_read_at_the_failure_ordering`
> and `a_compare_exchange_that_exchanges_is_one_rmw`. The checker's `failed_cxchg` gets only
> that read now. The race-checker test is already there: `tests/ub/main.rs`
> `failed_cxchg_races_with_a_non_atomic_write`. Its verdict doesn't change, because the checker's
> race step ignores an RMW read (`(Read, true) => Ok(())`). The extra read only went against the
> `Access` contract and added a second borrow access. Not built.

## From speed-hir-store (2026-10-29): `unsafe.md` in its paths, and four parsers for safety-fuzz

Paths: `crates/hir-def/src/{hir.rs,hir/,expr_store.rs,expr_store/}`. Three `unsafe` blocks,
each with a `// SAFETY:` naming the invariant and where it is upheld; none is new in the
columns work:
- `hir/persist.rs` `syntax_context(raw)`: `SyntaxContext::from_u32` behind a safe function,
  the one place persisted contexts are trusted (the item tree's decode calls it too). It refuses
  `u32::MAX`; any other value is trusted because the store's `blaze_format::read_verified`
  checks the buffer's content hash before `read`.
- `expr_store/lower.rs` `module_path` (~L2812) and `collect_block_` (~L3089): `to_static` on
  salsa ids that don't outlive the query they were made in (the comments say why). This is
  the same lifetime erasure as speed-hir-nameres' section above, and the same fix applies
  wherever that one lands.
`hir-def` has `unsafe` in other modules (`lib.rs`, `nameres/`, `dyn_map.rs`, `visibility.rs`),
so `#![forbid(unsafe_code)]` on the crate is not possible yet.

Status (the `to_static` part): done by speed-unsafe-front-def (2026-10-30). Both
`expr_store/lower.rs` blocks are plain calls: `to_static` is a safe `fn` over salsa's `FromId`,
since a stale tracked id is never UB (see "## From speed-hir-nameres"). `dyn_map.rs`'s two
transmutes are gone too (each key's map is a `KeyMap` field, so indexing is a field access).
hir-def now says `#![deny(unsafe_code)]`, with `#[allow(unsafe_code)]` only on `item_tree::block`,
`hir::col` and `columns!`'s expansion. `hir::persist::syntax_context` has no `unsafe` left:
span's `SyntaxContext::from_u32` is a safe `fn` now (queue 00792). The missing fuzz targets for the other
hir-def readers (dependency image, reads, signatures, frozen def map) are queue 00801.

Parsers of untrusted bytes, in `expr_store/persist.rs`:
`<ExpressionStore as Persist>::read` then `ExpressionStoreRef::decode()`, `<Body as Persist>::read`
then `BodyRef::decode()`, `<ExpressionStoreSourceMap as Persist>::read` then `.decode()`, and
`<BodySourceMap as Persist>::read` then `.decode()`, each giving `Result<_, FormatError>`.
- Zero-copy: the `Ref` types read kinds, counts and the sorted source-key columns in place,
  bounds-checked by `Slice::follow`. `read` checks kind bytes, one data word per kind, the
  side-table offsets, and the source keys' strict order. `decode` copies each column once and
  runs `HirColumns::check` / `TypeColumns::check` (every id, list range and side-table index in
  bounds). A bad id that gets past them panics on use (slice indexing); it is never UB.
- **For a fuzz target**: decoding arbitrary bytes is safe, but the contexts it yields through
  `syntax_context` are only trusted because of the hash check. So a target that skips
  `read_verified` must not hand decoded `SyntaxContext`s (hygiene ids, spans) to a database:
  compare and drop them. Or it should fuzz behind `read_verified` and mutate only payloads
  whose hash it recomputes.
- Seeds: the encoded stores from `expr_store/tests/body.rs` (`EVERY_VARIANT`) and
  `tests/signatures.rs`, through `assert_store_round_trip`.

> forwarded to safety: its fuzz and audit children.

### From safety-race: answers to safety-check (2026-10-29)

- Added `Races::all_terminated(&self) -> bool` (Miri `have_all_terminated`: every thread, `main`
  included). Call `exit()` for `main` first, then check it.
- `rmw(&mut self, alloc, range, ord, site) -> Result<(), RaceError>` is right, called **once per
  successful RMW** for both halves (Miri `validate_atomic_rmw` does the load and the store
  clocks together): no separate atomic `read` before it. Then `store_value(alloc, range, ord,
  new_bytes, Some(old_bytes))`. A failed compare-exchange is `failed_cxchg` instead.
- Thread names: `thread_name(&self, ThreadId) -> Option<&str>` (Miri `get_thread_name`; `main`
  is `Some("main")`) and `thread_display_name(&self, ThreadId) -> String` (`unnamed-N`, which
  Miri's reports use; `<unnamed>` is only its `Debug` output). `RaceError` already carries
  both names in `RacingOp::thread_name`.

## From safety-tb (2026-10-29): `crate::borrows` is done; what `check` needs to call

For safety-check (`check.rs`, `check/valid.rs`); no change to the parent's files.

- **Per-thread frames.** `Borrows::set_thread(ThreadId)` picks the thread whose stack
  `push_frame`, `pop_frame` and function-entry retags act on (main until set). Miri keeps
  protected tags per frame of each thread, so frames of different threads return in any
  order. `check` must call it on every thread switch before any call, return or retag
  (`fail/tree_borrows/spurious_read.rs` needs it).
- **Read-only memory.** `NewPermission::writes()` says whether a retag of a non-empty range
  writes through the parent (implicit writes). Miri's `tb_reborrow` refuses that as
  `WriteToReadOnly` before the tree sees it, so `check` checks mutability first when
  `writes()` holds (`implicit_writes/static_memory_modification.rs`).
- `check/valid.rs::retag` should pass `with_implicit_writes(true)` under
  `-Zmiri-tree-borrows-implicit-writes`, except for `#[rustc_no_writable]` callees.
- `Borrows::gc_due()` / `GC_EVERY`: call `gc` when due with every tag memory and the
  evaluator hold.
- `cells` is `check`'s to compute as Miri's `visit_freeze_sensitive`: each `UnsafeCell`
  field's bytes, and a whole non-`Freeze` union or multi-variant enum.
- `RetagMode::Raw`/`None` and a `&mut`/`Box` to a `!Unpin` pointee are no-ops (`Ok(None)`).
  Accesses, frees and retags of a non-empty range of a dead allocation panic: memory checks
  liveness first.

> forwarded to safety: between its children.

### M1 update (safety-model, 19:2x): `blaze-check` depends on no workspace crate

`blaze-core` now depends on `blaze-ids` (fix-radix), which will have model tests, so
`blaze-check` dropped its `blaze-core` dependency (it keeps its own copy of SplitMix64). Its
dependencies are now `rustc-hash`, `libc` (Unix) and `windows-sys` (Windows): the `Cargo.lock`
entry of M1 lists those three. `crates/blaze-check/src/lib.rs` exists, so M1 1 (the workspace
dependency) can go in.

## From safety-fuzz (2026-10-29): `hot_lint.py` and the fuzzer

> forwarded to the coordinator: `tools/v2/hot_lint.py` and `crates/blaze-fuzz` (with `blaze-alloc-count` keeping the largest request) are its paths; on requests' coordinator list.

### F4. Coordinator (`tools/v2/hot_lint.py`): it crashes when given `"crates"`

`PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": ".", "crates": ["blaze-link"]}'`
raises `TypeError: typed_input: crates has a type it does not read: list[str]`. `typed_input`
has no reader for `list[str]` (a `tuple[str, ...]` field, or a `list` reader in typed_input,
would fix it). Without `"crates"` it runs over the whole `HOT` list. The lint also looks only
in `crates/<name>/src`, so `fuzz/` (package `blaze-fuzzer`) is never scanned. I ran its
`lint_file` on `fuzz/src/` directly: 72 findings. One was a real per-run allocation (the
mutator's `to_string`), now fixed. A list built only to be iterated is gone, and a `Display`
type replaces a `String` built per status line. The rest are on lines that are errors, command
line, setup or a kept input's file write, and those carry `// alloc:` or `// clone:` reasons.
`fuzz/src/` now has none left.

Asked: treat `fuzz/targets/` as test code, as `tests/` is (826 findings, almost all `vec![..]`
in the `seeds()` builders and in the targets' checks: re-encodings to compare and models,
which are the test's own work). The engine's hot path is `fuzz/src/` and is held to the
budget by a test (below).

> forwarded to the coordinator: `tools/v2/hot_lint.py` and `crates/blaze-fuzz` (with `blaze-alloc-count` keeping the largest request) are its paths; on requests' coordinator list.

### F5. Whoever owns `crates/blaze-fuzz`: one counting allocator

`blaze-fuzz` installs its own `#[global_allocator]` (largest request per thread), so a test
binary that links it (every `blaze-fuzzer` test, and the store's fuzz tests) can't also install
`blaze-alloc-count`'s: two global allocators don't link. `fuzz/src/tests.rs`
(`a_run_that_keeps_nothing_allocates_nothing`) uses `blaze_fuzz::largest_allocation`, which is 0
exactly when nothing was allocated. Asked: make `blaze-fuzz` count through `blaze-alloc-count`
(that crate keeping the largest request per thread too, beside `allocations` and `bytes`), so
one allocator serves both kinds of test.

> forwarded to the coordinator: `tools/v2/hot_lint.py` and `crates/blaze-fuzz` (with `blaze-alloc-count` keeping the largest request) are its paths; on requests' coordinator list.

## dedup: bits: hir-ty-ub's GC live set and leak check onto `blaze_ids::bits::BitSet`, `Bytes`' init bits kept

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits::BitSet` (`crates/blaze-ids/src/bits.rs`) is a `Vec<u64>` set: `reset(len)`
(empty, resized, buffer kept: `clear` then `resize`), `insert` (whether it was new),
`contains` (`false` past the words), `capacity`. Same members, same buffer kept between GCs,
same allocation per leak check. `Bytes::init` stays words: empty stands for every byte
initialized, and its words are blitted and extracted by `init_mask`'s range moves with the
allocation's mask, so only its copies of `words_for` and the bit arithmetic go. hir-ty-ub
depends on blaze-ids already.

1. `crates/hir-ty-ub/src/borrows.rs`: imports, before `use smallvec::SmallVec;` add
   `use blaze_ids::bits::BitSet;`.

   `Borrows`, before / after:

   ```rust
       /// The GC's live set, a bit per tag; kept between runs.
       live: Vec<u64>,
   ```

   ```rust
       /// The GC's live set, a bit per tag; kept between runs.
       live: BitSet,
   ```

   `Borrows::new`: `live: Vec::new(),` becomes `live: BitSet::default(),`.

   `Borrows::gc`, before:

   ```rust
           set.clear();
           set.resize((next_tag.get() as usize).div_ceil(64), 0);
           let mut mark = |tag: BorTag| {
               let n = tag.get() as usize;
               if let Some(word) = set.get_mut(n / 64) {
                   *word |= 1u64 << (n % 64);
               }
           };
   ```

   after (a tag past the words is left, as before):

   ```rust
           set.reset(next_tag.get() as usize);
           let mut mark = |tag: BorTag| {
               let n = tag.get() as usize;
               if n < set.capacity() {
                   set.insert(n);
               }
           };
   ```

   and before / after:

   ```rust
           let is_live = |tag: BorTag| {
               let n = tag.get() as usize;
               set.get(n / 64).is_some_and(|&word| word & (1u64 << (n % 64)) != 0)
           };
   ```

   ```rust
           let is_live = |tag: BorTag| set.contains(tag.get() as usize);
   ```

2. `crates/hir-ty-ub/src/memory.rs`: imports, before:

   ```rust
   use smallvec::SmallVec;
   ```

   after:

   ```rust
   use blaze_ids::bits::{BitSet, words_for};
   use smallvec::SmallVec;
   ```

   and `bytes::{Run, offset, words},` becomes `bytes::{Run, offset},`.

   `Memory::read`: `Some(false) => init.resize(words(len), 0),` becomes
   `Some(false) => init.resize(words_for(len), 0),` and the `None` arm's
   `init.resize(words(len), 0);` becomes `init.resize(words_for(len), 0);`.

   `Memory::leaks`, before / after:

   ```rust
           let mut reachable = vec![0u64; n.div_ceil(64)];
   ```

   ```rust
           let mut reachable = BitSet::<usize>::new(n);
   ```

   before:

   ```rust
               if reachable[i / 64] & (1 << (i % 64)) != 0 {
                   continue;
               }
               reachable[i / 64] |= 1 << (i % 64);
   ```

   after:

   ```rust
               if !reachable.insert(i) {
                   continue;
               }
   ```

   and in the `leaked` filter, `&& reachable[i / 64] & (1 << (i % 64)) == 0` becomes
   `&& !reachable.contains(i)`.

3. `crates/hir-ty-ub/src/memory/bytes.rs`: imports, before `use smallvec::SmallVec;` add
   `use blaze_ids::bits::{locate, words_for};`. Delete:

   ```rust
   #[inline]
   pub(super) fn words(len: usize) -> usize {
       len.div_ceil(64)
   }
   ```

   and every `words(` call in the file becomes `words_for(` (six: in `uninit`, `word`,
   `materialize`, `slice`, `put`, and `PartialEq::eq`).

   `Bytes::init`'s doc, before:

   ```rust
       /// Empty: every byte is initialized. Otherwise bit `i` is byte `i`, and bits past `len` are
       /// clear.
       pub(super) init: SmallVec<[u64; 1]>,
   ```

   after:

   ```rust
       /// Empty: every byte is initialized. Otherwise bit `i` is byte `i`, and bits past `len` are
       /// clear. Words, not a `blaze_ids::bits` set: empty is a state of its own, and the words
       /// move by `init_mask`'s blits, as an allocation's mask does.
       pub(super) init: SmallVec<[u64; 1]>,
   ```

   `Bytes::is_init`, before / after:

   ```rust
           i < self.len && (self.init.is_empty() || self.init[i / 64] & (1 << (i % 64)) != 0)
   ```

   ```rust
           i < self.len && (self.init.is_empty() || {
               let (word, mask) = locate(i);
               self.init[word] & mask != 0
           })
   ```

Status: open

> taken by requests (safety is DONE): child `requests-bits` is applying it (pass 17).

> applied: by `requests-bits` (pass 17), all three parts as written; the tree matched every
> before text. `borrows.rs`: `live: BitSet` (`BitSet::default()` in `new`), `gc` on
> `reset`/`capacity`/`insert`/`contains`. `memory.rs`: imports `BitSet, words_for`, drops
> `words` from the `bytes` import, `read`'s two resizes on `words_for`, `leaks`'s `reachable` a
> `BitSet::<usize>::new(n)` with `insert`/`contains`. `memory/bytes.rs`: imports
> `locate, words_for`, `fn words` deleted, its six callers on `words_for`, `init`'s doc
> extended, `is_init` on `locate`. `rg` finds no `bytes::words` or bare `words(` caller left in
> hir-ty-ub (the `words` closure in `check/tests.rs` is a local of its own). `hot_lint` (its
> `lint_file`): findings went from 5 to 4 in `borrows.rs` and 14 to 13 in `memory.rs` (the
> `Vec::new()` and `vec![..]` this removed), and stayed at 5 in `bytes.rs`; none of the rest is on a
> line this changed.

## From speed-unsafe-back (2026-10-30): one parser of untrusted bytes without a fuzz target

- **`blaze-vfs`'s image base.** `store::Base::check` (`crates/blaze-vfs/src/store.rs:494`), with
  `Base::within`, is all that stands between a restored image's node, entry, name, file and index
  columns and the zero-copy reads `Writer::restore` makes of them. Public entry:
  `blaze_vfs::Vfs::restore` (`crates/blaze-vfs/src/image.rs:196`), over an image whose `vfs`
  sections are the input. Invariants: never panics; an image `Base::check` passes, its root lists,
  reads and follows every path without a read outside the mapping; one it refuses is an error that
  names the column. Owner: vfs (done); `crates/blaze-vfs` is speed-unsafe-back's for unsafe.md, so
  a `#[cfg(feature = "fuzz")]` hook there, if the target needs one, can be asked of it.

> taken by requests (pass 46; vfs is DONE): child `requests-vfsfuzz` writes `fuzz/targets/vfs_image.rs` through `Vfs::write_image` and `Vfs::restore`; a hook inside `crates/blaze-vfs`, if one is needed, is asked of speed-unsafe-back. Its outcome line follows.

> applied (requests-vfsfuzz, child of requests): `fuzz/targets/vfs_image.rs`, not built. Public API only, no hook in `crates/blaze-vfs`. Input: up to 15 ops committed to `main` (file, executable, symlink, delete), then up to 32 damages (a tree header word, bytes of one of the ten columns, of the disks section, or anywhere in the tree section). Each run writes the image with `Vfs::write_image`, makes it durable and settles the journal's rewrite onto it, re-wraps the damaged sections in a fresh `Builder` so the CRCs pass and `Base::within`/`Base::check` is reached, maps it with `Mapping::copy` and calls `Vfs::restore`. It asserts: allocation within `blaze_fuzz::allowed(image + journal)`; a used image's heads list, read and follow with every file's bytes inside the mapping and no corrupt read; a refused one is `Malformed` with a reason, `Corrupt` of the tree section by bounds or alignment (never a CRC), or `Unnamed` (the last two only with header damage), and equals `Vfs::recover` of the journal in its report, heads and files; an undamaged image is `Used` as written and walks equal to the writer's tree. Plus `[[bin]] vfs_image` and `blaze-image.workspace = true` in `fuzz/Cargo.toml` (`write_image` takes a `blaze_image::Builder`), a `fuzz/README.md` row, and 4 seeds in `fuzz/corpus/vfs_image/`; `seeds()` builds 16 more (4 trees, each undamaged and with a column's count, a column's place and a name byte overwritten).

## speed-unsafe-back-run (2026-10-30): two fuzz targets for blaze-live's patch messages

blaze-live reads two messages another process writes, and had no fuzz target for either:
v1's patch (`format::Patch::decode`, the daemon to a program linking `blaze-live`, then the
checks `apply.rs` makes before anything is mapped) and the jump-table runtime's
(`patch::Message::view`, `Answer::decode`, `Descriptor::decode`). This lane made the readers
stricter and exposed v1's checks; the targets are yours (`fuzz/`). Not compiled.

In `crates/blaze-live` (done here): `format::Patch::decode` refuses bytes past the end ("N bytes
past the end of the patch"), as `Message::view` already did; a `fuzz` feature exposes
`blaze_live::fuzz_checks(code, process, slide, image, page) -> Result<(u64, [Result<(), String>; 3]), String>`,
the platforms' `apply` order (`in_image`, `Overwrites::plan`, `claim`, `slide_at` of the
pointers) then each platform's unwind check (`frame_run`, `fdes`, `functions`). The v2 switch's
checks against the image are reference-tested in `slots/tests.rs`
(`a_bad_patch_is_declined_before_anything_is_written`, and new
`data_outside_the_image_is_refused_before_any_write`); a target over `Switch::take` needs the
tests' fake image behind the same feature, which is a next step, not asked here.

1. `fuzz/Cargo.toml`: in `[dependencies]`, `blaze-live = { workspace = true, features = ["fuzz"] }`
   (with a comment like blaze-link's: "`fuzz_checks`, v1's checks of a decoded patch"), and
   ```toml
   [[bin]]
   name = "live_message"
   path = "targets/live_message.rs"

   [[bin]]
   name = "live_patch"
   path = "targets/live_patch.rs"
   ```
   in name order. `Cargo.lock` gains `"blaze-live"` in the fuzz package's dependencies.
2. `fuzz/targets/live_message.rs`:
   ```rust
   //! What the linker writes a jump-table program and the program answers, through
   //! blaze-live's `patch`: `Message::view` (the runtime reads every message in place with it),
   //! `Message::decode`, `Answer::decode` and `Descriptor::decode`, each given the whole input.
   //! The program trusts no byte of a message: it comes from another process.
   //!
   //! Invariants, each checked below (a violation panics, which the engine reports as a crash):
   //! - No reader panics, on any bytes, nor does walking every list of a view.
   //! - `view` allocates nothing for a message it accepts, and asks for at most
   //!   `blaze_fuzz::allowed(len)` bytes at once for one it refuses (its error).
   //! - Every write's bytes a view gives are bytes of the message.
   //! - `decode` is `view` then `to_message`, and a message either reads encodes back to
   //!   exactly the bytes read; so does an answer, and a descriptor its first `SIZE` bytes.

   #![forbid(unsafe_code)]

   use std::time::Duration;

   use blaze_fuzzer::Target;
   use blaze_live::patch::{Answer, DESCRIPTOR_MAGIC, Descriptor, Message, MessageView, Patch};

   const TARGET: Target = Target {
       name: "live_message",
       check,
       seeds,
       dictionary: &[
           b"BLZLMSG2",
           b"BLZLANS2",
           b"BLZLIVE2",
           b"\x01\x00\x00\x00",
           b"\x00\x00\x00\x00",
           b"\x02\x00\x00\x00",
           b"\xff\xff\xff\xff",
       ],
       max_len: 1 << 16,
       budget: Duration::from_secs(300),
   };

   /// Every entry of every list of `view`, checked to be the message's: how many there are.
   fn walk(message: &[u8], view: &MessageView<'_>) -> usize {
       let MessageView::Patch(patch) = *view else { return 0 };
       let inside = message.as_ptr_range();
       let writes = patch.code.len() + patch.data.len();
       let mut seen = 0;
       for (_, bytes) in patch.code.chain(patch.data) {
           let range = bytes.as_ptr_range();
           assert!(inside.start <= range.start && range.end <= inside.end, "a write's bytes are the message's");
           seen += 1;
       }
       assert_eq!(seen, writes, "a write list gives as many writes as it says");
       let lists = [patch.rebase().len(), patch.cells().len(), patch.frames().len(), patch.windows().len()];
       let walked = [patch.rebase().count(), patch.cells().count(), patch.frames().count(), patch.windows().count()];
       assert_eq!(walked, lists, "a list gives as many entries as it says");
       seen + walked.iter().sum::<usize>()
   }

   fn check(data: &[u8]) {
       let (most, viewed) = blaze_fuzz::largest_allocation(|| Message::view(data).map(|view| (walk(data, &view), view)));
       match &viewed {
           Ok(_) => assert_eq!(most, 0, "a message read in place allocates nothing"),
           Err(_) => assert!(most <= blaze_fuzz::allowed(data.len()), "refusing {} bytes asked for {most} at once", data.len()),
       }
       let decoded = Message::decode(data);
       match (&viewed, &decoded) {
           (Ok((_, view)), Ok(message)) => {
               assert_eq!(&view.to_message(), message, "decode is the view's message");
               assert_eq!(message.encode(), data, "a message read encodes to its bytes");
               assert_eq!(message.encoded_len(), data.len());
           }
           (Err(a), Err(b)) => assert_eq!(a, b, "decode refuses as view does"),
           _ => panic!("view and decode disagree: {:?} and {:?}", viewed.as_ref().map(|it| it.0), decoded),
       }
       if let Ok(answer) = Answer::decode(data) {
           assert_eq!(answer.encode(), data, "an answer read encodes to its bytes");
       }
       if let Some(descriptor) = Descriptor::decode(data) {
           assert_eq!(descriptor.encode(), data[..Descriptor::SIZE], "a descriptor read encodes to its bytes");
       }
   }

   /// Each kind of message, answer and descriptor as the linker and program write them, and the
   /// ways bytes are not one.
   fn seeds() -> Vec<Vec<u8>> {
       let patch = Patch {
           id: [7; 16],
           generation: 2,
           base: 1,
           code: vec![(0x4000, vec![0x90; 16]), (0x4010, Vec::new())],
           data: vec![(0x8000, 0x4000u64.to_le_bytes().to_vec())],
           rebase: vec![0x8000],
           cells: vec![(0, 0x4000), (3, 0x4008)],
           frames: vec![(0x4000, 16)],
           windows: vec![(0x4000, 0x4010, 0x4008)],
           functions: 2,
           statics: 1,
       };
       let messages = [
           Message::Patch(patch),
           Message::Patch(Patch::default()),
           Message::RollBack { id: [7; 16], to: 1 },
           Message::Restart { id: [7; 16], generation: 2, why: "a layout changed: ünï".into() },
       ];
       let answers = [
           Answer::Applied { generation: 2 },
           Answer::RolledBack { generation: 2, to: 1, why: "panicked".into() },
           Answer::Declined { generation: 3, why: String::new() },
       ];
       let descriptor = Descriptor {
           magic: DESCRIPTOR_MAGIC,
           version: 1,
           arch: 0,
           id: [7; 16],
           generation: 1,
           this: 0x1000,
           table: 0x2000,
           slots: 4,
           patch_code: [0x4000, 0x8000],
           patch_data: [0x8000, 0xc000],
           dir: [0x3000, 12],
       };
       let mut seeds: Vec<Vec<u8>> = messages.iter().map(Message::encode).collect();
       seeds.extend(answers.iter().map(Answer::encode));
       seeds.push(descriptor.encode());
       let whole = messages[0].encode();
       seeds.push(whole[..whole.len() - 1].to_vec());
       seeds.push([&whole[..], &[0]].concat());
       let mut unknown = messages[2].encode();
       unknown[12] = 9;
       seeds.push(unknown);
       seeds
   }

   #[cfg_attr(test, allow(dead_code))]
   fn main() -> std::process::ExitCode {
       blaze_fuzzer::main(&TARGET)
   }

   #[cfg(test)]
   mod tests {
       #[test]
       fn seeds() {
           blaze_fuzzer::seeds(&super::TARGET);
       }
   }
   ```
   (`unknown[12]` is the kind word, after the 8-byte magic and the 4-byte version.)
3. `fuzz/targets/live_patch.rs`:
   ```rust
   //! What the daemon writes a v1 program, through blaze-live's `format::Patch::decode`, then
   //! `blaze_live::fuzz_checks`, the checks each platform's `apply` makes of a decoded patch
   //! before it maps or writes anything. The program trusts no byte of a patch: it comes from
   //! another process. The checks run against a fixed image and room, at three slides.
   //!
   //! Invariants, each checked below (a violation panics, which the engine reports as a crash):
   //! - Neither panics, on any bytes.
   //! - `decode` asks for at most `blaze_fuzz::allowed(len)` bytes at once: no count is believed
   //!   before its bytes are there.
   //! - A patch `decode` accepts encodes to bytes of its `encoded_len` that decode to it again.
   //! - Code the checks accept is laid out in whole pages within the process's room, and every
   //!   write over the executable is inside the image.

   #![forbid(unsafe_code)]

   use std::{ops::Range, time::Duration};

   use blaze_fuzzer::Target;
   use blaze_live::format::{Body, Code, Patch, Process, Write};

   const PAGE: u64 = 1 << 14;
   const IMAGE: Range<u64> = 0x1_0000_0000..0x1_0100_0000;
   const ROOM: Range<u64> = 0x2_0000_0000..0x2_0400_0000;

   const TARGET: Target = Target {
       name: "live_patch",
       check,
       seeds,
       dictionary: &[
           b"BLAZELV1",
           b"\x00\x00\x00\x00\x01\x00\x00\x00",
           b"\x00\x00\x00\x00\x02\x00\x00\x00",
           b"\x00\x40\x00\x00\x00\x00\x00\x00",
           b"\x01\x00\x00\x00\x00\x00\x00\x00",
           b"\x00\x00\x00\x00\x00\x00\x00\x00",
       ],
       max_len: 1 << 17,
       budget: Duration::from_secs(300),
   };

   fn check(data: &[u8]) {
       let (most, decoded) = blaze_fuzz::largest_allocation(|| Patch::decode(data));
       assert!(most <= blaze_fuzz::allowed(data.len()), "decoding {} bytes asked for {most} at once", data.len());
       let Ok(patch) = decoded else { return };
       let bytes = patch.encode();
       assert_eq!(bytes.len(), patch.encoded_len(), "a patch's encoded_len is its encoding's");
       assert_eq!(Patch::decode(&bytes).as_ref(), Ok(&patch), "a decoded patch encodes to itself");
       let Body::Code(code) = &patch.body else { return };
       let process = Process { pid: 1, uuid: patch.uuid, started: 0, next: ROOM.start, end: ROOM.end };
       for slide in [0, PAGE, 0u64.wrapping_sub(PAGE)] {
           let Ok((end, _)) = blaze_live::fuzz_checks(code, process, slide, IMAGE, PAGE) else { continue };
           assert!(code.base >= ROOM.start && code.base <= end && end <= ROOM.end, "laid out in the room");
           assert!(code.base % PAGE == 0 && end % PAGE == 0, "laid out in whole pages");
           for write in &code.writes {
               let start = write.address.wrapping_add(slide);
               assert!(start >= IMAGE.start && start + write.bytes.len() as u64 <= IMAGE.end, "a write in the image");
           }
       }
   }

   /// Patches as the daemon writes them: one with code, unwind tables, pointers, writes over the
   /// image and a static's first value, one that only puts back, a restart; and the ways bytes
   /// are not one.
   fn seeds() -> Vec<Vec<u8>> {
       let mut text = vec![0u8; PAGE as usize];
       text[8..16].copy_from_slice(&IMAGE.start.to_le_bytes());
       let code = Code {
           base: ROOM.start,
           text,
           data: vec![1; 64],
           frames: (0, 4),
           pointers: vec![8],
           writes: vec![Write { address: IMAGE.start + 0x100, bytes: vec![0xe9, 0, 0, 0, 0], pointers: Vec::new() }],
           redirected: 1,
           statics: 1,
           lasting: vec![Write { address: ROOM.start + PAGE, bytes: vec![0; 8], pointers: vec![0] }],
       };
       let patches = [
           Patch { uuid: [7; 16], sequence: 1, body: Body::Code(code) },
           Patch { uuid: [7; 16], sequence: 2, body: Body::Code(Code { base: ROOM.start, ..Code::default() }) },
           Patch { uuid: [7; 16], sequence: 3, body: Body::Restart("a layout changed".into()) },
       ];
       let mut seeds: Vec<Vec<u8>> = patches.iter().map(Patch::encode).collect();
       let whole = patches[0].encode();
       seeds.push(whole[..whole.len() - 3].to_vec());
       seeds.push([&whole[..], &[0]].concat());
       seeds.push([&b"BLAZELV0"[..], &whole[8..]].concat());
       seeds
   }

   #[cfg_attr(test, allow(dead_code))]
   fn main() -> std::process::ExitCode {
       blaze_fuzzer::main(&TARGET)
   }

   #[cfg(test)]
   mod tests {
       #[test]
       fn seeds() {
           blaze_fuzzer::seeds(&super::TARGET);
       }
   }
   ```
   Corpus: the seeds as `.hex` in `fuzz/corpus/live_message/` and `fuzz/corpus/live_patch/`,
   as the other targets keep theirs.

Status: open

> taken by requests (pass 48; safety is DONE): child `requests-livefuzz` writes both targets, their `fuzz/Cargo.toml` and README entries and seeds. Its outcome line follows.

> applied (requests-livefuzz, child of requests): `fuzz/targets/live_message.rs` and `fuzz/targets/live_patch.rs` as the request wrote them, not built; `fuzz/Cargo.toml` gains `blaze-live = { workspace = true, features = ["fuzz"] }` (with its comment) and `[[bin]] live_message`/`live_patch` between `link_wasm_object` and `macro_wasm_host`; two `fuzz/README.md` rows. Changed from the request: the seed descriptor is `DESCRIPTOR_VERSION` (3) and `ARCH_X86_64` rather than literal 1 and 0; the descriptor comparison is with `&data[..Descriptor::SIZE]`; `live_message` also checks an answer's `encoded_len`; `live_patch` uses `is_multiple_of`, checks each write in the image with `checked_add` (as `in_image` does) instead of an unchecked `+`, and its first seed has two writes (a jump and a static's value with a pointer) so they match `redirected: 1, statics: 1`. Every other item (`Patch::default()`, `Code::default()`, the view's list methods, `WriteList`'s `ExactSizeIterator`, `Process`'s fields and `Copy`, `fuzz_checks`'s signature) is as blaze-live has it; nothing was asked of `crates/blaze-live`. Corpus, derived byte by byte from the encoders: 9 files in `fuzz/corpus/live_message/` (every message kind, a patch with every list non-empty, the three answers, a descriptor, an unknown kind) and 3 in `fuzz/corpus/live_patch/` (a restart, the same under magic `BLAZELV0`, code that only puts back). `live_patch`'s 16 KiB code patch and its truncated and extended copies, and `live_message`'s truncated and extended patch, are left to `seeds()`. `Cargo.lock`'s new edge stays with requests' list.

## From speed-unsafe-front-syntax (2026-10-30): a fuzz target for the lexer and parser of source text

Source text is untrusted (any file of any dependency), and `fuzz/` has no target for its lexer
and parser: `parser::LexedStr::new` (with an unsafe SIMD front, `crates/parser/src/lexed_str/scan.rs`)
and `syntax::SourceFile::parse` (outline parse, lazy bodies parsed on first walk).
`crates/syntax/fuzz/` is rust-analyzer's old cargo-fuzz package (`libfuzzer-sys`, its own
`[workspace]`, run by cargo and rustc), so it does not count; once this target lands it can be
deleted (tell frontend-tree, who holds `crates/syntax`). `parser` gained a `fuzz` feature for it
(done here, not compiled): `LexedStr::lexes_alike(edition, text) -> bool`, the fast path against
`rustc_lexer` alone, as the test `fast_and_rustc_lexer_lex_alike` compares them.

1. `fuzz/Cargo.toml`, `[dependencies]` in name order:
   `parser = { workspace = true, features = ["fuzz"] }` (comment: "`LexedStr::lexes_alike`, the
   fast lexer against `rustc_lexer` alone"); `syntax` is there already. And, in name order
   (after `proc_macro_tt_wire`):
   ```toml
   [[bin]]
   name = "syntax_parse"
   path = "targets/syntax_parse.rs"
   ```
2. `fuzz/targets/syntax_parse.rs`:
   ```rust
   //! Source text, through the lexer (`parser::LexedStr::new`, whose byte classes are made by
   //! vector code) and the parser (`syntax::SourceFile::parse`, an outline whose bodies are
   //! parsed when walked). The first byte picks the edition; the rest is the text, made UTF-8 by
   //! replacement as a file read as text is.
   //!
   //! Invariants, each checked below (a violation panics, which the engine reports as a crash):
   //! - neither panics nor hangs, on any text;
   //! - the tokens tile the text: each starts where the last ended, the last ends at its end,
   //!   and each one's text is the text's bytes there;
   //! - the fast path lexes as `rustc_lexer` alone (`LexedStr::lexes_alike`);
   //! - the tree is lossless: walking it (every lazy body parsed) gives back the text, and every
   //!   error is inside it; in the current edition its blocks are balanced
   //!   (`syntax::fuzz::check_parser`);
   //! - neither asks for more than `blaze_fuzz::allowed(len)` bytes at once.

   #![forbid(unsafe_code)]

   use std::time::Duration;

   use blaze_fuzzer::Target;
   use syntax::{Edition, LexedStr, SourceFile, TextRange, TextSize};

   const TARGET: Target = Target {
       name: "syntax_parse",
       check,
       seeds,
       dictionary: &[
           b"fn", b"let", b"match", b"impl", b"macro_rules!", b"r#", b"r\"", b"br\"", b"c\"",
           b"b'", b"'a", b"#\"", b"##", b"///", b"//!", b"/*", b"*/", b"#!", b"---\n", b"1e10",
           b"0x", b"\\u{", b"::", b"=>", b"..=", b"(", b")", b"{", b"}", b"[", b"]", b"<", b">",
           b"\xc2\x85", b"\xe2\x80\xa8",
       ],
       max_len: 1 << 16,
       budget: Duration::from_secs(300),
   };

   fn check(data: &[u8]) {
       let Some((&first, rest)) = data.split_first() else { return };
       let editions = Edition::iter().count();
       let edition = Edition::iter().nth(first as usize % editions).unwrap();
       let text = String::from_utf8_lossy(rest);
       let bound = blaze_fuzz::allowed(text.len());

       let (largest, lexed) = blaze_fuzz::largest_allocation(|| LexedStr::new(edition, &text));
       assert!(largest <= bound, "lexing {} bytes asked for {largest} at once", text.len());
       let mut end = 0;
       for i in 0..lexed.len() {
           let range = lexed.text_range(i);
           assert_eq!(range.start, end, "token {i} starts where the last ended");
           assert_eq!(lexed.text(i), &text[range.clone()], "token {i} is the text's bytes");
           end = range.end;
       }
       assert_eq!(end, text.len(), "the tokens end at the text's end");
       assert!(LexedStr::lexes_alike(edition, &text), "the fast path lexes as rustc_lexer alone");

       let (largest, parse) = blaze_fuzz::largest_allocation(|| SourceFile::parse(&text, edition));
       assert!(largest <= bound, "parsing {} bytes asked for {largest} at once", text.len());
       let whole = TextRange::up_to(TextSize::of(&*text));
       for error in parse.errors() {
           assert!(whole.contains_range(error.range()), "an error at {:?}", error.range());
       }
       assert_eq!(parse.syntax_node().to_string(), *text, "the tree gives back the text");
       if edition == Edition::CURRENT {
           syntax::fuzz::check_parser(&text);
       }
   }

   /// The parser's own test files (every token, every grammar entry, their error cases), and
   /// texts at the lexer's edges: deep nesting, a long string, a shebang and frontmatter, bytes
   /// that are not UTF-8, Unicode whitespace and identifiers.
   fn seeds() -> Vec<Vec<u8>> {
       let texts: &[&str] = &[
           "",
           "fn main() { let x: Vec<u8> = vec![1, 2]; match x[0] { 0..=9 => {} _ => () } }",
           "#!/usr/bin/env cargo\n---\n[package]\n---\nfn main() {}\n",
           "r#\"raw\"# br\"b\" c\"c\" cr#\"c\"# b'x' '\\u{1F600}' 'a 'r#a #\"guarded\"# ##",
           "/* /* nested */ */ /// doc\n//! inner\n/** block doc */ x",
           "1e10 0x1f_u8 1.0f32 1. 1..2 1.e3 0b2 \"unterminated",
           "fn f() { \u{2028} é名🦀 \u{85} }",
           "impl<T: ?Sized + 'static> Tr for &mut [T; N] where for<'a> T: 'a { type A = (); }",
       ];
       let mut seeds: Vec<Vec<u8>> =
           texts.iter().map(|it| [&[3u8][..], it.as_bytes()].concat()).collect();
       seeds.push([&[0u8][..], "(".repeat(5000).as_bytes()].concat());
       seeds.push([&[3u8][..], "{".repeat(3000).as_bytes(), "}".repeat(3000).as_bytes()].concat());
       seeds.push([&[3u8][..], b"fn f() { \"", "s".repeat(1 << 15).as_bytes(), b"\" }"].concat());
       seeds.push(vec![3, b'f', b'n', 0xff, 0xc0, b' ', 0xe2, 0x80]);
       let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/parser/test_data");
       let mut dirs = vec![dir];
       let mut files = Vec::new();
       while let Some(dir) = dirs.pop() {
           for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
               let path = entry.path();
               match path.is_dir() {
                   true => dirs.push(path),
                   false if path.extension().is_some_and(|it| it == "rs") => files.push(path),
                   false => {}
               }
           }
       }
       files.sort();
       for (n, path) in files.iter().enumerate() {
           let text = std::fs::read(path).unwrap();
           seeds.push([&[n as u8 % 4][..], &text].concat());
       }
       seeds
   }

   #[cfg_attr(test, allow(dead_code))]
   fn main() -> std::process::ExitCode {
       blaze_fuzzer::main(&TARGET)
   }

   #[cfg(test)]
   mod tests {
       #[test]
       fn seeds() {
           blaze_fuzzer::seeds(&super::TARGET);
       }
   }
   ```
3. `fuzz/README.md`, a row: `syntax_parse` | `parser::LexedStr::new` and `syntax::SourceFile::parse`
   (source text, any edition) | never panics or hangs; the tokens tile the text; the fast lexer
   lexes as `rustc_lexer` alone; the tree gives back the text and its errors are inside it;
   blocks balanced in the current edition; bounded.
4. Corpus: the seeds as `.hex` in `fuzz/corpus/syntax_parse/`, as the other targets keep theirs
   (the parser's test files stay in `seeds()`, read from the tree, not copied).

Model: the lexer's two paths are each other's model (`lexes_alike`), the text itself is the
tree's. Bounds: `max_len` 64 KiB, 300 s; an allocation at once of at most `allowed(len)`.
Nothing in syntax-tree is ever decoded from bytes (its trees are built from text only, and it
only `Serialize`s), so this is the one target for these three crates.

Status: open

> taken by requests (pass 56; safety is DONE): child `requests-syntaxfuzz` writes `fuzz/targets/syntax_parse.rs`, its `fuzz/Cargo.toml` and README entries and seeds. Its outcome line follows. Deleting `crates/syntax/fuzz/` once it lands is frontend-tree's (it holds `crates/syntax`).

> applied (requests-syntaxfuzz, child of requests): wrote `fuzz/targets/syntax_parse.rs` as asked, with the `parser` (`fuzz` feature) dependency and the `[[bin]] syntax_parse` in `fuzz/Cargo.toml`, a row in `fuzz/README.md` after `macro_wasm_lex`, and the 9 literal seeds (the 8 texts at edition byte 3, and the non-UTF-8 one) as `.hex` in `fuzz/corpus/syntax_parse/`. Every item it uses was checked against the tree and matches. Changes from the request: (1) errors come from `Parse::errors_with_bodies()`, not `errors()`, because `errors()` covers only the bodies already walked. Its allocation is bounded by `allowed(len)` too, since that is where the lazy bodies are parsed. (2) Losslessness is checked through the view, `parse.root().descendants_with_ranges()`, which enters lazy parts: each token starts where the last ended, its text is the text's bytes there, and the last ends at the text's end. This replaces `syntax_node().to_string()`, because new code may not use handles (frontend-tree's rule). (3) `seeds()` panics, naming the path, when `crates/parser/test_data` or a file in it can't be read; the request silently skipped it, which is a fallback. (4) `(n % 4) as u8` for the test files' edition byte. Nothing needed in `crates/parser` or `crates/syntax`. Not built.

> frontend-tree (2026-10-30 01:16): `crates/syntax/fuzz/` is deleted. Its `reparse` target had no successor, so it got one before the package went: `fuzz/targets/syntax_reparse.rs` (`[[bin]]` in `fuzz/Cargo.toml`, a README row, 8 hand-made edits in `fuzz/corpus/syntax_reparse/`, seeded with `crates/syntax/test_data/reparse/fuzz-failures` and an edit of each parser `ok` file). It runs `syntax::fuzz::CheckReparse` on its existing input format, which now also checks that the reparse and the full parse have as many nodes. `syntax_parse` and `syntax_reparse` share their seed-file walk in `fuzz/targets/syntax_shared.rs`. Not built.

## From speed-unsafe-front-intern (2026-10-30): a fuzz target for the interner's table section

An interner table section (`intern::restore_symbols`, `Mapped::restore`, both through
`crates/intern/src/table.rs` `Table::restore`) is read from a store image, so its bytes are
untrusted past the image's content hash. Its values are then read in place, and a symbol's
bytes go to `str::from_utf8_unchecked` (`symbol.rs` `mod text`). The checks are written:
blaze-image's views check bounds and alignment, and `Table::validate` checks the root, the
starts' order and range, each value against the values' end, UTF-8 for a text table, and the
probe groups and free list. Each failure is a `RestoreError::{Corrupt, Invalid { section, what,
at }}` naming the byte. `fuzz/` has no target for these.

1. `crates/intern` (speed-unsafe-front-intern's, or whoever holds it): a `fuzz` feature with
   `pub mod fuzz { pub fn restore(section: &[u8]) -> Result<Vec<(u32, Box<[u8]>)>, RestoreError>; pub fn section(values: &[&[u8]], freed: &[u32]) -> Vec<u8>; }`.
   `restore` uses a fixed small text shape (3 statics, like `table/tests.rs`'s `shape()`) and a
   fresh `Table`, not the process's `OnceLock`s: both public restores run once a process. On
   `Ok` it returns every `(id, bytes)` it can read. Two things in the hook keep a long run
   bounded. The image must not be leaked per input (`restore` takes `&'static Image`, so either
   an `Arc` borrow inside the hook or a `Table::restore` over a scoped image). Interning the
   statics claims from the never-freed 64 GiB `space`, a few hundred bytes per table, which
   should be fine for 10^8 inputs but is worth stating. Filed here first so the target and the
   hook land together; tell me if the target's lane would rather own the hook.
2. `fuzz/targets/intern_table.rs`, `#![forbid(unsafe_code)]`, `max_len` 64 KiB, 300 s.
   Invariants:
   - it never panics, on any bytes: an `Err` is fine;
   - on `Ok`, every ID below the table's end reads as a value or as freed;
   - each value is UTF-8, `find(bytes) == Some(id)`, and IDs are distinct per value;
   - writing the restored table again (every ID live) and restoring that gives the same pairs
     (the model is the table itself, round-tripped);
   - nothing asks for more than `blaze_fuzz::allowed(len)` bytes at once.
3. Seeds: `fuzz::section` of an empty table; one with the statics only; 200 seeded names (the
   tests' `name()` alphabet: `a`, `b`, `é`, `_`, `0`, so tags and order keys collide); one
   with freed IDs; one value of 64 KiB. Plus the two damaged sections the tests refuse (a start
   past the values, a `0xff` value). As `.hex` in `fuzz/corpus/intern_table/`, with a row in
   `fuzz/README.md`.

Status: open

> forwarded, item 1, to speed-unsafe-front-intern (it holds `crates/intern`; told by tmux). Items 2-3 taken by requests (pass 59; safety is DONE): child `requests-internfuzz` writes `fuzz/targets/intern_table.rs` against the hook's stated signature, its entries and seeds. Its outcome line follows.

> item 1 done by speed-unsafe-front-intern (2026-10-30), signature as stated. What is in place:
> - `crates/intern/Cargo.toml` has `[features] fuzz = []`, and `src/lib.rs` has
>   `#[cfg(feature = "fuzz")] #[allow(unsafe_code)] pub mod fuzz;`.
> - `src/fuzz.rs`: `restore(section)` adds the bytes as a `SectionId::SYMBOLS` section under
>   the hook's own schema, so the CRCs pass and the fuzzer reaches the structure checks. It
>   restores into a fresh table: shape `STATICS = ["a", "b", "a"]`, text, align 1, so new IDs
>   start at 4.
> - It returns every live `(id, bytes)` from ID 4 on, copied out, or the `RestoreError`.
> - It checks the round trip itself and panics if it fails: the restored table, written again
>   with every value live, must restore to the same pairs. So the target doesn't need its own
>   round trip; "never panics" covers it.
> - No leak: the image is freed once the table is dropped (one `Box::into_raw`/`from_raw` pair,
>   both commented), so a run's memory doesn't grow per input. Restoring claims no interner
>   space. `section(values, freed)` interns into a fresh table (a few hundred bytes of never-freed
>   space each), which is fine for seeds.
> - The target needs `intern = { workspace = true, features = ["fuzz"] }` in `fuzz/Cargo.toml`.
> - Two tests in `fuzz.rs`. Not built.

> applied, items 2-3 (requests-internfuzz, child of requests): `fuzz/targets/intern_table.rs` (`#![forbid(unsafe_code)]`, `max_len` 64 KiB, 300 s; every restore held to `blaze_fuzz::allowed(len)` by `largest_allocation`; `Err` only `Invalid` at a byte `<= len` or `Corrupt` by `Bounds`/`Misaligned`; on `Ok` IDs ascend below `end`, every static there, values UTF-8, `found` never another value's ID; `written` restores to the same `first`, `end` and values, its `found` the first ID holding each value, and writes the same bytes), the `seeds` test plus two (the seeds restore as written and check the copied `Root` offsets 0, 48, 64; the two damaged seeds are refused at the bytes `table/tests.rs` names); seeds in `seeds()`: no bytes, statics only, 200 `name()` names, `kept`/`dropped` with two freed, one 64 KiB value, a start past the values, a `0xff` value; `fuzz/Cargo.toml` `intern` with `features = ["fuzz"]` and `[[bin]] intern_table`; a `fuzz/README.md` row; `fuzz/corpus/intern_table/empty.hex` (the empty input; every other seed's bytes come only from `section`, so they live in `seeds()` alone). Assumes the hook as it now is: `restore -> Result<Restored { first, end, values, found, written }, RestoreError>` (asked in `v2/requests/speed-unsafe-front-intern.md`, applied) and `section(values, freed)`. `Cargo.lock` needs `intern` among `blaze-fuzzer`'s dependencies (coordinator). Not built.
