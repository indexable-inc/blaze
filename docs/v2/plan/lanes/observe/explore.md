---
status: plan
summary: the explorer's daemon side (crate blaze-explore): an HTTP/3 WebTransport server on 127.0.0.1 that https://ix.dev/explore connects to pinning a 13-day self-signed P-256 certificate by hash; the link, origin and token rules, certificate rotation, the ops and the live stream, the WebTransport probe, and the speed of light of live and tiles.
date: 2026-10-31
---
# Lane observe: the explorer's daemon side

`blaze explore` prints and opens a link to `https://ix.dev/explore`. The page connects back to
the daemon over WebTransport on 127.0.0.1 and asks it for builds, spans, tiles, views, SQL over
the history and Flecs's world, and receives the live feed. Nothing is served from localhost: the
page is ix.dev's, the daemon only answers. The design of the page and the reasons for
WebTransport are in explore-design.md and its addenda; the byte layout is
`/tmp/blaze-lanes/explore-wire.md` (it moves to `docs/` with the page). This document is the
daemon side as built in `crates/blaze-explore`.

Written without building or running anything; the tests named here are written and not yet run.

## Where it runs

`blaze_explore::start(Config)` starts one thread, `blaze-explore`, with a current-thread tokio
runtime and one QUIC endpoint on `127.0.0.1:0`. The daemon's own threads never wait on a page.
The work of an answer (DuckDB, the tiles, Arrow encoding) runs on that runtime's blocking pool,
so a slow query never holds up the live stream or another query. Dropping the `Explorer` closes
every session (code 0) and joins the thread.

`Config` holds the history (the DuckDB binary from `nix run .#duckdb` and the history's
directory, `history::dir_in(state)`), Flecs's world (`Option<Arc<blaze_web::Graphs>>`), whether
the page's development server may connect, and the daemon's version for `hello`. Wiring it into
`blaze serve` and adding the `blaze explore` command in blaze-cargo is queued to the coordinator.

## The link, the origin and the token

```
https://ix.dev/explore#127.0.0.1:<port>/<token>/<certhash>
```

- **The fragment never leaves the browser**, so ix.dev never sees the port, the token or the
  hash.
- **The token** is 16 bytes from the OS's random source (`std::random::SystemRng`), written as
  32 lowercase hex characters. It is new each time the server starts and is compared in constant
  time. Its `Debug` prints `Token(..)`.
- **The certhash** is the SHA-256 of the certificate's DER (`blaze-sha256`), base64url without
  padding: 43 characters.
- **The CONNECT path is `/explore/v1`**; any other path gets 404 before a session opens.
- **The Origin must be `https://ix.dev`**, or `http://localhost:5173` when `Config::dev_origin`
  is set (vite's development server in indexable-inc/ix). A missing Origin, `null`, another
  scheme, a suffix, or a trailing slash gets 403. This keeps out another site that learned a
  link. The token keeps out a local process that is not a browser, since such a process can send
  any Origin it likes.
- **The first request of a session must be `hello` with the token.** A wrong token is answered
  `bad_token`, then the session is closed with code 1. Any other request before `hello` is
  answered `unauthenticated`, and the session stays open for a `hello`.

## The certificate and its rotation

The certificate is self-signed ECDSA P-256 with SHA-256, made with rcgen (the `ring` backend),
for `127.0.0.1` and `localhost`, with the CN `blaze explore`. It is made when the server starts
and is valid for 13 days, starting 60 s before it was made: 13 days and 60 s in all, under the
14 days that WebTransport's `serverCertificateHashes` allows. wtransport's client checks the same
rules (P-256, at most 14 days, now inside the validity), so the session test proves the page
will accept it.

`cert::Rotation` is the rotation rule alone, with the time as an argument. A new certificate is
due 12 days after the current one began, a day before it expires, or at once if the clock went
back to before it began. The server asks every 10 minutes of the wall clock, so a machine that
slept past the day is caught within minutes of waking. The new identity goes in with
`Endpoint::reload_config(.., rebind: false)`: same socket, same port. A certificate that fails to
be made is reported on the daemon's log with when the current one expires, and is tried again at
the next check. The current one keeps being served until it expires, and is never swapped for
anything weaker.

**What an open session sees: nothing breaks.** QUIC checked the certificate once, during the
handshake, and an open session keeps its keys. The live stream sends a `cert` frame,
`{"op":"cert","certhash","not_after_unix_s"}`, and the page writes the new hash into its own URL
for its next connection. A page opened later from the old link gets the browser's certificate
error, and `blaze explore` prints the current link.

Tested on a virtual clock (`cert::tests`): 50 days in 10-minute steps rotate at days 12, 24, 36
and 48, and every certificate is valid at every step. A rotation that was not made is retried,
and a clock jump either way is caught.

## Requests and answers

Each request is one bidirectional stream: the page writes one JSON object and closes its side.
The answer is a little-endian `u32` header length, then the header JSON padded with spaces so
that the IPC starts 8-aligned, then one Arrow IPC stream (zstd level 1, 8-byte alignment,
metadata V5), then FIN. Every answer's header carries `op`, `elapsed_ns`, and `rows` when it has
a table. An error has the header `{"error":{"kind","message",...}}` and no IPC. To cancel, the
page resets the stream or sends STOP_SENDING.

| op | asks | answers |
|---|---|---|
| `hello` | `token` | `daemon`, `schema`, `clock`, `now_ns`, `certhash`, `cert_not_after_unix_s`, `live_every_ms`; no table |
| `builds` | | the history's `builds`, oldest first |
| `sql` | `sql` | its result; `sql` in the header |
| `views` | | one row a view: `name`, `title`, `sql`, `args` (the JSON Schema of its arguments) |
| `view` | `name`, `args` | the view's result; the SQL it became in the header |
| `spans` | `build`, `t0_ns`, `t1_ns` | the build's spans overlapping the range, by process, thread and start |
| `tiles` | `build`, `t0_ns`, `t1_ns`, `bucket_ns`, optional `lane` (`thread` or `job`) | the `tiles` table, or `job_tiles` (a lane per fiber job, from `job_spans`) for `lane: job`; `lane`, `buckets`, `spans`, `sql_ns`, `tiles_ns` |
| `world` | `path` | Flecs's REST answer to `GET <path>` as JSON in the header's `world`; no table |

`sql` and `view` run through observe-history's `blaze_profile::sql::run` (the pinned DuckDB
process over the Parquet history), and `views` comes from `blaze_profile::views::{VIEWS, find,
render, args_schema}`. `spans` and `tiles` read the history the same way, not `run_live`,
because a live merge could count a span twice that was flushed between the two reads. A span
overlaps `[t0, t1)` when it starts before `t1` and ends after `t0`; an instant overlaps when it
is inside the range. `tiles` applies the same rule. An empty result is checked against `builds`,
so a build that doesn't exist answers `no_such_build`, not zero rows.

