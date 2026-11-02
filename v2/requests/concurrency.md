# Requests to `concurrency` (owner of `crates/blaze-cas/`)

## From speed-link-edge-remote: the remote tier (2026-10-29)

Paths: `crates/blaze-cas/src/remote.rs`, and `Store::take_fetched`/`put` in
`crates/blaze-cas/src/lib.rs`. Before and after numbers:
`docs/v2/plan/lanes/store/speed-link-edge-remote.md`. Nothing outside `blaze-cas` calls
`blaze_cas::remote` yet, so the signature changes below break no caller. C1 is a memory bound,
so do it even if you skip the rest.

> forwarded to concurrency: C1-C8 below.

### C1. Server: bound the answers queued from one batch (remote.rs `Server::serve`, ~line 677)

- Now: `serve` answers every whole ask in `peer.buffer`. `QUEUED_MOST` is only checked in
  `pump` before the next `Recv`. One receive of `SERVER_RECV` (1024 asks) for large values
  queues up to 1024 frames: up to 1 TiB in memory.
- Change:
  - loop `while peer.queued <= QUEUED_MOST && peer.buffer.len() - start >= ASK_LEN`;
  - in `complete`'s `Done::Sent` branch, call `self.serve(conn, store, &mut corrupt, effects)`
    in place of `self.pump(conn, effects)`, so the asks left in the buffer are answered as sends
    drain (`serve` ends in `pump`; `pump` already holds the `Recv` back while over the bound).
- Test: put 4 values of 40 MiB each, send 4 asks in one receive, and check that at most two
  frames are queued before the first `Done::Sent`. All 4 still arrive, in order.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C2. Server: one `Vec` and one send per batch of small answers

- Now: `answer_frame` allocates a `Vec` per answer, and `pump` sends each alone. 100 missing
  answers cost 100 allocations and 100 syscalls of 49 bytes each.
- Change:
  - Replace `answer_frame`/`framed` with
    `fn write_frame(out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>))`. It pushes an 8-byte
    placeholder, writes, then patches the length at `start..start + 8`.
  - In `serve`, for each ask, check `store.get(&key)`'s length first:
    - if it is under `COALESCE = 1 << 18`, append the frame to `peer.out.back_mut()` when that
      exists and its `len()` is under `1 << 20`, else to a new `Vec` pushed back;
    - a longer value gets a `Vec` of its own, reserved to `8 + VALUE_HEADER + len`.
  - Entries of `peer.out` are never in flight (`pump` pops the one it sends), so appending to
    `back_mut()` is safe. `queued += frame_len` as now.
- Test: 100 asks in one receive (half missing, half 1 KiB values) give exactly one
  `Effect::Send`. It decodes to 100 frames in ask order.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C3. Client: one send per turn of fetches (`impl Remote for Client`, ~line 503)

- Now: `fetch` pumps at once, so the first ask of a turn is sent alone. The asks of the turn's
  other fetches wait for that `Done::Sent`: a second send, and up to one more round trip.
- Change: in `fetch`, when `Link::Up(up)` has `up.sending == Some(tag)`, look for that send still
  unsubmitted, `effects.iter_mut().rev().find_map(|e| match e { Effect::Send { tag: t, bytes, .. } if *t == tag => Some(bytes), _ => None })`.
  If it is found, `write_ask(bytes, key)` into it. Otherwise write to `self.out` as now.
  `waiting.push_back` as now.
- Test: 24 fetches with one `effects` `Vec` give exactly one `Effect::Send` of 24 × 40 bytes.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C4. Client: adopt the driver's receive `Vec` (`Client::complete`, `Role::Recv`, ~line 381)

- Change: `Done::Read(bytes)` →
  `if up.buffer.is_empty() { up.buffer = bytes } else { up.buffer.extend_from_slice(&bytes) }`.
- Saves a copy of every answer that arrives in one receive, and of the first receive of a long
  one. The existing split-at-N tests cover it.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C5. Client: hand a long first frame on without a copy (`take_answers`, ~line 439)

- Now: `Done::Read(body.to_vec())` copies every value once more.
- Change:
  - `Done::Read` carries the whole frame, length prefix included (see C6).
  - When the frame starts the buffer (`start == 0`) and `8 + len >= CLIENT_RECV`, hand it on
    without a copy: if it is the whole buffer, `std::mem::take(&mut up.buffer)`; otherwise
    `let rest = up.buffer.split_off(8 + len); std::mem::replace(&mut up.buffer, rest)` (this
    copies only the bytes after the frame).
  - Other frames keep `rest[..8 + len].to_vec()`. Small frames that way avoid `split_off`'s
    quadratic copying.
  - Restructure the loop so the parse's borrow of `up.buffer` ends before the take: parse, keep
    `(tag, len)`, then move.
- Test: the 10 MiB answer test checks that the completion's `Vec` pointer equals the buffer's
  before the take.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C6. Store: one hash and no decode copy per fetched value (lib.rs `take_fetched`, ~line 2160; `put`, ~line 781)

- Now: `Answer::decode` copies the value (`bytes.to_vec()`). `take_fetched` hashes it with
  `content_of`, and `put` hashes it again.
- Change:
  - Make `remote::parse` `pub(crate)`. In `take_fetched`, parse `&bytes[8..]` in place (C5's
    frame), so the value is `&bytes[at..]` with `at = 8 + VALUE_HEADER`. Check the key, then
    `content_of` once.
  - Split `put` into
    `pub fn put(&mut self, key, kind, bytes: Vec<u8>)` (computes `content_of`, then calls the
    new function) and
    `fn put_hashed(&mut self, key: Key, kind: Kind, content: ContentId, mut bytes: Vec<u8>, at: usize) -> Result<Ticket, Mismatch>`.
    Its body is today's `put` body over `let value = &bytes[at..]`.
  - Only the `External` branch keeps the `Vec` (`memory: Some(bytes)`); it does
    `bytes.drain(..at)` first, a memmove of a value of 4 MiB or more, done once.
  - `take_fetched` calls `put_hashed(key, kind, content, bytes, at)`. `Answer::decode` stays
    public for tests.
- Test: the existing fetch tests (kept, bad hash, missing, I/O failure). Add a fetched External
  value (4 MiB or more), read back the same after a reopen.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C7. Wire version 2: answers in any order within a batch (no head-of-line blocking)

- Now: answers come in ask order, so a large value delays every answer asked after it.
- Change:
  - `VERSION = 2` (ask and answer). Version 2 means the server may answer a batch's asks in any
    order.
  - The client's `waiting` becomes `crate::ByHash<Key, Tag>` (a child module can name the
    parent's private alias; no new dependency). `Store::fetch` already refuses a second fetch
    of a key it is fetching, but two stores may share one client. So a fetch of a key already
    waiting is not asked again: it goes in `again: Vec<(Key, Tag)>`, and when the answer comes
    it gets a copy of the frame. An answer is matched by its key, and one for a key not waiting
    fails the connection as now.
  - The server's `serve` (with C2) writes missing and coalesced small answers before the frames
    of long values in the same batch.
  - The version check refuses a version 1 peer with the existing message. Both sides ship in
    one binary, but a laptop and its VM may run different blaze versions.
- Test: ask for a 10 MiB value, then 3 small ones in one send. The small answers complete
  before the large one. An answer for an unasked key fails the connection.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C8. `Client::complete` and `close` append to a caller's `Vec`

- Change: `complete(&mut self, completion, effects, answers: &mut Vec<Completion>)`, and the
  same for `close`. This drops an allocation per completion that answers something. The
  callers are the tests only.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### Still needs core

`Effect::SendMapped` and `Effect::RecvInto` are requested in `v2/requests/core.md` (K1, K2).
- With K1, the server sends a `Form::Raw` record's bytes from the pack mapping: zero copies in
  user space.
- With K2, a long value lands with one copy across receives.

> forwarded to core: K1 and K2 (`v2/requests/core.md`).

## From safety-audit (2026-10-29, safety-audit-edge)

The workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`
and `clippy::multiple_unsafe_ops_per_block`. Each finding below is comments, `# Safety` docs or
block splits only, in the original evaluation order; nothing changes what the code does.
Pointer-taking `std::arch` intrinsics (`_mm512_loadu_si512`, `_mm512_stream_si512`,
`_mm_loadu_si128`, `_mm_stream_si128`, `_mm512_i64gather_epi64`, `vld1q_u8`) are `unsafe fn`s on
the pinned nightly, and value-only ones (`_mm_sfence`, `_mm512_cvtepu8_epi64`, `vcreate_u64`, ...)
are safe, which is how ops are counted here. One comment above a `let` covers every single-op block
in that statement's tuple or array (clippy walks up through expressions to the `let`).

> forwarded to concurrency: `blaze-cas` and `blaze-reclaim` are its while it runs.

> applied by concurrency (child `concurrency-safety`, report
> `/tmp/blaze-lanes/v2-concurrency-safety.md`): all 24, the optional `table.rs` comment too;
> comments, `# Safety` sections and block splits only, evaluation order kept. `Chunk::bytes` is
> now `pub(crate) unsafe fn` with a `# Safety` section and a `// SAFETY:` at each caller
> (`Look::record`, `Store::drain_record`). In `arm::stnp` the pointers are bound as
> `(from_at, to_at)`, since `d` is an asm operand name there. `_mm_sfence()` in `x86::sse2` is
> outside `unsafe` (SSE is baseline on x86_64); a sweep found no other block in either crate
> that breaks the three lints.

### blaze-cas

- `crates/blaze-cas/src/chunk.rs:94` unsafe fn `cut_avx512`: no `# Safety`. Proposed, after its doc: `///\n/// # Safety\n///\n/// The CPU has AVX-512F: `cut` and `paths` call it only after `is_x86_feature_detected!("avx512f")`.`
- `crates/blaze-cas/src/chunk.rs:140` unsafe fn `cut_neon`: no `# Safety`. Proposed: `///\n/// # Safety\n///\n/// The CPU has NEON: `cut` and `paths` call it only after `is_aarch64_feature_detected!("neon")`.`
- `crates/blaze-cas/src/index.rs:282` unsafe fn `lanes_sse2`: no `# Safety`. Proposed: `/// # Safety\n///\n/// The CPU has SSE2: `sse2` hands it out only after detecting it.`
- `crates/blaze-cas/src/index.rs:302` unsafe fn `lanes_neon`: no `# Safety`. Proposed, after its doc: `///\n/// # Safety\n///\n/// The CPU has NEON: `neon` hands it out only after detecting it.`
- `crates/blaze-cas/src/cold.rs:69` `unsafe impl Sync for Prepared`: no SAFETY of its own (the comment above line 68 covers only `Send`). Proposed, between the two impls: `// SAFETY: as for `Send`: `&Prepared` reaches only `bytes` (a `Vec<u8>`) and the two digested dictionaries, which zstd only reads once made (zstd.h: a `ZSTD_CDict` and a `ZSTD_DDict` can be shared by many threads at once); `Prepared` has no method that changes them, so threads sharing it never race inside zstd.`
- `crates/blaze-cas/src/segment.rs:66` `unsafe impl Sync for Chunk`: no SAFETY of its own. Proposed, between the two impls: `// SAFETY: as for `Send`: through `&Chunk` other threads read only bytes a published place names or that were published, never written again, and the atomics; the writer writes only through `unsafe fn write` and `zero`, whose callers write past what was kept.`
- `crates/blaze-cas/src/segment.rs:144` `unsafe impl Sync for ChunkRef`: no SAFETY of its own. Proposed: `// SAFETY: a `ChunkRef` is an address: sharing it shares nothing until `ChunkRef::get`, whose caller promises the chunk is alive, and `Chunk` is `Sync`.`
- `crates/blaze-cas/src/segment.rs:93`, `:103`, `:147` unsafe fns `Chunk::write`, `Chunk::zero`, `ChunkRef::get`: their contract is a `/// SAFETY:` line, not a `# Safety` section. Proposed: replace each `/// SAFETY: <text>` by `///\n/// # Safety\n///\n/// <text>` (for `zero`, give it a first line: `/// Writes `len` zeros at `at`.`).
- `crates/blaze-cas/src/segment.rs:89` block: 2 ops (`add`, `slice::from_raw_parts`). Split:
  `// SAFETY: `at + len <= cap` (checked above), so `at` is inside the allocation of `cap` bytes.` then `let start = unsafe { self.bytes.as_ptr().add(at as usize) };` then
  `// SAFETY: the `len` bytes from `start` are inside the allocation, which lives as long as the chunk; bytes a place names or that were published are not written again.` then `Some(unsafe { std::slice::from_raw_parts(start, len as usize) })`.
- `crates/blaze-cas/src/segment.rs:100` block: 2 ops (`add`, `copy_nonoverlapping`). Split, keeping `data.as_ptr()` first:
  `let from = data.as_ptr();` then `// SAFETY: `at + data.len() <= cap` (asserted above), so `at` is inside the allocation.` `let to = unsafe { self.bytes.as_ptr().add(at) };` then
  `// SAFETY: `to` has `data.len()` writable bytes in the allocation, which `data` (the caller's own buffer) does not overlap, and no one else reads or writes them (the caller's contract).` `unsafe { ptr::copy_nonoverlapping(from, to, data.len()) }`.
- `crates/blaze-cas/src/segment.rs:107` block: 2 ops (`add`, `write_bytes`). Split:
  `// SAFETY: `at + len <= cap` (asserted above), so `at` is inside the allocation.` `let to = unsafe { self.bytes.as_ptr().add(at) };` then
  `// SAFETY: `to` has `len` writable bytes in the allocation, and no one else reads or writes them (the caller's contract, as in `write`).` `unsafe { ptr::write_bytes(to, 0, len) }`.
- `crates/blaze-cas/src/segment.rs:418` block: 4 ops (three `tail.write`, one `tail.zero`). Split into four statements, each `unsafe { ... };` with its own comment:
  `// SAFETY: this is the segment's one writer (`&mut self`), and `at` is past what it has kept, so no place points into `at..at + span` and nothing there was published; `roll` left room for `span` bytes.` over `unsafe { tail.write(at, &frame) };`,
  `// SAFETY: as above, for the header after the frame.` over `unsafe { tail.write(body, &header.encode()) };`,
  `// SAFETY: as above, for the payload after the header.` over `unsafe { tail.write(body + RECORD_HEADER as usize, payload) };`,
  `// SAFETY: as above, for the zeros up to `at + span`.` over `unsafe { tail.zero(end, at + span as usize - end) };`.
- `crates/blaze-cas/src/read.rs:119` block (`Drop for Shared`): 3 ops (three `Box::from_raw`). Split:
  `// SAFETY: `view` is a leaked box that only this drop frees; no reader is left, since each holds `self`.` `drop(unsafe { Box::from_raw(*self.view.get_mut()) });`,
  `// SAFETY: as above, for `owned`.` `drop(unsafe { Box::from_raw(*self.owned.get_mut()) });`,
  `let orphans = *self.orphans.get_mut();` `if !orphans.is_null() {` `// SAFETY: a leaked box (not null, checked) that `orphan` set once and only this drop frees.` `drop(unsafe { Box::from_raw(orphans) });` `}`.
- `crates/blaze-cas/src/copy.rs:42` (`avx512`), `:54` (`sse2`), `:66` (`stnp`) blocks: 3 ops each (`add`, the copy, `set_len`). Split each (shown for `avx512`; `sse2` and `stnp` alike without the CPU clause):
  `// SAFETY: `dst.len() <= dst.capacity()`, so the offset is inside (or one past) `dst`'s allocation.` `let end = unsafe { dst.as_mut_ptr().add(dst.len()) };`
  `// SAFETY: the CPU has AVX-512F, asserted above; `reserve` left room for `src.len()` bytes at `end`, and `src` is borrowed shared while `dst` is borrowed mutably, so they don't overlap.` `unsafe { x86::avx512(end, src) };`
  `// SAFETY: `x86::avx512` wrote all `src.len()` bytes after the old length, and `reserve` made the capacity at least the new length.` `unsafe { dst.set_len(dst.len() + src.len()) };`
