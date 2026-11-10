---
status: plan
summary: blaze's QUIC and WebTransport stack becomes sans-IO cores with thin drivers, on rustls. quinn-proto stays as the QUIC core; a new HTTP/3 + WebTransport core in blaze-http (static-only QPACK, SETTINGS, extended CONNECT, WebTransport streams and datagrams) drives it; one tokio driver owns the UDP socket and the timers behind the same driver contract as HTTP/1. quinn, quinn-udp, wtransport and wtransport-proto leave the graph. Covers what blaze uses QUIC for, the contracts, the browser's requirements, the losing designs, migration and tests.
date: 2026-11-04
---
# HTTP/3 and WebTransport, sans-IO

The owner has decided: blaze's QUIC and WebTransport stack follows rule 14 (sans-io-cores), as
the HTTP/1.1 client already does (`crates/blaze-http`). Protocol
logic lives in synchronous state machines that take bytes, completions and the time, and hand
back bytes, events and deadlines. One thin driver per IO backend does the IO.

## 1. What blaze uses QUIC for

One thing: `blaze explore`. The daemon serves an HTTP/3 WebTransport endpoint on `127.0.0.1`,
and the page `https://ix.dev/explore` connects back to it from the browser
(docs/v2/plan/design/explore.md). The users are:

- `crates/blaze-explore/src/server.rs`: the explorer's server. It binds UDP on `127.0.0.1:0`,
  refuses a CONNECT whose path is not `/explore/v1` (404) or whose Origin is not allowed (403),
  answers one JSON request per bidirectional stream, opens one unidirectional stream for the
  live feed, closes a session with code 1 after `bad_token`, swaps its certificate every 12
  days on the same socket, and closes every session with code 0 when it stops.
- `crates/blaze-explore/src/cert.rs`: the self-signed ECDSA P-256 certificate, valid 13 days,
  that the page pins by its SHA-256 (`serverCertificateHashes`).
- `crates/blaze-explore/src/bin/blaze-explore-probe.rs`: an echo server (bidirectional,
  unidirectional, datagrams) with the same certificate, for checking a browser by hand.
- `crates/blaze-cargo/src/serve/observe.rs`: starts and stops the explorer through
  `blaze_explore::{start, Config, Explorer}`; it never names the transport.

So blaze is the server. The only client is the explorer's own test, which plays the browser
(and, like the browser, checks the certificate by hash, P-256 and a validity of at most 14
days). No other part of blaze speaks QUIC, and none should until it needs to.

## 2. The decision

- **quinn-proto stays the QUIC core.** It is already sans-IO: `Endpoint::handle` takes a
  datagram, `Connection::poll_transmit` hands one back, `poll_timeout` is a deadline, and TLS is
  rustls's (`crypto::rustls`, on ring). It stays vendored without `tracing`
  (`vendor/quinn-proto`, `tools/vendor/sync.py`).
- **A new HTTP/3 + WebTransport core in blaze-http** (`blaze_http::h3`), on top of quinn-proto:
  - QPACK with the static table only. Both sides advertise `SETTINGS_QPACK_MAX_TABLE_CAPACITY
    = 0` and `SETTINGS_QPACK_BLOCKED_STREAMS = 0`, so no peer may reference a dynamic table. A
    field section that does is refused (`QPACK_DECOMPRESSION_FAILED`), never guessed at.
    Huffman strings are decoded (Chrome sends them); ours are written without Huffman.
  - SETTINGS, on the control stream, in a fixed order (section 4).
  - The extended CONNECT handshake (RFC 9220): `:method CONNECT`, `:protocol webtransport`,
    `:scheme https`, `:authority`, `:path`. The server's app sees each request and accepts it
    (200) or refuses it with a status.
  - WebTransport streams, both directions, both kinds: bidirectional streams start with the
    signal `0x41` and the session ID, unidirectional ones with type `0x54` and the session ID.
  - Datagrams (RFC 9297): the quarter stream ID, then the payload.
  - Closing a session with `CLOSE_WEBTRANSPORT_SESSION` (capsule `0x2843`, code and reason) and
    FIN; reading the peer's capsules across DATA frames; `DRAIN_WEBTRANSPORT_SESSION` and
    unknown capsules skipped.
  - Error codes mapped between WebTransport's 32-bit space and HTTP/3's
    (`0x52e4a40fa8db + n + n / 0x1e`), as the draft says.
