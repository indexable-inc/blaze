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
//! leaves an impl out. An impl whose self type is written as a path read to no constructor (a
//! path that names nothing: its header is refused) is in an "any" list but, by the coherence
//! rules, a candidate only where the asked self type can be its crate's local type
//! ([`PathSelf`], [`Homes::local`]). The check loop lowers the candidates' headers ([`Buckets::trait_wants`]),
//! then [`Buckets::answer`] keeps those whose lowered header matches, and the answer is kept.

use core::cell::{OnceCell, Ref, RefCell, RefMut};
use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use blaze_hash::{HashMap, HashSet};
use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_lower::{View, form as lf, head};
use blaze_scope::items_record::{ImplRow, SelfHead};
use blaze_scope::resolve::Names;
use blaze_surface::ImplFlags;
use blaze_typeck::{AssocItem, AssocKind, ItemKey, Mutbl, Name, PNode, PTy, PTys, Shape};

use super::heads::{CoherenceMarks, HeadError, Store, Want, lower};
use super::tables::view::BodyHeadFacts;
use super::index::Index;
use super::keys::{Keys, checker_of};
use super::linked::{Linked, RootsError};
use super::rows::Rows;
use super::source::NamesSource;
use super::types::{PRIMS, children, path_texts};
use crate::resolve::{Kind, Ns, ResolveError};

mod asked;
mod by_trait;
mod records;

pub use asked::{Ask, BucketRead, INCOHERENT_FACT, UNFILED_LIST};
pub use records::{
    BucketReadUnion,
    AdmitError, BucketSource, ImplChunk, ImplQuestionError, ImplShelf, ImplShelfOpts, ImplTables, ImplTablesDone, ImplTablesFailed, ItemsIndex, NoTables, ShelfError, ShelfSource, ShelvedTwice, TRAIT_HELD, TableSet, TablesError, TablesReadError, bucket_cell, cell_read, check_framed, cells_of, file_impl_rows_chunk, finish_impl_tables, framed,
    impl_tables_name, impl_tables_out, impl_tables_task, row_answers, row_digest, rows_key, trait_cell, write_impl_tables,
};

/// A candidate impl whose header the store does not hold when the answer is made, or an
/// inherent impl's fn whose signature it does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unfilled {
    Impl(LibKey),
    Fn(ItemKey),
    /// The crates a checked body sees could not be told ([`Unseen`]): its trait questions are
    /// not answered, never answered over the whole program.
    Sight(Unseen),
}

/// Whose impls a trait question's answer is made from: the key's third part (rule 21: the
/// crates seen are an input of the answer, so they are in its key).
///
/// The rule: a body of crate C is checked against the impls of C and of the crates C links
/// (its dependency closure, front2 linked.rs), never of a crate that depends on C. Coherence
/// makes that set complete for every goal C's text can write (the Reference, "Orphan rules":
/// an impl of a crate downstream of C names one of its OWN types or traits, which C's text
/// cannot name); seeing more only lets an inference variable of C's body meet an impl it
/// could never mean. rustc checks core's bodies seeing core's impls alone, so `v ==
/// (ErrorKind::NotFound as _)` in core's `from_prim!` infers `_ = u32` from `impl PartialEq for
/// u32`, the one impl of `u32: PartialEq<?T>`; a program that also links a crate with `impl
/// PartialEq<Mine> for u32` made that goal ambiguous in the walk and the cast E0282 (lane RF13,
/// 2026-11-18: the entry programs refused core's `<impl ErrorKind>::from_prim`).
///
/// A question at ground arguments after checking (an instance's resolution, a borrow check,
/// a test entry) sees the whole program: a downstream type may be there (`[Mine]::contains`
/// asks `Mine: PartialEq` in core's generic body), and no inference variable is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seen {
    /// Every crate of the program: a question at ground arguments (`Reveal::All`).
    Program,
    /// The crate's and the crates it links: a body of that crate checked (`Reveal::InBody`).
    From(usize),
}

/// Why the crates a checked body sees were not told (rule 19: the crate or module named).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unseen {
    /// The body's module is of no crate of the names.
    NoCrate { module: LibKey },
    /// The names source did not answer the crate's links (records: a fact filed in its log).
    NotAnswered { krate: usize },
    /// The crate is no crate of the names.
    Roots(RootsError),
    /// A read list of a body of the crate holds an impl question's bucket rows, and no sight of
    /// the crate was made ([`Buckets::sight_made`]): the question was answered on a path that
    /// did not ask [`Buckets::sight`].
    NotMade { krate: usize },
}

impl Seen {
    /// What a body checked in `module` sees (`Reveal::InBody`, front2 world_items.rs): its
    /// crate's, by the module key's prefix (no record is read).
    pub fn of_checked_body<'n, S: NamesSource<'n>>(names: S, module: LibKey) -> Result<Seen, Unseen> {
        names.crate_of(module).map(Seen::From).ok_or(Unseen::NoCrate { module })
    }
}

impl core::fmt::Display for Unseen {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unseen::NoCrate { module } => write!(f, "the checked body's module {module:x} is of no crate, so the impls it sees (its crate's dependency closure) are not known"),
            Unseen::NotAnswered { krate } => write!(f, "the crates crate {krate} links were not answered by the names source, so the impls its bodies see are not known"),
            Unseen::Roots(e) => write!(f, "the impls a checked body sees: {e}"),
            Unseen::NotMade { krate } => write!(f, "a read list of a body of crate {krate} holds impl bucket rows, and no sight of the crate was made (front2 buckets.rs `Buckets::sight_made`)"),
        }
    }
}

/// The crates one trait question's answer may hold impls of ([`Seen`]): every crate, or a
/// crate's dependency closure.
#[derive(Clone, Copy)]
pub struct Sight<'l> {
    pub seen: Seen,
    /// `None`: every crate of the source.
    row: Option<&'l SightRow>,
}

/// One crate's sight, made once a [`Buckets`] ([`Buckets::sight`]).
///
/// WHERE IT COMES FROM, so that it adds no read (rule 77; lane RF13, 2026-11-18): the crates a
/// body sees are a function of the crate list (the unit graph), never of a names record.
/// - Over records, crate 0 is the crate whose check scope the records ARE (blaze-world
///   check_scope.rs, the one rule of which crates a body sees: blaze-cli engine_bodies.rs
///   `scope_of` puts the member at 0 and its scope after it; a program's records put the
///   program at 0 and the crates it links after it): it sees every crate of them, which the
///   crate list alone says. Before, the sight was asked of the names source's link closure
///   (`NamesSource::linked`), which over the read-only view (front2 tables/view.rs) records
///   every crate's names and items WHOLE: every body that asked a trait question read every
///   crate's `CrateScope` whole, so any names edit entered it (blaze-cli
///   engine_edit_rows_tests.rs R6, R8, R10, R11; engine_scope_reads_tests.rs, four cases).
/// - Over the solved names, the link closure (front2 linked.rs `Linked`, the names path's
///   reading of the same rule over the in-memory crate graph): no record, so no read.
/// - Over records, a crate other than 0 (a dependency's body checked over a program's records:
///   tests only; the engine checks members, each over its own scope): the link closure, which
///   reads names whole (owed, stated: the end form is the unit graph's edges handed to the
///   records, blaze-world crate_list.rs `CrateGraph::deps`).
struct SightRow {
    /// `None`: every crate of the records.
    closure: Option<Linked<'static>>,
    /// The first 16 bytes of each seen crate's key, sorted: what a read-list row names a crate
    /// by (buckets/asked.rs [`BucketRead::krate`]). Empty when `closure` is `None`.
    ids: blaze_own::AVec<'static, u128>,
}

/// The first 16 bytes of a crate's key, big-endian: [`BucketRead::krate`]'s form.
fn crate_id(key: blaze_id::CrateItemKey) -> u128 {
    let mut first = [0u8; 16];
    first.copy_from_slice(&key.0.0[..16]);
    u128::from_be_bytes(first)
}

impl SightRow {
    /// Crate `krate`'s sight over `names` (the type's docs); the closure in `alloc`.
    fn of<'n, S: NamesSource<'n>>(names: S, krate: usize, alloc: blaze_own::Alloc<'static>) -> Result<SightRow, Unseen> {
        if krate >= names.crate_count() {
            return Err(Unseen::NotAnswered { krate });
        }
        let mut ids = blaze_own::AVec::new_in(alloc);
        if names.solved().is_none() && krate == 0 {
            return Ok(SightRow { closure: None, ids });
        }
        let linked = match names.linked(&[krate], alloc) {
            None => return Err(Unseen::NotAnswered { krate }),
            Some(Err(e)) => return Err(Unseen::Roots(e)),
            Some(Ok(linked)) => linked,
        };
        // The crate graph: no record is read (`NamesSource::crate_key`).
        for c in linked.crates() {
            let Some(key) = names.crate_key(c) else { return Err(Unseen::NotAnswered { krate: c }) };
            ids.push(crate_id(key));
        }
        ids.sort_unstable();
        Ok(SightRow { closure: Some(linked), ids })
    }
}

impl Sight<'_> {
    /// The whole program's.
    pub const PROGRAM: Sight<'static> = Sight { seen: Seen::Program, row: None };

    /// Whether an impl written in `krate` may answer: an impl of no known crate stays, as
    /// [`BucketIndex::trait_candidates`]'s home keeps it (the wide side: a refusal, if any,
    /// never a dropped impl).
    pub fn sees(&self, krate: Option<usize>) -> bool {
        match (self.row.and_then(|row| row.closure.as_ref()), krate) {
            (Some(linked), Some(c)) => linked.links(c),
            _ => true,
        }
    }

    /// Whether a read-list row of an impl question belongs to this sight's answer: a fact of
    /// the whole graph (`krate` 0: the incoherent crates) always; a crate's bucket only when the
    /// crate is seen. A bucket of a crate the body does not see cannot move its answer
    /// ([`Buckets::answer`] takes none of its impls), so it is no read of the body.
    pub fn sees_read(&self, read: &BucketRead) -> bool {
        match self.row {
            Some(SightRow { closure: Some(_), ids }) => read.krate == 0 || ids.binary_search(&read.krate).is_ok(),
            _ => true,
        }
    }
}

impl core::fmt::Display for Unfilled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unfilled::Impl(k) => write!(f, "the candidate impl {k:x} has no header in the store when its question is answered"),
            Unfilled::Fn(k) => write!(f, "the inherent impl fn {k:?} has no `self` flag in its impl's head when its question is answered"),
            Unfilled::Sight(why) => write!(f, "{why}"),
        }
    }
}

