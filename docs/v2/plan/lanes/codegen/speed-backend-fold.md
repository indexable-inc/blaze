---
status: plan
summary: Speed of folding functions that differ only in constants (#1292) - masking a prepared Cranelift function (`crates/hir-ty-codegen/v2/fold.rs`) and grouping a build's candidates (`crates/blaze-codegen/src/fold.rs`) - against LLVM MergeFunctions, lld's and mold's ICF and GCC's IPA ICF; what each cost before and after the 2026-10-29 pass, and the run-time cost of thunks on hot code.
date: 2026-10-29
---
# Folding's hot paths (speed-backend-fold)

Fold masks every foldable function of a dev build (`masked`, once per candidate, from
`v2/backend.rs::masked`, cached in the store under `keys::masked`), then groups all of a build's
candidates at once (`groups`, from `driver.rs`), then builds and compiles each group
(`build`/`merge`). The first two are the hot paths; `merge` runs per group.

## The benchmarks

- **LLVM MergeFunctions**: `StructuralHash` (a 64-bit hash of opcodes, types and the CFG,
  constants left out) buckets the functions, then `FunctionComparator` compares each candidate
  against the bucket's representatives instruction by instruction, with values numbered by
  first use. One pass over the IR to hash, one more per comparison.
- **GCC IPA ICF**: a hash of each function's body (`sem_function::get_hash`), congruence classes
  split by hash, then by the references each member makes until stable, then a full
  `equals` per pair.
- **lld ICF**: section contents and relocation targets hashed, classes refined in rounds by the
  class of each relocation's target until a fixed point; parallel over sections.
- **mold ICF**: the fastest known. A digest per section (contents plus relocations, SHA-256
  truncated), then parallel rounds where each section's digest is re-hashed with its targets'
  digests until the class count stops changing; equal digests are one class, with no
  pairwise comparison at all.

Folding here differs from all four in one way: members need not be identical, only equal
once constants are masked (Relax's `merge_similar`), and their differing constants become
parameters. Masking is MergeFunctions' structural hash made exact. Grouping is mold's
digest-equals-class step, then a first-fit split by parameter count, which none of them have.
References to other functions are compared by target, not by class, so no fixed-point
refinement is needed (a recursive function folds by its own key, which is a place like any
other).

## `masked` (`crates/hir-ty-codegen/v2/fold.rs`)

**Before**: Cranelift's printer. `dfg.display_inst(inst).to_string()` per instruction,
`name.display(None).to_string()` per name, `format!` per entity and type, then the text
re-tokenized to renumber entities through two `HashMap`s (SipHash). That is 3-6 heap
allocations and two SipHash lookups per instruction, and a `String` the size of the printed
function. It also lost information: the printer leaves out an atomic's memory flags, and shows a
global value load's flags by their handle number, not their contents.

**Now**: a structural walk over `InstructionData`, every format matched with no `_` arm, so a
new Cranelift format is a compile error and not a silent hole. It writes opcode, controlling
type and operands as fixed-width little-endian words into one per-thread buffer that is reused.
Entities (values with aliases resolved, blocks, function refs, signatures, global values,
stack slots, pool constants) are renumbered by first use through a `Vec<u32>` per kind,
indexed directly by the entity's own number, and written out whole on first use: a
signature, a stack slot, a pool constant's bytes, an external function with its signature, a
global value with its base chain. `iconst`, `f32const` and `f64const` immediates, and every
named function and global value, go to the constant column. Every other immediate is written
whole: memory flags by their data, condition codes, trap codes, offsets, shuffle masks, jump
tables, exception tables (tags, defaults, contexts) and user stack maps. Each list is
length-prefixed and each block and instruction tagged, so equal words are equal IR. The shape
is one BLAKE3 hash of the words (domain `blaze v2 fold ir 1`; the hashed bytes are new with
the walk, so the domain is new). Nothing is allocated per instruction. A function allocates
its `Masked` (the 32-byte shape and the constants) and nothing else once the thread's buffers
have grown.

Compared with MergeFunctions, this is one linear pass, like `StructuralHash`, but exact,
like `FunctionComparator`, because the whole masked IR goes into a 256-bit hash rather than
the 64-bit structural one. Two functions are compared by 32 bytes, never re-walked.

`data` (a function's own data object's content, by what it points at) memoizes the
objects it reaches through relocations, so an object reached by several paths is hashed once,
not once per path, which was exponential in the depth of a shared chain.

**Tests** (in the file): the printer-based masking as it was, kept as the model (`tests::text`).
Over 300 seeded random functions (loads and stores with three flag sets, compares, selects,
calls with and without stack maps, function and symbol addresses, stack addresses, `iconst` of
two widths, `f32`/`f64`/vector constants, traps, atomics, jumps, `brif`, jump tables,
`try_call` with exception tables, cold blocks, testcase and user names, a namespace `names`
refuses), each with renumbering noise (unused signatures, functions, global values, stack slots,
pool constants, memory flags, blocks and dead instructions declared in between), 12
one-choice mutations and one other seed: text-equal iff shape-equal, same constants, and
refused together. Two
targeted tests cover what the text missed (an atomic's flags, a global value load's flags). One
test checks that the per-thread buffers give the same answer after a bigger function.

## `groups` (`crates/blaze-codegen/src/fold.rs`)

**Before** (now `tests::reference`): a `BTreeMap<(Settings, ContentId), Vec<Candidate>>` (one
`Vec` per bucket and per candidate's cells), and a first-fit split that, per candidate and per
part, cloned the part, rebuilt its plan (varying constants, a `Vec<Vec<Constant>>` of columns,
a quadratic parameter search) and dropped it. That is \(O(\text{parts} \cdot \text{members}^2 \cdot n)\)
work and allocations per bucket. Each place constant was hashed with BLAKE3 (`target_id`)
per member just to compare targets.

**Now**:
- No BLAKE3 per candidate (fastcompile-ident-hashcons 4, 2026-10-29): a candidate's bucket ID
  is its `Form`'s content ID (shape, size and the constants' kinds, without their values,
  under the unchanged `blaze v2 masked 2`), computed once when the form is interned. The
  driver keeps one `driver::Masks` across builds, by stored key: an unchanged function is
  neither read from the store nor hashed again, a new one is hashed once. Then one stable sort
  of `u32` indices by (settings, bucket ID, key), so each bucket is a run: members as index
  ranges, with no `Vec` per bucket. The bound for an unchanged build is one lock-free probe
  per candidate (a 32-byte compare on a tag match, ~10 ns) against a BLAKE3 of its whole
  masked IR before (~1 cycle a byte, so ~1 µs for a 1 KiB shape): 100× less per candidate.
- The split is incremental: a part keeps, per constant, whether it varies and its class
  (constants of one class are equal in every member). Whether a member fits is one pass over its
  `n` constants against at most `max_params + 1` representatives, and adding it re-splits
  the classes with one sort of `n` positions. All buffers are reused across buckets; parts'
  members are a CSR (`starts`, `flat`).
- Cells hold `&Target` and compare targets whole. `target_into` encodes every field with a tag
  and length prefix, so two targets are equal exactly when their `target_id`s are. The hash is
  now computed only for a group's shape, for the first member's fixed places, instead of for
  every place constant of every bucket member. The bytes hashed are unchanged, so no domain
  changes.

Compared with mold, bucketing is the same (one digest per function, classes by equal digest)
without refinement rounds, since targets are compared, not classed. It is single-threaded: a
build's candidates are thousands, not mold's millions of sections, and the per-candidate hash
is already paid in `masked`, on the workers, and cached in the store. Sorting by the 32-byte
bucket ID before the key is a `memcmp`; a 128-bit prefix would compare faster but would reorder
buckets, and so groups of one key across two buckets, which the output must keep.

**Tests**: the existing permutation test, plus a differential test against `tests::reference`
(400 seeded inputs × 4 limit sets, three shapes, repeated keys with other constants, two
settings, few values and targets, so buckets are large and split), plus
`targets_compare_as_their_ids_did`: 2000 random target pairs of every kind, where
`Cell` equality equals `target_id` equality.

## `build` / `merge`

One `clone` of member 0's IR for the body. Each member is walked again (`walk`, the same
buffers) for its constants' sites, with no IR clone. Each thunk is built fresh, with its
member's signature cloned once. `merge` reuses one `Vec<Target>` for every thunk's references.
Nothing per member beyond that.

## Code speed: thunks on hot code (recommendation)

A folded member's callers call its thunk, which materializes the constants and calls the
body: a call, a frame and a return, because Cranelift accepts `return_call` only between `tail`
functions and blaze's use the platform convention. The profit test (`profitable`) counts
bytes only, so a group is folded however hot its members are. Where that costs run time:

- **Small members.** A body of a few instructions (an accessor, `len`, a `From` impl) with a
  thunk of 2 + `params` instructions spends most of each call in the thunk. `profitable`
  already refuses most of these (the saved bytes must beat the thunks plus an eighth of the
  body), but at the margin, about 16-40 instructions with 2-4 members, a fold saves a few dozen
  bytes and adds a call to every call.
- **`#[inline]` members.** Inlining is blaze's (`inline` in the IR builder), and a member that
  was inlined into its callers is not called through its thunk there. But an `#[inline]`
  function that stays out of line (because its caller is in another unit, or it is too big) is
  a sign it is hot, and folding it adds the indirection to that path.
- **The body's indirect calls.** A varying callee becomes `call_indirect` in the body, which a
  branch predictor handles well per call site but which blocks the callee being inlined by
  any later pass.

What would bound it, in order of value:
1. **A sibling call in the thunk** (`return_call` under the platform convention, a request
   already in `v2/requests/codegen.md` section 5, forwarded to integration). A thunk becomes a
   few moves and a jump, and a folded call costs about what it cost unfolded. This removes
   most of the cost without giving up any folding.
2. **Until then, a run-time floor in `profitable`**: refuse a group whose body is under
   about 4 × (`thunk_size` + `params`) instructions, so the thunk is at most a fifth of a
   call. The candidate's `#[inline]` hint and size are known to the driver, not to `Masked`,
   so leaving `#[inline]` members out is the driver's (`driver.rs`, lane codegen), as it
   already leaves out C-variadic and address-significant functions.

I have not changed `profitable` or `Limits`. Either change trades code bytes for run time, and
neither keeps both no worse, which the brief makes the bar; the choice and its numbers belong to
a measured dev build, not to this pass.
