//! A desugared representation of paths like `crate::foo` or `<Type as Trait>::bar`.
//!
//! A store keeps its paths as columns ([`PathColumns`], `docs/v2/plan/design/data.md`), held by
//! its type columns: a `u8` tag and a `u32` data word per path, the words of the variants that
//! need more in `extra`, and their generic arguments, associated-type bindings and bounds as
//! rows of plain records with `(start, len)` runs. [`Path`], [`GenericArgs`] and
//! [`AssociatedTypeBinding`] are `Copy` views decoded from them on the fly.
//!
//! | path | data | `extra` |
//! |---|---|---|
//! | `BarePath(mod_path)` | `mod_paths` t | |
//! | `LangItem(target, name)` | e | `[lang_items t, name+1 (names t)]` |
//! | `Normal` | e | `[mod_paths t, type_anchor+1, slots start, slots len]` |
//!
//! A `Normal` path's slots are one word per segment: 0 for a segment without generic
//! arguments, `generic_args` t + 1 for one with. The records:
//!
//! | record | words |
//! |---|---|
//! | [`GenericArgsRow`] | `[args start, args len (args), bindings start, bindings len (bindings), has_self_type \| parenthesized << 1]` |
//! | [`BindingRow`] | `[name (names t), args+1 (generic_args t), type_ref+1, bounds start, bounds len (bounds)]` |
//! | [`Names`], [`UseArgs`] | `[start, len]` into `names`, `use_args` |
//!
//! `e` is the index in `extra` of the path's words, `t` an index into the table named, `x+1` an
//! optional value plus one (0 for `None`). A binding's arguments are always an earlier row than
//! the arguments it is bound in ([`PathColumns::check`]), so walking a view ends.

use std::{
    fmt,
    hash::{Hash, Hasher},
    iter,
};

use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use la_arena::Idx;

use crate::{
    expr_store::{ExpressionStore, ExpressionStoreBuilder, ListMark},
    hir::{
        ExprId, PatId,
        columns::{Checked, below, entry, idx, len, list, raw, take_exact},
    },
    lang_item::LangItemTarget,
    type_ref::{ConstRef, LifetimeRefId, PathId, TypeBound, TypeRefId, UseArgRef},
};

/// A path of a store's [`PathColumns`], read with [`ExpressionStore::path_at`]: what an
/// `asm!` `sym` operand holds.
pub type PathIdx = Idx<Path<'static>>;
/// A row of generic arguments of a store's [`PathColumns`].
pub(crate) type GenericArgsIdx = Idx<GenericArgs<'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericArgsParentheses {
    No,
    /// Bounds of the form `Type::method(..): Send` or `impl Trait<method(..): Send>`,
    /// aka. Return Type Notation or RTN.
    ReturnTypeNotation,
    /// `Fn`-family parenthesized traits, e.g. `impl Fn(u32) -> String`.
    ///
    /// This is desugared into one generic argument containing a tuple of all arguments,
    /// and an associated type binding for `Output` for the return type.
    ParenSugar,
}

const PARENTHESES: [GenericArgsParentheses; 3] = [
    GenericArgsParentheses::No,
    GenericArgsParentheses::ReturnTypeNotation,
    GenericArgsParentheses::ParenSugar,
];

/// A single generic argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenericArg {
    Type(TypeRefId),
    Lifetime(LifetimeRefId),
    Const(ConstRef),
}

/// A path of a store, decoded from its [`PathColumns`], or a bare path borrowed from a local
/// [`ModPath`] ([`Path::from_known_path_with_no_generic`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Path<'a> {
    /// `BarePath` is used when the path has neither generics nor type anchor, since the vast
    /// majority of paths are in this category. When the path has either generics or type
    /// anchor, it is `Path::Normal` with a slot per segment, empty where a segment has none.
    BarePath(&'a ModPath),
    /// `Path::Normal` will always have either generics or type anchor.
    Normal(NormalPath<'a>),
    /// A link to a lang item. It is used in desugaring of things like `it?`. We can show these
    /// links via a normal path since they might be private and not accessible in the usage place.
    LangItem(LangItemTarget, Option<&'a Name>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NormalPath<'a> {
    pub generic_args: PathGenericArgs<'a>,
    pub type_anchor: Option<TypeRefId>,
    pub mod_path: &'a ModPath,
}

/// The generic arguments of a path's segments, one optional [`GenericArgs`] per segment.
#[derive(Clone, Copy)]
pub struct PathGenericArgs<'a> {
    columns: &'a PathColumns,
    slots: &'a [u32],
}

/// Generic arguments to a path segment (e.g. the `i32` in `Option<i32>`). This
/// also includes bindings of associated types, like in `Iterator<Item = Foo>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GenericArgs<'a> {
    pub args: &'a [GenericArg],
    /// This specifies whether the args contain a Self type as the first
    /// element. This is the case for path segments like `<T as Trait>`, where
    /// `T` is actually a type parameter for the path `Trait` specifying the
    /// Self type. Otherwise, when we have a path `Trait<X, Y>`, the Self type
    /// is left out.
    pub has_self_type: bool,
    /// Associated type bindings like in `Iterator<Item = T>`.
    pub bindings: Bindings<'a>,
    /// Whether these generic args were written with parentheses and how.
    pub parenthesized: GenericArgsParentheses,
}

/// The associated type bindings of one [`GenericArgs`].
#[derive(Clone, Copy)]
pub struct Bindings<'a> {
    columns: &'a PathColumns,
    rows: &'a [BindingRow],
}

/// An associated type binding like in `Iterator<Item = T>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AssociatedTypeBinding<'a> {
    /// The name of the associated type.
    pub name: &'a Name,
    /// The generic arguments to the associated type. e.g. For `Trait<Assoc<'a, T> = &'a T>`, this
    /// would be `['a, T]`.
    pub args: Option<GenericArgs<'a>>,
    /// The type bound to this associated type (in `Item = T`, this would be the
    /// `T`). This can be `None` if there are bounds instead.
    pub type_ref: Option<TypeRefId>,
    /// Bounds for the associated type, like in `Iterator<Item:
    /// SomeOtherTrait>`. (This is the unstable `associated_type_bounds`
    /// feature.)
    pub bounds: &'a [TypeBound],
}

/// A run of names in a store's [`PathColumns`]: a bound's `for<'a>` binder. Read with
/// [`ExpressionStore::names`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Names {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

/// A run of `use<..>` arguments in a store's [`PathColumns`]. Read with
/// [`ExpressionStore::use_args`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UseArgs {
    pub(crate) start: u32,
    pub(crate) len: u32,
}

impl Names {
    pub const EMPTY: Names = Names { start: 0, len: 0 };

