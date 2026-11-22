//! A path as written, in one home for every crate of the item layer (rule 30): how it begins
//! ([`PathKind`]), its segments ([`Path`]), what a `use` leaf binds ([`Rename`], [`UseForm`])
//! and what an `extern crate` names ([`ExternTarget`]); and the columns a record keeps its
//! paths in, read back as a [`PathView`].

use core::fmt;

use blaze_format::FormatError;
use blaze_id::{ContentId, CrateItemKey, Portable};
use blaze_own::{AVec, Alloc};

use crate::column::{NONE, Strings, Text, at_of, at_of_record, check_text, count, text_of, wide};
use crate::error::BuildError;
use crate::kind::{Range, byte_enum};

/// How a path begins.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum PathKind {
    /// `a::b`: the first segment is looked up lexically (or, in a 2015 `use`, in the crate root).
    Plain,
    /// `crate::a`.
    Crate,
    /// `self::a`.
    SelfMod,
    /// `super::a` (`Super(1)`), `super::super::a` (`Super(2)`), ...
    Super(u8),
    /// `::a`: the extern prelude (2018 on), or the crate root then the extern prelude (2015).
    Abs,
    /// `$crate::a`, with the crate the `$crate` token's hygiene names.
    DollarCrate(CrateItemKey),
}

byte_enum! {
    /// [`PathKind`]'s byte in a record; `super`'s count and `$crate`'s crate are fields beside it.
    pub(crate) enum PathTag, refused as "a byte that is no path start" {
        Plain = 0 => "plain",
        Crate = 1 => "crate",
        SelfMod = 2 => "self",
        Super = 3 => "super",
        Abs = 4 => "::",
        DollarCrate = 5 => "$crate",
    }
}

impl PathKind {
    /// Its bytes as a record holds them: zeros where the kind has no field, so two builders
    /// write equal bytes.
    pub(crate) fn to_record(self) -> (PathTag, u8, ContentId) {
        let zero = ContentId([0; 32]);
        match self {
            PathKind::Plain => (PathTag::Plain, 0, zero),
            PathKind::Crate => (PathTag::Crate, 0, zero),
            PathKind::SelfMod => (PathTag::SelfMod, 0, zero),
            PathKind::Super(depth) => (PathTag::Super, depth, zero),
            PathKind::Abs => (PathTag::Abs, 0, zero),
            PathKind::DollarCrate(krate) => (PathTag::DollarCrate, 0, krate.0),
        }
    }

    /// What [`PathKind::to_record`] wrote.
    pub(crate) fn from_record(tag: PathTag, depth: u8, krate: ContentId) -> PathKind {
        match tag {
            PathTag::Plain => PathKind::Plain,
            PathTag::Crate => PathKind::Crate,
            PathTag::SelfMod => PathKind::SelfMod,
            PathTag::Super => PathKind::Super(depth),
            PathTag::Abs => PathKind::Abs,
            PathTag::DollarCrate => PathKind::DollarCrate(CrateItemKey(krate)),
        }
    }
}

/// A path as written: how it begins and its segments. `N` is a segment's name: `&str` read
/// from a record, an interned symbol or a test's `&'static str` in a working form.
#[derive(Clone, Debug)]
pub struct Path<'a, N> {
    pub kind: PathKind,
    pub segments: AVec<'a, N>,
}

impl<'b, N: PartialEq> PartialEq<Path<'b, N>> for Path<'_, N> {
    fn eq(&self, other: &Path<'b, N>) -> bool {
        self.kind == other.kind && self.segments[..] == other.segments[..]
    }
}

impl<N: Eq> Eq for Path<'_, N> {}

impl<N: Copy> Path<'_, N> {
    /// The same path in `alloc`.
    pub fn copy_in<'b>(&self, alloc: Alloc<'b>) -> Path<'b, N> {
        Path { kind: self.kind, segments: AVec::from_slice_in(&self.segments, alloc) }
    }
}