- **One tokio driver** (`blaze_http::h3::tokio_driver`, feature `tokio`) owns the UDP socket
  and the one timer, and drives the core. On top of it sits a small async surface (sessions,
  streams, datagrams) that the explorer and the probe use, as `Driver::ask -> Answer` is for
  HTTP/1.
- **quinn, quinn-udp, wtransport and wtransport-proto leave the graph.** Their vendor copies,
  patch series and `[patch]` entries are deleted. `vendor/quinn-proto` is the one copy left,
  and `tools/vendor/sync.py` keeps working for it (and for `ra-ap-rustc_pattern_analysis`).

### The losing alternatives

- **quiche** (Cloudflare): sans-IO and has HTTP/3, but it is built on BoringSSL. That is a
  second TLS stack beside rustls, a C build in the seed's way, and a second set of roots and
  ciphers to audit. Losing.
- **Keeping quinn's driver** (quinn + quinn-udp under wtransport): it ties the protocol to
  tokio (`quinn::Runtime`), owns its own timers and tasks, and hides the state machine behind
  futures, so an io_uring driver would mean a rewrite and no test can replay a transcript.
  Rule 14 says a dependency that ties protocol logic to one runtime is replaced by a core and a
  driver, not wrapped. Losing.
- **h3 + h3-quinn + h3-webtransport**: runtime-bound the same way, and its WebTransport support
  is experimental. Losing.
- **Our own QUIC**: years of work that quinn-proto already did, sans-IO. Losing.

## 3. The contracts

### The core: `blaze_http::h3::Endpoint`

A synchronous state machine. It owns no socket, thread, timer or clock, and its module is
`#![blaze::sans_io]`. One `Endpoint` holds quinn-proto's `Endpoint`, every connection, and each
connection's HTTP/3 and WebTransport state. It can serve (a `ServerConfig`: the certificate and
key, the transport limits), connect (a `ClientConfig`: the pinned hashes), or both.

1. **Clock.** `Endpoint::new(epoch: Instant, ..)` takes the instant the driver started;
   `advance_clock(since: Duration)` moves the core's time. The core never reads a clock: it
   hands quinn-proto `epoch + since`, which is input, and every deadline it returns
   (`next_deadline() -> Option<Duration>`) is on the same scale. A test passes any `epoch` and
   steps `since` by hand.
2. **Datagrams in.** `received(from: SocketAddr, bytes: &[u8])` for each UDP datagram.
3. **Datagrams out.** After every call, drain `next_transmit() -> Option<Transmit { to, bytes }>`
   and send each with one `send_to`. One datagram per transmit (no GSO yet); a driver may batch
   them (`sendmmsg`, io_uring) without the core knowing.
4. **Asks.** `accept(session)`, `refuse(session, status)`, `connect(to, url, headers)`,
   `open(session, bidi)`, `send(stream, bytes)`, `finish(stream)`, `reset(stream, code)`,
   `stop(stream, code)`, `consumed(stream, bytes)`, `send_datagram(session, bytes)`,
   `close(session, code, reason)`, `set_identity(identity)` and `close_all(code, reason)`. Each
   returns at once; what it causes comes out as transmits and events.
5. **Events.** Drain `next_event()`: `SessionRequest`, `SessionOpened`, `SessionClosed`,
   `StreamOpened`, `StreamData`, `StreamFinished`, `StreamReset`, `StreamStopped`,
   `StreamSent`, `StreamsAvailable`, `Datagram`, `Refused` and `ConnectionClosed`.
6. **Flow.** The core reads a WebTransport stream at most `WINDOW` (256 KiB) ahead of its
   asker (`consumed`), as the HTTP/1 core does; past that QUIC's flow control holds the peer
   back. A send is buffered in the core until quinn-proto takes it, and `StreamSent` says the
   buffer emptied, which is the asker's back-pressure.