    #[inline]
    pub fn len(self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl UseArgs {
    #[inline]
    pub fn len(self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// One [`GenericArgs`]: its runs of arguments and bindings, and its flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct GenericArgsRow {
    pub(crate) args: [u32; 2],
    pub(crate) bindings: [u32; 2],
    /// Bit 0 `has_self_type`, bits 1 and 2 the index of `parenthesized` in [`PARENTHESES`].
    pub(crate) flags: u32,
}

/// One [`AssociatedTypeBinding`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct BindingRow {
    pub(crate) name: u32,
    pub(crate) args: u32,
    pub(crate) type_ref: u32,
    pub(crate) bounds: [u32; 2],
}

/// Which variant of [`Path`] a path is, in variant order.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PathTag {
    Bare,
    Normal,
    LangItem,
}

impl PathTag {
    /// Every tag, at the index of its value.
    pub(crate) const ALL: [PathTag; 3] = [PathTag::Bare, PathTag::Normal, PathTag::LangItem];
}

/// Every column of a store's paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)] // clone: in `TypeColumns`; path.rs tests copy it
pub(crate) struct PathColumns {
    /// [`PathTag`], by path.
    pub(crate) kind: Vec<u8>,
    pub(crate) data: Vec<u32>,
    pub(crate) extra: Vec<u32>,
    pub(crate) mod_paths: Vec<ModPath>,
    pub(crate) lang_items: Vec<LangItemTarget>,
    /// The per-segment generic arguments of every `Normal` path.
    pub(crate) slots: Vec<u32>,
    pub(crate) generic_args: Vec<GenericArgsRow>,
    /// The arguments of every [`GenericArgsRow`].
    pub(crate) args: Vec<GenericArg>,
    pub(crate) bindings: Vec<BindingRow>,
    /// The bounds of every [`BindingRow`].
    pub(crate) bounds: Vec<TypeBound>,
    /// Binding names, lang item names and `for<..>` binders.
    pub(crate) names: Vec<Name>,
    pub(crate) use_args: Vec<UseArgRef>,
}

/// The bytes of the columns of one path (`kind` and `data`); `extra`, the slots and the rows
/// are counted apart (2 words of `extra` for `LangItem`, 4 for `Normal`).
pub(crate) const PATH_BYTES: usize = size_of::<u8>() + size_of::<u32>();

const _: () = {
    assert!(PATH_BYTES <= 12);
    assert!(size_of::<PathIdx>() == size_of::<u32>());
    assert!(size_of::<GenericArgsRow>() == 20);
    assert!(size_of::<BindingRow>() == 20);
    assert!(size_of::<GenericArg>() == 8);
    assert!(size_of::<Names>() == 8);
    assert!(size_of::<UseArgs>() == 8);
    let mut i = 0;
    while i < PathTag::ALL.len() {
        assert!(PathTag::ALL[i] as usize == i);
        i += 1;
    }
    let mut i = 0;
    while i < PARENTHESES.len() {
        assert!(PARENTHESES[i] as usize == i);
        i += 1;
    }
};

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<ModPath>() == 16);
    assert!(size_of::<Name>() == 4);
    assert!(size_of::<TypeBound>() == 16);
};

const PATH_ID: &str = "a path id past its columns";
const PATH_KIND: &str = "a path kind out of range";
const EXTRA: &str = "a path's words past the extra column";
const MOD_PATH: &str = "a mod path index past its table";
const LANG_ITEM: &str = "a lang item index past its table";
const NAME: &str = "a name index past its table";
const SLOTS: &str = "a path's slots past their array";
const SLOT: &str = "a slot naming generic arguments past their rows";
const GENERIC_ARGS: &str = "a generic arguments index past its rows";
const FLAGS: &str = "generic arguments flags out of range";
const ARGS: &str = "generic arguments past their array";
const BINDINGS: &str = "bindings past their rows";
const BINDING_ARGS: &str = "a binding's arguments not an earlier row";
const BOUNDS: &str = "a binding's bounds past their array";
const NAMES: &str = "a name run past its table";
const USE_ARGS: &str = "a use<..> run past its table";
const TYPE_ID: &str = "a type id past its arena";
const LIFETIME_ID: &str = "a lifetime id past its arena";
const EXPR_ID: &str = "an expression id past its arena";

#[inline]
fn opt<T>(id: Option<Idx<T>>) -> u32 {
    id.map_or(0, |id| plus_one(raw(id)))
}

#[inline]
fn unopt<T>(word: u32) -> Option<Idx<T>> {
    word.checked_sub(1).map(idx)
}

#[inline]
fn plus_one(value: u32) -> u32 {
    value.checked_add(1).expect("an index too large for its word")
}

#[inline]
fn flags(has_self_type: bool, parenthesized: GenericArgsParentheses) -> u32 {
    has_self_type as u32 | (parenthesized as u32) << 1
}

#[inline]
fn parenthesized(flags: u32) -> Checked<GenericArgsParentheses> {
    PARENTHESES.get((flags >> 1) as usize).copied().ok_or(FLAGS)
}

#[cold]
#[inline(never)]
#[track_caller]
fn bad_paths(what: &str, at: u32) -> ! {
    panic!("path column bug at {at}: {what}")
}

/// Appends `value`, as its index.
fn push_one<T>(table: &mut Vec<T>, value: T) -> u32 {
    let at = len(table.len());
    table.push(value);
    at
}

/// Appends `items`, as the `[start, len]` of the run they take.
fn extend<T>(array: &mut Vec<T>, items: impl IntoIterator<Item = T>) -> [u32; 2] {
    let start = array.len();
    array.extend(items);
    [len(start), len(array.len() - start)]
}

/// `column` in a `Vec` of exactly its length, leaving `column` empty with its capacity.
fn exact<T>(column: &mut Vec<T>) -> Vec<T> {
    take_exact(column).into_vec()
}

