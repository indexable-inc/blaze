//! The item layer's schema and its two traits (docs/v2/plan/design/bottom.md).
//!
//! A question about one item reads a few small records keyed from text before the work; a
//! crate is never resolved whole, and a dependency is only its records. This crate is the one
//! home of what those records are and what may be asked of them (rule 30); the crates that
//! build one family of records (blaze-file-items, blaze-scope, blaze-impls, blaze-expand) keep
//! their algorithms and working forms, and re-export the record types from here:
//!
//! - The words: [`ItemKind`], [`Namespace`], [`Edition`], [`CallShape`], [`Visibility`] and its
//!   [`VisibilityClass`], [`WrittenVis`], [`Origin`], [`BindingState`], [`Binding`], [`Def`],
//!   [`Place`], [`ItemIndex`], [`Range`], [`Path`] and [`Rename`], and a macro's [`MacroKind`].
//! - Every record of bottom.md's table that needs no type-checker type, each one blaze-format
//!   buffer written once by a builder in the caller's allocator and read in place by its view
//!   (`*View::read`, which checks the buffer once): [`FileItems`], `ModuleDecls`
//!   ([`DeclsBuilder`], [`ModuleDeclsView`]), `ModuleScope` ([`ScopeBuilder`], [`ScopeView`]),
//!   [`TextualEntry`], [`ModuleFile`], `CrateRoot` ([`CrateRootBuilder`], [`CrateRootView`]),
//!   [`MacroDef`], [`KeyIndex`], `LangItems` ([`build_lang_items`]), `CrateGraph`
//!   ([`build_crate_graph`]), the impl index and `NoImplsOf` ([`build_impl_index`]).
//! - The traits: [`Items`], everything the type checker may ask of any crate, read from records
//!   only, and [`Source`], the crate a task compiles, whose text is reached only with a
//!   [`HasText`] proof. Each question by [`ItemKey`] has a form by [`Local`], a task's own
//!   number for an item from its [`LocalTable`]; a `Local` is not `blaze_id::Portable`.
//! - The typed stops: [`NotBuilt`] for a question this layer cannot answer yet, inside
//!   [`SurfaceError`]; [`BuildError`] for what a builder refuses.
//!
//! # Bytes
//!
//! Two builders of one record write equal bytes (rule 25): every list with no order of its own
//! is sorted when written, and each string table is laid out in the record's own order, so the
//! order rows were given in moves no byte. A list whose order is the source's (a file's items, a
//! textual macro scope, a derive's helpers) is given in that order, which the record's key fixes.
//!
//! # Positions (rule 71)
//!
//! Only [`FileItems`] holds positions. It is keyed by its file's bytes, so an entry's own range
//! is file-relative, which is what slicing the file's text needs; every other range of an entry
//! is counted from the entry's start. Every other record names an item by its key and a file by
//! its content; a file's path ([`ModuleFile`]) is relative to the crate's root directory.
//!
//! # `no_std`
//!
//! The code names only `core` and blaze-own. `std` is linked for one reason: blaze-format's
//! `record!` writes `::std::vec::Vec` into each record's impl, and a builder's output is that
//! type ([`RecordBytes`]). Both go when blaze-format moves to blaze-own.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool)]
#![register_tool(blaze)]

// `record!` names `::std`; see the crate docs.
#[cfg(not(test))]
extern crate std;

mod column;
mod crate_graph;
mod crate_root;
mod error;
mod file_items;
mod impl_index;
mod impl_row;
mod items;
mod key_index;
mod kind;
mod lang_items;
mod locals;
mod macro_def;
mod module_decls;
mod module_links;
mod module_scope;
mod names;
mod path;
mod proof;
mod shape;
mod through;
mod through_scratch;
pub mod builtin_attrs;
pub mod lang_features;
pub mod tool_attrs;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};

pub use crate::column::RecordBytes;
pub use crate::crate_graph::{
    CrateFlags, CrateGraphView, CrateIndex, CrateInput, DepEdge, DepInput, EdgeFlags,
    GRAPH_SCHEMA, GraphError, MAX_CRATES, MAX_EXTERN_NAME, Members, build_crate_graph,
};
pub use crate::crate_root::{CrateRootBuilder, CrateRootHead, CrateRootView, Dep};
pub use crate::error::{BuildError, NotBuilt, SurfaceError};
pub use crate::file_items::{
    Attr, AttrKind, AttrSpan, ChildItems, ErrorOrigin, FileItem, FileItems, FileItemsView, Item,
    ItemDetail, ItemFlags, SyntaxError, flagged_in_files, flags_of_files,
};
pub use crate::impl_index::{
    BUCKET_SCHEMA, Bucket, BucketHash, BucketKey, BucketView, CrateImpls, DIRECTORY_SCHEMA,
    DirHit, DirectoryView, ImplIndexError, MAX_ROWS, NO_IMPLS_SCHEMA, NoImplsOfView,
    build_impl_index, trait_mark,
};
pub use crate::impl_row::{ImplFlags, ImplRow, UnknownFlags};
pub use crate::items::{Items, Source};
pub use crate::key_index::{KeyEntry, KeyIndex, KeyIndexView};
pub use crate::kind::{
    AmbiguityKind, Binding, BindingState, CallRole, CallShape, Def, DefKind, Edition, ItemIndex, ItemKind,
    MacroShapes, Namespace, Origin, Place, Range, ScopeEntry, Visibility, VisibilityClass, WrittenVis,
};
pub use crate::lang_items::{
    LANG_SCHEMA, LangItemsError, LangItemsView, LangName, LangNameError, MAX_LANG_NAME,
    build_lang_items,
};
pub use crate::locals::{AnyItem, Local, LocalTable};
pub use crate::macro_def::{
    BuiltinAttr, BuiltinDerive, BuiltinFn, DefSite, MacroDef, MacroDefHead, MacroDefView,
    MacroKind, ProcKind, Transparency,
};
pub use crate::module_decls::{
    ChildModDecl, DeclaredItem, DeclsBuilder, ExpandedDecls, ExternCrateDecl, MacroCallDecl,
    MacroRulesDecl, ModuleDeclsView, ModuleInfo, UseDecl,
};
pub use crate::module_links::{
    CellSeen, GLOB_LINKS_DOMAIN, GlobView, IMPORT_DOMAIN, IMPORTS_FORM, IMPORTS_MAGIC, ImportView, ImportsBuilder,
    ImportsView, LINK_DOMAIN, LINKS_FORM, LINKS_MAGIC, LinkView, LinksBuilder, LinksView, Segments, TRAITS_CELL,
    DECLARED_VIS, RowVis, mark_cell,
    TRAITS_DOMAIN, Table, TableRefused, TargetView, imports_part, links_part,
};
pub use crate::module_scope::{
    AnonImport, GlobEdge, Globs, RefusedWhy, ScopeBuilder, ScopeError, ScopeView, Site, TextualDef, TextualMark,
    Unresolved, UseSite, globs_digest, textual_digest, GLOBS_DOMAIN, TEXTUAL_DOMAIN,
};
pub use crate::names::{ModuleFile, ModuleFileView, TextualEntry, TextualEntryView};
pub use crate::path::{ExternTarget, Path, PathKind, PathView, Rename, UseForm};
pub use crate::proof::HasText;
pub use crate::shape::{
    FloatWidth, IntWidth, Mutability, SelfShape, Shape, ShapeError, UintWidth,
};
pub use crate::through::{
    Asker, MAX_REACHED, Node, Passed, SeenFrom, Sources, Through, merge, through,
};
pub use crate::through_scratch::{ThroughScratch, through_in};

#[cfg(test)]
mod tests;
