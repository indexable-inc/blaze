---
status: plan
summary: The census of blaze's third-party dependencies on 2026-11-15 (99 workspace crates, 413 third-party packages, 102 named directly by normal or build dependencies), what replaces each one, the bottom-up order that takes crates to zero, and the hard ones with their options; the gate ratchet scripts/src/bin/thirdparty.rs holds the count per crate until it is zero.
date: 2026-11-15
---
# Third-party dependencies, to zero

Andrew, 2026-11-15: "we should have 0 third party". `docs/v2/plan/design/bottom.md` ("Also decided
on 2026-11-15") makes it a rule: every crate the `blaze` binary links depends only on crates of
this workspace, a gate ratchet holds the count per crate and lets it only fall, and a new crate
starts at zero. The ratchet is `scripts/src/bin/thirdparty.rs`, with its list
`tools/ratchets/third-party.txt`, and runs in the `scripts` CI job. This page is the census the
work starts from, and what replaces each package.

## How it was counted

The numbers come from `cargo metadata --format-version 1` on main at the start of 2026-11-15,
before the parked crates were folded back in. They are as measured that morning, and later
commits will move them. The ratchet counts from the manifests and `Cargo.lock` as text, so its
numbers can differ slightly from the ones here. The lock lists every optional dependency that
some feature turns on, but `cargo metadata` resolves only the features that are on. The list is
whatever `{"command": "write"}` writes.

- A workspace crate is a package whose manifest is in the repository and not under `vendor/`.
  On the census morning there were 99. `crates/rustc_abi` and `crates/rustc_lexer` are copies of
  rustc's code, but they sit in `crates/`, so they count as ours.
- Everything under `vendor/` counts as third-party. Each fork there is upstream's code with our
  patches: it has a SOURCE.md or PATCHES.md, and its authors and repository are upstream's. A
  fork stops counting as third-party when its manifest says `[package.metadata.blaze] ours =
  true`. That should happen only when we own the code: it has been read, trimmed to what we use,
  and is kept to this workspace's rules. On 2026-11-15 no fork says so.
- There were 413 third-party packages in the graph. 394 of them are reachable through some
  workspace crate's normal and build dependencies. 109 are named directly by some workspace
  crate: 102 by normal or build dependencies, and 7 only by dev-dependencies.
- 15 crates are at zero already: blaze-alloc-count, blaze-bench, blaze-cast, blaze-launch,
  blaze-phf, blaze-radix, blaze-rng, blaze-serde-derive, blaze-sha256, blaze-sim, blaze-sql,
  blaze-sys, const-ast, edition and error-chain.
- The largest closures are blaze-profile (191), blaze-std (185), blaze-fmt (157), blaze-remote
  (155) and blaze-manifest and blaze-new (146 each).
- The `blaze` binary's crate, blaze-engine, was parked on the census morning. The heaviest
  dependencies outside today's graph sit in parked crates: LLVM through `llvm-sys` and `inkwell`,
  plus Cranelift, in hir-ty-codegen, and Wasmtime in blaze-macro-eval. A parked crate comes back
  at zero, or with the row its return is allowed, in the same pull request.

## Each package named directly

The users column lists the crate names without their `blaze-` prefix. Packages used only by dev-dependencies are in
*italics*. Each replacement is one of three kinds:

- **ours**: a workspace crate does this already, and the users move to it.
- **write**: the code still has to be written, with a rough size.
- **delete**: the package goes away when its user is rewritten or deleted.

Sizes are guesses: lines of Rust, not counting generated tables.

| Package | Users | What for | Replacement |
|---|---|---|---|
| serde | 25: audit, build-script, cas, fetch, fmt, format, gc, graph, hash, id, ids, lint-defs, manifest, profile, protocol, remote, report, resolve, screen, serde, solo, source, store-check, vfs, ix-prof | (de)serialization traits | write: blaze-serde's own `Serialize`/`Deserialize` over blaze-pack and blaze-format, about 1.5k; the derive already exists (blaze-serde-derive, no syn) |
| serde_derive | build-script, profile | derives | ours: blaze-serde-derive |
| serde_json, sonic-rs | 16 normal (astlog, audit, cas, codegen-code, fetch, fmt, gc, graph, manifest, profile, remote, script, solo, store-check, ix-prof, scripts); dev in ids, protocol, resolve, screen, serde; sonic-rs in report | JSON in and out | write: one JSON reader and writer under blaze-serde, about 800; sonic-rs goes with it |
| serde-untagged | manifest | untagged enums in Cargo.toml | delete: goes with the TOML reader (see below) |
| tracing, tracing-subscriber | 14: cas, fetch, fmt, gc, host, profile, resolve, solo, cfg, ix-prof, parser, rustc_abi, stdx, syntax; dev in bench | spans and events | write: blaze-trace, as `docs/v2/plan/design/no-tracing.md` specifies; tracing-subscriber goes with it |
| crossbeam-channel | 10: cas, core, fetch, graph, inproc, launch-rt, mem, profile, storemodel, syntax; dev in gc, inproc-rt, parser, stdx | channels | write: a bounded MPMC channel in blaze-core over its wait, about 500, proved with blaze-check |
| crossbeam-queue | 9: cas, core, gc, graph, host, launch-rt, memo, profile, ix-prof | lock-free queues | write: `ArrayQueue` and `SegQueue` in blaze-core, about 400, under blaze-check |
| crossbeam-deque | graph | work stealing | write: a Chase-Lev deque in blaze-core, about 300, under blaze-check |
| parking_lot | check, core, mem; dev in inproc-rt | locks | ours: `blaze_core::lock`, whose bottom has to move onto blaze-sys futexes and ulocks |
| libc | 12: cas, check, core, fmt, gc, graph, inproc-rt, launch-rt, mem, profile, ix-prof, stdx; dev in inproc, sys | syscalls | ours: blaze-sys |
| windows-sys | check, core, gc, graph, inproc-rt, mem, profile | Windows syscalls | delete: Windows is out of scope, so the `cfg(windows)` arms go |
| foldhash, hashbrown, papaya | hash (all three), vfs, syntax-bridge (hashbrown), fetch (papaya) | hash and maps | write: blaze-hash's own hasher, Swiss table and concurrent map (the hash lane's work, 2026-11-15) |
| *indexmap* | dev in hash | ordered map in tests | write: an insertion-ordered map in blaze-hash, about 200 |
| smallvec, thin-vec, arrayvec | dbsp, ids, vfs, parser, syntax, syntax-bridge; thin-vec in ids; arrayvec in tt | inline vectors | write: one inline vector in blaze-core that takes an allocator (rule 11), about 400 |
| blake3, blake3-servil (vendor) | codegen-code, ids, launch-rt, ix-prof; the vendor fork in ids | content hashes | write: blaze-blake3, a portable version of about 600 lines with NEON next; or take the vendor fork as ours after a read |
| sha2 | fetch | registry checksums | ours: blaze-sha256 |
| rustc-stable-hash | source | SipHasher128 | write: about 200, in blaze-hash |
| expect-test | dev in fmt, graph, screen, cfg, parser, rustc_lexer, syntax, syntax-bridge | snapshot tests | write: an `expect!` in test-utils, about 250 |
| regex | astlog-core, astlog-engine, script, ix-prof, scripts | patterns | write: a small Pike VM in blaze-script, about 1.2k, or plain string matching where the patterns are literal |
| semver | audit, fmt, manifest, resolve, source | Cargo versions and requirements | write: see "Cargo's formats" below |
| toml, toml_edit, cargo-util-schemas | toml in audit, codegen-code, fmt, manifest, report, resolve, script, scripts; toml_edit and cargo-util-schemas in manifest, new | Cargo.toml and config | write: see "Cargo's formats" below |
| cargo-platform | manifest, resolve | `cfg(..)` and target keys | ours: the `cfg` crate parses cfg expressions; matching them against a target is about 200 more lines |
| anyhow, thiserror, snafu | anyhow in fmt, manifest, new, resolve, source; thiserror in fmt, profile; snafu in the four astlog crates | errors | delete: typed error enums with a hand-written `Display` (rule 19, the `untyped` ratchet) |
| unicode-width, unicode-ident, unicode-properties, unicode-normalization, unicode-segmentation | width in fmt, report, screen, span; ident in resolve, rustc_lexer; properties in fmt, rustc_lexer; normalization in fetch; segmentation in fmt | Unicode tables | write: one blaze-unicode crate whose tables are generated from the UCD by a script, about 400 plus the tables |
| url, percent-encoding | url in http, manifest, resolve, source; percent-encoding in http | URLs | write: the subset blaze parses, about 300 |
| httparse, http, bytes | httparse and bytes in http; http in http, remote, std; bytes also in profile | HTTP/1 | write: request and response types and a parser in blaze-http, about 600; bytes → ours: blaze-bytes |
| text-size | span, syntax-tree, test-utils, tt | text offsets | write: `TextSize` and `TextRange`, about 150 |
| glob | codegen-code, manifest, new | path globs | write: about 200 |
| itertools, either | fmt, stdx, syntax; either in syntax | adapters | delete: write the few uses out in place |
| base64 | http, pg, profile | encoding | write: about 100 |
| proc-macro2, quote, syn, synstructure | memo-macros, gpu-macros, macros; synstructure in macros | proc macros | ours: blaze-serde-derive's token reader; better still, replace the macros with compile-time reflection (rule 56) |
| tree-sitter, tree-sitter-python, tree-sitter-rust, tree-sitter-toml-ng | ast-merge-langs, astlog-core | astlog's other languages | hard: see below |
| *tempfile* | dev in astlog-core, fetch | scratch directories | write: about 80, in test-utils |
| memchr | build-script, rustc_lexer | byte search | write: a SWAR search with NEON in blaze-core, about 200 |
| zstd, lz4_flex, flate2, miniz_oxide | zstd in cas, format; lz4_flex in cas; flate2 in http, ix-prof; miniz_oxide in std | compression | hard for zstd (see below); lz4: write about 400; inflate: ours (blaze-std `image/zlib.rs`), deflate still to write |
| annotate-snippets | fmt, manifest | diagnostic rendering | write: into blaze-report, about 400 |
| arrow-array, arrow-buffer, arrow-ipc, arrow-schema, parquet | profile, ix-prof; arrow-buffer and parquet in profile | the profile's file | hard: see below |
| pathdiff, dunce, shell-escape, camino | pathdiff in manifest, new; *dunce* in manifest; shell-escape in manifest; camino in paths | path helpers | write: each under 100, and camino's `Utf8Path` about 200 |
| filetime | fetch | mtimes | ours: blaze-sys |
| tar | fetch | `.crate` files | write: a ustar reader, about 300 |
| diff, similar, dissimilar | diff in fmt; similar in edit-applier; dissimilar in test-utils | text diffs | write: one Myers diff, about 300 |
| corosensei, stacker | corosensei in graph; stacker in stdx | stack switching and growth | write: a context switch in blaze-core, about 150 of assembly per architecture, with stack growth on top of it |
| ring, rustls, rustls-native-certs, quinn-proto, *rcgen* | ring in remote; the rest in http | TLS, QUIC, signatures | hard: see below |
| *postcard* | dev in ids | test encoding | ours: blaze-pack |
| la-arena | memo | typed arenas | ours: blaze-ids typed ids over a vector |
| imbl | resolve | persistent maps | write: a HAMT, about 800, or drop the persistence from the resolver |
| libsqlite3-sys | sqlite | SQLite | delete: what is kept is a record (rule 51), and blaze-sql covers queries |
| objc2-core-foundation, objc2-core-media, objc2-core-video, objc2-video-toolbox | std | macOS frameworks for video | write: `extern` declarations of the few functions it calls, about 200; a system framework is no crate |
| wasmtime | wasi | running wasm | hard: see below |
| addr2line, gimli, object, rustc-demangle | ix-prof | reading symbols | write: Mach-O and ELF symbol tables and a DWARF line reader, about 1.5k (blaze already writes these formats), plus a v0 and legacy demangler, about 600 |
| clap | astlog | its CLI | delete: take one JSON value (rule 42) |
| ignore | astlog-core | `.gitignore` | write: a gitignore matcher, about 400 |
| winnow | parser | parser combinators | write: the few it uses, in place |
| bitflags | rustc_abi | flags | write: a `macro_rules!`, about 80 |
| ra-ap-rustc_hashes, ra-ap-rustc_index | rustc_abi | rustc's types | write: copy them into crates/, as rustc_abi was |
| rustc-literal-escaper | parser, syntax | unescaping literals | write: about 200, shared with rustc_lexer |
| smol_str, triomphe | syntax | small strings, `Arc` without weak counts | write: about 200 each |
| *rustc_apfloat* | dev in syntax | soft floats | write: blaze's const evaluator needs its own soft floats anyway; put them in a crate |
| zerocopy | tt | casts | ours: blaze-cast |

