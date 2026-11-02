---
status: plan
summary: lane ci's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane ci: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here, written by the
agent that owns it. Each edits only its own section. The machine figures are those of
[link's speed of light](../link/speed-of-light.md#the-machine), unless a section says otherwise.
To them, add:

| what | figure |
|---|---|
| Unix socket round trip, same machine | about 5 µs |
| loopback TCP round trip | about 15 µs |
| `sendmsg`/`recvmsg` copy | about 10 GB/s |
| io_uring submission with registered buffers | about 0.05 µs an operation, amortized |
| a terminal's write | about 1 GB/s; the screen redraws 60 times a second |

## The wire: `crates/blaze-protocol/`, `crates/blaze-api/` (speed-link-edge-sol-wire)

Four components carry a request from a client to the session and back: the CBOR codec, the
frame on the socket, the session's step for a request, and the HTTP and JSON edge. The figures
for "before" and "now" below are estimates from the instructions each path runs. Nothing here
was built or timed (this lane ran no builds), so `proof` measures each one against its budget,
which is the bound × 1.1.

### The CBOR codec (`blaze_protocol::cbor`)

**Workload.** Two kinds of frame. A byte-heavy one is `Response::Ran` with 1 MiB of stdout. An
item-heavy one is a 64 KiB batch of 1,000 events of about 64 bytes, each with 8 fields.

**Bound.** A frame costs the larger of its bytes ÷ bandwidth and its items × the least work an
item needs. That least work is a head byte read, one branch on its major type and the value
stored: about 4 instructions, or 1 cycle at an IPC of 4. A struct field is a key and a value,
so about 2 cycles.
- **Byte-heavy.** Decoding into an owned `Vec<u8>` is one copy: 1 MiB read and 1 MiB written,
  2 MiB ÷ 110 GB/s (L2) ≈ 19 µs. Encoding is the same copy, ≈ 19 µs. Budget: 21 µs each way.
- **Item-heavy.** 8,000 fields × 2 cycles = 16,000 cycles ≈ 4.6 µs. The bytes are
  64 KiB ÷ 110 GB/s ≈ 0.6 µs, so the items set the bound. Budget: 5 µs each way.

**Where it was.** The encoder writes into one kept buffer and reorders a map only when its keys
come out of order. The decoder reads in one pass and borrows strings from the bytes. Both
already met the "one pass, no copy" part of the bound. Two things did not:
- **Every `Vec<u8>` went out as an array of integers.** That is 1 to 2 bytes per byte, and one
  serde element (a head, a branch, a push: 5 to 8 cycles) per byte. A 1 MiB stdout was about
  1.9 MB on the wire and about 2^20 × 6 cycles ≈ 1.8 ms to decode, roughly 100 times the bound.
- **Each map key's order check called `memcmp`.** That is about 8 cycles a field, against the 2
  cycles the whole field should cost. The decoder compared twice per key: once for equal, once
  for greater.

**What changed.**
- **Byte strings.** `crates/blaze-protocol/src/bytes.rs` is a `#[serde(with)]` module that
  writes `&[u8]` as one CBOR byte string and reads it back with one copy. For human-readable
  formats (JSON) it still reads the array form, so the JSON stays as it was. A CBOR array where
  a byte string is due is refused, so each value keeps one encoding. blaze-api's `SCHEMA` went
  from 10 to 11 with `Response::Ran`'s stdout and stderr, `TestResult::output` and
  `Request::Cargo`'s files moved onto it.
  `SCHEMA` 13 moved `Op::Write` and `Op::Edit`'s bytes, each `ContentId` and each `Fingerprint`
  onto it too (34 bytes and one copy for a `ContentId`, where 32 integers took about 62 bytes
  and 33 decoder steps).
  - Tests: `bytes/tests.rs` checks the exact bytes for each head length and that the array form
    is refused. It decodes 500 seeded values both ways against the reference codec, and checks
    that the JSON is unchanged. `lib.rs`'s `bytes_go_out_as_byte_strings` checks the API's exact
    CBOR and JSON.
- **Key order.** `cbor::key_order` decides the order from the first bytes when they differ. A
  text key's first byte holds its length below 24, so keys of two lengths need no call to
  compare the rest. The decoder now makes one comparison per key, not two.
  - Test: `keys_are_ordered_as_their_bytes_are`, 20,000 seeded pairs compared with slice order.
    The identity tests against `cbor::reference` cover the encoder and the decoder as before.

**Where it is now.**
- **Byte-heavy.** At the bound: one `memcpy` each way.
- **Item-heavy.** Still about 5 to 8 times the bound, at an estimated 10 to 16 cycles a field.
  Through serde, a field's key is written as a head and a short `memcpy` call, and read back
  with a `from_utf8` call and serde's string match. Reaching 2 cycles needs a blaze derive.
  That derive would write each struct's sorted keys as one constant byte run, check their order
  when it is compiled, and match a key on its encoded bytes. It is the design to build next. A
  derive keeps the bytes, so the identity tests carry over unchanged.
- **Correction (speed-sol2-ci).** The figure above held for blaze-api's envelopes, whose keys
  `frames::item_into` writes by hand, but not for the event records. `Function`, `Summary`,
  `Slow`, `Took`, `Fns`, `Cache`, most of `Event`'s variants and `blaze_protocol::Message` were
  declared out of key order, so each such map took the permute path: its entries were rescanned
  with `skip`, sorted as spans, and copied out to the scratch and back. That is about 250 more
  cycles a `Function`. `Event::Functions` (`object`, `whole`, `functions`) put the whole function
  array through the scratch twice, and a record over 1 MiB made a new scratch each time. The
  section "CBOR against memory bandwidth (speed-sol2-ci)" below has the count and what changed.

**What `proof` measures.**
- The byte-heavy frame: decode and encode, each within 21 µs.
- The item-heavy batch: decode and encode, each within 5 µs.
- The same values through [cbor4ii](https://github.com/quininer/cbor4ii)'s serde codec, the
  like-for-like comparison (serde, borrowed decode). blaze's codec should be within 10% of it
  now.
- The same values through [minicbor](https://gitlab.com/twittner/minicbor)'s derive codec with
  map encoding, the ceiling a generated codec reaches. The derive above should come within 10%
  of it.

### A frame on the socket (`blaze_api::frames`, the server's effects)

**Workload.**
- A `Status` round trip over the Unix socket.
- A build streaming 10^4 events a second of about 200 bytes each, so batches of 1 to 100 frames
  per read.

**Bound.** One syscall per batch each way at 0.15 µs, or 0.05 µs per operation with io_uring
registered buffers. On top of that, the copy at 10 GB/s.
- **Round trip.** The socket's 5 µs plus 2 × 0.15 µs. Budget: 5.6 µs.
- **Stream.** 0.15 µs ÷ k per frame for a batch of k frames. Throughput should be at least
  10 GB/s ÷ 1.1 ≈ 9 GB/s for 64 KiB batches.

**Where it was.**
- **Syscalls.** The server already did one `recv` per batch of frames and one `send` per batch
  of coalesced replies. The client over Unix did one read per batch through `Frames::fill`.
- **Zeroing.** `Frames::fill` grew its buffer to 64 KiB and zeroed it on every read. That is
  64 KiB at 32 B/cycle, about 2,000 cycles ≈ 0.6 µs, more than the syscall it fed.

**What changed.** `Frames` keeps an end mark. The buffer is zeroed once when it grows and read
into in place after that. `push` copies into room the buffer already has. `push_vec` adopts the
buffer it is given when nothing is held.
- Tests in `frames/tests.rs`:
  - `every_way_in_gives_what_the_old_frames_gave`: 400 seeds mix push, push_vec and a trickling
    reader, and compare what is held, what is wanted and every item against a model of the old
    `Vec`-based frames.
  - `a_warm_fill_allocates_nothing`.

**Where it is now.** At the bound in syscalls and with no zeroing. Two things are left:
- **Allocation per read and per send.** `blaze_core`'s `Done::Read` hands each read a new
  `Vec`, and `Done::Sent` does not hand the buffer back. That is a `malloc` and a `free` (about
  50 to 100 ns) per read and per send. Requests K2 (`RecvInto`) and K3 in `v2/requests/core.md`
  are with the coordinator, since lane core is done. When core gives registered io_uring
  buffers, the bound drops to 0.05 µs per operation.
  - **Correction (speed-sol2-ci).** K2 and K3 have landed: `Effect::Recv` takes the buffer it
    appends to (`into`), and `Done::Sent` gives the send's buffer back, which the server keeps
    for its next reply. The server still passed an empty `into` on every read, so the driver
    allocated 64 KiB each time; it now lends a kept buffer (see "The API's request path, end to
    end (speed-sol2-ci)").
- **v1's `read_frame_into`.** It reads the length and then the body: 2 syscalls per frame on an
  unbuffered stream. Its callers are v1 blaze-cargo's.

**What `proof` measures.**
- Syscalls per batch, counted in an ix VM: one `recv` and one `send` per batch on the server.
- The `Status` round trip within 5.6 µs.
- The stream's throughput of at least 9 GB/s.
- The same frames through a [monoio](https://github.com/bytedance/monoio) io_uring echo server
  on the same VM, as the floor. [TigerBeetle](https://github.com/tigerbeetle/tigerbeetle)'s
  io_uring message bus is the design reference for registered buffers.

### The session's step for a request (`blaze_api::session`, `server`)

**Workload.** A steady stream of `Status`, `Report` and `Cancel` requests on an open
connection.

**Bound.** One hash lookup (about 30 cycles on a warm Fx table) and one state change, with no
lock, clone or allocation. With the codec's share for a small request and reply (about 30
items), that is about 100 cycles ≈ 30 ns. Budget: 33 ns per request, excluding the socket.

**Where it was.** `live` was a `BTreeMap`, which allocates a node each time a connection's
requests go from none to one: a `malloc` and a `free` per request on an idle connection.

**What changed.** `live` is an `FxHashMap`, which keeps its capacity. `gone` (rare: once per
closed connection) collects the connection's requests and radix-sorts them by id, so the order
it cancels them in is the order the `BTreeMap` gave.

**Where it is now.** One allocation-free lookup on the steady path, plus the following:
- **Two lookups for `report`.** It looks in jobs, then in pending. A slab indexed by the
  request's id would make that one.
- **One `String` per text field on decode.** The owned API types allocate it, about 20 ns
  each. Borrowed mirror types (`&'a str` fields, decoded from the frame in place) would remove
  it, and the decoder already supports them.
- **The core allocations** from the socket section, until K2 and K3 land. (Correction,
  speed-sol2-ci: they have landed, and a warm Unix request now allocates nothing on its way in
  or out; see "The API's request path, end to end (speed-sol2-ci)".)
- **A one-item `views` vector per build request.** Left as noise next to a build.

**What `proof` measures.**
- A warm `Status` request with `blaze_alloc_count::assert_at_most(0, ..)`. This will fail until
  K2 and K3 and the borrowed types land, and it is the check for them.
- The time per request within 33 ns.

### HTTP and JSON at the edge (`blaze_api::http`, `json`)

**Workload.**
- An HTTP API request: a 300-byte head with 6 headers and a 200-byte CBOR body.
- The JSON view of a 64 KiB event batch.

**Bound.**
- **The head.** One pass over it: UTF-8 checked at about 16 B/cycle and line ends found with
  `memchr` at about 32 B/cycle, about 30 cycles for 300 bytes. Then about 5 cycles to split each
  header. That is about 60 cycles ≈ 17 ns, so the budget is 19 ns. The body is never copied
  when it has a length. A chunked body is copied once, which is the least that joins its
  chunks.
- **JSON.** Written once into the output buffer, with escapes found by SIMD at about 16 B/cycle.
  64 KiB is about 4,000 cycles ≈ 1.2 µs. Budget: 1.3 µs.

**Where it was.** The head became 2 `String`s per header, a `Vec` and an owned method and path:
about 15 allocations ≈ 300 ns, read in several passes. That is about 15 times the bound. The
server then decoded the body from the owned copy.

**What changed.** `RequestParser::next_ref` lends an `HttpRequestRef<'_>`. Its method, path,
header lines and body all borrow the parser's buffer, and they are given back on the next call.
The server reads requests this way and decodes the CBOR body in place. The head is checked
without allocating: UTF-8 first, then a bare CR found with `memchr`, in the same order and with
the same errors as before. blaze-ci's webhook reads in place too, so `next` (owned, `HttpRequest`
unchanged) is kept for callers that keep the request past the next call, and the tests compare
against it.
- Tests in `http/tests.rs`:
  - `a_request_in_place_is_the_request`: 300 seeds feed 4 pipelined requests to an owned and a
    lent parser, including a chunked one with a trailer. The test compares what is held and
    every request. Six bad inputs give the same error from both.
  - `a_request_in_place_allocates_nothing`.

**Where it is now.**
- **The head.** No allocation. It is walked about 3 times: the check, the framing headers, and
  one walk per header the server looks up. That is an estimated 1.5 to 2 times the bound. One
  scan that records each header line's offsets into a fixed array would make it one pass.
- **JSON.** `serde_json::to_writer` writes once into the output buffer, but escapes strings a
  byte at a time, about 1 B/cycle. A string-heavy 64 KiB batch is about 19 µs, roughly 15 times
  the bound. Reaching the bound needs a SIMD writer such as
  [sonic-rs](https://github.com/cloudwego/sonic-rs), a new workspace dependency for whoever owns
  that call.
  - **Changed (speed-sol2-ci).** `json::line` and `line_into` write through `sonic_rs::to_writer`
    (already a workspace dependency, used by blaze-ci's webhook), which finds escapes 16 or 32
    bytes a step. Its escapes are serde_json's. The two write floats apart (zmij against ryu),
    and the API's types hold no float. serde_json moved to blaze-api's dev-dependencies, where
    the tests hold sonic-rs to it:
    - `frames/identity.rs`, `json_is_the_bytes_serde_json_wrote`: 3,000 seeded requests and
      responses of every kind give the same bytes from both writers.
    - `json/tests.rs`, `escapes_are_the_bytes_serde_json_wrote`: every character below 0x80, plus
      U+00E9, U+2028 and U+1F525, alone and in runs of 1 to 64 bytes at the start, the middle and
      the end of a run, so each SIMD step's edge is crossed.
- **`http::read_response`.** It allocates a 64 KiB buffer per call. It is the client side, once
  per request.

**What `proof` measures.**
- The head parsed in place within 19 ns, compared with [httparse](https://github.com/seanmonstar/httparse)
  (SIMD) on the same heads.
- JSON for the 64 KiB batch within 1.3 µs, compared with sonic-rs and
  [simdjson](https://github.com/simdjson/simdjson)'s writer.

### What stands in the way

These are what keep the wire from its bound, each with its owner:
- **K2 and K3 in `v2/requests/core.md`**: reads into a kept buffer, and sends that hand the
  buffer back. With the coordinator. (Correction, speed-sol2-ci: both landed and are used.)
- **A blaze derive for the API types**: one owner to name. It would write constant key runs and
  borrowed mirror types, for item-heavy frames and allocation-free decode. (speed-sol2-ci: the
  new crate and its workspace dependencies are asked of the coordinator as queue item 00908.)
- **A SIMD JSON writer**: a dependency decision for lane ci. (Correction, speed-sol2-ci: done,
  with sonic-rs.)

## CI, the terminal view and the build page: `crates/blaze-ci/`, `crates/blaze-ui/`, `crates/blaze-web/` (speed-link-edge-sol-ci)

Three crates carry a build out to people. blaze-ci takes GitHub's webhooks, checks out the
commit and posts check runs. blaze-ui draws the live region in a terminal. blaze-web streams
records to the build page, which draws the program in a browser. The "before" and "now" figures
below are estimates from the work each path does. Nothing was built or timed (this lane ran no
builds), so `proof` measures each one against its budget, which is the bound × 1.1.

### A webhook delivery (`blaze_ci::runtime::listen`, `webhook`)

**Workload.** A 25 KB `pull_request` delivery over loopback TCP from the VM's proxy. A push
delivery is about 8 KB and scales down from it.

**Bound.** The body has to come in from the kernel, be authenticated and be decoded, once each.
- The kernel's copy into the process: 25 KB ÷ 10 GB/s ≈ 2.5 µs.
- HMAC-SHA-256 over the body with SHA-NI: 25 KB ÷ 2 GB/s ≈ 12.5 µs. The key's two padded blocks
  are about 0.1 µs, so they can be computed for each delivery.
- A validating decode at simdjson's rate: 25 KB ÷ 2.5 GB/s ≈ 10 µs. It must validate all of the
  body, including the fields it skips, or it would accept what GitHub never sends.
- The syscalls: `accept`, about two `read`s, `write` and `close`, 5 × 0.15 ≈ 0.75 µs.

That is about 26 µs, so the budget is 28.6 µs. HMAC and decode on two threads would overlap
them, but a thread's wake (5 to 10 µs) costs what the overlap saves.

**Where it was.**
- A thread started for each connection (a stack mapped, a clone, an exit): about 20 µs.
- A zeroed 64 KiB read buffer for each connection: about 1.5 µs.
- The request copied from the read buffer into the parser (2.5 µs), with the parser's buffer
  regrown about five times on the way to 25 KB (about 3 µs).
- `RequestParser::next` made two `String`s per header plus the method and path (about 1 µs),
  and split the body out and drained the head in front of it (a 25 KB `memmove`, about 1.5 µs).
- serde_json validating and skipping at 1.5 to 2 GB/s: about 15 µs.

With HMAC and the kernel's share, that is about 60 µs: 2.3 times the bound.

**What changed.**
- **Kept handler threads.** The accepting thread hands each connection over a channel to a
  handler that waits for it. A handler is started only when every one is busy, up to
  `MOST_CONNECTIONS` (64), and then kept. Above that a connection is answered 503 on the
  accepting thread, and GitHub redelivers. Each handler has one read buffer for every
  connection it reads.
- **Read in place.** The handler reads with `RequestParser::next_ref`, and
  `webhook::receive_ref` checks the lent head and body. `receive` (owned) stays, and both go
  through one function, so what is taken and refused is the same.
- **sonic-rs for the decode.** `webhook::parse` decodes with `sonic_rs::from_slice` into the
  same serde payload types, at 2 to 3 GB/s instead of serde_json's 1.5 to 2. That is about
  10 µs instead of 15. `parse_by` takes the decoder as a type, so the tests hold sonic-rs to
  serde_json.
- Tests:
  - `runtime/tests.rs`: `needs_loopback_deliveries_at_once_are_each_answered_by_a_kept_handler`
    (three rounds of 16 deliveries at once) and
    `needs_loopback_a_connection_over_the_most_open_is_answered_503`.
  - `webhook/tests.rs`: `a_request_read_in_place_is_received_as_the_owned_one_is` runs 14
    requests (good, unsigned, tampered, wrong method, missing headers, other content types, a
    header given twice) through both parsers, the in-place one fed 7 bytes at a time, and asks
    for equal answers.
  - `webhook/tests.rs`: `sonic_rs_takes_and_refuses_the_payloads_serde_json_did` decodes 11
    payloads with both decoders, plus every prefix of each, every byte of each replaced by 21
    others, a field given twice, trailing bytes, and edge numbers, escapes and nesting. It asks
    for the same delivery, or the same status and words up to the decoder's own message.

- **A parser that reads into its own room** (speed-link-edge, `blaze_api::http`).
  `RequestParser::read_from(&mut impl Read)` reads into the room after what the parser holds:
  the rest of the body once the head has given its `Content-Length`, else 16 KiB. The room is
  zeroed once, when it grows, and an end mark says how much of it is the stream, as in
  `frames.rs`. `reset()` keeps the room (up to 1 MiB) for the next connection, so each handler
  keeps one parser and no read buffer of its own. That saves the copy into the parser and its
  regrowth, about 5.5 µs. Test: `http/tests.rs`
  `a_parser_read_into_is_the_parser_pushed_to` (200 seeds of short and interrupted reads,
  against `push`, on one parser reset between connections, and a broken stream).

**Where it is now.**
- The hand-off to a waiting handler is one futex wake, about 3 µs.
- HMAC: 12.5 µs.
- sonic-rs: about 10 µs.
- The kernel's share: about 3.25 µs.

That is about 28 µs, within the 28.6 µs budget. The 3 µs hand-off is what is left over the
bound. Removing it means each handler blocks in `accept` itself, and then a connection over
the most open waits in the kernel's backlog instead of being answered 503.

**What `proof` measures.** The time from the connection's first byte to the event in the
service's channel, for the 25 KB delivery on loopback, within 28.6 µs. Against the state of the
art: ring's HMAC with SHA-NI (about 2 GB/s, the same as OpenSSL's), simdjson's and sonic-rs's
`from_slice` on the same body, and a Probot-style Node receiver (`JSON.parse` at about
0.5 GB/s) as the common case.

### A check-run update (`blaze_ci::github`, `runtime::post`)

**Workload.** The checks of one commit: 10 to 30 runs created, then each moved through queued,
in progress and done.

**Bound.** GitHub has no call that creates or updates several check runs at once. GraphQL takes
several mutations in one document, but runs them one after another and counts each one toward
the secondary limits, so it saves round trips without saving GitHub's time. An update is
therefore one HTTPS request on a kept connection: one round trip to api.github.com, about 30 to
80 ms from the VM, plus GitHub's own time. GitHub allows 100 requests at once and 80
content-creating requests a minute. `k` updates at once are bounded by `ceil(k / lanes)` round
trips, with the per-minute limit as the ceiling over a burst.

**Where it was.** One thread posted every update in turn, so `k` runs at once cost `k` round
trips: about 1.5 s for 30 runs at 50 ms.

**What changed.** `POST_LANES` (8) threads each post on their own kept connection. They share
one installation token, minted once for all of them (`GitHub::lane`). A run's updates always go
to the same lane (by its id), so they stay in order, and an update superseded before it is sent
is still dropped. 30 runs now take `ceil(30 / 8) = 4` round trips, about 200 ms. Eight stays
well under GitHub's 100 at once. Tests in `runtime/tests.rs`:
`the_runs_of_a_job_post_at_once_each_on_its_lane` and `a_runs_posts_stay_in_order_on_its_lane`.

**Where it is now.** At the bound for the API GitHub offers: `ceil(k/8)` round trips. More
lanes would trade that against the secondary limit with nothing gained for one commit's checks.

**What `proof` measures.** The time from the first run created to the last one done, for 30 runs
against a fake GitHub that answers after 50 ms, within `ceil(30/8) × 50 ms × 1.1 = 220 ms`. The
Actions runner and the Buildkite agent post one update per state change to their own services,
one request each. blaze-ci's cost per update is the same, and what sets it apart is that updates
go out 8 at a time.

### A checkout (`blaze_ci::checkout`)

**Workload.** A push to a pull request's branch: fetch the commit, find what changed against
the base, and read the changed files.

**Bound.** The pack bytes over the network, plus the TLS handshake and git's negotiation (about
3 round trips), and the disk write of what git stores. For a small push that is a few hundred
milliseconds, nearly all of it network.

**Where it is.** `git fetch`, then `rev-parse`, `diff-tree` and one `cat-file --batch` for every
changed file. That is four processes at 1 to 2 ms each, under 10 ms against a fetch of hundreds
of milliseconds, so within 10% of the bound already. Nothing changed here. What does cost
more than it should is on the daemon's side: a change's file bytes go out as CBOR arrays of
integers (the wire's `Op` byte strings, `v2/requests/vfs.md`), and the default branch's ops are
sent twice, once to open its view and once to move it (`Request::Move`, a compare-and-swap of a
head, asked of blaze-api).

**What `proof` measures.** The checkout of the corpus's pull requests, within 1.1 times
`git fetch --depth=1` of the same commit plus 10 ms. `actions/checkout` runs the same fetch and
then writes a whole worktree, which blaze-ci never does.

### A record on the terminal (`blaze_ui::Live`)

**Workload.** A cold build of a workspace of about 300 crates: thousands of records a second,
with a live region of 10 to 20 lines below the lines that stay.

**Bound.** The screen is redrawn 60 times a second, and nothing drawn more often than that is
seen. So the bound is one `write` per frame of only the bytes that changed. A frame of 20 lines
of about 100 bytes is 2 KB at most, ≈ 2 µs at the terminal's 1 GB/s, plus one 0.15 µs syscall:
about 130 µs a second. The region's diff is bounded by its lines. Most of them hold a timer or a
count, so most change every frame, and drawing the region and diffing it line by line is
proportional to what changed.

**Where it was.** A draw per record: thousands of `write`s a second, each a region drawn and
diffed. A screen can't show those, and they kept the terminal busy.

**What changed.** `Live` draws at most once a frame (`FRAME_NS`, 1/60 s; `Live::frames(0)` gives
a draw per record, as the tests' reference). A record within a frame of the last draw is
applied, and its lines that stay are held. The next due draw writes them, then the region once,
in one `out`. A build's end is never held: the `Finished` record writes the held lines and the
summary at once. `Live::due_ns()` tells the caller when the held draw is due. Test in
`tests.rs`, `records_faster_than_frames_are_drawn_once_a_frame`, over 200 seeds:
- draws are at least a frame apart, except at the build's end;
- the screen equals the draw-per-record reference whenever nothing is held, after the due tick
  and after `finish`;
- the number of writes is bounded by the frames that passed.

**Where it is now.** At the bound inside `Live`. The caller in blaze-cargo still ticks only
after 100 ms with no records, so after a burst the last frame can show up to 100 ms late. It
also draws through `record` once per record, which makes a `String` for each. Both are asked of
lane cargo in `v2/requests/cargo.md`: a read timeout of the shorter of the tick and `due_ns()`,
and records handed over together through `records_into`.
- **Correction (speed-sol2-ci).** Both are done. `blaze-cargo/src/cargo/say.rs` hands each batch
  to `live.records_into(batch, now_ns, &mut out)` and reads with a timeout of `due_ns()` less
  now, so the terminal path is at its bound end to end.

**What `proof` measures.** For a replayed cold build's records at full speed, the `write`s per
second at most 60 and the bytes per frame within 1.1 times the changed lines. Against the state
of the art:
- indicatif draws at most 20 times a second by default;
- ratatui diffs a cell buffer and flushes once a frame;
- notcurses renders only damaged cells.

Line-level diffing is coarser than a cell diff. On a region whose changed lines are timers and
counts it writes nearly the same bytes, for far less work per frame.

### A record to every page (`blaze_web` on the daemon)

**Workload.** A cold build's record stream, in batches of up to `BATCH` (4096) records or
`FRAME` (1 MiB), to 1 to 5 open pages.

**Bound.** Each record is encoded once (the wire's item-heavy CBOR bound: about 2 cycles a
field). Each page gets one `write` per batch: 0.15 µs plus bytes ÷ 10 GB/s. A 64 KiB batch to
three pages is about 4.6 µs of encoding and 3 × 6.7 ≈ 20 µs of sending. Budget: 27 µs.

**Where it is.** At the bound. `blaze_protocol::cbor::append` encodes each record once into the
batch, the batch becomes one `Arc<[u8]>` frame shared by every page, and each page's writer
sends it with one write. The two copies on the way (into the frame and the kernel's) cost
under 1% of it. Nothing changed. This is uWebSockets' `publish` with corking: one message built,
one syscall per socket.
- **Correction (speed-sol2-ci).** The sending was at the bound, but the encoding was not. A
  `Functions` record's maps were out of key order, so every function took the encoder's permute
  path, about 250 cycles over its own cost (see "The CBOR codec" above). The encoding is now at
  the codec's serde cost; "The web and UI streams per record (speed-sol2-ci)" below has the count.

**What `proof` measures.** The time from a batch's last record to its last page's `write`,
within 27 µs for three pages, and one write per page per batch.

### The page (`crates/blaze-web/assets/`)

**Workload.** A cold build of a 200,000-function program. Its functions arrive in hundreds of
`Functions` records of a few hundred functions each, and the page draws every function as a
point and every call as a line.

**Bound.** Per frame, the bytes of that frame's records decoded once, and only the points they
touched restyled and uploaded to the GPU:
- **Decode.** About 1 GB/s for a JIT's typed-array scan. A 64 KiB frame is about 65 µs.
- **Model and restyle.** About 20 ns for each touched point: its key looked up, its columns
  written, its colour and size computed.
- **Upload.** 20 bytes for each touched point (an RGBA colour and a size).

A frame that brings 400 functions is about 65 + 8 µs, plus 8 KB uploaded.

**Where it was.**
- **Decode.** Every frame was decoded into one JS object per function, with a string per
  field, a BigInt per key and an array per call list. That is about 6 allocations a function.
- **Model.** The model looked each function up in a `Map` keyed by BigInt.
- **Counts.** The status line's counts rescanned all `n` functions every frame.
- **Restyle.** `restyleAll` made new colour and size arrays and restyled all `n` points every
  frame. During a cold build, with 200,000 points, that is about 4 ms of script a frame, and
  then a 4 MB upload.

**What changed.**
- **Columns.** `parseFrame` reads a `Functions` record's functions straight into `FnColumns`
  (`records.js`), typed arrays with one string per name. An ASCII name is a substring of one
  Latin-1 decode of the whole frame, so it costs no decoder call. A function that is not
  exactly as the fast reader takes it sends the whole array back to the generic decoder and
  checker, so what is refused, and the first error said, are the same as before.
- **Keys.** Functions are keyed by `KeyIndex` (`keys.js`): open addressing over the key's two
  32-bit halves, with no BigInt and no `Map` entry per function.
- **Counts.** The model keeps `howCount` as functions change, so `counts()` takes constant time.
- **Restyle.** `Paint` (`paint.js`) keeps the colour and size arrays from frame to frame, with
  spare room, plus the list of points drawn lit. A frame restyles the points its records
  touched, plus the lit points when a new build is shown, since those are the only ones that
  can dim. Flashes are repainted as before. `restyleAll` is left for a whole load.
- Tests in `dev/test.mjs`:
  - columns from `parseFrame` equal those read by items, over 300 seeds (shuffled keys,
    non-ASCII names and crates, keys above 2^53, every `how`);
  - 21 malformed functions, each in four placements, give the same error class and message
    from both readers;
  - `KeyIndex` against a `Map`;
  - merged functions and `counts()` against a one-by-one reference and a scan;
  - `Paint`'s partial restyle against the old every-point restyle, kept in the test, over 40
    seeds of 80 frames.

**Where it is now.** On the CPU, proportional to what changed: the records' bytes, the touched
points and the lit points when a build begins. On the GPU, still O(n) per frame that changes:
cosmos.gl 3.5.1 takes only whole arrays (`setPointColors`, `setPointSizes`, `setLinks`,
`setPointClusters`, `setPointPositions`) and uploads all of each. For 200,000 points that is 4 MB
and about 1 ms of cosmos's own copying a frame. That fits in a 16.7 ms frame, but it is not
proportional.

What stands in the way is a call in cosmos that takes a range of points and writes only those
texels (`texSubImage2D`). It goes in the vendored cosmos source under `assets/vendor/build/`,
and rebuilding the bundle needs npm, which this lane could not run. The model's call lists
(`out` and `inn`, one JS array per function) are the other place left with objects instead of
columns. Columns with a `next` link per call would append in constant time too, but they walk a
function's calls in the reverse order, which would change the order of the spread's levels. So
that change waits for the spread to sort its levels.

**What `proof` measures.** The page's script time per frame, replaying a cold build's frames in
a headless browser, within 1.1 times (bytes ÷ 1 GB/s + 20 ns × touched points). The GPU upload is
measured separately and reported as the open gap. Against the state of the art: Perfetto UI
keeps its tables in columns and draws only what is in view, and Solid updates only the DOM
nodes whose signals changed. The page matches both on the CPU side. It falls short of Solid's
precision on the GPU side until cosmos takes partial updates.

### What stands in the way

- **The terminal caller's tick and batching**: `v2/requests/cargo.md`. (Correction,
  speed-sol2-ci: done by lane cargo.)
- **Partial uploads in cosmos.gl**: a patched build under `assets/vendor/build/`, with npm.
- **`Request::Move`** for the checkout's daemon side: blaze-api. (`Op`'s bytes are byte strings
  since `SCHEMA` 13.)

## The request path, CBOR against bandwidth, and the streams per record (speed-sol2-ci)

A second pass over every component of lane ci's crates (`blaze-api`, `blaze-ci`,
`blaze-protocol`, `blaze-ui`, `blaze-web`), read from the code as of 2026-10-30. Like the
sections above, the figures are estimates from the instructions each path runs, using
[link's machine figures](../link/speed-of-light.md#the-machine). Nothing was built or timed, so
`proof` measures each one against its budget, which is the bound × 1.1.

### The API's request path, end to end (speed-sol2-ci)

**Workload.**
- One small request (`Status`, `Cancel`) and its reply, alone on a Unix connection.
- 100 such requests pipelined, so one read brings them all.
- An HTTP keep-alive POST with a 300-byte head and a 200-byte CBOR body.

**The work, counted per request.**
- One `recv` per batch, and one `send` per batch of replies.
- On Unix, the frame's length head and the cut of the item: about 5 cycles.
- Decoding the envelope and body: about 15 fields.
- Dispatch: one Fx hash lookup and one insert, about 30 cycles each.
- Encoding the reply: the envelope's keys are one constant run, and the body is about 5 fields.

**Bound.** For a batch of k requests, 2 × 0.15 µs ÷ k for the syscalls, plus about 100 cycles
(30 ns) of codec and lookup:
- k = 1: 0.30 µs + 0.03 µs = 0.33 µs, on top of the socket's 5 µs round trip. Budget: 0.36 µs.
- k = 100: 3 ns + 30 ns = 33 ns. Budget: 36 ns.
- HTTP: the head's 17 ns (see "HTTP and JSON at the edge") added to the k = 1 figure.

No request should allocate on the way in or out.

**Where it was.**
- **Syscalls.** At the bound: one `recv` and one `send` per batch.
- **Every read allocated.** The server passed `Effect::Recv` an empty `into`, so the driver
  reserved 64 KiB for every read. That is a `malloc` and a `free`, about 50 to 100 ns. When part
  of a frame was held, `Frames::push_vec` copied it, then dropped the read's buffer. HTTP did the
  same.
- **Session.** `Request::Commit` and the end of an edit cloned every `Op` into the VFS
  (`commit`), file bytes included. That is a copy of each edit's bytes per commit.

**What changed.**
- **Unix.** `Frames::lend` moves what is held to the front of the frames' own buffer and lends
  the whole buffer as the read's `into`. The driver appends after what is held, and
  `Frames::give_back` takes it back. Nothing is copied or allocated once the buffer has grown,
  and the room it keeps is capped at 1 MiB.
- **HTTP.** `RequestParser::push_vec` either adopts the read's buffer and frees the one it held,
  or copies the read and frees the read's buffer. Either way one buffer is free, and it is kept
  as the parser's `spare`, emptied and capped at 1 MiB. `lend_spare` lends it to the next read.
  The parser is still read while that read is in flight, which is why it lends the spare and
  not the buffer it reads from.
- **Session.** Both commit calls pass their ops by value (`Vfs::commit_owned`, queue item 00879
  of lane vfs), so an edit's bytes are not copied again.
- **Tests.**
  - `frames/tests.rs`, `every_way_in_gives_what_the_old_frames_gave`: a fourth way in (lend,
    append, give back) checked against the model.
  - `frames/tests.rs`, `a_warm_lent_buffer_allocates_nothing`: a frame split over two reads, at
    most 0 allocations over 100 rounds.
  - `http/tests.rs`, `a_warm_read_into_the_spare_allocates_nothing`: a request over two reads,
    so both the adopting and the copying way are taken, at most 0 allocations.
  - `http/tests.rs`, `a_broken_parser_still_lends_its_spare`.
  - `server/tests.rs`, `a_warm_request_and_its_reply_reuse_their_buffers`: on Unix and on HTTP,
    a warm request's in-flight receive has room for all it may read, so the driver's read
    allocates nothing either.

**Where it is now.**
- **Syscalls.** At the bound.
- **Allocations.** A warm Unix request allocates nothing on its way in or out. `Status` makes
  one, the `requests` list of the owned response, and that is made by whoever answers, not by
  the path. An HTTP request allocates once, for the reply's head buffer while the previous send
  is in flight.
- **Codec.** About 10 to 16 cycles a field × 15 fields ≈ 150 to 240 cycles ≈ 45 to 70 ns a
  request, plus about 20 ns for each text field's `String`.
- **The gap.**
  - k = 1: 0.30 + 0.07 + 0.02 ≈ 0.39 µs against 0.33 µs, about 1.2 times. It was about 1.5
    times with the allocation per read.
  - k = 100: 3 + 70 + 20 ≈ 93 ns against 33 ns, about 3 times.

  Both gaps are the codec's: serde's per-field calls and the owned `String`s. What stands in
  the way is the generated codec with borrowed mirror types, a new crate the coordinator owns:
  queue item 00908.

**What `proof` measures.**
- In an ix VM, `strace -c` on the daemon: one `recvmsg` and one `sendmsg` per batch.
- `blaze_alloc_count` over a warm request: 0 on Unix and 1 on HTTP. The tests above already
  check this.
- The time per request at k = 1 within 0.36 µs plus the socket, and at k = 100 within 36 ns.
  Until 00908 lands, the k = 100 figure is reported as the open gap.

### CBOR against memory bandwidth (speed-sol2-ci)

**Workload.**
- The byte-heavy frame above: `Response::Ran` with 1 MiB of stdout.
- A cold build's `Functions` records, up to 1 MiB each. Each function is about 100 bytes with 9
  fields counting nested ones (`id`, `how` and its variant, `key`, `name`, `calls` and its
  items, `krate`).
- v1 blaze-cargo's records, each sent through `blaze_protocol::send` as a `Message`.

**Bound.** The larger of bytes ÷ bandwidth and items × 2 cycles a field:
- **Byte-heavy.**
  - In cache: 2 MiB through L2 at 32 B/cycle ≈ 19 µs.
  - Cold from DRAM: 1 MiB read and 1 MiB written at 25 GB/s ≈ 84 µs.
- **A function.** max(100 B ÷ 32 B/cycle ≈ 3 cycles, 9 × 2 = 18 cycles) = 18 cycles ≈ 5 ns.
  Items set the bound, not bandwidth.
- **A 1 MiB `Functions` record** (about 10,000 functions): 180,000 cycles ≈ 51 µs, against
  9.5 µs for its bytes through L2.

So item-heavy CBOR is about 5 times slower than memory bandwidth even at its bound.

**Where it was.** The byte-heavy frame was at the bound, with one `memcpy` each way. The records
were not:
- **Each function went through the permute path.** `Function` declared its fields as `id`,
  `key`, `krate`, `name`, … against key order `id`, `how`, `key`, `name`, `calls`, `krate`. When
  `name` came after `krate`:
  - the encoder rescanned the entries already written with `skip`, about 50 cycles;
  - it kept a span for each of the 6 entries, about 30;
  - it sorted them by comparing their bytes, about 100;
  - it copied the map out to the scratch and back, about 70.

  That is about 250 cycles on top of serde's 90 to 145. In all, about 350 to 400 cycles
  ≈ 100 to 115 ns a function, about 20 times the bound.
- **Whole records took the permute path too.** `Event::Functions` (`object`, `whole`,
  `functions`) and `Message` (`schema`, `id`, `body`) were out of order at the top, so the whole
  record went through the scratch twice. A record over `KEPT_SCRATCH` (1 MiB) made a new
  scratch each time: about 256 first-touched pages × 0.25 µs ≈ 64 µs a record.
- **Each key's order check read back the output.** It compared the key's encoded bytes with the
  previous key's, which were already written.

**What changed.**
- **`blaze_protocol::in_key_order!`** (`keyed.rs`), a `macro_rules` that wraps a struct or an
  enum of struct variants and is given its keys in CBOR order. Its `Serialize` writes the fields
  in that order to CBOR (`is_human_readable` false), and in the declared order to JSON, so the
  JSON is byte for byte unchanged. Deserialize stays derived.
- **The wrapped types.** `Message`, every variant of `Event`, and `Slow`, `Function`, `Fns`,
  `Summary`, `Cache` and `Took` are wrapped.
- **Key order without reading the output.** Struct keys are `&'static str`, so `Map::name`
  orders them by `cbor::name_order` (length, then bytes) against the last name, without reading
  back what was written.
- **Tests.**
  - `cbor/tests.rs`, `names_are_ordered_as_their_encodings_are`: seeded names of 0 to 256 bytes
    of ASCII and two-byte characters, ordered as their encodings are.
  - `tests.rs`, `every_record_is_written_in_its_keys_order`: one record of every variant with
    every field set. Each is written with `permuted() == 0`, in exactly the bytes of
    `cbor::reference`, reads back to itself, and, decoded as a JSON value, equals serde_json's
    value of it.
  - `tests.rs`, `json_keeps_the_declared_order`: exact JSON for a `Function`, a `Message` of
    `Began` and a default `Summary`.

**Where it is now.**
- **Byte-heavy.** At the bound.
- **Item-heavy.** The permute path is never taken for a record, and the scratch is never used.
  A function is serde's 90 to 145 cycles ≈ 26 to 41 ns, which is 5 to 8 times the bound, down
  from about 20 times.
- **Still out of order.**
  - blaze-api's `Cargo`, which goes out once per build request.
  - blaze-query's `Input::FileAt` and `Input::DirAt` (`root`, `path`), which go out only in a
    `blaze why` answer.
  - blaze-ci's own types, which are JSON only.

  None is per record. The rest of the gap is serde's per-field calls. What stands in the way is
  the generated codec, queue item 00908.

**What `proof` measures.**
- Encoding a 1 MiB `Functions` record, in cache, within 1.1 × 51 µs. Until 00908 lands, it is
  reported against today's 5 to 8 times, and against the same record through the pre-change
  types (permuted) as the before.
- The byte-heavy frame within 21 µs in cache and 92 µs cold.
- `Writer::permuted()` summed over a replayed cold build is 0.
- minicbor's derive on the same records, the ceiling a generated codec reaches.

### The web and UI streams per record (speed-sol2-ci)

**Workload.** A cold build's records, thousands a second, to 1 to 5 pages and one terminal.

**The work, counted.**
- **To the page.**
  - Each `Functions` record is encoded once into the batch, by a `Writer` kept across records.
  - Any other record's bytes are copied from the encoding kept for it (`last_told`).
  - A batch closes at 4,096 records or 1 MiB (`BATCH`, `FRAME`) and becomes one `Arc<[u8]>`.
  - Each page's websocket writer makes one write per wake. A payload of 64 KiB or more goes out
    in place with `writev`; smaller ones are gathered into up to 256 KiB first.

  So a record costs 1/k frames and 1/k syscalls per page in a batch of k records, and its
  encoded bytes, about 100 per function.
- **To the terminal.** Each record is applied to `Live`. At most 60 writes a second go out, of
  the changed lines only.

**Bound.**
- **Page.** Per record, its items × 2 cycles, encoded once for all pages, plus
  (0.15 µs + batch bytes ÷ 10 GB/s) ÷ k per page. A 64 KiB batch of 600 functions to three
  pages: 600 × 18 cycles ≈ 3.1 µs, plus 3 × 6.7 ≈ 20 µs. Budget: 25 µs.
- **Terminal.** As in "A record on the terminal": about 130 µs a second.

**Where it was.**
- **Page.** Frames, syscalls and copies were at the bound. Encoding was about 20 times the
  bound, through the permute path: about 600 × 375 cycles ≈ 64 µs a batch, which made the
  batch about 3.4 times its budget.
- **Terminal.** At the bound, once lane cargo's caller changes landed.

**What changed.** The key-order change above, with nothing in blaze-web or blaze-ui itself.
Every byte sent to a page is the same: the reference-codec test checks each record's exact
bytes.

**Where it is now.**
- **Page.** Encoding is about 600 × 120 cycles ≈ 21 µs, so the batch is about 41 µs against
  23 µs, about 1.8 times. The gap is the codec, queue item 00908. Frames and bytes per record
  are at the bound: 1/k frames, one write per page per batch, and no byte written twice
  except the one copy into the frame.
- **Terminal.** At the bound.
- **blaze-ci.** Webhooks, check runs and checkouts happen once per event and are not a stream.
  They are as the sections above say, and unchanged.

**What `proof` measures.**
- Replay a cold build's records to three pages in an ix VM, and count:
  - websocket frames per page per batch: 1;
  - bytes per record against the record's encoded size: within 1.1 times;
  - the time from a batch's last record to its last page's write: within 25 µs, reported
    against today's estimated 41 µs until 00908.
- For the terminal: at most 60 writes a second, and bytes per frame within 1.1 times the
  changed lines.