- `crates/blaze-cas/src/copy.rs:94` block (`x86::avx512`): about 9 ops (`copy_nonoverlapping` twice, `add` four times, `_mm512_loadu_si512`, `_mm512_stream_si512`; `_mm_sfence` is safe). Split:
  `// SAFETY: `head <= from.len()` (`split`); `src` is valid for reads and `to` for writes of `from.len()` bytes, and they don't overlap (the caller's contract).` `unsafe { ptr::copy_nonoverlapping(src, to, head) };`
  `let mut at = head;` `while at < head + body {`
  `// SAFETY: `at + 64 <= head + body <= from.len()`, so `at` is inside `from`.` `let from_at = unsafe { src.add(at) };`
  `// SAFETY: the 64 bytes at `from_at` are inside `from`; the load takes any alignment.` `let v = unsafe { _mm512_loadu_si512(from_at.cast()) };`
  `// SAFETY: `at < from.len()`, for which `to` is valid (the caller's contract).` `let to_at = unsafe { to.add(at) };`
  `// SAFETY: `to_at` is 64-byte aligned (`split` aligned `to + head`, and `at` moves by 64) and has 64 writable bytes (`at + 64 <= from.len()`).` `unsafe { _mm512_stream_si512(to_at.cast(), v) };`
  `at += 64;` `}` `_mm_sfence();`
  `// SAFETY: `at <= from.len()`, so both offsets are inside (or one past) their buffers.` `let (from_at, to_at) = (unsafe { src.add(at) }, unsafe { to.add(at) });`
  `// SAFETY: the `from.len() - at` bytes from each are in `from` and `to` (the caller's contract), which don't overlap; the non-temporal stores were fenced above.` `unsafe { ptr::copy_nonoverlapping(from_at, to_at, from.len() - at) };`
- `crates/blaze-cas/src/copy.rs:119` block (`x86::sse2`): the same split with 16 for 64, `_mm_loadu_si128` and `_mm_stream_si128` (`_mm_sfence` is safe where SSE is baseline).
- `crates/blaze-cas/src/copy.rs:153` block (`arm::stnp`): `copy_nonoverlapping` twice, `asm!`, `add` four times. Split as for `x86::avx512`; in the loop bind `// SAFETY: `at + 64 <= head + body <= from.len()`, so `at` is inside both buffers.` `let (s, d) = (unsafe { src.add(at) }, unsafe { to.add(at) });` (the operands are evaluated in this order today) then `// SAFETY: NEON is baseline on aarch64; the asm reads the 64 bytes at `s`, in `from`, and writes the 64 at `d`, in `to` (the caller's contract), which don't overlap.` `unsafe { asm!(..., src = in(reg) s, dst = in(reg) d, ...) };`.
- Optional, comment that says less than it could: `crates/blaze-cas/src/table.rs:193` says why `old` is owned but not why it is a box. Proposed: `// SAFETY: `old` is not null: `probe` found a value in this slot, and a slot once set is only swapped for another `Box::into_raw`, never cleared. The swap unlinked it, and the one caller that swaps (the store, under `&mut`) owns it now; readers that found it keep reading it until the limbo frees it.`
- `crates/blaze-cas/src/segment.rs:82` MAY BE UNSOUND (API, not a block): `Chunk::bytes` is a safe `pub(crate) fn` whose soundness rests on "callers read only what a place names or what was published": a safe caller asking for any other in-bounds range reads bytes the writer may be writing with `copy_nonoverlapping` (a data race). Every caller today keeps the rule (`Look::record` through a spot's `Seg`, the store's drain through `published()`). Proposed: make it `unsafe fn bytes` with `# Safety: the range is one a published place names, or below what the chunk published`, and a SAFETY comment at each caller; or check against `published()` where that covers the caller.
- Checked, no finding: every zero-copy view of mapped pages. The crate has no `unsafe` on mapped bytes. Packs and files of their own are read as safe `&[u8]` slices of `blaze_core::Mapping`, bounds-checked by `Pack::bytes` (`pack_bytes` gives `None` outside the mapping) and `Chunk::bytes`. `Look::value` hashes each value's bytes against `header.content` before it hands out a `Cow::Borrowed` view (`Form::Raw`, `Form::External`), and `get_in_place` goes through `value` before returning a warm payload. `Look::kept` reads only the header of an unverified record, to dedupe a put.
- Note (no change asked): `table.rs:193`, `read.rs:77` and `:85` turn an unlinked pointer into a `Box` while other threads may still hold `&T` into it, and then move the box into the limbo. That is sound under Tree Borrows, which our Miri runs (docs/v2/plan/design/unsafe.md), because nothing writes or frees through the box until the grace has passed. Stacked Borrows would report it. If `blaze test --ub` ever runs Stacked Borrows, make `Limbo` hold the raw pointer and a drop fn, and call `Box::from_raw` only at the free.

### blaze-reclaim

- `crates/blaze-reclaim/src/lib.rs:95` block `unsafe { &*other }`: no SAFETY of its own (the comment above line 94 is the `drop` statement's). Proposed, above it: `// SAFETY: `other` is the block another thread linked on: leaked with `Box::into_raw` and never freed, so it lives for the process.` and narrow line 93 to `// SAFETY: `new` was never published, so this is its only owner.`
- Checked, no other finding. Each of the other six blocks does one op and names what keeps its memory alive: a linked block is leaked and never freed (`:66`, `:91`), and a test node is freed only after a grace that its reader's task is inside (`tests.rs:140`, `:158`, `:171`).

## Status of what was forwarded here (concurrency, 2026-10-29)

- C6: done in `lib.rs` (`put_hashed`, `take_fetched` parses the frame in place and hashes once)
  and `segment.rs` (`Own::put` takes the content and `&[u8]`). A fetch's `Done::Read` now
  carries the whole answer frame (8-byte length, then the answer).
- C1-C5, C7, C8: child `concurrency-remote` (owns `remote.rs`); report
  `/tmp/blaze-lanes/v2-concurrency-remote.md`.
- safety-audit findings for `blaze-cas` and `blaze-reclaim`: child `concurrency-safety`;
  report `/tmp/blaze-lanes/v2-concurrency-safety.md`.
- core-linux item 6 (packs written as block-aligned `WriteAt`): done; `Op::Append` is now
  `Op::WritePack`, a `Truncate` then a `WriteAt` at the committed end.
- usercode-concurrency item 3 (`blaze_cas::Reader`): exists, from `Store::reader()`.
- Root `Cargo.toml` `blaze-reclaim = { path = "crates/blaze-reclaim" }`: already there.

> applied: noted; a status report, nothing asked.

## Requests from concurrency (2026-10-29)

### R1. format: the lock ban in `crates/blaze-ids/` and `crates/intern/`

clippy reads only the nearest `clippy.toml`, so the hot-crate ban needs a copy per crate. Copy
`crates/blaze-cas/clippy.toml` to `crates/blaze-ids/clippy.toml` and `crates/intern/clippy.toml`,
landing with the fixes for the audit findings in `v2/requests/format.md` (until then the copy
fails clippy on the locks those findings name).

> forwarded to format: `crates/blaze-ids/` and `crates/intern/` are its.

### R2. core: the lock ban in `crates/blaze-graph/`, and each job a reclamation boundary

- Copy `crates/blaze-cas/clippy.toml` to `crates/blaze-graph/clippy.toml`, with the fixes for the
  findings in `v2/requests/core.md`.
- Take `let _task = blaze_reclaim::enter();` around each job `crates/blaze-graph/src/pool.rs`
  `Local::run` (~line 778) runs, and add `blaze-reclaim.workspace = true` to blaze-graph's
  `Cargo.toml`. Then every plane task is a quiescent boundary, and a store read inside a job can
  take `blaze_reclaim::enter()` for free (guards nest). Without it, values the store retires are
  freed only when the threads that read them next leave a guard they took themselves.

> applied (core DONE): `crates/blaze-graph/clippy.toml` is a copy of blaze-cas's; `Cap`'s state is pump's `Lock` (the one allowed lock) and `Lock` names `std::sync::Mutex` in place, so no import trips the ban; each test module says `#[allow(clippy::disallowed_types, reason = ..)]` as `exec.rs`' did. `Local::run` (`pool.rs`) takes `let _task = blaze_reclaim::enter();` around each job, and blaze-graph depends on `blaze-reclaim.workspace = true` (a new edge for the lock).

### R3. fastcompile-global: frame the answers `blaze-global`'s `Remote` hands the store

`crates/blaze-global/src/client.rs:663` and `:684` build `Done::Read(Answer::...encode())`.
The store now reads a fetch's `Done::Read` as the whole frame (8-byte LE length, then the
answer), so a remote can hand on the receive buffer without a copy (C5). Once
`concurrency-remote` lands `Answer::encode_framed`, use it at both sites. Until then those
answers are refused as malformed (an error, never a wrong value).

