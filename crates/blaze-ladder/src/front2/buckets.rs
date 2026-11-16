//! Impl selection by need: `World::impls_of(trait, shape)` and `World::inherent(shape, name)`
//! lower only the impls that can answer them.
//!
//! [`Buckets::new`] reads every impl header of the program's [`Index`] as written, lowering
//! none: the header is parsed, its trait path and the outermost constructor of its self type
//! are resolved through the names library, and the impl is filed under (trait, [`Shape`]).
//! An impl whose self type's head is no constructor read here (a parameter: a blanket impl; an
//! qualified path, `dyn`, a fn pointer, a path that names nothing; an alias is followed to its
//! type) is filed under its trait's "any" list; one whose trait path does not resolve to a
//! trait under "unread" by its self type's shape (or unread "any"), and one whose header does
//! not parse under unread "blind". A trait question's candidates are its trait's and shape's
//! list, the trait's "any" list, unread at its shape, unread "any" and blind; an inherent
//! question's take blind only: a superset of the impls that can apply, so filing never
//! leaves an impl out. The check loop lowers the candidates' headers ([`Buckets::trait_wants`]),
//! then [`Buckets::answer`] keeps those whose lowered header matches, and the answer is kept.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_lower::{View, form as lf, head};
use blaze_scope::items_record::{ImplRow, SelfHead};
use blaze_scope::resolve::Names;
use blaze_surface::ImplFlags;
use blaze_typeck::{AssocItem, AssocKind, ItemKey, Mutbl, Name, PNode, PTy, PTys, Shape};

use super::heads::{Store, lower};
use super::index::Index;
use super::keys::{Keys, checker_of};
use super::source::NamesSource;
use super::types::{PRIMS, children, path_texts};
use crate::resolve::{Kind, Ns, ResolveError};

mod asked;

pub use asked::{Ask, BucketRead, INCOHERENT_FACT, UNFILED_LIST};

/// A candidate impl whose header the store does not hold when the answer is made, or an
/// inherent impl's fn whose signature it does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unfilled {
    Impl(LibKey),
    Fn(ItemKey),
}

impl core::fmt::Display for Unfilled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unfilled::Impl(k) => write!(f, "the candidate impl {k:x} has no header in the store when its question is answered"),
            Unfilled::Fn(k) => write!(f, "the inherent impl fn {k:?} has no signature in the store when its question is answered"),
        }
    }
}

/// The shape a lowered self type is matched by; `None` for a parameter (a blanket impl).
pub(crate) fn shape_of(t: PTys<'_>, p: PTy) -> Option<Shape> {
    Some(match *t.nodes.get(p.0 as usize)? {
        PNode::Bool => Shape::Bool,
        PNode::Char => Shape::Char,
        PNode::Int(i) => Shape::Int(i),
        PNode::Uint(u) => Shape::Uint(u),
        PNode::Float(f) => Shape::Float(f),
        PNode::Str => Shape::Str,
        PNode::Never => Shape::Never,
        PNode::Tuple(l) => Shape::Tuple(l.len),
        PNode::Ref(m, _) => Shape::Ref(m),
        PNode::Ptr(m, _) => Shape::Ptr(m),
        PNode::Array(..) => Shape::Array,
        PNode::Slice(_) => Shape::Slice,
        PNode::Pat(..) => Shape::Pat,
        PNode::Adt(k, _) => Shape::Adt(k),
        PNode::FnDef(k, _) => Shape::FnDef(k),
        PNode::FnPtr(l, _) => Shape::FnPtr(l.len.saturating_sub(1)),
        PNode::Opaque(k, _) => Shape::Opaque(k),
        PNode::Closure { .. } | PNode::Future(..) => Shape::Closure,
        // A const generic's value is no type: never an impl's self type.
        PNode::Param(_) | PNode::Alias { .. } | PNode::Infer | PNode::Error | PNode::ConstValue { .. } | PNode::ConstParam(_) => return None,
        PNode::Dynamic(l) => Shape::Dynamic(blaze_typeck::object_principal(t, l)),
        PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => return None,
    })
}

/// Whether an impl whose self type has shape `of` (`None`: a blanket) answers a question about
/// `asked`.
fn fits(of: Option<Shape>, asked: Shape) -> bool {
    match (of, asked) {
        (None, _) => true,
        (Some(Shape::Int(_) | Shape::Uint(_)), Shape::AnyInt) | (Some(Shape::Float(_)), Shape::AnyFloat) => true,
        (Some(s), a) => s == a,
    }
}

/// The shape of a primitive type's name.
fn prim_shape(name: &str) -> Option<Shape> {
    let (_, n) = PRIMS.iter().find(|(p, _)| *p == name)?;
    Some(match *n {
        PNode::Bool => Shape::Bool,
        PNode::Char => Shape::Char,
        PNode::Int(i) => Shape::Int(i),
        PNode::Uint(u) => Shape::Uint(u),
        PNode::Float(f) => Shape::Float(f),
        PNode::Str => Shape::Str,
        _ => return None,
    })
}

/// Where a header as written files its impl.
enum Filed {
    /// Its trait, its self type's shape, and whether the trait is written with no arguments.
    Trait(ItemKey, Option<Shape>, bool),
    Inherent(Option<Shape>),
    /// Unread, with its self type's constructor where that was read: a trait impl whose trait
    /// path fails is still only a candidate at its self type's shape.
    Unread(Unread, Option<Shape>),
    /// Its self type is a builtin the checker has no type for (blaze's `i80`): no question
    /// the checker asks is at it, so it is filed nowhere (counted).
    NoCheckerType,
    /// A negative impl `impl !Trait for T` (the trait's key): never a candidate of a goal
    /// `T: Trait`, which no impl can then meet (coherence keeps a positive impl from
    /// overlapping it); for an auto trait it decides that the auto rule does not apply, which
    /// the checker does not read yet: the caller makes it a gap of such a question.
    Negative(ItemKey),
}

/// Why an impl's header could not be filed by its trait: each impl filed so is a candidate of
/// every question, so each class is a cost to remove at its home.
#[derive(Clone, Debug)]
pub enum Unread {
    /// The names library holds no text for the impl.
    NoText,
    /// The header did not lower.
    Unlowered,
    /// Its generic parameters were refused (`generic_names`).
    Generics,
    /// The trait as written is no path.
    TraitNotAPath,
    /// The trait path's segments could not be read.
    PathUnread,
    /// The names library's answer for the trait path, in the impl's module.
    Resolve { path: Box<str>, why: ResolveError },
    /// The path named something other than a trait, or named only a prefix.
    NotATrait { path: Box<str>, kind: Kind, consumed: u32, written: u32 },
}

impl Unread {
    /// Whether the impl is known to be a trait impl (its trait path was written; only the
    /// path failed).
    fn is_trait_impl(&self) -> bool {
        matches!(self, Unread::TraitNotAPath | Unread::PathUnread | Unread::Resolve { .. } | Unread::NotATrait { .. })
    }

    /// The class, for counting: the variant's name, and for a library refusal its reason.
    pub fn class(&self) -> String {
        match self {
            Unread::Resolve { why: ResolveError::Path(e), .. } => format!("Resolve({:?})", e.why),
            Unread::Resolve { why: ResolveError::NoText { .. }, .. } => "Resolve(NoText)".to_string(),
            Unread::NotATrait { kind, consumed, written, .. } if consumed == written => format!("NotATrait({kind:?})"),
            Unread::NotATrait { .. } => "NotATrait(a prefix only)".to_string(),
            other => format!("{other:?}"),
        }
    }
}

