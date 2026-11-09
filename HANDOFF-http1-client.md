# Handoff: lane T1 `http1-client` (docs/v2/plan/design/no-tracing.md, 6.1)

Code only: nothing was built, checked or tested (no blaze, cargo or VM ran). Cargo.lock is
untouched; the changes it should get are listed below. This second pass replaces the first
(0447e23a8f, a pool on `hyper::client::conn::http1` plus `tokio-rustls`) with a sans-IO client,
so any IO backend can drive it (tokio today, io_uring later) with no rewrite.

## Design

New crate `crates/blaze-http`, in two layers.

**Core** (`blaze_http::Client`): a synchronous state machine. It owns no socket, runtime,
thread, timer or clock. Every core module is marked `#![blaze::sans_io]` (lint
`blaze::sans_io_core`). It does:

- HTTP/1.1 request writing (`h1::write_request`). It writes `host` (unless the caller sets
  `Host`), `user-agent`, `accept-encoding: gzip` when gzip is asked for, then the caller's
  headers, then `content-length`. A caller header that is `Content-Length`,
  `Transfer-Encoding` or `Connection`, or is not a valid name or value, is refused
  (`Kind::Header`).
- Answer parsing with `httparse` (`h1::parse_head`): a 64 KiB head limit and 100 headers.
  Bodies are framed by `Content-Length`, `chunked` (a byte-wise state machine with extensions
  and trailers) or read to close. 1xx answers are skipped; 101 is refused. 204, 304 and answers
  to HEAD have no body. Two `Content-Length`s that disagree are refused. Header names come out
  lowercased; a value that is not visible ASCII is left out of the headers (the old client did
  the same with values that were not UTF-8).
- Keep-alive and reuse: a pool keyed by origin (scheme, host, port), most recently idle first,
  idle cap 64 and expiry 90 s (hyper-util's defaults). An idle connection is closed when it
  expires: its expiry is a deadline, not checked lazily. Idle connections keep a receive
  outstanding, so a server's close is seen at once. A request whose pooled connection closes
  before any byte of the answer comes is sent once more on a new connection, but only for
  idempotent methods.
- Redirects: at most 10 (`Config::redirects`), for any 3xx except 304. A redirect off https
  with `https_only` is `Kind::Downgraded`. The redirect's body is drained (at most 64 KiB,
  within the timeout) and its connection goes back to the pool. When that body is already
  there, the next hop to the same origin reuses the connection.
- gzip with `flate2` (`rust_backend`), decoded as the body comes, with CRC and length checked
  at the end. Bytes after the gzip stream are an error.
- Body bounds (`Request::most`): a `Content-Length` over the bound is refused before any of
  the body is read. Otherwise the body is held to the bound both as sent and as decoded.
- TLS through rustls's sans-IO `ClientConnection` (ring, tls12, the machine's roots, no ALPN),
  driven by hand: `read_tls`, `process_new_packets`, `reader`, `writer`, `write_tls`.
- Flow control: the core reads at most `WINDOW` (256 KiB) ahead of the asker
  (`Client::consumed`). While the window is full the stall deadline is off: time the asker
  holds the body is not counted against the server.
- Timeouts are deadlines on the injected clock. The head has a deadline from the start of each
  hop (connect, TLS and head as one wait, as before). Each piece of the body has a deadline
  from the last piece. The drain of a redirect has one, and so does an idle connection's
  expiry.
- `BTreeMap`s for connections and exchanges, so failures at the same deadline come out in the
  same order every run.

**Driver** (`blaze_http::tokio_driver`, feature `tokio`): one task owns the `Client` and loops:

1. Advance the clock.
2. Do the `Io` the core asks for. Each connect, send and receive is a small task that posts
   its completion back. Socket halves are lent to the task and returned with the completion.
3. Hand the events to their askers.
4. Wait on one inbox until the next message or `next_deadline` (`timeout_at`).

`Driver::ask(Request) -> Answer`, and `Answer::next().await -> Option<Event>`. Taking the next
event reports the last body piece as consumed. Dropping an `Answer` before its end cancels the
request, which closes its connection.