/// The shape a lowered self type is matched by; `None` for a parameter (a blanket impl).
pub(crate) fn shape_of(t: PTys<'_>, p: PTy) -> Option<Shape> {
    Some(match *t.get(p)? {
        PNode::Bool => Shape::Bool,
        PNode::Char => Shape::Char,
        PNode::Int(i) => Shape::Int(i),
        PNode::Uint(u) => Shape::Uint(u),
        PNode::Float(f) => Shape::Float(f),
        PNode::Str => Shape::Str,
        PNode::Never => Shape::Never,
        PNode::Tuple(l) => Shape::Tuple(l.len),
        PNode::Record(s, l) => Shape::Record(s.0, l.len),
        PNode::Union(l) => Shape::Union(l.len),
        PNode::Ref(m, _) => Shape::Ref(m),
        PNode::Ptr(m, _) => Shape::Ptr(m),
        PNode::Array(..) => Shape::Array,
        PNode::Slice(_) => Shape::Slice,
        PNode::Pat(..) => Shape::Pat,
        PNode::Adt(k, _) => Shape::Adt(k),
        PNode::Foreign(k) => Shape::Foreign(k),
        PNode::FnDef(k, _) => Shape::FnDef(k),
        PNode::FnPtr(l, _) => Shape::FnPtr(l.len.saturating_sub(1)),
        PNode::Opaque(k, _) => Shape::Opaque(k),
        PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => Shape::Closure,
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
    /// the checker does not read yet: the caller makes it a gap of such a question. With its
    /// self type's shape where that was read (an impl table files it there: records.rs).
    Negative(ItemKey, Option<Shape>),
}

/// Why an impl's header could not be filed by its trait: each impl filed so is a candidate of
/// every question, so each class is a cost to remove at its home.
#[derive(Clone, Debug)]
pub enum Unread {
    /// The names library holds no text for the impl.
    NoText,
    /// The names library holds no text for the impl, and its row's tokens say it is a trait
    /// impl ([`textless`]).
    NoTextTrait,
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
    /// Where the trait path's tokens are read could not be told (their marks were lost, or
    /// the mark names a site this build does not hold): `why` says which (item_site.rs).
    Site { path: Box<str>, why: Box<str> },
    /// Read back from an impl table (records.rs): a row holds whether it is a trait impl and
    /// its class ([`Unread::why_byte`]), not the path or the refusal.
    Indexed { trait_impl: bool, class: u8 },
}

impl Unread {
    /// Whether the impl is known to be a trait impl (its trait path was written; only the
    /// path failed).
    fn is_trait_impl(&self) -> bool {
        matches!(self, Unread::NoTextTrait | Unread::TraitNotAPath | Unread::PathUnread | Unread::Resolve { .. } | Unread::NotATrait { .. } | Unread::Site { .. } | Unread::Indexed { trait_impl: true, .. })
    }

    /// The class as an impl row's why byte ([`blaze_surface::ImplFlags::why`]): never 0.
    fn why_byte(&self) -> u8 {
        match self {
            Unread::NoText => 1,
            Unread::NoTextTrait => 2,
            Unread::Unlowered => 3,
            Unread::Generics => 4,
            Unread::TraitNotAPath => 5,
            Unread::PathUnread => 6,
            Unread::Resolve { .. } => 7,
            Unread::NotATrait { .. } => 8,
            Unread::Site { .. } => 9,
            Unread::Indexed { class, .. } => *class,
        }
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
    let text = names.signature_text_of(alias)?.text;
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

/// Files an impl by its header's text, with what bounds it when its self type is a path that
/// was read to no constructor ([`PathSelf`]; `None` for any other impl).
fn file<'n, S: NamesSource<'n>>(names: S, key: LibKey, module: LibKey) -> (Filed, Option<PathSelf>) {
    let Some(it) = names.signature_text_of(key) else { return (Filed::Unread(Unread::NoText, None), None) };
    let Ok(low) = lower(names, key, it.text) else { return (Filed::Unread(Unread::Unlowered, None), None) };
    let v = low.view();
    let mut params = Vec::new_in(Global);
    if super::heads::param_names(&v, 0, &mut params, super::heads::Consts::Skip).is_err() {
        return (Filed::Unread(Unread::Generics, None), None);
    }
    let self_ty = lf::TypeId(v.head[head::SELF_TY]);
    let Ok(self_shape) = written_shape(names, module, &params, &v, self_ty) else { return (Filed::NoCheckerType, None) };
    let path_self = self_shape.is_none() && is_named_path(&params, &v, self_ty);
    let Some(t) = v.head_opt(head::TRAIT) else { return (Filed::Inherent(self_shape), path_self.then_some(PathSelf::Inherent)) };
    let Ok(lf::TypeRef::Path(p)) = v.ty(lf::TypeId(t)) else { return (Filed::Unread(Unread::TraitNotAPath, self_shape), None) };
    let Ok((abs, texts, _, at)) = path_texts(&v, p) else { return (Filed::Unread(Unread::PathUnread, self_shape), None) };
    let path: Box<str> = format!("{}{}", if abs { "::" } else { "" }, texts.join("::")).into_boxed_str();
    // Its trait written with no type or const argument ([`Homes::bare`]).
    let bare = written_args(&v, p).is_ok_and(|n| n == 0);
    // The trait path is read where its first token's mark says (front2 item_site.rs,
    // token-mark-F1), as the impl's head reads it (heads.rs `impl_`): a macro's
    // `$crate::Tr` (written `crate::Tr` in the expansion, with the macro crate's mark) and a
    // def-site `Tr` of a `macro` name the macro's trait, not a path of the impl's module. Read
    // at the module, such an impl was filed unread: a candidate of EVERY trait question at its
    // shape, so each question made its whole head, associated types' values included
    // (primefield's `type ByteSize = U<{ <$name as ..>::PARAMS.modulus()..div_ceil(8) }>`),
    // and a constant's own evaluation that asked any trait of the type met that head needing
    // the constant itself (`ConstFail::Nested`, tempo census r20: 1,402 fns of p256).
    let marks = match super::item_site::item_marks(names, key, it.text) {
        Ok(m) => m,
        Err(lost) => return (Filed::Unread(Unread::Site { path, why: format!("{lost}").into_boxed_str() }, self_shape), None),
    };
    match super::item_site::resolve_written(&crate::resolve::Paths { names }, marks, module, &v, p, abs, &texts, Ns::Type, at) {
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() && v.head[head::FLAGS] & sig::NEGATIVE != 0 => (Filed::Negative(checker_of(f.key), self_shape), None),
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() => {
            (Filed::Trait(checker_of(f.key), self_shape, bare), path_self.then(|| PathSelf::Trait { of: names.crate_of(f.key), bare }))
        }
        Ok(f) => (Filed::Unread(Unread::NotATrait { path, kind: f.kind, consumed: f.consumed, written: texts.len() as u32 }, self_shape), None),
        Err(super::types::Why::Resolve(why, _)) => (Filed::Unread(Unread::Resolve { path, why }, self_shape), None),
        Err(other) => (Filed::Unread(Unread::Site { path, why: format!("{other:?}").into_boxed_str() }, self_shape), None),
    }
}

/// How many type or const arguments the trait path `p` writes, over all its segments; a
/// lifetime and an associated item constraint are none ([`Homes::bare`]: an impl whose trait
/// takes each parameter's default). Every written argument counts, a plain name (`Rem<Big>`)
/// too: [`children`] passes over a plain name (a type or a constant, made once the scope is
/// known), so its empty list said `impl Rem<BigUint> for u64` was written bare, and a question
/// `u64: Rem<BigUint>` dropped that impl as one only core could write (tempo census
/// 2026-11-17: num-bigint's `u64 % BigUint`, `u16 - BigInt` and their forwarding impls).
fn written_args(v: &View<'_>, p: lf::PathIdx) -> Result<usize, super::types::Why> {
    let mut n = 0usize;
    for &s in v.segments(v.path(p)?)? {
        n += super::types::written_type_args(v, &lf::Segment::decode(s)?)?;
    }
    Ok(n)
}

/// Whether the written type `t` is a path that names an item (not one of the impl's own
/// generic parameters `params`, not a qualified path `<T as Tr>::X`): the form [`PathSelf`]
/// is about.
fn is_named_path(params: &[(Name, u32)], v: &View<'_>, t: lf::TypeId) -> bool {
    let Ok(lf::TypeRef::Path(p)) = v.ty(t) else { return false };
    let Ok((abs, texts, segs, _)) = path_texts(v, p) else { return false };
    if segs.first().is_none_or(|s| s.kind == lf::SegKind::Anchor) {
        return false;
    }
    abs || texts.len() != 1 || !params.iter().any(|(n, _)| *n == Name::of(texts[0]))
}

/// An impl whose self type is written as a path that its filing read to no constructor (the
/// path names nothing in the impl's module, an alias of no constructor read here, a trait, a
/// module), so it is filed at every shape ("any"). What its header says that the coherence
/// rules bound it by ([`Buckets::bounded`]):
/// - an inherent impl: the Reference, "Inherent implementations": "An implementing type must
///   be defined within the same crate as the original type definition" (rustc E0116, E0390
///   for a primitive or a reference); only a crate that may write incoherent impls
///   ([`Home::Incoherent`]) is excepted;
/// - a trait impl of a trait of crate `of` written with no type or const argument (`bare`):
///   the Reference, "Orphan rules": "Given `impl<P1..=Pn> Trait<T1..=Tn> for T0`, an `impl` is
///   valid only if at least one of the following is true: `Trait` is a local trait; At least
///   one of the types `T0..=Tn` must be a local type." With no argument written each `Ti` is
///   its parameter's default, written in the trait's crate, which cannot name a type of the
///   impl's crate (that crate depends on the trait's) other than through `Self`; so for a trait
///   of another crate `T0` itself must be local: a nominal type or a trait object of the impl's
///   crate, or `&T`, `&mut T`, `Box<T>`, `Pin<T>` of one (fundamental). A trait written with
///   arguments (`impl Tr<Local> for Missing`) may be local by an argument, and a trait of the
///   impl's own crate may be implemented for any type: neither is bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathSelf {
    Inherent,
    Trait { of: Option<usize>, bare: bool },
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
    /// A path read to no constructor (names nothing, an alias of no constructor read here, a
    /// trait, a module): filed at every shape, bounded by [`PathSelf`].
    Unread,
    NoCheckerType,
    /// A segment that is no plain identifier: the header's text is read.
    Text,
}

/// What one crate's rows resolved so far ([`file_row`]): the trait of the current run of rows
/// (the rows are sorted by trait path, then module, so each run of equal (trait path, module)
/// is resolved once) and each self path met, by the module it is written in. One per crate
/// filed; dropped when its rows are.
struct RowNames<'n> {
    /// The run's trait path, its module, the crate the path's root names (token-mark-2 R2: two
    /// rows of one text and module may name two crates' `crate`), and the trait it names
    /// (`None`: not a trait as written: each impl of the run is filed from its header's text,
    /// which says why). `selves` is keyed by the self path's root the same way.
    /// The trait is kept with the crate it is defined in ([`PathSelf::Trait`]).
    run: Option<(&'n str, LibKey, Option<blaze_id::CrateItemKey>, Result<(ItemKey, Option<usize>), RowMiss>)>,
    selves: HashMap<(LibKey, &'n str, Option<blaze_id::CrateItemKey>), SelfAt, Global>,
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
///
/// The third answer is what bounds an impl whose self path was read to no constructor
/// ([`PathSelf`], as `file` gives it).
fn file_row<'n, S: NamesSource<'n>>(names: S, row: &ImplRow<'n>, kept: &mut RowNames<'n>) -> Result<(Filed, bool, Option<PathSelf>), RowMiss> {
    if row.header_unread {
        return Err(RowMiss::HeaderUnread);
    }
    if row.trait_unread || row.has_trait != row.trait_path.is_some() {
        return Err(RowMiss::TraitUnread);
    }
    let (shape, path_self) = match row_self(names, row, kept)? {
        RowSelf::Shape(shape, path_self) => (shape, path_self),
        // Before the trait is read, as `file` decides it.
        RowSelf::NoCheckerType => return Ok((Filed::NoCheckerType, false, None)),
    };
    let paths = crate::resolve::Paths { names };
    let Some(trait_path) = row.trait_path else { return Ok((Filed::Inherent(shape), false, path_self.then_some(PathSelf::Inherent))) };
    let trait_ = match kept.run {
        Some((path, module, root, trait_)) if path == trait_path && module == row.module && root == row.trait_root => trait_,
        _ => {
            let trait_ = row_path(trait_path).ok_or(RowMiss::TraitPath).and_then(|(abs, texts)| match paths.path_with(row.module, row.trait_root, abs, &texts, Ns::Type) {
                Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() => Ok((checker_of(f.key), names.crate_of(f.key))),
                Ok(_) | Err(_) => Err(RowMiss::NoTrait),
            });
            kept.run = Some((trait_path, row.module, row.trait_root, trait_));
            trait_
        }
    };
    let (trait_, of) = trait_?;
    let blanket = row.head == SelfHead::Param;
    let negative = row.flags.bits() & ImplFlags::NEGATIVE.bits() != 0;
    if negative {
        return Ok((Filed::Negative(trait_, shape), blanket, None));
    }
    Ok((Filed::Trait(trait_, shape, row.bare), blanket, path_self.then_some(PathSelf::Trait { of, bare: row.bare })))
}

/// An impl's self type's head as its row reads it ([`row_self`]).
enum RowSelf {
    /// The outermost constructor (`None`: none, the impl is at every shape), and whether that
    /// is a path read to no constructor ([`PathSelf`]).
    Shape(Option<Shape>, bool),
    /// A builtin the checker has no type for.
    NoCheckerType,
}

/// The self type's head of the impl `row` as the row writes it, resolved in the impl's module:
/// `Err` where the row does not name it ([`SelfHead::Other`], a path segment that is no plain
/// identifier). [`file_row`]'s first step, and what files an impl whose text did not read by
/// the head its row read ([`textless`]).
fn row_self<'n, S: NamesSource<'n>>(names: S, row: &ImplRow<'n>, kept: &mut RowNames<'n>) -> Result<RowSelf, RowMiss> {
    let paths = crate::resolve::Paths { names };
    let mut path_self = false;
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
            let at = match kept.selves.get(&(row.module, written, row.self_root)).copied() {
                Some(at) => at,
                None => {
                    // The arms of `shape_step`'s path case, over the row's path.
                    let at = match row_path(written) {
                        None => SelfAt::Text,
                        Some((abs, texts)) => match paths.path_with(row.module, row.self_root, abs, &texts, Ns::Type) {
                            Ok(f) if f.consumed as usize == texts.len() => match f.kind {
                                Kind::Struct | Kind::Enum | Kind::Union => SelfAt::Shape(Some(Shape::Adt(checker_of(f.key)))),
                                Kind::Builtin(name) => SelfAt::Shape(prim_shape(name)),
                                Kind::TypeAlias => match alias_shape(names, f.key) {
                                    Ok(Some(shape)) => SelfAt::Shape(Some(shape)),
                                    Ok(None) => SelfAt::Unread,
                                    Err(NoCheckerType) => SelfAt::NoCheckerType,
                                },
                                Kind::WideInt { .. } => SelfAt::NoCheckerType,
                                _ => SelfAt::Unread,
                            },
                            Ok(_) | Err(_) => SelfAt::Unread,
                        },
                    };
                    kept.selves.insert((row.module, written, row.self_root), at);
                    at
                }
            };
            match at {
                SelfAt::Shape(shape) => shape,
                SelfAt::Unread => {
                    path_self = true;
                    None
                }
                SelfAt::NoCheckerType => return Ok(RowSelf::NoCheckerType),
                SelfAt::Text => return Err(RowMiss::SelfPath),
            }
        }
    };
    Ok(RowSelf::Shape(shape, path_self))
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

/// One impl as its crate's rows and the names file it: where ([`Filed`]) and the facts its
/// header gave that judge it as a candidate. What [`Buckets::take`] files in a job's index and
/// what an impl table's row is written from (records.rs): one reading of a header, two homes
/// of the result until the impl tables replace the index.
pub(crate) struct Filing {
    pub(crate) key: LibKey,
    filed: Filed,
    /// A blanket trait impl by its header's tokens ([`written_trait`], [`Homes::blanket`]).
    blanket: bool,
    /// Filed by its header text's trait because the lowered header named none
    /// ([`file_by_text`]).
    by_text: bool,
    /// For an unread trait impl: its trait is written with no type or const argument.
    unread_bare: bool,
    /// What bounds it when its self path was read to no constructor ([`PathSelf`]).
    path_self: Option<PathSelf>,
}

/// What one [`file_rows`] call filed from rows and from text, and why each row missed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileCounts {
    pub from_rows: u32,
    pub from_text: u32,
    /// By [`RowMiss`] as `usize`.
    pub row_misses: [u32; 6],
}

/// Files every impl of `rows` (crate `krate`'s rows of its items record), handing each to
/// `each` once: by its row ([`file_row`]); where the row does not file it, by its header's
/// text ([`file_text`]); where that text did not read, by what its row read ([`textless`]).
/// A function of the rows and the names: in the record's order (trait path, module, key)
/// `file_row` resolves each run of equal (trait path, module) once, and the impls read from
/// text are filed by key. Every impl of `rows` is handed over exactly once.
pub(crate) fn file_rows<'n, S: NamesSource<'n>>(names: S, krate: usize, rows: &[ImplRow<'n>], each: &mut dyn FnMut(Filing)) -> FileCounts {
    let mut counts = FileCounts::default();
    let mut kept = RowNames { run: None, selves: HashMap::new_in(Global) };
    let mut impls: Vec<super::index::ImplAt<'n>, Global> = Vec::new_in(Global);
    for row in rows.iter() {
        // A refused derive's gap (D193 point 5, blaze-scope `ImplRow::refused_call`): no impl
        // item, so no text to read; filed by what its row reads (`textless`: a trait impl of an
        // unread trait at its self type's shape), its header the gap that names the refusal
        // (check.rs `lower_heads`).
        if row.refused_call {
            each(textless(names, row, &mut kept));
            continue;
        }
        match file_row(names, row, &mut kept) {
            Ok((filed, blanket, path_self)) => {
                counts.from_rows += 1;
                each(Filing { key: row.key, filed, blanket, by_text: false, unread_bare: false, path_self });
            }
            Err(miss) => {
                counts.row_misses[miss as usize] += 1;
                match names.impl_with_header(krate, row.key, row.module) {
                    Some(at) => impls.push(at),
                    // Its text did not read (the source filed why): unread, filed by what its
                    // row read.
                    None => each(textless(names, row, &mut kept)),
                }
            }
        }
    }
    // By key: the order impls are filed in is a function of the records.
    impls.sort_by_key(|at| at.key);
    counts.from_text = impls.len() as u32;
    for at in impls.iter() {
        each(file_text(names, at));
    }
    counts
}

/// Files the impl `at` by its header as written. The kind as written decides first: an impl
/// whose text names a trait is filed as that trait's even when its lowered header holds no
/// trait (`Filed::Inherent`) or did not lower (`NoText`, `Unlowered`, `Generics`): it is then
/// never a candidate of an inherent question. Only an impl whose header text does not read
/// stays blind.
fn file_text<'n, S: NamesSource<'n>>(names: S, at: &super::index::ImplAt<'n>) -> Filing {
    let (written, blanket) = written_trait(names, at);
    let (filed, path_self, by_text) = match (file(names, at.key, at.module), written) {
        // Filed by its text's trait, its self type not read: not bounded.
        ((Filed::Inherent(_), _), Some(t)) => (file_by_text(names, at.module, t), None, true),
        ((Filed::Unread(why, _), _), Some(t)) if !why.is_trait_impl() => (file_by_text(names, at.module, t), None, true),
        ((f, path_self), _) => (f, path_self, false),
    };
    Filing { key: at.key, filed, blanket, by_text, unread_bare: written.is_some_and(bare_text), path_self }
}

/// Files the impl of `row` whose row did not file it ([`file_row`]) and whose header text did
/// not read: unread, by what its row read. A row whose header's tokens read says whether the
/// impl is a trait impl (the Reference, "Implementations": `impl Trait for Type` against
/// `impl Type`) and writes its self type's head: a trait impl whose head the row resolves to a
/// constructor is a candidate of every trait question AT THAT SHAPE only, since a self type
/// whose outermost constructor is `C` unifies with no type whose outermost constructor is
/// another (an `impl Tr for Exit` answers nothing of `u8`). Only where the row read that
/// constructor: a header whose tokens did not read, an inherent impl, and a self head the row
/// does not name (`SelfHead::Other`, a path of no plain identifiers) stay candidates of every
/// question, as `file` files an impl with no text.
fn textless<'n, S: NamesSource<'n>>(names: S, row: &ImplRow<'n>, kept: &mut RowNames<'n>) -> Filing {
    let filing = |filed: Filed, unread_bare: bool| Filing { key: row.key, filed, blanket: false, by_text: false, unread_bare, path_self: None };
    if row.header_unread || !row.has_trait {
        return filing(Filed::Unread(Unread::NoText, None), false);
    }
    match row_self(names, row, kept) {
        Ok(RowSelf::Shape(shape, _)) => filing(Filed::Unread(Unread::NoTextTrait, shape), row.bare),
        // No question the checker asks is at a type it has none for.
        Ok(RowSelf::NoCheckerType) => filing(Filed::NoCheckerType, false),
        Err(_) => filing(Filed::Unread(Unread::NoTextTrait, None), row.bare),
    }
}

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
    /// Which crate's local types the question's self type can be: judges the impls whose self
    /// path was read to no constructor ([`PathSelf`], [`Buckets::bounded`]), which the
    /// coherence rules let apply only at their own crate's local types.
    pub local: Local,
}

/// Which crate's local types the self type of a question can be, read from its shape alone
/// ([`trait_local`], [`inherent_local`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Local {
    /// Any crate's (a reference or a fundamental wrapper of a type not seen in the shape, a
    /// nominal type whose crate or marks were not read): no bounded impl is dropped.
    Any,
    /// This crate's only: a nominal type, an extern type or a trait object of a trait defined
    /// there.
    In(usize),
    /// No crate's: a primitive, a tuple, an array, a slice, a pointer, a fn pointer or item,
    /// a closure, a parameter, an opaque type, a pattern type, `dyn` of auto traits only.
    Nowhere,
}

impl Homes {
    pub const ANY: Homes = Homes { all: Home::Any, bare: Home::Any, blanket: Home::Any, local: Local::Any };

    /// An inherent question's: no inherent impl is a blanket impl (`impl<T> T` is refused by
    /// the language, E0118), so a blanket trait impl is judged as any other impl there.
    fn one(h: Home) -> Homes {
        Homes { all: h, bare: h, blanket: h, local: Local::Any }
    }
}

/// The crate of the nominal type, extern type or trait `k` names, `None` where it is not read.
fn crate_of_checker<'s, 'n, S: NamesSource<'n>>(facts: BodyHeadFacts<'_, 's, S>, keys: &Keys, k: ItemKey) -> Option<usize> {
    facts.crate_of(facts.library(keys, k)?)
}

/// [`Homes::local`] of a trait question at `shape`: a local type is a nominal type (or a trait
/// object) of a crate, or `&T`, `&mut T`, `Box<T>`, `Pin<T>` of one (the Reference, "Orphan
/// rules", fundamental types), so a reference or a `#[fundamental]` type may be any crate's.
fn trait_local<'s, 'n, S: NamesSource<'n>>(facts: BodyHeadFacts<'_, 's, S>, keys: &Keys, shape: Shape, marks: Option<CoherenceMarks>) -> Local {
    match shape {
        Shape::Adt(k) => {
            match (marks, crate_of_checker(facts, keys, k)) {
                (Some(m), Some(c)) if !m.fundamental => Local::In(c),
                _ => Local::Any,
            }
        }
        Shape::Foreign(k) | Shape::Dynamic(Some(k)) => crate_of_checker(facts, keys, k).map_or(Local::Any, Local::In),
        Shape::Ref(_) => Local::Any,
        Shape::Bool
        | Shape::Char
        | Shape::Int(_)
        | Shape::Uint(_)
        | Shape::Float(_)
        | Shape::AnyInt
        | Shape::AnyFloat
        | Shape::Str
        | Shape::Never
        | Shape::Tuple(_)
        | Shape::Record(..)
        | Shape::Union(_)
        | Shape::Slice
        | Shape::Array
        | Shape::Ptr(_)
        | Shape::FnPtr(_)
        | Shape::FnDef(_)
        | Shape::Closure
        | Shape::Param
        | Shape::Opaque(_)
        | Shape::Pat
        | Shape::Dynamic(None) => Local::Nowhere,
    }
}

