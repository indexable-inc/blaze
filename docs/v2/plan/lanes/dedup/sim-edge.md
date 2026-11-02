---
status: plan
summary: dedup-sim-edge (2026-10-29): the cross-crate copies tools/similar found in the cargo, store, link, runtime, usercode and tool crates; four shared primitives (tag ranges, prehashed maps, hex, Address as text), what each copy became and its speed of light.
date: 2026-10-29
---
# dedup-sim-edge: copies across the edge crates

Part of the dedup lane. The 240 cross-crate groups `tools/similar` found outside the front end;
the full list, group by group, is in the lane report. Most were covered by other dedup children
(macros, codec, order, scan-cargo). These are the primitives this child picked a home for.

## Tag range of a state machine: `blaze_core::Tags`

A sans-IO machine owns the tags `base..base + 2^32` and numbers its effects in that range,
wrapping inside it, so one loop can drive several machines and send each completion to the one
that owns it. blaze-cas's store and remote client and blaze-global's client and server each had
the same struct. It lives beside `Tag` in blaze-core because every machine already depends on
it. blaze-query's `Io::tag(base)` is the same rule with the base passed per call and the counter
carried across its settle swap: 5 lines, left as is.

Speed of light: one add and one mask to issue, one subtract and one compare to route. That is
what it does; nothing allocates.

## Maps on keys that are hashes: `blaze_ids::prehashed`

A key that is already a hash (a `ContentId`, a 64-bit Fx hash) needs no hashing: its first word
is the hash. `Prehashed` takes it, `ByHash<K, V>` and `ByHashSet<K>` name the map and set.
blaze-cas and blaze-global each had one; blaze-cargo's `cli/print.rs` has one more (requested).
blaze-ids is the home: it defines `ContentId` and sits below both.

Kept: blaze-graph's `KeyHasher` and `KeyBits` (blaze-graph keeps off blaze-ids and blake3), and
blaze-items' `KeyHasher`, which takes a key's last word on purpose.

Speed of light: one 8-byte load per hash, which is what it does.

## Hexadecimal: `blaze_format::hex`

`encode`, `encode_into` (appends, no allocation when there is room), `is_encoding` (compares
without writing), `decode::<N>` (either case, to a fixed array) and `decode_lower::<N>`. Decoding
looks each digit up in a 256-entry table and checks one "bad" flag at the end, so a digit that is
not one, a `+` (which `u8::from_str_radix` took) or a wrong length is refused. Twelve copies
across blaze-ci, blaze-global, blaze-registry, blaze-cargo, blaze-macro-wasm and blaze-premap go
onto it, moved or requested.

Kept: blaze-inproc's (the start hook runs inside the user's program and the crate depends only
on blaze-mem), blaze-live's (no dependencies at all), blaze-graph's panic text, blaze-fuzz's
corpus reader (any length, whitespace skipped) and the test references.

Speed of light: one table lookup per digit each way. No caller is hot: the busiest is
blaze-macro-wasm keying a macro's inputs with 16-byte digests, so it stays scalar. SIMD would pay
only on kilobytes of hex, and nothing has that much.

## `Address` as text

blaze-cas and blaze-global each had a `place(&Address) -> String` for error messages, and
global's lacked the `Inherited` arm. `Address` now has a `Display` in blaze-core, so an error
formats it in place and no `String` is made first.

## Not copies

- cas's and global's `complete`: one pipelined connection with answers matched by key, against a
  pool of HTTP/1.1 connections answered in order. They share the primitives above and nothing more.
- blaze-cache's key writer against blaze-protocol's CBOR encoder: two formats. Protocol's
  `cbor/reference` copy is the oracle its differential tests use, on purpose.
- `Kevent` in blaze-core, blaze-live and the fork server, and `serialize` (`cpuid`) in
  blaze-inproc and blaze-live: each copy lives where it can't take a dependency.
