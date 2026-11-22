//! The impl index, lang items and crate graph of the item layer (docs/v2/plan/design/bottom.md,
//! the rows `ImplIndex` bucket, `LangItems`, `CrateGraph` and `NoImplsOf`).
//!
//! Each is a blaze-format record, written once by a pure builder and read in place by a view:
//!
//! | record | one per | key of a lookup | what a lookup reads |
//! |---|---|---|---|
//! | [`CrateGraph`](build_crate_graph) | build | a crate's index or key | the record, once per command |
//! | [`NoImplsOf`](NoImplsOfView) | crate | a trait, or a bucket's key | one binary search of two `u64` columns |
//! | impl directory ([`DirectoryView`]) | crate | a bucket's key hash | one binary search; a miss ends here |
//! | impl bucket ([`BucketView`]) | (crate, bucket key) | — | its rows, sorted by impl key |
//! | [`LangItems`](build_lang_items) | crate | a lang item's name | one binary search |
//!
//! A bucket holds the impls of one [`BucketKey`]: a trait for one [`SelfShape`], a trait's
//! blanket impls, or the inherent impls of one shape. So "which impls of `T` could apply to a
//! type of shape `S`" ([`candidates`]) reads, of each crate that can hold one, its
//! `NoImplsOf`, and only where that does not rule the crate out its directory and the one or
//! two buckets the directory names: never a crate's whole table, never a walk.
//!
//! Every read a lookup makes is handed to the caller ([`ImplRecords::note`],
//! [`LangRecords::note`]), the negative ones included, so the caller's read list holds
//! exactly the questions its answer depended on.
//!
//! Nothing here is ambient: every builder takes its allocator, every record its bytes, and
//! the bytes a builder writes are a function of the set of its inputs, in any order.
//!
//! The records, their builders and their views are blaze-surface's; this crate is the lookups
//! over them, on `core` alone: it allocates nothing.

#![no_std]

mod lang;
mod lookup;

// The records themselves, and their builders and views, are blaze-surface's (one home per
// record, rule 30): re-exported so a lookup's caller names one crate.
pub use blaze_surface::{
    BUCKET_SCHEMA, Bucket, BucketHash, BucketKey, BucketView, CrateFlags, CrateGraphView,
    CrateImpls, CrateIndex, CrateInput, DIRECTORY_SCHEMA, DepEdge, DepInput, DirHit, DirectoryView,
    EdgeFlags, Edition, FloatWidth, GRAPH_SCHEMA, GraphError, ImplFlags, ImplIndexError, ImplRow,
    IntWidth, LANG_SCHEMA, LangItemsError, LangItemsView, LangName, LangNameError, MAX_CRATES,
    MAX_LANG_NAME,
    MAX_EXTERN_NAME, MAX_ROWS, Members, Mutability, NO_IMPLS_SCHEMA, NoImplsOfView, SelfShape,
    Shape, ShapeError, UintWidth, UnknownFlags, build_crate_graph, build_impl_index,
    build_lang_items, trait_mark,
};

pub use crate::lang::{LangError, LangItem, LangRead, LangRecords, lang_item};
pub use crate::lookup::{
    Candidates, ImplLookupError, ImplRead, ImplRecords, Incoherent, Probe, RecordKind, candidates,
    candidates_in, inherent_candidates, inherent_candidates_in,
};
