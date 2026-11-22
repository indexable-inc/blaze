//! C in blaze, step 1 (docs/v2/plan/design/c.md, decision D174): everything of a header import
//! that needs no C compiler.
//!
//! `blaze::c::import! { mod rocksdb; header "rocksdb/c.h"; include "vendor/rocksdb/include"; }`
//! is a built-in macro whose expansion is ordinary Rust declarations. This crate is that macro's
//! whole core, sans-IO:
//!
//! | module | what |
//! |---|---|
//! | [`grammar`] | the macro's arguments, parsed into a typed, normalized [`Request`] |
//! | [`model`] | what a header gives a Rust program, as columns ([`DeclsBuilder`]): types by the target's sizes, records with the front end's layout, enums, typedefs, functions, variables, constant macros, and a typed "not expressed" row for every construct left out |
//! | [`canon`] | the record: the model in its one canonical order, so two front ends that agree write equal bytes |
//! | [`view`] | the record read in place ([`DeclsView`]), checked once |
//! | [`place`] | where a header is read from: a root and a checked relative path, never a path of the machine |
//! | [`key`] | the request key (from text, before the work), the read list and the import key |
//! | [`front`] | the seam: [`CFrontEnd`], a machine that asks for file texts by effect and yields declarations |
//! | [`import`] | the import machine over a front end, and [`import::import`], what the expansion side calls |
//! | [`render`] | a record to Rust text, with each declaration it could not render as a [`Finding`] |
//! | [`differ`] | the first difference between two records of one request, for the differential test |
//!
//! C++ (decision D176) extends this model and record and does not fork it: a declaration has a
//! namespace, a function row is one member of an overload set with its own linker symbol, a
//! record has methods, bases and a relocation fact, a type can be a reference, a function can
//! throw. C fills none of them; the rendering refuses each by a typed finding (`Cpp*` causes)
//! until C++'s steps are built.
//!
//! The crate opens no file, reads no clock and names no global allocator: a front end asks for a
//! file's text through [`front::FrontEffect::Read`], the import machine passes the ask on as an
//! [`import::ImportEffect::Read`], and the thin driver (the expansion's recorder, a test's map)
//! answers it.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod canon;
pub mod differ;
pub mod finding;
pub mod front;
pub mod grammar;
pub mod import;
pub mod key;
pub mod model;
pub mod place;
pub mod render;
pub mod view;

#[cfg(test)]
mod tests;

pub use canon::{CanonPlant, decls_schema, encode};
pub use differ::{Column, Difference, DifferPlant, Part, first_difference};
pub use finding::Finding;
pub use front::{
    CFrontEnd, FileNo, FrontEffect, FrontError, FrontFault, FrontInput, FrontState, ModelInclude, Search, SearchList,
    abi_row,
};
#[cfg(any(test, feature = "model-front-end"))]
pub use front::ModelFrontEnd;
pub use grammar::{
    Arg, C_SCALARS, Define, DefineValue, Expected, GrammarError, GrammarFault, MOST_POINTERS, MacroFn, MacroParam, MacroType,
    Pattern, Request, Tok, TokKind, parse,
};
pub use import::{
    Expansion, Files, Import, ImportEffect, ImportError, ImportEvent, ImportOpts, ImportState, Imported, import,
};
pub use key::{
    CAbi, CDialect, CTarget, Endian, ImportKey, KeyInputs, Read, ReadList, ReadsError, ReadsOpenError, ReadsView,
    RenderKey, RequestKey, Scalar, UnitKey, reads_schema,
};
pub use model::{
    Bitfield, CType, Conv, DeclKind, DeclsBuilder, DeclsOpts, EnumConst, EnumId, Field, FloatKind, FnAttrs, Function,
    HeaderId, IntKind, Linkage, MacroValue, MethodKind, ModelError, Name, NsId, Origin, Quals, RecordBody, RecordId,
    RecordKind, Relocation, TypeId, TypedefId, Unexpressed, ValueCategory, VarAttrs, VarId, VarInit, Variable,
};
pub use place::{Dir, Place, PlaceError, RelPath, Root, path_digest};
pub use render::{MacroFnFault, RenderError, Rendered, render};
pub use view::{DeclsView, Table, ViewError};

/// "None" in every `u32` reference column of the model and the record: no name, no owner, no
/// unit, the global namespace.
pub const NONE: u32 = u32::MAX;

/// `value` as an index.
pub(crate) fn at(value: u32) -> usize {
    usize::try_from(value).expect("a u32 fits a usize on every target blaze runs on")
}

/// `value` as a `u32` count or index, refused past `u32::MAX - 1` (the last value is [`NONE`]).
pub(crate) fn narrow(value: usize) -> Option<u32> {
    u32::try_from(value).ok().filter(|&v| v < NONE)
}
