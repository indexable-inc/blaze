---
status: plan
summary: The remote tier against Bazel's remote cache API, BuildBuddy, NativeLink, sccache and Turborepo; v1's blaze-cargo remote is skipped (deleted at integration), and the v2 tier's fixes are requests to concurrency and core.
date: 2026-10-29
---
# speed: the remote tier (`speed-link-edge-remote`)

Part of the [store lane](../store.md), section 8 "Remote".

## Which code this covers

- **`crates/blaze-cargo/src/remote.rs` and `remote/`** (v1): S3, HTTPS and gateway sync of
  whole packs on tokio. It is my path, but `docs/v2/plan/reference/crates.md` ("its remote store
  becomes the read-through tier") and `v2/requests/store.md` request 2 delete it at
  integration. So, per the speed rules, I left it alone. It is measured below only as the
  "before".
- **`crates/blaze-cas/src/remote.rs`** plus `Store::fetch` and `take_fetched` in
  `crates/blaze-cas/src/lib.rs`: the v2 tier. A laptop's `Client` pipelines 40-byte asks to the
  VM daemon's `Server` over a Unix socket or SSH-forwarded loopback. This code belongs to agent
  `concurrency`, so every change is a request in `v2/requests/concurrency.md` (C1 to C8). Two
  need new effects from core, requested in `v2/requests/core.md`.

## The benchmarks

- **Bazel remote cache API** (REAPI v2): `FindMissingBlobs` (a batched existence check),
  `BatchReadBlobs` (many small blobs in one response) and ByteStream `Read` (large blobs
  streamed in chunks), all multiplexed over HTTP/2 gRPC on one connection.
  - **BuildBuddy and NativeLink** are its fastest servers. NativeLink serves from a CAS keyed by
    BLAKE3 or SHA-256 and verifies uploads as they stream.
  - Per hit it costs one round trip after the connection exists. A miss also costs one
    `FindMissingBlobs`, which goes to the write path.
- **sccache and Turborepo**: one HTTP GET per artifact to S3 or a cache server, plus one PUT per
  upload. They have no batching, so each hit is one request and one round trip.
- **The fastest known way for a trusted local link** is one stream with pipelined asks and many
  answers coalesced into one write. A large value goes from the page cache straight to the
  socket (`sendfile`/`splice`, or io_uring zero-copy send) and is received straight into its
  final buffer. It is hashed once, and matched by key so a large value never holds up small ones.

The tier is read-only by design: a laptop reads from its VM and never writes to it (store.md
section 8). So the brief's "uploads deduplicated and batched" and Bazel's `FindMissingBlobs`
(which serves uploads) have nothing to measure here. The `Missing` answer is the existence
check, folded into the fetch: one round trip where REAPI's read-after-check takes two.

## Before and after

"v1" is the code being deleted. "v2 as written" is `blaze-cas/src/remote.rs` today. "After" is
with requests C1 to C8 applied; the numbers with core's two effects (K1, K2) are given where
they differ.

| | v1 (`blaze-cargo/src/remote*`) | v2 as written | after C1–C8 (with K1, K2) |
|---|---|---|---|
| requests per build | 1 manifest and P key lists at start, then 1 GET per pack hit lazily | 1 connect, then 2 sends per turn of N fetches | 1 connect, then 1 send per turn of N fetches |
| round trips per hit | 1 per pack, which brings the whole pack (MBs) for one key | 1 (pipelined); a large value ahead delays the answers after it | 1; answers in any order within a batch |
| copies per byte fetched | 2 (hyper `Bytes` `to_vec`, temp file write) plus a whole-file reread for zstd | 5: driver `Vec`, client buffer, `body.to_vec`, `Answer::decode`'s `to_vec`, pack append | 2 for a value in one receive, 3 across receives (2 with K2) |
| hash passes per byte | 1 SHA-256, as bytes arrive | 2 BLAKE3 (`take_fetched`, then `put`) | 1 BLAKE3 |
| allocations per hit (laptop) | a 64 KiB `Vec` per chunk, tokio tasks | driver `Vec`, body `Vec`, decoded `Vec`, answers `Vec` | the driver's `Vec` only |
| server copies per byte served | n/a (S3) | 1 (pack to frame `Vec`) | 1 (0 for raw records with K1) |
| server allocations and sends per small answer | n/a | 1 `Vec` and 1 send each | shared: 1 `Vec` and 1 send per batch of asks |
| blocking on a worker | yes: a build thread waits on a `Condvar` for up to `WAIT_AT_MOST` | none (sans-IO) | none |
| memory bound on the server | n/a | none within one batch: 1024 asks are all answered before the bound is checked | `QUEUED_MOST` (64 MiB) plus one frame |

How "v2 as written" was counted, from `crates/blaze-cas/src/remote.rs` and `lib.rs`:
- **Two sends per turn**: `Client::fetch` pumps right away, so the first ask goes out alone, and
  the asks of the rest of the turn wait for its `Done::Sent`.
- **Five copies**: `Role::Recv` does `up.buffer.extend_from_slice(&bytes)`, and `take_answers`
  does `Done::Read(body.to_vec())`. Then `Store::take_fetched` calls `Answer::decode`, which
  does `bytes.to_vec()`, and `put` appends to the pack.
- **Two hashes**: `take_fetched` hashes with `content_of`, and `put` hashes with `content_of`
  again.

## What changes, and why it is faster

Each change is spelled out exactly, with its test, in `v2/requests/concurrency.md`.

- **C1, a real bound.** `serve` stops answering once `queued` passes `QUEUED_MOST` and resumes on
  `Done::Sent`. Before, one 40 KiB receive of 1024 asks for large values was answered whole, up
  to 1 TiB of frames in memory.
- **C2, coalescing.** Small answers from one batch go into one `Vec` and one send, as
  `BatchReadBlobs` does: one syscall and one allocation per batch, not one per answer.
- **C3, one send per turn.** Asks of later fetches are appended to the turn's still-unsubmitted
  `Effect::Send`.
- **C4 and C5, fewer copies on the client.** The client adopts the driver's receive `Vec` as its
  buffer, and hands a long first frame on by `split_off` instead of `to_vec`.
- **C6, one hash and no decode copy.** `take_fetched` parses in place, hashes once and keeps the
  value through `put_hashed`, which `put` shares.
- **C7, no head-of-line blocking within a batch.** With wire version 2, answers may come in any
  order: the client matches by key, and the server sends missing and small answers first.
- **C8, no allocation per call.** `Client::complete` appends to a caller's `Vec`.
- **C9, a miss costs no round trip (wire version 3).** The server sends a binary fuse filter
  (8-bit fingerprints, about 9 bits a key, `blaze_ids::fuse`) of its store's keys when a client
  connects, then a delta per batch (`Server::refresh`), and a full one again once the deltas
  pass a quarter of it. The client answers `Missing` itself for a key no filter may hold
  (`Asked::Missing`) and asks only on a maybe. A key put after the filter's epoch reads
  `Missing` until the next delta: a stale filter costs a recompute, never a wrong value.
- **K1 (core): `Effect::SendMapped`.** A raw record is sent from the pack mapping with
  `sendfile`, or with io_uring zero-copy send.
- **K2 (core): `Effect::RecvInto`.** A receive appends into a buffer the caller owns, so a long
  value lands with one copy.

## Where it still falls short, and what that would take

- **Head-of-line blocking across batches.** A 1 GiB value already being sent still delays the
  answers to asks sent after it: about 2 s at 500 MB/s over SSH. HTTP/2 or ByteStream avoids
  this by splitting the stream. Matching it would take a wire version 4 whose frames carry a
  stream number and a chunk, with the server interleaving 1 MiB chunks of long values between
  short answers. It is not requested yet: a laptop's read-through mostly asks for query values
  and code, which are small. It should be measured first.
- **Hashing as bytes arrive** (BLAKE3 incremental) is not adopted. The store must hash what it
  keeps itself, because it cannot trust what the `Remote` trait gives it. Hashing in the client
  as well would make two passes again. Hashing after arrival adds length divided by hash speed:
  about 0.15 s for 1 GiB at 7 GB/s, against 2 s or more on the wire, under 8%. To overlap the
  hash with the wire, `Store::fetch` would take the concrete `Client` (not `&mut dyn Remote`),
  and the client would feed a `blake3::Hasher` from each receive. Worth it only if a profile
  shows the hash on the critical path.
- **HTTP/2 and HTTP/3** are not used, on purpose. The link is the user's own Unix socket or SSH
  tunnel. One pipelined binary stream already gives many asks in flight on one connection,
  without HPACK, gRPC framing or protobuf parsing. HTTP/3's gain, no TCP head-of-line blocking
  on lossy links, does not apply inside an SSH tunnel, which is TCP.
