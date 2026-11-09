# Handoff: HTTP/3 and WebTransport, sans-IO (docs/v2/plan/design/http3-sans-io.md)

Branch `nt-http3-webtransport`, worktree `/Volumes/Projects/blaze-wt-nt-http3`. Nothing here has
been built, checked or run: the lane's brief forbade every build, test, cargo, rustc, blaze and VM.
The first `blaze check -p blaze-http -p blaze-explore` is the first compile.

## Design

- **Spec**: docs/v2/plan/design/http3-sans-io.md (`plan`, 2026-11-04). It covers what blaze uses
  QUIC for, the contracts, what the browser requires, the designs that lost, cost, migration and
  proof.
- **QUIC core**: quinn-proto, vendored without tracing (vendor/quinn-proto). It stays as it is.
- **HTTP/3 + WebTransport core**: `blaze_http::h3::Endpoint`, in crates/blaze-http/src/h3/.
  - It is a synchronous state machine on an injected clock: `advance_clock`, `received`,
    `next_transmit`, `next_event`, `next_deadline`.
  - One `Endpoint` serves, connects, or both. It holds quinn-proto's `Endpoint` and every
    connection, and on each connection the HTTP/3 that WebTransport needs:
    - control streams and SETTINGS, the same six wtransport 0.7.2 sent: `0x01=0`, `0x07=0`,
      `0x08=1`, `0x33=1`, `0x2b603742=1`, `0xc671706a=1`;
    - QPACK, static table only (Huffman decoded, never encoded; a dynamic-table reference is
      refused, since none was advertised);
    - extended CONNECT; a CONNECT that arrives before the peer's SETTINGS is held until they
      come, or fails at `SETTINGS_WAIT` = 10 s;
    - draft-02 (`sec-webtransport-http3-draft02: 1` gets `sec-webtransport-http3-draft: draft02`
      in the answer) and draft-07;
    - WebTransport bidi streams (signal `0x41`) and uni streams (type `0x54`);
    - datagrams (quarter stream ID);
    - the CLOSE_WEBTRANSPORT_SESSION capsule and DRAIN;
    - WebTransport-to-HTTP/3 error code mapping.
  - Streams that arrive before their session opens are held, at most `MOST_HELD` = 16. Past that
    a stream is refused with `WEBTRANSPORT_BUFFERED_STREAM_REJECTED`. When a session ends, its
    streams are reset with `WEBTRANSPORT_SESSION_GONE`.
  - Flow control: no stream is read more than `WINDOW` (256 KiB) ahead of its asker
    (`consumed`). Sends are queued in the core, and `StreamSent` is the back-pressure signal.
  - Closing: `close(session)` sends the capsule and FIN, then closes the connection with
    `H3_NO_ERROR` once the FIN is acknowledged, or after `LINGER` (1 s).
  - Errors are `h3::Error { peer, step, why }`, displayed as "the WebTransport CONNECT with
    127.0.0.1:1234: ...". How a session ended is `h3::Closed`: `Refused { status }`,
    `ByPeer { code, reason }`, `Local { code, reason }` or `Failed(Error)`.
- **Client-side pinning** (config.rs): a rustls `ServerCertVerifier` that applies
  `serverCertificateHashes`' rules (`check_pinned`): the SHA-256 is pinned, the key is P-256,
  validity is at most 14 days, and now is inside it. A tiny DER reader gets the validity and the
  key OID. TLS 1.3 only, ALPN `h3`, ring.
- **tokio driver** (`h3::tokio_driver`, feature `tokio`):
  - One task owns the core, a `tokio::net::UdpSocket` and one sleep, and `select!`s over the
    socket, the inbox of asks and the next deadline.
  - Handles post asks to that inbox: `Driver` (`spawn`, `local_addr`, `connect`, `set_identity`,
    `close`), `Request` (`path`, `origin`, ..., `accept`, `refuse`), `Session` (`accept_bi`,
    `accept_uni`, `open_bi`, `open_uni`, `send_datagram`, `receive_datagram`, `close`,
    `closed`), `SendStream` (`write_all`, `finish`, `reset`, `stopped`) and `RecvStream` (`read`,
    `stop`).
  - What dropping a handle does:
    - a `Request` dropped unanswered is refused with 500;
    - a `SendStream` dropped is finished;
    - a `RecvStream` dropped is stopped with code 0;
    - when the last `Session` clone drops, the session closes with code 0.
  - Datagrams beyond 256 held per session are dropped.
