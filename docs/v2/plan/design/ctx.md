---
status: plan
summary: "blaze-ctx: hierarchical 3-level repository context engine and hybrid retriever (BM25 + char 3-grams + pluggable dense embeddings + 1-hop graph expansion) emitting token-budgeted blaze-tab tables over an incremental BLAKE3/CBOR index."
date: 2026-11-26
---
# blaze-ctx: Hierarchical Repo Context & Hybrid Search Engine

## 1. Problem & Goal

Large monorepos such as blaze (~190 workspace crates, hundreds of design documents, and 600+ agent how-tos and annoyance records under `agents/`) exceed what any agent or engineer should scan by raw `grep` or blind full-file reads. Unstructured search misses design docs linked to a crate, misses open annoyances for a subsystem, confuses historical `record`/`superseded` docs with `current` architecture, and wastes LLM context tokens on repeated JSON keys or irrelevant file bodies.

`blaze-ctx` (`crates/blaze-ctx`) solves this with a single, deterministic, zero-third-party, sans-IO context engine that:

1. Maintains a **3-level Hierarchical Summary Tree** over crates, docs, and `agents/` notes.
2. Runs **Hybrid Retrieval** (field-weighted BM25 + character 3-gram overlap + optional dense `EmbeddingStore` cosine similarity + 1-hop typed graph expansion).
3. Packs the highest-signal result slice within a strict **token budget** (`--budget <tokens>`), rendered natively in compact **`blaze-tab`** tables (via `crates/blaze-tab`) or JSON (`crates/blaze-json`) over `blaze-data::Value`.
4. Caches parsed nodes incrementally in deterministic RFC 8949 CBOR (`.blaze/ctx-index.cbor`, via `crates/blaze-cbor`) keyed by per-file BLAKE3 content hashes (`crates/blaze-blake3`).

## 2. Architecture

### Sans-IO Core & Thin Driver (Rule 14, Rule 75)

The library (`crates/blaze-ctx/src/lib.rs`) is `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`, with zero third-party dependencies and explicit allocators (`blaze_own::Alloc`) on every collection and API entry point:

| Module | Responsibility |
|---|---|
| `tree` | 3-level data model (`DomainSummary`, `CrateNode`, `DocNode`, `AgentNoteNode`, `SlicePtr`), incremental BLAKE3 snapshot ingestion (`update_incremental`), TOML/Rust/Markdown/JSON extractors, deterministic CBOR serialization. |
| `graph` | Typed directed + reverse-indexed concept graph (`DesignedBy`, `ImplementedIn`, `DependsOn`, `AnnoyancesFor`, `ReferencesDoc`) and 1-hop scored neighbor expansion with provenance. |
| `embed` | Optional fixed-dimension dense `EmbeddingStore` with strict dimension validation and deterministic cosine similarity (never a silent fallback). |
| `search` | Identifier-aware tokenizer (`snake_case`, `CamelCase`, `kebab-case`, `::`), pure-`core` deterministic `ln`/`sqrt`, field-weighted BM25 + char 3-gram overlap + optional dense fusion, heading-level slice pinpointing, and token-budgeted greedy packer. |
| `format` | `blaze-data::Value` builders for `blaze-tab` (sparse/homogeneous union-schema tables) and `blaze-json` output. |
| `bin/blaze-ctx` | Thin CLI driver (`index`, `query`, `outline`, `area`, `map`) performing filesystem traversal, reading/writing `.blaze/ctx-index.cbor`, and printing formatted output. |

### 3-Level Hierarchical Summary Tree

- **Level 0 — Domain Map (`DomainSummary`)**: Groups workspace crates, design docs, and agent notes into architectural domains (`frontend`, `backend`, `runtime-and-exec`, `storage-and-cache`, `data-and-formats`, `std-and-memory`, `network-and-db`, `ui-cad-and-site`, `lint-test-and-obs`, `other`), reporting live/parked/experimental counts, key crates (by dependency centrality), and key current/plan docs.
- **Level 1 — Unified Concept Nodes**:
  - **`CrateNode`**: crate name, workspace path, entry file (`src/lib.rs` or `src/main.rs`), status badge (`live | parked | experimental`), domain, 1-line summary (from `Cargo.toml` description or `//!` doc comment), linked design docs, workspace dependencies, and exported public items (`pub struct|enum|trait|fn|type|mod|const|use`) with line numbers.
  - **`DocNode`**: path, front-matter status (`current | plan | record | superseded | unmarked`), date, `superseded_by`, domain, title, summary, exact fenced-code-aware heading tree (`## Heading -> Lstart-Lend`), mentioned crates, and linked docs.
  - **`AgentNoteNode`**: `agents/howto/*.json` and `agents/annoyances/*.json` entries with kind (`howto | annoyance`), note id, date, state (`open | current | ...`), area, 1-line summary, full detail text for indexing, extracted crate/doc tags, and line span.
