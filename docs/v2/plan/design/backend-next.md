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
(`blaze_sim` tape, bounded: up to 6 blocks, 24 instructions a block, counted loops) builds
verified functions; the harness runs each through every `Tier` and compares outcomes. Tiers
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