- **Ports.** wtransport is replaced by this stack in blaze-explore's server.rs, cert.rs and
  probe. The behavior and public surface stay the same (`start`, `Config`, `Explorer`, the
  consts, the 403/404 refusals, `CLOSE_BAD_TOKEN`, `CLOSE_STOPPED`), with these changes:
  - `StartError::Config` now carries `h3::Error`;
  - `CertError::Take` now carries `h3::Error`;
  - `Cert::identity()` returns `h3::Identity`, validated by building the rustls server config;
  - rotation goes through `Driver::set_identity`, which keeps the same socket and port;
  - on stop, the explorer closes every session with code 0 "blaze explore stopped", bounded by
    `SHUTDOWN`.
  - observe.rs needed no code change: it uses only `blaze_explore::{start, Config, Explorer,
    History, frame::ipc}`. Only one comment changed.

## Files

New:
- `crates/blaze-http/src/h3/mod.rs`: docs, the driver contract, `Error`, `Step`, re-exports.
- `crates/blaze-http/src/h3/endpoint.rs`: the core.
- `crates/blaze-http/src/h3/frame.rs`: frames, SETTINGS, capsules, codes.
- `crates/blaze-http/src/h3/qpack.rs` and `huffman.rs`: QPACK and RFC 7541's Huffman decoder.
- `crates/blaze-http/src/h3/varint.rs`.
- `crates/blaze-http/src/h3/config.rs`: identity, transport, rustls and quinn configs, pin
  verifier.
- `crates/blaze-http/src/h3/tokio_driver.rs`.
- `crates/blaze-http/src/h3/tests.rs`.
- `docs/v2/plan/design/http3-sans-io.md`.

Changed:
- `Cargo.toml`:
  - dropped `vendor/quinn` and `vendor/wtransport` from `exclude`;
  - dropped the `[patch.crates-io]` entries for `quinn` and `wtransport` (only `quinn-proto` and
    pattern_analysis remain);
  - dropped the `wtransport` workspace dep;
  - added `quinn-proto = { version = "0.11.19", default-features = false, features =
    ["rustls-ring"] }` and `bytes = "1.11.1"`.
- `crates/blaze-http/Cargo.toml`: added `blaze-sha256`, `bytes` and `quinn-proto`, plus tokio
  `macros`.
- `crates/blaze-http/src/lib.rs`: `pub mod h3;` and the docs.
- `crates/blaze-explore/Cargo.toml`: `wtransport` replaced by `blaze-http` (features
  `["tokio"]`), plus tokio `net`.
- `crates/blaze-explore/src/server.rs`, `cert.rs` and `src/bin/blaze-explore-probe.rs`: ported.
  The server tests now use the h3 client driver.
- `crates/blaze-cargo/src/serve/observe.rs` and `crates/blaze-cargo/Cargo.toml`: comments only.
- `tools/vendor/sync.py`: `Crate` is `quinn-proto | ra-ap-rustc_pattern_analysis`; docstring
  example updated.
- `docs/v2/plan/design/explore.md` and `docs/v2/plan/design/no-tracing.md`: the rows and
  notes that named wtransport or quinn.

Deleted:
- `vendor/quinn/` and `vendor/wtransport/`, including their patch series.

## Tests written

**`crates/blaze-http/src/h3/tests.rs`.** An in-memory pair on one fake clock: no socket, no
thread, no sleep. Every wait is bounded at `MOST_STEPS`.

