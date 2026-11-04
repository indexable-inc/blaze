# Handoff: lane T1 `http1-client` (docs/v2/plan/observe/no-tracing.md, 6.1)

Code only: nothing was built, checked or tested (no blaze, cargo or VM ran). Cargo.lock is untouched.

## Files changed

- `Cargo.toml` (root): removed `hyper-rustls` and `hyper-util` from `[workspace.dependencies]`;
  added `tokio-rustls = { version = "0.26.6", default-features = false, features = ["ring", "tls12"] }`.
- `crates/blaze-registry/Cargo.toml`: dropped `hyper-util` and `hyper-rustls`; added optional
  `rustls`, `rustls-native-certs` and `tokio-rustls` (workspace); `tokio` gains `net`; `net`
  feature lists the new deps; new `[dev-dependencies] tokio` with `io-util, net, rt, time` for the test.
- `crates/blaze-registry/src/http/client.rs`: the rewrite (below), plus one new test.
- `docs/v2/plan/observe/no-tracing.md`: the spec, copied in (was staged already).

`git grep hyper_util\|hyper_rustls` over `crates/` is now empty. The public API of `HyperHttp`
(`new`, `shared`, `send`, `get_bounded`, the `Http` impl) is unchanged, so no other crate changes.

Left alone on purpose: `harness/apps/tlsfetch` (its own Cargo.lock, a test project blaze builds,
not part of blaze's graph), the separate lockfiles under `harness/` and `crates/blaze-wasm/web/dioxus`,
`harness/sweep1000.txt`, and `tools/nix/units/daemon.json` (generated from the lock; regenerate
it after the lock changes if that's how it's kept up to date).

## Design

- **Connections**: `Requests::connect` = `tokio::net::TcpStream::connect((host, port))`, then
  for https `tokio_rustls::TlsConnector::connect(ServerName, socket)`, then
  `hyper::client::conn::http1::handshake(Io(stream))`. The connection future is `tokio::spawn`ed
  on the client's runtime (the driver); it ends when the server closes or the sender is dropped.
  `Io<T>` is a small local stand-in for hyper-util's `TokioIo` (tokio `AsyncRead/AsyncWrite` to
  `hyper::rt::Read/Write`), with two `unsafe` blocks that have SAFETY comments (the same code as `TokioIo`).
- **TLS**: same as before. rustls on ring, safe default versions (explicit
  `builder_with_provider(ring)`, which is what `ClientConfig::builder()` picked, since ring is
  the only provider in the graph), roots from `rustls_native_certs::load_native_certs()`
  (unparsable ones skipped, zero roots is an error, as `with_native_roots` did), no client auth,
  **no ALPN** (hyper-rustls's `enable_http1()` without http2 left `alpn_protocols` empty). IPv6
  brackets are stripped for the socket and the SNI name, as `DefaultServerNameResolver` did.
- **Pool** (`Pool<S>`): a synchronous core with no IO and an injected `now: Instant`, generic
  over the sender. Keyed by `Origin { https, host, port }`. `take` returns the connection to that
  origin idle the shortest; on the way it drops any entry idle `>= IDLE_FOR` (90 s, hyper-util's
  default) or not `is_ready()` (driver ended or connection closed). `put` caps idle connections at
  `IN_FLIGHT` (64) and evicts the one idle longest. It sits behind `parking_lot::Mutex`, and the
  lock is never held across an await.
- **Reuse**: after the body has been read to its end (or dropped, or cut short), `release` puts
  the sender back. If it isn't ready yet, `release` awaits `sender.ready()` inline, bounded by the
  client's `timeout`, and drops the connection on an error or on the timeout. A body that was cut
  short or `Connection: close` closes the connection, so it is never pooled. Releasing inline
  rather than from a spawned task makes back-to-back requests reuse the connection every time,
  which the test relies on. Redirects release too: the answer is dropped, then released.
- **Retry of unsent requests**: as hyper-util's `retry_canceled_requests` did. If a reused
  connection gives the request back unsent (`TrySendError::take_message`), it is sent exactly once
  more on a new connection. Every other failure is the request's error. Bounded: at most one more send.
- **Request form**: origin-form URI (`path_and_query` or `/`) plus a `Host` header (port only
  when it isn't the scheme's default), inserted only if the caller didn't set one, as hyper-util did.
- **Timeouts**: unchanged. `self.timeout` covers connect, TLS, handshake and the answer's head as
  one wait (as it covered `client.request` before), and each body frame (`next`). REDIRECTS (10),
  bounds, gzip, redaction and scrubbing are unchanged.
- **Proxy**: none before (the legacy client with this connector asked no proxy), none now.
- **Errors**: connect, TLS and HTTP/1 handshake failures are `Transport` (so they're retried as
  before), worded `"{step} {host}:{port} for {said}: {e}"`, e.g. `connecting to 127.0.0.1:5 for
  http://127.0.0.1:5/a: Connection refused`. Send failures are `asking {said}: {e:#}`. A scheme
  other than http or https, a URL with no host, or a host that isn't a valid TLS name is a plain
  (non-`Transport`) error, so it's no longer retried. Before, hyper-rustls's "unsupported scheme"
  came back as `Transport` and was retried 3 times. All messages go through `scrub`.
- **Test** `requests_reuse_a_pooled_connection_and_a_refused_one_says_where`: an in-process tokio
  keep-alive server counts the connections it accepts. Three `get`s to it give exactly 1
  connection. A port that was bound and then dropped gives a `Transport` error whose message has
  `connecting to 127.0.0.1:{port} for http://127.0.0.1:{port}/a`.

## Expected Cargo.lock changes

- Remove `[[package]]` `hyper-rustls 0.27.10`, `hyper-util 0.1.21` and `tower-service 0.3.3`
  (only those two used it).
- In `blaze-registry`'s `dependencies`: remove `"hyper-rustls"` and `"hyper-util"`; add `"rustls"`,
  `"rustls-native-certs"` and `"tokio-rustls"`.
- No other entry should change. `tokio-rustls 0.26.6` stays as it is (deps `rustls` and `tokio`,
  no `log`, since `logging` is off). tokio's `net` adds nothing (mio and socket2 are already
  listed). futures-channel, futures-util, socket2, httparse and pin-project-lite keep other users.
  `tracing` stays in the lock until the other lanes land, but hyper-util is no longer among its users.