impl PathColumns {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.kind.len()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.kind.is_empty()
    }

    /// The path `id`. Columns are checked when they are read ([`PathColumns::check`]) and made
    /// by the pushes alone, so one that doesn't decode is a bug in the store.
    #[inline]
    pub(crate) fn get(&self, id: PathIdx) -> Path<'_> {
        match self.decode(id) {
            Ok(path) => path,
            Err(what) => bad_paths(what, raw(id)),
        }
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (PathIdx, Path<'_>)> + '_ {
        (0..len(self.len())).map(move |at| (idx(at), self.get(idx(at))))
    }

    /// The generic arguments in row `id`.
    #[inline]
    pub(crate) fn generic_args_at(&self, id: GenericArgsIdx) -> GenericArgs<'_> {
        match self.decode_args(raw(id)) {
            Ok(args) => args,
            Err(what) => bad_paths(what, raw(id)),
        }
    }

    #[inline]
    pub(crate) fn names(&self, names: Names) -> &[Name] {
        match list(&self.names, names.start, names.len, NAMES) {
            Ok(names) => names,
            Err(what) => bad_paths(what, names.start),
        }
    }

    #[inline]
    pub(crate) fn use_args(&self, args: UseArgs) -> &[UseArgRef] {
        match list(&self.use_args, args.start, args.len, USE_ARGS) {
            Ok(args) => args,
            Err(what) => bad_paths(what, args.start),
        }
    }

    #[inline]
    fn words<const N: usize>(&self, at: u32) -> Checked<[u32; N]> {
        let at = at as usize;
        self.extra
            .get(at..at.wrapping_add(N))
            .and_then(|words| <[u32; N]>::try_from(words).ok())
            .ok_or(EXTRA)
    }

    #[inline]
    pub(crate) fn decode(&self, id: PathIdx) -> Checked<Path<'_>> {
        let at = raw(id) as usize;
        let (Some(&kind), Some(&d)) = (self.kind.get(at), self.data.get(at)) else {
            return Err(PATH_ID);
        };
        Ok(match *PathTag::ALL.get(kind as usize).ok_or(PATH_KIND)? {
            PathTag::Bare => Path::BarePath(entry(&self.mod_paths, d, MOD_PATH)?),
            PathTag::LangItem => {
                let [target, name] = self.words(d)?;
                let name = match name.checked_sub(1) {
                    Some(at) => Some(entry(&self.names, at, NAME)?),
                    None => None,
                };
                Path::LangItem(*entry(&self.lang_items, target, LANG_ITEM)?, name)
            }
            PathTag::Normal => {
                let [mod_path, type_anchor, start, count] = self.words(d)?;
                Path::Normal(NormalPath {
                    generic_args: PathGenericArgs {
                        columns: self,
                        slots: list(&self.slots, start, count, SLOTS)?,
                    },
                    type_anchor: unopt(type_anchor),
                    mod_path: entry(&self.mod_paths, mod_path, MOD_PATH)?,
                })
            }
        })
    }

    /// The arguments a [`slot`] word names, `None` for 0: how an expression keeps a method
    /// call's turbofish.
    #[inline]
    pub(crate) fn decode_slot(&self, word: u32) -> Checked<Option<GenericArgs<'_>>> {
        word.checked_sub(1).map(|at| self.decode_args(at)).transpose()
    }

    #[inline]
    pub(crate) fn decode_args(&self, at: u32) -> Checked<GenericArgs<'_>> {
        let row = entry(&self.generic_args, at, GENERIC_ARGS)?;
        Ok(GenericArgs {
            args: list(&self.args, row.args[0], row.args[1], ARGS)?,
            has_self_type: row.flags & 1 == 1,
            bindings: Bindings {
                columns: self,
                rows: list(&self.bindings, row.bindings[0], row.bindings[1], BINDINGS)?,
            },
            parenthesized: parenthesized(row.flags)?,
        })
    }

    #[inline]
    fn decode_binding(&self, row: &BindingRow) -> Checked<AssociatedTypeBinding<'_>> {
        Ok(AssociatedTypeBinding {
            name: entry(&self.names, row.name, NAME)?,
            args: match row.args.checked_sub(1) {
                Some(at) => Some(self.decode_args(at)?),
                None => None,
            },
            type_ref: unopt(row.type_ref),
            bounds: list(&self.bounds, row.bounds[0], row.bounds[1], BOUNDS)?,
        })
    }

    fn node(&mut self, tag: PathTag, data: u32) -> PathIdx {
        let id = idx(len(self.kind.len()));
        self.kind.push(tag as u8);
        self.data.push(data);
        id
    }

    fn push_words<const N: usize>(&mut self, words: [u32; N]) -> u32 {
        let at = len(self.extra.len());
        self.extra.extend_from_slice(&words);
        at
    }

    pub(crate) fn push_bare(&mut self, mod_path: ModPath) -> PathIdx {
        let data = push_one(&mut self.mod_paths, mod_path);
        self.node(PathTag::Bare, data)
    }

    pub(crate) fn push_lang_item(&mut self, target: LangItemTarget, name: Option<Name>) -> PathIdx {
        let target = push_one(&mut self.lang_items, target);
        let name = name.map_or(0, |name| plus_one(push_one(&mut self.names, name)));
        let data = self.push_words([target, name]);
        self.node(PathTag::LangItem, data)
    }

    /// A `Normal` path, with one slot per segment ([`slot`]).
    pub(crate) fn push_normal(
        &mut self,
        mod_path: ModPath,
        type_anchor: Option<TypeRefId>,
        slots: impl IntoIterator<Item = u32>,
    ) -> PathIdx {
        let [start, count] = extend(&mut self.slots, slots);
        let mod_path = push_one(&mut self.mod_paths, mod_path);
        let data = self.push_words([mod_path, opt(type_anchor), start, count]);
        self.node(PathTag::Normal, data)
    }

    /// A row of generic arguments. `bindings` are rows made by [`Self::binding`], whose nested
    /// arguments are pushed before this row is.
    pub(crate) fn push_generic_args(
        &mut self,
        args: impl IntoIterator<Item = GenericArg>,
        has_self_type: bool,
        bindings: impl IntoIterator<Item = BindingRow>,
        parenthesized: GenericArgsParentheses,
    ) -> GenericArgsIdx {
        let args = extend(&mut self.args, args);
        let bindings = extend(&mut self.bindings, bindings);
        let row = GenericArgsRow { args, bindings, flags: flags(has_self_type, parenthesized) };
        idx(push_one(&mut self.generic_args, row))
    }

    /// The row of a binding, its name and bounds pushed; the caller collects the rows of one
    /// [`GenericArgs`] and pushes them with it.
    pub(crate) fn binding(
        &mut self,
        name: Name,
        args: Option<GenericArgsIdx>,
        type_ref: Option<TypeRefId>,
        bounds: impl IntoIterator<Item = TypeBound>,
    ) -> BindingRow {
        let name = push_one(&mut self.names, name);
        let bounds = extend(&mut self.bounds, bounds);
        BindingRow { name, args: opt(args), type_ref: opt(type_ref), bounds }
    }

    /// `args` (or none) with `self_type` put first and `has_self_type` set, for `<T as Trait>`:
    /// a new row sharing the bindings of the old one.
    pub(crate) fn with_self_type(
        &mut self,
        args: Option<GenericArgsIdx>,
        self_type: TypeRefId,
    ) -> GenericArgsIdx {
        let (old, bindings, flags_of) = match args {
            Some(at) => {
                let row = self.generic_args[raw(at) as usize];
                (row.args, row.bindings, row.flags)
            }
            None => ([0, 0], [0, 0], flags(false, GenericArgsParentheses::No)),
        };
        let start = len(self.args.len());
        self.args.push(GenericArg::Type(self_type));
        let (from, count) = (old[0] as usize, old[1] as usize);
        self.args.extend_from_within(from..from + count);
        let row = GenericArgsRow { args: [start, plus_one(old[1])], bindings, flags: flags_of | 1 };
        idx(push_one(&mut self.generic_args, row))
    }

    pub(crate) fn push_names(&mut self, names: impl IntoIterator<Item = Name>) -> Names {
        let [start, len] = extend(&mut self.names, names);
        Names { start, len }
    }

    pub(crate) fn push_use_args(&mut self, args: impl IntoIterator<Item = UseArgRef>) -> UseArgs {
        let [start, len] = extend(&mut self.use_args, args);
        UseArgs { start, len }
    }

    /// `path`, copied from the columns it was read from (never these: a view can't borrow the
    /// columns it is pushed into) with its rows and names. Ids are copied as they are.
    pub(crate) fn push_view(&mut self, path: Path<'_>) -> PathIdx {
        match path {
            Path::BarePath(mod_path) => self.push_bare(*mod_path),
            Path::LangItem(target, name) => self.push_lang_item(target, name.cloned()),
            Path::Normal(NormalPath { generic_args, type_anchor, mod_path }) => {
                let start = self.slots.len();
                for args in generic_args {
                    let word = args.map_or(0, |args| plus_one(raw(self.push_args_view(args))));
                    self.slots.push(word);
                }
                let count = len(self.slots.len() - start);
                let mod_path = push_one(&mut self.mod_paths, *mod_path);
                let data = self.push_words([mod_path, opt(type_anchor), len(start), count]);
                self.node(PathTag::Normal, data)
            }
        }
    }

    /// `args`, copied from other columns as [`Self::push_view`] copies a path.
    pub(crate) fn push_args_view(&mut self, args: GenericArgs<'_>) -> GenericArgsIdx {
        let from = args.bindings.columns;
        let first = self.bindings.len();
        self.bindings.resize(first + args.bindings.len(), BindingRow::default());
        for (at, binding) in args.bindings.iter().enumerate() {
            let nested = binding.args.map(|args| self.push_args_view(args));
            let bounds = binding.bounds.iter().map(|&bound| self.copy_bound(from, bound));
            // alloc: copying a view across stores is a test and repair path, never lowering's.
            let bounds: Vec<TypeBound> = bounds.collect();
            // clone: the name moves to these columns' table; a test and repair path.
            let row = self.binding(binding.name.clone(), nested, binding.type_ref, bounds);
            self.bindings[first + at] = row;
        }
        let args_run = extend(&mut self.args, args.args.iter().copied());
        let row = GenericArgsRow {
            args: args_run,
            bindings: [len(first), len(args.bindings.len())],
            flags: flags(args.has_self_type, args.parenthesized),
        };
        idx(push_one(&mut self.generic_args, row))
    }

    /// `bound`, its runs copied out of `from`.
    pub(crate) fn copy_bound(&mut self, from: &PathColumns, bound: TypeBound) -> TypeBound {
        match bound {
            TypeBound::ForLifetime(names, path) => {
                TypeBound::ForLifetime(self.push_names(from.names(names).iter().cloned()), path)
            }
            TypeBound::Use(args) => TypeBound::Use(self.push_use_args(from.use_args(args).iter().cloned())),
            TypeBound::Path(..) | TypeBound::Lifetime(_) | TypeBound::Error => bound,
        }
    }

    pub(crate) fn clear(&mut self) {
        let PathColumns {
            kind,
            data,
            extra,
            mod_paths,
            lang_items,
            slots,
            generic_args,
            args,
            bindings,
            bounds,
            names,
            use_args,
        } = self;
        kind.clear();
        data.clear();
        extra.clear();
        mod_paths.clear();
        lang_items.clear();
        slots.clear();
        generic_args.clear();
        args.clear();
        bindings.clear();
        bounds.clear();
        names.clear();
        use_args.clear();
    }

    /// The elements across the columns that grow with a store, for the scratch pool's cap.
    pub(crate) fn capacity(&self) -> usize {
        self.kind.capacity()
            + self.extra.capacity()
            + self.slots.capacity()
            + self.generic_args.capacity()
            + self.args.capacity()
            + self.names.capacity()
    }

    /// The columns of a finished store, each copied into a `Vec` of exactly its length (one
    /// allocation per non-empty column); `self` is left empty with its capacity.
    pub(crate) fn freeze_into(&mut self) -> PathColumns {
        PathColumns {
            kind: exact(&mut self.kind),
            data: exact(&mut self.data),
            extra: exact(&mut self.extra),
            mod_paths: exact(&mut self.mod_paths),
            lang_items: exact(&mut self.lang_items),
            slots: exact(&mut self.slots),
            generic_args: exact(&mut self.generic_args),
            args: exact(&mut self.args),
            bindings: exact(&mut self.bindings),
            bounds: exact(&mut self.bounds),
            names: exact(&mut self.names),
            use_args: exact(&mut self.use_args),
        }
    }

    /// Every tag in range, every word, run and table index in bounds, every id below its
    /// arena's count, every binding's arguments an earlier row than its own, and every bound's
    /// [`PathId`] accepted by `bound_path` (the type columns', which know their paths). The
    /// encoding calls it, through [`crate::hir::type_ref::TypeColumns::check`], after reading
    /// a buffer.
    pub(crate) fn check(
        &self,
        type_refs: usize,
        lifetimes: usize,
        exprs: usize,
        bound_path: impl Fn(PathId) -> Checked,
    ) -> Checked {
        if self.kind.len() != self.data.len() {
            return Err("path kind and data columns of different lengths");
        }
        if u32::try_from(self.len()).is_err() || u32::try_from(self.generic_args.len()).is_err() {
            return Err("more paths or generic arguments than ids");
        }
        let ty = |id: TypeRefId| below(raw(id), type_refs, TYPE_ID);
        let lifetime = |id: LifetimeRefId| below(raw(id), lifetimes, LIFETIME_ID);
        for at in 0..len(self.len()) {
            if let Path::Normal(NormalPath { generic_args, type_anchor, mod_path: _ }) =
                self.decode(idx(at))?
            {
                type_anchor.map_or(Ok(()), ty)?;
                for &word in generic_args.slots {
                    if word as usize > self.generic_args.len() {
                        return Err(SLOT);
                    }
                }
            }
        }
        for at in 0..len(self.generic_args.len()) {
            let args = self.decode_args(at)?;
            for arg in args.args {
                match *arg {
                    GenericArg::Type(id) => ty(id)?,
                    GenericArg::Lifetime(id) => lifetime(id)?,
                    GenericArg::Const(ConstRef { expr }) => below(raw(expr), exprs, EXPR_ID)?,
                }
            }
            for row in args.bindings.rows {
                if row.args.checked_sub(1).is_some_and(|nested| nested >= at) {
                    return Err(BINDING_ARGS);
                }
                let binding = self.decode_binding(row)?;
                binding.type_ref.map_or(Ok(()), ty)?;
            }
        }
        for bound in &self.bounds {
            self.check_bound(bound, lifetimes, &bound_path)?;
        }
        for arg in &self.use_args {
            if let UseArgRef::Lifetime(id) = *arg {
                lifetime(id)?;
            }
        }
        Ok(())
    }

    /// A word of other columns naming a path of these ([`PathIdx`]) in range.
    #[inline]
    pub(crate) fn check_path_word(&self, word: u32) -> Checked {
        below(word, self.len(), PATH_ID)
    }

    /// A [`slot`] word of other columns in range.
    #[inline]
    pub(crate) fn check_slot_word(&self, word: u32) -> Checked {
        match word as usize <= self.generic_args.len() {
            true => Ok(()),
            false => Err(SLOT),
        }
    }

    /// `names` a run in these columns.
    pub(crate) fn check_names(&self, names: Names) -> Checked {
        list(&self.names, names.start, names.len, NAMES).map(drop)
    }

    /// `bound`'s runs in these columns and its ids in range.
    pub(crate) fn check_bound(
        &self,
        bound: &TypeBound,
        lifetimes: usize,
        bound_path: &impl Fn(PathId) -> Checked,
    ) -> Checked {
        match *bound {
            TypeBound::Path(path, _) => bound_path(path),
            TypeBound::ForLifetime(names, path) => {
                list(&self.names, names.start, names.len, NAMES)?;
                bound_path(path)
            }
            TypeBound::Lifetime(id) => below(raw(id), lifetimes, LIFETIME_ID),
            TypeBound::Use(args) => list(&self.use_args, args.start, args.len, USE_ARGS).map(drop),
            TypeBound::Error => Ok(()),
        }
    }
}

