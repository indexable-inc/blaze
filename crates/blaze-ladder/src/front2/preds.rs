//! An item's where-clauses as the checker's [`PClause`]s: each type parameter's bounds and
//! implied `Sized`, the `where` list's type predicates, a trait's supertraits on `Self`.
//! Ported from front.rs's `bound` and `predicates` (whose copies go at the switch). A
//! `where const` condition is not passed over silently: it is collected for the caller
//! ([`Preds::conds`]), which makes its clause from the condition's own item.
//!
//! Lifetimes (D148, piece L1b). A lifetime bound is a clause: `T: 'a` is
//! [`PClause::TypeOutlives`] and `'a: 'b` [`PClause::RegionOutlives`], each region resolved
//! here by the item's [`Scope`] ([`heads_regions::lifetime`]). Beside each clause [`Preds`]
//! keeps what it was written as ([`Src`]: its `Self`, its bound's path, its `for<..>`
//! binders), which heads_regions.rs lays the clause's region column from.

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf, head};
use blaze_scope::names::PathError;
use blaze_own::{Alloc, Vec as OwnVec};
use blaze_surface::Range;
use blaze_typeck::{List, Name, PClause, PNode, PTy, PortableTys};

use super::heads_regions::{self, Binders, On, Scope, Src};
use super::types::{Bounded, Bounds, Builder, Env, Heads, ParamBound, SelfTy, Why, intern, intern_list, path_texts, path_texts_of, subst_clause};

/// The allocator a head's clause sources are kept in: the store's.
type Mem = Alloc<'static>;

/// What one bound says of its type's sizedness ([`Builder::bound`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sizing {
    /// `?Sized`: the implied clause is `MetaSized`, not `Sized` (`Builder::implied_size`).
    Relaxed,
    /// A trait bound, its trait.
    Trait(LibKey),
    /// A lifetime bound (`T: 'a`) or `use<..>`.
    NoTrait,
}

/// An item's clauses, and what [`Builder::predicates`] found besides them: the item's `where
/// const` conditions, by their const ids in the item's text, for the caller to key and
/// evaluate. `sources[i]` is what `clauses[i]` was written as (one each, kept in step by
/// [`Preds::push`] and [`Preds::remove_clause`]); `scope` names the item's lifetimes.
pub struct Preds {
    pub clauses: Vec<PClause, Global>,
    pub conds: Vec<lf::ConstIdx, Global>,
    pub sources: OwnVec<Src, Mem>,
    pub scope: Scope,
    /// Why the sources could not all be read as written (a bound's arguments the regions'
    /// reader refuses, bindings that do not line up with them): the clauses are whole and the
    /// head is made, and its regions are refused with this, by name (heads_regions.rs
    /// `fill_clauses`). The first such refusal is kept.
    pub regions_gap: Option<heads_regions::RegionsRefusal>,
    /// A fn head's bounds whose trait path the names library decided names nothing (E0405):
    /// each such bound's library refusal and its path's range, the bound dropped, for the head
    /// to keep as its own finding (heads.rs `fn_head`, type_miss.rs `TypeMiss::of_trait`).
    /// `None` (every item but a fn, which has no body to say it) keeps such a bound the
    /// head's error, as before.
    pub trait_misses: Option<OwnVec<(PathError, Range), Mem>>,
}

impl Preds {
    /// No clause yet, the item's lifetimes named by `scope` (heads_regions.rs
    /// [`Scope::item`]).
    pub fn new_in(scope: Scope) -> Preds {
        let alloc = scope.alloc();
        Preds { clauses: Vec::new_in(Global), conds: Vec::new_in(Global), sources: OwnVec::new_in(alloc), scope, regions_gap: None, trait_misses: None }
    }

    /// A fn head's: a bound whose trait names nothing is kept in [`Preds::trait_misses`] and
    /// dropped, not the head's error.
    pub fn keep_trait_misses(&mut self) {
        let alloc = self.alloc();
        self.trait_misses.get_or_insert_with(|| OwnVec::new_in(alloc));
    }

    /// The regions of these clauses cannot be laid, for `why` ([`Preds::regions_gap`]).
    pub fn refuse_regions(&mut self, why: heads_regions::RegionsRefusal) {
        self.regions_gap.get_or_insert(why);
    }

    /// The clauses of a filed head (head_record.rs): their region columns were filed beside
    /// them, so each one's source is [`Src::Derived`] and its scope names nothing.
    pub fn decoded(clauses: Vec<PClause, Global>, conds: Vec<lf::ConstIdx, Global>, alloc: Mem) -> Preds {
        let mut sources = OwnVec::with_capacity_in(clauses.len(), alloc);
        for _ in 0..clauses.len() {
            sources.push(Src::Derived);
        }
        Preds { clauses, conds, sources, scope: Scope::new_in(alloc), regions_gap: None, trait_misses: None }
    }

    /// The allocator the sources are kept in.
    pub fn alloc(&self) -> Mem {
        *self.sources.allocator()
    }

    /// `c`, written as `src`.
    pub fn push(&mut self, c: PClause, src: Src) {
        self.clauses.push(c);
        self.sources.push(src);
    }

    /// The container's clauses a member inherits (an impl's, a trait's), each the
    /// container's clause of the same index ([`Src::Inherited`]).
    pub fn inherit(&mut self, inherited: &[PClause]) {
        for (i, &c) in inherited.iter().enumerate() {
            let nth = u32::try_from(i).unwrap_or(u32::MAX);
            self.push(c, Src::Inherited { nth });
        }
    }

    /// Removes clause `at`, and moves every link past it ([`Src::Binding::of`],
    /// [`On::Nested::of`]) down by one. A link to the removed clause itself cannot be: only an
    /// implied sizedness clause ([`Src::Derived`]) is removed, and no clause links to one.
    pub fn remove_clause(&mut self, at: usize) {
        self.clauses.remove(at);
        self.sources.remove_shift(at);
        let at = u32::try_from(at).unwrap_or(u32::MAX);
        let down = |of: &mut u32| {
            if *of > at {
                *of -= 1;
            }
        };
        for s in self.sources.iter_mut() {
            match s {
                Src::Binding { of, .. } => down(of),
                Src::Trait { on: On::Nested { of, .. }, .. } | Src::TypeOutlives { on: On::Nested { of, .. }, .. } => down(of),
                _ => {}
            }
        }
    }