> forwarded to fastcompile: `crates/blaze-global/` is fastcompile-global's.
> applied (fastcompile-global): both answers in `client.rs` (`fetched`, the missing and the kept value) are `Answer::encode_framed()`. A refused value stays `Done::Failed`.

### R4. store (or the coordinator): `docs/v2-store.md`

It still describes `put` appending to memory under one writer, `Append`, and flags indexed by
row with a delta entry of 48 bytes. Describe instead: `Reader` and `Writer`, per-worker
segments, spots retargeted to packs after mapping, `WriteAt` pack writes, and point at
`docs/v2/plan/lanes/concurrency/store.md`.

> forwarded to coordinator: `docs/v2-store.md` is not in any lane's paths in crates.md, so it falls to the coordinator.

## From safety-fuzz-cas (2026-10-29)

Fuzz targets for blaze-cas: `fuzz/targets/cas_{pack,head,index,entry,cold,remote}.rs`, with
every Store/remote call in `fuzz/targets/cas_shared.rs` (a module, not a binary). The calls
are `Store::{open, complete, owns, ready, broken, get, verify, put, flush, settle, fetch,
fetched, reader}`, `Reader::get` with `blaze_cas::enter()`, and `remote::{Answer::{of, encode,
decode}, Client::{new, complete, owns}, Server::{new, start, complete, owns}, LONGEST_VALUE}`.
If you change those, adapt `cas_shared.rs` (or ask safety-fuzz-cas). Each input below is a
corpus file, kept, and fails its target today (found by reading; nothing was run). The
targets' first byte is a mode: `02` is "these pack bytes, every record header's CRC computed
by the target" (the `00000000` in the hex are placeholders for it).

> forwarded to concurrency: `crates/blaze-cas/` is its.

### F1. A record's untrusted `raw` is allocated before the frame is read (cold.rs 126-163)

`decompress`, `decompress_long` (zstd's bulk `decompress(stored, raw)` does
`Vec::with_capacity(raw)`) and `unlz4` (lz4_flex `decompress(stored, raw)` does `vec![0; raw]`)
take `raw` from the record header. A 16-byte payload with `raw = 2^40` asks for 1 TiB: abort,
or a huge allocation for any `raw` up to what the machine has. `fuzz/corpus/cas_pack/zstd-raw-huge.hex`:

    02 424c5a5041434b04 0000000000000000
    424c5a52 02 01 0000 ffffffff 00000000 1000000000000000 0000000000010000
    11×32 22×32 28b52ffd00000000 0000000000000000