/// [`Homes::local`] of an inherent question at `shape`: an inherent impl outside the crates that
/// may write incoherent ones is of a type its crate defines, never of a reference or a
/// primitive (E0116, E0390), and `Box<T>` is alloc's type whatever `T` is.
fn inherent_local<'s, 'n, S: NamesSource<'n>>(facts: BodyHeadFacts<'_, 's, S>, keys: &Keys, shape: Shape) -> Local {
    match shape {
        Shape::Adt(k) | Shape::Foreign(k) | Shape::Dynamic(Some(k)) => crate_of_checker(facts, keys, k).map_or(Local::Any, Local::In),
        Shape::Bool
        | Shape::Char
        | Shape::Int(_)
        | Shape::Uint(_)
        | Shape::Float(_)
        | Shape::AnyInt
        | Shape::AnyFloat
        | Shape::Str
        | Shape::Never
        | Shape::Tuple(_)
        | Shape::Record(..)
        | Shape::Union(_)
        | Shape::Slice
        | Shape::Array
        | Shape::Ref(_)
        | Shape::Ptr(_)
        | Shape::FnPtr(_)
        | Shape::FnDef(_)
        | Shape::Closure
        | Shape::Param
        | Shape::Opaque(_)
        | Shape::Pat
        | Shape::Dynamic(None) => Local::Nowhere,
    }
}