**Error kinds:** `unknown_op`, `bad_args`, `bad_token`, `unauthenticated`, `sql`
(DuckDB's message and the query in the detail), `no_such_build`, `no_such_view`, `no_world`,
`world` (Flecs answered something other than 200 JSON), `overloaded`, `encode` (Arrow
refused a result), and `cancelled` (the request was stopped and DuckDB killed).

**Limits:** 16 requests in flight per session, a 17th is answered `overloaded`. A request is at
most 1 MiB and must arrive whole within 10 s.

**Cancel:** a request's stream holds a stop flag, set however the stream ends: the page resets
it or sends STOP_SENDING, the session goes, or the server stops. Every history query runs
through `sql::run_with(&Query, &stop)`, which kills the DuckDB process once the flag is set and
answers `SqlError::Stopped`, sent as kind `cancelled` (to a page that is no longer listening,
unless the stop came from elsewhere). The tiles or encoding left after a killed query end with
it.

**`world`:** `blaze_web::Graphs::rest` answers Flecs's REST over the build graph, but no v2
daemon code makes a `Graphs` or feeds it its builds. Until it does, `world` answers `no_world`,
naming `blaze_web::Graphs` in the daemon as what is missing. The interface (make one `Graphs` in
`blaze serve`, feed it each build's records, pass it in `Config::world`) is queued to its owner.

## The live stream

After a good `hello` the server opens one unidirectional stream. Every 250 ms (`live::EVERY`) it
reads the session's own `live::Feed`, a cursor into the flight recorder's rings, on the blocking
pool. It sends one frame for each table that has rows: `spans`, `counters`, then `open`. A frame
is a `u32` LE length, then a body shaped like an answer: the header
`{"op":"live","table","rows","at_ns","skipped","lost"}` padded so the IPC is 8-aligned counting
from the body's first byte, then the table's IPC stream. When the open set empties, one `open` frame with `rows: 0` and no IPC follows, so the page
clears what it shows (`open` replaces, the others append). An idle daemon sends nothing. A
certificate rotation sends a `cert` frame, and a frame that failed to encode sends
`{"op":"live","error":{..}}`.

**The bound for a slow page:** each session holds one feed frame. While a write waits on the
page (QUIC flow control: the browser's stream window), ticks are skipped, not queued, and the
next read takes everything since this session's previous one. Memory per session is therefore
one frame's buffers plus at most one read of the rings: 1 MiB of rows per recording thread at
most, 64 MiB with 64 recording threads, and in practice a few frames' worth. A page that falls
further behind than the rings hold loses the oldest rows. The next frame's `skipped` counts
them, and nothing waits on the page.

`live::Frame`'s `spans_rows`, `counters_rows` and `open_rows` say which tables to skip and fill
`rows`.

## `tiles`

`tiles::tiles(&[RecordBatch], Range) -> RecordBatch` is a function over `spans` batches, with no
I/O. It reads the columns by name and accepts any integer width the history or the live feed
writes, and dictionary or plain strings for the name. For each (process, thread, depth, bucket)
it gives the count of spans overlapping the bucket, `busy_ns` (the union of their time inside
the bucket, so it never exceeds the bucket), and the longest span's name. The last bucket may be
narrower, ending at `t1_ns`. A request may ask for at most 65536 buckets.

It is one sweep per track after a sort by (process, thread, depth, start). The sort is skipped
when the rows come sorted, as the op's SQL asks for. Buckets before a span's first are final once
that span is reached. The union is a high-water mark: a span adds only its part past the end of
every earlier span on its track.

Its tests cover a hand-checked example, the partial last bucket, range refusals, every accepted
column type, and missing or mistyped columns. A property test on 300 random cases checks the
sweep against brute force, and checks that `busy_ns` ≤ the bucket and that the counts add up.

## The probe

`blaze-explore-probe` is an echo server with the explorer's own certificate code. It prints
`<page>#127.0.0.1:<port>/<certhash>` and echoes bidirectional streams, unidirectional streams (on
one it opens) and datagrams. For each session it prints the CONNECT's path, Origin and
User-Agent, so a run also records what Origin each browser sends. Its input is one optional JSON
value, `{"page", "port"}`. The page is `probe/index.html` in the crate. It is static, has no
dependencies, and is served from an https origin. Its home is `https://ix.dev/explore/probe`,
published by the ix repository with the explorer page; any https host of the same file works,
passed as `page`. Never `file://` or localhost: the point is to test the real case, an https
page reaching 127.0.0.1. The test steps for Chrome, Firefox and Safari 27, and the empty results
table, are in explore-wire.md's observe-explore section.

## Speed of light

### Live

- **The work:** reading the rings, building the batches and compressing them are the feed's,
  costed in speed-of-light.md: about 1 ms of one core per 250 ms frame at 20,000 spans a second,
  or 0.4% of a core per watching session. Each session has its own cursor, so N sessions cost N
  times that. Sharing one read between sessions would need cursors in lockstep, and isn't worth
  it for the one or two pages a person opens.
- **The transport:** that frame is about 350 KB of columns before zstd, and columnar span rows
  compress about 5 to 10 times, so 35 to 70 KB a frame and 140 to 280 KB a second. AES-GCM
  through QUIC runs at gigabytes a second, so encrypting it is about 0.1 ms per frame, and
  loopback adds tens of microseconds.
- **Latency:** a span closed just after a tick reaches the page within 250 ms plus that frame's
  work (about 1 ms) plus loopback: about 252 ms. The tick sets the bound, not the hardware; the
  hardware bound is the display's frame, 16.7 ms at 60 Hz. A 50 ms tick would cost about 4 times
  the per-frame overhead for 5 times the freshness. 250 ms is the brief's, and a constant in
  blaze-profile.
- **Idle:** a daemon whose threads are inside spans (a loop span) sends a small `open` frame,
  well under 1 KB, 4 times a second. An idle daemon with nothing open sends nothing.

### Tiles

- **The DuckDB process start** is the floor of every history op, so measure it first. It is
  roughly tens of milliseconds for a fresh process opening the views; a long-lived DuckDB
  process would remove it, and that is observe-history's to decide.
- **The scan** projects 6 columns of one build's spans from Parquet. At about 25 bytes a span
  compressed and 1 GB/s of zstd decode, 1M spans are about 25 ms.
- **The sweep** reads about 40 bytes a span. At 20 GB/s that is 2 ms for 1M spans, the hardware's
  bound. The code does a few branches and a deque operation per span and per (span, bucket)
  visit, so expect 10 to 20 ns a span, or 10 to 20 ms for 1M: 5 to 10 times the bound, with no
  allocation per row.
- **The output** is at most (tracks × depths × buckets) rows. A 2000-pixel view of 64 threads at
  4 depths is at most 512K rows, and is far sparser in practice, since most tracks are idle most
  of the time. At about 30 bytes a row before zstd, a dense worst case is about 15 MB, and a
  typical view tens of KB.
- **The bar:** a zoom answers within 100 ms on a 1M-span build, DuckDB's start included.

### How to measure it

The answers' headers carry `elapsed_ns`, and `tiles` also carries `sql_ns` and `tiles_ns`. In an
ix VM, write a synthetic history of 1M spans (64 threads, 8 depths), then time `tiles` at 2000
buckets over the whole build and over 1% of it, and `spans` over 1% of it. For live, attach one
and then four sessions to a daemon running the 20,000-spans-a-second load from speed-of-light.md,
and read the daemon's CPU time on the `blaze-explore` thread and its blocking pool.

## Tests

| test | where |
|---|---|
| Frame layout: header padding for 8-alignment, the live frame's length and body, an IPC round trip with zstd | `frame::tests` |
| Token and origin refusals, the certhash test vectors, the link's shape | `link::tests` |
| Request parsing: every op, unknown ops, unknown and missing fields | `wire::tests` |
| Tiles: `busy_ns` never exceeds the bucket, counts add up, brute-force property, column types, refusals | `tiles::tests` |
| Certificate rotation on a virtual clock, the P-256 OIDs in the DER | `cert::tests` |
| Each op without a transport: views, view errors, no DuckDB, ranges, `no_world` | `ops::tests` |
| An in-process session with wtransport's client: path and origin refusals, `unauthenticated` before `hello`, `hello`, `views` as IPC, `unknown_op`, `no_world`, a live frame's layout, `bad_token` closing only its own session, the dev origin | `server::tests` |
