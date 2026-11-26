---
status: current
summary: Full sans-IO, Git-wire-compatible VCS engine (`crates/blaze-git`) with pluggable storage backends, a macOS-optimized native `.bgit` format (mmap'd Zstd/Raw object slabs, binary fanout index, single-file binary reftable, `CGPH` commit-graph), ORT-style O(changed) 3-way merge, Git protocol v2 + v1 push server & client (Smart HTTP + stdio/stream), and `bgit infect` to convert existing `.git` repos in place.
date: 2026-11-26
---
# Sans-IO Git Engine, Native `.bgit` Store, 3-Way Merge, Wire Server/Client, and `infect`

## Goal

Extend `crates/blaze-git` (pure `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`,
explicit allocator, zero third-party crates) into a full Git-wire-compatible engine that is
dramatically faster than `git` on macOS (APFS / Apple Silicon), works over **any** storage backend
(`ObjectStore` + `RefStore` traits), speaks Git wire protocol as **both server and client**
(real `git clone`, `git fetch`, `git push`, `git ls-remote` interoperate seamlessly), performs
ORT-style O(changed-subtrees) 3-way merges, and provides `bgit infect` to convert existing `.git`
repositories into our native `.bgit` format (plus `disinfect` to export back to standard `.git`).

## Chosen design vs losing alternative

- **Chosen: Pluggable sans-IO traits (`ObjectStore` / `RefStore`) + native `.bgit` mmap slabs (`BGOB`/`BGIX`/`BGRF`/`CGPH`) + thin native driver (`crates/bgit`).**
  All object encoding, pack v2 writing/delta encoding, thin-pack unpacking, protocol v2/v1 server
  and client state machines, Smart HTTP framing, ORT 3-way merge, and `infect` conversion live in
  `crates/blaze-git` (`#![blaze::sans_io]`), tested under `blaze_sim::check` with planted-bug
  controls and verified end-to-end against real `git`.
- **Losing alternative: Wrapping loose `.git/objects/ab/cdef...` + loose `.git/refs/**` directly.**
  On macOS APFS, creating/opening/fsyncing thousands of loose files and ref directories hits
  APFS metadata journaling and per-inode overhead (10–100x slower than sequential slab writes +
  zero-copy mmap reads).

## Native `.bgit/` On-Disk Layout (`BGIT0001`)

All objects keep their exact Git SHA-1 OID (`Oid::of(kind, body)`), so wire compatibility with
`git` is 100% lossless with zero translation overhead on OIDs.

| File | Magic | Layout & Why It Beats `.git` on macOS |
|---|---|---|
| `objects/slab-NNNN.bgob` | `BGOB` v1 | Append-only 8-byte-aligned object records `(oid[20], kind, codec[Raw\|Zstd], flags, raw_len, stored_len, crc32, payload)`. Small/incompressible objects stored `Raw` (zero-copy `&[u8]` mmap read); compressible objects stored with `blaze-zstd` (~10x faster decompress than zlib). Zero 256-directory loose object fanout. |
| `objects/index.bgix` | `BGIX` v1 | 256-entry big-endian fanout + sorted 20-byte OIDs + 8-byte `(slab_id: u16, offset: u48)` + 4-byte `raw_len` + CRC-32 trailer. O(log N) binary search directly in mmap, zero heap allocation. |
| `refs.bgref` | `BGRF` v1 | Single atomic binary file (written temp + sync + rename) holding `HEAD` + sorted refs + symref targets + pre-peeled tag OIDs + CRC-32 trailer. `ls-refs` over 10,000 refs takes one mmap read (< 50 µs vs thousands of `openat` syscalls). |
| `graph.bggp` | `CGPH` v1 | Standard Git binary commit-graph (`OIDF`/`OIDL`/`CDAT`/`EDGE`) with topological generation numbers (`gen(c) = 1 + max(gen(p))`). O(log N) LCA / merge-base / ahead-behind / fetch negotiation with zero object decompression. |

## Hardware Floor & Cost Budget (macOS Apple Silicon / APFS)

| Operation | `.git` Cost | `.bgit` / `blaze-git` Design |
|---|---|---|
| `ls-refs` (R refs) | R loose `open/read/close` + `packed-refs` scan | 1 mmap of `refs.bgref` + binary search prefix slice |
| Object lookup by OID | 256-dir loose `stat` + `.idx` search + zlib inflate | 1 mmap `BGIX` binary search + zero-copy Raw slice or `blaze-zstd` decompress |
| 3-way tree merge (N files, K changed) | Full index unpack + O(N) walk (legacy) | ORT-style O(K log N) tree merge: equal/one-sided subtree OIDs skipped in O(1) without reading children |
| Merge-base / LCA (distance D) | Inflate commits from packfiles | `CGPH` binary commit-graph generation-pruned walk: O(D log C), 0 inflates |
| `git clone` / `fetch` server | Fork `git-upload-pack` + `pack-objects` | In-process `UploadPackServer` reading mmap'd slabs -> streaming PACK v2 + `OFS_DELTA` |
| `git push` server | Fork `git-receive-pack` + `index-pack` + loose refs | In-process `ReceivePackServer` -> thin-pack verify -> single slab append + atomic `BGRF` swap |
| `infect` (N objects, R refs) | N/A | One-pass import of all `.git` loose + pack objects + refs -> `BGOB` + `BGIX` + `BGRF` + `CGPH` |

## Wire Protocol Compatibility (Server & Client)

1. **`UploadPackServer` (Protocol v2)**:
   - Advertises `version 2`, `agent=blaze-git/1`, `ls-refs=unborn`, `fetch=shallow wait-for-done`, `object-format=sha1`.
   - `ls-refs`: `symrefs`, `peel`, `ref-prefix`, `unborn`.
   - `fetch`: `want`, `have`, `done`, `ofs-delta`, `thin-pack`, `deepen`, `include-tag`, `no-progress`; multi-round `acknowledgments` + `packfile` over side-band-64k.
2. **`ReceivePackServer` (Push v1)**:
   - Advertises refs + `report-status delete-refs ofs-delta side-band-64k atomic quiet agent=blaze-git/1`.
   - Parses `<old-oid> <new-oid> <ref>` commands + incoming PACK v2 (including thin packs resolved against `ObjectStore`), verifies every object's SHA-1, applies atomic CAS ref updates, responds with `unpack ok` + `ok`/`ng` lines.
3. **`WireClient`**:
   - Full Protocol v2 `ls-refs` + `fetch` (clone, incremental `have`, shallow `deepen`) and v1 `receive-pack` push client (computes missing object closure, encodes PACK v2 with `OFS_DELTA`, parses `report-status`).
4. **`SmartHttpRouter`**:
   - Sans-IO HTTP/1.1 parser (including `Transfer-Encoding: chunked` and `Git-Protocol: version=2`) serving all four Git Smart HTTP endpoints (`GET/POST info/refs`, `git-upload-pack`, `git-receive-pack`).

## Proof & Testing (`testing` skill)

1. **Codec & Storage DST (`blaze_sim::check`)**: round-trip random object graphs through `BGOB`/`BGIX`/`BGRF`/`CGPH`, bit-flip corruption detection, and planted-bug controls (`SkipSlabCrcCheck`, `SkipOidVerify`).
2. **ORT 3-Way Merge DST (`blaze_sim::check`)**: commutativity of clean merges, fast-forward/up-to-date, O(changed) subtree skip invariant, conflict markers, D/F conflicts, criss-cross merge-base, and planted-bug controls (`SkipThreeWayConflictCheck`, `IgnoreSubtreeDiff`).
3. **Wire Protocol & Pack DST (`blaze_sim::check`)**: full client <-> server round-trips (`ls-refs`, clone, incremental fetch, shallow deepen, push create/update/delete, atomic CAS refusal, thin-pack `REF_DELTA`, Smart HTTP chunked requests) with planted-bug controls (`SkipPackChecksum`, `SkipCasOldOidCheck`).
4. **Real `git` Interop & macOS Benchmark**: spawn `bgit serve --http` on an infected repository and run real `git clone`, `git fetch`, `git commit`, `git push` against it; run `bgit infect` and benchmark against `git` on macOS.