/// Which declarations supply the orphan-rule inputs of a question. An inherent
/// question about a trait object reads the principal trait's own coherence marks.
fn coherence_wants<'s, 'n, S: NamesSource<'n>>(facts: BodyHeadFacts<'_, 's, S>, keys: &Keys, ask: Ask) -> [Option<Want>; 2] {
    let (trait_, shape) = match ask {
        Ask::Trait(t, shape) => (facts.library(keys, t).map(Want::Head), shape),
        Ask::Inherent(shape) => (None, shape),
    };
    let nominal = match shape {
        // An ADT's field length can itself name one of its inherent constants.
        // The declaration already supplies its marks before those fields have types.
        Shape::Adt(k) if matches!(ask, Ask::Inherent(_)) => facts.library(keys, k).map(Want::Head),
        Shape::Adt(k) => facts.library(keys, k).map(Want::Adt),
        Shape::Dynamic(Some(k)) if matches!(ask, Ask::Inherent(_)) => facts.library(keys, k).map(Want::Head),
        _ => None,
    };
    [trait_, nominal]
}

pub(super) fn prepare_coherence<'n, S: NamesSource<'n>, H: super::check::ImplHeads<'n>>(
    names: S, heads: &mut H, ask: Ask,
) -> Result<super::check::Made<H::Later>, HeadError> {
    use super::check::Made;
    let wants = coherence_wants(BodyHeadFacts::new(heads.store(), names), heads.keys(), ask);
    for want in wants.into_iter().flatten() {
        if let Made::Later(why) = heads.make(want)? {
            return Ok(Made::Later(why));
        }
    }
    Ok(Made::Made)
}

/// Prepare a standalone index question over its chosen store. A body uses the same
/// wants through its head importer before asking for the home.
pub fn prepare_home<'n, S: NamesSource<'n>>(names: S, keys: &mut Keys, store: &mut Store<'n>, ask: Ask) -> Result<(), HeadError> {
    let mut heads = super::check::Lowering { names, keys, store };
    match prepare_coherence(names, &mut heads, ask)? {
        super::check::Made::Made => Ok(()),
        super::check::Made::Later(never) => match never {},
    }
}

fn adt_marks<'s, 'n, S: NamesSource<'n>>(facts: BodyHeadFacts<'_, 's, S>, want: Want, shape: Shape) -> Result<CoherenceMarks, HeadError> {
    use super::head_record::HeadAt;
    let lib = want.key();
    let at = if matches!(shape, Shape::Adt(_)) { HeadAt::Adt(lib) } else { HeadAt::Trait(lib) };
    facts.marks(lib).ok_or(HeadError::Filed { want, at, what: "the coherence marks are missing" })
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
/// [`Homes::local`] is the shape's alone ([`trait_local`]), whatever the trait's crates are.
pub fn trait_home<'s, 'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'s>, trait_: ItemKey, shape: Shape) -> Result<Homes, HeadError> {
    let facts = BodyHeadFacts::new(store, names);
    let [trait_want, nominal] = coherence_wants(facts, keys, Ask::Trait(trait_, shape));
    let marks = nominal.map(|w| adt_marks(facts, w, shape)).transpose()?;
    let local = trait_local(facts, keys, shape, marks);
    let Some(want) = trait_want else { return Ok(Homes { local, ..Homes::ANY }) };
    let lib = want.key();
    let parameters = facts.trait_parameters(lib).ok_or(HeadError::Filed {
        want, at: super::head_record::HeadAt::Trait(lib), what: "the trait's generic parameters are missing",
    })?;
    Ok(Homes { local, ..trait_crates(facts.crate_of(lib), shape, local, parameters) })
}

/// [`trait_home`]'s crates (`all`, `bare`, `blanket`).
fn trait_crates(home: Option<usize>, shape: Shape, local_type: Local, parameters: (u32, bool)) -> Homes {
    let Some(home) = home else { return Homes::ANY };
    // A blanket impl is written in its trait's crate whatever the goal's self type is
    // ([`Homes::blanket`]): the one narrowing that needs nothing but the trait's crate.
    let wide = Homes { blanket: Home::In(home, None), ..Homes::ANY };
    let local = match shape {
        Shape::Adt(_) | Shape::Foreign(_) | Shape::Dynamic(Some(_)) => {
            let Local::In(c) = local_type else { return wide };
            Some(c)
        }
        // Never a local type of any crate (no fundamental type is built in but references).
        // Matches `trait_local`: unnameable and placeholder shapes (`Param`, `Closure`, `FnDef`,
        // `Opaque`, `Pat`, `Dynamic(None)`) are also never local to any downstream crate, so a
        // 1-parameter trait narrows to `home` alone instead of scanning every linked crate.
        Shape::Bool
        | Shape::Char
        | Shape::Int(_)
        | Shape::Uint(_)
        | Shape::Float(_)
        | Shape::AnyInt
        | Shape::AnyFloat
        | Shape::Str
        | Shape::Never
        | Shape::Tuple(_)
        | Shape::Record(..)
        | Shape::Union(_)
        | Shape::Slice
        | Shape::Array
        | Shape::Ptr(_)
        | Shape::FnPtr(_)
        | Shape::FnDef(_)
        | Shape::Closure
        | Shape::Param
        | Shape::Opaque(_)
        | Shape::Pat
        | Shape::Dynamic(None) => None,
        Shape::Ref(_) => return wide,
    };
    let by_self = Home::In(home, local);
    match parameters {
        (1, _) => Homes { all: by_self, bare: by_self, ..wide },
        (_, true) => Homes { bare: by_self, ..wide },
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
/// [`Homes::local`] is the shape's alone ([`inherent_local`]).
pub fn inherent_home<'s, 'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'s>, shape: Shape) -> Result<Homes, HeadError> {
    let facts = BodyHeadFacts::new(store, names);
    let [_, nominal] = coherence_wants(facts, keys, Ask::Inherent(shape));
    let marks = nominal.map(|w| adt_marks(facts, w, shape)).transpose()?;
    Ok(Homes { local: inherent_local(facts, keys, shape), ..inherent_crates(facts, keys, shape, marks) })
}

/// [`inherent_home`]'s crates.
fn inherent_crates<'s, 'n, S: NamesSource<'n>>(facts: BodyHeadFacts<'_, 's, S>, keys: &Keys, shape: Shape, marks: Option<CoherenceMarks>) -> Homes {
    let nominal = match shape {
        Shape::Adt(k) | Shape::Dynamic(Some(k)) => k,
        Shape::Bool | Shape::Char | Shape::Int(_) | Shape::Uint(_) | Shape::Float(_) | Shape::AnyInt | Shape::AnyFloat | Shape::Str | Shape::Never | Shape::Tuple(_) | Shape::Record(..) | Shape::Union(_) | Shape::Slice | Shape::Array | Shape::Ptr(_) | Shape::Ref(_) | Shape::FnPtr(_) => {
            return Homes::one(Home::Incoherent);
        }
        _ => return Homes::ANY,
    };
    let Some(lib) = facts.library(keys, nominal) else { return Homes::ANY };
    match marks {
        Some(m) if !m.incoherent => {}
        Some(_) => return Homes::one(Home::Incoherent),
        None => return Homes::ANY,
    }
    match facts.crate_of(lib) {
        Some(c) => Homes::one(Home::In(c, None)),
        None => Homes::ANY,
    }
}