/// The slot word of a segment with `args`: 0 for none, its row plus one.
#[inline]
pub(crate) fn slot(args: Option<GenericArgsIdx>) -> u32 {
    opt(args)
}

/// The arguments a slot word names.
#[inline]
pub(crate) fn unslot(word: u32) -> Option<GenericArgsIdx> {
    unopt(word)
}

impl<'a> Path<'a> {
    /// Converts a known mod path to `Path`.
    #[inline]
    pub fn from_known_path_with_no_generic(path: &'a ModPath) -> Path<'a> {
        Path::BarePath(path)
    }

    #[inline]
    pub fn kind(&self) -> &'a PathKind {
        match *self {
            Path::BarePath(mod_path) => &mod_path.kind,
            Path::Normal(path) => &path.mod_path.kind,
            Path::LangItem(..) => &PathKind::Abs,
        }
    }

    #[inline]
    pub fn type_anchor(&self) -> Option<TypeRefId> {
        match self {
            Path::Normal(path) => path.type_anchor,
            Path::LangItem(..) | Path::BarePath(_) => None,
        }
    }

    #[inline]
    pub fn generic_args(&self) -> Option<PathGenericArgs<'a>> {
        match self {
            Path::Normal(path) => Some(path.generic_args),
            Path::LangItem(..) | Path::BarePath(_) => None,
        }
    }

    pub fn segments(&self) -> PathSegments<'a> {
        match *self {
            Path::BarePath(mod_path) => {
                PathSegments { segments: mod_path.segments(), generic_args: None }
            }
            Path::Normal(path) => PathSegments {
                segments: path.mod_path.segments(),
                generic_args: Some(path.generic_args),
            },
            Path::LangItem(_, seg) => {
                PathSegments { segments: seg.map_or(&[][..], std::slice::from_ref), generic_args: None }
            }
        }
    }

    #[inline]
    pub fn mod_path(&self) -> Option<&'a ModPath> {
        match *self {
            Path::BarePath(mod_path) => Some(mod_path),
            Path::Normal(path) => Some(path.mod_path),
            Path::LangItem(..) => None,
        }
    }

    /// The single segment of a plain path without generic arguments.
    #[inline]
    pub fn as_ident(&self) -> Option<&'a Name> {
        match *self {
            Path::BarePath(mod_path) => mod_path.as_ident(),
            Path::Normal(_) | Path::LangItem(..) => None,
        }
    }

    pub fn qualifier(&self) -> Option<Qualifier<'a>> {
        match *self {
            Path::BarePath(mod_path) => {
                if mod_path.is_ident() {
                    return None;
                }
                Some(Qualifier { mod_path: strip_last(mod_path), type_anchor: None, generic_args: None })
            }
            Path::Normal(path) => {
                if path.mod_path.is_ident() {
                    return None;
                }
                let generic_args = path.generic_args.strip_last();
                let keep = path.type_anchor.is_some() || generic_args.slots.iter().any(|&it| it != 0);
                Some(Qualifier {
                    mod_path: strip_last(path.mod_path),
                    type_anchor: path.type_anchor,
                    generic_args: keep.then_some(generic_args),
                })
            }
            Path::LangItem(..) => None,
        }
    }

    pub fn is_self_type(&self) -> bool {
        match self {
            Path::BarePath(mod_path) => mod_path.is_Self(),
            Path::Normal(path) => {
                path.type_anchor.is_none()
                    && path.mod_path.is_Self()
                    && path.generic_args.slots.iter().all(|&it| it == 0)
            }
            Path::LangItem(..) => false,
        }
    }
}

