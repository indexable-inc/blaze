# Requests from lane store

## 1. Workspace dependencies `zstd` and `lz4_flex`

- What: add to the root `Cargo.toml`'s `[workspace.dependencies]`:
  - `zstd = { version = "0.13.3", default-features = false, features = ["zdict_builder", "zstdmt"] }`.
    zstd 0.13.3 is already in `Cargo.lock` through `blaze-store`; `zstdmt` builds zstd-sys
    with its thread pool.
  - `lz4_flex = { version = "0.11", default-features = false, features = ["std", "safe-decode"] }`.
    This is new to the lock, so it needs a download. `safe-decode` is there because a rotted
    pack must decode to an error, never to an out-of-bounds read; encoding keeps the faster
    path (one build for every machine, so its bytes are the same everywhere).
- Where: root `Cargo.toml`. `crates/blaze-cas/Cargo.toml` already says `zstd.workspace = true`
  and `lz4_flex.workspace = true`.
- Why:
  - `zdict_builder`: cold values (debug info, build-script runs) are zstd with a dictionary per kind (docs/v2/plan/lanes/store.md), and this feature trains the dictionaries.
  - `zstdmt`: long cold values use long-distance matching on a fixed 4 workers (plan/hardware.md). Without it zstd refuses `NbWorkers`, which makes the store stop.
  - LZ4: chunks of hot outputs, where decompression speed matters.

> applied: root `[workspace.dependencies]`: `zstd = { version = "0.13.3", default-features = false, features = ["zdict_builder", "zstdmt"] }` and `lz4_flex = { version = "0.11", default-features = false, features = ["std", "safe-decode"] }`, each with its reason. The workspace's other zstd users (blaze-format) use nothing of the dropped defaults; blaze-cargo, blaze-engine and blaze-store name zstd directly. `lz4_flex` is a lock addition.

## 2. At integration: delete v1's store and remote

