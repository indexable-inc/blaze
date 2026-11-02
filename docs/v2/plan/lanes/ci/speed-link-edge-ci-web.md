---
status: plan
summary: blaze-web's event stream and page brought up to the speed bar - a lock-free hand-off from the daemon, one deflate stream per build until a page is asked for, records encoded once and sent in frames, a world that allocates nothing per function, and a page that draws once a frame.
date: 2026-10-29
---
# speed-link-edge-ci-web: the build page's stream

`crates/blaze-web/` (the daemon's side of the build page, docs/web.md, and the page). The
hot path is a cold build's records: thousands of `Crate`, `Node` and `Moved` records, and
hundreds of `Functions` records of up to tens of thousands of functions each (anim: 80 000
functions, 150 000 calls). Each record goes from the daemon's thread
(`Watched::record`, under the daemon's own `watched` lock) to `Graphs::record`, then to the
page's thread, then to every page's socket, then to the GPU.

## State of the art, per part

| part | benchmark | what it does that matters here |
|---|---|---|
| hand-off from the producer | Perfetto's shared-memory buffer (per-producer chunks, no syscall or lock per event) | a producer's write is a few stores; the consumer takes chunks |
| batching of a stream | Perfetto/Chrome tracing's packets; uWebSockets' `cork` | many events per message, one syscall per batch |
| keeping compressed | one zlib/zstd stream per trace, not per event | the compressor's state and window are made once and shared by every event |
| wire decode | simdjson, `sonic-rs` | SIMD structural scan, no allocation per value |
| incremental rendering | Solid's fine-grained updates; Perfetto's UI, one `requestAnimationFrame` redraw for any number of changes | work per frame, not per event |

## Before

Per record, on the daemon's thread (before a page was asked for):

- a `Mutex` taken;
- the record encoded to CBOR (one allocation), then a **new `GzEncoder` per record**: a fresh
  deflate state (miniz_oxide's compressor is some hundreds of KB, zeroed), a gzip header and
  CRC, an output `Vec` and a `Box<[u8]>`;
- on each `Began` and whole object, a scan and `retain` of every record kept.

Once the world ran: the `Mutex`, `root.to_owned()` (one allocation), one channel send.

On the world's thread, per record: a deep `record.clone()` into the program's last build's
records (one allocation per string in it), a second CBOR encoding for the pages (one
allocation), an `Arc<[u8]>` made from it (one allocation and copy), and **one channel message per
record per page**. A new page had the last build re-encoded, record by record, and a snapshot
made and gzipped again even when the program had not changed.

Per function of a `Functions` record: two allocations for its name (`replace` and `CString`),
three `Vec`s for its module path and one `String` per segment for the module lookup key, one
`Vec` per changed node for its `Callees`, and a clone of each node's call list for the snapshot.
Calls to ids not yet named sat in one list that **every record scanned in full**. Code of
another object is never named, so that list grows through a cold build: O(records × waiting).

On the socket: two `write`s per message (head, then payload) and a `Vec` for the head.

On the page: each `Functions` record restyled every point (two `Float32Array`s of the whole
program) or re-laid out the new ones and **uploaded every position, link and cluster to the
GPU**. Each record cost O(functions + calls), and a frame held as many records as came.

| per record | before |
|---|---|
| locks on the daemon's side | 1 |
| allocations on the daemon's side | 2 (world running) to ~5 including a deflate state (before) |
| encodings | 2 (kept, then again for the pages) or 1 + a deep clone |
| messages, per page | 1, plus a snapshot when the program began again |
| `write` syscalls, per page | 2 per message |
| GPU uploads on the page | 1 full upload per `Functions` record |

## What changed

- **`Graphs::record` takes no lock.** A root is numbered once in `Roots`, an append-only
  table read with acquire loads (concurrency.md), with the last root checked first. A record is
  then one send on `std::sync::mpsc`, whose list channel is lock-free (a CAS on its tail, a block
  allocated per 31 messages). The page's thread starts with the first record and holds no world
  until a page is asked for, so keeping, applying and encoding all happen off the daemon's
  thread.
- **Kept records are one deflate stream per run of a build** (`kept.rs`). Each workspace has
  one `DeflateEncoder` whose state `reset` reuses from run to run. A run's `Began`s and
  `Functions` go into the stream as length-prefixed CBOR, and its other records sit beside it in
  plain bytes, since the next `Began` drops them. Each record has a 1-byte kind in a column
  (`Kind`, 8 bytes with its object). A `Began` or a whole object drops whole runs, so nothing is
  ever compressed twice. The rules are `Kept::keep`'s from before, checked against that code by
  a differential test.
- **Batches and frames** (`lib.rs`). The world's thread takes everything already queued (up to
  4 096 asks, never waiting for more) and then sends each page **one frame**: a CBOR sequence
  of the batch's records, up to 1 MiB, made once into one `Arc<[u8]>` that every page shares.
  Each record is encoded **once**, into its program's last-build buffer (`Program::told`), and
  the frame copies those bytes. A new page is sent that buffer as it is, and the gzipped
  snapshot is cached until the program `changes`. How far behind a page may fall is now
  counted in bytes (256 MiB), since a message is a frame.
- **The world allocates nothing per function** once its crate and module exist (`world.rs`).
  Names go to Flecs through a reused NUL-terminated buffer. A crate's lookup first checks the
  crate named last. A module is **one FxHash lookup of its whole path** (crate index, then each
  segment after an `ff` byte), and each module above it is a prefix of that key in the same
  map. Module paths are walked in place with no `Vec`, over bytes (every delimiter is ASCII).
  `Callees` and the snapshot's sorted call lists use reused buffers. A call to an id not yet
  named waits in an **intrusive chain per `(object, id)`** (`Waiting`, 8 bytes, a `u32` next).
  Naming the id resolves that chain alone, in the order the calls came, so each call costs
  O(1) and the full scan is gone.
- **The socket** (`http.rs`). `Frames` writes every message that is ready in one go: heads and
  short payloads are copied into one buffer, and a long payload is written vectored with the
  buffered bytes before it.
- **The page.** `parseFrame` reads a frame's records in order (`records.js`). `Functions`
  records only collect what they added, and the graph gets it at the next animation frame
  (`scheduleView`/`applyView` in `app.js`): one restyle and one upload a frame, however many
  records came. Anything that reads the graph's points first (a flash, the edit's spread, a
  selection) applies what is waiting.
- **A fix found on the way.** When a whole object made the program anew, the build's other
  objects kept their ids for nodes that were gone, so a later record of one of them could index
  past the nodes. Their ids are now emptied, as on the page, which starts over on that snapshot.
  That they were said is kept, so when a program is made anew is unchanged.

## After

| per record | after |
|---|---|
| locks on the daemon's side | 0 (one only the first time a workspace is seen) |
| allocations on the daemon's side | 0 (the channel's block, 1 per 31 records) |
| encodings | 1 |
| allocations on the world's thread | 1 (the CBOR `Vec`, until `cbor::append` exists, see below); 0 per function |
| copies | encode, then one copy into the frame (then the kernel's) |
| messages, per page | 1 per batch (up to 4 096 records or 1 MiB) |
| `write` syscalls, per page | 1 per batch of messages |
| GPU uploads on the page | 1 per animation frame |

Time from a record to its pixel is no longer. A batch is only what is already queued, so an
idle stream sends each record as it comes. The page drew at the next frame anyway. Under a cold
build's load, the delay now stays bounded instead of growing with a backlog of per-record
uploads.

## Where it still falls short, and what it would take

- **Wire decode is not simdjson-class.** The page decodes CBOR in JS into an object per value,
  and `Model.mergeFns` takes those objects. To match it: a WebAssembly (SIMD128) decoder that
  turns a frame's `Functions` straight into columns (keys as a `BigUint64Array`, crates as
  interned `u32`, names as offsets into one UTF-8 buffer, calls as CSR), and a `mergeFns` over
  those columns.