| Test | What it checks |
|---|---|
| `settings_are_exchanged_and_the_connect_is_accepted` | Both sides parse each other's SETTINGS. The request's path (with its query), authority, origin, user-agent and peer. Accept leads to `SessionOpened`. The session is stream 0. |
| `a_bidirectional_stream_is_echoed` | Send, finish, `StreamSent`, then data and FIN echoed back. |
| `a_unidirectional_stream_is_echoed_on_one_the_server_opens` | Includes: sending on the peer's uni stream is refused with `Step::Stream`. |
| `a_datagram_crosses_with_its_session` | A datagram arrives tagged with its session. |
| `a_refused_session_ends_with_its_status` | `Closed::Refused { status: 404 }`, its Display text, and that a second answer is refused. |
| `a_certificate_not_pinned_fails_the_handshake_naming_the_server` | `Step::Handshake`, `peer = 127.0.0.1:4433`, and the message prefix. |
| `a_closed_session_tells_the_peer_its_code_and_reason` | `ByPeer { 7, "bye" }` on the client and `Local` on the server; both connections then close cleanly. |
| `a_peer_with_no_settings_fails_at_exactly_the_settings_wait` | Uses a bare quinn-proto client that never sends SETTINGS. Nothing happens at 10 s minus 1 ms. At 10 s the connection closes with `H3_MISSING_SETTINGS`, and the server's `ConnectionClosed` names `Step::Settings` and the client. |
| `a_request_that_is_not_webtransport_is_answered_and_reported` | A `websocket` CONNECT gets `Refused` with `Step::Connect`. |

**Unit tests in the other modules:**
- varint: the RFC 9000 vectors;
- huffman: the RFC 7541 C.4 vectors, prefix-free round trip, bad padding;
- qpack: round trip, dynamic-table refusal, uppercase refusal;
- frame: code mapping both ways, SETTINGS errors, capsule and close framing;
- config: DER time parsing.

**blaze-explore:**
- `server::tests` keeps all four tests, ported to the h3 client. The refusal table now asserts
  the exact status (403 or 404).
- The cert test is renamed `a_certificate_is_p256_der_rustls_and_the_pin_check_take_and_new_each_time`.
  It now also runs `check_pinned` at the validity's start and end, and checks it fails one
  second after.

## Expected Cargo.lock changes (left to the integrator)