    /// `other`'s clauses, conditions and sources after these, its links moved past them.
    pub fn append(&mut self, other: Preds) {
        let by = u32::try_from(self.clauses.len()).unwrap_or(u32::MAX);
        self.clauses.extend_from_slice(&other.clauses);
        self.conds.extend_from_slice(&other.conds);
        for &s in other.sources.iter() {
            let s = match s {
                Src::Binding { of, term, gat } => Src::Binding { of: of + by, term, gat },
                Src::Trait { on: On::Nested { of, gat }, bound, binders } => Src::Trait { on: On::Nested { of: of + by, gat }, bound, binders },
                Src::TypeOutlives { on: On::Nested { of, gat }, lifetime, binders } => Src::TypeOutlives { on: On::Nested { of: of + by, gat }, lifetime, binders },
                s => s,
            };
            self.sources.push(s);
        }
        if let Some(theirs) = other.trait_misses {
            self.keep_trait_misses();
            if let Some(mine) = self.trait_misses.as_mut() {
                mine.extend(theirs.iter().copied());
            }
        }
    }
}

/// Whether `t` is the alias `alias`, or a projection whose self type (its list's first, then
/// that one's, in turn) is: the rule that tells an associated type's item bounds from its own
/// clauses ([`Builder::assoc_own_clauses`]; blaze-solve structural.rs applies the same one).
/// The table is hash-consed (`PortableTys::intern`), so equal types are equal ids.
fn reaches_alias(tys: &PortableTys<'_>, mut t: PTy, alias: PTy) -> Result<bool, Why> {
    const MOST: usize = 256;
    for _ in 0..MOST {
        if t == alias {
            return Ok(true);
        }
        let view = tys.view();
        let Some(&PNode::Alias { args, .. }) = view.get(t) else { return Ok(false) };
        match view.list(args).map_err(|_| Why::Program("a projection's list is past the type table".into(), Range::new(0, 0)))?.first() {
            Some(&f) => t = f,
            None => return Ok(false),
        }
    }
    Err(Why::Unsupported("a projection nested past 256 in an associated type's own clause", Range::new(0, 0)))
}

fn place(v: &View<'_>, r: Result<blaze_lower::BodyRange, lf::BadRow>) -> Result<Range, Why> {
    Ok(v.site(r?)?)
}

/// A `for<..>` binder (on a bound, a where clause, a `dyn` bound, a fn pointer type): the
/// Reference, "Higher-ranked trait bounds" and "Function pointer types": it binds lifetimes.
/// The types stay erased (`&'a T` is `Ref(T)`); the binder is kept beside the clause
/// ([`Src`]'s [`Binders`]) and its variables are `Late { binder: 0, .. }` in the clause's
/// region column (heads_regions.rs, D148). A type or const in it (`for<T>`, the unstable
/// non-lifetime binders) is refused by name.
pub(super) fn lifetimes_only(v: &View<'_>, binder: lf::List, r: Range) -> Result<(), Why> {
    for &n in v.rows(binder)? {
        if !matches!(lf::GenericParam::decode(n)?, lf::GenericParam::Lifetime { .. }) {
            return Err(Why::Unsupported("a type or const in a `for<..>` binder (non-lifetime binders)", r));
        }
    }
    Ok(())
}