- **Rendering is per frame, not per changed point.** `restyleAll` still recomputes every
  point's colour once a frame, and cosmos.gl takes whole arrays (`setPointColors`). A
  Solid-style path would restyle only the points records touched, which needs a partial-upload
  call in the vendored cosmos build (`assets/vendor/build/`).
- **One allocation per record for CBOR** remains, because `blaze-protocol` only has `to_vec`.
  The request is `v2/requests/speed-link-edge-ci-web.md` item 2 (`cbor::append`).
- **Deflate, not zstd**, for kept records: zstd at level 1 compresses several times faster.
  It is not a workspace dependency, and kept records only exist until a page is asked for.
- Each function's fields are read back from Flecs (`ecs_get_id`, one lookup per node) to make
  a snapshot. That runs once per change of the program, and the snapshot is now cached.

## Tests (`src/tests.rs`, `assets/dev/test.mjs`)

- `records_kept_in_runs_are_those_kept_one_by_one`: 64 seeded runs, some over `KEPT`
  workspaces. The runs give the same records, kinds and order as the old one-by-one keeping,
  reimplemented in the test.
- `a_build_kept_in_one_stream_comes_back_whole`: 2 000 records through one stream round-trip.
- `a_page_sent_frames_of_many_records_is_told_what_one_a_record_told_it`: seeded builds of three
  workspaces. A page from the start and one from halfway are told the same snapshots and
  records in the same order with one frame per 64 records as with one message per record (the
  old wire), in fewer messages.
