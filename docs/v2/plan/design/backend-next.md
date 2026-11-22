---
status: plan
summary: The code generator audited against four backend ideas of Relax (address-free code records, memo up to callee renaming, unwinding, the fast tier) and its tier fuzzing. Verdict per idea with evidence from the tree, what blaze already has, what changes, and what this lane built (the relocated-field invariant, the shape record key, compact unwind for AArch64, and the LIR evaluator with a random-program harness).
date: 2026-11-15
---
# Backend: what Relax does that blaze lacks

bottom.md §15 lists four of Relax's backend ideas as "Adopt" (Andrew, 2026-11-15). This audit
reads blaze's code generator (crates/blaze-codegen, blaze-stencil, blaze-lir, and
hir-ty-codegen read only) against each, and against Relax (`makepad/relax`, MIT, read as data:
libs/relax/backend/src/rir.rs, regalloc.rs, asm/x64.rs, compiler/src/bmemo.rs,
codegen/tests/fuzz_tiers.rs). Judged in the goal order of bottom.md §1: incremental time, then
memory, then cold time, then disk.

Status words: **has** (blaze does it), **gap** (blaze lacks it and this lane built it),
**next** (lacks it, the change needs another lane's crate), **keep** (Relax's way is not better
for blaze).

## (a) Address-independent code records: has, plus one gap built

**Today.** A compiled function is bytes plus relocations naming targets by key, never by
address: `CompiledFunction { code, relocs, .. }` (blaze-codegen/src/lib.rs:167), `Target::
Function(InstanceKey)`, `Static { item, instance }`, `Extern(String)` (lib.rs:440-448), and a
template holds `Ref::Place(u32)` instead of a target (template.rs:40). Its content ID hashes
code, relocations and data, no address (content.rs `code_id`, "blaze v2 code 4"). Backends
write each relocated field as zero: the stencil glue emits `bl` with the reloc and a zero
immediate (blaze-stencil/src/arch/aarch64/glue.rs:470-474; `adrp`/`add #0` and an 8-byte zero
at :640-674), and the LLVM path moves implicit addends out of the bytes and zeroes the field
(hir-ty-codegen/llvm/emitted.rs:930, `fill(bytes, at, 0)`). The launcher patches at load
(blaze-launch/src/place.rs:382 `patch`).

**Relax.** The same: fields are emitted zero and `link`/`link_with` patch them, with a
callback that makes a veneer when an AArch64 branch is out of ±128 MiB
(backend/src/rir.rs:734-760).

**Gap 1: the invariant was never checked.** Nothing in blaze-codegen says which bits of the
code a relocation owns, or refuses a record whose field holds anything. A backend that left an
addend or a stale address in a field would make a record that is not a function of its key
(rule 21: two placements, one key, different bytes once a later patch ORs into it). **Built**:
`blaze_codegen::fields` (fields.rs): `Field::of(kind, In)` gives each `RelocKind`'s bits (the
whole 4 or 8 bytes on x86-64; `imm26`, `immhi:immlo`, `imm12` on AArch64; LEB value bits on
wasm), `check(code, relocs, In)` refuses a nonzero field, an overlap or a field past the end
with `FieldError` naming the relocation, and `clear` zeroes them (the inverse of any patch).
Tests: a function patched at two bases gives two images whose cleared bytes are equal to the
stored ones and to each other; its relocations round-trip through the blob (`blob::encode`/
`decode`); a planted nonzero field is refused.

**Not wired yet**: `check` is not called from `Template::of_owned_at`, because test fixtures
across the driver place an `Abs64` past the end of their code (template.rs `reserve`: an
8-byte field at 6 of 11 bytes). Wire it there once fixtures are made real; until then the
launcher calls it (below).

**Gap 2: veneers (next, launcher).** place.rs:393 refuses a `Call26` past 128 MiB and the
`NoThunk` path (codegen.md:136-145) relies on one image under it. A million-line workspace's
debug image can pass 128 MiB of text. Change, in blaze-launch: when a `Call26` is out of range,
place a 12-byte island (`adrp x16, T; add x16, x16, :lo12:T; br x16`, x16 being the AAPCS64
intra-procedure scratch register, so no callee may expect it kept) within range, one per target
per 128 MiB window, and branch to it; ld64 and lld do the same. The record does not change: an
island is a fact of placement, not of the function.

## (b) Memo up to renaming of callees: has in process, gap across commands

**Today.** A template is code with each reference by its place in the instance's reference
list (template.rs:834-970, `Template::of_owned_at`, `instantiate_at`); two instances whose IR
is equal up to which callees fill the places compile once (`Shape::of`, backend.rs:434,
"blaze v2 shape 1", hashing the IR with references by place) through a lock-free memo
(memo.rs; driver.rs:4404-4470 `compile_missing`), counted as `Stats::memo` (driver.rs:251).
The store key of a dev template already abstracts the targets to their kinds: `placed`
(driver.rs:551) hashes `place_kind(target)` per reference, never the target. Test:
template.rs:1815 `siblings_up_to_callee_renaming_share_one_template`.

**Relax.** bmemo.rs:1-35 keys a generic body by its bytes with callee slots numbered by first
occurrence and compares bytes in full on a hit; it reports about 24k bodies of about 4k generic
functions in its own compiler (its figure, not measured here).

**Gap.** The memo lives one command (rule 70: no daemon), and the store is keyed per instance
(the instance key holds the type arguments). A new instance (`Vec<NewType>::reserve`) whose
shape an earlier command compiled is compiled again. **Change**: a shape record, written next
to the instance record: `query::keys::shape(shape, toolchain)` (built, query.rs) holds the
same template blob, so a new instance after its IR is prepared probes it before compiling. The
binding is the instance's reference list, which describe already gives each build; nothing new
is stored for it. The instance record stays the one read on the hot path (a second read per
hit would cost the incremental loop). Driver wiring is **next**: `compile_missing` runs on
workers and the store is read on the calling thread (driver.rs:15), so it needs a batched shape
probe between prepare and compile; the old driver is to be replaced (bottom.md), so it goes
into the new item layer's codegen step. **Built**: the key, and a property test (blaze-sim,
template.rs) that random functions with random renamings of references instantiate from one
merged template exactly as their unmerged selves.

**Estimate (labeled).** The share merged is `Stats::memo / (Stats::compiled + Stats::memo)`.
By Relax's 24k bodies over 4k functions, the same-shape share of generic instances can be
large; for reth it is unmeasured. Measure: one cold `blaze build` of reth with the profile on,
read `memo` and `compiled` from the codegen stats; then the cross-command win is the share of a
second command's compiles whose shape the first had (the shape record's hit count).

## (c) Unwinding: keep DWARF, adopt compact encodings on macOS

**Today.** Every function carries `Unwind::Dwarf { cie, fde, personality, lsda }`
(lib.rs:291-298): its own CIE and FDE instruction bytes, from frame rules (unwind.rs
`FrameRule`; the rows `(offset, FrameRule)` of hir-ty-codegen/unwind.rs:75 and the stencils'
`Rule`, code.rs:123). The launcher writes `.eh_frame` with absolute pointers and calls
`__register_frame` per body (blaze-launch-rt/src/unwind.rs:61).

**Relax.** A per-function trailer: (return offset, stub) pairs, saved-register bytes, a stack
offset and a magic word (rir.rs:2340-2395, `UNWIND_MAGIC 0xA5`), walked by its own unwinder.