- **Level 2 — Exact Slice Pointers (`file#Lstart-Lend`)**: Every heading section, crate export, and note carries a verified 1-based line range pointer so agents open only the exact slice they need.

### Hybrid Retrieval & Explicit Semantics (Rule 3, Rule 25)

1. **Tokenization**: Splits ASCII identifiers on `_`, `-`, `/`, `.`, `::`, punctuation, digit boundaries, and `CamelCase` transitions (`HTTPServer` -> `http`, `server`). Lowercases deterministically.
2. **BM25 + Character 3-Grams**: Field-weighted BM25 (`k1 = 1.2`, `b = 0.75`; title/name/exports `3.0x`, summary/area/tags `2.0x`, body/detail/deps `1.0x`) combined with character 3-gram Dice overlap over titles, headings, and summaries so morphological variants (`refinement`/`refined`, `persistence`/`persisted`, `serialization`/`serialize`) match without a stemmer.
3. **Pluggable Dense Embeddings**: `QueryOpts::dense: Option<&[f32]>` fuses exact cosine similarity from `EmbeddingStore` when provided. Passing a dense vector when the store is empty returns `SearchError::NoEmbeddingsLoaded`; passing the wrong dimension returns `SearchError::DimensionMismatch`. Never silently degrades.
4. **1-Hop Graph Expansion**: Top lexical/hybrid seeds expand 1 hop across `designed_by`, `implemented_in`, `depends_on`, `annoyances_for`, and `references_doc` edges, attaching `via` provenance (e.g. `designed_by(crate:blaze-ty)`).
5. **Status Transparency & Freshness**: Every output row carries its status badge (`live`, `parked`, `experimental`, `current`, `plan`, `record`, `superseded`, `open`). Superseded docs receive a mild ranking discount (`0.55x`) unless matched directly by path or id, while never being hidden.
6. **Token-Budgeted Packing**: Estimates rendered tokens deterministically and packs direct hits, graph neighbors, and matching `#Lstart-Lend` heading slices up to `budget_tokens`, reporting `used_tokens`, `budget_tokens`, `total_candidates`, and `omitted`.

### Incremental Content-Hashed Cache (Rule 26)

`blaze-ctx index` hashes each tracked file with `blaze_blake3::hash`. If `.blaze/ctx-index.cbor` exists with matching `schema_version`, nodes whose BLAKE3 content digest is unchanged are reused verbatim without re-parsing TOML, Markdown, Rust, or JSON; only modified/new files are parsed and removed files are dropped. An incompatible `schema_version` is reported and rebuilt cleanly from scratch (no migration).

## 3. CLI Interface

- `blaze-ctx index [--root <dir>]` — incrementally builds/updates `<root>/.blaze/ctx-index.cbor` and reports counts, cache reuse, index size, and timing.
- `blaze-ctx query "<text>" [--root <dir>] [--budget <tokens>] [--format tab|json]` — runs hybrid search + 1-hop graph expansion and prints the token-budgeted result slice.
- `blaze-ctx outline <doc-path> [--root <dir>] [--format tab|json]` — prints the doc's heading tree with exact `path#Lstart-Lend` slice pointers.
- `blaze-ctx area <crate-or-topic> [--root <dir>] [--format tab|json]` — prints the crate card, dependencies, reverse dependents, top exports, linked design docs, and related `agents/howto` & `agents/annoyances` entries.
- `blaze-ctx map [--root <dir>] [--format tab|json]` — prints the Level-0 domain map.

## 4. Performance Floor (Rule 67)

On a single core (`1 core`), indexing ~1,100 metadata/doc/note files (~6–10 MiB total text) is bounded by filesystem read + BLAKE3 hashing (~2.5 GB/s/core) + single-pass linear text scanning (~500 MB/s/core) + CBOR encoding, giving a single-core floor of ~15–40 ms warm in page cache, and sub-5 ms for incremental no-op re-index and queries over ~800–1,000 Level-1 nodes.