/// The outermost constructor of the written type `t`, read without lowering it: `None` where
/// it is no constructor read here (the impl is then a candidate for every shape).
/// A type alias names the aliased type (the Reference, "Type aliases"), so an impl on one is
/// filed by the aliased type's constructor.
/// `Err` where it is a builtin the checker has no type for (blaze's `i80`): no question the
/// checker asks is at that type.
fn written_shape<'n, S: NamesSource<'n>>(names: S, module: LibKey, params: &[(Name, u32)], v: &View<'_>, t: lf::TypeId) -> Result<Option<Shape>, NoCheckerType> {
    match shape_step(names, module, params, v, t) {
        None => Ok(None),
        Some(Written::Shape(s)) => Ok(Some(s)),
        Some(Written::NoCheckerType) => Err(NoCheckerType),
        Some(Written::Alias(k)) => alias_shape(names, k),
    }
}

/// A self type that is a builtin the checker has no type for.
struct NoCheckerType;

/// The most aliases one chain is followed through (`type A = B; type B = C;`); past it the
/// impl is filed for every shape, which is never a wrong answer.
const MOST_ALIAS_HOPS: u32 = 64;

/// The constructor of the type alias `alias` stands for, following aliases of aliases as a
/// loop; `None` where the aliased type is no constructor read here (a parameter, `dyn`).
fn alias_shape<'n, S: NamesSource<'n>>(names: S, mut alias: LibKey) -> Result<Option<Shape>, NoCheckerType> {
    for _ in 0..MOST_ALIAS_HOPS {
        let Some(step) = alias_step(names, alias) else { return Ok(None) };
        match step {
            Written::Shape(s) => return Ok(Some(s)),
            Written::NoCheckerType => return Err(NoCheckerType),
            Written::Alias(k) => alias = k,
        }
    }
    Ok(None)
}

/// What the type alias `alias` stands for, one step.
fn alias_step<'n, S: NamesSource<'n>>(names: S, alias: LibKey) -> Option<Written> {
    let text = names.text_of(alias)?.text;
    let module = names.home_of(alias)?;
    let low = lower(names, alias, text).ok()?;
    let v = low.view();
    let mut params = Vec::new_in(Global);
    super::heads::param_names(&v, 0, &mut params, super::heads::Consts::Skip).ok()?;
    let t = v.head_opt(head::TY)?;
    shape_step(names, module, &params, &v, lf::TypeId(t))
}

/// What one written type is: a constructor, a path to a type alias to read next, or a builtin
/// the checker has no type for.
enum Written {
    Shape(Shape),
    Alias(LibKey),
    NoCheckerType,
}

fn shape_step<'n, S: NamesSource<'n>>(names: S, module: LibKey, params: &[(Name, u32)], v: &View<'_>, t: lf::TypeId) -> Option<Written> {
    let mut kids = Vec::new_in(Global);
    let shape = match v.ty(t).ok()? {
        lf::TypeRef::Ref { mutbl, .. } => Some(Shape::Ref(mutbl)),
        lf::TypeRef::Ptr { mutbl, .. } => Some(Shape::Ptr(mutbl)),
        lf::TypeRef::Slice(_) => Some(Shape::Slice),
        lf::TypeRef::Array { .. } => Some(Shape::Array),
        lf::TypeRef::Never => Some(Shape::Never),
        lf::TypeRef::Tuple(_) => {
            children(v, t, &mut kids).ok()?;
            Some(Shape::Tuple(u32::try_from(kids.len()).ok()?))
        }
        lf::TypeRef::Path(p) => {
            let (abs, texts, segs, _) = path_texts(v, p).ok()?;
            if segs.first()?.kind == lf::SegKind::Anchor {
                return None;
            }
            if !abs && texts.len() == 1 && params.iter().any(|(n, _)| *n == Name::of(texts[0])) {
                return None;
            }
            match (crate::resolve::Paths { names }).path(module, abs, &texts, Ns::Type) {
                Ok(f) if f.consumed as usize == texts.len() && matches!(f.kind, Kind::Struct | Kind::Enum | Kind::Union) => Some(Shape::Adt(checker_of(f.key))),
                Ok(f) if f.consumed as usize == texts.len() => match f.kind {
                    Kind::Builtin(name) => prim_shape(name),
                    Kind::TypeAlias => return Some(Written::Alias(f.key)),
                    Kind::WideInt { .. } => return Some(Written::NoCheckerType),
                    _ => None,
                },
                Ok(_) => None,
                Err(_) => None,
            }
        }
        _ => None,
    };
    shape.map(Written::Shape)
}

fn file<'n, S: NamesSource<'n>>(names: S, key: LibKey, module: LibKey) -> Filed {
    let Some(it) = names.text_of(key) else { return Filed::Unread(Unread::NoText, None) };
    let Ok(low) = lower(names, key, it.text) else { return Filed::Unread(Unread::Unlowered, None) };
    let v = low.view();
    let mut params = Vec::new_in(Global);
    if super::heads::param_names(&v, 0, &mut params, super::heads::Consts::Skip).is_err() {
        return Filed::Unread(Unread::Generics, None);
    }
    let Ok(self_shape) = written_shape(names, module, &params, &v, lf::TypeId(v.head[head::SELF_TY])) else { return Filed::NoCheckerType };
    let Some(t) = v.head_opt(head::TRAIT) else { return Filed::Inherent(self_shape) };
    let Ok(lf::TypeRef::Path(p)) = v.ty(lf::TypeId(t)) else { return Filed::Unread(Unread::TraitNotAPath, self_shape) };
    let Ok((abs, texts, _, _)) = path_texts(&v, p) else { return Filed::Unread(Unread::PathUnread, self_shape) };
    let path: Box<str> = format!("{}{}", if abs { "::" } else { "" }, texts.join("::")).into_boxed_str();
    // Its trait written with no generic arguments ([`Homes::bare`]).
    let mut args = Vec::new_in(Global);
    let bare = children(&v, lf::TypeId(t), &mut args).is_ok() && args.is_empty();
    match (crate::resolve::Paths { names }).path(module, abs, &texts, Ns::Type) {
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() && v.head[head::FLAGS] & sig::NEGATIVE != 0 => Filed::Negative(checker_of(f.key)),
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() => Filed::Trait(checker_of(f.key), self_shape, bare),
        Ok(f) => Filed::Unread(Unread::NotATrait { path, kind: f.kind, consumed: f.consumed, written: texts.len() as u32 }, self_shape),
        Err(why) => Filed::Unread(Unread::Resolve { path, why }, self_shape),
    }
}

/// The trait an impl's header names BY ITS TOKENS ([`blaze_file_items::impl_header`], the one
/// reader of a header's text): `Some` for `impl Tr<..> for X`, `None` for `impl X` and for a
/// header whose text does not read at all. An impl has this kind before anything of it is
/// lowered or resolved (the Reference, "Implementations": "Inherent implementations" are `impl`
/// followed by a type, "Trait implementations" are written `impl Trait for Type`), so a header
/// that is refused, not lowered, or lowered without its trait is still known to be a trait
/// impl, and a trait impl is never a candidate of an inherent question.
/// With it, whether the impl is a BLANKET trait impl: its self type as written is one name that
/// is one of its own generic parameters (`impl<T> Tr<..> for T`), read from the same tokens
/// ([`Homes::blanket`] says which questions such an impl can answer).
fn written_trait<'n, S: NamesSource<'n>>(names: S, at: &super::index::ImplAt<'n>) -> (Option<&'n str>, bool) {
    // A crate with no edition (over records: no items record, filed by the source) reads as a
    // header that does not read.
    let Some(edition) = names.edition_of(at.krate) else { return (None, false) };
    let Ok(h) = blaze_file_items::impl_header(at.header, edition, blaze_own::Alloc::global()) else { return (None, false) };
    let blanket = h.trait_.is_some() && h.params.iter().any(|p| *p == h.self_ty);
    (h.trait_, blanket)
}