7. **Waits.** Sleep until the next datagram, ask or `next_deadline`. Every wait of the core's
   is a deadline: quinn-proto's timers (idle 30 s with a 5 s keep-alive for the explorer), the
   peer's SETTINGS (`SETTINGS_WAIT`, 10 s after the handshake), and the close of a connection
   after its session ended (`LINGER`, 1 s).

Errors (`h3::Error`) name the peer, the step (`Handshake`, `Settings`, `Connect`, `Stream`,
`Datagram`, `Session`, `Config`) and why, in the message: `"the WebTransport CONNECT with 127.0.0.1:51234: an indexed field line names
the dynamic table, ..."`. How a session ended is `h3::Closed`: `Refused { status }`,
`ByPeer { code, reason }`, `Local { code, reason }` or `Failed(Error)`.

### The tokio driver

One task owns the core, the `tokio::net::UdpSocket` and one sleep. It loops: advance the clock,
send every transmit, hand every event to its asker, then wait on the socket, the inbox of asks
and the next deadline, whichever is first. Askers hold handles (`Request`, `Session`,
`SendStream`, `RecvStream`) that post asks to the inbox and receive their events on channels of
their own. Dropping a `RecvStream` stops it, dropping a `SendStream` finishes it, and dropping
a `Request` unanswered refuses it with 500. An io_uring driver implements the same loop against
the same core with no change to it.

The driver's waits open no span yet: blaze-trace is not in the graph
(docs/v2/plan/design/no-tracing.md). Each is bounded by a core deadline or by the asker's own
(the explorer's `REQUEST_WAIT`).

## 4. What the browser requires

Chrome's WebTransport speaks draft-ietf-webtrans-http3 draft-02 and draft-07, choosing by the
server's SETTINGS. Draft-13 and later (`SETTINGS_WT_MAX_SESSIONS = 0x14e9cd29`,
`webtransport-h3`, `RESET_STREAM_AT`, the WT flow-control capsules) are not what shipping Chrome
or Firefox use as of this date, so the core does not send their codepoints: advertising one
would invite a peer to pick a version whose rules the core does not follow. The server sends
exactly what wtransport 0.7.2 sent, which is what the explorer was written against:

| setting | id | value |
|---|---|---|
| `SETTINGS_QPACK_MAX_TABLE_CAPACITY` | `0x01` | 0 |
| `SETTINGS_QPACK_BLOCKED_STREAMS` | `0x07` | 0 |
| `SETTINGS_ENABLE_CONNECT_PROTOCOL` | `0x08` | 1 |
| `SETTINGS_H3_DATAGRAM` | `0x33` | 1 |
| `SETTINGS_ENABLE_WEBTRANSPORT` (draft-02) | `0x2b603742` | 1 |
| `SETTINGS_WEBTRANSPORT_MAX_SESSIONS` (draft-07) | `0xc671706a` | 1 |

Also required, and done:

- ALPN `h3`, TLS 1.3 only, an ECDSA P-256 certificate valid at most 14 days
  (`serverCertificateHashes`), and quinn-proto's `max_datagram_frame_size` transport parameter
  above 0 (quinn-proto sends it while datagrams are on, the default).
- A draft-02 CONNECT carries `sec-webtransport-http3-draft02: 1`; the 200 then carries
  `sec-webtransport-http3-draft: draft02`, which Chrome's draft-02 client checks. A draft-07
  CONNECT has neither.
- The server does not process a CONNECT before the client's SETTINGS arrive (draft-07, 3.1): a
  CONNECT that comes first waits for them, bounded by `SETTINGS_WAIT`.
- A WebTransport stream that arrives before its session's CONNECT is held (at most 16 per
  connection; past that it is refused with `WEBTRANSPORT_BUFFERED_STREAM_REJECTED`), and the
  held ones are reset with `WEBTRANSPORT_SESSION_GONE` if the session is refused.