/// What a `use` leaf binds its target as.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Rename<N> {
    /// `use a::b;`: as its last segment.
    Same,
    /// `use a::b as c;`.
    To(N),
    /// `use a::b as _;`: as nothing nameable (it still brings a trait's methods into scope).
    Underscore,
}

byte_enum! {
    /// [`Rename`]'s byte in a record; the new name is a field beside it.
    pub(crate) enum RenameTag, refused as "a byte that is no rename" {
        Same = 0 => "same",
        To = 1 => "as",
        Underscore = 2 => "as _",
    }
}

impl<N> Rename<N> {
    pub fn as_ref(&self) -> Rename<&N> {
        match self {
            Rename::Same => Rename::Same,
            Rename::To(name) => Rename::To(name),
            Rename::Underscore => Rename::Underscore,
        }
    }

    pub fn map<M>(self, f: impl FnOnce(N) -> M) -> Rename<M> {
        match self {
            Rename::Same => Rename::Same,
            Rename::To(name) => Rename::To(f(name)),
            Rename::Underscore => Rename::Underscore,
        }
    }

    pub(crate) fn split(self) -> (RenameTag, Option<N>) {
        match self {
            Rename::Same => (RenameTag::Same, None),
            Rename::To(name) => (RenameTag::To, Some(name)),
            Rename::Underscore => (RenameTag::Underscore, None),
        }
    }

    pub(crate) fn join(tag: RenameTag, name: impl FnOnce() -> N) -> Rename<N> {
        match tag {
            RenameTag::Same => Rename::Same,
            RenameTag::To => Rename::To(name()),
            RenameTag::Underscore => Rename::Underscore,
        }
    }
}

/// What one leaf of a `use` tree imports: a name, or every name (a glob). `use a::{b, c::*}`
/// is two leaves.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum UseForm<N> {
    /// `use a::b` (`types_only` for `use a::{self}`, which imports only the module).
    Named { rename: Rename<N>, types_only: bool },
    /// `use a::*`.
    Glob,
}

/// Which crate an `extern crate` names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ExternTarget<N> {
    /// A dependency by its name.
    Dep(N),
    /// `extern crate self as name;`.
    SelfCrate,
}

blaze_format::record! {
    /// One path of a record: its start, and its segments, a range of the record's segment
    /// column.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct PathRow: 1 {
        pub(crate) segments: Range,
        pub(crate) krate: ContentId,
        pub(crate) kind: PathTag,
        pub(crate) depth: u8,
        pub(crate) pad: u16,
    }
}

/// A record's paths while it is written: rows and their segments, in the order pushed. The
/// writer pushes each path where the record's order reaches it, so the bytes are a function
/// of the value written.
pub(crate) struct PathTable<'a> {
    pub(crate) rows: AVec<'a, PathRow>,
    pub(crate) segments: AVec<'a, Text>,
}

impl<'a> PathTable<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> PathTable<'a> {
        PathTable { rows: AVec::new_in(alloc), segments: AVec::new_in(alloc) }
    }

    /// Pushes `path`, its segments' text into `strings`; its row's index.
    pub(crate) fn push<N: AsRef<str>>(
        &mut self,
        path: &Path<'_, N>,
        strings: &mut Strings<'_>,
        table: &'static str,
    ) -> Result<u32, BuildError> {
        let index = count(self.rows.len(), table)?;
        let start = count(self.segments.len(), table)?;
        for segment in path.segments.iter() {
            self.segments.push(strings.push(segment.as_ref(), table)?);
        }
        let end = count(self.segments.len(), table)?;
        let (kind, depth, krate) = path.kind.to_record();
        self.rows.push(PathRow { segments: Range::new(start, end), krate, kind, depth, pad: 0 });
        Ok(index)
    }

    /// [`PathTable::push`] of an optional path; [`NONE`] for none.
    pub(crate) fn push_opt<N: AsRef<str>>(
        &mut self,
        path: Option<&Path<'_, N>>,
        strings: &mut Strings<'_>,
        table: &'static str,
    ) -> Result<u32, BuildError> {
        match path {
            Some(path) => self.push(path, strings, table),
            None => Ok(NONE),
        }
    }
}