**Verdict: keep the system unwinder.** Rust's `panic_unwind` calls `_Unwind_RaiseException`;
`catch_unwind`, foreign frames (C callbacks, `extern "C-unwind"`), backtraces, debuggers and
profilers all walk with the system's unwinder and its tables. A private trailer would need a
private unwinder for blaze's frames and a seam at every foreign frame: no program gets faster,
and correctness at the foreign boundary becomes blaze's alone. The cost to cut is size and
registration, not the format:

- **Size.** A CIE per function repeats the same 16 to 20 bytes; an FDE is 24 to 40 more. On
  macOS a frame-pointer function's whole description is one 32-bit compact encoding (Apple's
  `__unwind_info`), 4 bytes against about 50. **Built**: `unwind::compact_aarch64(rows, len)`
  derives the encoding from the rows: frame mode (`0x04000000`, the pairs x19..x28 and d8..d15
  as clang lays them, checked against `clang -O2` output on this Mac: `0x0400000f` for four x
  pairs, `0x04000300` for two d pairs) and frameless with no saves; anything else (pointer
  signing, a save out of the pair layout, a frame that changes in the body) is `None`, so the
  function keeps DWARF (`0x03000000`). Tests over the clang layouts and refusals.
- **Registration (next, launcher).** One `__register_frame` per body is a call and a lock per
  function at load. The launcher can hand libunwind one lookup for the whole image through
  `__unw_add_find_dynamic_unwind_sections` (macOS 14 and later; rule 45: test that the symbol
  exists), answering with a compact table built from these encodings, and DWARF only for the
  `None` functions.
- **x86-64 RBP frames** (`0x01000000`) are the same derivation; not built (arm64 is the Mac).
- Compact encodings are imprecise inside the prologue and epilogue, as Apple's own compilers'
  are: an unwind there (a signal) sees the body's frame. Panics unwind only from call sites.

## (d) The fast tier: keep copy-and-patch, measure it

**Today.** Tier 0 is copy-and-patch stencils: one pass over the IR (blaze-stencil/src/patch.rs:
scan, select, slots, emit), values in frame slots with an accumulator for the next stencil
(patch/slots.rs), holes patched in place, no register allocator, no relaxation. Its input is
still Cranelift IR (lib.rs `compile(func: &ir::Function, ..)`); on LIR is lir.md:544-585.

**Relax.** A baseline register allocator (interval hulls, linear scan in reverse postorder,
liveness only with loops, no coalescing; regalloc.rs:3080-3090) and a one-pass assembler with
no relaxation (asm/x64.rs:1-12): better code than slot traffic, at more compile work per
instruction.

**Verdict: keep.** blaze tiers up by counts (blaze-stencil/src/tier.rs:25-40, ski rental):
code that runs enough is recompiled by Cranelift, then LLVM, so tier 0 should take the least
time per function, and copy-and-patch is the floor of that: a stencil copy and a few patches
per instruction. A register allocator buys speed only for code that runs too little to tier up.
**Evidence is missing**: compile-time.md:36 says stencils are "roughly 10 to 100 times faster
than cranelift" and tier.rs:30 "a Cranelift compile is about 50 stencil compiles"; neither
cites a measurement, and none was found in the tree. Measure before deciding more: the spans
of a stencil compile and a Cranelift compile per function on reth's dev build, the ratio of
their `self_cpu_ns`, and the stencil code's run time against Cranelift's on reth's tests. The
change that does matter is the input: stencils that read LIR in place skip building Cranelift
IR, which for a one-pass emitter is likely most of the time (to be confirmed by the same
spans).

## (e) Tier fuzzing: gap, built for LIR

**Today.** The stencils have a seeded fuzzer of Cranelift programs, compared against
Cranelift's interpreter and Cranelift's code (blaze-stencil/src/tests/fuzz.rs,
tests/fuzz/program.rs:81, tests/interp.rs:38). LIR, the one input of every backend to come, has
no evaluator ("LIR has no interpreter", lir.md:707), so no backend reading LIR can be tested
against a reference.

**Relax.** codegen/tests/fuzz_tiers.rs runs random programs through each tier and compares.

**Built** (blaze-lir, feature `eval`, on in its own tests): `blaze_lir::eval`, a reference
evaluator of LIR's integer core (scalar integers i1 to i128, the arithmetic, bitwise, shift,
rotate, min/max, saturating and overflow operations, `project`, `icmp`, `select`, the
conversions, `freeze`, stack slots with `slot_addr`/`ptr_add`/`load`/`store`/`slot_live`/
`slot_dead`, and `jump`/`br`/`switch`/`return`/`trap`/`unreachable`), by LIR's stated
semantics (lib.rs: shifts mask, division by zero and `MIN / -1` trap); undefined behaviour is
an outcome of its own, and an operation it does not model is a typed refusal, never a guess.
Fuel bounds every run. It is a test reference, never const eval. A seeded generator
(`blaze_sim` tape, bounded: up to 6 blocks of 12 operations, forward edges plus self-loops
counted to at most 5, every value mixed into a returned accumulator) builds verified functions
(crates/blaze-lir/src/tiers.rs); the harness runs each through every `Tier` and compares. Tiers
today: the evaluator on the built function, on its record read in place (`persist`), and on
`parse(print(f))`; a planted tier that miscompiles `sub` as `add` must be caught
(`blaze_sim::control`). The stencils on LIR and LLVM on LIR join as `Tier`s when they exist.