**Port**: blaze-registry's `HyperHttp` keeps its API (`new`, `shared`, `send`, `get_bounded`,
and `Http`'s `get`, `stream`, `get_async`, `sleep`). Underneath it is a runtime (2 workers), the
core with `roots::native()` TLS, and the tokio driver. `fetch` maps the events to `Response`,
and `failed()` maps `blaze_http::Error` to registry errors:

- `NotHttps`, `Downgraded` and `TooBig` become `Refused`.
- `NoAnswer`, `Stalled`, `Io` and `Gzip` become `Transport` (retried).
- The other kinds become plain errors naming the redacted address.

`Io` messages read `"{step} {host}:{port} for {redacted url}: {why}"`, scrubbed. Streamed
requests (`Http::stream`):

- They ask without gzip.
- A 200 with a non-identity `Content-Encoding` is refused.
- A non-200 body is capped at 64 KiB, then the answer is dropped.
- An unasked-for gzip error body is still decoded for its message, as before.

The `unsafe` `Io` adapter from the first pass is gone. The crate has no `unsafe`.

## The core/driver contract

This is the io_uring seam. It is written out in full in `crates/blaze-http/src/lib.rs`, "The
driver contract":

1. **Clock.** Call `advance_clock(since driver start)` from a monotonic clock before handing
   the core anything, and whenever `next_deadline()` passes.
2. **Asks.** Call `request(id, Request)` with an id the driver picks, unique among requests in
   flight; a reused id is `Err(InUse)`, which changes nothing. Call `consumed(id, bytes)` as
   the asker takes each `Event::Body`, and `cancel(id)` when the asker goes away.
3. **IO.** After every call, drain `next_io()`. Completions are a completion model, the same
   shape io_uring has:
   - `Connect { conn, host, port }`: resolve and open TCP, then `Connected` or
     `ConnectFailed { why }`.
   - `Send { conn, bytes }`: write all of the bytes, then `Sent` or `Broken`.
   - `Recv { conn }`: read any amount, then `Received { bytes }` (empty means EOF) or `Broken`.
   - `Close { conn }`: drop the connection; no completion is expected.

   There is at most one send and one receive in flight per connection. Connection ids are
   never reused, and a completion for an unknown or closed connection is ignored.
4. **Events.** Drain `next_event()`: `Head` once, then `Body` pieces, then `Done`; or `Failed`
   at any point.
5. **Waits.** Sleep until the next completion, ask or deadline. Every request ends by a
   deadline.

## Files changed

- `crates/blaze-http/` (new):
  - `Cargo.toml`: deps flate2 (`rust_backend`), http, httparse, rustls, rustls-native-certs,
    url, and tokio (optional, feature `tokio`); dev-dep rcgen.
  - `src/lib.rs`: crate docs and the driver contract.
  - `src/client.rs`: the core.
  - `src/h1.rs`: request writing, head parsing, body framing and chunked decoding.
  - `src/pool.rs`: the idle pool.
  - `src/tls.rs`: the rustls sans-IO session.
  - `src/gzip.rs`: streaming gunzip.
  - `src/roots.rs`: building the TLS config. This is not part of the core: it reads the
    machine's certificates.
  - `src/tokio_driver.rs`: the tokio driver.
  - `src/tests.rs`: the deterministic core tests.
- `Cargo.toml` (root): removed `hyper`, `http-body-util` and `tokio-rustls` (with its comment);
  added `httparse = "1.10.1"` and `blaze-http = { path = "crates/blaze-http", default-features = false }`.
- `crates/blaze-registry/Cargo.toml`: removed hyper, http-body-util, rustls, rustls-native-certs
  and tokio-rustls. Added optional `blaze-http` (features `["tokio"]`). tokio now has features
  `rt-multi-thread`, `sync` and `time` (`net` moved into blaze-http). Feature
  `net = ["dep:blaze-http", "dep:tokio"]`. The dev-dep tokio is unchanged.
- `crates/blaze-registry/src/http/client.rs`: rewritten onto blaze-http (433 lines, was 752).
- `crates/blaze-registry/src/http.rs`: module doc only.
- `docs/v2/plan/design/no-tracing.md`: section 6.1, the section 6 table row, and the T1 lane
  row now describe this design.
