//! A trait fn's `impl Trait` (and an `async fn`'s future in a trait) as a synthetic associated
//! type of its trait: Rust's model. The Reference, "Impl trait", "`impl Trait` in traits"
//! (stable since 1.75): an impl's fn may write the same `impl Trait`, an `async fn`, or any type
//! meeting the bounds, and a call through that impl sees that type; through a generic `T: Tr`
//! only the trait's bounds are known. rustc lowers each return-position `impl Trait` of a trait
//! fn to an associated type `<Self as Tr>::{synthetic#i}` whose value in each impl is the type
//! at that place of the impl fn's return type, and whose DEFAULT, for a provided fn, is the
//! opaque type its default body defines (`rustc_ty_utils::assoc::associated_types_for_impl_
//! traits_in_associated_fn`, `associated_type_for_impl_trait_in_trait`).
//!
//! Here the trait fn's return type keeps the opaque `Opaque(o, ident)` at every use (borrowck,
//! regions and code read it unchanged); what changes is what the opaque REVEALS: its hidden type
//! is the projection `<Self as Tr<..>>::{o}` over the trait fn's generics ([`Synthetic::alias`]).
//! So an auto trait leaks through it as rustc's does:
//!
//! - a concrete receiver: the projection selects the impl, whose row ([`impl_assoc`]) is the type
//!   at the opaque's place of its fn's return type: the impl's own opaque (an `async fn`'s
//!   future, its body's coroutine), or a written type. `Send` then holds by that type. An impl
//!   that does not write the fn (a provided one) takes the trait's default: the opaque's
//!   [`Twin`], whose hidden type is what the default body gives `o`.
//! - a generic receiver: the projection is rigid, no impl answers it, and an auto trait of a
//!   rigid projection is not proved (E0277: rustc's "future cannot be sent between threads
//!   safely"), unless the trait fn wrote `+ Send`: the opaque's own bounds answer that first
//!   (blaze-solve structural.rs `assemble_auto`), with no reveal.
//!
//! A row is made for every opaque of the fn at a PLACE of its return type (the whole of it,
//! `-> Option<impl A>`, `-> (impl A, impl B)`): an impl's value is the type at the same place of
//! its fn's return type, found by walking both types together ([`impl_assoc`]). An impl whose
//! return type has another shape around that place (a projection or an alias written where the
//! trait wrote `Option<..>`) is refused by name, never guessed.
//!
//! Not yet (refused by name: opaque_auto.rs `HiddenFail::TraitFnInBounds`): an `impl Trait` in
//! the BOUNDS of another (`-> impl Iterator<Item = impl A>`): it has no place in the impl's type;
//! its value per impl is a projection of the outer place's value through the bound, which needs
//! the bound's arguments put in the impl's frame, not built.
//!
//! The rows are the world's answers (front2 world_items.rs `trait_def`, `impl_header`,
//! `assoc_generics`, `opaque_hidden`, `predicates`), derived from heads alone and made the same
//! way in every process, so each answer's digest is the same wherever it is asked (rule 25).

use blaze_own::{Alloc, Vec};
use blaze_typeck::dyn_compat::MemberShape;
use blaze_typeck::{AssocTyDef, ItemKey, List, Name, PClause, PNode, PTy, PTys, StableHasher};

use super::heads::{ImplHead, Store, TraitHead};

/// One synthetic associated type: an opaque at a place of a trait fn's return type.
#[derive(Clone, Copy, Debug)]
pub struct Synthetic {
    /// [`name_of`] the opaque: no written name hashes to it (a written one is `Name::of` its
    /// text, in another hasher domain).
    pub name: Name,
    /// The trait fn's opaque (its key, `keys::opaque_key`): the associated type's item, whose
    /// predicates are its bounds.
    pub opaque: ItemKey,
    /// The trait fn's name: the impl's fn of that name gives the value.
    pub fn_name: Name,
    /// The trait fn (its checker key): its return type is walked against an impl fn's.
    pub trait_fn: ItemKey,
    /// The trait fn's own type and const parameters: the associated type's own.
    pub own: u32,
    /// `<Self as Tr<..>>::{name}` with the opaque's own arguments (the trait fn's identity), in
    /// `Store::tys`: the opaque's hidden type.
    pub alias: PTy,
    /// A provided fn's: `Opaque(twin, ident)` ([`Twin`]), the associated type's default.
    pub default: Option<PTy>,
}