**lir.md:707** says LIR has no interpreter; that line is now false for tests (lir.md is not
this lane's: listed below).

## What other lanes must change

- blaze-launch: veneer islands for `Call26` past 128 MiB (a); call `fields::check` on each
  record before patching, and take field bits from `fields::Field` rather than its own
  (place.rs:382), so one definition of a relocation's bits exists (a).
- blaze-launch-rt: one dynamic unwind sections hook per image with compact encodings, DWARF
  only for the rest (c).
- hir-ty-codegen: every path (Cranelift as well as LLVM) leaves fields zero; `fields::check`
  over its outputs in its tests (a); emit rows to `unwind::compact_aarch64` on macOS (c).
- The new item layer's codegen step: probe `keys::shape` after prepare, before compile (b).
- lir.md:707: LIR has a test evaluator (e).
- Root Cargo.toml: blaze-codegen and blaze-stencil are excluded (parked), so the tests built in
  blaze-codegen here compile only when it is unparked; blaze-lir's do now.

## End state: code generation on the item layer (coordinator's decisions, 2026-11-16)

The owner: "does it do a JIT and eventually get to LLVM cross function inlining", "are u using stencils at all",
"best architecture u should give and then implement it". These are the decisions (D97). They sit inside
bottom.md section 11 (code is records behind slots; compile on first call; a background tier that inlines across
functions; `export` whole and optimized) and lir.md (LIR is the one input of every backend; LLVM is fed from LIR).
An architect's pass below this section checks each against the tree and lists the pieces.

1. ONE SEAM, THREE TIERS. `Tier` is a type parameter at one seam (rule 78): one LIR function and its context in,
   one code record out. `Reference`: today's emitter (crates/blaze-emit: every value in a stack cell): kept as the
   simplest machine-code oracle, never the default once `Fast` passes. `Fast`: point 2. `Optimized`: point 4. Every
   tier of one LIR function gives the same observable result: generated LIR programs (blaze-sim) and every ladder
   rung run at every tier and are compared with LIR's evaluator; a planted wrong encoder must be caught.

2. THE FAST TIER IS ONE ANALYSIS PASS AND ONE EMISSION PASS, WITH REGISTERS, AND ITS ENCODERS ARE DATA.
   - Analysis, one pass over the function: block order, loop depth, each value's last use.
   - Emission, one pass: a greedy register allocator (a value lives in a register from its definition to its last
     use; when none is free the value with the farthest next use is spilled to its cell; at a block boundary block
     parameters are in fixed places), instruction selection, and encoding, together.
   - The encoders are a TABLE, not hand-written code: for each LIR operation, type and operand shape (register,
     constant, memory) a machine-code template with marked fields: the register fields, the immediate, the branch
     or import site. Emitting an operation is a copy and a few field writes. This is copy-and-patch whose holes
     include the registers, so there is one template an operation and not one per register assignment; on AArch64
     every register field is at a fixed place in a 32-bit word.
   - The table is MADE at blaze's build time, never at its run time, by lowering each one-operation LIR function
     through the SAME LIR-to-LLVM lowering the optimizing tier uses (one home) with the operands pinned to
     placeholder registers, and reading the bytes and the fields out of LLVM's object. It is checked in as data
     with its digest and the generator's inputs; building blaze does not need LLVM for it; two builders agree
     because nobody regenerates it implicitly. A second target (x86-64) is a second table and a second register
     description; the two passes are shared.
   - Each template is validated once, when the table is generated: against LIR's evaluator on every operand edge
     value (the engine's edge tables), and, for the integer and bit operations, proved equal to the operation's
     bit-vector meaning (the separate `bv_decide` proofs the gate already lists). The backend is then a table of
     checked fragments and a small composer.
   - Why not plain stencils as in the parked crate: a template per register assignment multiplies the table or
     forces values through memory between operations; register fields as holes keep the table small and the code
     in registers. Why not hand-written encoders with an allocator: the allocator is needed either way and is
     built FIRST on today's hand-written encoders (stage 1: it removes the stack-cell traffic); the table then
     replaces the hand-written encoders operation by operation (stage 2) and brings x86-64 with it.
   - What decides emission speed matters less here than elsewhere: code is a record found by its key, so the fast
     tier runs once per function per content, and after an edit for the one function edited. Its CODE's speed
     matters more: an edited function and the callers that had inlined it run fast-tier code until the tier-up.
     Floors (estimates until measured): 10 to 30 ns per LIR instruction emitted; fast code within 2x of optimized
     code on the ladder's integer programs.

3. HEAT COMES FROM SAMPLING THE PROGRAM COUNTER, NOT FROM A COUNTER IN EVERY PROLOGUE (bottom.md left it open).
   The launcher's runtime takes a profiling timer signal on each program thread and writes (program counter,
   link register) pairs to a ring; the control thread drains it and maps addresses to functions through the
   placed records. No instruction is added to any function; a hot LOOP in a function called once is seen (an
   entry counter never sees it); the link register gives the calling function, so hot call EDGES come for free,
   and they are what inlining is chosen by. Past a bound a function is queued once per run for the optimizing
   tier, compiled by the run's parent on the pool, after the answer, and published by writing one slot word:
   calls made after the write run the new code; a frame already in the old code finishes in it (no on-stack
   replacement: a loop that never returns stays in fast code, which point 2 makes acceptable; revisit only if a
   real program shows it). A profile record per program, keyed by names that survive edits (instance symbols, not
   code identities), keeps the hot set and the hot edges, so after an edit the functions known hot are
   re-optimized without being rediscovered. A startup record keeps the order functions were first called in, so
   the next run asks for them in one batch instead of one stub at a time.

4. THE OPTIMIZING TIER: BLAZE INLINES ON LIR; LLVM OPTIMIZES ONE FUNCTION.
   - The unit is one instance. Blaze's own inliner, an LIR-to-LIR pass, splices in the callees it chose (always:
     tiny bodies and `#[inline(always)]`; then hot edges from the profile, largest benefit first, under a size
     budget; never a cycle). The record's read list is then exact BY CONSTRUCTION: the code identity of every
     body spliced in, transitively. An edit of a callee makes exactly the callers that inlined it stale, and they
     run their fast code until remade. LLVM's inliner is not used: its choices would have to be read back out of
     it, and a read list that is guessed is a wrong build waiting.
   - The inlined LIR function is lowered to LLVM IR and run through a fixed, named function pipeline (the
     standard -O2 function passes without an inliner; -O3 only if a measurement on the acceptance projects pays
     for its compile time), single-threaded, with the target features in the key: equal inputs give equal bytes.
   - LLVM is told what Rust knows, carried on LIR from the front end (rule 31), never recovered by pattern:
     no-alias and read-only parameters, non-null, dereferenceable sizes, alignments, value ranges of loads, and,
     from blaze's effect facts, that a callee cannot unwind or is pure. A fact about a callee that was NOT inlined
     is a read like any other (its fact record, keyed by its code identity).
   - The inliner is blaze's own pass on purpose: it is validated like every tier (evaluator equality on generated
     programs), it is the first pass of the backend that replaces LLVM at the top one day, and it is where the
     profile is used.
   - An optimized record is found without knowing what it inlined through a pointer keyed by the instance and
     tier, and is valid only if every entry of its read list still holds (a record valid by its read list).

5. EXPORT IS THE SAME UNITS, ALL OF THEM. Every reached instance at the optimized tier, the inline plan from the
   profile record when there is one and from static sizes when not, direct calls in place of slots, the layout
   from the profile (code-layout.md). The file is written whole each time; the units behind it are records, so
   an export after a session of runs reuses what the background tier already made.