/// Whether a negative impl filed at `filed` (`None`: any self type) can answer a question at
/// `asked`: as the positive buckets are filed (`put`), a blanket one always, one at a shape
/// for that shape, an integer or float one for `{integer}` or `{float}`.
fn negative_fits(filed: Option<Shape>, asked: Shape) -> bool {
    match filed {
        None => true,
        Some(s) => s == asked || (asked == Shape::AnyInt && matches!(s, Shape::Int(_) | Shape::Uint(_))) || (asked == Shape::AnyFloat && matches!(s, Shape::Float(_))),
    }
}

fn push<K: core::hash::Hash + Eq, V>(m: &mut HashMap<K, Vec<V, Global>, Global>, k: K, v: V) {
    m.entry(k).or_insert_with(|| Vec::new_in(Global)).push(v);
}

/// The impls filed by their headers as written, and the answers made so far.
///
/// FILING IS CONSTANT TIME PER IMPL, and nothing filed is ever sorted again (lane BK,
/// 2026-11-17). Each set an impl is looked up in by key (its crate, unread, bare, blanket,
/// bounded, by text, inherent "any") is a hash set or map, so filing one more impl inserts one
/// entry and a lookup is one probe. Before, these were vectors kept sorted for a binary search,
/// and every [`Buckets::admit_asked`] that filed anything sorted ALL of them again: a body job's
/// N questions cost N sorts of lists that grow with the job's impls, quadratic in its questions
/// (tempo's cold `blaze check`, sample of g082400 at 08:50: `quicksort<(ItemKey, usize)>` under
/// `admit_asked` 855 samples, `quicksort<ItemKey>` under `sort_filed` 400, of one busy worker's
/// 2,361). What a question reads in order is made per question and sorted there, once: its
/// candidate list ([`Buckets::trait_candidates`], [`Buckets::inherent_candidates`]) and its
/// read list; a set's iteration order reaches nothing but such a list, so no answer depends on
/// the order impls were filed in ([`Buckets::bucket_lookups`] and the test
/// tests/front2_buckets_order.rs).
///
/// FILLED THROUGH `&self` (lane RA1, plan row A1): a seeded body's world answers an impl
/// question on the spot (world_items.rs `BodyWorld::impl_question`) while the check holds
/// answers borrowed from here (`World::impls_of` hands out `&[ItemKey]`, `World::inherent`
/// `&[AssocItem]`). So the four answer tables are [`Rows`] (a row filed never moves; a reader's
/// slice stays), and every other field is in ONE [`BucketIndex`] behind one `RefCell`, borrowed
/// only through [`Buckets::index_mut`] (its comment says what may not be called inside the
/// borrow) or [`Buckets::index`], and never across a return to the world. The lowering path
/// (between rounds, `&mut Buckets`) reaches the index through [`Buckets::index_get_mut`], which
/// takes no runtime borrow. No `unsafe`.
pub struct Buckets {
    /// Each trait question's answer, by the trait, the shape and whose impls it may hold
    /// ([`Seen`]).
    answers: Rows<(ItemKey, Shape, Seen), Vec<ItemKey, Global>>,
    /// Per answered trait question, the least-keyed candidate whose header failed: the
    /// question's gap. Only a question that HAS such a candidate is a gap of the body asking it,
    /// so a body's outcome never depends on which other bodies or rounds lowered what first.
    answer_failed: Rows<(ItemKey, Shape, Seen), LibKey>,
    /// Trait questions whose answer read a provisional impl header during constant evaluation
    /// (`Store::provisional_impls`): cleared once the waited-on constant finishes so the final
    /// `ImplHead` is lowered and cached.
    provisional_answers: OnceCell<RefCell<blaze_own::Vec<(ItemKey, Shape, Seen), blaze_own::Alloc<'static>>>>,
    /// Each crate's sight ([`SightRow`]), made once at its first checked body's first trait
    /// question ([`Buckets::sight`]) in the store's allocator, which that question names.
    sights: OnceCell<Rows<usize, SightRow, blaze_own::Alloc<'static>>>,
    /// A test's removed cause ([`Buckets::plant_whole_program_sight`]): every question sees
    /// every crate's impls, as before answers were keyed by what they see.
    whole_program_sight: bool,
    inherent: Rows<(Shape, Name), Vec<AssocItem, Global>>,
    /// The answered inherent questions (a shape and a name), each with its least-keyed failed
    /// candidate, if any: only an impl that declares a member of the name is a candidate.
    inherent_done: Rows<(Shape, Name), Option<LibKey>>,
    index: RefCell<BucketIndex>,
}

/// Per-crate cached facts on [`BucketIndex`].
#[derive(Clone, Copy)]
pub(super) struct CrateState {
    /// Whether the crate's root turns on `rustc_attrs` ([`Home::Incoherent`]).
    pub(super) incoherent: bool,
    /// Cached `linked::links_unit(krate == 0, names.proc_macro(krate) == Some(true))` so
    /// repeated impl questions do not re-query `names.proc_macro(krate)` on every crate.
    pub(super) linked: Option<bool>,
}

/// What [`Buckets`] files impls in and counts: every field but the answers, mutated through
/// one `RefCell` ([`Buckets`]'s docs).
pub struct BucketIndex {
    by_shape: HashMap<(ItemKey, Shape), Vec<LibKey, Global>, Global>,
    any: HashMap<ItemKey, Vec<LibKey, Global>, Global>,
    /// Every unread impl ([`Buckets::is_unread`]).
    unread: HashSet<LibKey, Global>,
    /// Trait impls whose trait path failed, by their self type's shape where it was read
    /// (then also under `AnyInt`/`AnyFloat`), else in `unread_any`: candidates of trait
    /// questions at that shape only, never of inherent ones.
    unread_by_shape: HashMap<Shape, Vec<LibKey, Global>, Global>,
    unread_any: Vec<LibKey, Global>,
    /// Impls not read far enough to tell trait from inherent (no text, not lowered, generics
    /// refused): candidates of every question.
    unread_blind: Vec<LibKey, Global>,
    inherent_by_shape: HashMap<Shape, Vec<LibKey, Global>, Global>,
    /// Inherent impls whose self type's shape was not read ([`Buckets::is_unsure`]).
    inherent_any: HashSet<LibKey, Global>,
    /// How many impl headers the questions made the store lower.
    pub lowered: u32,
    /// Impls on a self type the checker has no type for (blaze's `i80`), filed nowhere.
    pub no_checker_type: u32,
    /// The unread candidates whose header failed, with why (sorted by key): left out of the
    /// answers; a gap of each body that asks a question they are candidates of.
    pub failed: Vec<(LibKey, Box<str>), Global>,
    /// Why each unread impl's trait path was not filed (in filing order).
    pub unread_why: Vec<(LibKey, Unread), Global>,
    /// The negative impls, by trait ([`Filed::Negative`]), each with its self type's shape
    /// (`None`: any), in filing order: no trait question's candidates.
    negative: HashMap<ItemKey, Vec<(LibKey, Option<Shape>), Global>, Global>,
    /// Each impl's crate, by key: known without reading its header.
    krates: HashMap<LibKey, usize, Global>,
    /// The home each answered trait question was narrowed to ([`trait_home`]).
    homes: HashMap<(ItemKey, Shape), Homes, Global>,
    /// The trait impls whose trait path is written with no generic arguments.
    bare: HashSet<LibKey, Global>,
    /// Per crate: whether its root turns on `rustc_attrs` ([`Home::Incoherent`]) and whether
    /// it is a linked (non-proc-macro or root) unit.
    crate_state: Vec<CrateState, Global>,
    /// The trait impls filed by their header's text ([`file_by_text`]) because the lowered
    /// header did not name their trait: each is a defect of the lowering or of the block's
    /// module to remove at its home, counted here so it is seen. A set: [`Buckets::answer`]
    /// asks it of each candidate (it was a vector searched by halves that, over
    /// [`Buckets::lazy`], was never sorted across crates, so a search could miss a member).
    pub by_text: HashSet<LibKey, Global>,
    /// The blanket trait impls (`impl<T> Tr<..> for T`, by their header's tokens:
    /// [`written_trait`]), read or unread: each is judged by [`Homes::blanket`], so an
    /// unread one is a candidate only of questions about a trait of its own crate.
    blanket: HashSet<LibKey, Global>,
    /// The impls whose self path was read to no constructor that the coherence rules bound
    /// to their crate's local types ([`PathSelf`]), each with its crate: filed at every
    /// shape, but a candidate only of a question whose [`Homes::local`] may be that crate's,
    /// so a refused one is no gap of a question at a primitive, a parameter or a type of
    /// another crate.
    bounded: HashMap<LibKey, usize, Global>,
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
    /// The cells already offered to this body, allocated by its caller on the first lookup.
    table_reads: Option<BucketReadUnion<blaze_own::Alloc<'static>>>,
    /// Cached `Home::Any` `NoImplsOf` precheck results per trait, allocated in the body's
    /// scratch allocator on the first `Home::Any` trait lookup.
    tables_cache: Option<records::TablesCache>,
}

impl Buckets {
    /// No impl filed, no crate known, no question answered.
    fn of(index: BucketIndex) -> Buckets {
        Buckets {
            answers: Rows::new_in(Global),
            answer_failed: Rows::new_in(Global),
            provisional_answers: OnceCell::new(),
            sights: OnceCell::new(),
            whole_program_sight: false,
            inherent: Rows::new_in(Global),
            inherent_done: Rows::new_in(Global),
            index: RefCell::new(index),
        }
    }