impl<'a> From<&'a ModPath> for Path<'a> {
    #[inline]
    fn from(mod_path: &'a ModPath) -> Path<'a> {
        Path::BarePath(mod_path)
    }
}

fn strip_last(mod_path: &ModPath) -> ModPath {
    let segments = mod_path.segments();
    ModPath::from_segments(
        mod_path.kind,
        segments.split_last().map_or(&[][..], |it| it.1).iter().cloned(),
    )
}

/// A path's qualifier: a new [`ModPath`] of all but its last segment, and the arguments of
/// those segments borrowed from the store. Read it with [`Qualifier::path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Qualifier<'a> {
    mod_path: ModPath,
    type_anchor: Option<TypeRefId>,
    generic_args: Option<PathGenericArgs<'a>>,
}

impl Qualifier<'_> {
    #[inline]
    pub fn path(&self) -> Path<'_> {
        match self.generic_args {
            Some(generic_args) => Path::Normal(NormalPath {
                generic_args,
                type_anchor: self.type_anchor,
                mod_path: &self.mod_path,
            }),
            None => Path::BarePath(&self.mod_path),
        }
    }

    #[inline]
    pub fn is_self_type(&self) -> bool {
        self.path().is_self_type()
    }
}

impl<'a> PathGenericArgs<'a> {
    #[inline]
    fn with(self, slots: &'a [u32]) -> PathGenericArgs<'a> {
        PathGenericArgs { columns: self.columns, slots }
    }

    #[inline]
    fn arguments(self, word: u32) -> Option<GenericArgs<'a>> {
        word.checked_sub(1).map(|at| self.columns.generic_args_at(idx(at)))
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The arguments of segment `at`: `None` past the last segment, `Some(None)` for a segment
    /// without arguments.
    #[inline]
    pub fn get(&self, at: usize) -> Option<Option<GenericArgs<'a>>> {
        self.slots.get(at).map(|&word| self.arguments(word))
    }

    #[inline]
    pub fn first(&self) -> Option<Option<GenericArgs<'a>>> {
        self.get(0)
    }

    #[inline]
    pub fn last(&self) -> Option<Option<GenericArgs<'a>>> {
        self.get(self.len().checked_sub(1)?)
    }

    /// The slot words ([`slot`]), for lowering to copy a trait path's into a `<T as Trait>`.
    #[inline]
    pub(crate) fn slot_words(&self) -> &'a [u32] {
        self.slots
    }

    /// All but the last segment's.
    #[inline]
    pub fn strip_last(&self) -> PathGenericArgs<'a> {
        self.with(self.slots.split_last().map_or(&[][..], |it| it.1))
    }

    #[inline]
    pub fn iter(&self) -> PathGenericArgsIter<'a> {
        PathGenericArgsIter { args: *self, slots: self.slots.iter() }
    }
}

impl PartialEq for PathGenericArgs<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for PathGenericArgs<'_> {}

impl Hash for PathGenericArgs<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.len());
        self.iter().for_each(|args| args.hash(state));
    }
}

impl fmt::Debug for PathGenericArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The arguments of each segment of a path, in order.
#[derive(Clone)]
pub struct PathGenericArgsIter<'a> {
    args: PathGenericArgs<'a>,
    slots: std::slice::Iter<'a, u32>,
}

impl<'a> Iterator for PathGenericArgsIter<'a> {
    type Item = Option<GenericArgs<'a>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.slots.next().map(|&word| self.args.arguments(word))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.slots.size_hint()
    }
}

impl DoubleEndedIterator for PathGenericArgsIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.slots.next_back().map(|&word| self.args.arguments(word))
    }
}

impl ExactSizeIterator for PathGenericArgsIter<'_> {}