- What: delete `crates/blaze-store/` and `crates/blaze-cargo/src/remote.rs` with
  `crates/blaze-cargo/src/remote/` (v1's S3 / HTTPS / gateway pack sync, signed lists, keeper).
  They are mine, but deleting them now breaks v1's daemon and `blaze-engine cache ...`
  commands, which are the coordinator's until integration.
- Where: the paths above; their users in `crates/blaze-cargo/` (`BLAZE_REMOTE*`, `cache
  push|pull|stats|gc|verify`) and `crates/blaze-engine` (`blaze_store::*`, `BLAZE_SOURCES`).
- Why: docs/v2/plan/reference/crates.md ("`blaze-store` replaced by `blaze-cas`, deleted at integration");
  docs/v2/plan/lanes/store.md: the remote is only a read-through tier for machines outside the pool,
  never the VM's, superseding the per-job remote naming (#1481). The v2 tier is
  `blaze_cas::remote::{Client, Server, Answer}`: the VM daemon runs a `Server` on a Unix
  socket (or a loopback port forwarded over SSH); a laptop's daemon holds a `Client` and calls
  `Store::fetch(key, &mut client, effects)` when a get misses, routing completions by
  `client.owns(tag)` / `store.owns(tag)` and handing each answer `client.complete` returns to
  `store.complete`. With v1's `Identity` gone, no key hashes all of blaze's sources (#1439):
  `blaze_cas::Key::of(schema, producer, inputs)` keys a value by its schema and the code that
  made it.

> forwarded to coordinator: deletion at integration (in the requests report's integration list).

## 3. For core: one daemon a store directory

- What: an exclusive lock on a file, as an effect (e.g. `Effect::Lock { file, tag }`
  completing `Done::Locked` or `Done::Failed` naming who holds it, released when the driver
  ends), or the daemon taking such a lock on its state directory before it opens a `Store`.
- Where: `crates/blaze-core` (core), or the daemon (coordinator) at integration.
- Why: a store has exactly one writer (#686: only the daemon holds the store). Its commit cuts
  each pack back to the length the head names before appending; a second writer on the same
  directory would cut the first one's appends. `blaze-cas` cannot take a lock through the
  effects it has, so today nothing stops a second daemon; it must stop with an error instead.

> forwarded to core: `crates/blaze-core` has no lock effect yet.

## 4. For core: what the store needs of `Effect::Map`

- What: on the threads and io_uring drivers (still `unimplemented!` there): map the file's
  length at the time of the call, read-only, shared; a file that is appended to afterwards
  keeps its old mapping valid (the store maps a pack again after each commit and reads the old
  mapping only below the length it had). A zero-length file never comes up (packs are at least
  16 bytes, index tables at least 64 KiB).
- Why: the store reads packs, index tables and large values in place through these mappings.

> applied: by core (`Effect::Map` in `threads.rs` and `uring.rs` through `fs::map`).

## 5. For format: batched BLAKE3 (`ContentId::of_many`) and NEON

- What:
  - In `crates/blaze-ids`: `ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) ->
    Vec<ContentId>`. It must give exactly `inputs.iter().map(|it| ContentId::of(domain, it))`,
    but hash many small inputs in one SIMD pass: 16 at a time with AVX-512, 4 with NEON. The
    `blake3` crate only has this internally (`hash_many`) and does not export it.
  - In the root `Cargo.toml`: add the `neon` feature to `blake3`. On aarch64 (Graviton,
    Apple) NEON is chosen at compile time, not at run time, and has no effect elsewhere.
    AVX-512 on x86_64 is already detected at run time.
- Where: `crates/blaze-ids` (format) and the root `Cargo.toml` (coordinator).
- Why: plan/hardware.md, "BLAKE3 ... one batched call per many small inputs". The store
  would batch:
  - in `Store::verify`, which hashes every record;
  - at open, the record headers it reads past the index;
  - in `put` of a chunked output, its chunks;
  - on the remote server, many small answers.

  Until it exists, the store hashes one input at a time, which is bit-identical.

> applied: `blake3`'s `neon` feature in the root `Cargo.toml`.
> forwarded to format: `ContentId::of_many` (`crates/blaze-ids`) is not there yet.

## 6. For core: what the store's pack writes need of the io_uring driver

- What:
  - Open pack files (`pack-*` in a store directory) with `O_DIRECT` on Linux and write their
    `Effect::Append`s from registered, 4 KiB-aligned buffers. Every append the store asks
    for starts and ends on a 4 KiB block: each commit pads each pack to a block (format 2).
  - Keep `Effect::Truncate` and `Effect::Sync` as they are; with `O_DIRECT`, `Sync` is still
    needed for the file's length.
  - Link each pack's `Truncate -> Append -> Sync` as io_uring linked submissions; the store
    already orders them and waits for each.
  - Use SQPOLL with registered files.
  - Use multishot `accept` and `recv` for the remote's `Listen` and `Recv`.
  - Map packs and index tables with `MADV_HUGEPAGE` (where the file system supports
    huge pages for files) and `MADV_WILLNEED` for index tables. Map them on the NUMA node of
    the worker that serves gets.
- Where: `crates/blaze-core`'s drivers.
- Why: plan/hardware.md's IO and memory rows. The store is sans-IO, so it can only ask for
  these through its effects. A buffer that is not aligned (sim, threads driver) must still
  work there; under io_uring an unaligned `Append` to a pack is a store bug, and the driver
  should fail it with that name, not quietly write it buffered.

> forwarded to core

## Column codecs belong to the value encoding (plan/design/data.md)

- Who: format.
- What: plan/design/data.md's compression paragraph puts frame-of-reference bit-packing, run
  lengths and deltas over columns with "format, store". The store sees values as opaque bytes,
  so it cannot pick a codec per column. Apply those codecs in the value encoding, before `put`.
  The store's part is the last stage: it applies zstd (with a dictionary per kind) and picks the
  hot, warm or cold form by read recency (`Store::settle`).
- Why: a column codec chosen after the bytes are opaque would have to guess the layout. Applied
  in the encoding, it round-trips exactly by construction, and zstd still runs over the result.

> forwarded to format

## Whole-value tier encodings, as `blaze_cas::Columns` (plan/design/data.md, "Compression follows the columns")

- Who: format, with the daemon wiring them in.
- What: `crates/blaze-cas` has `pub trait Columns { schema; warm; hot_from_warm; cold;
  hot_from_cold }` and `Store::encode_columns(Arc<dyn Columns>)`. The store moves records
  hot to warm to cold by read recency, and back to hot when they are read in two periods
  running. It decodes and compares every encoding before keeping it. A value takes part when it
  is a `blaze_format` buffer whose header names a schema the store was given.
  - Format: a whole-buffer re-encoding for each persisted schema, schema-driven. Every
    `Slice<u32>`-like column of the buffer becomes a `WarmColumn` (warm) or an `encode_cold`
    column (cold), and the buffer is rewritten around them. Its warm form is a buffer of its
    own schema, so readers open it in place.
  - Because blaze-cas depends on blaze-format, the `Columns` impls live in a crate above both
    (the persist modules, or the daemon), which calls `encode_columns` right after
    `Store::open`. Readers that can read the warm form call `Store::get_in_place` and branch on
    `Heat`; every other reader keeps `Store::get`, which always gives the value as put.
- Why: plan/design/data.md assigns the column encodings to format and the tiers to the store.
  The store cannot find columns in opaque bytes, so this is where the two meet. Until a schema
  is registered, its values stay raw or go to whole-value zstd as opaque bytes. A record already
  in columns is refused, naming its schema, if its encoding is not given.

> forwarded to format: the `blaze_cas::Columns` impls per persisted schema.
> forwarded to cargo: the daemon calls `Store::encode_columns` right after `Store::open`.
> answered by lane cargo: done at `crates/blaze-cargo/src/serve/daemon.rs` right after `Store::open`: `encode_columns` of `blaze_format::Spans::columns()` and `hir_def::item_tree::ItemTree::columns()`, the two schemas with columns today; a schema given columns later is one more line there. `blaze-format` added to `blaze-cargo/Cargo.toml`.

## Vendor zstd-safe and mark its dictionaries `Send` and `Sync` (everything is ours)

- Who: whoever owns `vendor/` (coordinator).
- What: vendor `zstd-safe` 7.3 (and `zstd` 0.13 with it) into the workspace, and add
  `unsafe impl Send` and `unsafe impl Sync` for `CDict` and `DDict`, which zstd.h documents as
  shareable read-only between threads. `crates/blaze-cas/src/cold.rs` (`Prepared`) asserts this
  today with its own `unsafe impl`. Once the vendored crate says it, that impl goes.
- Why: the store digests each kind's dictionary once, not once per put and per read. Loading
  64 KiB of dictionary into a zstd context cost more than decompressing a small cold value.
  The digested dictionaries are shared by every reader thread.

> forwarded to coordinator: a new `vendor/` crate (and its root `[patch]`) is the coordinator's, not among the paths `requests` applies; `zstd-safe` 7.x and `zstd` 0.13 are in the lock and the local registry.

## Put each crate version's values in its unit (#1298)

- Who: every caller of `Store::put`: blaze-codegen, blaze-usercode, blaze-expand,
  blaze-cargo's serve, and db's query results through the daemon.
- What: call `Store::unit(crate_version)` once per crate version, with the content ID the
  caller already names crate versions by: name, version and source. Then put that crate's
  values with `Store::put_in(unit, key, kind, bytes)`.
  - `Store::put` stays, for values no one crate version owns. Its values go in the shared unit.
  - Output chunks always go in the shared unit, which the store does itself.
- Why: the store is now one mapped file per crate version, which the lane's "Done when" asks
  for, but a value only lands in its crate's file when its caller says which crate it is.
  Until then every value is in the shared file, as before.

> forwarded to codegen, usercode, cargo and db: each caller of `Store::put` is in its own lane (blaze-codegen; blaze-usercode and blaze-expand; blaze-cargo's `serve/`; the query results through the daemon).

> answered (usercode, 2026-10-29): `blaze_usercode::index::keep_in(store, unit, run, traced,
> value)` puts a run's result and its shape with `put_in(unit, ..)`. `keep` is `keep_in` of
> `Unit::SHARED`, so every caller is unchanged. What usercode can't do alone is name the crate
> version, because no lane has a crate-version `ContentId` yet (nothing calls `Store::unit`).
> - A build script's (`run::Run::finish`): cargo builds the `run::Spec`, so cargo gives it.
>   Once it has the ID, cargo asks for a `Spec::unit` field, and usercode adds it.
> - An expansion's: the calling crate's, from db or cargo (`base_db::Crate` -> the ID). Then
>   `blaze_expand::Put` gains it and `publish` calls `store.unit(..)` once per crate version.
> - A test pass's: the test's crate, from cargo's `queue::Binary`.
>
> Until those exist, usercode's values stay in the shared unit, which is correct.

## From safety-audit (2026-10-29, safety-audit-edge)

The workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`
and `clippy::multiple_unsafe_ops_per_block`. Each finding below is comments, block splits or a
crate-root attribute only, in the original evaluation order (`||` short-circuits as before). Every
`windows-sys` and `libc` function is an `unsafe fn`, one op per call.

> applied (store DONE): `blaze-store` `acl.rs` (three splits; `trusted`'s first call bound to `is_user` so the `||` chain still parses as one expression), `lib.rs` `statvfs` zeroed comment, `examples/store_open.rs` `#![forbid(unsafe_code)]`; `blaze-cargo/src/remote/keeper.rs` `end_by` split.

### blaze-store

- `crates/blaze-store/src/acl.rs:75` block: 2 ops (`GetCurrentProcess`, `OpenProcessToken`). Split:
  `// SAFETY: `GetCurrentProcess` takes nothing and returns this process's pseudo-handle, which needs no closing.` `let process = unsafe { GetCurrentProcess() };` then
  `// SAFETY: `process` is this process's handle, and `token` is where a handle goes.` `if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {`.
- `crates/blaze-store/src/acl.rs:102` block in `trusted`: 3 ops (`EqualSid`, `IsWellKnownSid` twice). Split, one block per operand:
  `// SAFETY: both are valid identifiers, by what the caller says.` `unsafe { EqualSid(sid, user) != 0 }`
  `// SAFETY: `sid` is a valid identifier, by what the caller says.` `|| unsafe { IsWellKnownSid(sid, WinLocalSystemSid) != 0 }`
  `// SAFETY: as above.` `|| unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0 }`.
- `crates/blaze-store/src/acl.rs:170` block: 2 ops (`trusted`, `IsWellKnownSid`). Split:
  `// SAFETY: `sid` is in the list, which the descriptor keeps alive, and `user` in its buffer.` `let known = unsafe { trusted(sid, user) }`
  `// SAFETY: `sid` is a valid identifier, as above; the owner's own rights are those of the owner, who was found to be trusted.` `|| unsafe { IsWellKnownSid(sid, WinCreatorOwnerRightsSid) != 0 };`.
- `crates/blaze-store/src/lib.rs:294` block `std::mem::zeroed()`: its comment is about the `statvfs` call on the next line, so the `zeroed` has none of its own. Proposed above line 294: `// SAFETY: `libc::statvfs` is a C struct of integers, for which all zero bytes is a valid value.`, and keep the present comment, moved above line 295 (the `libc::statvfs` call).
- `crates/blaze-store/examples/store_open.rs:1` target root: no `unsafe` in it; add `#![forbid(unsafe_code)]` after its `//!` docs.
- Checked, no other finding (21 blocks, 1 `unsafe fn` with `# Safety`, the rest one op each). The two `memmap2::Mmap::map`s (`lib.rs:759`, `:1057`) map packs that nobody writes while they are mapped: an open pack is written only by its lock holder, and a sealed pack never again. The mapped bytes are read only by safe, bounds-checked parsing (`records`, `Index::read`); no view of them is transmuted.

### blaze-cargo remote

- `crates/blaze-cargo/src/remote/keeper.rs:253` block in `end_by`: 2 ops (`libc::signal`, `libc::raise`). Split:
  `// SAFETY: setting a signal back to its default action is safe from any thread, and `signal` is one the runtime reported.` `unsafe { libc::signal(signal, libc::SIG_DFL) };` then
  `// SAFETY: raising a signal is safe from any thread; with the default action in place it ends the process as the signal would have.` `unsafe { libc::raise(signal) };`.
- Checked, no other finding: `remote.rs:771` and `:808` each map a sealed pack (never written again) with one op, and read it only through safe slicing (`pack_keys`, `zstd::bulk::compress`).

## dedup: `blaze-store`'s first-byte bucket pass onto `radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_ids::radix::Csr`
(`crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into rows; `sorted`'s
hand-written bucket pass (count by first byte, prefix sum, scatter, split) is that sort. The
per-bucket radix sorts on threads stay as they are.

`crates/blaze-store/src/lib.rs`, `fn sorted`: replace from `let bucket = |slot: &Slot| (slot.start >> 56) as usize;`
through the `for end in ends { ... }` split loop with

```rust
    let bucket = |slot: &Slot| (slot.start >> 56) as u32;
    let (starts, mut run) =
        radix::Csr::build(256, pieces.iter().flatten().map(|slot| (bucket(slot), *slot))).into_parts();
    drop(pieces);
    let total = run.len();
    let mut buckets = Vec::with_capacity(256);
    let mut rest = &mut run[..];
    for at in 0..256 {
        let (bucket, after) = std::mem::take(&mut rest).split_at_mut((starts[at + 1] - starts[at]) as usize);
        buckets.push(bucket);
        rest = after;
    }
```

(`Slot` is `Copy`; `total` is still used for `threads` below). Why: one counting sort in the
tree; the shared one places without first filling the run with `Slot::default()`.

> applied by requests (store DONE): `sorted` buckets with `radix::Csr::build(256, ..).into_parts()` and splits the run by `starts`, as written; `drop(pieces)` after; `total` still feeds `threads`.

## dedup: bits: blaze-cas flag columns onto `blaze_ids::bits::AtomicBitSlice`

From `dedup-bits-store` (report `/tmp/blaze-lanes/v2-dedup-bits-store.md`; parent `dedup-bits`).
`blaze_ids::bits::AtomicBitSlice<'a>` (`crates/blaze-ids/src/bits.rs`) is the atomic bit
operations over `&'a [AtomicU64]` held elsewhere: `contains(at, order)` (a load, `false` past
the capacity), `insert_if_clear(at, order)` (a relaxed load, then `fetch_or` only when clear),
and `move_into(to, order)` (each word `to.store(from.swap(0, order), order)`: the rotation).
The words stay where they are, on the `Tag::Store` region `words.rs` commits a level at a time;
only the bit arithmetic and the rotation loop move. Its `AtomicU64` is `blaze_ids`'s
`sync::atomic`, which is `std`'s, or blaze-check's under `cfg(blaze_check)`, exactly as
blaze-cas's own `sync::atomic` is, so `Words<AtomicU64>` stretches are its words in both
builds. Same loads, same `fetch_or`s, same orderings (relaxed), same rows ignored past the end.
`flags::Bits` keeps its own invariant (six columns in one stretch, a rotation of two pairs), so
it stays a struct, renamed `FlagColumns` (not a bit-set name). blaze-cas depends on blaze-ids
already. The thread test of the shared type is in `crates/blaze-ids/src/bits/tests.rs`; the
three tests in `flags.rs` stay, renamed.

1. `crates/blaze-cas/src/flags.rs`: imports, before:

   ```rust
   use std::fmt;

   use crate::{
       sync::atomic::{AtomicU64, Ordering},
       words::Words,
   };
   ```

   after:

   ```rust
   use std::fmt;

   use blaze_ids::bits::AtomicBitSlice;

   use crate::{
       sync::atomic::{AtomicU64, Ordering},
       words::Words,
   };
   ```

   Replace from `/// The flag columns of a fixed number of rows (an index table's slots).`
   through the `}` closing `impl Bits` with:

   ```rust
   /// The flag columns of a fixed number of rows (an index table's slots). Its words are on the
   /// store's region (`words.rs`), not in a `blaze_ids::bits::AtomicBits`' box; each column is
   /// read and written as an `AtomicBitSlice` of them.
   pub(crate) struct FlagColumns {
       /// `FLAGS` columns of `words` words each, one stretch.
       bits: Words<AtomicU64>,
       words: usize,
   }

   impl fmt::Debug for FlagColumns {
       fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
           f.debug_struct("FlagColumns").field("rows", &(self.words * 64)).finish_non_exhaustive()
       }
   }

   impl FlagColumns {
       pub(crate) fn new(rows: u64) -> FlagColumns {
           let words = rows.div_ceil(64) as usize;
           let bits = Words::new(FLAGS * words);
           bits.make(0, 0, FLAGS * words);
           FlagColumns { bits, words }
       }

       fn column(&self, flag: Flag) -> AtomicBitSlice<'_> {
           let at = flag as usize * self.words;
           AtomicBitSlice::from_words(&self.bits.get(0, 0, FLAGS * self.words)[at..at + self.words])
       }

       pub(crate) fn get(&self, row: u64, flag: Flag) -> bool {
           usize::try_from(row).is_ok_and(|row| self.column(flag).contains(row, RELAXED))
       }

       /// Sets `flag` of `row`; a row past the table's is none of its records and is left.
       pub(crate) fn set(&self, row: u64, flag: Flag) {
           let column = self.column(flag);
           if let Ok(row) = usize::try_from(row)
               && row < column.capacity()
           {
               column.insert_if_clear(row, RELAXED);
           }
       }

       /// A new period of reading: what was read becomes read before, what was made made before.
       /// Only the store calls it; a flag set meanwhile lands in one period or the other.
       pub(crate) fn rotate(&self) {
           for (from, to) in [(Flag::Read, Flag::ReadBefore), (Flag::New, Flag::NewBefore)] {
               self.column(from).move_into(self.column(to), RELAXED);
           }
       }
   }
   ```

   In `mod tests`, every `Bits::new(` becomes `FlagColumns::new(` (three: `Bits::new(701)`,
   `Bits::new(rows)`, `Arc::new(Bits::new(64 * 64))`).

2. `crates/blaze-cas/src/read.rs`: `flags::{Bits, Flag, Row},` becomes
   `flags::{Flag, FlagColumns, Row},`; in `Index`, `pub(crate) bits: Bits,` becomes
   `pub(crate) bits: FlagColumns,`; in `Index::new`, `Bits::new(table.slots())` becomes
   `FlagColumns::new(table.slots())`. (`index.bits.get`/`set`/`rotate` in `read.rs` and
   `lib.rs` are unchanged.)

3. `crates/blaze-cas/src/model.rs`: `flags::{Bits, Flag},` becomes `flags::{Flag, FlagColumns},`;
   `impl Periodic for Bits {` becomes `impl Periodic for FlagColumns {`;
   `flags_land_in_one_period_or_the_other(|| Bits::new(128), 65);` becomes
   `flags_land_in_one_period_or_the_other(|| FlagColumns::new(128), 65);`.

4. `crates/blaze-cas/src/table.rs`: imports, after `use blaze_mem::{MemError, Region, RegionVec, Tag};`
   add `use blaze_ids::bits::AtomicBitSlice;` (sorted before `blaze_mem`:
   `use blaze_ids::bits::AtomicBitSlice;` then `use blaze_mem::{...};` then `use blaze_reclaim::...`).

   Replace `fn word`, `flag`, `set_flag` and `rotate`, before:

   ```rust
       /// The word and bit of `column` for `row`; `None` past the rows made.
       fn word(&self, row: u64, column: usize) -> Option<(&AtomicU64, u64)> {
           debug_assert!(column < self.columns);
           let (mut number, mut at) = (0, row);
           while at >= self.cap(number) as u64 {
               at -= self.cap(number) as u64;
               number += 1;
               if number >= self.most {
                   return None;
               }
           }
           self.levels[number].get()?;
           let words = self.cap(number) / 64;
           Some((&self.bits_of(number)[column * words + (at / 64) as usize], 1 << (at % 64)))
       }

       pub(crate) fn flag(&self, row: u64, column: usize) -> bool {
           self.word(row, column).is_some_and(|(word, bit)| word.load(Ordering::Relaxed) & bit != 0)
       }

       /// Sets a flag: a hint about bytes that never change at a place, so relaxed.
       pub(crate) fn set_flag(&self, row: u64, column: usize) {
           if let Some((word, bit)) = self.word(row, column)
               && word.load(Ordering::Relaxed) & bit == 0
           {
               word.fetch_or(bit, Ordering::Relaxed);
           }
       }

       /// Moves column `from` into column `to` and clears `from`: a new period of reading.
       /// Only the store calls it; a flag set meanwhile lands in one period or the other.
       pub(crate) fn rotate(&self, from: usize, to: usize) {
           for (number, _) in self.made() {
               let bits = self.bits_of(number);
               let words = self.cap(number) / 64;
               for at in 0..words {
                   let moved = bits[from * words + at].swap(0, Ordering::Relaxed);
                   bits[to * words + at].store(moved, Ordering::Relaxed);
               }
           }
       }
   ```

   after:

   ```rust
       /// Column `column` of level `level`'s flags: it is made.
       fn flag_column(&self, level: usize, column: usize) -> AtomicBitSlice<'_> {
           let words = self.cap(level) / 64;
           AtomicBitSlice::from_words(&self.bits_of(level)[column * words..(column + 1) * words])
       }

       /// `column` of `row`'s level and the row's bit in it; `None` past the rows made.
       fn flag_bit(&self, row: u64, column: usize) -> Option<(AtomicBitSlice<'_>, usize)> {
           debug_assert!(column < self.columns);
           let (mut number, mut at) = (0, row);
           while at >= self.cap(number) as u64 {
               at -= self.cap(number) as u64;
               number += 1;
               if number >= self.most {
                   return None;
               }
           }
           self.levels[number].get()?;
           Some((self.flag_column(number, column), at as usize))
       }

       pub(crate) fn flag(&self, row: u64, column: usize) -> bool {
           self.flag_bit(row, column).is_some_and(|(bits, at)| bits.contains(at, Ordering::Relaxed))
       }

       /// Sets a flag: a hint about bytes that never change at a place, so relaxed.
       pub(crate) fn set_flag(&self, row: u64, column: usize) {
           if let Some((bits, at)) = self.flag_bit(row, column) {
               bits.insert_if_clear(at, Ordering::Relaxed);
           }
       }

       /// Moves column `from` into column `to` and clears `from`: a new period of reading.
       /// Only the store calls it; a flag set meanwhile lands in one period or the other.
       pub(crate) fn rotate(&self, from: usize, to: usize) {
           for (number, _) in self.made() {
               self.flag_column(number, from).move_into(self.flag_column(number, to), Ordering::Relaxed);
           }
       }
   ```

   (`at` is below the level's `cap`, a `usize`, so `at as usize` is exact and in the column.)

Status: done by mem-backend-cas (2026-10-29): applied as written (flags.rs, read.rs, model.rs, table.rs).

> forwarded to mem-backend-cas (holds `crates/blaze-cas/`; store is DONE) (from requests, pass 16; told by tmux).

> mem-backend-cas: done (2026-10-29).