/// Whether the generic arguments `args` (rows of [`lf::GenericArg`]) are lifetimes only (or
/// none).
pub(super) fn lifetime_args(v: &View<'_>, args: lf::List) -> Result<bool, Why> {
    for &n in v.rows(args)? {
        if !matches!(lf::GenericArg::decode(n)?, lf::GenericArg::Lifetime(_)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A trait's type arguments after `Self`, its `Name = T` bindings, and its associated type
/// bounds `Name: Bounds` (the bounds' rows of [`lf::Bound`], made by the caller), as one bound's
/// last segment writes them.
pub(super) type SegArgs = (Vec<PTy, Global>, Vec<(Name, PTy), Global>, Vec<(Name, lf::List), Global>);

/// The one home of how a trait bound's last segment gives its arguments, in a bound, a where
/// clause, an `impl Trait` and a `dyn` type alike. The Reference, "Paths", "Paths in types":
/// the parenthesized form `Fn(A, B) -> C` is the trait with ONE type argument, the tuple of the
/// parameter types, and the binding `Output = C` (`()` when no `->` is written); the angle form
/// gives each type argument and each `Name = T`, and a lifetime is erased. `make` makes each
/// written type, in the order [`super::dyn_ty::dyn_arg_types`] lists them (the inputs, then the
/// output; else in writing order).
pub(super) fn segment_args<'t>(tys: &mut PortableTys<'t>, v: &View<'_>, seg: &lf::Segment, r: Range, make: &mut dyn FnMut(&mut PortableTys<'t>, lf::TypeId) -> Result<PTy, Why>) -> Result<SegArgs, Why> {
    let mut given = Vec::new_in(Global);
    let mut binds = Vec::new_in(Global);
    let nested = Vec::new_in(Global);
    if seg.return_notation {
        return Err(Why::Unsupported("return type notation (`Tr(..)`)", r));
    }
    if seg.parenthesized {
        let mut inputs = Vec::new_in(Global);
        for &n in v.rows(seg.args)? {
            match lf::GenericArg::decode(n)? {
                lf::GenericArg::Type(t) => inputs.push(make(tys, t)?),
                _ => return Err(Why::Program("a parenthesized argument that is no type".into(), r)),
            }
        }
        let l = intern_list(tys, &inputs)?;
        given.push(intern(tys, PNode::Tuple(l))?);
        let output = match seg.ty {
            Some(t) => make(tys, t)?,
            None => intern(tys, PNode::Tuple(List::EMPTY))?,
        };
        binds.push((Name::of("Output"), output));
        return Ok((given, binds, nested));
    }
    if !seg.has_args {
        return Ok((given, binds, nested));
    }
    // The angle form has one walk (const_args_in_bounds.rs); this caller does not know the
    // trait's parameter kinds, so a const argument is refused by name there.
    super::const_args_in_bounds::angle_args(tys, v, seg, r, None, make)
}

// `'n` is the lowered text's (`View<'n>`); `'s` the names source's.
impl<'b, 'n, 's, H: Heads, S: super::source::NamesSource<'s>> Builder<'b, H, S> {
    /// The bound `b` on `self_ty` (written as `on`, under the where predicate's `binders`) as
    /// clauses into `out`: the trait's, a projection for each `Name = Type` argument, and
    /// `self_ty: 'a` for a lifetime bound; what it says of sizedness (`sized` is the `sized`
    /// lang trait).
    #[expect(clippy::too_many_arguments, reason = "the bound, where it is written, and the out list")]
    pub fn bound(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, b: lf::Bound, (self_ty, on, binders): (PTy, On, Binders), sized: Option<LibKey>, out: &mut Preds) -> Result<Sizing, Why> {
        // The bound, then each associated type bound written in it, in writing order (a work
        // list: `Tr<A: B<C: D>>` nests). The Reference, "Trait and lifetime bounds" (associated
        // item constraints, RFC 2289): `T: Tr<Assoc: Bounds>` is `T: Tr` and
        // `<T as Tr>::Assoc: Bounds`; core: `R: [const] Try<Output = T, Residual: Residual<..>>`.
        const MOST_NESTED: usize = 256;
        let mut todo: OwnVec<(lf::Bound, PTy, On, Binders), Mem> = OwnVec::new_in(out.alloc());
        let first = self.one_bound(tys, env, v, b, (self_ty, on, binders), sized, out, &mut todo)?;
        let mut i = 0;
        while i < todo.len() {
            if i == MOST_NESTED {
                let lf::Bound::Trait { ty, .. } = b else { break };
                return Err(Why::Unsupported("associated type bounds nested past 256", place(v, v.type_range(ty))?));
            }
            let (nb, nty, non, nbinders) = todo[i];
            if self.one_bound(tys, env, v, nb, (nty, non, nbinders), sized, out, &mut todo)? == Sizing::Relaxed {
                let lf::Bound::Trait { ty, .. } = nb else { break };
                return Err(Why::Program("`?Sized` on an associated type bound (`?` relaxes a type parameter's or an associated type's implied `Sized` only where it is declared)".into(), place(v, v.type_range(ty))?));
            }
            i += 1;
        }
        Ok(first)
    }

    /// One bound of [`Builder::bound`]: its clauses into `out`, its associated type bounds
    /// (each on its projection) into `todo`.
    #[expect(clippy::too_many_arguments, reason = "the bound, where it is written, and the out lists")]
    fn one_bound(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'n>,
        b: lf::Bound,
        (self_ty, on, outer): (PTy, On, Binders),
        sized: Option<LibKey>,
        out: &mut Preds,
        todo: &mut OwnVec<(lf::Bound, PTy, On, Binders), Mem>,
    ) -> Result<Sizing, Why> {
        let (ty, binder, mods) = match b {
            lf::Bound::Trait { ty, binder, mods } => (ty, binder, mods),
            // `T: 'a` (the Reference, "Trait and lifetime bounds": "`T: 'a` means that all
            // lifetime parameters of `T` outlive `'a`").
            // A lifetime the regions' reader does not name here (`'_` where a name is required,
            // E0637; a return opaque's `+ '_` the output rule gives no candidate) is a regions
            // refusal, never the head's (D148: the erased clause holds no region the type
            // check reads): the clause is left out and the head's regions are refused by name
            // ([`Preds::refuse_regions`], heads_regions.rs `fill_clauses`). A name no scope
            // declares stays the program's error (E0261), and E0106 is the signature's own
            // (heads.rs `output_rule`).
            lf::Bound::Lifetime(n) => {
                match heads_regions::lifetime(v, &out.scope, outer, n) {
                    Ok(region) => out.push(PClause::TypeOutlives { ty: self_ty, region }, Src::TypeOutlives { on, lifetime: n, binders: outer }),
                    Err(why @ heads_regions::RegionsRefusal::Undeclared { .. }) => return Err(Why::Regions(why)),
                    Err(why) => out.refuse_regions(why),
                }
                return Ok(Sizing::NoTrait);
            }
            // `use<..>` is an opaque type's capture list (heads_regions.rs `captures`), no clause.
            lf::Bound::Use(_) => return Ok(Sizing::NoTrait),
        };
        let r = place(v, v.type_range(ty))?;
        lifetimes_only(v, binder, r)?;
        let binders = outer.with(binder).map_err(|e| match e {
            heads_regions::RegionsRefusal::Unsupported { what, .. } => Why::Unsupported(what, r),
            e => Why::Regions(e),
        })?;
        // `const Trait` and `[const] Trait` hold for typing exactly when `Trait` does (the
        // Reference's const trait RFC 3762: const-ness is checked only in const contexts); that
        // check is a const checker's, which reads `mods` again when it exists. So the clause is
        // the trait's. `!Trait` and `async Trait` are other predicates: refused.
        if mods.negative {
            return Err(Why::Unsupported("a negative bound (`!Trait`)", r));
        }
        if mods.asyncness {
            return Err(Why::Unsupported("an async bound (`async Fn`)", r));
        }
        let lf::TypeRef::Path(p) = v.ty(ty)? else { return Err(Why::Program(format!("a bound that is no path").into_boxed_str(), r)) };
        let (abs, texts, segs, pr) = path_texts_of(v, p, true)?;
        // What the path names comes first: the trait's head says which of the written
        // arguments are constants (`Uint<LO>: Concat<HI>`; const_args_in_bounds.rs).
        // A fn head's bound whose trait the library decided names nothing is its E0405: kept,
        // the bound dropped (no clause, no sizedness), the head made without it.
        let target = match (self.bound_target(env, v, p, abs, &texts, pr), out.trait_misses.as_mut()) {
            (Err(Why::Resolve(crate::resolve::ResolveError::Path(e), at)), Some(misses)) if super::type_miss::decided(e) => {
                misses.push((e, at));
                return Ok(Sizing::NoTrait);
            }
            (target, _) => target?,
        };
        let (given, binds, nested) = match segs.last() {
            Some(last) => self.bound_segment_args(tys, env, v, target, last, pr)?,
            None => (Vec::new_in(Global), Vec::new_in(Global), Vec::new_in(Global)),
        };
        let key = match target {
            Bounded::Trait(key) => key,
            // A trait alias stands for its clauses (its head; nested aliases already expanded
            // there). It states no sizedness of its own: core writes `T: PointeeSized + Thin`
            // although `Thin`'s right-hand side holds `PointeeSized`.
            Bounded::Alias(alias) => {
                if mods.maybe {
                    return Err(Why::Unsupported("`?` on a trait alias", r));
                }
                if !binds.is_empty() || !nested.is_empty() {
                    return Err(Why::Unsupported("an associated type binding on a trait alias (`Alias<Name = T>`)", pr));
                }
                // Its clauses have no text here: each a `Src::Derived` (an empty column, or
                // refused by name when its types carry lifetimes).
                let from = out.clauses.len();
                self.alias_clauses(tys, alias, pr, self_ty, &given, &mut out.clauses)?;
                for _ in from..out.clauses.len() {
                    out.sources.push(Src::Derived);
                }
                return Ok(Sizing::NoTrait);
            }
        };
        let (key, args) = self.trait_args(tys, key, pr, self_ty, &given)?;
        if mods.maybe {
            if Some(key) != sized {
                return Err(Why::Unsupported("`?Trait` for a trait that is not `Sized`", r));
            }
            return Ok(Sizing::Relaxed);
        }
        let of = u32::try_from(out.clauses.len()).map_err(|_| Why::Unsupported("a head with more than 2^32 clauses", r))?;
        out.push(PClause::Trait { trait_: self.heads.checker(key), args }, Src::Trait { on, bound: ty, binders });
        // What each `Name = T` and `Name: Bounds` is written as: its term's type and the
        // lifetimes written on its name, in the order `segment_args` gives them.
        let alloc = out.alloc();
        let read = match segs.last() {
            Some(last) => heads_regions::binding_terms(v, last, alloc).and_then(|t| heads_regions::nested_gats(v, last, alloc).map(|g| (t, g))),
            None => Ok((OwnVec::new_in(alloc), OwnVec::new_in(alloc))),
        };
        let read = read.and_then(|(t, g)| if t.len() != binds.len() || g.len() != nested.len() { Err(heads_regions::RegionsRefusal::Unaligned { nth: of }) } else { Ok((t, g)) });
        // What the regions' reader refuses here refuses the head's regions, not its clauses:
        // each binding is then written as nothing (no term, no lifetimes) and never laid.
        let (terms, gats) = match read {
            Ok(tg) => tg,
            Err(why) => {
                out.refuse_regions(why);
                let mut t = OwnVec::with_capacity_in(binds.len(), alloc);
                t.resize(binds.len(), (None, List::EMPTY));
                let mut g = OwnVec::with_capacity_in(nested.len(), alloc);
                g.resize(nested.len(), List::EMPTY);
                (t, g)
            }
        };
        // `Name = T` is a projection on the trait among `key` and its supertraits that declares
        // `Name` (`Fn(A) -> B`'s `Output` is `FnOnce`'s).
        for (&(name, term), &(written, gat)) in binds.iter().zip(terms.iter()) {
            let shown = v.text_at(v.path_range(p)?)?;
            let (owner, oargs) = self.assoc_owner(tys, key, args, name, pr, shown)?.ok_or_else(|| Why::AssocNotFound(shown.into(), pr))?;
            out.push(PClause::Projection { trait_: self.heads.checker(owner), args: oargs, name, term }, Src::Binding { of, term: written, gat });
        }
        // `Name<T> = X` (a generic associated type's binding with type arguments): the
        // projection's list is the owner's arguments then the binding's own (gat.rs).
        if let Some(last) = segs.last() {
            let shown = v.text_at(v.path_range(p)?)?;
            self.gat_bindings(tys, env, v, last, key, args, pr, shown, &mut |c| out.push(c, Src::Derived))?;
            // `Name<T>: Bounds` (an associated type bound on a generic associated type with
            // type or const arguments): each bound on `<self_ty as Owner>::Name<T>`, into the
            // same work list as `Name: Bounds` below (gat_bounds.rs). Its `Self` has type
            // arguments of its own: a column is laid only where it has no regions (`On::Made`).
            self.gat_bounds(tys, env, v, last, key, args, pr, shown, &mut |b, on_ty| todo.push((b, on_ty, On::Made, binders)))?;
        }
        // `Name: Bounds`: each bound on `<self_ty as Owner>::Name`, the owner found as for
        // `Name = T`.
        for (&(name, list), &gat) in nested.iter().zip(gats.iter()) {
            let shown = v.text_at(v.path_range(p)?)?;
            let (owner, oargs) = self.assoc_owner(tys, key, args, name, pr, shown)?.ok_or_else(|| Why::AssocNotFound(shown.into(), pr))?;
            let proj = intern(tys, PNode::Alias { trait_: self.heads.checker(owner), name, args: oargs })?;
            for &row in v.rows(list)? {
                todo.push((lf::Bound::decode(row)?, proj, On::Nested { of, gat }, binders));
            }
        }
        Ok(Sizing::Trait(key))
    }

    /// Whether `s` states its type's sizedness, so that no implied `Sized` is added: `?Sized`,
    /// `Sized` itself, or a supertrait of `Sized` (`MetaSized`, `PointeeSized`: RFC 3729, the
    /// sized hierarchy; the pinned core's `impl<T: PointeeSized> Clone for &T`).
    fn states_size(&self, s: Sizing, sized: Option<LibKey>) -> Result<bool, Why> {
        match s {
            Sizing::Relaxed => Ok(true),
            Sizing::NoTrait => Ok(false),
            Sizing::Trait(k) if Some(k) == sized => Ok(true),
            Sizing::Trait(k) => self.sized_super(k, sized),
        }
    }

    /// `key` is a supertrait of the `sized` lang trait, through its supertraits (by the heads'
    /// `trait_supers`, a work list). Asked only for a parameter's bounds, never while a trait's
    /// own supertraits are made, so lowering `Sized`'s head never waits on itself.
    fn sized_super(&self, key: LibKey, sized: Option<LibKey>) -> Result<bool, Why> {
        const MOST: usize = 16;
        let Some(s) = sized else { return Ok(false) };
        let mut todo = Vec::new_in(Global);
        todo.push(s);
        for _ in 0..MOST {
            let Some(t) = todo.pop() else { return Ok(false) };
            for &(st, _) in self.heads.trait_supers(t).ok_or(Why::Need(t))? {
                if st == key {
                    return Ok(true);
                }
                todo.push(st);
            }
        }
        Ok(false)
    }

    /// The implied sizedness clause's trait for a type whose bounds are `stated` (a sizedness
    /// trait is written: none) or `relaxed` (`?Sized`): `?Sized` relaxes `Sized` one level of
    /// the sized hierarchy (RFC 3729), to `MetaSized`, not to nothing; only a written
    /// `PointeeSized` states no size at all. The pinned core: `pub trait Sized: MetaSized`
    /// (marker.rs), and `alloc`'s `data_offset<T: ?Sized>` calls
    /// `Alignment::of_val_raw<T: MetaSized>` with its own `T`. `MetaSized` is found as
    /// `Sized`'s one supertrait (the heads hold `Sized` and no lang table); a core whose
    /// `Sized` has none (a `no_core` rung) gets no clause for `?Sized`, as before the
    /// hierarchy. `None`: push nothing (also when `sized` is none).
    fn implied_size(&self, stated: bool, relaxed: bool, sized: Option<LibKey>) -> Result<Option<LibKey>, Why> {
        let Some(s) = sized else { return Ok(None) };
        if stated {
            return Ok(None);
        }
        if !relaxed {
            return Ok(Some(s));
        }
        match self.heads.trait_supers(s).ok_or(Why::Need(s))? {
            [] => Ok(None),
            [(meta, _)] => Ok(Some(*meta)),
            _ => Err(Why::Unsupported("`?Sized` where the `sized` lang trait has more than one supertrait (the sized hierarchy puts one, `MetaSized`, above it)", Range::new(0, 0))),
        }
    }

    /// The trait bounds on type parameters written in `v` (on its own parameters, from index
    /// `base`, and in its `where` clauses on any parameter in scope), appended to `out`, which
    /// holds the container's: what `T::X` is looked up through. Each is lowered with the ones
    /// before it in scope. Only the trait and its arguments are read here; the bound's other
    /// parts (`Name = Type`, `?Sized`, `!Trait`) are [`Builder::predicates`]'s to take or refuse.
    pub fn param_bounds(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, base: u32, out: &mut Vec<ParamBound, Global>) -> Result<(), Why> {
        self.param_bounds_kept(tys, env, v, base, out, None)
    }

    /// [`Builder::param_bounds`] for a fn head: a bound whose trait the library decided names
    /// nothing is pushed on `misses` (as [`Preds::trait_misses`]) and declares nothing.
    pub fn param_bounds_kept(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'n>,
        base: u32,
        out: &mut Vec<ParamBound, Global>,
        mut misses: Option<&mut OwnVec<(PathError, Range), Mem>>,
    ) -> Result<(), Why> {
        let mut i = base;
        for &n in v.rows(v.list(head::GENERICS))? {
            // A const parameter takes an index too (`heads::param_names`), and has no bounds.
            match lf::GenericParam::decode(n)? {
                lf::GenericParam::Type { bounds, .. } => {
                    for &b in v.rows(bounds)? {
                        self.param_bound(tys, env, v, lf::Bound::decode(b)?, i, out, misses.as_deref_mut())?;
                    }
                    i += 1;
                }
                lf::GenericParam::Const { .. } => i += 1,
                lf::GenericParam::Lifetime { .. } => {}
            }
        }
        for &n in v.rows(v.list(head::WHERE))? {
            let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(n)? else { continue };
            // A where clause on a type that is no parameter (`Vec<T>: Tr`) bounds no `T::X`.
            let lf::TypeRef::Path(p) = v.ty(ty)? else { continue };
            let (abs, texts, _, _) = path_texts(v, p)?;
            let (false, [t]) = (abs, texts.as_slice()) else { continue };
            // `where Self: Tr` in a trait or one of its members: `Self` is `Param(0)` there, and
            // a bound on it is what `Self::X` is looked up through beside the trait itself (the
            // Reference, "Paths", "`Self`"; tap's `fn try_conv<T>(self) -> Result<T,
            // Self::Error> where Self: TryInto<T>`). In an impl `Self` is no parameter.
            let param = match env.params.iter().find(|(n, _)| *n == Name::of(t)) {
                Some(&(_, p)) => p,
                None if *t == "Self" && matches!(env.self_, SelfTy::Trait(_)) => 0,
                None => continue,
            };
            for &b in v.rows(bounds)? {
                self.param_bound(tys, env, v, lf::Bound::decode(b)?, param, out, misses.as_deref_mut())?;
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "one bound, where it is written, and the out lists")]
    fn param_bound(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, b: lf::Bound, param: u32, out: &mut Vec<ParamBound, Global>, misses: Option<&mut OwnVec<(PathError, Range), Mem>>) -> Result<(), Why> {
        let lf::Bound::Trait { ty, mods, .. } = b else { return Ok(()) };
        // `?Sized` and `!Trait` declare no associated type of the parameter.
        if mods.maybe || mods.negative {
            return Ok(());
        }
        let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(()) };
        let e = Env { bounds: Bounds::Building(out), ..*env };
        let (abs, texts, segs, pr) = path_texts_of(v, p, true)?;
        // A fn head's bound whose trait names nothing (decided): kept, declaring nothing; its
        // predicates walk keeps the same bound's miss, the head says it once (heads.rs).
        let target = match (self.bound_target(&e, v, p, abs, &texts, pr), misses) {
            (Err(Why::Resolve(crate::resolve::ResolveError::Path(pe), at)), Some(misses)) if super::type_miss::decided(pe) => {
                misses.push((pe, at));
                return Ok(());
            }
            (target, _) => target?,
        };
        let given = match segs.last() {
            Some(last) => self.bound_segment_args(tys, &e, v, target, last, pr)?.0,
            None => Vec::new_in(Global),
        };
        let me = intern(tys, PNode::Param(param))?;
        match target {
            Bounded::Trait(key) => {
                let (trait_, args) = self.trait_args(tys, key, pr, me, &given)?;
                out.push(ParamBound { param, trait_, args });
            }
            // `T: Thin` declares through each trait the alias stands for on `Self`.
            Bounded::Alias(alias) => {
                let head = self.heads.trait_alias(alias).ok_or(Why::Need(alias))?;
                if given.len() + 1 != head.generics as usize {
                    return Err(Why::Program(format!("a trait alias written with {} type arguments; it takes {}", given.len(), head.generics - 1).into_boxed_str(), pr));
                }
                let mut args = Vec::with_capacity_in(given.len() + 1, Global);
                args.push(me);
                args.extend_from_slice(&given);
                for &(trait_, l) in &head.traits {
                    let PClause::Trait { args, .. } = subst_clause(tys, PClause::Trait { trait_: self.heads.checker(trait_), args: l }, &args)? else { continue };
                    out.push(ParamBound { param, trait_, args });
                }
            }
        }
        Ok(())
    }

    /// The predicates of `v`'s generics (GENERICS and WHERE, own type parameters from index
    /// `base`): bounds, where clauses and each own parameter's implied `Sized`; in a trait
    /// (`self_bounds` = `Some(Self)`) its supertraits (BOUNDS) on `Self` too. `where const`
    /// conditions go to `out.conds`.
    /// The predicates of `impl Trait` types (`list`: each written type and the type it is:
    /// an anonymous parameter `Param(i)` or an opaque `Opaque(k, ..)`): each one's bounds as
    /// written on it with that type as `Self`, and its implied `Sized` unless `?Sized` is
    /// written, as for a declared type parameter. One home for both positions: an opaque's
    /// list is its own predicates (what the checker reads as `Predicates(k)`).
    pub fn impl_bounds(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, list: &[(lf::TypeId, PTy)], sized: Option<LibKey>, out: &mut Preds) -> Result<(), Why> {
        for &(t, me) in list {
            let lf::TypeRef::Impl(bounds) = v.ty(t)? else {
                return Err(Why::Program("an `impl Trait` entry names no `impl Trait` type".into(), place(v, v.type_range(t))?));
            };
            let (mut stated, mut relaxed) = (false, false);
            for &b in v.rows(bounds)? {
                let s = self.bound(tys, env, v, lf::Bound::decode(b)?, (me, On::Made, Binders::NONE), sized, out)?;
                relaxed |= s == Sizing::Relaxed;
                stated |= s != Sizing::Relaxed && self.states_size(s, sized)?;
            }
            if let Some(s) = self.implied_size(stated, relaxed, sized)? {
                let args = intern_list(tys, &[me])?;
                out.push(PClause::Trait { trait_: self.heads.checker(s), args }, Src::Derived);
            }
        }
        Ok(())
    }

    /// The item bounds of a trait's associated type `v` (`type A: B1 + B2;`), `me` the type it
    /// is in its trait (`<Self as Trait<params>>::A`), `env` the trait's: each bound written on
    /// it (BOUNDS) with `me` as `Self`, and its implied `Sized`. The Reference, "Associated
    /// Items", "Associated Types": "The optional trait bounds must be fulfilled by the
    /// implementations of the type alias. There is an implicit `Sized` bound on associated
    /// types that can be relaxed using the special `?Sized` bound." What the checker reads as `Predicates(the associated type's key)` and gives the
    /// solver as the alias's bounds: without them `<S as Serializer>::SerializeStruct` is
    /// neither `Sized` nor a `SerializeStruct` in a body generic over `S: Serializer`.
    ///
    /// An associated type with type or const parameters of its own (`type Out<T>: B;`): the
    /// caller gives `me` with those parameters after the trait's (`<Self as
    /// Trait<params>>::Out<T>`, the checker's `AssocGenerics`) and an `env` that has them in
    /// scope (heads.rs `Store::trait_members`). Its OWN clauses (a bound on such a parameter,
    /// `type Out<T: Clone>;`, its where clauses, each own parameter's implied `Sized`) follow
    /// the item bounds in the same list ([`Self::assoc_own_clauses`], decision D89 (b)).
    pub fn assoc_ty_bounds(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, me: PTy, sized: Option<LibKey>, out: &mut Preds) -> Result<(), Why> {
        // `type X: ?Sized;` keeps `<Self as Tr>::X: MetaSized` ([`Self::implied_size`]).
        let (mut stated, mut relaxed) = (false, false);
        for &b in v.rows(v.list(head::BOUNDS))? {
            let s = self.bound(tys, env, v, lf::Bound::decode(b)?, (me, On::Made, Binders::NONE), sized, out)?;
            relaxed |= s == Sizing::Relaxed;
            stated |= s != Sizing::Relaxed && self.states_size(s, sized)?;
        }
        if let Some(s) = self.implied_size(stated, relaxed, sized)? {
            let args = intern_list(tys, &[me])?;
            out.push(PClause::Trait { trait_: self.heads.checker(s), args }, Src::Derived);
        }
        self.assoc_own_clauses(tys, env, v, me, sized, out)
    }

    /// The OWN clauses of the associated type `v` (decision D89 (b)), appended to `out.clauses`
    /// after its item bounds: the bounds written on its own type parameters (`type Out<T:
    /// Clone>;`), its where clauses (`type Out<T> where T: Clone;`), merged in writing order,
    /// and each own type parameter's implied `Sized` (relaxed by `?Sized` as on any item:
    /// [`Self::predicates`]). The Reference, "Associated Items", "Associated Types":
    /// "Associated types may include generic parameters and where clauses"; "Generic
    /// parameters": a bound on a parameter is the where clause `T: Bound`. `me` is the alias as
    /// [`Self::assoc_ty_bounds`] takes it, its list the trait's parameters then the item's own,
    /// so the own parameters start at its length less their count (heads.rs `own_generics`
    /// numbers them the same way).
    ///
    /// What they ARE for the checker: requirements of the projection, not facts of it. The
    /// solver's well-formedness of `<X as Tr>::Out<A>` proves each with the projection's
    /// arguments substituted (blaze-solve structural.rs, the `Projection` arm), so `Out<NotClone>`
    /// is the language's E0277 naming `Clone`; nothing is kept unchecked (rule 3). They share the
    /// associated item's `Predicates` with its item bounds, and the two kinds are told apart by
    /// one rule both sides apply: an item bound's self type is the alias itself or a projection
    /// whose self chain reaches it; an own clause's never is. So an own clause on the alias
    /// itself (`where Self::Out<T>: Bound`) is refused by name, never filed as an item bound.
    /// A `where const` condition on an associated type is refused by name (no caller keys it).
    pub fn assoc_own_clauses(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, me: PTy, sized: Option<LibKey>, out: &mut Preds) -> Result<(), Why> {
        let mut own = 0u32;
        for &n in v.rows(v.list(head::GENERICS))? {
            if !matches!(lf::GenericParam::decode(n)?, lf::GenericParam::Lifetime { .. }) {
                own += 1;
            }
        }
        if own == 0 && v.rows(v.list(head::WHERE))?.is_empty() {
            return Ok(());
        }
        let Some(&PNode::Alias { args, .. }) = tys.view().get(me) else {
            return Err(Why::Program("an associated type's own clauses: its alias is no alias node".into(), Range::new(0, 0)));
        };
        let len = tys.view().list(args).map_err(|_| Why::Program("an associated type's alias list is past the type table".into(), Range::new(0, 0)))?.len();
        let base = u32::try_from(len)
            .ok()
            .and_then(|l| l.checked_sub(own))
            .ok_or_else(|| Why::Program("an associated type's alias lists fewer arguments than its own parameters".into(), Range::new(0, 0)))?;
        // In the associated type's scope (its own lifetimes after its trait's: the caller's).
        let mut mine = Preds::new_in(out.scope.clone_in(out.scope.alloc()));
        self.predicates(tys, env, v, base, None, sized, &mut mine)?;
        if !mine.conds.is_empty() {
            return Err(Why::Unsupported("a `where const` condition on an associated type", Range::new(0, 0)));
        }
        for c in mine.clauses.iter() {
            let first = match *c {
                PClause::Trait { args, .. } | PClause::Projection { args, .. } => {
                    tys.view().list(args).map_err(|_| Why::Program("an own clause's list is past the type table".into(), Range::new(0, 0)))?.first().copied()
                }
                // `T: 'a` on the alias itself is a clause on the alias as much as a trait bound.
                PClause::TypeOutlives { ty, .. } => Some(ty),
                PClause::RegionOutlives { .. } => None,
                PClause::ForFields { container, .. } => Some(container),
                PClause::Const { .. } => return Err(Why::Unsupported("a `where const` condition on an associated type", Range::new(0, 0))),
            };
            if let Some(s) = first
                && reaches_alias(tys, s, me)?
            {
                return Err(Why::Unsupported(
                    "a where clause of a generic associated type on the associated type itself (`type Out<T> where Self::Out<T>: Bound`)",
                    Range::new(0, 0),
                ));
            }
        }
        out.append(mine);
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "the item's text, where it is written, and the out lists")]
    pub fn predicates(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, base: u32, self_bounds: Option<PTy>, sized: Option<LibKey>, out: &mut Preds) -> Result<(), Why> {
        self.predicates_implying(tys, env, v, base, self_bounds, sized, out, &mut |_| {})
    }

    /// [`Self::predicates`], and each clause that `X: Trait` implies handed to `implied` as it
    /// is made (a trait's head: `self_bounds` is `Some(Self)`). The reference's
    /// `explicit_implied_clauses_of` (`PredicateFilter::SelfAndAssociatedTypeBounds`,
    /// rustc_hir_analysis collect/predicates_of.rs): every clause of the header's bounds list
    /// (`trait Tr: Super<A: B>` gives `Self: Super` AND `<Self as Super>::A: B`), and of a
    /// `where` predicate whose bounded type is `Self` (with its associated item constraints
    /// too). A `where` predicate on another type, `<Self as Super>::A: B` written in the
    /// `where` list included, is a clause of the trait's own items but not implied (the
    /// Reference, "Traits", "Supertraits"; rust-lang/rust#20671). No implied clause is an own
    /// parameter's implied `Sized` (a trait's `Self` has none here, and those are the only ones
    /// the end of this function changes or drops), so each is handed over final.
    #[expect(clippy::too_many_arguments, reason = "the item's text, where it is written, and the out lists")]
    pub fn predicates_implying(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'n>,
        base: u32,
        self_bounds: Option<PTy>,
        sized: Option<LibKey>,
        out: &mut Preds,
        implied: &mut dyn FnMut(PClause),
    ) -> Result<(), Why> {
        // The Reference, "Trait and lifetime bounds": every type parameter is `Sized` unless its
        // bounds say otherwise (`?Sized`, or a sizedness trait: [`Self::states_size`]), on the
        // parameter or in a `where` clause of the same item. Each own parameter, and the index
        // of its implied `Sized` clause while no bound has said otherwise. `?Sized` (in either
        // place) does not take the clause away: it relaxes it to `MetaSized`
        // ([`Self::implied_size`]; `relax`, applied at the end); a written sizedness trait
        // (`Sized`, `MetaSized`, `PointeeSized`) takes it away (`dropped`).
        let mut own: Vec<(PTy, Option<usize>), Global> = Vec::new_in(Global);
        let mut relax: Vec<usize, Global> = Vec::new_in(Global);
        let mut i = base;
        for &n in v.rows(v.list(head::GENERICS))? {
            // A const parameter takes an index too (`heads::param_names`) and is never `Sized`.
            if let lf::GenericParam::Const { .. } = lf::GenericParam::decode(n)? {
                i += 1;
                continue;
            }
            // `'a: 'b + 'c` (a lifetime parameter takes no index among the types).
            if let lf::GenericParam::Lifetime { name, bounds } = lf::GenericParam::decode(n)? {
                self.region_outlives(v, name, bounds, out)?;
                continue;
            }
            if let lf::GenericParam::Type { bounds, .. } = lf::GenericParam::decode(n)? {
                let me = intern(tys, PNode::Param(i))?;
                let (mut stated, mut relaxed) = (false, false);
                for &b in v.rows(bounds)? {
                    let s = self.bound(tys, env, v, lf::Bound::decode(b)?, (me, On::Made, Binders::NONE), sized, out)?;
                    relaxed |= s == Sizing::Relaxed;
                    stated |= s != Sizing::Relaxed && self.states_size(s, sized)?;
                }
                let mut at = None;
                if let (false, Some(s)) = (stated, sized) {
                    let args = intern_list(tys, &[me])?;
                    at = Some(out.clauses.len());
                    if relaxed {
                        relax.push(out.clauses.len());
                    }
                    out.push(PClause::Trait { trait_: self.heads.checker(s), args }, Src::Derived);
                }
                own.push((me, at));
                i += 1;
            }
        }
        if let Some(me) = self_bounds {
            for &b in v.rows(v.list(head::BOUNDS))? {
                let b = lf::Bound::decode(b)?;
                // `trait Tr: Super` and `trait Tr: 'a` (`Self: 'a` is implied as a supertrait
                // is: the Reference, "Traits", "Supertraits" and "Trait and lifetime bounds").
                if let lf::Bound::Use(_) = b {
                    continue;
                }
                let from = out.clauses.len();
                if self.bound(tys, env, v, b, (me, On::Made, Binders::NONE), sized, out)? == Sizing::Relaxed {
                    // The Reference, "Trait and lifetime bounds": `?` relaxes the implied
                    // bound of a type parameter or associated type only; `Self` has none.
                    let lf::Bound::Trait { ty, .. } = b else { return Err(Why::Program("`?` on a lifetime bound".into(), Range::new(0, 0))) };
                    return Err(Why::Unsupported("`?Sized` on a trait's `Self`", place(v, v.type_range(ty))?));
                }
                for &c in &out.clauses[from..] {
                    implied(c);
                }
            }
        }
        let mut dropped = Vec::new_in(Global);
        for &n in v.rows(v.list(head::WHERE))? {
            match lf::WherePred::decode(n)? {
                lf::WherePred::Const { cond } => out.conds.push(cond),
                lf::WherePred::Lifetime { name, bounds } => self.region_outlives(v, name, bounds, out)?,
                lf::WherePred::Type { binder, ty, bounds } => {
                    let r = place(v, v.type_range(ty))?;
                    lifetimes_only(v, binder, r)?;
                    let me = self.ty(tys, env, v, ty)?;
                    let mine = own.iter().find(|(t, _)| *t == me).copied();
                    let on_self = self_bounds == Some(me);
                    for &b in v.rows(bounds)? {
                        let from = out.clauses.len();
                        let s = self.bound(tys, env, v, lf::Bound::decode(b)?, (me, On::Written(ty), Binders::outer(binder)), sized, out)?;
                        if on_self {
                            for &c in &out.clauses[from..] {
                                implied(c);
                            }
                        }
                        match mine {
                            // `where T: ?Sized` on the item's own parameter relaxes its default
                            // to `MetaSized`; a sizedness trait there takes it away.
                            Some((_, Some(at))) if s == Sizing::Relaxed => relax.push(at),
                            Some((_, Some(at))) if self.states_size(s, sized)? => dropped.push(at),
                            Some(_) => {}
                            // `?Sized` on what has no implied bound here (an outer item's
                            // parameter, a type that is no parameter) is the language's error.
                            None if s == Sizing::Relaxed => {
                                return Err(Why::Unsupported("`?Sized` in a where clause on a type that is no type parameter of this item", r));
                            }
                            None => {}
                        }
                    }
                }
                lf::WherePred::ForFields { param, container, subject, bounds } => {
                    self.for_fields_pred(tys, env, v, param, container, subject, bounds, self_bounds, out, implied)?;
                }
            }
        }
        dropped.sort_unstable();
        dropped.dedup();
        relax.sort_unstable();
        relax.dedup();
        if !relax.is_empty() {
            let meta = self.implied_size(false, true, sized)?;
            for &at in relax.iter() {
                if dropped.binary_search(&at).is_ok() {
                    continue;
                }
                match (meta, &mut out.clauses[at]) {
                    (Some(m), PClause::Trait { trait_, .. }) => *trait_ = self.heads.checker(m),
                    (None, _) => dropped.push(at),
                    (Some(_), _) => return Err(Why::Program("an own parameter's implied `Sized` is no trait clause".into(), Range::new(0, 0))),
                }
            }
            dropped.sort_unstable();
        }
        for &at in dropped.iter().rev() {
            out.remove_clause(at);
        }
        Ok(())
    }

    /// `'long: 's1 + 's2` (a lifetime parameter's bounds, or a `where` lifetime predicate): one
    /// [`PClause::RegionOutlives`] per bound, each region by the item's scope.
    /// A lifetime the regions' reader does not name (`'_: 'a`, E0637) is a regions refusal by
    /// name, the clause left out (as `T: '_` in `one_bound`), never the head's error; an
    /// undeclared one stays the program's (E0261).
    fn region_outlives(&self, v: &View<'n>, long: lf::NameIdx, bounds: lf::List, out: &mut Preds) -> Result<(), Why> {
        let l = heads_regions::lifetime(v, &out.scope, Binders::NONE, long);
        for &b in v.rows(bounds)? {
            let lf::Bound::Lifetime(short) = lf::Bound::decode(b)? else {
                return Err(Why::Program("a lifetime bounded by something other than a lifetime".into(), place(v, v.name_range(long))?));
            };
            let s = heads_regions::lifetime(v, &out.scope, Binders::NONE, short);
            match (l, s) {
                (Ok(l), Ok(s)) => out.push(PClause::RegionOutlives { long: l, short: s }, Src::RegionOutlives { long, short }),
                // A name no scope declares stays the program's error (E0261).
                (Err(why @ heads_regions::RegionsRefusal::Undeclared { .. }), _) | (_, Err(why @ heads_regions::RegionsRefusal::Undeclared { .. })) => return Err(Why::Regions(why)),
                (Err(why), _) | (_, Err(why)) => out.refuse_regions(why),
            }
        }
        Ok(())
    }

    /// `where for<F in fields(T)> F: Bound1 + Bound2<Arg>`: verifies `subject` names `param`,
    /// lowers `container`, and emits one [`PClause::ForFields`] per trait bound.
    #[expect(clippy::too_many_arguments, reason = "mirrors predicates_implying parameters")]
    fn for_fields_pred(
        &self,
        tys: &mut PortableTys<'_>,
        env: &Env<'_>,
        v: &View<'n>,
        param: lf::NameIdx,
        container: lf::TypeId,
        subject: lf::TypeId,
        bounds: lf::List,
        self_bounds: Option<PTy>,
        out: &mut Preds,
        implied: &mut dyn FnMut(PClause),
    ) -> Result<(), Why> {
        let container_ty = self.ty(tys, env, v, container)?;
        let sr = place(v, v.type_range(subject))?;
        let param_text = v.name_text(param)?;
        let ok_subject = match v.ty(subject)? {
            lf::TypeRef::Path(p) => {
                let (abs, texts, segs, _) = path_texts_of(v, p, false)?;
                !abs
                    && texts.len() == 1
                    && texts.first().copied() == Some(param_text)
                    && segs.first().is_some_and(|s| s.kind == lf::SegKind::Ident && !s.has_args && !s.parenthesized)
            }
            _ => false,
        };
        if !ok_subject {
            return Err(Why::Unsupported("a `for<F in fields(..)>` bound whose subject is not the bound field parameter", sr));
        }
        let on_self = self_bounds == Some(container_ty);
        for &row in v.rows(bounds)? {
            let b = lf::Bound::decode(row)?;
            let lf::Bound::Trait { ty, binder, mods } = b else {
                return Err(Why::Unsupported("a non-trait bound in `for<F in fields(..)>`", sr));
            };
            let br = place(v, v.type_range(ty))?;
            lifetimes_only(v, binder, br)?;
            if mods.negative {
                return Err(Why::Unsupported("a negative bound (`!Trait`) in `for<F in fields(..)>`", br));
            }
            if mods.asyncness {
                return Err(Why::Unsupported("an async bound (`async Fn`) in `for<F in fields(..)>`", br));
            }
            if mods.maybe {
                return Err(Why::Unsupported("`?Trait` in `for<F in fields(..)>`", br));
            }
            let lf::TypeRef::Path(p) = v.ty(ty)? else {
                return Err(Why::Unsupported("a non-path bound in `for<F in fields(..)>`", br));
            };
            let (abs, texts, segs, pr) = path_texts_of(v, p, true)?;
            let target = match (self.bound_target(env, v, p, abs, &texts, pr), out.trait_misses.as_mut()) {
                (Err(Why::Resolve(crate::resolve::ResolveError::Path(e), at)), Some(misses)) if super::type_miss::decided(e) => {
                    misses.push((e, at));
                    continue;
                }
                (target, _) => target?,
            };
            let key = match target {
                Bounded::Trait(key) => key,
                Bounded::Alias(_) => {
                    return Err(Why::Unsupported("a trait alias in `for<F in fields(..)>`", pr));
                }
            };
            let args = if let Some(last) = segs.last() {
                let (given, binds, nested) = self.bound_segment_args(tys, env, v, target, last, pr)?;
                if !binds.is_empty() || !nested.is_empty() {
                    return Err(Why::Unsupported("an associated type constraint in `for<F in fields(..)>`", pr));
                }
                self.trait_args_after_self(tys, key, pr, &given)?
            } else {
                self.trait_args_after_self(tys, key, pr, &[])?
            };
            let clause = PClause::ForFields { container: container_ty, trait_: self.heads.checker(key), args };
            out.push(clause, Src::Derived);
            if on_self {
                implied(clause);
            }
        }
        Ok(())
    }
}
