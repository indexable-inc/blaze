---
status: plan
summary: Lane speed-link-edge-wire's notes for blaze v2: the agent API's wire (deterministic CBOR, frames, the daemon's listener, the client, HTTP and JSON) made cheap per frame with the bytes unchanged, with counts before and after and what each is measured against.
date: 2026-10-29
---
# blaze v2: speed-link-edge-wire's notes

These notes cover making `crates/blaze-protocol` and `crates/blaze-api` cost little per frame
without changing a byte on the wire. `SCHEMA` was not bumped for this work. The API is
"one versioned schema in deterministic CBOR" ([principles](../../goals/principles.md)). The
codec is also what `blaze-cargo` writes its compile records and cache entries in, so every
serde type must come out exactly as before, not only the API's.

## How the bytes are held to what they were

- The old encoder and decoder are kept as `blaze_protocol::cbor::reference`, behind the
  feature `reference` or `cfg(test)`, and are never on a real path. They are the oracle.
- `crates/blaze-protocol/src/cbor/tests.rs` covers every shape serde gives a serializer
  (3000 seeded shapes: maps in and out of order, unsaid lengths, every integer and float
  width, nesting to the depth limit).
  - It checks the same bytes, or the same refusal, from `to_vec`, `append` and a kept
    `Writer`.
  - It checks the same value or the same error for those bytes and for mutated ones, read
    as a typed value, as `IgnoredAny` and as a self-describing tree.
- `crates/blaze-api/src/frames/identity.rs` covers seeded values of every `Request` and
  `Response` variant. An exhaustive `match` means a new variant fails to compile until it is
  generated. It checks that:
  - `cbor::to_vec`, `frames::encode` and `frames::encode_into` with a kept writer all give
    `reference::to_vec`'s bytes;
  - the new decoder, the reference decoder and `frames::decode` all read the value back;
  - both decoders give the same answer for mutated bytes;
  - a value made only of the API's own types is written with nothing put in order. This
    keeps the field order below true as types change.

## What changed

- **Encoder** (`cbor/ser.rs`):
  - Items are written in place onto the caller's buffer. A map is written as it comes while
    its keys arrive in their encoding's order.
  - When a key comes out of order, the map's entries are found by walking the bytes already
    written, sorted, and permuted once through scratch the `Writer` keeps.
  - A head whose length serde did not say is one byte at first and is fixed in place,
    widened by a splice when the count needs more.
  - `cbor::Writer` keeps the scratch between values. Scratch over 1 MiB is dropped.
- **Decoder** (`cbor/de.rs`):
  - One pass checks the deterministic encoding (shortest heads, key order, shortest floats,
    depth) while it decodes. Text and byte strings are borrowed from the frame.
  - Whatever the visitor leaves unread is walked by the checker.
  - On an error only, the checker runs over the whole item, so the error named is the one
    the old two-pass decoder named.
- **API types** (`blaze-api/src/lib.rs`): every struct and struct variant declares its fields
  in the order of their keys' encoding (shorter name first, then bytewise), so each is
  written with nothing to sort.
  - The bytes are the same in any declaration order.
  - JSON (`Accept: application/json`, the CLI's `--json`) shows fields in this order now.
    Every JSON reader of the API reads by key. cargo's own JSON messages are hand-written in
    cargo's order (`blaze-cargo/src/cli/json.rs`, `blaze-ui`) and do not change.
  - `Envelope` keeps `schema, id, body` for JSON. Its CBOR is written by hand in key order
    by `frames::item_into`.
- **Frames:**
  - `blaze_protocol::write_frame` writes the length and the item in one vectored write,
    with no frame `Vec`.
  - `append_frame` and `frames::encode_into` encode a frame where it goes, with the length
    patched in after.
  - `read_frame_into` reads into a kept buffer.
  - `Frames::push_vec` and `RequestParser::push_vec` adopt a received buffer when nothing
    is held before it.
- **Server** (`server.rs`):
  - Every reply is encoded with one kept `cbor::Writer` straight onto its connection's
    queue. The queue is empty while no send is in flight, so an idle connection hands it to
    `Effect::Send` whole.
  - An HTTP chunk's content (a frame, or a JSON line through `serde_json::to_writer`) is put
    together in one kept buffer and copied once into the queue behind its size.
  - `pending` and `conns` are `FxHashMap`s. `stop` sorts the connections, so the order of
    effects is what it was.
- **HTTP** (`http.rs`):
  - The head scan finds line ends with `memchr`, with the same limits and errors.
  - A request's body is moved out with `split_off` (only pipelined bytes after it are
    copied), and a response's body is swapped out when it is all that is held.
  - The writers have `_into` forms that build the head with no `format!`.
- **Client** (`client.rs`):
  - Requests are encoded with a kept writer into a kept buffer.
  - Over HTTP, the head and the item go out in one vectored write (`write_both`), not
    copied into one `Vec`.
  - The response body is adopted by the frames rather than copied.

## Before and after

All counts are per frame or per request on the steady path, once kept buffers are warm.
"Map" means every struct, struct variant and map, the `Envelope` included. In the table, d
is how deep a byte is nested in maps.

| | before | after |
|---|---|---|
| Encoder allocations, per map | 3 or more (entries, keys, spans, plus regrowth) | 0 |
| Encoder allocations, per unsaid-length sequence | 1 | 0 |
| Bytes the encoder copies, per byte at depth d | d (each map copied into its parent) | 0 when keys come in order; 2 for a map out of order (to scratch and back) |
| Allocations per frame written (`frames::encode_into`, `append_frame`) | 1 frame `Vec`, plus the encoder's | 0 |
| `write_frame` | 1 allocation and 1 copy of the item, 1 write | 0 allocations, 0 copies, 1 vectored write |
| Decoder passes over the item | 2 (check, then decode) | 1 (2 only when it is refused) |
| UTF-8 checks per text | 2 | 1 |
| Bytes copied per decode, by the codec | 0 (borrowed) | 0 (borrowed) |
| Bytes copied per decode, by `Frames` | 1 copy of each received byte | 0 when a frame starts a receive's bytes; 1 when it spans receives |
| Server reply, Unix | 1 frame `Vec`, plus a copy into the queue when a send is in flight | encoded onto the queue: 0 allocations, 0 copies |
| Server reply, HTTP | JSON `String` or frame `Vec`, chunk `Vec`, head `String`s, plus a copy into the queue | 1 copy of the chunk into the queue, no allocation |
| Server allocations per send | 1 (the `Vec` `Effect::Send` takes) | 1, the same (K3 below removes it) |
| Locks, `Arc` clones per request (server, session) | 0, 0 | 0, 0 |
| Connection and tag maps | `BTreeMap` (pointer chasing, node allocation) | `FxHashMap` |
| HTTP request body | 1 memmove for the head and 1 copy of the body (`drain().collect()`) | 1 memmove for the head; body moved |
| HTTP client request | item `Vec`, a request `Vec` holding a copy of it, 1 write | kept buffers, 0 copies, 1 vectored write |
| Syscalls per frame | 1 send or write | 1 send or write |

Some costs stay the same, because the API's types own their data. `Request` and `Response`
are kept past their frame by the session (a request outlives its receive; a client keeps
responses of other requests). So decoding allocates one `String` per text field and one `Vec`
per list. Removing that would take borrowed mirror types (`ResponseRef<'a>`) for a reader that
looks at a response once, like the CLI's printer.

## What is left, and whose it is

- **Driver buffers** (lane core: `v2/requests/core.md`).
  - K3, filed here: `Done::Sent` hands its `Vec` back, so a connection's queue is reused and
    a warm connection allocates nothing per reply.
  - K2 (`RecvInto`), from speed-link-edge-remote, serves the server too. It lets `Frames`
    pass its own buffer in, giving one copy from the kernel and no allocation per receive.
  - With both, io_uring registered buffers or provided-buffer rings are the next step.
- **Other crates' field order** (lanes vfs and db: `v2/requests/vfs.md`, `v2/requests/db.md`).
  `Op`, `Change`, `Edge`, `Changed` and `Why` are declared out of key order, so each is put
  in order on every frame that carries it. Reordering their declarations changes no byte.
- **Bytes as byte strings** (needs a `SCHEMA` bump, so lane ci decides).
  - Every `Vec<u8>` of the API goes out as an array of integers: `Op`'s bytes,
    `Ran::stdout`/`stderr`, `TestResult::output`, `Cargo::files`.
  - As CBOR byte strings they would be one head and a borrow, not one item per byte.
- **HTTP headers** are a `String` pair each, because `HttpRequest` is a public type used by
  blaze-ci's webhook. A borrowed request view for the API server would remove them. The HTTP
  side is for remote clients; local ones use the Unix socket.

## The server's shape

The listener is a sans-IO state machine over `blaze_core`'s effects. It runs on whichever
driver the daemon starts: io_uring on Linux, kqueue on macOS, IOCP on Windows. Where io_uring
is unavailable the daemon stops with an error naming why ([decisions](../../process/decisions.md)).
There is no per-request thread, lock or `Arc`: one receive and one send in flight per
connection, and replies coalesce behind the send in flight. What io_uring could still give,
registered buffers and zero-copy send, needs K2 and K3 in the effect API. Nothing in the
server would need to change beyond passing the buffers through.

## What each part is measured against

- **[minicbor](https://docs.rs/minicbor):** borrowing decode (`&'b str`/`&'b [u8]` from the
  input) and encoding into a caller's buffer. The decoder here borrows the same way and adds
  the deterministic-encoding check in the same pass. minicbor checks nothing of that.
- **[cbor4ii](https://docs.rs/cbor4ii):** a serde codec that writes into a reused buffer with
  no intermediate value. The encoder here does that and also sorts map keys, which cbor4ii
  leaves to the caller. Sorting costs nothing when fields are declared in key order.
- **JSON** ([simdjson](https://simdjson.org), [sonic-rs](https://docs.rs/sonic-rs),
  [simd-json](https://docs.rs/simd-json)): these speed up parsing. The API only writes JSON,
  at the edge (HTTP with `Accept: application/json`, the CLI's `--json`), with
  `serde_json::to_writer` straight into the output buffer and no `Value` tree. A SIMD
  serializer (sonic-rs's) is the step to take if JSON output shows in a profile.
- **io_uring servers** ([monoio](https://docs.rs/monoio), [glommio](https://docs.rs/glommio),
  [TigerBeetle's I/O loop](https://github.com/tigerbeetle/tigerbeetle/tree/main/src/io)):
  - Their shape is one thread per core, completions instead of readiness, buffers owned by
    the ring, and no allocation per operation once warm.
  - blaze's drivers have the first two.
  - The last is K2 and K3. TigerBeetle's fixed, preallocated message pool is the model for a
    pool of queue buffers handed back by `Done::Sent`.
