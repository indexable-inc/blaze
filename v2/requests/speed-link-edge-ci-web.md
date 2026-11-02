# Requests of speed-link-edge-ci-web (`crates/blaze-web/`)

## 1. `docs/web.md`, "The wire", item 2: a frame of many records (coordinator)

What: the websocket's record messages now hold every record the world's thread had at once, as a
CBOR sequence (RFC 8742), not one record per message. The page reads either (`parseFrame` in
`crates/blaze-web/assets/records.js`); a sender of one record per message (the bench's
`screens.py`) still works.

Where: `docs/web.md`, the numbered list under "The wire". Replace item 2

```
2. Then the records of that workspace's last build (without its `Functions`, which the snapshot
   has), and each record of every build from then on as it comes: one CBOR item of
   [`Record`](../crates/blaze-protocol/src/events.rs) per message, in the deterministic
   encoding of the socket, without the socket's length prefix.
```

with

```
2. Then the records of that workspace's last build (without its `Functions`, which the snapshot
   has), and each record of every build from then on as it comes, in frames: a message is a
   CBOR sequence (RFC 8742) of one or more [`Record`](../crates/blaze-protocol/src/events.rs)s,
   each in the deterministic encoding of the socket without its length prefix, as many as the
   daemon had at once. Each is encoded once, whatever the number of pages.
```

and in item 3, "and the first record of a whole object one in place of the record" stays as it
is. In "Where it comes from", the sentence "Until then the daemon keeps no world, only the
records it would be made from: …" stays true; add after it: "They are kept in one deflate
stream per run of a build's records, on the page's thread, so a record costs the daemon one
send on a channel."

Why: `docs-say-what-they-are`; the change is in `crates/blaze-web/src/lib.rs` (`State::flush`).

> forwarded to coordinator: `docs/web.md` is the coordinator's, not among the paths `requests` applies.

## 2. `blaze-protocol`: encode into a buffer the caller has (speed-link-edge-wire)

What: in `crates/blaze-protocol/src/cbor/ser.rs`, beside `to_vec`, and exported from
`cbor.rs` (`pub use ser::{append, to_vec};`):

```rust
/// Appends the deterministic encoding of `value` to `out`; on an error, `out` is as it was.
pub fn append<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> Result<(), Error> {
    let len = out.len();
    value.serialize(Encoder(out)).inspect_err(|_| out.truncate(len))
}

pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    append(&mut out, value)?;
    Ok(out)
}
```

Why: blaze-web encodes each record once, straight into the buffer it is kept or sent in
(`Program::tell` in `world.rs`, `State::record` in `lib.rs`, `Workspace::keep` in `kept.rs`).
With only `to_vec`, each record costs one more allocation and one more copy of its bytes. Once
`append` exists, those three calls become `blaze_protocol::cbor::append(&mut buffer, record)`
(the kept one writes its length prefix after, into a reserved 4 bytes).

> forwarded to speed: speed-link-edge-wire's.

> applied: `cbor::append` and a reusable `cbor::Writer` are in `crates/blaze-protocol/src/cbor/ser.rs`,
> exported from `cbor.rs` (speed-link-edge-wire). By speed-link-edge, `blaze-web` uses it in
> `Program::tell` (straight into `told`), `State::record` (straight into `pending`) and
> `Workspace::keep` (one reused `Workspace::item` buffer).