impl<'a> IntoIterator for PathGenericArgs<'a> {
    type Item = Option<GenericArgs<'a>>;
    type IntoIter = PathGenericArgsIter<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &PathGenericArgs<'a> {
    type Item = Option<GenericArgs<'a>>;
    type IntoIter = PathGenericArgsIter<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> Bindings<'a> {
    #[inline]
    fn binding(&self, row: &'a BindingRow) -> AssociatedTypeBinding<'a> {
        match self.columns.decode_binding(row) {
            Ok(binding) => binding,
            Err(what) => bad_paths(what, row.name),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[inline]
    pub fn get(&self, at: usize) -> Option<AssociatedTypeBinding<'a>> {
        self.rows.get(at).map(|row| self.binding(row))
    }

    #[inline]
    pub fn first(&self) -> Option<AssociatedTypeBinding<'a>> {
        self.get(0)
    }

    #[inline]
    pub fn last(&self) -> Option<AssociatedTypeBinding<'a>> {
        self.get(self.len().checked_sub(1)?)
    }

    #[inline]
    pub fn iter(&self) -> BindingsIter<'a> {
        BindingsIter { bindings: *self, rows: self.rows.iter() }
    }

    /// The columns these bindings (and their bounds' runs) are read from.
    #[inline]
    pub(crate) fn columns(&self) -> &'a PathColumns {
        self.columns
    }
}

/// Bindings are equal by what they read: a bound's `for<..>` names and `use<..>` arguments are
/// runs in each side's own columns, so two copies of one binding in different columns hold
/// different runs.
impl PartialEq for Bindings<'_> {
    fn eq(&self, other: &Self) -> bool {
        let bound_eq = |a: &TypeBound, b: &TypeBound| match (*a, *b) {
            (TypeBound::ForLifetime(a_names, a_path), TypeBound::ForLifetime(b_names, b_path)) => {
                a_path == b_path && self.columns.names(a_names) == other.columns.names(b_names)
            }
            (TypeBound::Use(a_args), TypeBound::Use(b_args)) => {
                self.columns.use_args(a_args) == other.columns.use_args(b_args)
            }
            (a, b) => a == b,
        };
        self.len() == other.len()
            && self.iter().zip(other.iter()).all(|(a, b)| {
                a.name == b.name
                    && a.args == b.args
                    && a.type_ref == b.type_ref
                    && a.bounds.len() == b.bounds.len()
                    && a.bounds.iter().zip(b.bounds).all(|(a, b)| bound_eq(a, b))
            })
    }
}

impl Eq for Bindings<'_> {}

impl Hash for Bindings<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.len());
        for binding in self.iter() {
            binding.name.hash(state);
            binding.args.hash(state);
            binding.type_ref.hash(state);
            state.write_usize(binding.bounds.len());
            for &bound in binding.bounds {
                std::mem::discriminant(&bound).hash(state);
                match bound {
                    TypeBound::ForLifetime(names, path) => {
                        self.columns.names(names).hash(state);
                        path.hash(state);
                    }
                    TypeBound::Use(args) => self.columns.use_args(args).hash(state),
                    TypeBound::Path(..) | TypeBound::Lifetime(_) | TypeBound::Error => bound.hash(state),
                }
            }
        }
    }
}

impl fmt::Debug for Bindings<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The bindings of one [`GenericArgs`], in order.
#[derive(Clone)]
pub struct BindingsIter<'a> {
    bindings: Bindings<'a>,
    rows: std::slice::Iter<'a, BindingRow>,
}

impl<'a> Iterator for BindingsIter<'a> {
    type Item = AssociatedTypeBinding<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.rows.next().map(|row| self.bindings.binding(row))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl DoubleEndedIterator for BindingsIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.rows.next_back().map(|row| self.bindings.binding(row))
    }
}

impl ExactSizeIterator for BindingsIter<'_> {}

impl<'a> IntoIterator for Bindings<'a> {
    type Item = AssociatedTypeBinding<'a>;
    type IntoIter = BindingsIter<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &Bindings<'a> {
    type Item = AssociatedTypeBinding<'a>;
    type IntoIter = BindingsIter<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathSegment<'a> {
    pub name: &'a Name,
    pub args_and_bindings: Option<GenericArgs<'a>>,
}

impl PathSegment<'_> {
    pub const MISSING: PathSegment<'static> =
        PathSegment { name: &Name::missing(), args_and_bindings: None };
}

#[derive(Debug, Clone, Copy)]
pub struct PathSegments<'a> {
    segments: &'a [Name],
    generic_args: Option<PathGenericArgs<'a>>,
}

impl<'a> PathSegments<'a> {
    pub const EMPTY: PathSegments<'static> = PathSegments { segments: &[], generic_args: None };
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn len(&self) -> usize {
        self.segments.len()
    }
    pub fn first(&self) -> Option<PathSegment<'a>> {
        self.get(0)
    }
    pub fn last(&self) -> Option<PathSegment<'a>> {
        self.get(self.len().checked_sub(1)?)
    }

    pub fn get(&self, idx: usize) -> Option<PathSegment<'a>> {
        let res = PathSegment {
            name: self.segments.get(idx)?,
            args_and_bindings: self.generic_args.and_then(|it| it.get(idx)?),
        };
        Some(res)
    }

    pub fn skip(&self, len: usize) -> PathSegments<'a> {
        PathSegments {
            segments: self.segments.get(len..).unwrap_or(&[]),
            generic_args: self.generic_args.and_then(|it| Some(it.with(it.slots.get(len..)?))),
        }
    }

    pub fn take(&self, len: usize) -> PathSegments<'a> {
        PathSegments {
            segments: self.segments.get(..len).unwrap_or(self.segments),
            generic_args: self.generic_args.map(|it| it.with(it.slots.get(..len).unwrap_or(it.slots))),
        }
    }

    pub fn strip_last(&self) -> PathSegments<'a> {
        PathSegments {
            segments: self.segments.split_last().map_or(&[], |it| it.1),
            generic_args: self.generic_args.map(|it| it.strip_last()),
        }
    }

    pub fn strip_last_two(&self) -> PathSegments<'a> {
        PathSegments {
            segments: self.segments.get(..self.segments.len().saturating_sub(2)).unwrap_or(&[]),
            generic_args: self
                .generic_args
                .map(|it| it.with(it.slots.get(..it.slots.len().saturating_sub(2)).unwrap_or(&[]))),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = PathSegment<'a>> + use<'a> {
        let generic_args = self.generic_args;
        self.segments.iter().enumerate().map(move |(at, name)| PathSegment {
            name,
            args_and_bindings: generic_args.and_then(|it| it.get(at)?),
        })
    }
}

impl ExpressionStore {
    /// The path a [`PathId`] names (a `TypeRef::Path`).
    #[inline]
    pub fn path(&self, id: PathId) -> Path<'_> {
        self.types.path(id)
    }

    /// The path `id` of this store's columns.
    #[inline]
    pub fn path_at(&self, id: PathIdx) -> Path<'_> {
        self.types.paths.get(id)
    }

    /// The names of a bound's `for<..>` binder or a where predicate's.
    #[inline]
    pub fn names(&self, names: Names) -> &[Name] {
        self.types.paths.names(names)
    }

    /// The arguments of a `use<..>` bound.
    #[inline]
    pub fn use_args(&self, args: UseArgs) -> &[UseArgRef] {
        self.types.paths.use_args(args)
    }
}