/// Whether the trait path `trait_text` of a header is written with no type or const argument:
/// no argument list, or one of lifetimes only (`Deserialize<'de>`). Any other list, and one
/// this does not read (a bracket inside it), is not bare: the wide side.
fn bare_text(trait_text: &str) -> bool {
    let t = trait_text.trim();
    let Some(open) = t.find(['<', '(']) else { return true };
    let Some(inside) = t[open..].strip_prefix('<').and_then(|rest| rest.trim_end().strip_suffix('>')) else { return false };
    if inside.contains(['<', '(', '[', '{']) {
        return false;
    }
    // A lifetime starts with `'` and holds no second one (`'a'` is a `char` const argument).
    inside.split(',').all(|a| {
        let a = a.trim();
        a.is_empty() || a.strip_prefix('\'').is_some_and(|rest| !rest.contains('\''))
    })
}

/// Files a trait impl by the trait path of its header's TEXT (`trait_text`, as
/// [`written_trait`] gave it), for an impl whose lowered header did not say which trait it is
/// of: under that trait's "any" list when the names library resolves the path to a trait in the
/// impl's module, else unread with why (a trait impl, so a candidate of trait questions only).
/// Its self type's shape is not read here (`None`: a candidate at every shape of its trait).
fn file_by_text<'n, S: NamesSource<'n>>(names: S, module: LibKey, trait_text: &str) -> Filed {
    let whole = trait_text.trim();
    // The path before its generic arguments (`Tr<..>`) or its parenthesized ones (`Fn(..)`).
    let head = whole.split(['<', '(']).next().unwrap_or("").trim();
    let (abs, rest) = match head.strip_prefix("::") {
        Some(rest) => (true, rest),
        None => (false, head),
    };
    let mut texts: Vec<&str, Global> = Vec::new_in(Global);
    for s in rest.split("::") {
        let s = s.trim();
        let ident = s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident {
            return Filed::Unread(Unread::PathUnread, None);
        }
        texts.push(s);
    }
    let bare = head.len() == whole.len();
    let path: Box<str> = Box::from(head);
    match (crate::resolve::Paths { names }).path(module, abs, &texts, Ns::Type) {
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() => Filed::Trait(checker_of(f.key), None, bare),
        Ok(f) => Filed::Unread(Unread::NotATrait { path, kind: f.kind, consumed: f.consumed, written: texts.len() as u32 }, None),
        Err(why) => Filed::Unread(Unread::Resolve { path, why }, None),
    }
}

/// A path as an impl row holds it (`a::b`, `::a::b`: written, without arguments): whether it
/// is global, and its segments. `None` for a segment that is no plain identifier (`r#a`, a
/// macro's `$crate`): such an impl is filed from its header's text, where the lowering spells
/// the segment.
fn row_path(path: &str) -> Option<(bool, Vec<&str, Global>)> {
    let (abs, rest) = match path.strip_prefix("::") {
        Some(rest) => (true, rest),
        None => (false, path),
    };
    let mut texts: Vec<&str, Global> = Vec::new_in(Global);
    for s in rest.split("::") {
        // An expansion's text writes a path's tokens apart (`:: core :: clone :: Clone`, a
        // derive's impl): the row keeps the path as written.
        let s = s.trim();
        let ident = s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident {
            return None;
        }
        texts.push(s);
    }
    Some((abs, texts))
}

/// What a self type's head path names, as [`file_row`] keeps it per (module, path).
#[derive(Clone, Copy)]
enum SelfAt {
    Shape(Option<Shape>),
    NoCheckerType,
    /// A segment that is no plain identifier: the header's text is read.
    Text,
}

/// What one crate's rows resolved so far ([`file_row`]): the trait of the current run of rows
/// (the rows are sorted by trait path, then module, so each run of equal (trait path, module)
/// is resolved once) and each self path met, by the module it is written in. One per crate
/// filed; dropped when its rows are.
struct RowNames<'n> {
    /// The run's trait path, its module, and the trait it names (`None`: not a trait as
    /// written: each impl of the run is filed from its header's text, which says why).
    run: Option<(&'n str, LibKey, Result<ItemKey, RowMiss>)>,
    selves: HashMap<(LibKey, &'n str), SelfAt, Global>,
}

/// Files an impl by its ROW of the items record ([`ImplRow`]: its trait's path and its self
/// type's head as its header writes them, read from the header's tokens when its crate was
/// solved), so no text is opened to file it: [`file`]'s twin over a row, with whether the impl
/// is a blanket trait impl ([`written_trait`]'s second answer).
///
/// `None`: the row does not hold what files this impl (its header or its trait did not read
/// as written; its self type is a form the row does not name, [`SelfHead::Other`]; a path
/// with a segment that is no plain identifier; a trait path the names do not resolve to a
/// trait). The caller then reads that impl's header text and files it by [`file`], which
/// says why.
///
/// Where a row files an impl differently from [`file`], it is by a fact of the header as
/// written, on the side that fact decides:
/// - the shape is the written self type's head even for a header that does not lower
///   (`file` then files the impl at every shape of its trait: a wider candidate list);
/// - `bare` is "the trait is written with no argument list": a trait written with lifetimes
///   only (`Tr<'a>`) is not bare here, and may be for `file`. Not bare is the wide side
///   (judged by `Homes::all`): no impl is dropped that `file` keeps.
fn file_row<'n, S: NamesSource<'n>>(names: S, row: &ImplRow<'n>, kept: &mut RowNames<'n>) -> Result<(Filed, bool), RowMiss> {
    if row.header_unread {
        return Err(RowMiss::HeaderUnread);
    }
    if row.trait_unread || row.has_trait != row.trait_path.is_some() {
        return Err(RowMiss::TraitUnread);
    }
    let paths = crate::resolve::Paths { names };
    let shape = match row.head {
        SelfHead::Other => return Err(RowMiss::SelfOther),
        // `impl<T> Tr for T`: no constructor (as `shape_step` reads a parameter's name).
        SelfHead::Param => None,
        SelfHead::Ref => Some(Shape::Ref(Mutbl::Not)),
        SelfHead::RefMut => Some(Shape::Ref(Mutbl::Mut)),
        SelfHead::PtrConst => Some(Shape::Ptr(Mutbl::Not)),
        SelfHead::PtrMut => Some(Shape::Ptr(Mutbl::Mut)),
        SelfHead::Slice => Some(Shape::Slice),
        SelfHead::Array => Some(Shape::Array),
        SelfHead::Never => Some(Shape::Never),
        SelfHead::Tuple(n) => Some(Shape::Tuple(u32::from(n))),
        SelfHead::Path => {
            let written = row.self_path.ok_or(RowMiss::SelfPath)?;
            let at = match kept.selves.get(&(row.module, written)).copied() {
                Some(at) => at,
                None => {
                    // The arms of `shape_step`'s path case, over the row's path.
                    let at = match row_path(written) {
                        None => SelfAt::Text,
                        Some((abs, texts)) => match paths.path(row.module, abs, &texts, Ns::Type) {
                            Ok(f) if f.consumed as usize == texts.len() => match f.kind {
                                Kind::Struct | Kind::Enum | Kind::Union => SelfAt::Shape(Some(Shape::Adt(checker_of(f.key)))),
                                Kind::Builtin(name) => SelfAt::Shape(prim_shape(name)),
                                Kind::TypeAlias => match alias_shape(names, f.key) {
                                    Ok(shape) => SelfAt::Shape(shape),
                                    Err(NoCheckerType) => SelfAt::NoCheckerType,
                                },
                                Kind::WideInt { .. } => SelfAt::NoCheckerType,
                                _ => SelfAt::Shape(None),
                            },
                            Ok(_) | Err(_) => SelfAt::Shape(None),
                        },
                    };
                    kept.selves.insert((row.module, written), at);
                    at
                }
            };
            match at {
                SelfAt::Shape(shape) => shape,
                // Before the trait is read, as `file` decides it.
                SelfAt::NoCheckerType => return Ok((Filed::NoCheckerType, false)),
                SelfAt::Text => return Err(RowMiss::SelfPath),
            }
        }
    };
    let Some(trait_path) = row.trait_path else { return Ok((Filed::Inherent(shape), false)) };
    let trait_ = match kept.run {
        Some((path, module, trait_)) if path == trait_path && module == row.module => trait_,
        _ => {
            let trait_ = row_path(trait_path).ok_or(RowMiss::TraitPath).and_then(|(abs, texts)| match paths.path(row.module, abs, &texts, Ns::Type) {
                Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() => Ok(checker_of(f.key)),
                Ok(_) | Err(_) => Err(RowMiss::NoTrait),
            });
            kept.run = Some((trait_path, row.module, trait_));
            trait_
        }
    };
    let trait_ = trait_?;
    let blanket = row.head == SelfHead::Param;
    let negative = row.flags.bits() & ImplFlags::NEGATIVE.bits() != 0;
    Ok((if negative { Filed::Negative(trait_) } else { Filed::Trait(trait_, shape, row.bare) }, blanket))
}

/// Why a row did not file its impl ([`file_row`]): the impl's header text is then read. Each
/// class is counted ([`Buckets::row_misses`], indexed by the class as `usize`): a text opened
/// that a fuller row would not open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMiss {
    /// The header's tokens did not read when the crate was solved.
    HeaderUnread = 0,
    /// A trait impl whose trait is not a plain path as written.
    TraitUnread = 1,
    /// A self type the row names no head for (`SelfHead::Other`: `dyn Tr`, `fn(A) -> B`,
    /// `<T as Tr>::Out`, `(T)`, a macro call).
    SelfOther = 2,
    /// A self type's path with a segment that is no plain identifier (`$crate`, `r#a`).
    SelfPath = 3,
    /// A trait's path with such a segment.
    TraitPath = 4,
    /// A trait path the names do not resolve to a trait in the impl's module.
    NoTrait = 5,
}