/// The opaque type that is a provided trait fn's synthetic associated type's default (rustc's
/// default-body opaque): its key is [`default_key`] of the fn's opaque `source`, filed in
/// `Store::rpitit_defaults`. It is not the trait fn's return type (that reveals the projection);
/// its hidden type is what the default body gives `source` (`Store::hidden`, filed under
/// `source`: [`filed_as`]), and its bounds are `source`'s with itself as `Self`.
pub struct Twin {
    pub source: ItemKey,
    /// The provided fn whose body defines `source`.
    pub owner: ItemKey,
    /// `source`'s bounds, `Opaque(source, ident)` replaced by `Opaque(twin, ident)`, in
    /// `Store::tys`.
    pub clauses: Vec<PClause, Alloc<'static>>,
}

/// A trait's associated types with its synthetic ones after them (`TraitDef::assoc_types`), and
/// the synthetic ones.
pub struct TraitRows {
    pub assoc: Vec<AssocTyDef, Alloc<'static>>,
    pub rows: Vec<Synthetic, Alloc<'static>>,
}

/// What a row needs first.
#[derive(Clone, Copy, Debug)]
pub enum Need {
    /// The signature of this fn (checker key): the world wants it, then asks again.
    Fn(ItemKey),
    /// Refused, naming the item and why (rule 19).
    Refused(ItemKey, &'static str),
}

/// The most type nodes one walk of a return type visits (rule 18): a signature's type is far
/// smaller.
const MOST_NODES: usize = 1 << 16;

/// The synthetic associated type's name for the trait fn's opaque `opaque`.
pub fn name_of(opaque: ItemKey) -> Name {
    let mut h = StableHasher::new("blaze front2 rpitit: a required trait fn's impl Trait as an associated type");
    h.u128(opaque.0);
    Name(h.finish())
}

/// The key of the [`Twin`] of a provided trait fn's opaque `opaque`: as `keys::opaque_key`, the
/// crate prefix kept (an opaque key's first eight bytes are its fn's), then 64 bits of a hash in
/// its own domain, so it is no fn's opaque and no library item.
pub fn default_key(opaque: ItemKey) -> ItemKey {
    let id = blaze_ids::ContentId::of("blaze-ladder front2 rpitit default key 1", &opaque.0.to_be_bytes());
    let mut first = [0u8; 16];
    first[..8].copy_from_slice(&opaque.0.to_be_bytes()[..8]);
    first[8..].copy_from_slice(&id.0[..8]);
    ItemKey(u128::from_be_bytes(first))
}

/// The opaque whose filed rows answer `k`: a [`Twin`]'s source, else `k` itself
/// (`Store::opaques` and `Store::hidden` hold the source only).
pub fn filed_as(store: &Store<'_>, k: ItemKey) -> ItemKey {
    store.rpitit_defaults.get(&k).map_or(k, |t| t.source)
}

/// Whether trait `head` declares a fn whose return type may be an `impl Trait` (an `async fn`, or
/// `-> impl Trait`): read from its head's member facts alone, so a trait without one costs no
/// member's signature.
pub fn may_have_rows(head: &TraitHead) -> bool {
    head.members.iter().any(|m| may_return_opaque(m.shape))
}

/// Whether a member of this shape may be a fn returning `impl Trait`: an `async fn`, one whose
/// facts say so (an `impl Trait` anywhere in its return type: front2 dyn_ty.rs `member`), and
/// every fn whose facts stopped before the return type (`where Self: Sized` reads no further,
/// an unread receiver is `Unread`): its signature then says.
fn may_return_opaque(shape: MemberShape) -> bool {
    match shape {
        MemberShape::Fn(f) => f.is_async || f.opaque_return || f.where_self_sized,
        MemberShape::Unread(_) => true,
        MemberShape::Const | MemberShape::Type { .. } => false,
    }
}

/// Trait `trait_`'s rows (its head `head`, its written associated types `assoc`), made once and
/// kept in `Store::rpitit_traits`; `None` when no fn of it returns an opaque. A trait fn whose
/// signature is not made yet is [`Need::Fn`]; one that was refused has no opaque at any use, so
/// it gives no row. A provided fn's rows carry their default ([`Twin`], filed here).
pub fn trait_rows<'s>(store: &'s Store<'_>, trait_: ItemKey, head: &TraitHead, assoc: &[AssocTyDef]) -> Result<Option<&'s TraitRows>, Need> {
    // A trait filed with no synthetic row (no fn returns an opaque) is filed too, so the scan
    // is made once.
    if let Some(r) = store.rpitit_traits.get(&trait_) {
        return Ok((!r.rows.is_empty()).then_some(r));
    }
    if !may_have_rows(head) {
        return Ok(None);
    }
    let alloc = store.tys.alloc();
    let view = store.tys.view();
    let mut rows: Vec<Synthetic, Alloc<'static>> = Vec::new_in(alloc);
    let mut placed: Vec<(ItemKey, List), Alloc<'static>> = Vec::new_in(alloc);
    for m in head.members.iter() {
        if !may_return_opaque(m.shape) {
            continue;
        }
        // An `Unread` member may be no fn: then the trait's fns do not name it.
        let Some(&(_, fk)) = head.fns.iter().find(|&&(n, _)| n == m.name) else { continue };
        let Some(fh) = store.fns.get(&fk) else {
            if store.fn_gaps.contains_key(&fk) {
                continue;
            }
            return Err(Need::Fn(fk));
        };
        placed.clear();
        opaques_placed(store, view, fk, fh.output, &mut placed).map_err(|why| Need::Refused(fk, why))?;
        for &(o, args) in placed.iter() {
            let name = name_of(o);
            let alias = store
                .tys
                .intern_shared(PNode::Alias { trait_, name, args })
                .map_err(|_| Need::Refused(fk, "the projection a trait fn's `impl Trait` reveals could not be put in the heads' type table"))?;
            let default = if fh.has_body { Some(twin(store, view, fk, o, args)?) } else { None };
            rows.push(Synthetic { name, opaque: o, fn_name: m.name, trait_fn: fk, own: fh.own, alias, default });
        }
    }
    let mut all: Vec<AssocTyDef, Alloc<'static>> = Vec::new_in(alloc);
    if !rows.is_empty() {
        all.extend_from_slice(assoc);
    }
    for s in rows.iter() {
        // A required fn's has no default: every impl's fn gives the value (an impl without the
        // fn is E0046, the checker's; its projection is the error type). A provided fn's
        // default is its body's opaque, taken by every impl that does not write the fn.
        all.push(AssocTyDef { name: s.name, item: s.opaque, default: s.default });
    }
    let filed = store
        .rpitit_traits
        .insert_absent(trait_, TraitRows { assoc: all, rows })
        .ok_or(Need::Refused(trait_, "the heads store kept no row of the trait's synthetic associated types it filed"))?;
    Ok((!filed.rows.is_empty()).then_some(filed))
}

/// Each opaque fn `fk` made at a place of its return type `output`, once, in walk order, with
/// its arguments: every node is walked but an opaque's (its arguments are its identity, and an
/// `impl Trait` in its bounds is no place: the module doc's "Not yet").
fn opaques_placed(store: &Store<'_>, view: PTys<'_>, fk: ItemKey, output: PTy, out: &mut Vec<(ItemKey, List), Alloc<'static>>) -> Result<(), &'static str> {
    const PAST: &str = "its return type names a type past the heads' type table";
    let mut work: Vec<PTy, Alloc<'static>> = Vec::new_in(store.tys.alloc());
    work.push(output);
    let mut seen = 0usize;
    while let Some(t) = work.pop() {
        seen += 1;
        if seen > MOST_NODES {
            return Err("its return type has more type nodes than one walk visits (rule 18)");
        }
        let node = view.node(t).map_err(|_| PAST)?;
        if let PNode::Opaque(k, args) = node {
            if store.opaques.get(&k).map(|&(owner, _)| owner) == Some(fk) && !out.iter().any(|&(o, _)| o == k) {
                out.push((k, args));
            }
            continue;
        }
        let at = work.len();
        view.children_of(node, &mut work).map_err(|_| PAST)?;
        // Children in declaration order: the walk takes the first child first.
        work[at..].reverse();
    }
    Ok(())
}

/// The default of the provided trait fn `fk`'s opaque `o` (arguments `args`): `Opaque(twin,
/// args)`, its [`Twin`] filed once.
fn twin(store: &Store<'_>, view: PTys<'_>, fk: ItemKey, o: ItemKey, args: List) -> Result<PTy, Need> {
    const TABLE: &str = "the default of a provided trait fn's `impl Trait` could not be put in the heads' type table";
    let d = default_key(o);
    let me = store.tys.intern_shared(PNode::Opaque(d, args)).map_err(|_| Need::Refused(fk, TABLE))?;
    if store.rpitit_defaults.contains_key(&d) {
        return Ok(me);
    }
    let Some(p) = store.preds.get(&o) else {
        return Err(Need::Refused(fk, "its `impl Trait`'s bounds were not filed with its head, so the default's bounds are not known"));
    };
    if !p.conds.is_empty() {
        return Err(Need::Refused(fk, "its `impl Trait`'s bounds hold a `where const` condition, which the default's bounds do not carry"));
    }
    let mut clauses: Vec<PClause, Alloc<'static>> = Vec::with_capacity_in(p.clauses.len(), store.tys.alloc());
    let swap = |t: PTy| swap_self(store, view, t, o, args, me).map_err(|why| Need::Refused(fk, why));
    let swap_list = |l: List| -> Result<List, Need> {
        let tys = view.list(l).map_err(|_| Need::Refused(fk, TABLE))?;
        let mut new: Vec<PTy, Alloc<'static>> = Vec::with_capacity_in(tys.len(), store.tys.alloc());
        for &t in tys {
            new.push(swap(t)?);
        }
        if new.as_slice() == tys {
            return Ok(l);
        }
        store.tys.intern_list_shared(&new).map_err(|_| Need::Refused(fk, TABLE))
    };
    for &c in p.clauses.iter() {
        clauses.push(match c {
            PClause::Trait { trait_, args } => PClause::Trait { trait_, args: swap_list(args)? },
            PClause::Projection { trait_, args, name, term } => PClause::Projection { trait_, args: swap_list(args)?, name, term: swap(term)? },
            PClause::Const { cond, args } => PClause::Const { cond, args: swap_list(args)? },
            PClause::ForFields { container, trait_, args } => PClause::ForFields { container: swap(container)?, trait_, args: swap_list(args)? },
            PClause::TypeOutlives { ty, region } => PClause::TypeOutlives { ty: swap(ty)?, region },
            c @ PClause::RegionOutlives { .. } => c,
        });
    }
    store.rpitit_defaults.insert_absent(d, Twin { source: o, owner: fk, clauses });
    Ok(me)
}

/// `t`, or `me` where `t` is the opaque `Opaque(o, args)` itself (a bound's `Self`); refused
/// where `o` stands deeper in `t` (a written bound cannot name its own opaque, so none is
/// expected; a rebuild of the type around it is not made).
fn swap_self(store: &Store<'_>, view: PTys<'_>, t: PTy, o: ItemKey, args: List, me: PTy) -> Result<PTy, &'static str> {
    const PAST: &str = "its `impl Trait`'s bounds name a type past the heads' type table";
    match view.node(t).map_err(|_| PAST)? {
        PNode::Opaque(k, l) if k == o && l == args => return Ok(me),
        PNode::Opaque(k, _) if k == o => return Err("its `impl Trait`'s bounds name that opaque with other arguments"),
        _ => {}
    }
    let mut work: Vec<PTy, Alloc<'static>> = Vec::new_in(store.tys.alloc());
    work.push(t);
    let mut seen = 0usize;
    while let Some(x) = work.pop() {
        seen += 1;
        if seen > MOST_NODES {
            return Err("its `impl Trait`'s bounds have more type nodes than one walk visits (rule 18)");
        }
        let node = view.node(x).map_err(|_| PAST)?;
        if matches!(node, PNode::Opaque(k, _) if k == o) {
            return Err("its `impl Trait`'s bounds name that opaque below their `Self`");
        }
        view.children_of(node, &mut work).map_err(|_| PAST)?;
    }
    Ok(t)
}

/// Impl `impl_`'s (head `head`) associated types with one row per synthetic type of its trait
/// (`rows`): the value is the type at the opaque's place of the impl's fn of that name's return
/// type, over the impl's generics then the fn's own (as an associated type's value is). Made
/// once, in `Store::rpitit_impls`. An impl without the fn gives no row (the trait's default, or
/// E0046, the checker's); one whose fn's signature was refused is refused, naming the fn: its
/// projection has no value to give; so is one whose return type has another shape around the
/// place.
pub fn impl_assoc<'s>(store: &'s Store<'_>, impl_: ItemKey, head: &ImplHead, rows: &TraitRows) -> Result<&'s [(Name, PTy)], Need> {
    if let Some(v) = store.rpitit_impls.get(&impl_) {
        return Ok(&v[..]);
    }
    let alloc = store.tys.alloc();
    let view = store.tys.view();
    let mut all: Vec<(Name, PTy), Alloc<'static>> = Vec::new_in(alloc);
    all.extend_from_slice(&head.assoc);
    for s in rows.rows.iter() {
        let Some(&(_, f)) = head.fns.iter().find(|&&(n, _)| n == s.fn_name) else { continue };
        let Some(fh) = store.fns.get(&f) else {
            if store.fn_gaps.contains_key(&f) {
                return Err(Need::Refused(f, "its signature was refused, so the value it gives its trait fn's `impl Trait` is not known"));
            }
            return Err(Need::Fn(f));
        };
        // Made by `trait_rows` before the row was.
        let Some(tf) = store.fns.get(&s.trait_fn) else { return Err(Need::Fn(s.trait_fn)) };
        let value = at_place(store, view, tf.output, fh.output, s.opaque).map_err(|why| Need::Refused(f, why))?;
        all.push((s.name, value));
    }
    store
        .rpitit_impls
        .insert_absent(impl_, all)
        .map(|v| &v[..])
        .ok_or(Need::Refused(impl_, "the heads store kept no row of the impl's synthetic associated types it filed"))
}

/// The type of impl fn return type `imp` at the place opaque `o` stands in trait fn return type
/// `tr`: both walked together, a node of the trait's descended into only where the impl's has
/// the same constructor (`same_head`) and as many children.
fn at_place(store: &Store<'_>, view: PTys<'_>, tr: PTy, imp: PTy, o: ItemKey) -> Result<PTy, &'static str> {
    place_in(view, tr, view, imp, o, store.tys.alloc())
}