- `HANDOFF-http1-client.md`: this file.

## Tests

`crates/blaze-http/src/tests.rs` drives the core with scripted bytes and a fake clock. It has
no socket and no runtime.

- `a_chunked_body_is_read_whole_however_it_is_cut`: the answer is fed at every cut size from 1
  to its whole length. A non-hex chunk size fails as `Io { step: Body, host, port }`.
- `a_kept_alive_connection_is_reused_until_it_idles_out`: checks the exact bytes of the request
  head, that a second request to the same origin sends no Connect, that another origin gets
  its own connection, and that the idle Close comes exactly at `IDLE_FOR` and not 1 ms before.
- `a_request_its_pooled_connection_closed_under_is_asked_again`: the retry. Also checks that
  `Connection: close` keeps a connection out of the pool.
- `a_redirect_is_followed_and_its_connection_kept`: a cross-origin redirect. The head reports
  the final URL, and the first connection is drained and reused.
- `a_request_past_its_redirects_fails`: `Kind::Redirects { most: 1 }`, with the URL being the
  hop that failed.
- `an_https_only_redirect_to_plain_http_is_refused`: the old registry cases, ported onto
  `redirect_to`.
- `a_request_with_no_answer_fails_at_its_deadline`: `NoAnswer` exactly at the timeout, and
  `Stalled` measured from the last body byte.
- `a_malformed_answer_fails_naming_the_host_and_step`: the message starts with
  "reading the answer's head from example.test:80: not an HTTP/1 answer" and names no path.
  The request is not retried.
- `gzip_is_decoded_where_asked_and_bodies_are_held_to_their_bound`: gzip decoded, gzip passed
  through raw when not asked for, `TooBig` once decoded, and `TooBig` from `Content-Length`.
- `a_slow_asker_holds_the_transfer_back`: no Recv past `WINDOW`, and no stall while held.
- `tls_is_spoken_through_rustls_sans_io`: an in-memory `rustls::ServerConnection` with an
  rcgen ECDSA P-256 cert for `localhost` as the client's only root. It checks a full exchange,
  and that a request to `other.test` against that cert fails as `Io { step: Tls }` mentioning
  "certificate".

blaze-registry keeps three tests:

- `a_bounded_get_stops_at_its_limit_and_refuses_plain_http`.
- `a_stalled_transfer_says_no_credential`.
- `requests_reuse_a_pooled_connection_and_a_refused_one_says_where`: the one in-process tokio
  server test, now testing the driver.

`an_https_only_redirect_to_plain_http_is_refused` there now tests the `Downgraded` to
`Refused::Downgraded` mapping with redaction; the redirect rule itself is tested in blaze-http.

## Expected Cargo.lock changes

Measured against the lock at this branch's base (the first pass never touched it).

- **Remove packages**: `hyper 1.12.0`, `hyper-util 0.1.21`, `hyper-rustls 0.27.10`,
  `tokio-rustls 0.26.6`, `http-body`, `http-body-util`, `want`, `try-lock`, `atomic-waker` and
  `tower-service`. Each has only those packages as users in the lock.
- **Keep**:
  - `httparse`: now used by blaze-http.
  - `futures-channel`: futures, futures-util.
  - `socket2`, `pin-project-lite`, `bytes`, `itoa`, `smallvec`, `http`, `futures-util` and
    `futures-core`: all have other users.
  - `tracing`: it stays in the lock until the other lanes land, but hyper-util is no longer
    among its users.
- **Add package** `blaze-http 0.0.0` with dependencies `flate2`, `http`, `httparse`, `rcgen`,
  `rustls`, `rustls-native-certs`, `tokio` and `url`. rcgen 0.14.10 is already in the lock,
  from blaze-explore and wtransport.
- **`blaze-registry` dependencies**: drop `http-body-util`, `hyper`, `hyper-rustls` and
  `hyper-util`; add `blaze-http`.
- No version of anything else changes.
- `tools/nix/units/daemon.json` (and `life.json` if it lists them) is generated from the lock
  and still names hyper, hyper-rustls and hyper-util. Regenerate it as CI's `unit_graph.py`
  writes it.

## Risks