6. KEYS. A fast record: the instance's symbol, its body's digest, the facts of types it was built with, the
   target and features, the tier, the producer of that tier (the stage's own code identity, D87), the table's
   digest. An optimized record: the same, plus the pipeline's name, LLVM's version, and its read list. The profile
   and startup records hold no address and no time.

7. ORDER OF BUILDING: (a) the `Tier` seam and the tier-equality harness on today's emitter; (b) liveness and the
   register allocator in blaze-emit (stage 1); (c) the LIR inliner with its validation; (d) LIR-to-LLVM lowering
   with the facts, behind the seam, in a helper the run's parent starts; (e) the encoder table's generator and
   the table (stage 2), which reuses (d); (f) sampling, the profile and startup records, promotion; (g) export at
   the optimized tier; (h) x86-64. (a), (b) and (c) need nothing that does not exist today and start at once.

### Checked against the published state of the art (coordinator, 2026-11-16 16:45; D98 amends D97)

The owner: "it needs to be sota". Each point above was compared with what is published; three change.

- THE FAST TIER'S SHAPE IS THE PUBLISHED BEST FOR EMISSION SPEED, AND ITS FLOOR WAS WRONG. Point 2 is the shape of
  TPDE (Schwarz, Kamm, Engelke, CGO 2026, arXiv 2505.22610): one analysis pass, then one pass that selects,
  allocates registers and encodes, with encoders generated through LLVM from high-level code ("snippet encoders");
  its authors report 8 to 24 times LLVM -O0's back-end speed on SPEC CPU2017 with run time within about 9% of
  -O0. Copy-and-patch (Xu and Kjolstad 2021; Deegen 2024) is the other reference: stencil variants per register
  situation, nothing allocated across stencils. So the two passes and the generated table stand. What does NOT
  stand is "fast code within 2x of optimized": the published result for this shape is -O0-class code, and
  -O0-class Rust is far from optimized Rust because nothing is inlined (an iterator's `next`, a `Deref`, an
  operator on a wrapper are calls). AMENDED: the Fast tier's input is LIR after the TRIVIAL INLINER, blaze's own
  LIR-to-LIR pass: bodies under a small instruction bound with no loop, and `#[inline(always)]`, spliced in; the
  fast record's read list gains exactly those bodies' code identities. The bound is set by a measurement (the
  ladder's programs and tempo's tests at Fast with and without it, instructions retired), and the ratio of fast
  to optimized code is REPORTED from that run; no number is claimed before it.
- THE OPTIMIZING TIER USES LLVM'S INLINER INSIDE AN OFFER SET BLAZE CHOOSES (point 4 amended). A blaze-own
  inliner deciding on unoptimized LIR would decide worse than LLVM's, which works bottom-up with simplification
  between inlines and is what `rustc -O` code gets its speed from; a top tier below that is not the state of the
  art. The module handed to LLVM holds the one instance and, as `available_externally` bodies, the callees blaze
  OFFERS: transitively, under a size bound, hot edges from the profile first, never a cycle, never unbounded.
  The standard -O2 module pipeline runs on it; code is emitted for the one instance. The record's read list is
  the OFFER SET: a superset of what was inlined or read for an inferred attribute, and sound BY CONSTRUCTION,
  because LLVM cannot use a body it was not given. This is the shape of ThinLTO's incremental cache key (the
  hashes of the import list). Its cost: an edit of a callee that was offered and not inlined remakes the caller
  for nothing; accepted, and never tightened from LLVM's remarks (an offered body may have fed an inferred
  attribute). Blaze's own inliner is the trivial one above and nothing more. Later, in this order: optimized
  bitcode kept as a record an instance so a callee is offered already optimized; the samples as entry counts and
  branch weights on the module (sampling PGO, AutoFDO's shape).
- A RUNNING LOOP TIERS UP (point 3 amended: "no on-stack replacement" was below HotSpot, V8 and JavaScriptCore:
  a loop in a function entered once would run fast-tier code for the whole run, and that is the first program
  anyone times). Designed in from the first fast record, built after promotion works: (i) the fast tier keeps
  every value live across a loop header in a FIXED PLACE at that header and writes the LOOP MAP (a header: its
  live values and their places) into the code record; (ii) each loop back-edge carries one POLL: a load through
  the run-context register from a page the runtime can protect (HotSpot's safepoint poll: one instruction, no
  branch, no counter); (iii) when the samples show a thread staying inside one loop, the parent makes the
  instance's optimized LOOP-ENTRY variant (the same LIR entered at the header, the loop map's values as its
  parameters) and protects the page; the poll faults at a back-edge, and the handler moves the mapped values and
  resumes in the variant. The same poll is the bounded-time stopping point that later features need (patching a
  running program). The allocator piece (b) reserves the register and writes the loop map from its first
  version; the poll instruction goes in from the start unless the ladder shows it costs more than 1%.

