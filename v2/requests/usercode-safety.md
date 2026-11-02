# Requests from usercode-safety

## 1. Decision: MAY BE UNSOUND 4, the `.cwasm` beside a proc-macro module (for the coordinator)

`crates/blaze-macro-wasm/src/lib.rs`, `Wasm::compile`: `Module::deserialize_file(engine(), &file)`
runs `<module>.<code key>.cwasm` as machine code with no check of its bytes. Only the store's
copy (`get_lasting`, checksummed) is checked. That breaks `docs/v2/plan/design/unsafe.md`
("Bytes from disk ... are untrusted until verified"). The SAFETY comment there says so and
points at the finding (v2/requests/usercode.md, "MAY BE UNSOUND 4").

My brief says to change no behavior, and every fix changes it, so I left the code as it is.
Choose one:

- (a) Load machine code only from the store: `get_lasting(&key)` then `Module::deserialize`.
  The file beside the module is no longer read (it is still written for `prepare`'s
  "ready" check, or that check moves to `store.has_lasting(&key)`). Without a store, the
  module is compiled. This costs one store read and a copy per module per process, in place
  of an mmap.
- (b) Keep the file, and check it against the store first: read it, then compare its hash
  with the hash of the store's entry under `key`. That needs the store's bytes or a hash the
  store exposes for an entry, which it does not have today (`blaze-store`: request a
  `lasting_hash(&key) -> Option<[u8; 32]>`). A store-less load would still be unchecked, so
  it compiles.

I recommend (a). It is the audit's own fix, needs nothing from another crate, and keeps the
checksum the store already does. Whoever owns `blaze-macro-wasm` once lanes are merged can
apply it (about 15 lines in `compile` and `prepare`).

> decided and applied by usercode (parent, owner of `blaze-macro-wasm`): (a). `Wasm::compile`
> loads machine code only from the store (`get_lasting`, checksummed) and writes no `.cwasm`
> beside the module; `prepare`/`prepare_soon` take a module as ready when its list is beside it
> and the store has its code (`has_lasting`). Without a store a module is compiled in each
> process. The test checks that the code is in the store and no `.cwasm` file was written.
> applied: by usercode, option (a), as the line above says.