impl ExpressionStoreBuilder {
    #[inline]
    pub(crate) fn paths(&self) -> &PathColumns {
        &self.types.paths
    }

    #[inline]
    pub(crate) fn paths_mut(&mut self) -> &mut PathColumns {
        &mut self.types.paths
    }

    /// The path `id` of this builder.
    #[inline]
    pub fn path_at(&self, id: PathIdx) -> Path<'_> {
        self.types.paths.get(id)
    }

    /// A bare path of one segment, `name`.
    #[inline]
    pub(crate) fn push_name_path(&mut self, name: Name) -> PathIdx {
        self.types.paths.push_bare(ModPath::from_segments(PathKind::Plain, iter::once(name)))
    }

    /// `Expr::Path` of the one-segment path `name`.
    #[inline]
    pub(crate) fn alloc_name_path_expr(&mut self, name: Name) -> ExprId {
        let path = self.push_name_path(name);
        self.alloc_path_expr(path)
    }

    /// `Pat::Path` of the one-segment path `name`.
    #[inline]
    pub(crate) fn alloc_name_path_pat(&mut self, name: Name) -> PatId {
        let path = self.push_name_path(name);
        self.alloc_path_pat(path)
    }

    /// A lang item path, `target` or its item `name`.
    #[inline]
    pub(crate) fn push_lang_path(&mut self, target: LangItemTarget, name: Option<Name>) -> PathIdx {
        self.types.paths.push_lang_item(target, name)
    }

    /// The row of an associated type binding whose bounds are on the list stack above `bounds`,
    /// moved off it into the paths' bounds.
    pub(crate) fn binding_with_bounds(
        &mut self,
        name: Name,
        args: Option<GenericArgsIdx>,
        type_ref: Option<TypeRefId>,
        bounds: ListMark,
    ) -> BindingRow {
        self.types.paths.binding(name, args, type_ref, self.lists.bounds.drain(bounds.bounds as usize..))
    }
}

#[cfg(test)]
mod tests {
    use intern::Symbol;
    use la_arena::RawIdx;
    use blaze_query::db::FromId;

    use super::*;
    use crate::{FunctionId, type_ref::TraitBoundModifier};

    fn name(text: &str) -> Name {
        Name::new_symbol_root(Symbol::intern(text))
    }

    fn mod_path(kind: PathKind, texts: &[&str]) -> ModPath {
        ModPath::from_segments(kind, texts.iter().map(|it| name(it)))
    }

    fn t(at: u32) -> TypeRefId {
        idx(at)
    }

    fn lang_item() -> LangItemTarget {
        LangItemTarget::FunctionId(FunctionId::from_id(blaze_query::db::Id::from_u32(5 + (1 << 20))))
    }

    /// Every shape: bare, lang items with and without a name, `Normal` with a type anchor only,
    /// with arguments on some segments, with nested binding arguments and bounds, `<T as
    /// Trait>` and every parenthesization. The ids of each path, in push order.
    fn every_path(columns: &mut PathColumns) -> Vec<PathIdx> {
        let mut paths = vec![
            columns.push_bare(mod_path(PathKind::Plain, &["a"])),
            columns.push_bare(mod_path(PathKind::Crate, &["a", "b"])),
            columns.push_bare(mod_path(PathKind::SELF, &[])),
            columns.push_lang_item(lang_item(), None),
            columns.push_lang_item(lang_item(), Some(name("branch"))),
            columns.push_normal(mod_path(PathKind::Plain, &["Item"]), Some(t(3)), [0]),
        ];

        let inner = columns.push_generic_args(
            [GenericArg::Lifetime(idx(0))],
            false,
            [],
            GenericArgsParentheses::No,
        );
        let output = columns.binding(name("Output"), None, Some(t(1)), []);
        let assoc = columns.binding(
            name("Assoc"),
            Some(inner),
            None,
            [
                TypeBound::Path(PathId::from_type_ref_unchecked(t(0)), TraitBoundModifier::Maybe),
                TypeBound::Lifetime(idx(0)),
                TypeBound::Error,
            ],
        );
        let names = columns.push_names([name("'a"), name("'b")]);
        let use_args = columns.push_use_args([UseArgRef::Name(name("T")), UseArgRef::Lifetime(idx(0))]);
        let with_runs = columns.binding(
            name("Runs"),
            None,
            None,
            [
                TypeBound::ForLifetime(names, PathId::from_type_ref_unchecked(t(0))),
                TypeBound::Use(use_args),
            ],
        );
        let outer = columns.push_generic_args(
            [GenericArg::Type(t(0)), GenericArg::Const(ConstRef { expr: idx(0) })],
            false,
            [output, assoc, with_runs],
            GenericArgsParentheses::No,
        );
        let sugar = columns.push_generic_args(
            [GenericArg::Type(t(2))],
            false,
            [output],
            GenericArgsParentheses::ParenSugar,
        );
        let rtn = columns.push_generic_args([], false, [], GenericArgsParentheses::ReturnTypeNotation);
        paths.push(columns.push_normal(
            mod_path(PathKind::Abs, &["std", "Iter", "f"]),
            None,
            [0, slot(Some(outer)), slot(Some(rtn))],
        ));
        paths.push(columns.push_normal(mod_path(PathKind::Plain, &["Fn"]), None, [slot(Some(sugar))]));
        let self_args = columns.with_self_type(Some(outer), t(4));
        let bare_self = columns.with_self_type(None, t(4));
        paths.push(columns.push_normal(
            mod_path(PathKind::Plain, &["Trait", "Assoc"]),
            None,
            [slot(Some(self_args)), 0],
        ));
        paths.push(columns.push_normal(
            mod_path(PathKind::Plain, &["Trait", "Assoc"]),
            Some(t(2)),
            [slot(Some(bare_self)), 0],
        ));
        paths
    }

    fn filled() -> (PathColumns, Vec<PathIdx>) {
        let mut columns = PathColumns::default();
        let paths = every_path(&mut columns);
        (columns, paths)
    }

    fn check(columns: &PathColumns) -> Checked {
        columns.check(5, 1, 1, |_| Ok(()))
    }