Unchanged after the comparison: the one seam with tiers tested equal (point 1); sampling instead of prologue
counters (the sampler's link register gives call edges, which counters do not); the table generated through the
one LIR-to-LLVM lowering and validated per template; export from the same units; the keys.

### Evidence, gap and pieces (architect, 2026-11-16)

SUPERSEDED IN PART (17:15): D99 and D100 below decide every objection here and replace the piece list. The
evidence per point still stands. Piece 0 is now crates/blaze-tier. The one wave of pieces is in WAVE-BACKEND.md:
R, A, K1, K2, T1, S1, S2, C1, C2, O, D1, D2, D3, T2, F1, F2, L, U, G, X, M. In piece 0 the new record format
lives in blaze-tier, because blaze-code cannot depend on blaze-tier without a cycle. blaze-code keeps its leaf
types, segments and image. `bzcode01` is deleted when its last user moves.

What the tree has under each decided point (D97 as amended by D98), what must change, the objections, the gap
table and the pieces. Ready writer prompts: /Volumes/Projects/blaze-int-1112-notes/port27/WAVE-BACKEND.md. Every
number marked "estimate" is one; no run backs it.

#### Evidence per point

1. The seam. crates/blaze-emit record.rs `emit_record(func: View, names: &dyn Names, isa: Isa, alloc) ->
   Result<CodeRecord, RecordEmitError>` is already the one entry every caller uses; it becomes the `Reference`
   tier unchanged. crates/blaze-run lib.rs `enum Tier { Fast, Optimized }` and `struct CodeKey { function, tier }`
   name the tier in the run's keys; tier.rs `trait Optimizer`, `struct Optimized { code, inlined }`, `fn usable`
   are the optimized record's shape (its `inlined` list becomes the offer set, D98). The harness to reuse:
   blaze-emit tests.rs `mod random` `fn program(tape)`, `fn machine_agrees`, `random_programs_agree_with_the_
   evaluator` (1000 cases, literal-word form through `run::place`/`run::call`); record_tests.rs `mod runs` `fn
   bound(func, home)` binds a record and runs it, but emits the record itself: it is split so any tier's record can
   be bound (piece A's wiring). blaze-lir tiers.rs already has a private `trait Tier` and a `blaze_sim::control`
   with a planted `SubAsAdd`: that is a model, not the seam (it compares LIR evaluators, not machine code).
2. The fast tier. blaze-emit lib.rs: every LIR value has a 16-byte stack cell; pass.rs `fn ld` is
   `self.ld_sp(rt, value.0 * 16)` and `fn st` is `self.st_sp(rt, inst * 16)`: one seam through which every operand
   passes, so registers enter at two functions (plus pass/float.rs `fld`/`fst`). Scratch use today: X9..X12, X16,
   V0, V1 (enc.rs constants stop at X16; FP=29, LR=30); x19..x28 and d8..d15 are never used, so a callee-saved
   pool exists without moving anything. pass.rs `run` writes the prologue `stp x29,x30,[sp,#-16]!; mov x29, sp`
   and patches the frame in `settle`; `fn edge` makes block-argument moves through cells. No liveness, loop or
   last-use analysis exists anywhere: blaze-lir verify.rs computes dominators privately (lines 596-683) for its
   checks only. The encoders are enc.rs, one `const fn` per A64 form (about 100), each pinned by tests.rs
   `encodings`: stage 1 keeps them. The baseline to beat: codegen.md "cells 1.031 ns/iter, registers 0.234
   ns/iter, ratio 4.41" from tests.rs `the_cell_baseline_against_registers`.
   The trivial inliner (D98): blaze-lir has one LIR-to-LIR pass, coroutine.rs `split`, built through Builder: the
   model; builder.rs `Builder::inlined(parent, call_site)` already makes inline span scopes; eval.rs refuses every
   call (`EvalError::Unsupported`), so validation needs the evaluator to follow calls first (piece C1).
3. Heat by sampling. Nothing samples today: no SIGPROF, setitimer, timer_create or thread_get_state in
   blaze-launch-rt or blaze-run (blaze-core wait/linux.rs lists SIGPROF only among signals it blocks). What exists
   is the count stub: blaze-launch-rt observe.rs `count` (14 words, x16/x17), driven by blaze-run heat.rs
   `Config { bound }` and `Heat::reached(slot, hot)` (once per run). `Heat` stays (a sample count reaching the
   bound calls it); the count stub is retired when F2 lands. Publication: run.md's order (copy, cache
   maintenance, unwind entry, release store, wake) and blaze-run slots.rs are already one slot word.
   launch-rt run_mode.rs refuses "a record at the optimized tier" by name (line ~495) and refuses x86-64 (module
   doc): F2 and H lift those. Loop map and poll (D98): blaze-code record.rs `CodeRecord { code, relro, targets,
   sites, words, frame, pads, names, text }`, MAGIC `bzcode01`, header "frame, flags (0), eight counts": no place
   for a map; it gains two sections (piece B3).
4. The optimizing tier. Facts LIR already carries (blaze-lir func.rs): `ParamFactsRec` NONNULL, NOALIAS,
   READONLY, NOUNDEF, CAPTURES_NONE, DEREF_OR_NULL, WRITABLE, DEAD_ON_UNWIND, deref, size, range, align;
   `MemAttrsRec` range, align, NONNULL, NOUNDEF, INVARIANT; flags.rs NSW NUW EXACT DISJOINT INBOUNDS and
   fast-math; `FnAttrs` nounwind, cold, inline forms. What the front end FILLS today: almost none. blaze-build sets
   only `MemAttrs::aligned` on loads, NSW/NUW/INBOUNDS only in intrinsics.rs, and at walk.rs ~2802 only size and
   align on memory parameters: no noalias, nonnull, dereferenceable or range reaches LIR. LLVM -O2 without those
   facts is not `rustc -O` code; filling them is front-end work (rule 31) and a piece of its own (gap row 9).
   Knowledge that carries over from the parked crates/hir-ty-codegen: llvm.rs `set_options`
   (`-aarch64-enable-global-isel-at-O=-1` because of a landing-pad bug seen in LLVM 20.1, `-disable-preinline`,
   `-disable-vp`); the diagnostic handler (LLVM 23 prints an error and continues); `INLINE_THRESHOLD` 500,
   measured geomean 1.091 to 1.080 of rustc for 2-5% more code; `shown_name`/`unshow` (callee bodies shown for
   inlining, then dropped: the same idea as `available_externally`); llvm/emitted.rs reads an object back and
   zeroes relocated fields; llvm/lower.rs (3083 lines) is how each MIR form became IR. crates/blaze-stencil
   (parked) gen/ shows LLVM driven at build time with a pinned prebuilt output checked by BLAKE3 (pin.rs,
   stencils.json): the model for the table's digest (point 2, stage 2).
   How LLVM is pinned and linked: root Cargo.toml `[workspace.metadata.blaze.native.llvm] pin =
   "pins/llvm.json", env = "LLVM_SYS_231_PREFIX", packages = ["llvm-sys",
   "blaze-stencil/llvm-stencils"]`; llvm.json pins 23.1.4 with per-target tarballs and sha256; vendor/llvm-sys
   pin.rs reads `PIN_JSON = "../../pins/llvm.json"`, wrappers/passes.cpp holds
   `LLVMBlazeRunPasses`. The pin lives in a parked crate and must move when blaze-llvm exists (coordinator's
   edit: vendor/ is no lane's).
5. Export. blaze-export export.rs `export(program, platform, out, alloc)` already makes every in-reach call a
   `bl`; layout.rs `order` puts input.rs `Profile { digest, hot }` functions first. It needs only optimized
   records as input (G).
6. Keys. blaze-ladder code_records.rs `code_key` hashes symbol, body digest, ground, triple, lse, tier byte,
   settings, toolchain, ENTRY_FORM, MAGIC; layouts and constants are its READ LIST. Point 6 puts "the facts of the
   types it was built with" in the key: allowed (see the note below), but it is a change from today's form. The
   producer's code identity (D87) and the table's digest are not in `code_key` yet.
7. Order. Checked: (a), (b) stage 1, (c) and the offer set need nothing that does not exist; (d) needs a new
   crate and the pin; (e) needs (d); (f) needs the OS-sampling choice below; (h) needs (e).

#### Objections

OBJECTION: point 2's spill rule ("when none is free the value with the farthest next use is spilled to its cell")
cannot run in ONE forward emission pass: code already emitted, including a loop body a back edge returns to,
reads the register the rule would hand to another value. Evidence: pass.rs emits each instruction's bytes at once
(`put`, `lower`), with only branch offsets patched later (`patches`). Architect's choice, written into B1: homes
for whole intervals, decided in the analysis pass: a value gets a register only if it is free for all of [def,
last use], intervals extended to a loop's end when live into its header; otherwise its cell. This also gives
D98's fixed place at each header for free, and lets the prologue know which callee-saved registers to save.

OBJECTION: D98's run-context register is not reliable at every entry. A callee-saved register (x28 is the one
blaze-emit can reserve: nothing uses x19..x28 today; x18 is the platform's on Apple) holds whatever foreign code
put there when C calls back into blaze code (a `qsort` comparator, a thread start through libc, a signal
handler): run.md says a function's address is its entry thunk, and every indirect entry would have to reload it.
Evidence: blaze-emit lib.rs (AAPCS64 calls), run.md "End state" (entry thunks). Proposal for the coordinator: the
poll is a Load site, `adrp x16, poll@PAGE; ldr xzr, [x16, poll@PAGEOFF]`, two instructions, no reserved register,
in the site form blaze-code already has (SiteKind::Load). B2 is written with x28 as decided; switching is one
function.

OBJECTION: point 3's "the link register gives the hot call edges" holds only in a leaf or before the prologue's
`stp x29, x30`. In a non-leaf body the link register holds the return address of the last call the function made,
which points into the function itself. Evidence: blaze-emit always builds an x29 frame (pass.rs `run` prologue).
The caller is the return address in the frame record at [x29+8]; F1 takes lr only in the first 8 bytes and at a
`ret` word.

OBJECTION: macOS has no per-thread CPU timer (no `timer_create`; `setitimer(ITIMER_PROF)` is one per process and
the kernel picks the thread), and a profiling signal is observable by the program: an interrupted `nanosleep`,
`poll` or `select` returns EINTR even under SA_RESTART, and a program's own SIGPROF handler collides. Proposal: on
macOS the launcher's control thread samples with `thread_suspend` + `thread_get_state` (no signal; launch-rt
lib.rs already names thread_suspend as a tool); on Linux `timer_create(CLOCK_THREAD_CPUTIME_ID)` with
`SIGEV_THREAD_ID` or perf_event. F1 is written so either works.

OBJECTION: point 2 stage 2 on x86-64. "Templates with marked register fields" holds on AArch64 (Rd at bit 0, Rn at
5, Rm at 16, Ra at 10), not on x86-64, where the register changes the bytes and the length (REX prefixes, the rax
short forms, SIB for rsp/r12, disp8 for rbp/r13). The table needs canonical long forms or a variant per register
class there. On both targets a constant operand needs encodability classes (AArch64 logical immediates and imm12
depend on the value).

OBJECTION: reading LLVM's code back (D3). blaze-code SiteKind Load assumes `adrp xN; ldr xN` at +0/+4 and Call
assumes `blr x16` at +8. LLVM's scheduler may separate the pair, and a GOT call may use any register. Either the
site form gains two offsets and a register, or D3 refuses such code by name and loses calls. The coordinator
decides before D3 is written.

Not an objection, a note on point 6: today's `code_key` keeps layouts out of the key and in the read list, so a
layout change revalidates instead of rekeying. Putting the facts of types in the key is sound; it makes fewer
records shareable across a layout edit. Kept as decided.

#### Gap table

| # | What the decision needs | Today | Piece |
|---|---|---|---|
| 1 | One `Tier` seam, tiers tested equal on machine code | `emit_record`; a private LIR-level `Tier` in blaze-lir tiers.rs | A |
| 2 | Liveness, loops, last use, homes, loop map | none | B1 |
| 3 | Values in registers, callee-saved pairs, unwind rows, poll | cells only (pass.rs `ld`/`st`) | B2 (after Run piece 11) |
| 4 | The loop map in the record | no section; MAGIC bzcode01 | B3 |
| 5 | Evaluator follows calls | refused | C1 |
| 6 | Trivial inliner with exact read list | none; `split` and `Builder::inlined` to build on | C2 |
| 7 | The offer set | none; `Callee`, `InstanceKey`, `CodeId` exist | O |
| 8 | LIR to LLVM, -O2 module pipeline, one context per call, own `extern "C"` | parked inkwell code in hir-ty-codegen | D1, D2 |
| 9 | Facts filled by the front end | only align, intrinsics' flags | front-end piece (blaze-build; WRONG ANSWERS holds walk.rs now) |
| 10 | Object read back into a record | parked emitted.rs | D3 (after the site objection) |
| 11 | Optimized tier in a helper, keyed per point 6 | `Optimizer` trait, no implementation | D4 |
| 12 | Template table, generated, digested, validated | hand-written enc.rs | E |
| 13 | Sampling, profile and startup records, promotion | count stub, `Heat` | F1 (core), F2 (OS half, records, promotion) |
| 14 | Loop-entry variant, poll fault handler | none | F3 |
| 15 | Export of optimized records | export takes today's records | G |
| 16 | x86-64 | refused in run mode; parked stencils only | H |
| 17 | The LLVM pin out of a parked crate (moved 2026-11-16: crates/hir-ty-codegen/llvm.json is pins/llvm.json) | pins/llvm.json, vendor/llvm-sys pin.rs | coordinator |

#### Pieces, in point 7's order

- A, the seam and the harness (blaze-emit tier.rs, tier_tests.rs): now.
- B1, liveness, homes, loop map (blaze-emit pass/live.rs): now.
- B3, the loop map in the record (blaze-code loop_map.rs, record.rs wiring, cross-crate): now; apply after or
  with Run piece 11 if that changes the header too.
- C1, the evaluator follows calls (blaze-lir eval/calls.rs): now.
- C2, the trivial inliner (blaze-lir inline.rs): now; its test needs C1.
- O, the offer set (blaze-run offer.rs): now.
- B2, registers at the ld/st seam, the poll, the `Fast` tier: after A, B1, B3 and Run piece 11's unwind wiring.
- D1, blaze-llvm: IR text to an object at `default<O2>` (staged new crate): now; the pin move is the
  coordinator's.
- D2, LIR to LLVM IR text with the facts and `available_externally` offers (staged, sans-IO): now.
- D3, object read back into a CodeRecord: after D1 and the site-form objection.
- D4, the optimized tier in a helper the parent starts, keyed per point 6, read list = the offer set: after D1,
  D2, D3, O.
- Front-end facts (noalias, nonnull, dereferenceable, range from types and borrows): after the WRONG ANSWERS lane
  releases walk.rs; before any optimized-tier speed is quoted.
- E, the table generator and table (stage 2): after D1, D2 and the x86-64 objection.
- F1, sample attribution and heat (blaze-launch-rt sample.rs): now (sans-IO).
- F2, taking samples, profile and startup records, promotion, lifting run_mode's refusal: after F1, D4 and the
  sampling objection.
- F3, the loop-entry variant and the poll's fault handler: after F2, B2, B3.
- G, export at the optimized tier: after D4.
- H, x86-64: after E.

Measurements that must come first (each names the tree's commit, the load and instructions retired):
- The cell baseline ratio after B2 (today 4.41; B2's test prints the new one; the ratio of fast to optimized code
  is reported from the ladder after C2 and D4, never claimed before, D98).
- Emission time per LIR instruction today and after B2, over the hello world's 12,651 reached bodies
  (port27/hello-1116/NOTES.md): before E, to know what stage 2 buys. The HELLO WORLD lane can run it.
- The trivial inliner's bound: the ladder and tempo's tests at Fast with and without C2 (D98).
- The poll's cost on the ladder (D98: kept unless above 1%), after B2.
- The number of first-call asks before `main` prints, and the cost of sampling at the chosen rate: before F2's
  bound. The RUN PROFILE lane (BODY TABLES row) or the HELLO WORLD lane.
- LLVM's bytes equal across two processes (D1's test does it within one; a two-process run is the integrator's).
- `default<O2>` against `default<O3>` on the ladder and tempo, compile and run time: before O3 is ever named.

Contradictions with bottom.md section 11 found in other docs (bottom.md itself left "prologue add against
sampling" open, and point 3 closes it): run.md "Machines" keys the profile "per instance by code identity", point 3
by instance symbol; run.md and blaze-launch-rt observe.rs still describe the count stub as the heat; lir.md "LLVM"
keeps inkwell over vendor/llvm-sys while bottom.md 11 says the Rust bindings go; lir.md "LLVM's memory" admits
compiles by memory, against rule 50; tiers.md calls tier 0 stencils, which point 2 replaces.

### Built straight at the end state (coordinator, 2026-11-16 16:50; D99, replaces point 7's order and point 2's "stage 1")

The owner: "go straight to sota". So there is no stage 1 and no order of arrival: nothing is built that the end
state then replaces, and every piece starts at once.

- PIECE 0, FIRST AND ALONE (rule 79): one small crate of types that compiles and holds no logic: the `Tier` seam;
  the code record with its loop map, poll sites, import sites and read list; the TEMPLATE TABLE's format (a
  template: its bytes, its register fields, its immediate and site fields, the operand shapes it serves, its
  clobbers) and the trait a table is read through; the register description of a target; the offer set; the
  sample ring's entry and the profile and startup records. Every other piece is written against it.
- THE FAST TIER IS WRITTEN ONCE, AS THE COMPOSER OVER THE TABLE: analysis pass, emission pass with the allocator,
  the loop map and the polls, reading templates only through the table's trait. It never calls a hand-written
  encoder. Today's hand-written encoders (crates/blaze-emit enc.rs) are not thrown away and not built upon: they
  become a GENERATOR of table entries (each run once with placeholder registers at blaze's build time), so the
  composer runs on a real table the day it compiles; the LLVM generator fills the same table by the same format,
  and where both give an entry the two are tested equal against LIR's evaluator: two independent sources of one
  template is a differential test the design gets for nothing.
- IN PARALLEL, each a writer against piece 0: the composer (arm64); the table generator from enc.rs; the tier
  equality harness (generated LIR programs and the rungs at every tier against the evaluator, planted wrong
  template caught); the trivial inliner; the offer set's chooser; LIR-to-LLVM lowering with the facts and the
  -O2 module pipeline in a helper; the table generator through LLVM and the per-template validation; the sampler
  (signal, ring, address map), the profile and startup records, promotion by slot write; the loop-entry variant
  and the poll's fault handler; export at the optimized tier; the x86-64 register description and table.
- What is NOT parallel is admission: the coordinator's integrators compile each piece as it arrives, and a tier
  becomes the default only after the equality harness passes on it and the real command has run a real project
  through it with the profile on (rules 68, 82).

### The outside critic's findings and the architect's objections, decided (coordinator, 2026-11-16 17:00; D100)

An outside reader was given the design above and asked only where it is below the best systems; the architect
listed where the code contradicts it. Each is decided here; where this differs from an earlier line, this wins.

1. FAST CODE NEEDS A SIMPLIFIER, NOT ONLY AN INLINER (critic, blocker; accepted). Splicing `next` or `deref` in
   and leaving the `Option`, the range and the iterator in memory keeps the code 5 to 20 times from optimized;
   rustc pairs its MIR inliner with scalar replacement, Cranelift has a mid-end, V8 and .NET have a middle tier
   for this reason. The Fast tier's input is LIR after the LINEAR SIMPLIFY STAGE, blaze's own LIR-to-LIR passes,
   each one walk: the trivial inliner; scalar replacement of slots whose address does not escape (their fields
   become values); copy and constant propagation with folding; dead code and trivial branch removal. Each is
   validated like a tier (evaluator equality on generated programs, a planted wrong rewrite caught). The four
   setups are measured on the ladder and on tempo's tests (Fast; with the inliner; with the whole stage;
   Optimized), instructions retired, and the stage's bounds are set by that run.
2. A RUNNING LOOP TIERS UP ON TOP OF ITS FRAME, NEVER BY MOVING IT (critic, blocker; accepted; replaces D98's
   "moves the mapped values"). Rust takes the address of locals everywhere; moving a frame leaves pointers into
   it dangling. The loop-entry variant runs in a NEW frame above the fast frame, as .NET's does: slots whose
   address is taken stay where they are in the fast frame and reach the variant as pointers; only values held in
   registers or private cells are passed as parameters; the variant runs the rest of the function, drops what
   the fast frame held, and returns through a stub that pops both. The fast frame's unwind row at that stub has
   no cleanup; the variant's landing pads own the drops.
3. NO RESERVED REGISTER (architect and critic; accepted). The poll is a load site in the form blaze-code has:
   `adrp x16, poll@PAGE; ldr xzr, [x16, poll@PAGEOFF]`: no register is taken from the allocator and an entry
   from foreign code needs nothing reloaded.
4. CALL EDGES COME FROM THE FRAME RECORD, AND macOS IS SAMPLED BY SUSPENSION (architect and critic; accepted;
   replaces point 3's link register and timer signal). Every tier keeps the frame record. A sample is the
   program counter, the frame pointer and the link register; the caller is the return address at [x29+8],
   except in the prologue's first words and at a `ret`, where it is the link register; the reader checks the
   frame pointer against the thread's stack bounds before it reads. On macOS the runtime's control thread
   samples with `thread_suspend` and `thread_get_state` and holds no lock and allocates nothing while a thread
   is stopped (JavaScriptCore's sampler); no signal reaches the program, so no sleep or poll of its own is
   interrupted. On Linux: `timer_create(CLOCK_THREAD_CPUTIME_ID)` with `SIGEV_THREAD_ID`.
5. HOMES AND EVICTION (architect's objection to point 2's spill rule; accepted with one addition). A value live
   across a block boundary gets its home (a register for its whole interval, extended to a loop's end when live
   into its header, or its cell) in the analysis pass: that is what gives the fixed place at each header. A
   value defined and last used inside one block, which is most values, takes any free register in the emission
   pass and may be evicted to its cell there, since no other path can reach that code.
6. FUSED PATTERNS AND IMMEDIATE CLASSES (critic 10, architect on x86-64; accepted). A template may cover a small
   tree of operations when the inner value has one use (compare and branch, an address folded into a load or
   store, a shifted operand). An operand shape includes the immediate's encodability class; a constant outside
   it is put in a register by the composer. On x86-64 a template uses the canonical long form, so its register
   fields are at fixed bits, and a short variant only where a measurement pays for it.
7. THE READ LIST OF AN OPTIMIZED RECORD IS THE INLINED SET, EXACTLY (critic 5; accepted; replaces D98's "the
   offer set, never tightened"). With the offer set as the read list, an edit of one small hot function demotes
   every caller it was offered to, at once, in the edit loop: a cliff. So: (a) the pipeline is a NAMED CUSTOM
   one: LLVM's inliner in its bottom-up order with the -O2 function simplification, and NO pass that infers a
   fact about a callee from an offered body (no interprocedural attribute inference, no interprocedural
   constant propagation over them); (b) what such passes would have inferred is SUPPLIED from blaze's own fact
   records (cannot unwind, pure, reads only, no-alias, ranges), each a read by the fact's content, so a callee
   edited without changing its facts invalidates nobody; (c) the inliner's remarks are read from LLVM's remark
   stream by the helper (a remark for every inlined call site; an unreadable stream is an error, never a guess),
   and the read list is the bodies inlined, transitively, plus the fact records of callees still called; (d) an
   optimized caller whose non-inlined callee changed stays valid and reaches the new callee through its slot.
8. THE KEY HOLDS THE OFFER SET'S DIGEST (critic 4; accepted). The profile chooses the offer set, so two runs
   would write different bytes under one key. The optimized record's key includes the digest of the offer set
   (in a total order); the pointer from (instance, tier) to the newest record is local and never shared; a
   record made from an observed profile stays in the private layer; the shared cache holds records whose offer
   set came from the static policy or from a profile that is itself a named input.
9. CALLEES ARE OFFERED ALREADY OPTIMIZED, FROM THE FIRST VERSION (critic 6; accepted; moves "later" to now).
   Each optimized instance also keeps its optimized LLVM bitcode as a record; a callee with one is offered in
   that form (ThinLTO's import of already-simplified code), so a caller's compile does not re-simplify it, and
   the parent makes hot callees before their callers. The helper compiles many instances in one process, a
   fresh module each, and "alone and batched give equal bytes" is a test. What a whole-crate pipeline has that
   this lacks (dead argument elimination, argument promotion, merging, global optimization) belongs to `export`,
   which is not incremental: blaze's whole-program LIR passes (devirt.md, icf.md) run there.
10. UNWINDING, POSITIONS AND DEBUGGERS ARE PART OF EVERY TIER'S RECORD (critic 7; accepted; the design was
   silent). A code record carries its unwind rows and landing pads, and a position map (item-relative, rule 71)
   with the inline frames the simplifier and LLVM created, so a backtrace names spliced bodies. Placed code is
   registered with the unwinder through `__unw_add_find_dynamic_unwind_sections` on macOS (a lookup callback,
   not a list walked on every throw) and its like on Linux; a debugger sees placed code through the JIT
   interface. The loop-entry variant's rows describe its frame above the fast frame.
11. ONE RECORD FORMAT CHANGE CARRIES ALL OF IT (architect on reading LLVM's code back; decided). A site is one
   relocation: an offset, a kind (page, page offset, branch, GOT page, GOT page offset) and a target; today's
   paired load and call forms are two and three such sites. With the loop map, the poll sites, the unwind rows
   and the position map this is ONE new format of the code record, made once.
12. SHARED INSTANCES NEVER PASS THROUGH FAST (critic 9; accepted). Keys use the instance's share identity
   (polymorph.md); an instance whose content depends only on dependencies is found in the shared cache at the
   optimized tier before anything is compiled.
13. OPEN, BY MEASUREMENT (critic 8): a call through a slot word is a load and an indirect branch; the
   alternative is `bl` to a per-callee `b` that tier-up rewrites. Slot words stay (bottom.md section 11, the
   owner's design: no code is patched, any distance) until the ladder's call-heavy rungs show the difference;
   that run is part of the first measurement. Accepted now: promoted code is placed together in a hot region
   ordered by the sampled call edges (code-layout.md), not in first-call order.
14. Proofs do not gate a template (critic 12): a template is admitted by evaluator equality over the edge
   tables; the bit-vector proofs are added where they are generated for free. The target CPU is named in the
   key and given to LLVM explicitly, never `native` (critic 11).

## One lowering (the coordinator's ruling, 2026-11-16 21:07; the end state is not built)

Found on the coroutine state's layout (ASYNC P8): flow-lower (about 29 local and temporary sites) and the
builder's walk (about 56 slot sites) lower the same checked body independently, and neither gives a
temporary a name the other knows. So the state's layout, made from the flow body's saved locals, had no
stated tie to the slots the builder keeps across a suspension.

END STATE: one lowering. The checked body is lowered once, to the flow body (the form the borrow check,
liveness and the Lean semantics of rule 57 are about), and the builder builds code FROM the flow body, for
every body, coroutine or not. Then a local is one thing by construction and there is no table to keep in
step. This is a rebuild of the builder's front and is started after tempo runs, as its own design.

UNTIL THEN, one owner of identities, never two conventions that must agree: the FLOW layer names every
local and temporary and EMITS the table (a binding's pattern to its local; an expression and its role
(value, borrow, awaitee, pin, result) to its local). The builder never invents a slot that lives across a
suspension: at each such site it LOOKS UP the local in that table and writes the value to that local's place
in the state; a value it wants to keep across a suspension that the table does not name is refused, naming
the expression: that is a gap in the flow body (and so in what the check knows is held across the await),
fixed there. The builder's own values (addresses, derived pointers) are recomputed after a resume. Test, a
rung each: every slot kept across a suspension names a flow local; plant: a builder-made slot is caught.

### The body: one lowering, built now (D119, the coordinator's design, 2026-11-16 23:50)

The owner asked for the end state now, in parallel with tempo ("if there is more of a SOTA architecture ...
please just send that as well"). This replaces "started after tempo runs" above.

What was read (rounds/wave-2311/G1-one-lowering-facts.md in the coordinator's notes): a checked body is
lowered THREE times, each over the same 37 expression forms: blaze-flow-lower (7.4k lines) to a check-only
skeleton (no type of any local, no callee, no constant's value, every operator a nameless aggregate, a
`match` one coarse switch, no unwind edge); blaze-build's walk (17k lines) to LIR, once per INSTANCE,
redoing scopes, temporaries, match tests and drop flags each time; blaze-consteval's compiler to its own
ops. On one night ten lanes each added a form to one of the three and two had to add it to a second. The
borrow check proves a different program from the one that runs, and no proof passes between them.

END STATE. One lowering, to one complete, typed, generic BODY; everything else reads it:

    checked body --lower (once a body)--> Body --check: loans, moves, saved locals (proof)
                                               --elaborate drops (body to body, once a body)
                                               --evaluate (constants, at ground arguments)
                                               --build (LIR, at ground arguments: one linear pass)

The body is a RECORD (crate `blaze-body`): flat tables read in place, keyed like the check that made it,
positions item-relative (rule 71), no pointer in it. It is generic: its types are the checked record's
portable types; an instance grounds them when it builds or evaluates. What it holds, so that no reader goes
back to the expression tree:

- `locals`: type, kind (return, argument, binding, temporary), mutability, and the identity the 21:07
  ruling named (binding's pattern, or expression and role). A local is one thing for every reader.
- `places`: interned (local, projections): field (index, type), deref, index (by a local), constant index
  and subslice (slice patterns), downcast (variant).
- `consts`: a literal's bits with its type; a string or byte data row; an item constant (key, arguments);
  a promoted; a fn item (key, arguments); a static's address; an anonymous constant.
- statements, fixed-width rows: `Assign(place, value)`, `StorageLive`, `StorageDead`, `Activate(loan)`,
  `FakeRead` (a guard's shallow borrow). A value is one of: use, borrow (kind, place, loan), raw address,
  binary op, checked binary op (value and overflow flag), unary op, cast (kind, operand, type), aggregate
  (tuple, array, ADT variant, closure, coroutine; operands), repeat, length, discriminant, a type's size or
  alignment, a thread-local's address.
- terminators: `Goto`; `Switch` (an operand; (value, block) cases; otherwise); `Call` (callee, arguments,
  destination, target, unwind; its lifetime part for the check); `Drop` (place, target, unwind); `Assert`
  (condition, expected, kind: overflow of an op, bounds (length, index), division or remainder by zero,
  a resumed-after-completion coroutine; target, unwind); `Suspend` (resume, cancel); `Return`;
  `Unreachable`; `Resume`; `Terminate`; `InlineAsm`. A callee is a fn item (key, arguments), a trait method
  (key, arguments: resolved to an impl or a vtable slot when the instance is ground), a value (a fn
  pointer), or an intrinsic. `unwind` is continue, a cleanup block, unreachable or terminate.
- blocks (rows, terminator, normal or cleanup), and the borrow facts the flow body has today (loans,
  origins, subsets), carried by the same ids.

Rules that make it better than a second MIR:

1. A `match` is lowered ONCE to tests (switches on discriminants and values, in Rust's order of arms and
   guards), and the borrow check, the evaluator and the builder all run those same tests.
2. Unwinding is in the body: every call, drop and assert that may unwind names its cleanup block, made once
   a body. `panic = "abort"` is a build fact: the builder then emits no pad; the body does not change.
3. Drops are written un-elaborated (`Drop(place)` means "if initialized") so the check sees Rust's rule;
   ELABORATION is one body-to-body pass that reads the check's move dataflow (blaze-flow moves.rs already
   computes `DropKind` and nothing reads it) and writes drop flags as locals. Once a body, not once an
   instance.
4. The builder takes the check's PROOF (rule 74): `build(body: &Checked<Body>, ..)`. No code is built from
   a body the check has not passed.
5. Building an instance is a linear pass over the rows with a substitution: layout, ABI, intrinsics,
   vtables, format templates and checks stay in blaze-build (layout.rs, cabi.rs, intrinsics.rs, vtable.rs
   are kept); scope logic, temporaries, match lowering and drop flags leave it. FLOOR: an instance's build
   reads each row once; the first measurement to quote is rows a second against today's walk on the same
   instances (the profile's build rows), and the target is within 10% of LIR emission alone.
6. Two instances whose builds READ the same facts (layouts, callee symbols) share one code record (point
   (b) above): the key is the body's digest and the facts read, never the type arguments as written.
7. The evaluator interprets the same rows over blaze-consteval's memory; its own compiler is deleted.
8. The Lean semantics of rule 57 is of this body; each body-to-body pass (elaboration, the coroutine
   transform when it moves here) is validated against it.

HOW IT IS BUILT (rule 79, then rule 48: a fresh core in new crates beside the old code; nothing old is
edited until the switch):

- P1 `blaze-body` (interfaces first): the tables and ids above; a builder API; a printer and parser of a
  text form (tests are written in it); `verify` (every operand typed and of the type its use wants; every
  block ends in one terminator; an unwind edge enters only a cleanup block; a local is live where used);
  the record form (blaze-format, read in place). Compiles alone.
- P2 `blaze-body-lower`: checked body to body, on the same task stack as today's lowerings: (a) operators,
  casts, places, literals, constants; (b) calls, methods, closures, fn pointers, operators that are calls;
  (c) patterns and `match` tests; (d) scopes, temporaries, drops and cleanup blocks; (e) async and
  coroutines; (f) `format_args!`, statics, `asm!`. Its signature and lifetime reading (3k lines of
  blaze-flow-lower) is kept and moved, not rewritten.
- P3 the check over the body: blaze-flow's loans, moves and saved-locals analyses read `blaze-body`.
- P4 drop elaboration (body to body) from the check's dataflow.
- P5 the evaluator over the body (blaze-consteval's machine and memory kept, its compiler replaced).
- P6 the builder over the body (a linear pass; blaze-build's layout, ABI, intrinsics, vtables kept).
- P7 the switch: front2 calls the new path; flow-lower, the walk and the evaluator's compiler are deleted.

PROOF AT EACH STEP (rule 78: the two builders are one seam with two instantiations, tested equal): every
x0 ladder rung and the std census run through both paths and must print the same; the body's interpreter
against the built code on generated bodies (the tier fuzzer of (e), one level up); a planted bug in each
pass is caught.
