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
//! It is `no_std` on `core` and `alloc`, as blaze-id is; `std` is named only because
//! blaze-format's `record!` names `::std::vec::Vec` in what it expands to.

#![no_std]
#![feature(allocator_api)]

extern crate alloc;
// `blaze_format::record!` expands to `::std::vec::Vec`: the one reason `std` is linked.
extern crate std;

mod graph;
mod index;
mod lang;
mod lookup;
mod row;
mod shape;

pub use crate::graph::{
    CrateFlags, CrateGraphView, CrateIndex, CrateInput, Dep, DepInput, Edition, EdgeFlags,
    GRAPH_SCHEMA, GraphError, MAX_CRATES, MAX_EXTERN_NAME, Members, build_crate_graph,
};
pub use crate::index::{
    Bucket, BucketHash, BucketKey, BucketView, CrateImpls, DIRECTORY_SCHEMA, BUCKET_SCHEMA,
    DirHit, DirectoryView, ImplIndexError, MAX_ROWS, NO_IMPLS_SCHEMA, NoImplsOfView,
    build_impl_index, trait_mark,
};
pub use crate::lang::{
    LANG_SCHEMA, LangError, LangItem, LangItemsError, LangItemsView, LangName, LangNameError,
    LangRead, LangRecords, build_lang_items, lang_item,
};
pub use crate::lookup::{
    Candidates, ImplLookupError, ImplRead, ImplRecords, Incoherent, Probe, RecordKind, candidates,
    inherent_candidates,
};
pub use crate::row::{ImplFlags, ImplRow, UnknownFlags};
pub use crate::shape::{FloatWidth, IntWidth, Mutability, SelfShape, Shape, ShapeError, UintWidth};
