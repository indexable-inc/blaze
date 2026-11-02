---
status: plan
summary: lane codegen's speed of light: for each component, the bound its work and the hardware set, the arithmetic, how the design reaches 90% of it, and what stands in the way where it does not yet.
date: 2026-10-29
---
# Lane codegen: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each part of lane codegen has a section here,
written by the agent that owns it; a sibling adds its own section below rather than rewriting
the file. The thread shims have theirs in [speed-mir-threads.md](speed-mir-threads.md).

Figures are for one core of the target machines (EPYC Turin, Graviton 5), from
[hardware.md](../../design/hardware.md), with the same constants as
[frontend's](../frontend/speed-of-light.md):
- about 3.5 GHz;
- an L1 hit in 4 to 5 cycles, L2 in about 14;
- streaming from L2 at about 32 bytes a cycle, DRAM at 20 to 30 GB/s;
- an IPC of about 2 for branchy code and about 4 for straight-line integer code;
- a mispredicted branch costs about 15 cycles;
- a hash-map hit (FxHash, one probe, `RefCell` borrow flag) about 20 to 30 cycles;
- a small `malloc` with its `free` about 40 to 80 cycles.

Nothing has been measured: nothing may compile until every lane is done. `proof`'s budgets
measure each bound once it does. Instruction counts are estimates from the code as written.

## codegen-mir: `crates/hir-ty-mir` (MIR building, passes, monomorphization, the evaluator)

Owner: codegen-mir. The inliner (`optimize/inline*`) is codegen-inline's and not counted here.

### 1. The evaluator, per statement (`eval.rs`, `Machine = Plain`)

What it runs: const eval, `#[test]`s and build-time evaluation, so this is the bound of every
program blaze interprets.

**Work.** Take `_3 = Add(_1, _2)` on `u64`, the common case: read 16 bytes, add, write 8.
- **Bytes.** 24 bytes of frame, plus the statement's encoding (16 bytes when decoded), all in
  L1 in a running loop: under 1 cycle.
- **Instructions, the minimum.** Fetch the opcode and dispatch (2, plus an indirect jump), two
  frame-relative loads (base plus an immediate offset: 2), the add (1), the store (1), advance
  (1): about 8. The indirect jump predicts well under threaded dispatch, about 1 in 10 missed
  over mixed code: 1.5 cycles. At an IPC near 2, **the bound is about 5 to 6 cycles, 1.6 ns a
  statement.** A `SwitchInt` or `Goto` costs the same; an overflow-checked add is two
  statements (`CheckedBinaryOp` and `Assert`), about 11 cycles.

**Where the code is.** Per such statement, estimated:

| Step | Cycles |
| --- | --- |
| `place_addr` of the destination (arena index, projection loop) | 15 |
| two `eval_operand` → `eval_place` → `place_addr_and_ty_and_metadata` | 30 |
| `operand_ty` (`place_ty`, a `projected_ty_cache` probe per projection) | 10 to 30 |
| `size_align_of_sized` of the operand type (`layout_cache` probe) | 25 |
| two `read_scalar` → `peek_memory` (address-space match, thread-region math, bounds) | 30 |
| the arithmetic (`pad16`, match on op and type, widening through `u128`) | 40 |
| the result as `IntervalOrOwned::Owned(Vec<u8>)`: `malloc` and `free` | 40 to 80 |
| `copy_from_interval_or_owned` into the destination | 20 |
| `steps_left`, `exec_statement`'s match, `Result` plumbing | 15 |
| **total** | **about 230 to 290** |

That is **about 2% of the bound**. A terminator adds `pick_at_terminator` (a branch while one
thread runs). A call adds the callee lookup (`mir_or_dyn_index_cache`, one probe), the frame's
layout (`create_locals_for_body` sizes every local through `size_align_of`, one probe each),
and copying the arguments: hundreds of cycles plus 25 per local, against a bound of about 20
cycles plus the bytes of the arguments.

**What stands in the way.** The evaluator walks typed MIR and works out on every execution
what is fixed once the body is monomorphized: each place's type, size and offset, each
operand's scalar kind, the callee of each direct call, and the frame's layout. Values in flight
are heap `Vec`s. None of that depends on the values, so it is all removable.

**The design that reaches 90%: decoded bodies.**
- **Decode once per instance.** The first time a `(body, generic args)` runs, it is decoded to
  an `EvalBody`: a frame layout (every local's offset and size, computed once), and a flat
  `Vec<Inst>` of 16-byte instructions with everything resolved: frame offsets, sizes, the
  scalar kind (width, signedness, float), constant bytes already placed in the heap, and each
  direct call's callee as an index into the evaluator's table of decoded bodies. A place with
  projections decodes to a short chain of steps (add an immediate, load a pointer, scale an
  index), each fixed at decode time. Decoding costs about one walk of the body, paid once per
  instance per evaluator (each `const` evaluates once, so this is the bound of the decode).
- **No values on the heap.** Every instruction reads its operands where they live and writes
  its result straight to its destination; `IntervalOrOwned::Owned` goes from the hot path.
- **Frame-relative fast paths.** A frame operand is `stack[base + offset]` in the running
  thread's region, with no address-space decoding; pointers keep the packed `Address` and its
  decoding only behind a dereference.
- **Dispatch.** A dense `match` on the opcode in one loop (a jump table) first. If dispatch
  mispredicts dominate when measured, threaded dispatch with explicit tail calls (`become`,
  available on blaze's own nightly) and superinstructions for the common pairs
  (`CheckedBinaryOp` + `Assert`, `Discriminant` + `SwitchInt`).
- **Hooks.** `M::CHECKS` already compiles every hook out for `Plain`, the `RefCell` that
  holds the machine (`eval/hooks.rs`) included: no borrow flag is read in const eval. Under a
  checker, an instruction carries the type ids the hooks need (`typed` still takes the `Ty`),
  so the checker's cost is its own work and the evaluator adds under 10% to it; the borrow
  flag is one load, compare and store per hook (about 1 cycle), against the checker's tens to
  hundreds of cycles per access.
- **One path.** The decoded form replaces the typed walk; there is no second, slower
  interpreter kept beside it (rule no-fallback). The generic-placeholder case
  (`assert_placeholder_ty_is_unused`, which takes `(0, 1)` as the size) decodes with that
  size.

Estimated after it: about 8 instructions plus dispatch, **7 cycles a simple statement, about
80% of the bound**; with superinstructions and threaded dispatch, **6 cycles, about 90%**.
Calls: the frame is a bump of the thread's stack by the decoded frame size and a copy of the
arguments, about 25 cycles plus the bytes.

**State of the art.** The fastest interpreters of a typed bytecode run a simple operation in a
few cycles: wasm3's threaded dispatch and LuaJIT's interpreter. The comparable checker of Rust
is Miri, which walks MIR and re-derives types and layouts as this evaluator does today, at
hundreds of cycles a statement or more. The decoded design is wasm3's design on MIR; it beats Miri for the same
reason. Where it does not match wasm3 is the checker's hooks, which are the checker's work.

**Done now.** An `Assert` no longer asks the database whether the body checks overflow unless
the assertion has failed (`checks_overflow` was two database lookups on every checked
operation, an estimated 100 to 200 cycles each time). The decoded-body rewrite is the next change in this
component; it replaces `eval_place`, `eval_operand`, `eval_rvalue` and `create_locals_for_body`
as a whole, so it lands in one piece once the crate compiles, with the differential tests
against rustc (`tests/mir`) and the shim tests as its proof.

### 2. Unwinding and threads in the evaluator (`eval/unwind.rs`, `eval/threads.rs`)

- **Unwinding.** The work is the cleanup blocks that run, plus one step per frame left. Each
  frame is popped once and its cleanup block is a block like any other; the payload is moved,
  not copied. There are no unwind tables to search. **At the bound** (the cleanup code itself),
  once the cleanup blocks run decoded as in section 1.
- **Thread switch.** The work: park the running thread's stack region and unpark the next
  one's, and the machine's `schedule`. Regions are separate `Vec`s that are swapped, not
  copied, so a switch is a few pointer moves and the scheduler call: about 20 cycles, **at the
  bound**. What is off it: `pick_at_terminator` builds the list of runnable threads (a `Vec`)
  at every terminator while more than one thread runs, about 50 cycles a block. A runnable set
  kept up to date on block and wake (a bitset of at most 240 threads, four words) removes it.
- **Shims.** In [speed-mir-threads.md](speed-mir-threads.md).

### 3. MIR building (`lower.rs`, `lower/`)

**Work.** Per HIR expression, about 1.5 MIR statements and 0.7 locals come out.
- **Bytes.** Read the expression's view (about 16 bytes), its type and adjustments from
  inference (8 to 16), write about 1.5 × 40 bytes of statements and a 16-byte local: about 100
  bytes, 3 cycles from L2.
- **Instructions, the minimum.** Dispatch on the expression (3, plus a mispredict 1 in 4:
  4 cycles), fetch the type and adjustments (4), build and push the statements (about 20 each,
  30), the local (10): about 50 at an IPC of 2. **Bound: about 30 cycles, 9 ns an expression.**

**Where the code is**, estimated per expression: the type fetch is an arena index (cheap);
`expr_adjustments.get` is a hash probe (25); every temporary's type is interned again with
`.store()` (a probe of the type interner, 50 to 100); an operand list is a boxed slice (a
`malloc`, 50); drop-scope bookkeeping (20); and a path expression resolves its name again
through the resolver (`update_to_inner_scope` and `resolve_path_in_value_ns_fully`, several
def-map probes, 200 to 500), though inference resolved it already. **About 200 to 400 cycles:
8 to 15% of the bound.**

**What stands in the way, and what removes it.**
- Name resolution done twice: inference records what each path resolves to (it has
  `assoc_resolutions` and `variant_resolution` already); recording the value-namespace result
  for every path expression in `InferenceResult` lets lowering read one column. That is
  `hir-ty`'s; I will ask db-ty for it in `v2/requests/codegen-mir.md`.
- Types interned again: locals keep the stored type inference already holds (`StoredTy` is an
  interned id; a `Ty` → `StoredTy` map per body, filled once, makes `.store()` an index).
- Boxed operand lists: one arena per body for operands, freed with the body.
- Hash probes for adjustments: an adjustments column indexed by `ExprId`, as the HIR columns
  are (hir-ty's, same request).

With those, about 60 to 70 cycles: **about 45% of the bound**; the remaining gap is the
statement representation (an enum of up to 48 bytes with boxed parts), which the decoded form
of section 1 and codegen's lowering both read. Making MIR a column store like the HIR's is the
step to 90%; it is lane-wide (every reader of MIR changes), so I propose it in the codegen
plan rather than doing it alone.

**The edit bound.** MIR building is a salsa query per body: an edit rebuilds only the bodies
whose HIR or inference changed. That is at the bound for invalidation at body granularity.

### 4. MIR passes (`optimize.rs`)

**Work.** 25 passes (`run!` in `optimize`), each a walk over every statement and terminator;
rustc's order, which is semantics (rule superset-of-rust: what a pass leaves decides what
codegen and the lints see). Per statement per pass, about 20 to 40 cycles: **about 500 to 1000
cycles a statement** at release, against about 4 passes' worth (simplify-cfg, the dataflow
passes, and one cleanup walk) if independent local passes were fused: **about 100 to 150
cycles, so 15 to 25% of the bound.**

**What stands in the way.** Fusing passes changes nothing only where they commute; each pair
has to be shown to (the differential tests against rustc's MIR at each level are that proof).
The local, statement-at-a-time passes (`RemoveStorageMarkers`, `RemoveZsts`,
`RemoveNoopLandingPads`, the copy-prop cleanups) fuse into one walk; the dataflow passes stay
separate. Until that is measured, the passes run once per body per edit (they are a salsa
query), so their cost is paid on changed bodies only.

### 5. Monomorphization (`monomorphization.rs`)

**Work.** Copy the body (memory-bound: body bytes over L2 bandwidth, about 0.1 cycle a byte)
and substitute the generic arguments in every type, constant and argument list.
- **Bound.** Each distinct type of the body folded once: a body has far fewer distinct types
  than type occurrences (often 10 to 20 against hundreds), and a fold is an interner probe
  (50 to 100 cycles). So about 20 × 75 = 1500 cycles plus the copy for a body of a few hundred
  occurrences.
- **Where the code is.** `fill_ty`, `fill_const` and `fill_args` fold every occurrence
  (`try_fold_with` then `.store()`): hundreds × 75 to 150 cycles, **about 10 to 20% of the
  bound** on a typical body.
- **What removes it.** A per-instance memo from the stored type to its substitution (an
  `FxHashMap<StoredTy, StoredTy>`, 25 cycles a hit), so each distinct type folds once: **about
  80 to 90%**, the rest being the copy. This one is small and local; it is next after the
  decoded evaluator.

STATUS of this section: the bounds are written; one gap closed (the `Assert` overflow query);
the evaluator's decoded bodies, monomorphization's memo and the runnable bitset are the next
changes, in that order; MIR lowering's two gaps are requests to hir-ty.

## codegen-api-roots: a unit's roots (`crates/hir-ty-codegen/v2/roots.rs`, `v2/roots/`)

**Work.** Once a unit a build: choose what it starts from. For a program, the C `main`, the
allocator shim and every linked crate's kept symbols; for a library, what another crate names;
for a test harness, the tests, listed and checked against the harness in the root file, and
for each whether it reaches a process, the network or assembly.

- **Kept symbols.** A walk of every item of a crate, bodies' block scopes included, which is
  work the front end did to build the def maps. **Bound:** a probe of the attribute flags per
  item (about 20 cycles), done once a crate a revision: `kept` is a salsa query, so a warm
  build of a program over the standard library reads a memo per crate, about 50 ns each.
  **Where the code is:** at the bound; the cold walk is one pass, and the attribute flags are
  hir-def's own memo.
- **The test listing.** One pass over the crate's modules (the walk above) plus, per test,
  its attributes' syntax and the line of its name. **Bound:** the tests' count times a
  syntax-tree lookup (about 1 µs), so 1 ms for a thousand tests. **Where the code is:**
  at the bound for the read; it runs twice a unit (`harness` writes the text, `roots` checks
  its digest), both on the same memoized syntax, so within 2× of it. Gap: a salsa query of
  the listing would make a warm unchanged crate free, but not the second listing of a cold
  build: writing the harness changes the root file, so `roots` lists on a new revision, and
  that listing is the check that the harness is the crate's (sol-all, 2026-10-30). A query per
  module would leave only the root module listed twice. Either needs the test's file name from
  the database: today it comes from `HirFrontend::hashes`, which a query can't read.
- **`outside`.** Tarjan's over the program's reference graph, one visit a function or static
  and one hash probe an edge. Each node's references are `refs::walked`'s memo, the same walk
  the build does. **Bound:** the edges' count times a probe (about 25 cycles): for 50 000
  instances of 10 references, about 5 ms on one core. **Where the code is:** the walk is
  linear, but it copies each node's references (`into_owned`, an allocation a node) and keys
  its map by the instance (a hash of its arguments a probe), about 2 to 3× the bound; and a
  static's references come from evaluating it, which the build does too. **What removes it:**
  the build's own walk answering it (`Built::reached` with the classifier as it places a
  symbol), which makes it free: the walk is paid once. That needs the driver's reached set
  (codegen-api-link's), so it is the next change after it lands.
  **Update (sol-all, 2026-10-30, estimate, not measured):** the walk now keeps each function's
  references as the memo's slice and keys its map by the reference itself, so it copies a
  node's key once (its insert) and nothing on an edge. That leaves the hash of an instance per
  probe, about 1.5× the bound. `Built::reach` has landed, but `roots` answers `outside` before
  the build runs, so folding it in reorders roots and build: a change to the driver's API, for
  codegen-api-link.

STATUS of this section: the bounds are written; kept symbols are at the bound, the listing
within 2×, `outside` about 1.5× until it folds into the build's walk.

## codegen-fc-lazy: a lazy build (`crates/blaze-codegen/src/driver/later.rs`)

A lazy build (`Build::run_later`) walks what a whole build walks, compiles its statics, its
entry and what start sections name, and checks every other function; `Build::compile_one`
compiles one on its first call. The walk is the whole build's and is counted in its section;
what follows is the work a lazy build adds or saves.

- **The check, warm** (the store has the function's template): one store probe and one
  decode of the template's lists in place, no copy and no instantiation. **Bound:** a probe of
  the mapped store is about 100 ns (a hash, an index read, a page that is usually resident);
  the decode streams the template at about 32 bytes a cycle, about 100 ns for a 1 KB
  template. For 50 000 functions: 5 ms of probes, and 5 ms of decoding spread over all cores
  (about 0.2 ms on 32). A whole warm build instead instantiates every function (a copy of its
  code, relocations and lines, 1 to 2 µs each, 50 to 100 ms), so the lazy check is 10 to 20×
  less. **Where the code is:** the decode is on all cores; the probes are on the calling
  thread, as `Lookup` says the store is read, so they are the floor of the check. **What
  closes it:** a `Lookup` that is `Sync` (`blaze_cas::Reader` reads take no lock) would put
  the probes on all cores too; that is a change to `driver.rs`'s trait, the parent's.
- **The check, cold** (no template): the function's IR is built (its MIR monomorphized and
  lowered, its layouts), on all cores, and let go on the thread that built it; 64 bytes of it
  are kept (its key and shape). **Bound:** the front end's IR build of each reached function,
  which is what makes a lazy build fail where a whole build fails. Emission (instruction
  selection, register allocation and encoding, about two thirds of a dev compile) is not
  paid until a first call. **Where the code is:** at the bound: one prepare per function, no
  emit, no IR held.
- **`compile_one`.** A store hit is one probe and one instantiation, bound by copying the
  function (about 50 ns for 1 KB). A function whose check built its IR finds the memo's
  template by the shape it kept, one probe and one instantiation, with no IR built again. A
  miss is one prepare and one emit, the backend's bound. Nothing is locked: the build is read
  only, and the memo is lock-free. **Gap:** the kept shapes are a hash map, one probe a first
  call; a column indexed by the function's place in the walk would be a read, which needs the
  lazy runner to pass that place (its `LazyId` follows the walk's order). Small.

STATUS of this section: the check and `compile_one` are at their bounds, except that the
check's store probes run on one thread, as `Lookup` requires.

## codegen-fc-built: owners and the execution table (`crates/blaze-codegen/src/driver/owners.rs`, `exec.rs`)

Every build now says whose each instance is (`Built::owners`, `static_owners`) and what it is
across versions (`Built::versions`); a test binary's link adds its table (`ExecTable::new`).
Counts are for 100 000 functions with 10 references each, not measured (not built).

- **Owners and the check.** Two copies of the walk's key-ordered lists and, per reference to
  a function or static, a binary search of them for its owner: 1 M searches of 17 compares of
  32-byte keys, about 10 ms on one thread. **Bound:** the walk already resolves each reference
  to its node by a hash; keeping that node's place there would make the owner a read, about
  1 ms. **Where the code is:** the searches, after the walk and before any compile, on the
  walking thread. **What closes it:** an owner column in `Sorted`, filled by the walk, read by
  place; it changes the walk's node, so it waits on the lanes that share `driver.rs`.
- **Versions.** Per instance one `fn_id` hash (64 bytes) and one `exec::bound` hash of its
  references' item keys (a hash lookup each, about 40 bytes hashed each): about 1 M lookups and
  40 MB hashed, 10 to 20 ms on one thread, paid by every build though only a test binary's
  table reads it. **Bound:** none for a build that is no test binary. **What closes it:**
  `Build` told whether the unit is a test binary (a `Policy` field), or the versions made by
  `ExecTable::new` from the nodes, which `Built` does not keep. The per-instance parts are
  independent, so a parallel map would take them to under 1 ms on 32 cores.
- **`ExecTable::new`.** One sort of the functions by `FnId` (100 000 × 17 compares, about
  3 ms), one slot lookup per function (the link's, a hash each), and per test one walk of its
  reach (`Reach::functions_of`, at its bound: a bit per node, cleared by the walk that set it)
  mapped to table places and sorted, by a sort when it is under a quarter of the table's
  words and by a reused bit per function otherwise. **Bound:** the sort is what
  `blaze_testsel::Image::new` would do and now checks in one pass (fastcompile-sol-select 2);
  the walks are the floor, as each test's reach is its own.

STATUS of this section: `ExecTable::new` at its bound; owners and versions each about 10× off
theirs, with the fixes above, and both on one thread.

**Update (sol-all, 2026-10-30; by count, not measured).** Both now run on every core.
`owners::owners` collects the dependency's functions and statics (a key pair and a slice each,
about 3 MB for 100 000) and checks their references in parallel, the refusals collected in key
order as before (`refusals_across_cores_come_in_key_order`). On 16 cores the 10 ms of searches is
about 0.7 ms plus the collect, **about 0.8 ms against the 1 ms one-thread bound**, at it without
the owner column. `versions` is a parallel map over the nodes in key order: 10 to 20 ms on one
thread is about 1 ms on 16, which is the bound of a test binary (its hashing spread across the
cores). A build that is no test binary still pays that 1 ms against a bound of 0. Closing it
needs `Build` to know it is one (a `Policy` field, about 40 constructors in 15 files, most of
them other lanes' tests), or `ExecTable::new` making the versions from nodes `Built` keeps.

## codegen-rq-lower: a function's unwind entry and its variables' places (`crates/hir-ty-codegen/unwind.rs`, `debuginfo.rs`)

Owner: codegen-rq-lower.

**The FDE of a Cranelift function.** v2 keeps, per function, the call frame instructions
of its `.eh_frame` FDE after a CIE shared by the ISA (`v2/unwind.rs` `Common`). The bound is
the bytes themselves. A dev function saves 0 to 12 registers, so it has 2 to 14 rows: on
x86-64, `push rbp` (a CFA offset and a register rule), `mov rbp, rsp` (a CFA register) and
one offset per callee-saved register; AArch64 adds the link register and, when signing, a
`RaSignState`. Each row is 1 to 3 bytes, padded to 8, so about 8 to 32 bytes. At about one
byte every 2 cycles (branchy, a LEB128 per operand), that is 20 to 70 cycles, or 10 to 20 ns,
plus the one allocation of the output (40 to 80 cycles). Before, each function cloned the
CIE (its own `Vec` of instructions), registered it and the FDE in a gimli `FrameTable`
(an `IndexSet`, a `Vec` of FDEs and an atomic `BaseId`), wrote the whole `.eh_frame` into an
`EndianVec`, parsed it back and copied out both halves. That is about six allocations and
a few hundred bytes written and read, so roughly 1,500 to 3,000 cycles.

How the design reaches the bound:
- `FunctionUnwind::of` makes the rows once, at compile time, from the `(offset, UnwindInst)`
  pairs Cranelift's buffer already holds (`SystemV::rows`, a port of Cranelift's
  `create_unwind_info_from_insts`). That is one pass and one `Vec` of rows, which the
  function's `UnwindInfo` made anyway.
- `unwind::Fde::write` encodes them straight into the output with gimli's encoding (checked
  byte for byte), with no table, no CIE and no parse.
- What is left over the bound is the output `Vec` and a `Vec` of rows per function. Writing
  into a buffer the caller keeps would remove the first, if `Unwind::Dwarf::fde` borrowed.

What stands in the way:
- `Unwind::Dwarf::cie` is a `Vec<u8>` per function, a clone of about 20 bytes and one
  allocation. It is the parent's to make a `ContentId` or carry once per build.
- Other architectures (riscv64, s390x) still go through gimli (`Frame::Cranelift`).
- A benchmark should confirm, per function of the dev path, the FDE in under 100 ns and one
  allocation, against the convert time before.

**Where a named variable lives** (`SlotPlaces::of`). The side table asks once per named
local which place its stack slot has. With \(m\) slots and \(n\) locals, the bound is reading
each local once, plus a probe of \(\log_2 m\) compares, which is under 10 for any function's
slot count, at about 2 cycles each. The linear `find` it replaces cost \(O(nm)\): for a
function with 1,000 slots and as many locals, about 500,000 compares, or about 0.3 ms. Now
it is about 10,000. `SlotPlaces::new` sorts once, a stable \(O(m \log m)\) sort, and skips
the sort when the slots are already in order. A benchmark should confirm that
`describe_variables` stays linear on a function with thousands of named locals.

## codegen-rq-features: target-feature sets (`crates/hir-ty-codegen/target.rs`, `llvm.rs`, `v2/backend.rs`)

Owner: codegen-rq-features.

**Numbering a set** (`Target::features_of`, `Features`). A set is a `u8`. The bound for
finding a set already named is one compare per set named so far, a few in any process (a
dev job's host set, a release job's baseline, one per distinct `#[target_feature]` set), each
an early-out slice compare of about 5 to 15 short strings, with no lock and no allocation.
The design is at that bound: `FEATURE_SETS` is a fixed array of `OnceLock`s, scanned
lock-free. The lock is taken only to add a set, once per set per process. Checking the
names (`cranelift_setting`, an 18-entry scan each) adds about 10 × 18 compares, or about
100 ns, per prepared function. That is under 1% of a prepare (10 to 100 µs). Numbering the
unit's set once per unit would remove it, but the spec is not carried from `check_spec` to
`prepared`.

**A function's set** (`v2/backend.rs` `features`). For a function with no
`#[target_feature]` the work is the salsa hit of `TargetFeatures::from_fn` (about 30
cycles) and the numbering above. For one with it, there is one `Vec` of the unit's names plus
what it enables, sorted. That is a handful of allocations, only for such functions.

**The code generator of a set.** Cranelift: `target_of` indexes a static
`[[[OnceLock; 3]; 2]; 128]` by set, level and kind, which is one load and a check. The ISA is
made once per set and kind. LLVM: a thread's machines are keyed `(level, pgo, set)` in one
`FxHashMap` (one more byte hashed). LLVM's feature string for a set is made once
(`llvm::told`, a static array of `OnceLock<Box<str>>`), and each function gets it as one
string attribute, interned by LLVM, which costs nothing next to LLVM's compile. Names LLVM
lacks are checked once per machine made, not per function.

**The shape.** 1 + 4 bytes for a set with no names, plus 8 + the length per name. Hashing
that is far under a nanosecond per byte.

## codegen-fc-sol: stopping, the memo probe and the pieces' digests (`crates/blaze-codegen/src/driver.rs`, `driver/later.rs`, `driver/digests.rs`)

Not measured (not built). Counts are for 100 000 functions.

- **Stopping per function** (`Build::until_stopped`). Every per-function loop (prepare,
  summarize, emit, merge, static compile, the lazy check and the lazy entry) asks
  `Frontend::stopped` before each item: a relaxed load of the job's flag and, with a body
  check beside the build, one indirect call, together 1 to 2 ns against a function's 50 to
  200 µs (0.002%). **Bound:** a design that never stops code mid-function gives a thread back
  when the function it is in ends, so the latency is one function a core: 50 to 200 µs for a
  dev compile. **Where the code is:** at the bound; nothing is begun once the flag is set, and
  the one `stopped` error is made once per loop, not per item left.
- **`Build::memo_hit`.** Three probes (the walk's node, the recorded shape, the lock-free
  memo), about 20 ns each, then the instantiation (a copy of the function, about 50 ns for
  1 KB) and the template's encoding for its `Put` (a second copy and one allocation, about the
  same). **Bound:** the probes and one copy, about 110 ns. **Where the code is:** about 1.5× it,
  for the `Put`'s bytes. **What closes it:** giving the store writer the memo's template by
  reference and encoding it there, off the reading thread; that changes the signature
  fastcompile-sol-lazy 3 asked for, so it waits on the lazy runner's `hit`.
- **Digests** (`Built::digests`). A function's is its code hash when it built nothing in (a
  32-byte copy, as nearly every dev function), else one BLAKE3 of 40 + 32 bytes per callee,
  about 100 ns. A static's is one BLAKE3 of its bytes, relocations and data, made on the cores
  that instantiate or compile it, at BLAKE3's SIMD rate (several GB/s a core), with
  instantiation now on all cores too. A debug name's is one BLAKE3 of about 60 bytes, 80 ns,
  made beside `describe` (microseconds) on the walk's cores: 8 ms of CPU, about 0.3 ms on 32
  cores. Sorting is one `sort_unstable` of the function digests (already in key order but for
  the lazy and release pushes) and one of the statics'; debug names come in key order.
  **Bound:** zero for functions; for statics, one pass over bytes the build already copies;
  for names, the hash. **What closes the rest:** the debug name's digest kept with the
  instance's description in the front end's memo, so a warm walk hashes nothing.

STATUS of this section: stopping is at its bound; `memo_hit` 1.5× it; digests at their bound
for functions and statics, the debug names' hash paid on every walk.
