# rustls

- Upstream: https://github.com/rustls/rustls, crates.io `rustls` 0.23.46 (sha256
  48e13bd8c0e9365c43cfa5c9e8f9ad49d3c8444926c9aac819e0e4dc503c8fdf, the checksum `Cargo.lock`
  had for it).
- Licence: Apache-2.0 OR ISC OR MIT (LICENSE-APACHE, LICENSE-ISC, LICENSE-MIT).
- Left out: `Cargo.lock`, `.cargo_vcs_info.json`, `.clippy.toml`. `Cargo.toml` is the published
  one, unchanged.
- Excluded from the workspace (root `Cargo.toml`), like `vendor/quinn-proto`: its tests and
  benches need dev-dependencies blaze has no use for.

## Why

A TLS 1.3 session ticket cannot leave a rustls 0.23 process. `Tls13ClientSessionValue` has
private fields, a crate-private constructor and no encoding, and it holds weak references to the
config's verifier and client credentials that `compatible_config` compares by pointer, so even a
ticket kept in memory resumes only under the very `ClientConfig` that received it.

blaze's remote cache is read by a process that answers and exits (docs/v2/plan/design/http3-sans-io.md,
"Requests and the remote cache"), so every command opens its QUIC connection from cold. Without
a ticket from an earlier process that is one round trip before the first request leaves; with
one, the first requests leave in the first flight as 0-RTT. The ticket has to cross processes,
as bytes the driver keeps on disk, and rustls has no way to do that; this is the missing piece
(rule build-the-missing-piece), built where the fields are.

## Changes

All additive; no existing behaviour changes. Each new item's doc starts `blaze:`.

1. `src/msgs/persist.rs`: `Tls13ClientSessionValue::to_bytes` writes, in rustls's own codec, a
   layout byte (`TICKET_BYTES_LAYOUT`, 1), the cipher suite, `age_add`, `max_early_data_size`,
   the ticket, the resumption secret, the issue time (`epoch`), the lifetime, the server's
   certificate chain and the QUIC transport parameters. `from_bytes` (crate-private) reads them
   back, refusing another layout byte, a suite the config lacks, and trailing bytes, caps the
   lifetime at `MAX_TICKET_LIFETIME` as `ClientSessionCommon::new` does, and binds the value to
   the verifier and client credentials it is given. `issued` and `lifetime_secs` say when the
   ticket was issued and for how long it may be used, for a store that bounds tickets by age.
2. `src/client/client_conn.rs`: `ClientConfig::tls13_ticket_from_bytes` calls `from_bytes`
   with the config's own cipher suites, verifier and client credentials, so a ticket read back
   is `compatible_config` with exactly that config.

What a resumption does with the ticket (its age check, whether the server takes early data, the
transport parameters QUIC validates against the 1-RTT ones) is upstream's, unchanged.

## Removal

When upstream rustls can encode and decode a client's TLS 1.3 tickets itself, move blaze-http's
store (`crates/blaze-http/src/h3/config.rs`, `Tickets`) to that API and delete this copy and its
`[patch]` line.