- `a_snapshot_is_made_again_once_its_program_changes`: the cached snapshot is always the one
  the world would make, and is made once.
- `calls_waiting_in_chains_are_those_a_scan_found`: 32 seeded builds. The calls from waiting
  chains are those of the old full scan, reimplemented in the test.
- `a_module_path_read_in_place_is_the_one_collected_step_by_step`: 20 000 seeded names, non-ASCII
  among them. The in-place walk over bytes gives the old `Vec`-per-step result over `char`s.
- `roots_are_numbered_once_each`, `frames_written_together_are_the_frames_written_alone`
  (including a stream that takes 7 bytes a write).
- Page: `frame: every record of it, in order`, `frame: a record of it that is not one is named
  by its place`, `frame: one cut short is refused`.

## Unsafe (docs/v2/plan/design/unsafe.md)

None of the speed work is unsafe: the hand-off (`OnceLock` levels and `mpsc`), the frames, the
kept runs and the module lookup are safe code. The crate's only unsafe is the Flecs FFI, which
reaches a C library and buys no speed. It now sits in one module, `src/flecs.rs`, behind a
safe API (`Flecs`: `entity`, `delete`, `set_workspace`, `set_function`, `set_parent`,
`set_callees`, `named`, `add_calls`, `function`, `rest`).

- `lib.rs` has `#![deny(unsafe_code)]`. `flecs.rs` alone has `#![allow(unsafe_code)]`, and
  `world.rs`, `http.rs`, `kept.rs` and `tests.rs` each have `#![forbid(unsafe_code)]`. A
  crate-level `forbid` is not possible while one module needs unsafe.
- Each unsafe block holds one unsafe operation and has a `// SAFETY:` comment naming its
  invariant and where it is upheld. The module doc lists the invariants: one thread (raw
  pointers, so not `Send`), live entities, layouts, Flecs's copies, and reads borrowed until
  the next write.
- **Checked, not assumed.**
  - Every entity handed in is checked with `ecs_is_alive`. A dead one stops the thread with
    its id rather than reach Flecs, whose release build does not check.
  - `Flecs::new` checks each Rust type's size and alignment against Flecs's `EcsComponent`.
    That covers `Workspace`, `Function`, `Callees` (`ecs_vec_t`) and `Parent`, so a change to
    `KINDS` or to Flecs that moves a field stops the world rather than misread it.
  - A read checks for null and alignment before it makes a reference.
- **Safe reference.** The world's calls and functions, read back through its snapshot, are
  checked against a model with no Flecs (`calls_waiting_in_chains_are_those_a_scan_found`).
  The REST test reads the entities back through Flecs.
- No bytes from a peer reach Flecs: REST paths become `CString`s, which refuse a NUL, and
  names have their NULs removed (`c_text`).