    #[test]
    fn every_shape_reads_back_as_pushed() {
        let (columns, paths) = filled();
        assert_eq!(check(&columns), Ok(()));

        assert_eq!(columns.get(paths[0]), Path::BarePath(&mod_path(PathKind::Plain, &["a"])));
        assert_eq!(columns.get(paths[0]).as_ident(), Some(&name("a")));
        assert_eq!(columns.get(paths[2]).segments().len(), 0);
        assert_eq!(columns.get(paths[3]), Path::LangItem(lang_item(), None));
        assert_eq!(columns.get(paths[4]), Path::LangItem(lang_item(), Some(&name("branch"))));
        assert_eq!(columns.get(paths[4]).segments().first().map(|it| it.name), Some(&name("branch")));

        let anchored = columns.get(paths[5]);
        assert_eq!(anchored.type_anchor(), Some(t(3)));
        assert_eq!(anchored.generic_args().map(|it| it.len()), Some(1));
        assert_eq!(anchored.segments().first().and_then(|it| it.args_and_bindings), None);

        let path = columns.get(paths[6]);
        assert_eq!(path.kind(), &PathKind::Abs);
        let segments: Vec<_> = path.segments().iter().collect();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].args_and_bindings, None);
        let outer = segments[1].args_and_bindings.expect("the second segment has arguments");
        assert_eq!(outer.args, &[GenericArg::Type(t(0)), GenericArg::Const(ConstRef { expr: idx(0) })]);
        assert!(!outer.has_self_type);
        assert_eq!(outer.parenthesized, GenericArgsParentheses::No);
        let bindings: Vec<_> = outer.bindings.iter().collect();
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].name, &name("Output"));
        assert_eq!(bindings[0].type_ref, Some(t(1)));
        assert_eq!(bindings[1].args.map(|it| it.args), Some(&[GenericArg::Lifetime(idx(0))][..]));
        assert_eq!(bindings[1].bounds.len(), 3);
        let TypeBound::ForLifetime(names, _) = bindings[2].bounds[0] else { panic!("a for<..> bound") };
        assert_eq!(columns.names(names), &[name("'a"), name("'b")]);
        let TypeBound::Use(args) = bindings[2].bounds[1] else { panic!("a use<..> bound") };
        assert_eq!(columns.use_args(args)[1], UseArgRef::Lifetime(idx(0)));
        assert_eq!(
            segments[2].args_and_bindings.map(|it| it.parenthesized),
            Some(GenericArgsParentheses::ReturnTypeNotation)
        );
        assert_eq!(path.segments().skip(1).len(), 2);
        assert_eq!(path.segments().skip(1).first().and_then(|it| it.args_and_bindings), Some(outer));
        assert_eq!(path.segments().take(1).last().and_then(|it| it.args_and_bindings), None);
        assert_eq!(path.segments().strip_last_two().len(), 1);

        let qualifier = path.qualifier().expect("a path of three segments has a qualifier");
        let qualifier = qualifier.path();
        assert_eq!(qualifier.mod_path(), Some(&mod_path(PathKind::Abs, &["std", "Iter"])));
        assert_eq!(qualifier.generic_args().and_then(|it| it.last()), Some(Some(outer)));
        let bare = columns.get(paths[1]).qualifier().expect("two segments");
        assert_eq!(bare.path(), Path::BarePath(&mod_path(PathKind::Crate, &["a"])));
        assert_eq!(columns.get(paths[0]).qualifier(), None);

        let sugar = columns.get(paths[7]).segments().last().and_then(|it| it.args_and_bindings);
        assert_eq!(sugar.map(|it| it.parenthesized), Some(GenericArgsParentheses::ParenSugar));

        let self_args = columns.get(paths[8]).generic_args().and_then(|it| it.first()).flatten();
        let self_args = self_args.expect("the trait segment has arguments");
        assert!(self_args.has_self_type);
        assert_eq!(self_args.args[0], GenericArg::Type(t(4)));
        assert_eq!(&self_args.args[1..], outer.args);
        assert_eq!(self_args.bindings, outer.bindings, "the bindings are shared");
        let bare_self = columns.get(paths[9]).generic_args().and_then(|it| it.first()).flatten();
        assert_eq!(bare_self.map(|it| it.args), Some(&[GenericArg::Type(t(4))][..]));
        assert_eq!(columns.get(paths[9]).type_anchor(), Some(t(2)));
        assert!(!columns.get(paths[9]).is_self_type());
    }

    #[test]
    fn a_view_copied_into_other_columns_reads_back_equal() {
        let (from, paths) = filled();
        let mut to = PathColumns::default();
        to.push_bare(mod_path(PathKind::Plain, &["shifts", "every", "index"]));
        let copies: Vec<_> = paths.iter().map(|&id| to.push_view(from.get(id))).collect();
        assert_eq!(check(&to), Ok(()));
        for (&id, &copy) in paths.iter().zip(&copies) {
            assert_eq!(to.get(copy), from.get(id));
        }
    }

    #[test]
    fn frozen_columns_equal_the_built_ones_and_reused_scratch_rebuilds_them() {
        let (mut columns, paths) = filled();
        let built = columns.clone();
        let frozen = columns.freeze_into();
        assert_eq!(frozen, built);
        assert!(columns.is_empty() && columns.generic_args.is_empty(), "freezing empties the scratch");
        assert_eq!(every_path(&mut columns), paths);
        assert_eq!(columns.freeze_into(), frozen);
        assert_eq!(frozen.iter().count(), paths.len());
    }

    #[test]
    fn check_refuses_a_kind_out_of_range() {
        let (mut columns, _) = filled();
        columns.kind[0] = PathTag::ALL.len() as u8;
        assert_eq!(check(&columns), Err(PATH_KIND));
    }

    #[test]
    fn check_refuses_runs_and_indices_past_their_columns() {
        let (columns, paths) = filled();
        let normal = columns.data[raw(paths[6]) as usize] as usize;

        let mut bad = columns.clone();
        bad.extra[normal + 3] = len(columns.slots.len()) + 1;
        assert_eq!(check(&bad), Err(SLOTS));

        let mut bad = columns.clone();
        bad.data[raw(paths[6]) as usize] = len(columns.extra.len()) - 1;
        assert_eq!(check(&bad), Err(EXTRA));

        let mut bad = columns.clone();
        bad.data[raw(paths[0]) as usize] = len(columns.mod_paths.len());
        assert_eq!(check(&bad), Err(MOD_PATH));

        let mut bad = columns.clone();
        let slot = columns.extra[normal + 2] as usize + 1;
        bad.slots[slot] = len(columns.generic_args.len()) + 1;
        assert_eq!(check(&bad), Err(SLOT));

        let mut bad = columns.clone();
        bad.generic_args[0].args = [0, len(columns.args.len()) + 1];
        assert_eq!(check(&bad), Err(ARGS));

        let mut bad = columns.clone();
        bad.generic_args[1].bindings[1] = len(columns.bindings.len()) + 1;
        assert_eq!(check(&bad), Err(BINDINGS));

        let mut bad = columns.clone();
        bad.generic_args[0].flags = 3 << 1;
        assert_eq!(check(&bad), Err(FLAGS));

        let mut bad = columns.clone();
        bad.names.clear();
        assert_eq!(check(&bad), Err(NAME));
    }

    #[test]
    fn check_refuses_ids_past_their_arenas() {
        let (columns, _) = filled();
        assert_eq!(columns.check(4, 1, 1, |_| Ok(())), Err(TYPE_ID));
        assert_eq!(columns.check(5, 0, 1, |_| Ok(())), Err(LIFETIME_ID));
        assert_eq!(columns.check(5, 1, 0, |_| Ok(())), Err(EXPR_ID));
        assert_eq!(columns.check(5, 1, 1, |_| Err("not a path")), Err("not a path"));
    }

    #[test]
    fn check_refuses_a_binding_whose_arguments_are_not_an_earlier_row() {
        let (mut columns, _) = filled();
        let assoc = columns.bindings.iter().position(|it| it.args != 0).expect("a binding with args");
        columns.bindings[assoc].args = len(columns.generic_args.len());
        assert_eq!(check(&columns), Err(BINDING_ARGS));
    }
}