/// The classes of [`RowMiss`], in the order of [`Buckets::row_misses`].
pub const ROW_MISSES: [&str; 6] = ["header unread", "trait unread", "self type other", "self path not plain", "trait path not plain", "trait not resolved"];

/// The crates an impl must be written in to answer one question, by the coherence rules of the
/// Reference, "Implementations"; `Any` where they do not narrow it. An impl is filed with its
/// crate without reading its header, so this narrows unread and refused impls too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Home {
    Any,
    /// The trait's crate (or, for an inherent question, the type's), and the crate of the
    /// one local type the goal's self type can be, if any.
    In(usize, Option<usize>),
    /// The crates that may write an inherent impl of a type they do not define (a built-in
    /// type's, `impl u32`): only those whose root turns on `rustc_attrs`
    /// ([`Buckets::may_write_incoherent`]).
    Incoherent,
}

/// Where the impls answering one trait question can be written: `all` for any impl, `bare`
/// for an impl whose trait path is written with no arguments, its trait's parameters all
/// defaulting to `Self` (`impl PartialEq for A`: `Rhs = Self`, so `T1` is `T0` and only `T0`
/// can be the local type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Homes {
    pub all: Home,
    pub bare: Home,
    /// For a BLANKET impl, whose self type is one of its own type parameters (`impl<T>
    /// Tr<A, B> for T`): the Reference, "Orphan rules": "Given `impl<P1..=Pn> Trait<T1..=Tn>
    /// for T0`, an `impl` is valid only if at least one of the following is true: `Trait` is a
    /// local trait; At least one of the types `T0..=Tn` must be a local type. Let `Ti` be the
    /// first such type. No uncovered type parameters `P1..=Pn` may appear in `T0..Ti`
    /// (excluding `Ti`)". Its `T0` is an uncovered type parameter: `T0` is no local type, and
    /// before any later local `Ti` stands `T0`, uncovered. So only "`Trait` is a local trait"
    /// is left: the impl is written in its trait's crate, whatever the goal's self type and
    /// whatever arguments the trait takes. Narrowed by the trait's crate alone.
    pub blanket: Home,
}

impl Homes {
    pub const ANY: Homes = Homes { all: Home::Any, bare: Home::Any, blanket: Home::Any };

    /// An inherent question's: no inherent impl is a blanket impl (`impl<T> T` is refused by
    /// the language, E0118), so a blanket trait impl is judged as any other impl there.
    fn one(h: Home) -> Homes {
        Homes { all: h, bare: h, blanket: h }
    }
}

/// The marks on a nominal type that widen where its impls may be written.
struct Marks {
    /// `#[fundamental]` (`Box`, `Pin`): `Box<Local>` is a local type, so its crate decides
    /// nothing.
    fundamental: bool,
    /// `#[rustc_has_incoherent_inherent_impls]`: the standard library's crates may write its
    /// inherent impls (each fn marked `#[rustc_allow_incoherent_impl]`).
    incoherent: bool,
}

/// The marks of the nominal type `lib`, read from its attributes as written; `None` where they
/// could not be read (the caller then does not narrow).
fn adt_marks<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<Marks> {
    let text = names.text_of(lib)?.text;
    let low = lower(names, lib, text).ok()?;
    let v = low.view();
    let mut m = Marks { fundamental: false, incoherent: false };
    for &n in v.rows(v.list(head::ATTRS)).ok()? {
        let path = v.text_at(v.attr(n).ok()?.path).ok()?.trim();
        m.fundamental |= path == "fundamental";
        m.incoherent |= path == "rustc_has_incoherent_inherent_impls";
    }
    Some(m)
}

