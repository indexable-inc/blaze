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
use blaze_scope::resolve::Names;
use blaze_typeck::{AssocItem, AssocKind, ItemKey, Name, PNode, PTy, PTys, Shape};

use super::heads::{Store, lower};
use super::index::Index;
use super::keys::checker_of;
use super::types::{PRIMS, children, path_texts};
use crate::resolve::{Kind, Ns, ResolveError, Resolver};

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
        PNode::Closure(..) | PNode::Future(..) => Shape::Closure,
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
    Trait(ItemKey, Option<Shape>),
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
fn written_shape(names: &Names<'_>, module: LibKey, params: &[(Name, u32)], v: &View<'_>, t: lf::TypeId) -> Result<Option<Shape>, NoCheckerType> {
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
fn alias_shape(names: &Names<'_>, mut alias: LibKey) -> Result<Option<Shape>, NoCheckerType> {
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
fn alias_step(names: &Names<'_>, alias: LibKey) -> Option<Written> {
    let text = names.item_text(alias)?.text;
    let module = names.module_of(alias)?;
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

fn shape_step(names: &Names<'_>, module: LibKey, params: &[(Name, u32)], v: &View<'_>, t: lf::TypeId) -> Option<Written> {
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
            match (Resolver { names }).path(module, abs, &texts, Ns::Type) {
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

fn file(names: &Names<'_>, key: LibKey, module: LibKey) -> Filed {
    let Some(it) = names.item_text(key) else { return Filed::Unread(Unread::NoText, None) };
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
    match (Resolver { names }).path(module, abs, &texts, Ns::Type) {
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() && v.head[head::FLAGS] & sig::NEGATIVE != 0 => Filed::Negative(checker_of(f.key)),
        Ok(f) if f.kind == Kind::Trait && f.consumed as usize == texts.len() => Filed::Trait(checker_of(f.key), self_shape),
        Ok(f) => Filed::Unread(Unread::NotATrait { path, kind: f.kind, consumed: f.consumed, written: texts.len() as u32 }, self_shape),
        Err(why) => Filed::Unread(Unread::Resolve { path, why }, self_shape),
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
}

impl Buckets {
    /// Files every impl of `index` by its header as written.
    pub fn new(names: &Names<'_>, index: &Index<'_>) -> Buckets {
        let mut b = Buckets {
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
        };
        for at in index.impls.iter() {
            match file(names, at.key, at.module) {
                Filed::Trait(tr, Some(s)) => {
                    push(&mut b.by_shape, (tr, s), at.key);
                    match s {
                        Shape::Int(_) | Shape::Uint(_) => push(&mut b.by_shape, (tr, Shape::AnyInt), at.key),
                        Shape::Float(_) => push(&mut b.by_shape, (tr, Shape::AnyFloat), at.key),
                        _ => {}
                    }
                }
                Filed::Trait(tr, None) => push(&mut b.any, tr, at.key),
                Filed::Inherent(Some(s)) => push(&mut b.inherent_by_shape, s, at.key),
                Filed::Inherent(None) => b.inherent_any.push(at.key),
                Filed::NoCheckerType => b.no_checker_type += 1,
                Filed::Negative(tr) => push(&mut b.negative, tr, at.key),
                Filed::Unread(why, shape) => {
                    b.unread.push(at.key);
                    match (why.is_trait_impl(), shape) {
                        (false, _) => b.unread_blind.push(at.key),
                        (true, None) => b.unread_any.push(at.key),
                        (true, Some(s)) => {
                            push(&mut b.unread_by_shape, s, at.key);
                            match s {
                                Shape::Int(_) | Shape::Uint(_) => push(&mut b.unread_by_shape, Shape::AnyInt, at.key),
                                Shape::Float(_) => push(&mut b.unread_by_shape, Shape::AnyFloat, at.key),
                                _ => {}
                            }
                        }
                    }
                    b.unread_why.push((at.key, why));
                }
            }
        }
        b.unread.sort_unstable();
        for v in b.negative.values_mut() {
            v.sort_unstable();
        }
        b
    }

    /// The candidates for `trait_` at `shape`, each once.
    pub fn trait_candidates(&self, trait_: ItemKey, shape: Shape) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(self.by_shape.get(&(trait_, shape)).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(self.any.get(&trait_).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(self.unread_by_shape.get(&shape).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(&self.unread_any);
        out.extend_from_slice(&self.unread_blind);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The negative impls of `trait_` (any self type), sorted by key.
    pub fn negative_of(&self, trait_: ItemKey) -> &[LibKey] {
        self.negative.get(&trait_).map_or(&[], |v| v.as_slice())
    }

    /// The candidates for an inherent fn of `shape`, each once.
    pub fn inherent_candidates(&self, shape: Shape) -> Vec<LibKey, Global> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(self.inherent_by_shape.get(&shape).map_or(&[], |v| v.as_slice()));
        out.extend_from_slice(&self.inherent_any);
        // A trait impl (its trait path written, though unread) is never an inherent candidate.
        out.extend_from_slice(&self.unread_blind);
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
    pub fn answer(&mut self, store: &Store<'_>, trait_: ItemKey, shape: Shape) -> Result<(), Unfilled> {
        let view = store.tys.view();
        let mut got = Vec::new_in(Global);
        // Candidates come sorted, so the first failed one is the least key.
        let mut first_failed = None;
        // The negative impls are candidates too: the solver reads their polarity (a negative
        // impl decides an auto trait's structural rule and never meets a positive goal).
        let mut candidates = self.trait_candidates(trait_, shape);
        candidates.extend_from_slice(self.negative_of(trait_));
        candidates.sort_unstable();
        candidates.dedup();
        for k in candidates {
            if self.has_failed(k) {
                first_failed = first_failed.or(Some(k));
                continue;
            }
            let h = store.impls.get(&checker_of(k)).ok_or(Unfilled::Impl(k))?;
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
    pub fn answer_inherent(&mut self, store: &Store<'_>, shape: Shape) -> Result<(), Unfilled> {
        let view = store.tys.view();
        let mut first_failed = None;
        for k in self.inherent_candidates(shape) {
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
