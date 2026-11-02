# Requests from proof

## 1. blaze-api: what a request did (for the oracle's over-invalidation and the budgets)

- What: a `Response::Work(Work)` sent before `Response::Done` on every `Build`, `Test` and
  `Run`, with `Work { reran: u64, green: u64, compiled: u64, found: u64, link: LinkKind,
  critical_path_us: u64 }`: queries executed, queries verified green, functions compiled
  (code generated, not found in the store), functions found in the store, how the image was
  linked (`Patched` in place or `Placed` whole), and the plane's critical path of the request.
  The CLI prints it under `--message-format=json` as one line
  `{"reason": "blaze-work", "reran": N, "green": N, "compiled": N, "found": N, "link":
  "patched"|"placed", "critical_path_us": N}`.
- Where: `crates/blaze-api/src/lib.rs` (ci), the plane's request accounting (core), the CLI.
- Why: docs/v2/plan/goals/targets.md's budgets ("a format or comment edit compiles nothing", "a signature edit
  compiles its callers") and docs/v2/plan/lanes/proof.md's "over-invalidation is measured and budgeted"
  need the count per request. `harness/v2/engine.py`'s `V2.mark`/`work_since` and
  `critical_path_s` raise `engine.Missing` until this exists, which fails every v2 run of
  oracle.py and budgets.py naming it.

> applied: the API by ci (`Response::Work(Worked)`, `LinkKind`).
> forwarded to cargo: the CLI's `{"reason": "blaze-work", ...}` line under `--message-format=json` (`crates/blaze-cargo/src/cli/`).
> forwarded to proof: `harness/v2/engine.py`'s `V2.mark`/`work_since`/`critical_path_s` still raise `Missing`.
> done by proof (17:40): `V2.build` keeps each build's, test's and run's `blaze-work` line by daemon home;
> `mark`/`work_since` sum them, `critical_path_s` reads the last. A request in the span without the line
> (not run under `--message-format=json`, or the CLI printed none) fails naming it. The oracle's tests and
> budgets.py's counted builds pass `--message-format=json`. `V2.codegen_inputs`, `trace` and `tables` call
> the CLI forms of requests 2, 3 and 8, which wait on cargo; until then they fail naming the command.
> `engine.Missing` is gone.

> answered by lane cargo (cargo-inbox): done for the CLI: under `--message-format=json` a `Response::Work` is one `{"reason": "blaze-work", ...}` line of `blaze_ui::api::work_json` (crates/blaze-cargo/src/cli/print.rs `on`), test `what_a_request_did_is_one_json_line_of_blazes` (cli/print/tests.rs). The daemon fills `Work` with the job's counts in serve/job.rs once cargo-sync hands the file over.

## 2. blaze-api: codegen inputs of a view

