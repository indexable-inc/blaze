---
status: plan
summary: blaze v2 ci lane, hot_lint pass on blaze-api and blaze-protocol: allocation per frame, request and reply, before and after, the budget tests that hold them, and what is left in mem-core's files.
date: 2026-10-29
---
# speed-link-edge-hot-wire: allocation on the API's hot paths

Part of [ci](../ci.md), after [speed-link-edge-wire](speed-link-edge-wire.md). Paths:
`crates/blaze-api/` and `crates/blaze-protocol/`. Nothing here was built or run
(docs/v2/plan/goals/principles.md). The counts below are read off the code, and the budget tests
listed hold them once they run.

## The hot paths

Every agent and CLI request goes through four stages: a frame is cut and decoded (`frames.rs`,
`cbor`), or an HTTP head is parsed (`http.rs`, `RequestParser::next_ref` and `read_from`). The
server then routes it (`server.rs`), the session answers it (`session.rs`), and the reply is
encoded back into a frame or an HTTP chunk. The steady state is a warm connection that sends small
requests. Each one should cost nothing beyond the reply's own bytes. `units.rs`, `activation*`
and `client.rs` are the edge: once per `blaze daemon install`, once per daemon start, or once
per CLI process.

## State of the art

- **tokio-util's `LengthDelimitedCodec`, hyper's `h1` codec:** each keeps one `BytesMut` per
  connection, decodes frames in place and grows it only for a frame larger than any before.
  blaze's `Frames` already did this (`next_ref`). The gap was everything around it: replies
  formatted into a fresh `String`, then a fresh `Vec`, and errors built eagerly.
- **httparse:** parses the head over the caller's bytes into a caller-sized header array,
  with no allocation. `RequestParser` scans the head in place and owns only the request it
  hands out. That stays, since `HttpRequest` is public and owned.
- **minicbor and ciborium with a reused writer:** serialization appends onto a kept `Vec`.
  blaze-protocol's `cbor::Writer` did this for `append`, but `send` and `receive` built a new
  writer and frame each call.

## Before and after, per steady-state operation

| path | before | after | test |
|---|---|---|---|
| `blaze_protocol::send` + `receive`, one small `Message` | 3-4 allocations (writer scratch, frame, read buffer) | 0, using thread-local kept buffers, capped at 1 MiB | `tests.rs` `a_warm_send_and_receive_allocate_nothing` |
| `Follow::read` of an append to the events file | open + read buffer + records `Vec` | 0 with `read_into` (file kept open, partial drained in place) | `follow.rs` `a_warm_read_allocates_nothing` |
| a frame encoded, pushed, cut (`next_ref`) and decoded | 0 (already) | 0, now held by a test | `frames/tests.rs` `a_warm_round_of_a_frame_allocates_nothing` |
| `http::response`, `chunked`, `request` | 2-6 allocations (format, regrowth) | 1, sized up front; `response_into` 0 | `http/tests.rs` `a_message_is_one_allocation_of_its_size` |
| `RequestParser` read of a warm request, chunked body | 0 past the request handed out | 0, now held by a test | `http/tests.rs` `a_warm_read_and_chunked_request_allocate_nothing` |
| `read_response` | a 64 KiB heap read buffer, body copied | stack buffer, first body piece taken whole | `http/tests.rs` `a_response_of_many_reads_is_read_whole` |
| server: a request read on a warm connection | the error replies and `queue` formatted eagerly | 0 on Unix, at most 1 on HTTP (the 200 head) | `server/tests.rs` `a_warm_request_allocates_nothing_and_its_reply_one_buffer` |
| server: a reply | a fresh `String` + `Vec` per plain reply | one queue buffer of 4 KiB per send, 0 per `Done::Sent` | as above |
| `Intents::tick`, `take_over`, `moved`, `gone` | a collected `Vec` + radix scratch per call that picks | 0, with kept `picked` and `scratch` | `session/intent/tests.rs` |
| `BestOf::ranked` | 2 allocations | 1 (the ranking handed out) | the session's best-of tests |
| `units.rs` rendering, once per install | a `format!` per line, a `Vec<String>` of words | one `String` per file; escaping writes straight onto it | `units/tests.rs` goldens, `a_plain_word_doubles_its_specifiers_and_dollars` |

One allocation per send is left: the server hands each reply to `Effect::Send` as an owned
`Vec`, and `Done::Sent` does not give it back. That is request K3 with the coordinator
(`v2/requests/core.md`). With it, the reply's buffer is reused and a warm request costs nothing
at all.

## Clone

Clone was removed from types that are never cloned anywhere in the workspace: `bytes::Buf`,
`cbor::Error`, `events::Malformed`, `Order`, `Subscribe`, `frames::Refused`, `HttpRequest`,
`HttpResponse`, `server::Http`, `units::UnitError` and `units::Installed`. It was kept, with a
`// clone:` line saying who clones it, on the event types that blaze-ui and blaze-graph clone
(`Record`, `Event`, `Slow`, `Function`, `NodeState`, `Cause`, `Summary`, `Cache`, `Took`), on
`Message`, on `HttpError` (an error kept and handed out again), and on `units::Unit`, `Place`,
`Step` and `UnitFile` (once per install, and the tests make variants of one).

## Lint (`/tmp/sle_lint.py`)

| crate | before | after |
|---|---|---|
| blaze-protocol | 39 | 0 |
| blaze-api | 215 | 61, all in mem-core's `session.rs` (49), `frames/identity.rs` (10) and `lib.rs` (2) |

Each of the 61 has a proposed fix or justification in `v2/requests/mem-core.md`
("speed-link-edge-hot-wire: blaze-api's last 61 hot-lint findings"). The real per-request ones
are the views `vec!` per job start, the best-of's `targets` and `filter` cloned per candidate,
and the status and connection-end collects.

## New public API (signatures kept, nothing removed)

- `blaze_protocol::follow::Follow::read_into`: `read` onto the caller's records `Vec`.
- `blaze_protocol::events::Cause::shown`: `short` as a `Display`, with no `String`.
- `blaze_api::http::response_into`: `response` onto the caller's buffer. blaze-ci's webhook
  answers with it (speed-link-edge-hot-ci's request).