    /// The impl index the names source `names` is answered from: over the solved names every
    /// impl filed now ([`Buckets::new`]), over records none ([`Buckets::lazy`]). The one maker
    /// both paths call: check.rs `make_once` on the first impl fill, and `check_rounds_as`
    /// before round 0 of a seeded body.
    pub fn for_source<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>) -> Buckets {
        match names.solved() {
            Some(solved) => Buckets::new(solved, index),
            None => Buckets::lazy(names),
        }
    }

    /// Files every impl of `index` by its header as written.
    pub fn new(names: &Names<'_>, index: &Index<'_>) -> Buckets {
        let mut b = BucketIndex::empty();
        b.krates.extend(index.impls.iter().map(|at| (at.key, at.krate)));
        // `#![feature(rustc_attrs)]` is an inner attribute of the crate root, the first file
        // read; a mention anywhere in that text keeps the crate on the wide side.
        b.crate_state.extend(names.crates().iter().map(|c| CrateState {
            incoherent: blaze_scope::items_record::root_may_write_incoherent(c),
            linked: None,
        }));
        b.file_all(names, &index.impls);
        // Each refused derive's gap (D193 point 5), as the items record writes it: the solve's
        // values give the same rows (blaze-scope `each_refused_impl_row`, the one home).
        for (krate, c) in names.crates().iter().enumerate() {
            let mut gaps: Vec<ImplRow<'_>, Global> = Vec::new_in(Global);
            blaze_scope::items_record::each_refused_impl_row(c, &mut |row| gaps.push(row));
            if gaps.is_empty() {
                continue;
            }
            // By key: the order impls are filed in is a function of the names.
            gaps.sort_unstable_by_key(|row| row.key);
            let counts = file_rows(names, krate, &gaps, &mut |f| b.take(names, krate, f));
            b.count(counts);
        }
        Buckets::of(b)
    }

    /// The front end over records: no impl is filed when this is made. Before each question,
    /// [`Buckets::admit`] files the impls of the crates the question's home admits, from the
    /// source's impl rows ([`NamesSource::each_impl`]); a crate is filed once.
    pub fn lazy<'n, S: NamesSource<'n>>(names: S) -> Buckets {
        let mut b = BucketIndex::empty();
        let crates = names.crate_count();
        // Unknown until asked ([`Buckets::admit`]): the wide side, as `admits` reads a crate
        // with no row.
        b.crate_state.resize(crates, CrateState { incoherent: true, linked: None });
        b.lazy_filed.resize(crates, false);
        Buckets::of(b)
    }

    /// The index, read: a shared borrow of the one cell, held by the caller only while it
    /// reads (never across a call that files: that would be [`Buckets::index_mut`] inside it,
    /// a panic by `RefCell`'s rule, which no path here takes).
    pub fn index(&self) -> Ref<'_, BucketIndex> {
        self.index.borrow()
    }

    /// The index, to file in, through a shared borrow of the buckets: THE ONE MUTABLE BORROW of
    /// the cell. Each caller holds it over straight-line code that files or counts and calls
    /// only the index's own methods, the names source (`NamesSource`: the task's recorder and
    /// the records, which never reach a body's buckets) and the impl tables' lookups; NEVER the
    /// world (a `World` method), the store's seeding (`HeadSeed::seed`) or any other method of
    /// these buckets, any of which could ask for the cell again and panic. No caller returns the
    /// guard or holds it across a head being made.
    fn index_mut(&self) -> RefMut<'_, BucketIndex> {
        self.index.borrow_mut()
    }

    /// The index through `&mut self`: no runtime borrow (the lowering path between rounds, and
    /// tests that file by hand).
    pub fn index_get_mut(&mut self) -> &mut BucketIndex {
        self.index.get_mut()
    }

    /// THE CAUSE REMOVED, for tests/front2_records.rs only ([`BucketIndex::plant_no_rows`]).
    #[doc(hidden)]
    pub fn plant_no_rows(&mut self) {
        self.index_get_mut().plant_no_rows();
    }

    /// [`BucketIndex::admit`], through `&mut self`.
    pub fn admit<'n, S: NamesSource<'n>>(&mut self, names: S, home: Homes) -> usize {
        self.index_get_mut().admit(names, home)
    }

    /// [`BucketIndex::admit_asked`], through `&mut self`.
    pub fn admit_asked<'n, S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, home: Homes, ask: Ask) -> Result<usize, AdmitError> {
        self.index_get_mut().admit_asked(names, keys, home, ask)
    }

    /// [`BucketIndex::admit_tables`], through `&mut self`.
    pub fn admit_tables<'n, S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, home: Homes, ask: Ask, tables: &TableSet<'_>, alloc: blaze_own::Alloc<'static>) -> Result<usize, AdmitError> {
        self.index_get_mut().admit_tables(names, keys, home, ask, tables, alloc)
    }

    /// Files what `ask` at `home` is answered from, through `&self` (the on-the-spot path and
    /// the lowering path both; check.rs `trait_question`, `inherent_question`): from the impl tables when the names
    /// source holds them (buckets/records.rs), else whole crates by their rows and the items
    /// records' bucket section. One borrow of the index ([`Buckets::index_mut`]) over the
    /// admit: inside it only the names source and the tables are called. The bucket cells read
    /// are recorded inside `admit_tables` (`names.impls_read`), on either path.
    pub fn admit_shared<'n, S: NamesSource<'n>>(&self, names: S, keys: &Keys, home: Homes, ask: Ask, alloc: blaze_own::Alloc<'static>) -> Result<usize, AdmitError> {
        let mut ix = self.index_mut();
        match names.impl_tables() {
            Some(tables) => ix.admit_tables(names, keys, home, ask, tables, alloc),
            None => {
                let whole = ix.admit(names, home);
                Ok(whole + ix.admit_asked(names, keys, home, ask)?)
            }
        }
    }

    pub fn is_lazy(&self) -> bool {
        self.index().is_lazy()
    }

    pub fn trait_candidates(&self, trait_: ItemKey, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        self.index().trait_candidates(trait_, shape, home)
    }

    pub fn negative_of(&self, trait_: ItemKey, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        self.index().negative_of(trait_, shape, home)
    }

    pub fn inherent_candidates(&self, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        self.index().inherent_candidates(shape, home)
    }

    pub fn admits(&self, home: Home, krate: usize) -> bool {
        self.index().admits(home, krate)
    }

    pub fn krate(&self, k: LibKey) -> Option<usize> {
        self.index().krate(k)
    }

    pub fn home_of(&self, trait_: ItemKey, shape: Shape) -> Homes {
        self.index().home_of(trait_, shape)
    }

    pub fn is_unread(&self, k: LibKey) -> bool {
        self.index().is_unread(k)
    }

    pub fn is_unsure(&self, k: LibKey) -> bool {
        self.index().is_unsure(k)
    }

    pub fn has_failed(&self, k: LibKey) -> bool {
        self.index().has_failed(k)
    }

    /// Records an unread candidate whose header failed ([`BucketIndex::fail`]).
    pub fn fail(&self, k: LibKey, why: Box<str>) {
        self.index_mut().fail(k, why);
    }

    /// Counts one impl header a question made the store lower (or import).
    pub fn count_lowered(&self) {
        self.index_mut().lowered += 1;
    }

    pub fn filed(&self) -> [usize; 5] {
        self.index().filed()
    }

    pub(crate) fn trait_buckets(&self) -> usize {
        self.index().trait_buckets()
    }

    /// [`BucketIndex::each_trait_bucket`]; `each` must not reach these buckets' filing (the
    /// index is borrowed while it runs).
    pub(crate) fn each_trait_bucket(&self, each: impl FnMut(ItemKey, Shape)) {
        self.index().each_trait_bucket(each);
    }

    pub(crate) fn each_unread_shape(&self, each: impl FnMut(Shape)) {
        self.index().each_unread_shape(each);
    }

    /// The unread impls filed at no shape ([`BucketIndex::shapeless_unread`]), in filing order.
    pub(crate) fn shapeless_unread(&self) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        out.extend(self.index().shapeless_unread());
        out
    }

    pub fn trait_reads(&self, trait_: ItemKey, shape: Shape) -> blaze_own::Vec<BucketRead, Global> {
        self.index().trait_reads(trait_, shape)
    }

    pub(super) fn with_trait_reads<R>(&self, trait_: ItemKey, shape: Shape, f: impl FnOnce(&[BucketRead], Option<(&[BucketRead], &[BucketRead])>) -> R) -> R {
        self.index().with_trait_reads(trait_, shape, f)
    }

    pub fn inherent_reads(&self, shape: Shape) -> Ref<'_, [BucketRead]> {
        Ref::map(self.index(), |ix| ix.inherent_reads(shape))
    }

    pub fn asked(&self) -> Vec<Ask, Global> {
        self.index().asked()
    }

    pub fn filed_from_index(&self) -> u32 {
        self.index().filed_from_index()
    }

    pub fn bucket_lookups(&self) -> u32 {
        self.index().bucket_lookups()
    }

    pub fn bucket_asks(&self) -> u32 {
        self.index().bucket_asks()
    }

    pub fn crates_asked(&self) -> usize {
        self.index().crates_asked()
    }

    pub fn crates_asked_sorted(&self) -> blaze_own::Vec<usize, Global> {
        self.index().crates_asked_sorted()
    }

    pub fn not_filed(&self) -> [(&'static str, u32); blaze_scope::items_record::buckets::NOT_FILED.len()] {
        self.index().not_filed()
    }
}