/// Where an impl answering `trait_` at `shape` can be written. The Reference, "Trait
/// implementation coherence", "Orphan rules": "Given `impl<P1..=Pn> Trait<T1..=Tn> for T0`, an
/// `impl` is valid only if at least one of the following is true: `Trait` is a local trait; At
/// least one of the types `T0..=Tn` must be a local type. Let `Ti` be the first such type. No
/// uncovered type parameters `P1..=Pn` may appear in `T0..Ti` (excluding `Ti`)" (`&Local`,
/// `&mut Local`, `Box<Local>`, `Pin<Local>` are local too: fundamental). Only the necessary
/// half is used (the uncovered clause narrows further and is not read). So
/// for a trait with no type or const parameter but `Self` (or an impl writing none of them, each
/// defaulting to `Self`: [`Homes::bare`]), an impl in crate C answers `T0: Trait` only if C
/// defines `Trait`, or `T0` is a nominal type of C (or a trait object of a trait of C).
/// Anything else read here (a trait with parameters an impl writes, a reference, a fundamental
/// wrapper, an opaque type, a closure) is `Any`: the conservative side.
pub fn trait_home<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'_>, trait_: ItemKey, shape: Shape) -> Homes {
    let Some(lib) = keys.library_in(names, trait_) else { return Homes::ANY };
    let Some(home) = names.crate_of(lib) else { return Homes::ANY };
    // A blanket impl is written in its trait's crate whatever the goal's self type is
    // ([`Homes::blanket`]): the one narrowing that needs nothing but the trait's crate.
    let wide = Homes { blanket: Home::In(home, None), ..Homes::ANY };
    let local = match shape {
        Shape::Adt(k) => {
            let Some(adt) = keys.library_in(names, k) else { return wide };
            match adt_marks(names, adt) {
                Some(m) if !m.fundamental => {}
                _ => return wide,
            }
            let Some(c) = names.crate_of(adt) else { return wide };
            Some(c)
        }
        Shape::Dynamic(Some(t)) => {
            let Some(c) = keys.library_in(names, t).and_then(|l| names.crate_of(l)) else { return wide };
            Some(c)
        }
        // Never a local type of any crate (no fundamental type is built in but references).
        Shape::Bool | Shape::Char | Shape::Int(_) | Shape::Uint(_) | Shape::Float(_) | Shape::AnyInt | Shape::AnyFloat | Shape::Str | Shape::Never | Shape::Tuple(_) | Shape::Slice | Shape::Array | Shape::Ptr(_) | Shape::FnPtr(_) => None,
        _ => return wide,
    };
    let by_self = Home::In(home, local);
    // Its type and const parameters, `Self` included, and whether each after `Self` defaults
    // to `Self`: from its head, else (the count only) as written.
    let (generics, self_defaults) = match store.maps.traits.get(&lib) {
        Some(t) => {
            let view = store.tys.view();
            let all_self = t.defaults.iter().all(|d| d.is_some_and(|p| matches!(view.nodes.get(p.0 as usize), Some(PNode::Param(0)))));
            (Some(t.generics), all_self)
        }
        None => {
            let n = names.text_of(lib).and_then(|it| lower(names, lib, it.text).ok()).and_then(|low| {
                let mut ps = Vec::new_in(Global);
                super::heads::param_names(&low.view(), 1, &mut ps, super::heads::Consts::Keep).ok()
            });
            // `param_names` counts the WRITTEN parameters (from index 1: `Self` is 0 and is
            // not written); `generics` counts `Self` too, as the head's `t.generics` does.
            // Without the `+ 1` a trait with no parameter but `Self` (`Clone`) read 0 here,
            // fell to `wide` below, and its question admitted EVERY crate whenever the store
            // did not hold the trait's head yet (measured 2026-11-16, front2_records: `bool:
            // Clone` alone over a fresh reader opened 70 crates and filed 28,719 impls).
            (n.map(|written| written + 1), false)
        }
    };
    match generics {
        Some(1) => Homes { all: by_self, bare: by_self, ..wide },
        Some(_) if self_defaults => Homes { bare: by_self, ..wide },
        _ => wide,
    }
}

/// Where an inherent impl of `shape` can be written. The Reference, "Implementations",
/// "Inherent implementations": "An implementing type must be defined within the same crate as
/// the original type definition." rustc's E0116: "An inherent implementation was defined for a
/// type outside the current crate"; E0390: "A method or constant was implemented on a primitive
/// type". The exceptions are the standard library's: core's `#![rustc_coherence_is_core]` and
/// alloc's and std's `#[rustc_allow_incoherent_impl]` (on a built-in type, or a type or trait
/// marked `#[rustc_has_incoherent_inherent_impls]`): each is an internal attribute a crate can
/// write only with `#![feature(rustc_attrs)]` at its root ([`Home::Incoherent`]).
pub fn inherent_home<'n, S: NamesSource<'n>>(names: S, keys: &Keys, shape: Shape) -> Homes {
    let nominal = match shape {
        Shape::Adt(k) | Shape::Dynamic(Some(k)) => k,
        Shape::Bool | Shape::Char | Shape::Int(_) | Shape::Uint(_) | Shape::Float(_) | Shape::AnyInt | Shape::AnyFloat | Shape::Str | Shape::Never | Shape::Tuple(_) | Shape::Slice | Shape::Array | Shape::Ptr(_) | Shape::Ref(_) | Shape::FnPtr(_) => {
            return Homes::one(Home::Incoherent);
        }
        _ => return Homes::ANY,
    };
    let Some(lib) = keys.library_in(names, nominal) else { return Homes::ANY };
    match adt_marks(names, lib) {
        Some(m) if !m.incoherent => {}
        Some(_) => return Homes::one(Home::Incoherent),
        None => return Homes::ANY,
    }
    match names.crate_of(lib) {
        Some(c) => Homes::one(Home::In(c, None)),
        None => Homes::ANY,
    }
}

fn push<K: core::hash::Hash + Eq, V>(m: &mut HashMap<K, Vec<V, Global>, Global>, k: K, v: V) {
    m.entry(k).or_insert_with(|| Vec::new_in(Global)).push(v);
}

/// The impls filed by their headers as written, and the answers made so far.
pub struct Buckets {
    by_shape: HashMap<(ItemKey, Shape), Vec<LibKey, Global>, Global>,
    any: HashMap<ItemKey, Vec<LibKey, Global>, Global>,
    /// Every unread impl, sorted ([`Buckets::is_unread`]).
    unread: Vec<LibKey, Global>,
    /// Trait impls whose trait path failed, by their self type's shape where it was read
    /// (then also under `AnyInt`/`AnyFloat`), else in `unread_any`: candidates of trait
    /// questions at that shape only, never of inherent ones.
    unread_by_shape: HashMap<Shape, Vec<LibKey, Global>, Global>,
    unread_any: Vec<LibKey, Global>,
    /// Impls not read far enough to tell trait from inherent (no text, not lowered, generics
    /// refused): candidates of every question.
    unread_blind: Vec<LibKey, Global>,
    inherent_by_shape: HashMap<Shape, Vec<LibKey, Global>, Global>,
    inherent_any: Vec<LibKey, Global>,
    answers: HashMap<(ItemKey, Shape), Vec<ItemKey, Global>, Global>,
    /// Per answered trait question, the least-keyed candidate whose header failed: the
    /// question's gap. Only a question that HAS such a candidate is a gap of the body asking it,
    /// so a body's outcome never depends on which other bodies or rounds lowered what first.
    answer_failed: HashMap<(ItemKey, Shape), LibKey, Global>,
    inherent: HashMap<(Shape, Name), Vec<AssocItem, Global>, Global>,
    /// The answered inherent shapes, each with its least-keyed failed candidate, if any.
    inherent_done: HashMap<Shape, Option<LibKey>, Global>,
    /// How many impl headers the questions made the store lower.
    pub lowered: u32,
    /// Impls on a self type the checker has no type for (blaze's `i80`), filed nowhere.
    pub no_checker_type: u32,
    /// The unread candidates whose header failed, with why (sorted by key): left out of the
    /// answers; a gap of each body that asks a question they are candidates of.
    pub failed: Vec<(LibKey, Box<str>), Global>,
    /// Why each unread impl's trait path was not filed (in filing order).
    pub unread_why: Vec<(LibKey, Unread), Global>,
    /// The inherent fns, by shape and name, whose signatures are in `Store::fn_gaps`.
    gapped: HashMap<(Shape, Name), ItemKey, Global>,
    /// The negative impls, by trait ([`Filed::Negative`]): no trait question's candidates.
    negative: HashMap<ItemKey, Vec<LibKey, Global>, Global>,
    /// Each impl's crate, by key (the index's order): known without reading its header.
    krates: Vec<(LibKey, usize), Global>,
    /// The home each answered trait question was narrowed to ([`trait_home`]).
    homes: HashMap<(ItemKey, Shape), Homes, Global>,
    /// The trait impls whose trait path is written with no generic arguments, sorted.
    bare: Vec<LibKey, Global>,
    /// Per crate: whether its root turns on `rustc_attrs` ([`Home::Incoherent`]).
    incoherent: Vec<bool, Global>,
    /// The trait impls filed by their header's text ([`file_by_text`]) because the lowered
    /// header did not name their trait, in the index's order: each is a defect of the lowering
    /// or of the block's module to remove at its home, counted here so it is seen.
    pub by_text: Vec<LibKey, Global>,
    /// The blanket trait impls (`impl<T> Tr<..> for T`, by their header's tokens:
    /// [`written_trait`]), read or unread, sorted: each is judged by [`Homes::blanket`], so an
    /// unread one is a candidate only of questions about a trait of its own crate.
    blanket: Vec<LibKey, Global>,
    /// Over [`Buckets::lazy`]: per crate, whether its impls were filed ([`Buckets::admit`]).
    /// Empty over [`Buckets::new`], which files every crate's.
    lazy_filed: Vec<bool, Global>,
    /// Over [`Buckets::lazy`]: how many impls [`Buckets::admit`] filed from their rows of the
    /// items record, no text opened ([`file_row`]).
    pub filed_from_rows: u32,
    /// Over [`Buckets::lazy`]: how many impls [`Buckets::admit`] filed from their header's
    /// text (every impl of a source with no impl rows; else those a row does not file).
    pub filed_from_text: u32,
    /// Why each impl a row did not file was read from text, counted by class ([`RowMiss`] as
    /// `usize`; [`ROW_MISSES`] names the classes).
    pub row_misses: [u32; 6],
    /// A test's removed cause ([`Buckets::plant_no_rows`]): every impl is filed from its
    /// header's text, as before impls were filed from rows.
    no_rows: bool,
    /// Over a source with a bucket index: which buckets are filed and what each question read
    /// ([`Buckets::admit_asked`]).
    indexed: asked::Indexed,
}