## Order of work

Work runs bottom up. A crate reaches zero only when everything it depends on is at zero, so the
leaves go first. When a shared leaf reaches zero, every crate above it loses that much closure in
one change.

1. **Hash, map, channel and queue.** This step unlocks the most. blaze-core has a closure of 26
   and almost every crate depends on it. Its third-party packages are blaze-hash's (foldhash,
   hashbrown, papaya, serde: 15), crossbeam-channel and crossbeam-queue, parking_lot, and libc
   and windows-sys. Replace them with blaze-hash's own table (the hash lane), a channel and
   queues in blaze-core, `blaze_core::lock` on blaze-sys, and blaze-sys itself. blaze-mem (25:
   crossbeam-channel, parking_lot, libc) and blaze-reclaim, blaze-spin and blaze-check (23 each)
   fall with it. Then blaze-bytes (25), blaze-inproc (25) and blaze-inproc-rt (3: libc and
   windows-sys) reach zero, because what remains of their closures comes in through
   core and mem. Proofs come first, because these are concurrent: each structure is checked under
   blaze-check and blaze-sim with a planted bug, as it is written.
2. **serde, the traits.** blaze-serde (7) depends on serde. Once blaze-serde owns its traits,
   which its derive already targets, the 25 serde users move to it. blaze-ids (58; the other
   lane is on it) then also needs an inline vector to replace smallvec and thin-vec, and BLAKE3.
   Those three give blaze-id, blaze-lint-defs, blaze-record, intern and target-features (58
   each), and the 63s (format, hashcons, image, keyed, lir, program, surface) whose closures run
   through ids.