## Risks

1. **Spans**: every-wait-says-what. The waits (connect, the answer's head, `release`'s ready)
   have no span. The crate has no tracing or blaze-trace dependency now, and blaze-trace comes
   in phase 0 of the same plan. Add spans when blaze-trace lands.
2. **Idle connections expire lazily**: there's no reaper task. A stale idle connection is
   dropped the next time the pool is touched, or sooner if the server closes it (the driver
   ends). With no traffic, up to 64 idle sockets can stay open past 90 s until the next request.
3. **No cap on live connections**: the spec says "at most `IN_FLIGHT` connections". `IN_FLIGHT`
   caps idle connections, and the existing semaphore caps `get_async` requests in flight. Sync
   `send`/`get` from many threads can still open more, as with hyper-util. Capping live
   connections would add a wait to sync callers; I left it out.
4. **No Happy Eyeballs**: `TcpStream::connect` tries the resolved addresses one after another.
   HttpConnector raced IPv6 and IPv4 with a 300 ms delay, so a host whose AAAA record black-holes
   could now take up to `timeout` per bad address before falling to IPv4. It's still inside the
   same overall timeout. Watch registry fetches on IPv6-broken networks.
5. **`unsafe` in `Io::poll_read`**: it copies `TokioIo`'s pattern, but it is new unsafe code in
   this crate. A reviewer should check the two SAFETY comments.
6. **Closure argument coercion**: `connect`'s `failed` closure takes `&dyn Display`, and its
   callers pass `&io::Error` and `&hyper::Error`. I expect it to coerce. If the checker objects,
   add an explicit `as &dyn std::fmt::Display` at the three call sites.

## What the main agent should check

- `blaze check -p blaze-registry` (default `net` feature) and the seed or no-`net` profile
  (`offline.rs` is untouched, and the new deps are all optional under `net`).
- `blaze test -p blaze-registry`: the new test, plus the three existing client tests
  (`a_bounded_get_stops_at_its_limit_and_refuses_plain_http`, `a_stalled_transfer_says_no_credential`,
  `an_https_only_redirect_to_plain_http_is_refused`). They serve `Connection: close`, so they exercise
  "closed connections are not pooled".
- Regenerate Cargo.lock and confirm the diff is exactly the list above (that `hyper-util`,
  `hyper-rustls` and `tower-service` are gone).
- `blaze::implicit_allocator` warnings in `client.rs`: the new global allocations carry
  `// alloc:` comments; pre-existing ones in `fetch` were left as they were.
- One real HTTPS fetch (a sparse-index crate download through crates.io) to confirm TLS with the
  native roots and no ALPN still works end to end.