- GREASE: unknown frame types are skipped, unknown unidirectional stream types are stopped with
  `H3_STREAM_CREATION_ERROR`, unknown settings are ignored, reserved HTTP/2 setting ids are
  `H3_SETTINGS_ERROR`.
- The peer's QPACK encoder and decoder streams are read and their bytes dropped: with a table
  capacity of 0 there is nothing on them the core needs.
- The client's SETTINGS are recorded, not judged: a peer without `SETTINGS_H3_DATAGRAM = 1`
  gets its sessions anyway, and a datagram sent to it is refused with an error naming the
  missing setting. Refusing sessions on a browser's SETTINGS is the interop risk this design
  avoids.

Firefox speaks the same drafts. Safari ships the later `SETTINGS_WT_MAX_SESSIONS`; it is not a
target of `blaze explore` and is not sent (a later change adds it with the draft-13 rules).

The browser check is manual and required before this lands for users: `blaze-explore-probe`
against `https://ix.dev/explore/probe` in Chrome and Firefox, then `blaze explore` end to end
(section 7).

## 5. Cost

Nothing here is hot. One page or two per daemon, a 250 ms live feed of tens of KB a frame, a
few requests a second. The core copies each received stream piece once (quinn-proto's `Bytes`
into the event) and each sent piece once (into quinn-proto's buffer). AES-GCM through rustls is
gigabytes a second; loopback adds tens of microseconds.

## 6. Migration

One change, whole port (rule 23):

1. `crates/blaze-http/src/h3/`: the core (`endpoint.rs`), `qpack.rs`, `huffman.rs`,
   `frame.rs`, `varint.rs`, `config.rs` (rustls and quinn-proto configs, the certificate pin
   verifier, a minimal DER reader for its checks) and `tokio_driver.rs`.
2. blaze-explore's `server.rs`, `cert.rs` and the probe move onto `blaze_http::h3`. Their public
   surface stays (`start`, `Config`, `Explorer`, `StartError`, the constants, `Cert`); the one
   type that changes is `StartError::Config`, which carries `blaze_http::h3::Error` instead of
   wtransport's, and `Cert::identity` returns `blaze_http::h3::Identity`.
3. Root `Cargo.toml`: drop `wtransport` from `[workspace.dependencies]`; drop `quinn` and
   `wtransport` from `[patch.crates-io]` and `exclude`; add `quinn-proto` (no default
   features, `rustls-ring`) and `bytes` to `[workspace.dependencies]`. Delete `vendor/quinn` and
   `vendor/wtransport`. `tools/vendor/sync.py` takes `quinn-proto` and
   `ra-ap-rustc_pattern_analysis` only.
4. The lock loses quinn, quinn-udp, wtransport, wtransport-proto and what only they used; the
   handoff lists it.

## 7. How it is proven

- **Core tests, deterministic** (`crates/blaze-http/src/h3/tests.rs`): a quinn-proto client and
  server `Endpoint` in memory, datagrams moved between them by the test, the clock stepped by
  the test to each `next_deadline`. No socket, no runtime. They cover the SETTINGS exchange (each
  side sees the other's), a CONNECT accepted, a bidirectional stream echoed, a datagram each way,
  a unidirectional stream, a session refused with 404 whose error names the status, a session
  closed with a code and reason the other side reads, and a peer that never sends SETTINGS
  failing at exactly `SETTINGS_WAIT`. The QPACK, Huffman, varint, frame and error-code
  functions have unit tests with RFC vectors. The outcomes are deterministic; the bytes are
  not, because TLS and quinn-proto draw randomness (keys, connection IDs).
- **The explorer's tests** (`crates/blaze-explore/src/server.rs`) keep every case and move onto
  the new client over real UDP on loopback: path and origin refusals, `unauthenticated` before
  `hello`, `hello`, `views` as IPC, `unknown_op`, a live frame's layout, `bad_token`
  closing only its own session, and the dev origin.
- **The browser**, by hand: the probe page in Chrome and Firefox (bidirectional echo,
  unidirectional echo, datagrams), then `blaze explore` with the real page on ix.dev. This is
  the one check no test here can make.