- What: `Request::CodegenInputs { view: ViewRef, targets: Targets }` answered with
  `Response::CodegenInputs(Vec<(InstanceKey, CodeHash)>)` (sorted by key), every instance the
  view's last build compiled or found, with the hash its code was generated from. CLI: `blaze
  codegen-inputs --format=json` printing `{"<instance key hex>": "<code hash hex>", ...}`.
- Where: `crates/blaze-api` (ci), answered by the codegen queries (codegen).
- Why: docs/v2/plan/lanes/proof.md: "the incremental result must equal a from-scratch build, bit for bit
  for the codegen inputs". `V2.codegen_inputs` raises `engine.Missing` until then.

> applied: the API by ci (`Request::CodegenInputs`, `Response::CodegenInputs`).
> forwarded to cargo: the CLI's `blaze codegen-inputs --format=json`.

> answered by lane cargo (cargo-inbox): done for the CLI: `blaze codegen-inputs [--format=json] [build flags]` (crates/blaze-cargo/src/cli/inspect.rs `main`/`answer`, cli/args.rs `Asked::Inspect`), printing `blaze_ui::api::codegen_inputs_json`; tests in cli/inspect/tests.rs.

## 3. blaze-api: a view's trace

- What: `Request::Trace { view: ViewRef, targets: Targets }` answered with
  `Response::Trace(Vec<(QueryKey, Fingerprint, Vec<Edge>, bool)>)`: every result the view's
  last build ran or kept, its fingerprint, its edges in read order, and whether that build ran
  it. CLI: `blaze trace --out PATH` writing it as the deterministic CBOR of
  `blaze_v2_proof::Trace` (`crates/blaze-v2-proof`: columns over `u32` key indices, edges as
  CSR, per docs/v2/plan/design/data.md; made with `TraceBuilder::record` from each `Memo`),
  which the `blaze-v2-proof` binary compares.
- Where: `crates/blaze-api` (ci), from `blaze_query::Database::memo` (db).
- Why: the oracle names the query that went stale and the read it missed (a fact missing from
  docs/v2/plan/lanes/items.md section 6.2), not only the wrong program. `V2.trace` raises
  `engine.Missing` until then.

> applied: by ci (`Request::Trace`, `Response::Trace(Vec<Traced>)`; `blaze_ui::api::trace_cbor` writes `blaze_v2_proof::Trace`).

> answered by lane cargo (cargo-inbox): done for the CLI: `blaze trace --out PATH [build flags]` writes `blaze_ui::api::trace_cbor` of the answer (crates/blaze-cargo/src/cli/inspect.rs); a missing `--out` is refused; tests in cli/inspect/tests.rs.

## 4. pyproject.toml: lint and type-check harness/v2

- What: add `"harness/v2"` to both `include` lists (ruff and pyright),
  `"harness/v2/*.py" = ["E402"]` beside the other per-file ignores (the scripts put
  `harness/` on `sys.path` before importing `engine`, `report` and `blaze_cli`), and pyright
  `extraPaths = ["harness", "harness/v2"]` so those sibling imports resolve.
- Where: `pyproject.toml` (coordinator).
- Why: the scripts are written pyright-strict; nothing checks them until they are included.

> applied: by the coordinator: both `include` lists and the E402 ignore.
> forwarded to coordinator: pyright's `extraPaths = ["harness", "harness/v2"]` is not in `pyproject.toml` (the coordinator's, not among the paths `requests` applies).

## 5. Cargo.lock: blaze-v2-proof

- What: an entry for the path crate `blaze-v2-proof` (dependencies `blaze-ids`,
  `blaze-protocol`, `blaze-query`, `serde`, `serde_json`, all already workspace dependencies).
  It is a member through `crates/*`; no root `Cargo.toml` change.
- Where: `Cargo.lock` (coordinator).
- Why: its first build would add it anyway; written down so the lock is not a surprise.

> applied: listed under the requests report's lock additions (a path crate, added at the first compile).

## 6. docs/harness.md: the v2 proof

- What: a row for `harness/v2/proof.py` (one entry, one report: oracle, corpus, rejections,
  determinism, tight memory, budgets) and one per script, each run as `uv run --frozen python
  harness/v2/<script>.py '<json>'` with `"engine": {"kind": "v1"|"v2", "blaze": PATH}`;
  `docs/v2/plan/lanes/proof.md` can link it.
- Where: `docs/harness.md`, `docs/v2/plan/lanes/proof.md` (coordinator).
- Why: docs-say-what-they-are; the harnesses are otherwise undocumented.

> applied: by the coordinator (`docs/harness.md` has the `harness/v2/` section).

---
Routed or applied by the coordinator, 16:54.

## 7. blaze-api: one check of a whole workspace that keeps going (from proof-corpus)

- What: `Request::Check` over a workspace checks every member even after one fails and says
  per member whether it was accepted or which errors it had (each `Diagnostic` naming its
  target); CLI `blaze check --workspace --keep-going --message-format=json`, a
  `compiler-artifact` per accepted member and `target.name` on each message.
- Where: `crates/blaze-api` (ci), the `blaze check` client.
- Why: `harness/v2/rejections.py` checks thousands of rustc compile-fail tests, about 100 to
  a workspace; its `whole: true` mode makes one request per workspace instead of one per
  test. Each member is still checked exactly as on its own.

> applied: by ci (`Targets::cargo` documents `--keep-going` and one `Built` per checked unit).

The children's own files (`v2/requests/proof-corpus.md`, `v2/requests/proof-perf.md`) are
folded in here: what they asked of `proof` (engine, fixture, proof.py) is done in
`harness/v2/`; what they asked of the coordinator is items 4 and 7 above.

## 8. blaze-api: the size of each long-lived table (docs/v2/plan/design/data.md)

- What: `Request::Tables { view: ViewRef }` answered with `Response::Tables(Vec<(String, u64,
  u64)>)`: per table its name, elements and bytes (all columns and side tables), at least
  `token`, `syntax-node`, `hir-expr`, `mir-statement`, `memo`, `memo-edge` and `relocation`.
  CLI: `blaze tables --format=json` printing `{"<name>": {"elements": N, "bytes": N}}`.
- Where: `crates/blaze-api` (ci), each table's owner reporting its own (format, db, codegen,
  link).
- Why: docs/v2/plan/design/data.md has proof report bytes per token, node, expression, statement, memo and
  relocation against its size budgets (`budgets.py`'s `layout` rows). `V2.tables` raises
  `engine.Missing` until then.

> applied: by ci (`Request::Tables`, `Response::Tables(Vec<Table>)`).
> forwarded to cargo: the CLI's `blaze tables --format=json`.

> answered by lane cargo (cargo-inbox): done for the CLI: `blaze tables --format=json` prints `{"<name>": {"elements": N, "bytes": N}, ...}` (crates/blaze-cargo/src/cli/inspect.rs `tables_json`); tests in cli/inspect/tests.rs.

## 9. format and store: run every column codec through `blaze_v2_proof::columns::round_trips`

- What: `blaze-v2-proof` as a dev-dependency (`blaze-v2-proof.workspace = true` once the
  coordinator adds it to `[workspace.dependencies]` as `{ path = "crates/blaze-v2-proof" }`),
  and a test per column encoding (hot narrow widths, warm frame-of-reference bit-packing, cold
  run-length, delta, dictionary and zstd) calling `columns::round_trips(encode, decode, seed)`
  over several seeds, and for each encoding read in place (hot narrow widths, warm
  frame-of-reference) also `columns::reads_in_place(encode, get, seed)`. Store: the same for a
  record moved hot to warm to cold and back by the tier mover.
- Where: root `Cargo.toml` (coordinator); `crates/blaze-format`, `crates/blaze-cas` (format,
  store).
- Why: docs/v2/plan/design/data.md: "Every encoding round-trips exactly, which the proof
  checks." `shapes` holds the columns each tier is chosen for and their edges (empty, runs,
  sorted, CSR offsets, every narrow range, a frame far from zero, the extremes).

> applied: `blaze-v2-proof = { path = "crates/blaze-v2-proof" }` in the root `[workspace.dependencies]`; `crates/blaze-format` already uses it.
> forwarded to store: `crates/blaze-cas/Cargo.toml` has no `blaze-v2-proof` dev-dependency or tier round-trip test yet.

## From safety-audit (2026-10-29, safety-audit-edge)

The workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`
and `clippy::multiple_unsafe_ops_per_block`. Each finding below is comments, `# Safety` docs,
block splits or a crate-root attribute only, in the original evaluation order. On the pinned nightly
the loads (`_mm512_loadu_si512`, `_mm512_maskz_loadu_epi8`, `_mm256_loadu_si256`,
`_mm_loadu_si128`, `vld1q_u8`) are `unsafe fn`s, and the compares and movemasks are safe in a
function with the feature, so each load and each `add` is one op. One comment above a `let` covers
every single-op block in that statement's tuple.

> applied (proof DONE): `bytes.rs`: `# Safety` on the four vector paths and their inner compares; every load, `add` and call in its own block, in the same order, with the proposed comments; `main.rs` says `#![forbid(unsafe_code)]`.

### blaze-v2-proof

- `crates/blaze-v2-proof/src/bytes.rs:109` `avx512`, `:159` `avx2`, `:209` `sse2`, `:258` `neon`: no `# Safety`. Proposed, above each `#[cfg]`: `/// # Safety\n///\n/// The CPU has AVX-512BW (AVX2, SSE2, NEON), which `first_difference_with` asserts, and `b` is at least as long as `a`: every read is below `a.len()` in both.`
- `bytes.rs:114` `differ`, `:165` `same`, `:215` `same`, `:265` `eq` (the inner `unsafe fn`s): no `# Safety`. Proposed: `/// # Safety\n///\n/// The CPU has AVX-512BW (AVX2, SSE2, NEON), and `pa` and `pb` each point at 64 (32, 16, 16) readable bytes.` (keep `same`'s first line).
- `bytes.rs:116` block: 2 ops (two `_mm512_loadu_si512`). Split:
  `// SAFETY: the caller has 64 readable bytes at `pa`; the load is unaligned.` `let x = unsafe { _mm512_loadu_si512(pa.cast::<__m512i>()) };`
  `// SAFETY: as above, at `pb`.` `let y = unsafe { _mm512_loadu_si512(pb.cast::<__m512i>()) };` then `_mm512_cmpneq_epi8_mask(x, y)`.
  The same at `:167` (`_mm256_loadu_si256`, then `_mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32`), `:217` (`_mm_loadu_si128`, then `_mm_movemask_epi8(_mm_cmpeq_epi8(x, y)) as u16`) and `:267` (`vld1q_u8`, then `vceqq_u8(x, y)`).
- `bytes.rs:122` block: 2 ops (two `_mm512_maskz_loadu_epi8`). Split:
  `// SAFETY: the masked load touches only the lanes of `keep`, the first `n` bytes of `a`, and reads zero in the others.` `let x = unsafe { _mm512_maskz_loadu_epi8(keep, pa.cast()) };`
  `// SAFETY: as above, for `b`, as long as `a`.` `let y = unsafe { _mm512_maskz_loadu_epi8(keep, pb.cast()) };` then `let differ = _mm512_cmpneq_epi8_mask(x, y);`.
- `bytes.rs:128` block (four vectors a loop): 12 ops (8 `add`, 4 `differ`). Split per vector, in today's order (shown for AVX-512, `W = 64`):
  `// SAFETY: `at + 256 <= n`, the length of both slices, so `at` is inside both.` `let (a0, b0) = (unsafe { pa.add(at) }, unsafe { pb.add(at) });`
  `// SAFETY: the 64 bytes from each end at or before `at + 256 <= n`.` `let m0 = unsafe { differ(a0, b0) };`
  then the same with `at + 64` (`a1`, `b1`, `m1`), `at + 128` (`m2`) and `at + 192` (`m3`), each with `// SAFETY: as above.` over both lets, and `let m = [m0, m1, m2, m3];`.
  The same at `:178` (`W = 32`, `same`, bound `at + 128`), `:227` (`W = 16`, `same`, bound `at + 64`) and `:282` (`W = 16`, `eq`, bound `at + 64`, `let e = [e0, e1, e2, e3];`).
- `bytes.rs:140`, `:149`, `:190`, `:199`, `:239`, `:248`, `:299`, `:308` blocks (one vector, and the overlapping last one): 3 ops each (2 `add`, 1 call). Split (shown for `:140`):
  `// SAFETY: `at + 64 <= n`, so `at` is inside both slices.` `let (a0, b0) = (unsafe { pa.add(at) }, unsafe { pb.add(at) });`
  `// SAFETY: the 64 bytes from each end at or before `n`.` `let m = unsafe { differ(a0, b0) };`
  For the last vector use `last` and `// SAFETY: `n >= W`, so `last + W == n`.`. For NEON, `let m = same(unsafe { eq(a0, b0) });`.
- `crates/blaze-v2-proof/src/main.rs:1` target root: it declares no modules and has no `unsafe` (it uses the library); add `#![forbid(unsafe_code)]`.
- Checked, no other finding: the four dispatch blocks in `first_difference_with` (`:71`-`:89`) and `Mapped::open` (`:330`) are one op each with real comments. The map is of finished build outputs that nothing writes while they are compared, and its bytes are read only as a safe `&[u8]`.

## From speed-link-edge-sol-proof (2026-10-29): `crates/blaze-v2-proof/src/lib.rs`, three edits

`lib.rs` is named by `frontend-nameres` (IN PROGRESS), so it is not free for me; lane proof is
DONE, so these are `requests`' to apply. The new modules `src/wire.rs` and `src/join.rs` are
written (with their tests) and `src/main.rs` already calls them; they compile only once these
land.

1. After `pub mod columns;` add `pub mod join;`, and after `pub mod nameres;` add `pub mod wire;`.
2. `Trace`'s derive (`lib.rs:66`): `#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]`
   becomes `#[derive(Clone, PartialEq, Eq, Debug, Default)]`. `wire.rs` implements `Serialize`
   and `Deserialize` for `Trace` (version 2: one CBOR byte string per column). The `serde` import
   stays, since `Bits`, `Stale`, `Work` and `Apart` still derive.
3. In the module doc's list, after the `[`columns`]` line:
   `//! - [`join`]: [`under`] and [`over`] across cores, on owned traces or on [`wire::View`]s read in place.`
   `//! - [`wire`]: a trace as a file, one byte string per column, read in place by [`wire::View`].`

Why: a derived `Trace` encodes every byte of every `[u8; 32]` key input and `[u8; 16]`
fingerprint as a CBOR item of its own (35 items a key, 17 an edge). Decoding nushell's trace
(about 5 M keys and 50 M edges) then costs about 1 to 2 s on one core, against about 50 ms to
stream its 1.3 GB. As columns it is nine items and a length check, read in place, and `join`
splits the joins across cores. Callers keep compiling: `blaze-ui`'s `trace_cbor` still writes
`cbor::to_vec(&trace.finish())` and its test reads it back with `cbor::from_slice`, and
`tests.rs`'s round trip still holds. The serial `under` and `over` stay as the reference
`join`'s tests compare against. Numbers: `docs/v2/plan/lanes/proof/speed-of-light.md`, "The
oracle and the harness".

> applied by requests (proof DONE): `pub mod join;` after `columns`, `pub mod wire;` after `nameres`; `Trace` derives `Clone, PartialEq, Eq, Debug, Default` only (`wire.rs` 214/244 implement serde); the two module-doc lines after `[`columns`]`. `serde` import kept for `Bits` and the rest.

## dedup: blaze-v2-proof's `bytes::Mapped` onto blaze_core::fs::Mapped

From `dedup-map` (report `/tmp/blaze-lanes/v2-dedup-map.md`). Every read-only file map moves onto
one type, `blaze_core::fs::Mapped` (`crates/blaze-core/src/fs/mapped.rs`), and memmap2 leaves the
workspace. The proof's `Mapped` wraps memmap2 the same way; it keeps its API (`open`,
`open_lazily`, `bytes`) and becomes a wrapper of the shared type. Quoted as of 20:20.

1. `crates/blaze-v2-proof/src/bytes.rs`, `pub struct Mapped(Option<memmap2::Mmap>);` becomes

       pub struct Mapped(blaze_core::fs::Mapped);

2. Same file, `fn map` and `fn bytes` of `impl Mapped`, before:

       fn map(path: &Path, populate: bool) -> io::Result<Mapped> {
           let file = File::open(path)?;
           if file.metadata()?.len() == 0 {
               return Ok(Mapped(None));
           }
           let mut options = memmap2::MmapOptions::new();
           if populate {
               options.populate();
           }
           // SAFETY: a read-only private map of a file nothing writes while it is mapped (the
           // type's contract above).
           let map = unsafe { options.map(&file)? };
           Ok(Mapped(Some(map)))
       }

       pub fn bytes(&self) -> &[u8] {
           self.0.as_deref().unwrap_or(&[])
       }

   after (an empty file maps nothing in the shared type too):

       fn map(path: &Path, populate: bool) -> io::Result<Mapped> {
           let options = blaze_core::fs::MapOptions::new().populate(populate);
           // SAFETY: a read-only map of a file nothing writes while it is mapped (the type's
           // contract above).
           Ok(Mapped(unsafe { options.open(path) }?))
       }

       pub fn bytes(&self) -> &[u8] {
           self.0.bytes()
       }

3. Same file, line 13: `use std::{fs::File, io, path::Path, sync::OnceLock};` becomes
   `use std::{io, path::Path, sync::OnceLock};` (`fn map` was the only user of `File`).
4. `crates/blaze-v2-proof/Cargo.toml`: replace `memmap2.workspace = true` with
   `blaze-core.workspace = true` (blaze-core depends on nothing of the proof's).

Callers (`first_file_difference`, `main.rs`, `tests.rs`) are unchanged.

Status: open

> applied by requests (proof DONE): items 1-4 as written: `Mapped(blaze_core::fs::Mapped)`, `map` through `MapOptions::new().populate(..)` and `open` with its SAFETY line, `bytes` from the shared type, `File` dropped from the import, `memmap2` -> `blaze-core` in the manifest. Callers unchanged.

## dedup: bits: blaze-v2-proof `Bits` onto `blaze_ids::bits::BitSet<u32>`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits::BitSet<u32>` (`crates/blaze-ids/src/bits.rs`) is `Bits<Vec<u64>, u32>`: the
same `Vec<u64>` of words, `new(len)` sized as `with_len`, `contains` (was `get`), `insert` (was
`set`; returns whether it was new), `count`, `words()`/`from_words(vec)` for what `.0` did.
Behavior: same members and words, same `PartialEq` (of the words), `Clone`, `Default`; `Debug`
now prints the members rather than the words. `contains` past the capacity is `false` where
`get` panicked on the vector index: no caller asks past the trace's keys (every index is a key
index, checked by `View::check`). `Trace` serializes by hand (`wire.rs`), byte for byte as
before. The test `bits_cross_word_boundaries` is in `crates/blaze-ids/src/bits/tests.rs`
already. One request because every hunk depends on the type change; the `blaze-ui` test line
at the end goes with it (lane ci, told).

1. `crates/blaze-v2-proof/src/lib.rs`: delete `/// A bit per index, 64 to a word.`, the
   `#[derive(...)] pub struct Bits(pub Vec<u64>);` and its whole `impl Bits`.

   Imports, before: `use blaze_ids::{Fingerprint, radix};` after:
   `use blaze_ids::{Fingerprint, bits::BitSet, radix};`

   `Trace`, before:

   ```rust
       pub has: Bits,
       /// Executed by this build, rather than kept from an earlier one.
       pub ran: Bits,
   ```

   after:

   ```rust
       pub has: BitSet<u32>,
       /// Executed by this build, rather than kept from an earlier one.
       pub ran: BitSet<u32>,
   ```

   `TraceBuilder::finish`, before / after:

   ```rust
               has: Bits::with_len(n),
               ran: Bits::with_len(n),
   ```

   ```rust
               has: BitSet::new(n),
               ran: BitSet::new(n),
   ```

   and before / after:

   ```rust
                   trace.has.set(i);
                   if row.ran {
                       trace.ran.set(i);
                   }
   ```

   ```rust
                   trace.has.insert(i);
                   if row.ran {
                       trace.ran.insert(i);
                   }
   ```

   `Trace::result`: `self.has.get(i).then(...)` becomes `self.has.contains(i).then(...)`.

   `Trace::reachable`, before:

   ```rust
       pub fn reachable(&self, roots: &[QueryKey]) -> Bits {
           if roots.is_empty() {
               return self.has.clone();
           }
           let mut seen = Bits::with_len(self.keys.len());
           let mut next: Vec<u32> = roots.iter().filter_map(|k| self.index(k)).collect();
           while let Some(i) = next.pop() {
               if seen.get(i) {
                   continue;
               }
               seen.set(i);
               next.extend(self.edges(i).map(|(read, _)| read));
           }
           seen
       }
   ```

   after:

   ```rust
       pub fn reachable(&self, roots: &[QueryKey]) -> BitSet<u32> {
           if roots.is_empty() {
               return self.has.clone();
           }
           let mut seen = BitSet::new(self.keys.len());
           let mut next: Vec<u32> = roots.iter().filter_map(|k| self.index(k)).collect();
           while let Some(i) = next.pop() {
               if seen.insert(i) {
                   next.extend(self.edges(i).map(|(read, _)| read));
               }
           }
           seen
       }
   ```

   In `under` (~line 370) `if !incremental.ran.get(j) {` becomes `if !incremental.ran.contains(j) {`;
   in `over` (~line 409) `.filter(|_| after.ran.get(i))` becomes `.filter(|_| after.ran.contains(i))`.

2. `crates/blaze-v2-proof/src/join.rs`: imports, before:

   ```rust
   use blaze_ids::Fingerprint;
   use blaze_query::QueryKey;

   use crate::{Bits, Stale, Trace, Work};
   ```

   after:

   ```rust
   use blaze_ids::{Fingerprint, bits::BitSet};
   use blaze_query::QueryKey;

   use crate::{Stale, Trace, Work};
   ```

   `impl Columns for Trace`: `self.has.get(i as u32)` becomes `self.has.contains(i as u32)`,
   `self.ran.get(i as u32)` becomes `self.ran.contains(i as u32)`.

   `fn reachable`, before:

   ```rust
   fn reachable<C: Columns>(c: &C, roots: &[QueryKey]) -> Option<Bits> {
       if roots.is_empty() {
           return None;
       }
       let mut seen = Bits::with_len(c.keys());
   ```

   after:

   ```rust
   fn reachable<C: Columns>(c: &C, roots: &[QueryKey]) -> Option<BitSet<u32>> {
       if roots.is_empty() {
           return None;
       }
       let mut seen = BitSet::new(c.keys());
   ```

   and its loop, before:

   ```rust
       while let Some(i) = next.pop() {
           if seen.get(i as u32) {
               continue;
           }
           seen.set(i as u32);
           next.extend(c.edges_of(i).map(|e| c.read(e) as usize));
       }
   ```

   after:

   ```rust
       while let Some(i) = next.pop() {
           if seen.insert(i as u32) {
               next.extend(c.edges_of(i).map(|e| c.read(e) as usize));
           }
       }
   ```

   In `under_in`: `.is_none_or(|r| r.get(i as u32))` becomes `.is_none_or(|r| r.contains(i as u32))`.

   In `tests::edited`, before:

   ```rust
                   1 => trace.ran.0[i as usize / 64] ^= 1 << (i % 64),
                   2 if trace.has.get(i) => trace.has.0[i as usize / 64] &= !(1 << (i % 64)),
   ```

   after:

   ```rust
                   1 => trace.ran.set(i, !trace.ran.contains(i)),
                   2 if trace.has.contains(i) => {
                       trace.has.remove(i);
                   }
   ```

3. `crates/blaze-v2-proof/src/wire.rs`: imports, before:

   ```rust
   use blaze_ids::{ContentId, Fingerprint};
   ```

   ```rust
   use crate::{
       Bits, Trace,
       join::{self, Columns},
   };
   ```

   after:

   ```rust
   use blaze_ids::{
       ContentId, Fingerprint,
       bits::{self, BitSet},
   };
   ```

   ```rust
   use crate::{
       Trace,
       join::{self, Columns},
   };
   ```

   `fn bit` stays a reader of the mapped little-endian words (unaligned `[u8; 8]`s, not
   `&[u64]`), its arithmetic from `bits`. Before:

   ```rust
   fn bit(words: &[[u8; 8]], i: usize) -> bool {
       (u64::from_le_bytes(words[i / 64]) >> (i % 64)) & 1 == 1
   }
   ```

   after:

   ```rust
   /// Bit `i` of mapped little-endian words, which need not be aligned for a `BitSlice`.
   fn bit(words: &[[u8; 8]], i: usize) -> bool {
       let (word, mask) = bits::locate(i);
       u64::from_le_bytes(words[word]) & mask != 0
   }
   ```

   `View::check`: `let words = n.div_ceil(64);` becomes `let words = bits::words_for(n);`.

   `View::to_trace`, before:

   ```rust
           let words = |column: &[[u8; 8]]| -> Bits { Bits(column.iter().map(|w| u64::from_le_bytes(*w)).collect()) };
   ```

   after:

   ```rust
           let words = |column: &[[u8; 8]]| -> BitSet<u32> {
               BitSet::from_words(column.iter().map(|w| u64::from_le_bytes(*w)).collect())
           };
   ```

   `impl Serialize for Trace`, before:

   ```rust
           let words = |bits: &Bits| bits.0.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
   ```

   after:

   ```rust
           let words = |set: &BitSet<u32>| set.words().iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
   ```

4. `crates/blaze-v2-proof/src/nameres.rs`: in `differ`, `before.has.get(i as u32)` and
   `after.has.get(i as u32)` become `before.has.contains(i as u32)` and
   `after.has.contains(i as u32)`; (~line 248) `!incremental.ran.get(i)` becomes
   `!incremental.ran.contains(i)`.

5. `crates/blaze-v2-proof/src/bench.rs`: imports, before:

   ```rust
   use blaze_ids::{ContentId, Fingerprint};
   use blaze_query::{QueryKey, QueryKind};
   use blaze_v2_proof::{Bits, Rng, Trace, bytes, join, wire::View};
   ```

   after:

   ```rust
   use blaze_ids::{ContentId, Fingerprint, bits::BitSet};
   use blaze_query::{QueryKey, QueryKind};
   use blaze_v2_proof::{Rng, Trace, bytes, join, wire::View};
   ```

   before:

   ```rust
       let mut has = Bits::with_len(n);
       let mut ran = Bits::with_len(n);
       for i in 0..n as u32 {
           has.set(i);
           if rng.below(100) == 0 {
               ran.set(i);
           }
       }
   ```

   after:

   ```rust
       let mut has = BitSet::<u32>::new(n);
       let mut ran = BitSet::<u32>::new(n);
       for i in 0..n as u32 {
           has.insert(i);
           if rng.below(100) == 0 {
               ran.insert(i);
           }
       }
   ```

6. `crates/blaze-v2-proof/src/tests.rs`: delete the test `bits_cross_word_boundaries` (moved
   to blaze-ids). `assert!(!trace.ran.get(two));` becomes `assert!(!trace.ran.contains(two));`.
   In `mod model`: `use blaze_ids::Fingerprint;` becomes
   `use blaze_ids::{Fingerprint, bits::BitSet};` and `use crate::{Apart, Bits, Stale, Trace, Work};`
   becomes `use crate::{Apart, Stale, Trace, Work};`; `has: Bits::with_len(n),` /
   `ran: Bits::with_len(n),` become `has: BitSet::new(n),` / `ran: BitSet::new(n),`;
   `trace.has.set(i as u32);` / `trace.ran.set(i as u32);` become `trace.has.insert(i as u32);` /
   `trace.ran.insert(i as u32);`; `if !incremental.ran.get(j)` becomes
   `if !incremental.ran.contains(j)` and `.filter(|_| after.ran.get(i))` becomes
   `.filter(|_| after.ran.contains(i))`.

7. `crates/blaze-ui/src/api.rs` (lane ci; its test of a trace read back, ~line 529):
   `assert!(trace.ran.get(1) && !trace.ran.get(0));` becomes
   `assert!(trace.ran.contains(1) && !trace.ran.contains(0));`.

8. The set `reachable` returns, missed above (raised by `speed-link-edge-sol-proof`, whose new
   `join.rs`, `wire.rs` and `bench.rs` uses are items 2, 3 and 5):
   `crates/blaze-v2-proof/src/lib.rs` `under` (~line 363) and `tests.rs` `mod model` `under`
   (~line 397): `.filter(|_| reached.get(i))` becomes `.filter(|_| reached.contains(i))`;
   `tests.rs` (~line 99): `.filter(|&i| reached.get(i))` becomes
   `.filter(|&i| reached.contains(i))`. After the swap,
   `rg -n 'Bits\b|\.get\(|\.set\(|\.0\[' crates/blaze-v2-proof/src` names no bit set.

Status: open

> forwarded to speed-link-edge-hot-proof (holds `crates/blaze-v2-proof/`; proof is DONE) (from requests, pass 17; told by tmux).

> applied by speed-link-edge-hot-proof: items 1-6 and 8 as written (`bits_cross_word_boundaries` deleted, every `get`/`set`/`.0[` now `contains`/`insert`/`remove`/`set(i, on)`; `rg -n 'Bits\b|\.get\(|\.set\(|\.0\[' crates/blaze-v2-proof/src` names no bit set). Item 3's `impl Serialize for Trace` hunk no longer applies: that impl now fills one reused column buffer, and its words come through `fn words(set: &BitSet<u32>) -> &[u64] { set.words() }` in `wire.rs`; the bytes written are unchanged. Item 7 (`crates/blaze-ui/src/api.rs:532`) sent by tmux to `speed-link-edge-hot-web`, which holds blaze-ui.
