//! The item layer's schema and its two traits (docs/v2/plan/design/bottom.md).
//!
//! A question about one item reads a few small records keyed from text before the work; a
//! crate is never resolved whole, and a dependency is only its records. This crate holds what
//! those records are and what may be asked of them, and nothing that makes them:
//!
//! - The words: [`ItemKind`], [`Namespace`], [`Visibility`], [`Origin`], [`Target`],
//!   [`ExportClass`], [`Edition`], [`MacroKind`], [`Place`], [`ItemIndex`], [`ByteRange`].
//! - Every record of bottom.md's table that needs no type-checker type, each one blaze-format
//!   buffer written once by a builder in the caller's allocator (`new_in(.., alloc)`, then
//!   `finish`) and read in place by its view (`*View::read`, which checks the buffer once):
//!   [`FileItems`], [`ModuleDecls`], [`ModuleScope`], [`TextualEntry`], [`ModuleFile`],
//!   [`CrateRoot`], [`MacroDef`], [`KeyIndex`], [`LangItems`], [`CrateGraph`], [`NoImplsOf`].
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
//! is sorted at `finish` (names by namespace and name, keys by key), and each string table is
//! laid out at `finish` in the record's own order, so the order rows were given in moves no
//! byte. A list whose order is the source's (a file's items, a textual macro scope, a derive's
//! helpers) is given in that order, which the record's key fixes.
//!
//! # Positions (rule 71)
//!
//! Only [`FileItems`] holds positions. It is keyed by its file's bytes, so a position in the file
//! moves exactly when its key does: an item's start ([`FileItem::at`]) and a macro call's
//! ([`MacroCall::at`]) are file-relative, which is what slicing the file's text needs. What an
//! item holds inside itself (its signature, body and attributes, [`ByteRange`]) is counted from
//! the item's start, so a later record that copies a range keeps it when an edit above moves the
//! item. Every other record names an item by its key and a file by its content; a file's path
//! ([`ModuleFile`]) is relative to the crate's root directory.
//!
//! # Not `no_std` yet
//!
//! blaze-id is `no_std` on `core` and `alloc`; this crate is not, because blaze-format is a std
//! crate and its `record!` macro writes `::std::vec::Vec` into each record's impl. The code here
//! names only `core` and `alloc` items, so it turns `no_std` when blaze-format does.

#![feature(allocator_api, register_tool)]
#![register_tool(blaze)]

mod column;
mod crate_facts;
mod error;
mod file_items;
mod items;
mod key_index;
mod kind;
mod locals;
mod module_decls;
mod module_scope;
mod names;
mod proof;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};

pub use crate::crate_facts::{
    CrateGraph, CrateGraphView, LangItems, LangItemsView, NoImplsOf, NoImplsOfView,
};
pub use crate::error::{BuildError, NotBuilt, SurfaceError};
pub use crate::file_items::{FileItem, FileItems, FileItemsView, MacroCall, ModDecl, UseLeaf};
pub use crate::items::{Items, Source};
pub use crate::key_index::{KeyEntry, KeyIndex, KeyIndexView};
pub use crate::kind::{
    ByteRange, Edition, ExportClass, ItemIndex, ItemKind, MacroKind, Namespace, Origin, Place,
    Target, Visibility,
};
pub use crate::locals::{AnyItem, Local, LocalTable};
pub use crate::module_decls::{DeclaredName, ModuleDecls, ModuleDeclsView};
pub use crate::module_scope::{Binding, EXPORTS_DOMAIN, ModuleScope, ScopeView};
pub use crate::names::{
    CrateRoot, CrateRootHead, CrateRootView, MacroDef, MacroDefHead, MacroDefView, ModuleFile,
    ModuleFileView, TextualEntry, TextualEntryView,
};
pub use crate::proof::HasText;

#[cfg(test)]
mod tests;
