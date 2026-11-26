//! `blaze-ctx`: hierarchical 3-level repository context engine and hybrid search core.
//!
//! Pure `#![no_std]`, `#![blaze::sans_io]`, `#![forbid(unsafe_code)]`, zero third-party
//! dependencies, explicit allocators (`blaze_own::Alloc`).
//!
//! - **Level 0** ([`DomainSummary`]): grouped architectural domain map across crates, docs, and `agents/`.
//! - **Level 1** ([`CrateNode`], [`DocNode`], [`AgentNoteNode`]): unified concept nodes with status badges.
//! - **Level 2** ([`SlicePtr`]): exact `file#Lstart-Lend` pointers on headings, exports, and notes.
//! - **Hybrid Retrieval** ([`search`]): identifier-aware BM25 + character 3-gram overlap + optional
//!   dense [`EmbeddingStore`] cosine similarity + 1-hop typed concept graph expansion, packed into
//!   a strict token budget and rendered in compact LLM-first `blaze-tab` (or `blaze-json`).
//! - **Incremental Cache**: per-file BLAKE3 content hashes persisted in deterministic RFC 8949 CBOR.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod embed;
pub mod format;
pub mod graph;
mod math;
pub mod search;
pub mod tree;

#[cfg(test)]
mod tests;

pub use embed::{EmbedError, EmbeddingStore, cosine_similarity};
pub use format::{
    FormatError, OutputFormat, area_to_value, domain_map_to_value, index_stats_to_value, outline_to_value,
    render_value, search_result_to_value,
};
pub use graph::{EdgeKind, ExpandedNeighbor, GraphEdge, expand_1hop};
pub use search::{
    AreaDoc, AreaExport, AreaNote, AreaReport, DEFAULT_BUDGET_TOKENS, DEFAULT_MAX_HITS, HybridWeights, NodeKind,
    OutlineEntry, OutlineReport, QueryOpts, SearchError, SearchHit, SearchResult, SectionHit, area_report, doc_outline,
    estimate_tokens, extract_trigrams, search, tokenize,
};
pub use tree::{
    AgentNoteNode, CacheError, CrateNode, CrateSnapshot, CrateStatus, CtxIndex, DocNode, DocStatus, DomainSummary,
    ExportItem, FileSnapshot, HeadingSlice, IndexStats, NoteKind, RepoSnapshot, SCHEMA_VERSION, SlicePtr,
    classify_domain, format_slice_ptr,
};