impl BucketIndex {
    /// No impl filed, no crate known.
    fn empty() -> BucketIndex {
        BucketIndex {
            by_shape: HashMap::new_in(Global),
            any: HashMap::new_in(Global),
            unread: HashSet::new_in(Global),
            unread_by_shape: HashMap::new_in(Global),
            unread_any: Vec::new_in(Global),
            unread_blind: Vec::new_in(Global),
            inherent_by_shape: HashMap::new_in(Global),
            inherent_any: HashSet::new_in(Global),
            lowered: 0,
            no_checker_type: 0,
            failed: Vec::new_in(Global),
            unread_why: Vec::new_in(Global),
            negative: HashMap::new_in(Global),
            krates: HashMap::new_in(Global),
            homes: HashMap::new_in(Global),
            bare: HashSet::new_in(Global),
            crate_state: Vec::new_in(Global),
            by_text: HashSet::new_in(Global),
            blanket: HashSet::new_in(Global),
            bounded: HashMap::new_in(Global),
            lazy_filed: Vec::new_in(Global),
            filed_from_rows: 0,
            filed_from_text: 0,
            row_misses: [0; 6],
            no_rows: false,
            table_reads: None,
            tables_cache: None,
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
        // `local` judges filed impls, never which crates are filed.
        let Homes { all, bare, blanket, local: _ } = home;
        let mut filed = 0;
        for krate in 0..self.lazy_filed.len() {
            if self.lazy_filed[krate] {
                continue;
            }
            if [all, bare, blanket].contains(&Home::Incoherent) {
                // Not answered (no record: the source filed it): the wide side.
                self.crate_state[krate].incoherent = names.may_write_incoherent(krate).unwrap_or(true);
            }
            if !(self.admits(all, krate) || self.admits(bare, krate) || self.admits(blanket, krate)) {
                continue;
            }
            self.lazy_filed[krate] = true;
            filed += 1;
            // A proc-macro unit the program reaches answers none of its questions; the
            // program's own crate (0) does, a proc macro's too (linked.rs `links_unit`).
            if !self.crate_linked(names, krate) {
                continue;
            }
            let mut rows: Vec<ImplRow<'n>, Global> = Vec::new_in(Global);
            if !self.no_rows && names.each_impl_row(krate, &mut |row| rows.push(row)) {
                let counts = file_rows(names, krate, &rows, &mut |f| self.take(names, krate, f));
                self.count(counts);
            } else {
                // Every impl of a source with no impl rows is read from its header's text.
                let mut impls: Vec<super::index::ImplAt<'n>, Global> = Vec::new_in(Global);
                names.each_impl(krate, &mut |at| impls.push(at));
                // By key: the order impls are filed in is a function of the records.
                impls.sort_by_key(|at| at.key);
                self.filed_from_text += impls.len() as u32;
                self.file_all(names, &impls);
            }
        }
        filed
    }

    /// Files each impl of `impls` by its header as written.
    fn file_all<'n, S: NamesSource<'n>>(&mut self, names: S, impls: &[super::index::ImplAt<'n>]) {
        for at in impls.iter() {
            let f = file_text(names, at);
            self.take(names, at.krate, f);
        }
    }

    /// Files one impl of crate `krate` as [`file_rows`] or [`file_text`] read it: the one
    /// place a [`Filing`] lands in this index.
    fn take<'n, S: NamesSource<'n>>(&mut self, names: S, krate: usize, f: Filing) {
        self.krates.insert(f.key, krate);
        if f.blanket {
            self.blanket.insert(f.key);
        }
        if f.by_text {
            self.by_text.insert(f.key);
        }
        self.put(f.key, f.filed, f.unread_bare);
        self.bound(names, f.key, krate, f.path_self);
    }

    /// Adds what one [`file_rows`] call filed to the counts.
    fn count(&mut self, counts: FileCounts) {
        self.filed_from_rows += counts.from_rows;
        self.filed_from_text += counts.from_text;
        for (total, n) in self.row_misses.iter_mut().zip(counts.row_misses) {
            *total += n;
        }
    }

    /// Keeps `key` (an impl of crate `krate` just filed at every shape) in [`Buckets::bounded`]
    /// when what its header says (`path_self`) bounds it by the coherence rules: an inherent
    /// impl of a crate that may not write incoherent impls (as the source says; not said is
    /// the wide side), or a trait impl of another crate's trait written with no argument.
    fn bound<'n, S: NamesSource<'n>>(&mut self, names: S, key: LibKey, krate: usize, path_self: Option<PathSelf>) {
        let bounded = match path_self {
            None => false,
            Some(PathSelf::Inherent) => names.may_write_incoherent(krate) == Some(false),
            Some(PathSelf::Trait { of, bare }) => bare && of.is_some_and(|c| c != krate),
        };
        if bounded {
            self.bounded.insert(key, krate);
        }
    }

    /// Whether the impl `k` may answer a question whose self type can be local to `local`'s
    /// crates: always, unless `k` is bounded ([`Buckets::bounded`]) to a crate `local` excludes.
    fn may_be_local(&self, k: LibKey, local: Local) -> bool {
        let Some(&krate) = self.bounded.get(&k) else { return true };
        match local {
            Local::Any => true,
            Local::In(c) => c == krate,
            Local::Nowhere => false,
        }
    }

    /// Puts the impl `key` in the lists `filed` names: the one place an impl is filed, from
    /// its header's text ([`Buckets::file_all`]) or from its row ([`file_row`]).
    /// `unread_bare`: for an unread trait impl, whether its trait is written with no type or
    /// const argument ([`bare_text`]).
    fn put(&mut self, key: LibKey, filed: Filed, unread_bare: bool) {
        let b = self;
        if let Filed::Trait(_, _, true) = filed {
            b.bare.insert(key);
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
            Filed::Inherent(None) => {
                b.inherent_any.insert(key);
            }
            Filed::NoCheckerType => b.no_checker_type += 1,
            Filed::Negative(tr, shape) => push(&mut b.negative, tr, (key, shape)),
            Filed::Unread(why, shape) => {
                b.unread.insert(key);
                // An unread trait impl written with no type or const argument on its trait
                // is judged by `Homes::bare` as a read one is: if it is of the asked trait
                // at all, that trait's parameters are at their defaults, so where those
                // are `Self` only its self type can be its crate's local type.
                if why.is_trait_impl() && unread_bare {
                    b.bare.insert(key);
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
    /// `home.blanket` first: its trait's crate alone, the narrowest of the three. A bounded
    /// impl ([`Buckets::bounded`]) is also judged by `home.local`.
    fn keep_home(&self, out: &mut Vec<LibKey, Global>, home: Homes) {
        if home == Homes::ANY {
            return;
        }
        out.retain(|k| {
            let h = if self.blanket.contains(k) {
                home.blanket
            } else if self.bare.contains(k) {
                home.bare
            } else {
                home.all
            };
            self.krate(*k).is_none_or(|c| self.admits(h, c)) && self.may_be_local(*k, home.local)
        });
    }

    /// Whether `krate` is linked for impl lookup (root or non-proc-macro), memoized once per
    /// crate on this [`BucketIndex`] so repeated impl questions do not re-query `names.proc_macro`
    /// and re-offer `NameCell::Root` on every crate.
    pub(super) fn crate_linked<'n, S: NamesSource<'n>>(&mut self, names: S, krate: usize) -> bool {
        if let Some(linked) = self.crate_state.get(krate).and_then(|s| s.linked) {
            return linked;
        }
        let linked = super::linked::links_unit(krate == 0, names.proc_macro(krate) == Some(true));
        if let Some(slot) = self.crate_state.get_mut(krate) {
            slot.linked = Some(linked);
        }
        linked
    }

    /// Whether an impl written in crate `krate` can be in `home`.
    pub fn admits(&self, home: Home, krate: usize) -> bool {
        match home {
            Home::Any => true,
            Home::In(a, b) => krate == a || b == Some(krate),
            // A crate the names library has no row for stays on the wide side.
            Home::Incoherent => self.crate_state.get(krate).map_or(true, |s| s.incoherent),
        }
    }

    /// The crate impl `k` is written in.
    pub fn krate(&self, k: LibKey) -> Option<usize> {
        self.krates.get(&k).copied()
    }

    /// The home the answered trait question was narrowed to.
    pub fn home_of(&self, trait_: ItemKey, shape: Shape) -> Homes {
        self.homes.get(&(trait_, shape)).copied().unwrap_or(Homes::ANY)
    }

    /// The negative impls of `trait_` (any self type) written where `home` admits (the orphan
    /// rules bound a negative impl as they bound a positive one), sorted by key. Narrowed by
    /// the home like the candidates: before, every negative impl of the trait filed so far was
    /// one, so a question about `Bee` asked after one about `A` held `a`'s `impl !Tr for
    /// NotTr` (its crate filed by the first question) and asked first did not: its candidates,
    /// the headers lowered for it and so its gap depended on what the job asked before it
    /// (rule 21; tests/front2_buckets_order.rs found it).
    ///
    /// And narrowed by the shape, as the positive candidates are: only a negative impl whose
    /// self type can be `shape` (a blanket one, or one filed at `shape`; every integer width
    /// for `{integer}`). Over the impl tables a question reads only its shape's buckets, so
    /// the trait's other negatives are filed only by other questions; read whole, the answer
    /// depended on what was asked before (rule 21; tests/front2_buckets_order.rs found it).
    pub fn negative_of(&self, trait_: ItemKey, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        let filed = self.negative.get(&trait_).map_or(&[][..], |v| v.as_slice());
        out.extend(filed.iter().filter(|(_, at)| negative_fits(*at, shape)).map(|(k, _)| *k));
        self.keep_home(&mut out, home);
        out.sort_unstable();
        out
    }

    /// The candidates for an inherent fn of `shape`, each once.
    pub fn inherent_candidates(&self, shape: Shape, home: Homes) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(self.inherent_by_shape.get(&shape).map_or(&[], |v| v.as_slice()));
        out.extend(self.inherent_any.iter().copied());
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
        self.unread.contains(&k)
    }

    /// Whether `k` is a candidate of questions it may not answer: unread, or an inherent impl
    /// whose self type's shape was not read as written (an alias, a qualified path). Such a
    /// candidate's failed header is a listed gap of the body, not the end of its check.
    pub fn is_unsure(&self, k: LibKey) -> bool {
        self.is_unread(k) || self.inherent_any.contains(&k)
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

    /// How the impls were filed: (under a trait and a shape, under a trait's "any", inherent
    /// by shape, inherent "any", unread).
    pub fn filed(&self) -> [usize; 5] {
        let shaped = self.by_shape.iter().filter(|((_, s), _)| !matches!(s, Shape::AnyInt | Shape::AnyFloat)).map(|(_, v)| v.len()).sum();
        let any = self.any.values().map(|v| v.len()).sum();
        let inherent = self.inherent_by_shape.values().map(|v| v.len()).sum();
        [shaped, any, inherent, self.inherent_any.len(), self.unread.len()]
    }

    /// The failed candidate `k` with why (owned: the list it is in grows through `&self` of
    /// the buckets, so no borrow of it leaves the index).
    fn failed_why(&self, k: LibKey) -> Option<(LibKey, Box<str>)> {
        let at = self.failed.binary_search_by_key(&k, |f| f.0).ok()?;
        Some((k, self.failed[at].1.clone()))
    }
}

impl Buckets {
    /// Answers `trait_` at `shape` from its candidates' headers, which `store` must hold. A
    /// question is answered once a run, as [`Buckets::answer_inherent`] is: every body that
    /// reads it reads the first answer, its home and its gap (rule 21: one key, one answer).
    /// Through `&self`: the answer is a new row ([`Rows`]), so a slice an earlier answer handed
    /// out stays where it is.
    ///
    /// Only the impls of the crates `sight` sees are candidates ([`Seen`]): the answer is
    /// filed under its `seen`, so a body of core and a body of a crate downstream of it read
    /// two answers of `u32: PartialEq<_>`, each its own crate's, whichever is asked first.
    pub fn answer(&self, store: &Store<'_>, trait_: ItemKey, shape: Shape, home: Homes, sight: Sight<'_>) -> Result<(), Unfilled> {
        let asked = (trait_, shape, sight.seen);
        if self.answers.contains_key(&asked) {
            return Ok(());
        }
        let candidates = {
            let ix = self.index();
            // The negative impls are candidates too: the solver reads their polarity (a negative
            // impl decides an auto trait's structural rule and never meets a positive goal).
            let mut candidates = ix.trait_candidates(trait_, shape, home);
            candidates.extend_from_slice(&ix.negative_of(trait_, shape, home));
            candidates.retain(|k| sight.sees(ix.krate(*k)));
            candidates.sort_unstable();
            candidates.dedup();
            candidates
        };
        self.answer_candidates(store, trait_, shape, home, sight, &candidates)
    }

    /// [`Buckets::answer`] reusing the caller's already assembled, sight-filtered, sorted, and
    /// deduplicated candidate list (`check::trait_question`).
    pub(crate) fn answer_candidates(&self, store: &Store<'_>, trait_: ItemKey, shape: Shape, home: Homes, sight: Sight<'_>, candidates: &[LibKey]) -> Result<(), Unfilled> {
        let asked = (trait_, shape, sight.seen);
        if self.answers.contains_key(&asked) {
            return Ok(());
        }
        let view = store.tys.view();
        let mut got = Vec::new_in(Global);
        // Candidates come sorted, so the first failed one is the least key.
        let mut first_failed = None;
        let mut used_provisional = false;
        {
            // The index borrowed once (`Buckets::index_mut`): inside, only the index's own
            // methods and reads of `store`'s rows.
            let mut ix = self.index_mut();
            ix.homes.insert((trait_, shape), home);
            for &k in candidates {
                if ix.has_failed(k) {
                    first_failed = first_failed.or(Some(k));
                    continue;
                }
                let (h, provisional) = store.impl_or_provisional(checker_of(k)).ok_or(Unfilled::Impl(k))?;
                if provisional {
                    used_provisional = true;
                }
                // A header built WITHOUT the trait its text names answers nothing: it is the
                // question's gap, never an impl left out of the answer unseen.
                if h.trait_.is_none() && ix.by_text.contains(&k) {
                    ix.fail(k, Box::from("its header's text names a trait; the header as lowered holds none"));
                    first_failed = first_failed.or(Some(k));
                    continue;
                }
                if h.trait_ == Some(trait_) && fits(shape_of(view, h.self_ty), shape) {
                    got.push(checker_of(k));
                }
            }
        }
        got.sort_unstable();
        // The gap first: a reader asks `trait_gap` before `impls_of`.
        if let Some(k) = first_failed {
            self.answer_failed.insert(asked, k);
        }
        self.answers.insert(asked, got);
        if used_provisional {
            let list = self.provisional_answers.get_or_init(|| RefCell::new(blaze_own::Vec::new_in(store.tys.alloc())));
            let mut list = list.borrow_mut();
            if !list.iter().any(|&a| a == asked) {
                list.push(asked);
            }
        }
        Ok(())
    }

    /// Removes trait question answers that depended on provisional impl headers so subsequent
    /// checks lower and cache the complete `ImplHead`.
    pub(super) fn clear_provisional(&self) {
        let Some(list) = self.provisional_answers.get() else { return };
        let mut list = list.borrow_mut();
        for &asked in list.iter() {
            self.answers.remove(&asked);
            self.answer_failed.remove(&asked);
        }
        list.clear();
    }

    /// A test's removed cause (rule 23): every trait question, a checked body's too, takes the
    /// impls of every crate of the program (the answer is still filed under its [`Seen`]).
    pub fn plant_whole_program_sight(&mut self) {
        self.whole_program_sight = true;
    }

    /// The crates a trait question asked for `seen` may take impls of: the program's, or
    /// crate `c`'s dependency closure ([`SightRow`]: from the crate list, no read, over the
    /// records' crate 0 and over the solved names), made once per crate in `alloc` (the
    /// store's: [`Store::tys`]). Not told is a typed refusal, never the whole program in its
    /// place (rule 3).
    pub fn sight<'n, S: NamesSource<'n>>(&self, names: S, seen: Seen, alloc: blaze_own::Alloc<'static>) -> Result<Sight<'_>, Unseen> {
        let Seen::From(krate) = seen else { return Ok(Sight::PROGRAM) };
        if self.whole_program_sight {
            return Ok(Sight { seen, row: None });
        }
        let rows = self.sights.get_or_init(|| Rows::new_in(alloc));
        if let Some(row) = rows.get(&krate) {
            return Ok(Sight { seen, row: Some(row) });
        }
        match rows.insert_absent(krate, SightRow::of(names, krate, alloc)?) {
            Some(row) => Ok(Sight { seen, row: Some(row) }),
            // `insert_absent` returns the live row it just filed.
            None => Err(Unseen::NotAnswered { krate }),
        }
    }

    /// The sight [`Buckets::sight`] made for `seen`, asked again with no names source: what a
    /// body's read list keeps of its impl questions' bucket rows (front2 reads.rs
    /// `facts_with_buckets`). A crate whose bodies asked no trait question has none made:
    /// refused by name ([`Unseen::NotMade`]), never the whole program's in its place.
    pub fn sight_made(&self, seen: Seen) -> Result<Sight<'_>, Unseen> {
        let Seen::From(krate) = seen else { return Ok(Sight::PROGRAM) };
        if self.whole_program_sight {
            return Ok(Sight { seen, row: None });
        }
        match self.sights.get().and_then(|rows| rows.get(&krate)) {
            Some(row) => Ok(Sight { seen, row: Some(row) }),
            None => Err(Unseen::NotMade { krate }),
        }
    }

    /// Answers the inherent items of `shape` named `name` from the headers of `candidates`
    /// alone (sorted by key; the inherent candidates of the shape that declare a member of the
    /// name, `check::make_once`'s filter by `member_names::may_declare`), which `store` must
    /// hold: no fn's signature is made or read (brief 14). An impl that declares no member of
    /// the name is no candidate, so its header (and the anonymous constants in it) is never
    /// needed for the answer, nor its refusal a gap of it.
    pub fn answer_inherent(&self, store: &Store<'_>, shape: Shape, name: Name, candidates: &[LibKey]) -> Result<(), Unfilled> {
        if self.inherent_done.contains_key(&(shape, name)) {
            return Ok(());
        }
        let view = store.tys.view();
        let mut first_failed = None;
        let mut got = Vec::new_in(Global);
        for &k in candidates {
            if self.has_failed(k) {
                first_failed = first_failed.or(Some(k));
                continue;
            }
            let ck = checker_of(k);
            let h = store.impls.get(&ck).ok_or(Unfilled::Impl(k))?;
            if h.trait_.is_some() || shape_of(view, h.self_ty) != Some(shape) {
                continue;
            }
            for (at, &(n, f)) in h.fns.iter().enumerate() {
                if n != name {
                    continue;
                }
                // A name is answered from the impl's member list and each fn's `self` read
                // from its text (`ImplHead::fn_self`), never from a made signature: a fn's
                // signature is its own want (heads.rs `Want::MemberFn`), made when the fn is
                // used, and one refused then is the gap of that fn alone (`Gap::FnHead`, from
                // `fn_sig`). So the answer is the impls' text's, whatever was made before it.
                let self_param = *h.fn_self.get(at).ok_or(Unfilled::Fn(f))?;
                got.push(AssocItem { container: ck, item: f, kind: AssocKind::Fn { self_param } });
            }
            for &(n, c) in h.consts.iter() {
                if n == name {
                    got.push(AssocItem { container: ck, item: c, kind: AssocKind::Const });
                }
            }
        }
        got.sort_unstable_by_key(|a| a.item);
        // The items before the question is marked answered: a reader asks `inherent` only
        // of an answered question.
        self.inherent.insert((shape, name), got);
        self.inherent_done.insert((shape, name), first_failed);
        Ok(())
    }

    /// Every trait question answered so far, with its answer, in a stable order (for a work
    /// list naming what selection was asked and what it found).
    pub fn answered(&self) -> Vec<(ItemKey, Shape, &[ItemKey]), Global> {
        let mut out = Vec::new_in(Global);
        // One row per question and sight ([`Seen`]): two rows of one question differ in their
        // impls, which order them too (equal rows print the same, in whichever order).
        out.extend(self.answers.iter().map(|(&(t, s, _), v)| (t, s, v.as_slice())));
        out.sort_unstable_by(|a, b| (a.0, format!("{:?}", a.1), a.2).cmp(&(b.0, format!("{:?}", b.1), b.2)));
        out
    }

    /// The whole program's answer for `trait_` at `shape`, once made ([`Seen::Program`]).
    pub fn impls_of(&self, trait_: ItemKey, shape: Shape) -> Option<&[ItemKey]> {
        self.impls_seen(trait_, shape, Seen::Program)
    }

    /// The answer for `trait_` at `shape` from the impls `seen` names, once made.
    pub fn impls_seen(&self, trait_: ItemKey, shape: Shape, seen: Seen) -> Option<&[ItemKey]> {
        self.answers.get(&(trait_, shape, seen)).map(|v| v.as_slice())
    }

    /// The failed candidate (the least key, with why) of the answered trait question, if any:
    /// its answer may miss that impl, so asking it is a gap, never an answer. The whole
    /// program's question ([`Seen::Program`]).
    pub fn trait_gap(&self, trait_: ItemKey, shape: Shape) -> Option<(LibKey, Box<str>)> {
        self.trait_gap_seen(trait_, shape, Seen::Program)
    }

    /// [`Buckets::trait_gap`] of the question asked for `seen`.
    pub fn trait_gap_seen(&self, trait_: ItemKey, shape: Shape, seen: Seen) -> Option<(LibKey, Box<str>)> {
        let k = *self.answer_failed.get(&(trait_, shape, seen))?;
        self.index().failed_why(k)
    }

    /// The failed candidate (the least key, with why) of the answered inherent question
    /// (`shape`, `name`), if any: a refused impl that declares no member of the name is none.
    pub fn inherent_gap(&self, shape: Shape, name: Name) -> Option<(LibKey, Box<str>)> {
        let k = self.inherent_done.get(&(shape, name)).copied().flatten()?;
        self.index().failed_why(k)
    }

    /// The inherent items of `shape` named `name`, once that question is answered.
    pub fn inherent(&self, shape: Shape, name: Name) -> Option<&[AssocItem]> {
        if !self.inherent_done.contains_key(&(shape, name)) {
            return None;
        }
        Some(self.inherent.get(&(shape, name)).map_or(&[], |v| v.as_slice()))
    }
}

#[cfg(test)]
mod trait_crates_tests {
    use super::{Home, Homes, ItemKey, Local, Mutbl, Shape, trait_crates};

    #[test]
    fn unnameable_and_foreign_shapes_narrow_trait_homes_by_orphan_rules() {
        let home = Some(3);
        let one_param = (1, false);
        let defaulted_two = (2, true);
        let multi_param = (2, false);
        let only_home = Home::In(3, None);

        for shape in [
            Shape::Param,
            Shape::Closure,
            Shape::FnDef(ItemKey(7)),
            Shape::Opaque(ItemKey(8)),
            Shape::Pat,
            Shape::Dynamic(None),
        ] {
            let h1 = trait_crates(home, shape, Local::Nowhere, one_param);
            assert_eq!(h1, Homes { all: only_home, bare: only_home, blanket: only_home, local: Local::Any }, "1-param at {shape:?}");

            let h2 = trait_crates(home, shape, Local::Nowhere, defaulted_two);
            assert_eq!(h2, Homes { all: Home::Any, bare: only_home, blanket: only_home, local: Local::Any }, "defaulted 2-param at {shape:?}");

            let h3 = trait_crates(home, shape, Local::Nowhere, multi_param);
            assert_eq!(h3, Homes { all: Home::Any, bare: Home::Any, blanket: only_home, local: Local::Any }, "multi-param at {shape:?}");
        }

        // Foreign types are local to their defining crate (just like non-fundamental ADTs).
        let foreign = trait_crates(home, Shape::Foreign(ItemKey(9)), Local::In(5), one_param);
        assert_eq!(foreign, Homes { all: Home::In(3, Some(5)), bare: Home::In(3, Some(5)), blanket: only_home, local: Local::Any });

        // References remain fundamental -> wide (`Home::Any`) for `all` and `bare`.
        let rf = trait_crates(home, Shape::Ref(Mutbl::Not), Local::Any, one_param);
        assert_eq!(rf, Homes { all: Home::Any, bare: Home::Any, blanket: only_home, local: Local::Any });
    }
}