impl Buckets {
    /// No impl filed, no crate known.
    fn empty() -> Buckets {
        Buckets {
            by_shape: HashMap::new_in(Global),
            any: HashMap::new_in(Global),
            unread: Vec::new_in(Global),
            unread_by_shape: HashMap::new_in(Global),
            unread_any: Vec::new_in(Global),
            unread_blind: Vec::new_in(Global),
            inherent_by_shape: HashMap::new_in(Global),
            inherent_any: Vec::new_in(Global),
            answers: HashMap::new_in(Global),
            answer_failed: HashMap::new_in(Global),
            inherent: HashMap::new_in(Global),
            inherent_done: HashMap::new_in(Global),
            lowered: 0,
            no_checker_type: 0,
            failed: Vec::new_in(Global),
            unread_why: Vec::new_in(Global),
            negative: HashMap::new_in(Global),
            gapped: HashMap::new_in(Global),
            krates: Vec::new_in(Global),
            homes: HashMap::new_in(Global),
            bare: Vec::new_in(Global),
            incoherent: Vec::new_in(Global),
            by_text: Vec::new_in(Global),
            blanket: Vec::new_in(Global),
            lazy_filed: Vec::new_in(Global),
            filed_from_rows: 0,
            filed_from_text: 0,
            row_misses: [0; 6],
            no_rows: false,
            indexed: asked::Indexed::new(),
        }
    }

    /// THE CAUSE REMOVED, for tests/front2_records.rs only (rule 23): from now on
    /// [`Buckets::admit`] files every impl from its header's text, never from its row, so the
    /// test can count the texts a candidates question opens without the rows.
    #[doc(hidden)]
    pub fn plant_no_rows(&mut self) {
        self.no_rows = true;
    }

    /// Files every impl of `index` by its header as written.
    pub fn new(names: &Names<'_>, index: &Index<'_>) -> Buckets {
        let mut b = Buckets::empty();
        b.krates.extend(index.impls.iter().map(|at| (at.key, at.krate)));
        b.krates.sort_unstable_by_key(|k| k.0);
        // `#![feature(rustc_attrs)]` is an inner attribute of the crate root, the first file
        // read; a mention anywhere in that text keeps the crate on the wide side.
        b.incoherent.extend(names.crates().iter().map(|c| c.files.first().is_none_or(|f| f.text.contains("rustc_attrs") || f.text.contains("rustc_coherence_is_core"))));
        b.file_all(names, &index.impls);
        b.sort_filed();
        b
    }

    /// The front end over records: no impl is filed when this is made. Before each question,
    /// [`Buckets::admit`] files the impls of the crates the question's home admits, from the
    /// source's impl rows ([`NamesSource::each_impl`]); a crate is filed once.
    pub fn lazy<'n, S: NamesSource<'n>>(names: S) -> Buckets {
        let mut b = Buckets::empty();
        let crates = names.crate_count();
        // Unknown until asked ([`Buckets::admit`]): the wide side, as `admits` reads a crate
        // with no row.
        b.incoherent.resize(crates, true);
        b.lazy_filed.resize(crates, false);
        b
    }

    /// Whether this was made by [`Buckets::lazy`] (a caller then calls [`Buckets::admit`]
    /// before each question; over [`Buckets::new`] every impl is filed already).
    pub fn is_lazy(&self) -> bool {
        !self.lazy_filed.is_empty()
    }

    /// Files the impls of every crate `home` admits that is not filed yet (nothing over
    /// [`Buckets::new`]). The crates a question could involve are the ones its home names: the
    /// trait's crate and the self type's (`trait_home`), the type's (`inherent_home`); a home
    /// that does not narrow (`Home::Any`) files every crate, and `Home::Incoherent` first asks
    /// each crate whether it may write incoherent impls (its items record's header).
    /// Returns how many crates this call filed.
    pub fn admit<'n, S: NamesSource<'n>>(&mut self, names: S, home: Homes) -> usize {
        if !self.is_lazy() {
            return 0;
        }
        // Over a bucket index no crate is filed whole: the question's own buckets are filed
        // by `Buckets::admit_asked`, which the caller calls next with the question.
        if names.has_buckets() && !self.no_rows {
            return 0;
        }
        // Every field: a new kind of home is a compile error here, not a crate left unfiled.
        let Homes { all, bare, blanket } = home;
        let mut filed = 0;
        for krate in 0..self.lazy_filed.len() {
            if self.lazy_filed[krate] {
                continue;
            }
            if [all, bare, blanket].contains(&Home::Incoherent) {
                // Not answered (no record: the source filed it): the wide side.
                self.incoherent[krate] = names.may_write_incoherent(krate).unwrap_or(true);
            }
            if !(self.admits(all, krate) || self.admits(bare, krate) || self.admits(blanket, krate)) {
                continue;
            }
            self.lazy_filed[krate] = true;
            filed += 1;
            // A proc-macro unit's impls answer no question of the program's (`Index::new`).
            if names.proc_macro(krate) == Some(true) {
                continue;
            }
            // The impls whose header text is read: every impl of a source with no impl rows,
            // else only those a row does not file.
            let mut impls: Vec<super::index::ImplAt<'n>, Global> = Vec::new_in(Global);
            let mut rows: Vec<ImplRow<'n>, Global> = Vec::new_in(Global);
            if !self.no_rows && names.each_impl_row(krate, &mut |row| rows.push(row)) {
                // In the record's order (trait path, module, key): `file_row` resolves each run
                // of equal (trait path, module) once. Every list a row lands in is sorted
                // before it is read (`sort_filed`, the candidates), so the order filed in is
                // not seen.
                let mut kept = RowNames { run: None, selves: HashMap::new_in(Global) };
                self.krates.extend(rows.iter().map(|row| (row.key, krate)));
                for row in rows.iter() {
                    match file_row(names, row, &mut kept) {
                        Ok((filed, blanket)) => {
                            if blanket {
                                self.blanket.push(row.key);
                            }
                            self.put(row.key, filed, false);
                            self.filed_from_rows += 1;
                        }
                        Err(miss) => {
                            self.row_misses[miss as usize] += 1;
                            match names.impl_with_header(krate, row.key, row.module) {
                                Some(at) => impls.push(at),
                                // Its text did not read (the source filed why): unread, a
                                // candidate of every question, as `file` files an impl with
                                // no text.
                                None => self.put(row.key, Filed::Unread(Unread::NoText, None), false),
                            }
                        }
                    }
                }
            } else {
                names.each_impl(krate, &mut |at| impls.push(at));
                self.krates.extend(impls.iter().map(|at| (at.key, at.krate)));
            }
            // By key: the order impls are filed in is a function of the records.
            impls.sort_by_key(|at| at.key);
            self.filed_from_text += impls.len() as u32;
            self.file_all(names, &impls);
        }
        if filed != 0 {
            self.krates.sort_unstable_by_key(|k| k.0);
            self.sort_filed();
        }
        filed
    }

