---
status: plan
summary: blaze v2: the store.
date: 2026-10-29
---
# blaze v2: store

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 8. Store

- One content-addressed store on the VM's disk, keyed by BLAKE3 of exact inputs (source,
  cfg, features, target, toolchain, the layout facts read). A key that misses an input is a bug.
- **Packs**: aligned zero-copy records, mmapped. Hot objects uncompressed; cold objects zstd
  with a dictionary per kind. Large outputs content-defined-chunked, so near-identical binaries
  across views share bytes. Outputs placed with reflink (Linux) or `clonefile` (macOS).
- **Index**: an mmapped open-addressed table on BLAKE3 keys; one or two cache misses a lookup.
- **Verified**: a read checks the content hash; archive and path entries can't escape their
  directory.
- **The user's alone**: the store refuses a directory of another user or one others may write
  to, and reads no pack or file others may write to, as v1's did: a value is checked against its
  own hash, never against what its key says made it, and the store keeps machine code that is
  run (`Kind::Machine`). `Store::open` asks `blaze_core::Effect::Private` of its directory
  before reading anything in it. That effect makes the directory 0o700 if it is missing, refuses
  it if another user owns it or others may write it, closes it to the group and others (Unix),
  then refuses any file under it, links followed, that is another user's or others may write.
  A file's group write bit (a 002 umask) is refused only while the directory lets its group in.
  On Windows it reads the owner and the access list, as v1's `acl.rs` did. A layer is read-only
  and may be shared, so it is not checked; nothing outside tests configures one yet.
- **Remote** is a read-through tier for machines outside the pool (a laptop) only, never on the
  hot path of the VM. Supersedes the per-job, per-pin remote naming (#1481).

## Issues this lane takes in

- **#1439**: the store keys every value by a hash of all of blaze's sources, so any release
  turns CI cold. v2 keys a value by the schema and code of what produced it (section 5,
  Schema). (`v2-store`)
- **#686**: every process indexes the whole store (1.6-1.8 GB each, OOM at 117 GB). In v2 only
  the daemon holds the store, and its index is the mmapped table. (`v2-store`)
- **#1480**: two store inputs not hashed (unreadable sources skipped, the C compiler of build
  scripts). Traced inputs hash both. (`v2-usercode`, `v2-store`)
- **#1298 store as one mapped file per crate version**: append payloads, write the index into
  the inactive of two slots, flip the header; one barrier, not a sync per entry (anim cold
  wrote 77,481 entries serially). (`v2-store`)

## Done when

- one mapped file per crate version with the index flip; the mmapped BLAKE3 index
- hot, warm and cold tiers with background moves; every read verified; a store others can
  write to refused
- keys by schema and producing code; the remote as an off-path tier
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
