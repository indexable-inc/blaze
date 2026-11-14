//! What a checked statement takes and gives: the types every layer shares. The checker writes a
//! [`Description`], a driver compares it with the engine's own, and the typed surface builds
//! the parameter record and the row type from it.

#![blaze::sans_io]

use alloc::alloc::Global;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::any::TypeId;

/// How an engine stores a value. SQLite keeps a `Boolean` as an integer 0 or 1; Postgres has
/// `boolean`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Storage {
    Boolean,
    Integer,
    Real,
    Text,
    Blob,
}

/// The Rust type a column, a result or a parameter has: `PersonId`, not only "an integer".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RustTy<'s> {
    /// The type's name as written in the schema's struct, for messages.
    pub name: &'s str,
    pub id: TypeId,
}

impl RustTy<'static> {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn of<T: 'static>(name: &'static str) -> RustTy<'static> {
        RustTy { name, id: TypeId::of::<T>() }
    }
}

impl Storage {
    /// Every storage with the name `cast` writes it by (`cast(x as integer)`), in declaration
    /// order: the one place a storage's SQL name is spelled.
    pub const NAMES: [(Storage, &'static str); 5] = [
        (Storage::Boolean, "boolean"),
        (Storage::Integer, "integer"),
        (Storage::Real, "real"),
        (Storage::Text, "text"),
        (Storage::Blob, "blob"),
    ];

    /// The storage `cast` names by `name`, without regard to ASCII case.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn named(name: &str) -> Option<Storage> {
        Storage::NAMES
            .iter()
            .find(|(_, text)| text.eq_ignore_ascii_case(name))
            .map(|&(storage, _)| storage)
    }

    /// The name `cast` writes this storage by; `None` only if [`Storage::NAMES`] missed it.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn name(self) -> Option<&'static str> {
        Storage::NAMES.iter().find(|&&(storage, _)| storage == self).map(|&(_, text)| text)
    }

    /// The Rust type of a value of this storage that no column gave a type: `bool`, `i64`,
    /// `f64`, `String`, `Vec<u8>`. Only the types' ids are taken; nothing is allocated.
    #[expect(
        blaze::implicit_allocator,
        reason = "names `String`, the type a text column is read into, for its `TypeId`; no \
                  value is made"
    )]
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn plain(self) -> RustTy<'static> {
        match self {
            Storage::Boolean => RustTy::of::<bool>("bool"),
            Storage::Integer => RustTy::of::<i64>("i64"),
            Storage::Real => RustTy::of::<f64>("f64"),
            Storage::Text => RustTy::of::<alloc::string::String>("String"),
            Storage::Blob => RustTy::of::<Vec<u8, Global>>("Vec<u8>"),
        }
    }
}

/// The type of a parameter or a result column. `nullable` is `Option<rust>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ty<'s> {
    pub storage: Storage,
    pub rust: RustTy<'s>,
    pub nullable: bool,
}

/// One named parameter. `name` is without its `$`. `list` is `in $name`: a slice of `ty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param<'s> {
    pub name: &'s str,
    pub ty: Ty<'s>,
    pub list: bool,
}

/// One column of a returned row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultCol<'s> {
    pub name: &'s str,
    pub ty: Ty<'s>,
}

#[derive(Debug)]
pub enum Returns<'s, A: Allocator> {
    /// `select`, or a statement with `returning`: these columns, in this order.
    Rows(Vec<ResultCol<'s>, A>),
    /// How many rows an `insert`, `update` or `delete` changed.
    Count,
}

/// A statement's parameters, each name once in the order it first appears, and what it returns.
#[derive(Debug)]
pub struct Description<'s, A: Allocator> {
    pub params: Vec<Param<'s>, A>,
    pub returns: Returns<'s, A>,
}

/// The engine a statement is rendered for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Backend {
    Sqlite,
    Postgres,
}

/// What binds to one placeholder of a rendered text. `param` indexes [`Description::params`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// The parameter's one value.
    Scalar { param: u32 },
    /// Entry `index` of a list parameter (SQLite: one placeholder per entry).
    ListItem { param: u32, index: u32 },
    /// A list parameter whole, as an array (Postgres: `= any($n)`).
    Array { param: u32 },
}

/// A statement as an engine's text. Placeholder `k` (from 1: SQLite's `?k`, Postgres's `$k`)
/// binds `slots[k - 1]`; a parameter used twice has one placeholder.
#[derive(Debug)]
pub struct Rendered<A: Allocator> {
    /// UTF-8.
    pub text: Vec<u8, A>,
    pub slots: Vec<Slot, A>,
}

/// A value going to a bind or read from a column, borrowed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueRef<'a> {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

impl ValueRef<'_> {
    /// Its storage; `None` for null.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn storage(&self) -> Option<Storage> {
        match self {
            ValueRef::Null => None,
            ValueRef::Boolean(_) => Some(Storage::Boolean),
            ValueRef::Integer(_) => Some(Storage::Integer),
            ValueRef::Real(_) => Some(Storage::Real),
            ValueRef::Text(_) => Some(Storage::Text),
            ValueRef::Blob(_) => Some(Storage::Blob),
        }
    }
}