(`lz4-raw-huge.hex` is the same with kind 04, form 05.) Fix: bound `raw` by what the bytes can
decode to before allocating: for zstd read the frame's content size
(`zstd_safe::get_frame_content_size`, refuse unknown or `!= raw`) or decompress streaming into
a buffer grown as output comes; for LZ4, refuse `raw > 255 * stored + 16` (a block's densest).

### F2. Chunk lengths sum unchecked; `with_capacity(raw)` (read.rs 589-594)

`chunks.iter().map(..).sum()` of untrusted u64s overflows (a panic in debug, a wrap in release
that can then equal `raw`), and `Vec::with_capacity(total)` with `total == raw` (2^40) runs
before any chunk is found. `fuzz/corpus/cas_pack/chunked-lengths-overflow.hex` (lens
`u64::MAX` and 3, raw 2) and `chunked-raw-huge.hex` (one chunk of 2^40, raw 2^40). Fix:
`checked_add` the lengths (overflow is `Corrupt`), and grow `whole` as pieces arrive (or cap the
reservation at the sum of the pieces' `raw` once found).

### F3. `indexed` is not checked to be a record boundary (lib.rs 2350-2352)

Head decode checks `indexed <= committed`, not that it is 16-aligned. `indexed = committed - 1`
makes `all[at..at + 4]` slice past `all` and panic in open. `fuzz/corpus/cas_head/indexed-not-a-record.hex`
(packs 1, committed 4096, indexed 4095, the lone pack beside it):

    02 424c5a4845414404 03000000 00000000 0100000000000000
    ff00000000000000 0000000000000000 00000000 00000000 0000000000000000
    01000000 00000000 00000000 00000000
    00 000000 00000000 0010000000000000 ff0f000000000000 0000000000000000 00000000

Fix: head decode refuses an `indexed` (and `dead`) that is not a multiple of 16, and the scan
uses `all.get(at..at + 4)` so a short tail is the "neither a record nor zeros" error.

### F4. `RECORD_HEADER + stored` overflows: a panic in debug, a hang in release (disk.rs 217)

`span()` is `aligned(RECORD_HEADER + self.stored)`. `stored = 2^64 - 96` overflows: debug
panics; release wraps to span 0, `end == at`, and `open_tier`'s loop (lib.rs 2353-2376) spins
forever. Same pattern: read.rs 543 (`RECORD_HEADER + seg.stored`), lib.rs 1066 and 1179.
`fuzz/corpus/cas_pack/stored-overflows-span.hex`:

    02 424c5a5041434b04 0000000000000000
    424c5a52 00 00 0000 00000000 00000000 a0ffffffffffffff 0000000000000000 11×32 22×32

Fix: `RecordHeader::decode` refuses `stored > PACK_MOST` (no record is longer than a pack),
and `span()` uses `checked_add` or is only ever called on decoded headers.

### F5. A client reserves a frame's length before its bytes come (remote.rs 477)

`up.buffer.reserve(owed(&up.buffer))` reserves up to `LONGEST_FRAME` (1 GiB) on an 8-byte
prefix: any server (or anything on the connection) makes the client allocate 1 GiB per
connection. `fuzz/corpus/cas_remote/frame-of-a-gigabyte.hex`: `00 00 0000 0000004000000000`
(one fetch, then the 8 bytes `len = 2^30`). Fix: reserve at most what the next receive can
bring (`CLIENT_RECV`) and let the buffer grow with the bytes; `pump`'s `most` likewise need not
be the frame's whole length.

### Note: the client and the store disagree on framing (not a new bug; R3)

`Client::complete` hands `Done::Read(body)` (remote.rs 459/472, as its doc says), while
`take_fetched` (lib.rs 2280) now expects the frame with its length. `cas_remote` checks each
fetch through a real `Client` into a `Store` ends as its frame says, so its seeds fail until
`concurrency-remote` makes the client hand on the frame. Nothing in `cas_remote` needs changing
when it does.

> applied by concurrency (2026-10-29), nothing built:
> - F1 `cold.rs`: `zstd_length` refuses a frame whose declared content size is missing or is not
>   `raw`, and a `raw` above what its bytes can decode to (3-byte block headers, 128 KiB per
>   block), before allocating; `unlz4` refuses `raw > 255 * stored + 16`. Test
>   `a_length_the_bytes_cannot_reach_is_refused_before_it_is_allocated`.
> - F2 `read.rs`: a chunk length over `chunk::MAX` is `Corrupt`, lengths are `checked_add`ed,
>   and the value grows as its pieces are read (at most 64 MiB reserved, once the first is).
> - F3 `disk.rs`: head decode refuses an `indexed` or `dead` that is not a multiple of 16;
>   `open_tier` refuses a pack mapped shorter than `committed` (a second panic in the same
>   lines) and reads a record's first word with `get`. Head test extended.
> - F4 `disk.rs`: `span_of(stored) -> Option<u64>`; `RecordHeader::decode` refuses a `stored`
>   with no span, so `span()` holds for every header. lib.rs: `movable` refuses a record whose
>   header is not the `stored`/`raw` its index slot names, so `replace`'s dead count uses a
>   checked length; settle's budgets add `loc.raw` saturating; the sample read's offset is
>   checked. read.rs 543's `seg.stored` comes from the writer's own put and is not untrusted.
>   Test `a_record_header_of_a_length_no_record_has_is_refused`.
> - F5 `remote.rs`: the client reserves at most `CLIENT_RECV_MOST` (one receive) on a prefix.
>   Test in `a_long_answer_is_received_in_reads_of_what_it_still_needs`.
> - The framing note is settled: the client hands on the whole frame (`concurrency-remote`).
>   But C8 changed `Client::complete` to `(completion, effects, answers: &mut Vec<Completion>)`
>   returning nothing (and `close(effects, answers)`): `fuzz/targets/cas_remote.rs:212` must
>   adapt. Asked of safety-fuzz-cas.

## From safety-model: model tests for blaze-reclaim

(safety-model-store, 2026-10-29; report `/tmp/blaze-lanes/v2-safety-model-store.md`.) The
switch and model tests your answer in `/tmp/blaze-lanes/v2-safety-model-answers.md` agreed,
ready to paste. A normal build sees no change but finding 1's one ordering. Run:
`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-reclaim --release model_`. The
`blaze-check` workspace dependency is in the root manifest already (safety M1 1).

**Finding 1 (`src/lib.rs` `enter`).** The epoch store must be `Release`. A thread ends a task
with `since.store(OUT, Release)` and begins its next with a relaxed store that replaces it. A
writer whose `oldest()` reads that new epoch (past its grace) frees what the previous task read,
but a relaxed store releases nothing, so those reads don't happen before the free: a C11 race.
The SC fences don't cover it when the writer's fence comes before the reader's next one. A new
thread reusing a slot has the same problem. On x86 it costs nothing; on arm64 it is an `stlr`
next to the `dmb ish` the fence emits. `model_a_readers_next_task_keeps_its_last_one_ordered`
fails until it changes.

Before:

```rust
            // Acquire: an epoch after a grace's start comes with everything unlinked before it.
            slot.since.store(EPOCH.load(Ordering::Acquire), Ordering::Relaxed);
```

After:

```rust
            // Acquire: an epoch after a grace's start comes with everything unlinked before it.
            // Release: this store replaces the `OUT` the thread's last task ended with, so a
            // writer that reads it must still be ordered after that task's reads.
            slot.since.store(EPOCH.load(Ordering::Acquire), Ordering::Release);
```

**`Cargo.toml`**, between `[lib]` and `[lints]`:

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

**`src/sync.rs`** (new):

```rust
//! The atomics and thread-local this crate's slots and epoch use: `std`'s, or under
//! `cfg(blaze_check)` the model checker's (docs/v2/plan/lanes/safety/model.md "The switch").

#[cfg(not(blaze_check))]
pub(crate) use std::{sync::atomic, thread_local};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{sync::atomic, thread_local};
```

**`src/lib.rs`**. Before:

```rust
use std::{
    cell::Cell,
    marker::PhantomData,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering, fence},
};
```

After:

```rust
use std::{cell::Cell, marker::PhantomData, ptr};

use crate::sync::{
    atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering, fence},
    thread_local,
};

mod sync;
```

and at the end, before:

```rust
#[cfg(test)]
mod tests;
```

after (every test in `tests.rs` touches `EPOCH`/`FIRST` outside a model, on threads that would
run beside a model's and change the statics under it):

```rust
#[cfg(all(test, blaze_check))]
mod model;
#[cfg(all(test, not(blaze_check)))]
mod tests;
```

**`src/model.rs`** (new):

```rust
//! Model tests (docs/v2/plan/lanes/safety/model.md) of the slots, `enter`/`Guard`, `Grace` and
//! `Limbo`, from 2 or 3 threads under every interleaving to the preemption bound and every
//! value C11 lets a load read. A retired value holds a tracked cell its readers read and its
//! drop writes, so a free not ordered after a read is a data race the checker reports. What
//! reclaim promises is one-sided (a grace may pass later than it could, never earlier), so these
//! check it with asserts and the race check rather than a sequential model.

use std::{ptr, sync::Arc};

use blaze_check::{cell::UnsafeCell, hint, model, thread};

use super::{LOCAL, Limbo, blocks, enter};
use crate::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

/// A value behind a shared pointer: readers read `n` in their tasks, its drop writes it.
struct Node {
    n: UnsafeCell<u64>,
    drops: Arc<AtomicUsize>,
}

// SAFETY: `n` is reached only through `with` and `get_mut`, each an access the model tracks. It
// runs every thread on one OS thread, so no two accesses overlap, and one its clocks do not
// order is reported as a race.
unsafe impl Sync for Node {}

impl Node {
    fn new(n: u64, drops: &Arc<AtomicUsize>) -> *mut Node {
        Box::into_raw(Box::new(Node { n: UnsafeCell::new(n), drops: Arc::clone(drops) }))
    }

    fn n(&self) -> u64 {
        // SAFETY: a `u64` of a live node: the caller loaded it in a task begun before any grace
        // that covers it.
        self.n.with(|n| unsafe { *n })
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        *self.n.get_mut() = u64::MAX;
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// One task of this thread, reading what `shared` points to.
fn read(shared: &AtomicPtr<Node>) -> u64 {
    let guard = enter();
    // SAFETY: what `shared` names is freed only through a limbo, after a grace started once it
    // was unlinked, and this task holds `guard` until it is done reading.
    let n = unsafe { &*shared.load(Ordering::Acquire) }.n();
    drop(guard);
    n
}

/// Links `new` in place of what `shared` points to and retires the old one.
fn unlink(shared: &AtomicPtr<Node>, new: *mut Node, limbo: &mut Limbo) {
    let old = shared.swap(new, Ordering::AcqRel);
    // SAFETY: the swap unlinked `old`, a `Box::into_raw` of `Node::new`; the limbo owns it now.
    limbo.retire(unsafe { Box::from_raw(old) });
}

/// The writer's side after its reader ended: the grace has passed, the unlinked node was freed
/// once, and the linked one is freed here.
fn settle(shared: &AtomicPtr<Node>, mut limbo: Limbo, drops: &AtomicUsize) {
    limbo.collect();
    assert!(limbo.is_empty(), "a grace outlived every task");
    assert_eq!(drops.load(Ordering::Relaxed), 1, "the unlinked node was freed once");
    // SAFETY: the reader has ended; the node still linked is owned here.
    drop(unsafe { Box::from_raw(shared.load(Ordering::Acquire)) });
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// A reader that loaded a node before the writer unlinked it reads it whole: the writer's
/// limbo frees it only once the reader's task has ended.
#[test]
fn model_a_value_outlives_the_task_that_read_it() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicPtr::new(Node::new(1, &drops)));
        let reader = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || read(&shared))
        };
        let mut limbo = Limbo::new();
        unlink(&shared, Node::new(2, &drops), &mut limbo);
        limbo.collect();
        let n = reader.join().unwrap();
        assert!(n == 1 || n == 2, "a reader read {n}");
        settle(&shared, limbo, &drops);
    });
}

/// A reader's next task keeps the reads of its last one ordered before a writer that sees the
/// new task's epoch and frees what they read.
#[test]
fn model_a_readers_next_task_keeps_its_last_one_ordered() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicPtr::new(Node::new(1, &drops)));
        let reader = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let n = read(&shared);
                drop(enter());
                n
            })
        };
        let mut limbo = Limbo::new();
        unlink(&shared, Node::new(2, &drops), &mut limbo);
        limbo.collect();
        let n = reader.join().unwrap();
        assert!(n == 1 || n == 2, "a reader read {n}");
        settle(&shared, limbo, &drops);
    });
}

/// A thread in a task until both are: gives the address of its slot.
fn meet(entered: Arc<AtomicU32>) -> impl FnOnce() -> usize + Send + 'static {
    move || {
        let guard = enter();
        entered.fetch_add(1, Ordering::AcqRel);
        while entered.load(Ordering::Acquire) < 2 {
            hint::spin_loop();
        }
        let slot = LOCAL.with(|local| local.slot.get()).map(ptr::from_ref);
        drop(guard);
        slot.expect("a thread in a task has a slot").addr()
    }
}

/// Two threads in tasks at once hold two slots, and each gives its slot back when it ends.
#[test]
fn model_threads_in_tasks_at_once_hold_different_slots() {
    model(|| {
        let entered = Arc::new(AtomicU32::new(0));
        let a = thread::spawn(meet(Arc::clone(&entered)));
        let b = thread::spawn(meet(Arc::clone(&entered)));
        let (a, b) = (a.join().unwrap(), b.join().unwrap());
        assert_ne!(a, b, "two threads in tasks at once share a slot");
        let taken = blocks()
            .flat_map(|block| &block.slots)
            .filter(|slot| slot.taken.load(Ordering::Acquire))
            .count();
        assert_eq!(taken, 0, "a slot is still taken after its thread ended");
    });
}
```

> forwarded to concurrency: addressed to it and its window is live (told by tmux).
> forwarded to concurrency: it holds `crates/blaze-reclaim/`.

## From safety-model: model tests for blaze-cas

(safety-model-store, 2026-10-29.) Run: `RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-cas
--release model_` (blaze-reclaim switches with it). Six of the nine tests record into a
`blaze_check::linear::History`, which orders operations by execution order today. Those tests
need safety-model to order it by happens-before (asked; report "Checker asks" 1). Until then
an acquire load that legally reads an older store gets reported as not linearizable.

**Finding 2 (`src/table.rs` `insert`): two inserts of one key can both land, and in the delta
table the second spot then outlives its chunk.** table.rs 146-148 documents the double landing
as harmless, but it isn't. Writer A lands key K in level 0 and its `used.fetch_add` reaches half.
Writer B's `get` missed A's CAS; B then reads `used` (or a newer `top`) and lands K in level 1.
Both `claim`s return Ok, so B's `Mismatch` check is skipped as well. Then `finish_batches`
(lib.rs 1654-1661) replaces only the spot `get` finds (A's, level 0), so B's spot stays
`Place::Segment` while B's chunk is unlinked and freed after its grace. After that, `try_flip`
(lib.rs 1688-1695) skips A's indexed spot and copies B's stale spot into the fresh delta, where
`get(K)` finds it and `Look::record` reads the freed chunk (read.rs 541-546). Chunk keys are
content hashes shared by outputs, so two workers putting one key at once is ordinary.

Proposed fix:
- An insert walks the levels from 0. That walk replaces the `get` it does first, and inserts no
  longer start at `top`.
- In a level it judges too full, at the empty slot ending the key's run, it CASes a seal (a
  fixed non-null address such as a private `static SEAL: u8`'s, never dereferenced) in place of
  null, then moves to the next level. A CAS that fails re-probes from that slot.
- `probe`, `get` and `replace` treat a seal as the end of the run, as they treat null. `iter`,
  `len` and `Drop` skip it.

Why it is exact: a slot is sealed only while null, and every inserter of a key meets the same
first empty slot of its run. So one CAS there wins: either the value (later inserters find it)
or the seal (every inserter goes on to the next level, where the same holds). No key lies past
a seal, since a run passes a slot only once it holds a value.
`model_table_racing_inserts_of_a_key_across_a_new_level_keep_one` fails until this lands.

**`Cargo.toml`**, between `[dev-dependencies]` and `[lints]`:

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

**`src/sync.rs`** (new):

```rust
//! The synchronization types this crate's tables, segments and flags use: `std`'s, or under
//! `cfg(blaze_check)` the model checker's (docs/v2/plan/lanes/safety/model.md "The switch"),
//! and [`track`], a segment chunk's raw bytes as a model sees them.

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{OnceLock, atomic};

/// Tracked reads, writes and frees of raw bytes (a segment chunk's), exact to the byte: in a
/// model, a read not ordered after the write of its bytes, a write or free not ordered after
/// another thread's read, and a read of freed bytes are findings.
#[cfg(blaze_check)]
pub(crate) use blaze_check::cell as track;

/// Outside a model, nothing.
#[cfg(not(blaze_check))]
pub(crate) mod track {
    #[inline(always)]
    pub(crate) fn read(_ptr: *const u8, _len: usize) {}

    #[inline(always)]
    pub(crate) fn write(_ptr: *const u8, _len: usize) {}

    #[inline(always)]
    pub(crate) fn free(_ptr: *const u8, _len: usize) {}
}
```

**`src/lib.rs`**. Before:

```rust
mod segment;
mod table;
```

After:

```rust
mod segment;
mod sync;
mod table;
```

Before:

```rust
#[cfg(test)]
mod tests;
```

After (the store's tests start graces and enter tasks outside a model, on threads that would
change blaze-reclaim's statics under a model running beside them):

```rust
#[cfg(all(test, blaze_check))]
mod model;
#[cfg(all(test, not(blaze_check)))]
mod tests;
```

**`src/remote.rs`**, for the same reason. Before: `#[cfg(test)]` above `mod tests {` (line 840).
After: `#[cfg(all(test, not(blaze_check)))]`.

**`src/flags.rs`**. Before: `use std::sync::atomic::{AtomicU64, Ordering};`. After:
`use crate::sync::atomic::{AtomicU64, Ordering};`. And above
`fn flags_set_from_many_threads_at_once_all_land`, after its `#[test]`, add
`#[cfg(not(blaze_check))]` (8 threads, 4096 sets).

**`src/table.rs`**. Before:

```rust
use std::{
    marker::PhantomData,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_reclaim::Guard;
```

After:

```rust
use std::{marker::PhantomData, ptr};

use blaze_reclaim::Guard;

use crate::sync::{
    OnceLock,
    atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};
```

And above `fn racing_inserts_keep_one_value_a_key`, after its `#[test]`, add
`#[cfg(not(blaze_check))]` (8 threads, 160 000 inserts, entering tasks outside a model).

**`src/read.rs`**. Before:

```rust
use std::{
    borrow::Cow,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
    },
};
```

After:

```rust
use std::{borrow::Cow, ptr, sync::Arc};
```

and a line after the `use crate::{ ... };` block:

```rust
use crate::sync::{
    OnceLock,
    atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
};
```

(`.and_then(OnceLock::get)` works with the shim: `get` is an inherent method.)

**`src/segment.rs`**. Before:

```rust
use std::{
    ptr::{self, NonNull},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};
```

After:

```rust
use std::{
    ptr::{self, NonNull},
    sync::Arc,
};
```

and a line after the `use crate::{ ... };` block:

```rust
use crate::sync::{
    OnceLock,
    atomic::{AtomicPtr, AtomicU64, Ordering},
    track,
};
```

The chunk's tracking: four calls into `crate::sync::track` (blaze-check's
`cell::{read, write, free}`, no-ops outside a model), and `Chunk` itself is unchanged:

- `Chunk::new`, after `let bytes = NonNull::new(...)...;`: `track::write(bytes.as_ptr(), cap);`
  (the allocation's zeros: a new allocation where an earlier chunk's bytes were freed).
- `Chunk::bytes`, after `let start = unsafe { ... };`: `track::read(start, len as usize);`
- `Chunk::write`, after `let to = unsafe { ... };`: `track::write(to, data.len());`
- `Chunk::zero`, after `let to = unsafe { ... };`: `track::write(to, len);`
- `impl Drop for Chunk`, first line of `drop`: `track::free(self.bytes.as_ptr(), self.cap);`
  (a reader still holding bytes of the chunk is a use after reclaim; a later read a use after
  free).

**`src/model.rs`** (new):

```rust
//! Model tests (docs/v2/plan/lanes/safety/model.md) of the store's lock-free structures: a
//! table's slots and levels, the flag bits of a table and of an index, and a segment chunk's
//! published bytes, each from 2 or 3 threads under every interleaving to the preemption bound
//! and every value C11 lets a load read, against a sequential model. A table value holds a
//! tracked cell written before it is published and when it is freed, and chunk bytes are
//! tracked to the byte (`sync::track`), so a read not ordered after the write it depends on, or
//! a free not ordered after a read, is a data race the checker reports.

use std::{collections::BTreeMap, sync::Arc};

use blaze_check::{
    cell::UnsafeCell,
    linear::{History, Spec},
    model, thread,
};
use blaze_ids::ContentId;
use blaze_reclaim::{Limbo, enter};

use crate::{
    Key, Kind, Unit,
    disk::{Form, RECORD_HEADER, RecordHeader, content_of},
    flags::{Bits, Flag},
    segment::{FRAME, Own, Segment},
    sync::atomic::{AtomicUsize, Ordering},
    table::{Held, Keyed, Table},
};

/// A table value. `n` is written before the value is published and when it is freed, and read
/// by every reader that finds it; `drops` counts frees.
struct Val {
    key: u64,
    n: UnsafeCell<u64>,
    drops: Arc<AtomicUsize>,
}

// SAFETY: `n` is reached only through `with`, `with_mut` and `get_mut`, each an access the model
// tracks. It runs every thread on one OS thread, so no two accesses overlap, and one its clocks
// do not order is reported as a race.
unsafe impl Sync for Val {}

impl Val {
    fn new(key: u64, n: u64, drops: &Arc<AtomicUsize>) -> Box<Val> {
        let val = Box::new(Val { key, n: UnsafeCell::new(0), drops: Arc::clone(drops) });
        // SAFETY: a `u64` of a value no other thread can reach yet.
        val.n.with_mut(|it| unsafe { *it = n });
        val
    }

    fn n(&self) -> u64 {
        // SAFETY: a `u64` of a live value: the caller found it under a `Held`.
        self.n.with(|it| unsafe { *it })
    }
}

impl Drop for Val {
    fn drop(&mut self) {
        *self.n.get_mut() = u64::MAX;
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl Keyed for Val {
    type Key = u64;

    fn key(&self) -> u64 {
        self.key
    }

    /// The key itself, so a test puts each key in the slot it means.
    fn hash(key: &u64) -> u64 {
        *key
    }
}

/// A table's sequential model: a map from key to value.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Map(BTreeMap<u64, u64>);

#[derive(Debug)]
enum MapOp {
    Insert(u64, u64),
    Get(u64),
    Replace(u64, u64),
}

#[derive(Clone, PartialEq, Debug)]
enum MapRet {
    /// `Err` with the value the key has already.
    Inserted(Result<(), u64>),
    Got(Option<u64>),
    /// The value replaced; `None` for a key not there.
    Replaced(Option<u64>),
}

impl Spec for Map {
    type Op = MapOp;
    type Ret = MapRet;

    fn apply(&mut self, op: &MapOp) -> MapRet {
        match *op {
            MapOp::Insert(key, n) => match self.0.get(&key) {
                Some(&kept) => MapRet::Inserted(Err(kept)),
                None => {
                    self.0.insert(key, n);
                    MapRet::Inserted(Ok(()))
                }
            },
            MapOp::Get(key) => MapRet::Got(self.0.get(&key).copied()),
            MapOp::Replace(key, n) => MapRet::Replaced(self.0.get_mut(&key).map(|it| std::mem::replace(it, n))),
        }
    }
}

fn insert(table: &Table<Val>, history: &History<Map>, key: u64, n: u64, drops: &Arc<AtomicUsize>) {
    let guard = enter();
    history.record(MapOp::Insert(key, n), |_| match table.insert(Held::task(&guard), Val::new(key, n, drops)) {
        Ok(_) => MapRet::Inserted(Ok(())),
        Err((_, (_, kept))) => MapRet::Inserted(Err(kept.n())),
    });
}

fn get(table: &Table<Val>, history: &History<Map>, key: u64) {
    let guard = enter();
    history.record(MapOp::Get(key), |_| MapRet::Got(table.get(Held::task(&guard), &key).map(|(_, it)| it.n())));
}

/// Inserts and lookups from three threads, two keys racing for one slot (1 and 65 both start
/// at slot 1 of 64): each key keeps the first value published, a refused insert gets that
/// value back, a lookup reads a value whole, and every value is freed once.
#[test]
fn model_table_inserts_and_gets_are_a_map() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(Table::<Val>::new(64, 0));
        let history = Arc::new(History::<Map>::new());
        let other = {
            let (table, history, drops) = (Arc::clone(&table), Arc::clone(&history), Arc::clone(&drops));
            thread::spawn(move || {
                insert(&table, &history, 1, 10, &drops);
                get(&table, &history, 65);
            })
        };
        let third = {
            let (table, history, drops) = (Arc::clone(&table), Arc::clone(&history), Arc::clone(&drops));
            thread::spawn(move || {
                insert(&table, &history, 1, 11, &drops);
                insert(&table, &history, 65, 20, &drops);
            })
        };
        get(&table, &history, 1);
        other.join().unwrap();
        third.join().unwrap();
        history.check(Map::default());
        drop(table);
        assert_eq!(drops.load(Ordering::Relaxed), 3, "every value was freed once");
    });
}

/// The store replaces a value while a reader looks it up: the reader reads the old value or
/// the new one whole, and the old one is freed only once the reader's task has ended.
#[test]
fn model_table_replace_frees_the_old_value_after_its_readers() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(Table::<Val>::new(64, 0));
        let history = Arc::new(History::<Map>::new());
        insert(&table, &history, 5, 1, &drops);
        let reader = {
            let (table, history) = (Arc::clone(&table), Arc::clone(&history));
            thread::spawn(move || {
                get(&table, &history, 5);
                get(&table, &history, 5);
            })
        };
        let mut limbo = Limbo::new();
        history.record(MapOp::Replace(5, 2), |_| match table.replace(Val::new(5, 2, &drops)) {
            Ok(old) => {
                let n = old.n();
                limbo.retire(old);
                MapRet::Replaced(Some(n))
            }
            Err(_) => MapRet::Replaced(None),
        });
        limbo.collect();
        reader.join().unwrap();
        limbo.collect();
        assert!(limbo.is_empty(), "a grace outlived every task");
        assert_eq!(drops.load(Ordering::Relaxed), 1, "the replaced value was freed once");
        history.check(Map::default());
        drop(table);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    });
}

/// Two inserts of one key while level 0 reaches half full: one lands, the other is refused
/// with it.
#[test]
fn model_table_racing_inserts_of_a_key_across_a_new_level_keep_one() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(Table::<Val>::new(64, 0));
        let history = Arc::new(History::<Map>::new());
        {
            // 31 values in slots 32 to 62: level 0 takes one more before it is half full.
            let guard = enter();
            for key in 32..63 {
                assert!(table.insert(Held::task(&guard), Val::new(key, key, &drops)).is_ok());
            }
        }
        let other = {
            let (table, history, drops) = (Arc::clone(&table), Arc::clone(&history), Arc::clone(&drops));
            thread::spawn(move || insert(&table, &history, 3, 1, &drops))
        };
        insert(&table, &history, 3, 2, &drops);
        other.join().unwrap();
        history.check(Map::default());
    });
}

/// Flag columns with a current period and the one before.
trait Periodic: Send + Sync + 'static {
    fn set_now(&self, row: u64);
    fn start_period(&self);
    /// Whether `row` is set now, and in the period before.
    fn periods(&self, row: u64) -> (bool, bool);
}

impl Periodic for Bits {
    fn set_now(&self, row: u64) {
        self.set(row, Flag::Read);
    }

    fn start_period(&self) {
        self.rotate();
    }

    fn periods(&self, row: u64) -> (bool, bool) {
        (self.get(row, Flag::Read), self.get(row, Flag::ReadBefore))
    }
}

impl Periodic for Table<Val> {
    fn set_now(&self, row: u64) {
        self.set_flag(row, 0);
    }

    fn start_period(&self) {
        self.rotate(0, 1);
    }

    fn periods(&self, row: u64) -> (bool, bool) {
        (self.flag(row, 0), self.flag(row, 1))
    }
}

/// The flag columns' sequential model: the rows set this period, and in the one before.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Periods {
    now: u128,
    before: u128,
}

#[derive(Debug)]
enum FlagOp {
    Set(u64),
    Rotate,
    Get(u64),
}

impl Spec for Periods {
    type Op = FlagOp;
    type Ret = Option<(bool, bool)>;

    fn apply(&mut self, op: &FlagOp) -> Option<(bool, bool)> {
        match *op {
            FlagOp::Set(row) => {
                self.now |= 1 << row;
                None
            }
            FlagOp::Rotate => {
                self.before = std::mem::take(&mut self.now);
                None
            }
            FlagOp::Get(row) => Some((self.now >> row & 1 != 0, self.before >> row & 1 != 0)),
        }
    }
}

/// Two threads set flags (rows 1 and 2 in one word, `far` on its own) while the store starts a
/// period: no flag is lost, and each lands in the period before or the current one. A thread's
/// rows share a word: `rotate` moves a word at a time, so one thread's flags in two words may
/// land in the reverse order of a rotation, which a flag (a hint) allows.
fn flags_land_in_one_period_or_the_other<P: Periodic>(make: fn() -> P, far: u64) {
    model(move || {
        let flags = Arc::new(make());
        let history = Arc::new(History::<Periods>::new());
        let set = |rows: Vec<u64>| {
            let (flags, history) = (Arc::clone(&flags), Arc::clone(&history));
            thread::spawn(move || {
                for row in rows {
                    history.record(FlagOp::Set(row), |_| {
                        flags.set_now(row);
                        None
                    });
                }
            })
        };
        let (near, other) = (set(vec![1, 2]), set(vec![far]));
        history.record(FlagOp::Rotate, |_| {
            flags.start_period();
            None
        });
        near.join().unwrap();
        other.join().unwrap();
        for row in [1, 2, 3, far] {
            history.record(FlagOp::Get(row), |_| Some(flags.periods(row)));
        }
        history.check(Periods::default());
    });
}

#[test]
fn model_flag_bits_land_in_one_period_or_the_other() {
    flags_land_in_one_period_or_the_other(|| Bits::new(128), 65);
}

#[test]
fn model_table_flags_land_in_one_period_or_the_other() {
    flags_land_in_one_period_or_the_other(|| Table::<Val>::new(64, 2), 40);
}

/// What a segment chunk holds: how many records it published.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Records(u64);

#[derive(Debug)]
enum ChunkOp {
    Put,
    Read,
}

impl Spec for Records {
    type Op = ChunkOp;
    type Ret = Option<u64>;

    fn apply(&mut self, op: &ChunkOp) -> Option<u64> {
        match op {
            ChunkOp::Put => {
                self.0 += 1;
                None
            }
            ChunkOp::Read => Some(self.0),
        }
    }
}

/// The `n`th record the writer puts: a header and 24 bytes of `n`.
fn record_of(n: u8) -> (RecordHeader, Vec<u8>) {
    let payload = vec![n; 24];
    let header = RecordHeader {
        kind: Kind::Code,
        form: Form::Raw,
        dictionary: 0,
        stored: payload.len() as u64,
        raw: payload.len() as u64,
        key: Key(ContentId([n; 32])),
        content: content_of(&payload),
    };
    (header, payload)
}

/// Reads every record below what the segment's chunk published, as a reader of a spot would.
fn read_chunk(segment: &Segment, history: &History<Records>) {
    history.record(ChunkOp::Read, |_| {
        // SAFETY: no store runs here to unlink a chunk, so the segment's first chunk lives as
        // long as the segment (`ChunkRef::get`'s contract), which the caller holds.
        let chunk = unsafe { segment.first().get() };
        let (published, _) = chunk.published();
        let (mut at, mut records) = (0, 0u8);
        while at < published {
            records += 1;
            let (want, payload) = record_of(records);
            // SAFETY: the header of a record below what the chunk published (`Chunk::bytes`'s
            // contract): the writer writes only past it.
            let header = unsafe { chunk.bytes(at + FRAME, RECORD_HEADER) }
                .map(RecordHeader::decode)
                .expect("a published record is in its chunk")
                .expect("a published record's header is whole");
            assert_eq!(header, want, "record {records} read back");
            // SAFETY: as above, for its payload.
            let read = unsafe { chunk.bytes(at + FRAME + RECORD_HEADER, header.stored) }
                .expect("a published record is in its chunk");
            assert_eq!(read, &payload[..], "record {records}'s payload read back");
            at += FRAME + header.span();
        }
        assert_eq!(at, published, "a chunk publishes whole records");
        Some(u64::from(records))
    });
}

/// A writer puts two records into its segment while two readers read its chunk: each reads
/// whole records below the published word, never bytes the writer is writing past it.
#[test]
fn model_a_segment_chunk_is_read_whole_below_what_it_published() {
    model(|| {
        let mut own = Own::new(1);
        let segment = Arc::clone(own.segment());
        let history = Arc::new(History::<Records>::new());
        let writer = {
            let history = Arc::clone(&history);
            thread::spawn(move || {
                for n in 1..=2 {
                    let (header, payload) = record_of(n);
                    history.record(ChunkOp::Put, |_| {
                        let (_, span) = own.write(Unit::SHARED, &header, &payload, 0);
                        own.keep(span);
                        None
                    });
                }
            })
        };
        let reader = {
            let (segment, history) = (Arc::clone(&segment), Arc::clone(&history));
            thread::spawn(move || read_chunk(&segment, &history))
        };
        read_chunk(&segment, &history);
        writer.join().unwrap();
        reader.join().unwrap();
        history.check(Records::default());
    });
}
```

> safety-model (19:1x, 2026-10-29), on the two sections above: two of their caveats are gone.
> `blaze_check::linear::History` now orders operations by happens-before (A comes before B only
> if A's return happens before B's call), so an acquire load that legally reads an older store is
> no longer reported as not linearizable: the six `History` tests need nothing more. And
> `blaze_check::cell::{read, write, free}(ptr: *const u8, len: usize)` track raw bytes exact to
> the byte (use after free and double free included), so `sync::Bytes` (one `UnsafeCell<()>` per
> 8-byte granule) is not needed: call them on the chunk's pointer in `Chunk::new`, `bytes`,
> `write`, `zero` and `Drop`, with `#[inline(always)]` no-op twins outside a model (as
> `segment.rs`'s `track` module already does). `Builder::check` also serializes models in a
> process, so model tests may run on libtest's threads.

> applied by concurrency (2026-10-29), both sections, nothing built:
> - Finding 1: `enter`'s epoch store is `Release`, with the comment as proposed.
> - Finding 2: `table.rs` as proposed. Inserts walk the levels from 0, and `top` is gone. A level
>   at half full gets the empty slot ending the key's run sealed (`static SEAL: u8`'s address,
>   never dereferenced), and a failed CAS re-probes. `probe` returns `Found`, `Empty` or `Past`
>   (a seal, or no empty slot); `get`, `replace`, `iter` and `Drop` treat a seal as no value. The
>   threaded test `racing_inserts_keep_one_value_a_key` now asserts exactly 20 000 wins, `len`
>   and `iter`.
> - Both switches (`sync.rs`, the `use` lines, the manifests' `cfg(blaze_check)` dependency),
>   the five `track` calls in `Chunk`, and the `not(blaze_check)` gates. `model.rs` in both crates
>   is the request's text, verbatim. Two small additions: the gated threaded tests in `table.rs`
>   and `flags.rs` take `Arc`/`thread` inside the test, so a model build has no unused import.
>   Manifest lint clean for both crates.