/// A record's paths read in place.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Paths<'a> {
    rows: &'a [PathRow],
    segments: &'a [Text],
    strings: &'a str,
}

impl<'a> Paths<'a> {
    /// The paths of a record, checked once: every segment's text, every row's range inside
    /// the segment column, a depth only on `super` and a crate only on `$crate`.
    pub(crate) fn read(
        rows: &'a [PathRow],
        segments: &'a [Text],
        strings: &'a str,
        buffer: &[u8],
    ) -> Result<Paths<'a>, FormatError> {
        for text in segments {
            check_text(strings, *text, text, buffer)?;
        }
        for row in rows {
            let inside = row.segments.start <= row.segments.end
                && wide(row.segments.end) <= segments.len();
            let canonical = row.pad == 0
                && (row.kind == PathTag::Super || row.depth == 0)
                && (row.kind == PathTag::DollarCrate || row.krate == ContentId([0; 32]));
            if !inside || !canonical {
                return Err(FormatError::Invalid {
                    at: at_of_record(row, buffer),
                    what: "a path past its record's segments, or with a field its start has not",
                });
            }
        }
        Ok(Paths { rows, segments, strings })
    }

    /// The paths of a record [`Paths::read`] accepted before in this process, with no check
    /// (the in-place views' `trusted`, D114 rule 1).
    pub(crate) fn trusted(rows: &'a [PathRow], segments: &'a [Text], strings: &'a str) -> Paths<'a> {
        Paths { rows, segments, strings }
    }

    /// Whether `index` is a path of the record, or [`NONE`] when `optional`.
    pub(crate) fn check<T: blaze_format::Record>(
        &self,
        index: u32,
        optional: bool,
        row: &T,
        buffer: &[u8],
    ) -> Result<(), FormatError> {
        match (index == NONE && optional) || wide(index) < self.rows.len() {
            true => Ok(()),
            false => Err(FormatError::Invalid { at: at_of_record(row, buffer), what: "a path no row of its record is" }),
        }
    }

    /// The path at `index`, which [`Paths::check`] accepted.
    pub(crate) fn get(&self, index: u32) -> PathView<'a> {
        let row = &self.rows[wide(index)];
        PathView {
            kind: PathKind::from_record(row.kind, row.depth, row.krate),
            segments: &self.segments[row.segments.to_usize()],
            strings: self.strings,
        }
    }
}

/// A path of a record, read in place.
#[derive(Clone, Copy)]
pub struct PathView<'a> {
    kind: PathKind,
    segments: &'a [Text],
    strings: &'a str,
}

impl<'a> PathView<'a> {
    pub fn kind(&self) -> PathKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Its segments, in order.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.segments.iter().map(move |text| text_of(strings, *text))
    }

    /// The path as a working form, its segments borrowed from the record, in `alloc`.
    pub fn to_path_in<'b>(&self, alloc: Alloc<'b>) -> Path<'b, &'a str> {
        let mut segments = AVec::with_capacity_in(self.segments.len(), alloc);
        segments.extend(self.segments());
        Path { kind: self.kind, segments }
    }
}

impl PartialEq for PathView<'_> {
    /// Two paths read from records are equal when they begin alike and have the same segments'
    /// text, whatever record they are in.
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.segments().eq(other.segments())
    }
}

impl Eq for PathView<'_> {}

impl fmt::Debug for PathView<'_> {
    /// `Crate::a::b`: its start, then each segment.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)?;
        for segment in self.segments() {
            write!(f, "::{segment}")?;
        }
        Ok(())
    }
}