    /// Files each impl of `impls` by its header as written (the lists [`Buckets::sort_filed`]
    /// sorts are left unsorted).
    fn file_all<'n, S: NamesSource<'n>>(&mut self, names: S, impls: &[super::index::ImplAt<'n>]) {
        let b = self;
        for at in impls.iter() {
            // The kind as written decides first: an impl whose text names a trait is filed as
            // that trait's even when its lowered header holds no trait (`Filed::Inherent`) or
            // did not lower (`NoText`, `Unlowered`, `Generics`): it is then never a candidate
            // of an inherent question. Only an impl whose header text does not read stays blind.
            let (written, blanket) = written_trait(names, at);
            if blanket {
                b.blanket.push(at.key);
            }
            let filed = match (file(names, at.key, at.module), written) {
                (Filed::Inherent(_), Some(t)) => {
                    b.by_text.push(at.key);
                    file_by_text(names, at.module, t)
                }
                (Filed::Unread(why, _), Some(t)) if !why.is_trait_impl() => {
                    b.by_text.push(at.key);
                    file_by_text(names, at.module, t)
                }
                (f, _) => f,
            };
            b.put(at.key, filed, written.is_some_and(bare_text));
        }
    }

    /// Puts the impl `key` in the lists `filed` names: the one place an impl is filed, from
    /// its header's text ([`Buckets::file_all`]) or from its row ([`file_row`]).
    /// `unread_bare`: for an unread trait impl, whether its trait is written with no type or
    /// const argument ([`bare_text`]).
    fn put(&mut self, key: LibKey, filed: Filed, unread_bare: bool) {
        let b = self;
        if let Filed::Trait(_, _, true) = filed {
            b.bare.push(key);
        }
        match filed {
            Filed::Trait(tr, Some(s), _) => {
                push(&mut b.by_shape, (tr, s), key);
                match s {
                    Shape::Int(_) | Shape::Uint(_) => push(&mut b.by_shape, (tr, Shape::AnyInt), key),
                    Shape::Float(_) => push(&mut b.by_shape, (tr, Shape::AnyFloat), key),
                    _ => {}
                }
            }
            Filed::Trait(tr, None, _) => push(&mut b.any, tr, key),
            Filed::Inherent(Some(s)) => push(&mut b.inherent_by_shape, s, key),
            Filed::Inherent(None) => b.inherent_any.push(key),
            Filed::NoCheckerType => b.no_checker_type += 1,
            Filed::Negative(tr) => push(&mut b.negative, tr, key),
            Filed::Unread(why, shape) => {
                b.unread.push(key);
                // An unread trait impl written with no type or const argument on its trait
                // is judged by `Homes::bare` as a read one is: if it is of the asked trait
                // at all, that trait's parameters are at their defaults, so where those
                // are `Self` only its self type can be its crate's local type.
                if why.is_trait_impl() && unread_bare {
                    b.bare.push(key);
                }
                match (why.is_trait_impl(), shape) {
                    (false, _) => b.unread_blind.push(key),
                    (true, None) => b.unread_any.push(key),
                    (true, Some(s)) => {
                        push(&mut b.unread_by_shape, s, key);
                        match s {
                            Shape::Int(_) | Shape::Uint(_) => push(&mut b.unread_by_shape, Shape::AnyInt, key),
                            Shape::Float(_) => push(&mut b.unread_by_shape, Shape::AnyFloat, key),
                            _ => {}
                        }
                    }
                }
                b.unread_why.push((key, why));
            }
        }
    }

    /// Sorts the lists that are searched or read in key order, after impls were filed.
    fn sort_filed(&mut self) {
        let b = self;
        b.unread.sort_unstable();
        b.bare.sort_unstable();
        b.blanket.sort_unstable();
        // Searched by key ([`Buckets::is_unsure`]); over `Buckets::lazy` crates are filed in
        // the order questions admit them, and a crate's rows before the impls read from text.
        b.inherent_any.sort_unstable();
        for v in b.negative.values_mut() {
            v.sort_unstable();
        }
    }

    /// The candidates for `trait_` at `shape`, each once.
    pub fn trait_candidates(&self, trait_: ItemKey, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(self.by_shape.get(&(trait_, shape)).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(self.any.get(&trait_).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(self.unread_by_shape.get(&shape).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(&self.unread_any);
        out.extend_from_slice(&self.unread_blind);
        self.keep_home(&mut out, home);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Drops the impls written outside `home` (an impl of no known crate stays; a bare trait
    /// impl is judged by `home.bare`, any other by `home.all`). A blanket impl is judged by
    /// `home.blanket` first: its trait's crate alone, the narrowest of the three.
    fn keep_home(&self, out: &mut Vec<LibKey, Global>, home: Homes) {
        if home == Homes::ANY {
            return;
        }
        out.retain(|k| {
            let h = if self.blanket.binary_search(k).is_ok() {
                home.blanket
            } else if self.bare.binary_search(k).is_ok() {
                home.bare
            } else {
                home.all
            };
            self.krate(*k).is_none_or(|c| self.admits(h, c))
        });
    }

    /// Whether an impl written in crate `krate` can be in `home`.
    pub fn admits(&self, home: Home, krate: usize) -> bool {
        match home {
            Home::Any => true,
            Home::In(a, b) => krate == a || b == Some(krate),
            // A crate the names library has no row for stays on the wide side.
            Home::Incoherent => self.incoherent.get(krate).copied().unwrap_or(true),
        }
    }

    /// The crate impl `k` is written in.
    pub fn krate(&self, k: LibKey) -> Option<usize> {
        let at = self.krates.binary_search_by_key(&k, |e| e.0).ok()?;
        Some(self.krates[at].1)
    }

    /// The home the answered trait question was narrowed to.
    pub fn home_of(&self, trait_: ItemKey, shape: Shape) -> Homes {
        self.homes.get(&(trait_, shape)).copied().unwrap_or(Homes::ANY)
    }

    /// The negative impls of `trait_` (any self type), sorted by key.
    pub fn negative_of(&self, trait_: ItemKey) -> &[LibKey] {
        self.negative.get(&trait_).map_or(&[], |v| v.as_slice())
    }

    /// The candidates for an inherent fn of `shape`, each once.
    pub fn inherent_candidates(&self, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(self.inherent_by_shape.get(&shape).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(&self.inherent_any);
        // A trait impl (its trait written in its header's text, though unread, refused or
        // lost by the lowering: `written_trait`) is never an inherent candidate; `unread_blind`
        // holds only impls whose header text does not read as an impl at all.
        out.extend_from_slice(&self.unread_blind);
        self.keep_home(&mut out, home);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether `k` was filed as unread (its trait path names nothing as written).
    pub fn is_unread(&self, k: LibKey) -> bool {
        self.unread.binary_search(&k).is_ok()
    }

    /// Whether `k` is a candidate of questions it may not answer: unread, or an inherent impl
    /// whose self type's shape was not read as written (an alias, a qualified path). Such a
    /// candidate's failed header is a listed gap of the body, not the end of its check.
    /// Both lists are in the index's order (by key).
    pub fn is_unsure(&self, k: LibKey) -> bool {
        self.is_unread(k) || self.inherent_any.binary_search(&k).is_ok()
    }

    /// Records an unread candidate whose header failed.
    pub fn fail(&mut self, k: LibKey, why: Box<str>) {
        if let Err(at) = self.failed.binary_search_by_key(&k, |f| f.0) {
            self.failed.insert(at, (k, why));
        }
    }

    pub fn has_failed(&self, k: LibKey) -> bool {
        self.failed.binary_search_by_key(&k, |f| f.0).is_ok()
    }

    /// Answers `trait_` at `shape` from its candidates' headers, which `store` must hold.
    pub fn answer(&mut self, store: &Store<'_>, trait_: ItemKey, shape: Shape, home: Homes) -> Result<(), Unfilled> {
        let view = store.tys.view();
        let mut got = Vec::new_in(Global);
        // Candidates come sorted, so the first failed one is the least key.
        let mut first_failed = None;
        self.homes.insert((trait_, shape), home);
        // The negative impls are candidates too: the solver reads their polarity (a negative
        // impl decides an auto trait's structural rule and never meets a positive goal).
        let mut candidates = self.trait_candidates(trait_, shape, home);
        candidates.extend_from_slice(self.negative_of(trait_));
        candidates.sort_unstable();
        candidates.dedup();
        for k in candidates {
            if self.has_failed(k) {
                first_failed = first_failed.or(Some(k));
                continue;
            }
            let h = store.impls.get(&checker_of(k)).ok_or(Unfilled::Impl(k))?;
            // A header built WITHOUT the trait its text names answers nothing: it is the
            // question's gap, never an impl left out of the answer unseen.
            if h.trait_.is_none() && self.by_text.binary_search(&k).is_ok() {
                self.fail(k, Box::from("its header's text names a trait; the header as lowered holds none"));
                first_failed = first_failed.or(Some(k));
                continue;
            }
            if h.trait_ == Some(trait_) && fits(shape_of(view, h.self_ty), shape) {
                got.push(checker_of(k));
            }
        }
        got.sort_unstable();
        self.answers.insert((trait_, shape), got);
        if let Some(k) = first_failed {
            self.answer_failed.insert((trait_, shape), k);
        }
        Ok(())
    }

    /// Answers every inherent fn of `shape` from its candidates' headers and signatures.
    pub fn answer_inherent(&mut self, store: &Store<'_>, shape: Shape, home: Homes) -> Result<(), Unfilled> {
        let view = store.tys.view();
        let mut first_failed = None;
        for k in self.inherent_candidates(shape, home) {
            if self.has_failed(k) {
                first_failed = first_failed.or(Some(k));
                continue;
            }
            let ck = checker_of(k);
            let h = store.impls.get(&ck).ok_or(Unfilled::Impl(k))?;
            if h.trait_.is_some() || shape_of(view, h.self_ty) != Some(shape) {
                continue;
            }
            for &(name, f) in h.fns.iter() {
                // A fn whose signature could not be made answers no question of its name:
                // asking for that name is the gap (never an answer without it).
                if !store.fns.contains_key(&f) && store.fn_gaps.contains_key(&f) {
                    self.gapped.insert((shape, name), f);
                    continue;
                }
                let self_param = store.fns.get(&f).ok_or(Unfilled::Fn(f))?.self_param;
                push(&mut self.inherent, (shape, name), AssocItem { container: ck, item: f, kind: AssocKind::Fn { self_param } });
            }
            for &(name, c) in h.consts.iter() {
                push(&mut self.inherent, (shape, name), AssocItem { container: ck, item: c, kind: AssocKind::Const });
            }
        }
        for ((s, _), v) in self.inherent.iter_mut() {
            if *s == shape {
                v.sort_unstable_by_key(|a| a.item);
            }
        }
        self.inherent_done.insert(shape, first_failed);
        Ok(())
    }

    /// How the impls were filed: (under a trait and a shape, under a trait's "any", inherent
    /// by shape, inherent "any", unread).
    pub fn filed(&self) -> [usize; 5] {
        let shaped = self.by_shape.iter().filter(|((_, s), _)| !matches!(s, Shape::AnyInt | Shape::AnyFloat)).map(|(_, v)| v.len()).sum();
        let any = self.any.values().map(|v| v.len()).sum();
        let inherent = self.inherent_by_shape.values().map(|v| v.len()).sum();
        [shaped, any, inherent, self.inherent_any.len(), self.unread.len()]
    }

    /// Every trait question answered so far, with its answer, in a stable order (for a work
    /// list naming what selection was asked and what it found).
    pub fn answered(&self) -> Vec<(ItemKey, Shape, &[ItemKey]), Global> {
        let mut out = Vec::new_in(Global);
        out.extend(self.answers.iter().map(|(&(t, s), v)| (t, s, v.as_slice())));
        out.sort_unstable_by(|a, b| (a.0, format!("{:?}", a.1)).cmp(&(b.0, format!("{:?}", b.1))));
        out
    }

    /// The answer for `trait_` at `shape`, once made.
    pub fn impls_of(&self, trait_: ItemKey, shape: Shape) -> Option<&[ItemKey]> {
        self.answers.get(&(trait_, shape)).map(|v| v.as_slice())
    }

    /// The failed candidate (the least key, with why) of the answered trait question, if any:
    /// its answer may miss that impl, so asking it is a gap, never an answer.
    pub fn trait_gap(&self, trait_: ItemKey, shape: Shape) -> Option<(LibKey, &str)> {
        self.answer_failed.get(&(trait_, shape)).and_then(|&k| self.failed_why(k))
    }

    /// The failed candidate (the least key, with why) of the answered inherent shape, if any.
    pub fn inherent_gap(&self, shape: Shape) -> Option<(LibKey, &str)> {
        self.inherent_done.get(&shape).copied().flatten().and_then(|k| self.failed_why(k))
    }

    fn failed_why(&self, k: LibKey) -> Option<(LibKey, &str)> {
        let at = self.failed.binary_search_by_key(&k, |f| f.0).ok()?;
        Some((k, &*self.failed[at].1))
    }

    /// The inherent fns of `shape` named `name`, once the shape is answered.
    /// The inherent fn named `name` at `shape` whose signature could not be made, if any.
    pub fn gapped(&self, shape: Shape, name: Name) -> Option<ItemKey> {
        self.gapped.get(&(shape, name)).copied()
    }

    pub fn inherent(&self, shape: Shape, name: Name) -> Option<&[AssocItem]> {
        if !self.inherent_done.contains_key(&shape) {
            return None;
        }
        Some(self.inherent.get(&(shape, name)).map_or(&[], |v| v.as_slice()))
    }
}
