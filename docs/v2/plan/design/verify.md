---
status: plan
summary: How blaze checks its own store and incremental builds: BLAZE_VERIFY_STORE recomputes a sampled fraction of store hits and compares bytes, and a Hegel edit fuzzer holds every incremental build to a clean one through the invalidation oracle (blaze-v2-proof).
date: 2026-11-02
---
# Verifying the store and incremental builds

Part 4 of the content-keyed incremental kernel (the kernel, purity and identity lanes are
the other three). A store that hands back a value under a key promises that computing the
value again gives the same bytes. Two checks hold it to that promise.

## Who owns what

Since 2026-11-02 the store check and the invalidation oracle are one system in two crates:

- `crates/blaze-store-check` is the runtime layer: the setting, **the one sampler**
  (`Setting::sampled`, which every sampled check uses), `Hit`, `Mismatch`, `compare`, `check`,
  `Recompute`, and **the one byte comparison** (`blaze_store_check::bytes`: `first_difference`, by
  the widest SIMD the CPU has, and each path by name for tests). It depends on nothing but
  `blaze-ids`, so the kernel and the driver can call it on every hit.
- `crates/blaze-v2-proof` is the oracle, run offline or from a test: the query trace, `under`
  (what is stale and the read it missed), `over` (reruns for nothing), `apart` (two maps' keys
  that differ), and `kernel`, which puts the kernel's entries in the trace as rows (below). Its
  `bytes` adds the multi-core and file comparisons on top of `blaze_store_check::bytes`, which it
  re-exports.
- The edit fuzzer (`crates/hir-ty-codegen/v2/edits.rs`) uses both: the store check on every
  hit, the oracle for its verdict.

Deleted as duplicates: the oracle's own `sample` (its one caller was a test; the sampler is
`Setting::sampled`), the store check's scalar `first_difference` (the SIMD one won), and the
fuzzer's field-by-field comparison (`apart` won).

## The store check: `BLAZE_VERIFY_STORE`

`BLAZE_VERIFY_STORE=<rate>` in a request's environment (the client's, as `BLAZE_SANDBOX`
is) recomputes that fraction of the build's store hits and compares bytes. The value is a
bare rate from 0 to 1 (`0.01`, `1`) or one JSON object that refuses unknown fields,
`{"rate": 0.01, "seed": 7}`. Anything else is an error naming the variable. Unset or 0 is
off.

Which hits are sampled depends only on `(seed, identity)`: a BLAKE3 hash whose first eight
bytes fall below `rate` of their range. The same tree and setting check the same entries
every run (rule 22). Changing the seed rotates the sample through the store.

A sampled hit is computed again from its inputs, never from the store or a memo. If the
bytes differ, that is a typed error (`blaze_store_check::Mismatch`) naming:

- the function,
- the identity it is stored under,
- the reads its entry recorded (none for today's content-keyed templates),
- the first differing byte, with both bytes and both lengths.

If the recompute fails where the store had a value, that is an error too. The check never
replaces the stored value and never falls back. It only fails the unit.

- `crates/blaze-store-check` holds the setting, the sampler, `Hit`, `Mismatch`, `compare` and
  `check`. It also holds the trait `Recompute`, which the kernel implements for each compute
  step it registers.
- Today's store-hit path is blaze-codegen's driver (`crates/blaze-codegen/src/driver.rs`):
  - `Build::verify_store(setting)` turns the check on. A sampled dev (Cranelift) template
    hit is prepared and emitted again.
  - A sampled release (stencil, LLVM) template hit is summarized and emitted again against
    its imports' exports.
  - In both cases the encoded template is compared with the stored bytes, and
    `Stats::verified` counts the checks.
  - blaze-cargo's `serve::unit::codegen::generate` reads the setting from the request.
- Not yet covered: a lazy build's (`Build::run_later`) entry functions, folded groups' bodies
  and thunks, summaries and statics. Each is a further `Out::verify` call at its own read.

When the kernel stores entries as `identity -> { output, reads, code }`, its lookup calls
`blaze_store_check::check` on each hit, passing the entry's recorded reads ([the
kernel](kernel.md#checks-lane-verify): `Kernel::get` does, and its recompute must make
the entry's reads as well as its bytes). A mismatch then names
the narrow reads (the layout of `Foo`, a const's value) that should have changed the identity
and did not, which is the missing input a key must cover (rule 18).

## The edit fuzzer: incremental equals clean

`crates/hir-ty-codegen/v2/edits.rs` is a Hegel property test. It renders a small real crate
on minicore from a model. The crate has:

- a struct whose fields are built and read,
- two consts,
- an `#[inline]` callee,
- two `cfg`-chosen functions,
- a generic and its instance,
- a static,
- a struct boxed by a small `alloc` of the fixture's, whose `impl Drop` comes and goes.

Each run builds with `heap_to_stack` (`hir-ty-mir`'s `HeapToStack`) on or off, which Hegel
draws; the smoke run does both. Hegel draws one to four edits:

- add a struct field,
- change a const,
- change the inlined callee's body,
- flip the `cfg`,
- swap two items,
- edit a comment,
- add or remove the boxed struct's `impl Drop`, which with `heap_to_stack` on moves its box
  to the stack or back to the heap,

- reformat (blank lines between items, the body's indent).

Each edit is a change to the model, so every text is a valid crate. After each edit:

1. The incremental build changes the one database in place. It builds with the memo and
   store that earlier builds left, under `BLAZE_VERIFY_STORE=1`, so every hit is also
   recomputed.
2. A fresh database of the same text builds with an empty memo and store.
3. The oracle judges the incremental build against the clean one (`verdict`), and reports
   every finding of the step at once:
   - the codegen inputs (each instance and its code hash) and the kernel entries (each
     instance and the key its dev template is kept under), by `apart`; an entry kept under
     another key than the clean build's names an identity that depends on history;
   - the kernel entries as a trace (`Built::kernel`, recorded by the driver; rows by
     `blaze_v2_proof::kernel::entry`, each one's fingerprint the hash of its bytes), by
     `under`. An entry taken from the store whose bytes differ from what the clean build
     made under the same key is named with its function: its identity misses an input its
     bytes depend on (rule 18). One with reads names the read that should have missed;
   - `over` against the build before: an entry compiled again under a key the store held
     is a missed hit;
   - code, relocations, data, statics, roots, symbols, items, debug names and digests by
     `apart`, a function's code by its first differing byte.
   Stats, puts and memo use are left out, since a warm build legitimately has other values
   for them.

Kernel entries enter the trace like queries: an entry is a row of kind `0x6800 + step`
(`blaze_query::kinds::KERNEL_ENTRIES`) over the key the store keeps it under, its
fingerprint its value's hash, ran when computed and kept when the store gave it; each read
its entry recorded is a row of kind `0x6c00 + read kind` (`KERNEL_READS`) whose fingerprint
is the hash it saw. What the kernel exposes for that is `Entry::reads()` on a hit and
`Got::Computed { key, value, reads }` on a compute (`kernel::record_got`). Codegen's dev
templates are keyed by the driver itself under `whole::key`, so the driver records them in
`Built::kernel`; a lazy build (`Build::run_later`) records none yet.

Hegel shrinks a failure to the shortest edit sequence that still breaks the property. That
sequence is saved as a `#[test]` calling `replay(&[..])`, named for the property it breaks.
The default is 12 cases of up to four edits on a seven-item crate, to stay inside the one-to-two
minute loop. `HEGEL_TEST_CASES=<n>` runs longer. `each_edit_kind_builds_incrementally_as_clean`
replays one edit of each kind as a smoke run.

## The identity property

The same model checks code identity (lane identity, `hir-ty-identity`), in
`crates/hir-ty-codegen/v2/edits/identity.rs`: a smoke run of each edit kind and a Hegel run.
After each edit, in a database of its own as another process would build it:

1. an edit of comments, formatting or item order keeps every root's compile identity
   (`instance_identity`, in a debug and in a release unit) and its `code_id`;
2. an edit of behaviour changes the `code_id` of every root that reaches what it edited
   (`Model::reached`: `entry` everything, `mix` its constant, `mode` the `cfg`, `read_s` the
   consts its static is made of, `wide` nothing), and of no other root;
3. the same program checked out at another path, its roots asked in the other order, has the
   same identities.

Every finding of a step is reported at once.