3. **The small leaves.** Each is a few hundred lines and gets one pull request:
   - blaze-pg (1): base64.
   - paths (1): camino.
   - rustc_lexer (3): memchr and blaze-unicode.
   - gpu and gpu-macros (4), and macros (5): the proc-macro stack.
   - blaze-protocol and rustc_abi (15): serde, bitflags and the rustc copies.
   - blaze-memo-macros (16): the proc-macro stack.
   - blaze-build-script (27).
   - blaze-sqlite (34): deleted.
4. **The front end's leaves.** These are syntax, parser, span, tt, cfg, syntax-bridge,
   syntax-tree and test-utils, which have closures of 59 to 104. Their packages are text-size,
   smol_str, triomphe, either, itertools, winnow, rustc-literal-escaper, expect-test, the
   Unicode tables and tracing. tracing goes through blaze-trace (no-tracing.md), which every
   tracing user then follows.
5. **The tools.** astlog and its crates: clap, snafu, regex, ignore, similar, tree-sitter. Then
   ix-prof and blaze-profile: the symbol readers, plus the profile's file format.
6. **The hard ones,** below. They come last because each one needs a decision.

## The hard ones

**LLVM** (llvm-sys, inkwell, in parked hir-ty-codegen; patched to `vendor/llvm-sys`). Options:

1. Drop the Rust bindings and declare the few LLVM C API functions LIR lowering calls as
   `extern "C"` in a workspace crate, generated from LLVM's headers by a script. This removes
   the third-party crates, though LLVM itself is still a C++ library we link.
2. Make blaze's own backend the top tier. `lir.md` plans for that end state, and LLVM stays an
   optional tier until then.

The rule counts crates, so option 1 meets it now. Option 2 is the end state.

**Wasmtime** (blaze-wasi; parked blaze-macro-eval; patched to `vendor/wasmtime`). Proc macros run
on wasm (rule 10). Options:

1. Write a wasm interpreter. The wasm32-wasip1 subset that proc macros use is small. An
   interpreter with no JIT is about 5k lines, and proc macros are short.
2. Write a wasm-to-LIR translator that runs on blaze's own code generator. That is about 8k
   lines on top of option 1, and gives native speed.
3. Keep `vendor/wasmtime` and mark it as ours. It is a huge tree to own, so this is not
   recommended.

The plan is option 1 first, then option 2 if profiles show macro expansion is a hot spot.

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
2. Write a small TOML parser, which is needed anyway (below), and drop Python.

The plan is to do both.

**Cargo's formats** (toml, toml_edit, cargo-util-schemas, cargo-platform, semver and
serde-untagged in blaze-manifest, resolve, audit, fmt, new, source and the scripts). Cargo
compatibility needs:

- a full TOML 1.0 reader;
- an editor that keeps formatting, for `blaze new` and `--fix` edits to manifests;
- semver versions and requirements, with cargo's exact matching rules, prerelease included;
- the schema of Cargo.toml.

Write all four:

- blaze-toml, a reader with item-relative spans, about 1.5k lines. `scripts/src/bin/thirdparty.rs`
  already holds a refusing subset of one.
- An editor over its spans that rewrites only the bytes of the changed entry, about 500 lines.
- blaze-semver, about 600 lines, tested against cargo's cases.
- The schema, as blaze-manifest's own types, which it mostly has already.

Of all the hard ones, this is the lowest risk.

**TLS, QUIC and signatures** (ring in blaze-remote; rustls, rustls-native-certs and quinn-proto
in blaze-http; rcgen in tests). The shared cache is read without a login over HTTPS, so
something has to speak TLS. Options:

1. Write TLS 1.3 client-only, with one cipher suite (TLS_AES_128_GCM_SHA256 over X25519, and
   Ed25519 or ECDSA P-256 certificates). That is about 4k lines, including the primitives, and
   needs constant-time code reviewed by a DST and test vectors.
2. Use the operating system's TLS: Network.framework on macOS, and on Linux a kernel TLS
   offload once the handshake is done.
3. Serve the cache over plain HTTP, with every record verified by its signed statement and
   digest, which the Trust rule requires anyway. Then TLS protects only privacy, and public
   records do not need it.

Option 3 removes the need for reads of public records. Option 1 is for remote execution.

**zstd** (blaze-cas, blaze-format; patched to `vendor/zstd-sys`, which is C). Options:

1. A decoder only, about 2k lines, since CI writes the compressed records, plus an encoder at
   one level, about 1.5k lines.
2. Switch the store to lz4, about 400 lines for both directions, at a worse ratio.

Measure first. The page cache is the only cache (rule 51), and uncompressed records mapped in
place may win on the edit loop.

**objc2** (blaze-std's video code) and **libsqlite3-sys** (blaze-sqlite) are listed with the hard
ones only because they are FFI. The table above replaces both.
