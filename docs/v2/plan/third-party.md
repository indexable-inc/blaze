---
status: plan
summary: What replaces each third-party package a workspace crate still names directly, and the hard ones with their options; the counts and the order are the gate's ratchets (scripts/src/bin/thirdparty.rs) and end-state.md's D180.
date: 2026-11-26
---
# Third-party dependencies, to zero

Andrew, 2026-11-15: "we should have 0 third party". `docs/v2/plan/design/bottom.md` ("Also decided
on 2026-11-15") makes it a rule: every crate the `blaze` binary links depends only on crates of
this workspace, a gate ratchet holds the count per crate and lets it only fall, and a new crate
starts at zero. The ratchet is `scripts/src/bin/thirdparty.rs`, with its lists
`tools/ratchets/third-party.txt` (per crate), `tools/ratchets/thirdparty.txt` (what the binary
reaches) and `tools/ratchets/vendored.txt`, and runs in the `scripts` CI job. The counts and the
order of work are those lists and [end-state.md](design/end-state.md) (D180). This page says what
replaces each package.

- A workspace crate is a package whose manifest is in the repository and not under `vendor/`.
- Everything under `vendor/` counts as third-party. Each fork there is upstream's code with our
  patches: it has a SOURCE.md or PATCHES.md, and its authors and repository are upstream's. A
  fork stops counting as third-party when its manifest says `[package.metadata.blaze] ours =
  true`. That should happen only when we own the code: it has been read, trimmed to what we use,
  and is kept to this workspace's rules.

## Each package named directly

The users column lists the crate names without their `blaze-` prefix, from Cargo.lock. Packages
used only by dev-dependencies are in *italics*. Each replacement is one of three kinds:

- **ours**: a workspace crate does this already, and the users move to it.
- **write**: the code still has to be written, with a rough size.
- **delete**: the package goes away when its user is rewritten or deleted.

Sizes are guesses: lines of Rust, not counting generated tables.

| Package | Users | What for | Replacement |
|---|---|---|---|
| serde | fmt, graph, ids, ix-prof, manifest, profile, protocol, report, resolve, screen, source | (de)serialization traits | delete: records are blaze-format and CBOR (D180); the derives go with them |
| serde_derive | profile | derives | delete, with serde |
| serde_json, sonic-rs | astlog, fmt, graph, ids, ix-prof, manifest, profile, protocol, resolve, screen, script, scripts; sonic-rs in report | JSON in and out | write: one JSON reader and writer, about 800; sonic-rs goes with the legacy report |
| tracing | proc-macro, trace | spans and events | ours: blaze-trace for its users, then spans are bstd's (D178) |
| crossbeam-deque, corosensei | graph | work stealing, stack switching | delete: blaze-graph's fiber scheduler is not the engine's (D157) |
| parking_lot | inproc-rt | locks | ours: `blaze_core::lock` |
| libc | cas, check, core, graph, inproc, inproc-rt, ix-prof, ladder, launch-rt, launcher, mem, proc-macro, profile, stdx, sys | syscalls | ours: blaze-sys, the one home of kernel calls (D170) |
| foldhash | hash | hashing | write: blaze-hash's own hasher |
| blake3 | ids, ix-prof; dev in blake3 | content hashes | ours: blaze-blake3 |
| rustc-stable-hash | source | SipHasher128 | delete: the cargo layer is parked (D172) |
| url | manifest, resolve, source | URLs | delete: the cargo layer is parked (D172) |
| imbl | resolve | persistent maps | delete: the cargo layer is parked (D172) |
| stacker | stdx | stack growth | delete: stdx goes (D172); deep recursion is a frame with a stated bound |
| bytes | http | buffers | ours: blaze-bytes |
| quinn-proto, rustls, ring, *rcgen* | quinn-proto, rustls in http; rustls in bgit; ring in remote, ed25519; rcgen in http, ix-prof | TLS, QUIC, signatures | hard: see below |
| wasmtime | proc-macro, wasi | running wasm | stays (D175, D180); blaze-wasi goes with the interim module builder |
| zstd | format-tiers, parquet, squeeze | compression | hard: see below |
| miniz_oxide | deflate | inflate and deflate | write: blaze-deflate's own deflate |
| base64 | pg | encoding | ours: blaze-base64 |
| tar | tar | `.crate` files | write: a ustar reader in blaze-tar, about 300 |
| annotate-snippets, diff | fmt | diagnostic rendering, text diffs | write: into blaze-report, about 400; one Myers diff, about 300 |
| *dissimilar* | dev in test-utils | text diffs | write: the same Myers diff |
| la-arena | memo | typed arenas | ours: blaze-ids typed ids over a vector |
| *postcard* | dev in ids | test encoding | ours: blaze-pack |
| pathdiff | new | path helpers | write: under 100 |
| proc-macro2, quote, syn | memo-macros | proc macros | ours: blaze-serde-derive's token reader; better still, replace the macros with compile-time reflection (rule 56) |
| ra-ap-rustc_hashes, ra-ap-rustc_index | rustc_abi | rustc's types | write: copy them into crates/, as rustc_abi was |
| *rustc_apfloat* | dev in syntax | soft floats | write: blaze's const evaluator needs its own soft floats anyway; put them in a crate |
| tree-sitter, tree-sitter-python, tree-sitter-rust, tree-sitter-toml-ng | ast-merge-langs, astlog-core | astlog's other languages | hard: see below |
| *tempfile* | dev in astlog-core | scratch directories | write: about 80, in test-utils |
| addr2line, gimli, object, rustc-demangle | ix-prof | reading symbols | write: Mach-O and ELF symbol tables and a DWARF line reader, about 1.5k (blaze already writes these formats), plus a v0 and legacy demangler, about 600 |
| libsqlite3-sys | sqlite | SQLite | delete: what is kept is a record (rule 51), and blaze-sql covers queries |
| objc2-core-foundation, objc2-core-media, objc2-core-video, objc2-video-toolbox | std | macOS frameworks for video | write: `extern` declarations of the few functions it calls, about 200; a system framework is no crate |

## The hard ones

**The profile's file** (arrow-array, arrow-buffer, arrow-ipc, arrow-schema and parquet in
blaze-profile and ix-prof: most of the 191). The profile is a set of records. Options:

1. Write blaze-format records and a DuckDB extension, or a `blaze debug sql` reader, that maps
   them.
2. Write a minimal Parquet writer: plain encoding, no compression, a handful of column types.
   That is about 1.5k lines, and DuckDB still reads the files.

Option 2 keeps every query in the debugging skill working, so it comes first.

**tree-sitter** (astlog-core and ast-merge-langs, with the Python, Rust and TOML grammars, which
are C code). blaze's own syntax trees already cover Rust (`astlog-sema.md`). Options:

1. Delete astlog's tree-sitter source and keep Rust only, through blaze's own parser.
2. Read TOML with blaze-toml, and drop Python.

The plan is to do both.

**Cargo's formats** (toml, toml_edit, cargo-util-schemas, cargo-platform, semver): ours, in
blaze-toml and blaze-semver; no crate names those packages.

**TLS, QUIC and signatures** (ring in blaze-remote; rustls, rustls-native-certs and quinn-proto
in blaze-http; rcgen in tests). Locked sources are fetched over HTTPS, and so is the remote
cache when one is chosen (off unless chosen), so something has to speak TLS. Options:

1. Write TLS 1.3 client-only, with one cipher suite (TLS_AES_128_GCM_SHA256 over X25519, and
   Ed25519 or ECDSA P-256 certificates). That is about 4k lines, including the primitives, and
   needs constant-time code reviewed by a DST and test vectors.
2. Use the operating system's TLS: Network.framework on macOS, and on Linux a kernel TLS
   offload once the handshake is done.
3. Serve the cache over plain HTTP, with every record verified by its signed statement and
   digest, which the Trust rule requires anyway. Then TLS protects only privacy, and public
   records do not need it.

Option 3 removes the need for reads of public records; fetching sources still needs TLS.

**zstd** (blaze-format-tiers, blaze-parquet, blaze-squeeze; patched to `vendor/zstd-sys`, which
is C). Options:

1. A decoder only, about 2k lines, since CI writes the compressed records, plus an encoder at
   one level, about 1.5k lines.
2. Switch the store to lz4, about 400 lines for both directions, at a worse ratio.

Measure first. The page cache is the only cache (rule 51), and uncompressed records mapped in
place may win on the edit loop.

**objc2** (blaze-std's video code) and **libsqlite3-sys** (blaze-sqlite) are listed with the hard
ones only because they are FFI. The table above replaces both.
