---
status: plan
summary: blaze v2: hardware acceleration, and why no GPU.
date: 2026-10-29
---
# Hardware: use all of it

Every hot loop uses the machine's acceleration, chosen at run time by CPU feature detection.
Every path gives **bit-identical results** (determinism), so the choice is dispatch, not a
fallback. Targets: AMD EPYC Turin (Zen 5: AVX-512 with a full 512-bit datapath, VAES,
VPCLMULQDQ, SHA-NI, 12 CCDs), AWS Graviton 5 (Neoverse V3: SVE2, NEON, SHA3, crypto
extensions, one die), Apple M-series (NEON, crypto extensions, P and E cores).

| work | acceleration | lane |
|---|---|---|
| BLAKE3 of sources, items and blobs | AVX-512 and NEON (BLAKE3's SIMD, many chunks at once); one batched call per many small inputs, not one call each | format, items, store |
| lexing and outline parse | SIMD classification of bytes (simdjson-style: whitespace, identifiers, punctuation, string and comment bounds found 64 bytes at a time) | format |
| interner and memo lookups | SwissTable-style probing with SIMD tag matching; prefetch of the next probe; hashes computed once and carried | format, db |
| content-defined chunking | a SIMD gear hash (FastCDC) | store |
| compression | zstd with long-distance matching and threads for cold packs; dictionaries per kind; LZ4 where decompression speed matters | store |
| checksums of headers and indexes | CRC32C instructions (SSE4.2, ARMv8 CRC) | store |
| relocations and blob copies | parallel per function; large copies with non-temporal stores; reflink or clonefile instead of copies on disk | link, store |
| byte comparison (oracle, dedupe, backend memo) | SIMD compares, hash first | proof, codegen |
| memory | 2 MB huge pages (THP, `MAP_HUGETLB`) for arenas, interners and mapped packs; memory allocated on the NUMA node of the worker that uses it; per-worker arenas | core, format |
| IO | io_uring with SQPOLL, registered buffers and files, linked submissions, `O_DIRECT` for pack writes, multishot accept and recv; NVMe queue depth kept full | core, store |
| threads | pinned per core; work stealing within a CCD or cluster first; P cores for the critical path on Apple | core |
| blaze itself | built with PGO and BOLT per target CPU (`znver5`, `neoverse-v3`, `apple-m4`); hot paths checked for vectorization in the emitted code | coordinator |
| the code blaze emits | target features of the machine it runs on for dev builds (`-C target-cpu=native` semantics when the program runs where it was built), and the profile's choice for release | codegen |

**No GPU on the hot path.** A compiler front end is pointer-chasing and branchy (name
resolution, trait solving, inference, macro expansion), which GPUs run badly, and the
Graviton and EPYC machines have no GPU. GPU compilers exist only for small languages
(Pareas, co-dfns). The only parts that would map well are hashing, chunking, compression and
byte comparison of very large data, and on these machines the CPU's SIMD already runs them
near memory bandwidth. A GPU is reconsidered only if a measurement shows one of those
saturating the CPU.