- **No spans on waits.** blaze-trace is not in the graph yet, so the driver's waits (connect,
  send, receive, the asker's next event, the loop's sleep) open no span. This is noted in
  `lib.rs` and `tokio_driver.rs`. Every wait is bounded by a core deadline. When blaze-trace
  lands, the driver should open one span per wait, named by origin and request.
- **rustls reads the wall clock.** The certificate time check uses rustls's `TimeProvider`.
  This is the one clock read under the core, and it is documented. `sans_io_core` should not
  flag it, because the call goes through `Arc<dyn TimeProvider>`; check that.
- **Lenient close-delimited bodies over TLS.** An EOF without `close_notify` on a
  close-delimited body (no length, not chunked) is accepted as the end, as most clients do. A
  truncation attack on such a body is not caught. Bodies with a length or chunked framing are
  checked.
- **Memory per piece under gzip.** One received piece of up to 64 KiB is decoded whole before
  the decoded bound is checked, so a bomb can cost about 1000x one piece (around 64 MiB)
  before it is refused. It is never unbounded.
- **gzip output lags.** flate2's writer is never flushed (its flush unwraps, which would panic
  the driver), so decoded output lags by up to 32 KiB and the rest comes at the end of the
  body.
- **Narrower retry.** Only idempotent methods are retried on a pooled connection closed before
  the answer. hyper-util retried any request it had not written yet.
- **No Happy Eyeballs.** Connecting uses `TcpStream::connect((host, port))`, trying addresses in
  order, as before. There is still no cap on live connections, only `IN_FLIGHT` (64) for
  `get_async`.
- **A driver panic ends every request.** Askers get `None`, which the registry maps to an error
  naming the URL ("blaze-http's driver ended..."). The core has no `unwrap` on input paths, but
  this is unverified without a build.
- **Unverified borrow checking.** The core's `advance` and `answer` hold `&mut` borrows of one
  field (`conns`) while touching others (`exchanges`, `events`, `now`, `config`). This should
  pass disjoint-field borrow checking, but it is the most likely place for compile errors.
  `answer` now calls `advance(conn)` recursively to drain a redirect before the next hop.
- **Behaviour changes.**
  - Redirect bodies are drained instead of the connection being closed.
  - Some error wording changed: "more than 10 redirects" now names the last hop, not the
    first; "redirects nowhere" now gives the status; URL errors are worded differently.
  - Header values with non-ASCII bytes are now left out where UTF-8 ones were kept.
- **Duplicates (one-concept-one-home).** blaze-global has its own sans-IO `tls.rs`/`http.rs`,
  and blaze-ci's `github.rs` has an HTTP client and `tls()`. Both should move onto blaze-http
  later. The name `HyperHttp` is now misleading; rename it later with a `.clippy.toml` ban and
  `blaze check --replace`.
- **Channels.** The tokio driver uses `tokio::sync::mpsc` (unbounded). It is not banned by
  `.clippy.toml`, but standard-structures prefers crossbeam for thread handoffs. An async inbox
  is needed here.

## What to check

1. `blaze check -p blaze-http` with and without `--features tokio`, and `blaze test -p
   blaze-http`. The TLS test needs rcgen's `ring` feature only.
2. `blaze check -p blaze-registry` with default features (`net`) and with
   `--no-default-features` (seed profile: no blaze-http, no tokio), then `blaze test -p
   blaze-registry`.
3. Lints:
   - `blaze::sans_io_core` must be clean on `client`, `h1`, `pool`, `tls` and `gzip`. Watch
     for anything that walks into rustls and reaches `SystemTime::now` without going through a
     `dyn`.
   - `// alloc:` coverage.
   - Whether workspace lints object to `kind @ (...)` or `_ =` patterns.
4. Regenerate Cargo.lock and confirm the diff is exactly the list above. Regenerate
   `tools/nix/units/daemon.json`.
5. A real fetch over HTTPS: a crates.io sparse index read plus a `.crate` download through
   blaze. Also a GitHub release download (a redirect to signed S3, with `https_only`).
6. Rule 7 (verify-on-real-projects): a cold build of a real project that fetches many crates,
   to exercise `get_async` with 64 in flight and the pool.