/// [`at_place`] over two tables: the trait fn's return type `tr` in `tv`, the impl fn's `imp` in
/// `iv`; the answer is a type of `iv`, over the impl fn's generics (the impl's, then the fn's
/// own). The one walk of the place (rule 30): the heads' impl row ([`impl_assoc`]) and the code
/// builder's reveal of a trait fn's opaque at the impl a call selects (front2 coroutines.rs
/// `opaque_hidden`) read the same place. `alloc` holds the walk's columns.
pub(crate) fn place_in(tv: PTys<'_>, tr: PTy, iv: PTys<'_>, imp: PTy, o: ItemKey, alloc: Alloc<'_>) -> Result<PTy, &'static str> {
    const PAST: &str = "its return type or its trait fn's names a type past the heads' type table";
    let mut work: Vec<(PTy, PTy), Alloc<'_>> = Vec::new_in(alloc);
    let mut a: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    let mut b: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    work.push((tr, imp));
    let mut seen = 0usize;
    while let Some((t, i)) = work.pop() {
        seen += 1;
        if seen > MOST_NODES {
            return Err("its return type has more type nodes than one walk visits (rule 18)");
        }
        let (nt, ni) = (tv.node(t).map_err(|_| PAST)?, iv.node(i).map_err(|_| PAST)?);
        match nt {
            PNode::Opaque(k, _) if k == o => return Ok(i),
            PNode::Opaque(..) => continue,
            _ if !same_head(nt, ni) => continue,
            _ => {}
        }
        a.clear();
        b.clear();
        tv.children_of(nt, &mut a).map_err(|_| PAST)?;
        iv.children_of(ni, &mut b).map_err(|_| PAST)?;
        if a.len() != b.len() {
            continue;
        }
        for (&x, &y) in a.iter().zip(b.iter()) {
            work.push((x, y));
        }
    }
    Err("its return type is not its trait fn's around the place of the trait fn's `impl Trait` (another type is written there), so the value it gives that `impl Trait` is not read")
}

/// Whether two type nodes have one constructor (their children aside): the walk of
/// [`at_place`] goes on below them.
fn same_head(a: PNode, b: PNode) -> bool {
    use PNode as N;
    match (a, b) {
        (N::Tuple(x), N::Tuple(y)) | (N::Union(x), N::Union(y)) | (N::Dynamic(x), N::Dynamic(y)) => x.len == y.len,
        (N::Record(s1, x), N::Record(s2, y)) => s1 == s2 && x.len == y.len,
        (N::Ref(m, _), N::Ref(n, _)) | (N::Ptr(m, _), N::Ptr(n, _)) => m == n,
        (N::Array(..), N::Array(..)) | (N::Slice(_), N::Slice(_)) => true,
        (N::Adt(k, _), N::Adt(j, _)) | (N::FnDef(k, _), N::FnDef(j, _)) | (N::ExTrait(k, _), N::ExTrait(j, _)) | (N::Foreign(k), N::Foreign(j)) => k == j,
        (N::FnPtr(_, h), N::FnPtr(_, g)) => h == g,
        (N::Alias { trait_: t, name: n, .. }, N::Alias { trait_: u, name: m, .. }) | (N::ExProj { trait_: t, name: n, .. }, N::ExProj { trait_: u, name: m, .. }) => {
            t == u && n == m
        }
        _ => false,
    }
}

/// The hidden type of opaque `opaque` when it is at a place of a trait fn's return type of a
/// trait with rows: its [`Synthetic::alias`]. `None`: not such an opaque (its owner is no trait
/// fn, it stands in another's bounds, or `rows` is `None`).
pub fn hidden_of(rows: Option<&TraitRows>, opaque: ItemKey) -> Option<PTy> {
    rows?.rows.iter().find(|s| s.opaque == opaque).map(|s| s.alias)
}