- Removed:
  - `quinn 0.11.12`, `quinn-udp 0.5.16`, `wtransport 0.7.2`, `wtransport-proto 0.7.2`;
  - their now-orphaned dependencies, computed by walking the current lock: `httlib-huffman 0.3.4`,
    `octets 0.3.7`, `pem 3.0.6`, `base64 0.22.1` (pem's).
  - `sha2 0.11.1`, `x509-parser`, `time`, `socket2`, `cfg_aliases`, `web-time` and `rustc-hash`
    still have other users in this lock, so they stay unless the regenerated lock says
    otherwise.
- `blaze-explore`: drops `wtransport` and gains `blaze-http`.
- `blaze-http`: gains `blaze-sha256`, `bytes` and `quinn-proto` (path `vendor/quinn-proto`
  through the patch). This lock has no `blaze-http` entry at all yet, because the HTTP/1 lane's
  lock changes are not applied. Regenerate once for both.
- `quinn-proto`: `default-features = false` drops `fastbloom` (from `bloom`) and `log`. It keeps
  `bytes`, `getrandom 0.4.3`, `lru-slab`, `rand`, `rand_pcg`, `ring`, `rustc-hash`, `rustls`,
  `rustls-pki-types`, `slab`, `thiserror`, `tinyvec` and `web-time`. Its `tracing` edge is
  already gone in the vendored copy.
- Regenerate `tools/nix/units/daemon.json` (CI's `unit_graph.py`). It still lists `wtransport`
  under blaze-explore, plus the quinn, quinn-udp, wtransport and wtransport-proto units.
- `crates/blaze-wasm/web/dioxus/Cargo.lock` is a separate workspace and is untouched.

## Risks

1. **Browser interop is unproven.** The settings, headers and framing copy wtransport's and the
   drafts', but no browser has spoken to this code. Specific points:
   - Chrome's draft-02 vs draft-07 negotiation;
   - whether Chrome sends its CONNECT before our SETTINGS arrive (handled by holding the CONNECT,
     but untested against Chrome);
   - QPACK: Chrome may Huffman-encode or use literal forms the decoder has not met;
   - the close capsule;
   - datagram quarter IDs.

   **A real Chrome check against ix.dev is required before landing:** run
   `blaze-explore-probe`, open its link in Chrome (https://ix.dev/explore/probe), and confirm
   the bidi echo, the uni echo, the datagram echo, and the printed Origin and User-Agent. Then
   open `blaze explore` end to end (hello, views, the live stream, a certificate rotation if
   practical). Safari and Firefox: check what each does; the old stack's behavior is the bar.
2. **Never compiled.** Likely spots for borrow-checker errors:
   - `match frame::next(&stream.inbuf)` arms that call `self.fail`;
   - `stopped()` and `took()` matching `stream.kind` and then calling `&mut self` methods;
   - `tokio::select!` arms using `self`.

   Lint warnings to expect: `bool_param` was avoided via `Direction` and `Opener`, but
   `wildcard_arm`, `implicit_allocator` (every global allocation has an `// alloc:` line) and
   `struct_update` deserve a look.
3. **quinn-proto API assumptions.**
   - `recv_stream()` and `send_stream()` assert direction; the core guards each call with
     `sends` and `receives`.
   - `Chunks` must be finalized; it is.
   - `StreamEvent::Finished` is used as "our FIN acknowledged".
   - `ConnectionLost` is not emitted on a local `close()`; if it is, it is treated as clean
     unless the core failed the connection itself.
4. **Inputs the core does not take from its caller** (documented in `h3` and the spec):
   - quinn-proto's randomness (`rand`);
   - rustls's handshake randomness and its wall-clock read in the client verifier.

   What the core decides is deterministic; the bytes on the wire are not.
5. **Behavior differences from wtransport:**
   - On stop, sessions get a CLOSE_WEBTRANSPORT_SESSION (code 0) and a FIN, then the connection
     closes. wtransport closed the QUIC connection with code 0 immediately.
   - A CONNECT that is not WebTransport is answered 400 or 501 and reported as `Event::Refused`.
     The driver drops that event; nothing logs it.
   - `Session` drop closes the session, as wtransport's `Connection` drop did.
6. **Unbounded edges, each named in a comment:**
   - a stream held for a session that never opens stays held until its connection ends (at most
     16 per connection);
   - the driver's stream data channels are unbounded, but the core's `WINDOW` bounds them;
   - a client keeps one session per connection; a second CONNECT on a server connection is
     served even though the server advertises `WEBTRANSPORT_MAX_SESSIONS = 1`.
7. **HANDOFF-vendor-third-party.md** still describes vendor/quinn and vendor/wtransport; this
   lane removed both copies.

## What to check

1. `blaze check -p blaze-http --features tokio -p blaze-explore`, then
   `blaze test -p blaze-http --features tokio` (the h3 tests) and `blaze test -p blaze-explore`.
   Fix compile errors in the core first; the driver and the ports follow its API.
2. Regenerate `Cargo.lock` and `tools/nix/units/daemon.json`. Confirm that quinn, quinn-udp,
   wtransport and wtransport-proto are gone and that no lockfile ban (tracing) fires.
3. Run `uv run --frozen tools/vendor/sync.py '{"crate": "quinn-proto", "version": "0.11.19",
   "check": true}'`. It must still pass with the narrowed `Crate`.
4. Run `python3 tools/docs.py check` (the new plan doc, and the edited explore.md and
   no-tracing.md).
5. Do the real Chrome probe against ix.dev (Risk 1) before landing. Then run `blaze explore`
   end to end in Chrome.
6. `blaze check` lints on the new files: `sans_io_core` on endpoint.rs, frame.rs and config.rs
   (quinn-proto and rustls are outside the workspace walk, so neither should be a finding), and
   `implicit_allocator`.
