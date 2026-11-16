//! An item's where-clauses as the checker's [`PClause`]s: each type parameter's bounds and
//! implied `Sized`, the `where` list's type predicates, a trait's supertraits on `Self`.
//! Ported from front.rs's `bound` and `predicates` (whose copies go at the switch). A
//! `where const` condition is not passed over silently: it is collected for the caller
//! ([`Preds::conds`]), which makes its clause from the condition's own item.

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf, head};
use blaze_surface::Range;
use blaze_typeck::{List, Name, PClause, PNode, PTy, PortableTys};

use super::types::{Bounded, Bounds, Builder, Env, Heads, ParamBound, Why, intern, intern_list, path_texts, path_texts_of, subst_clause};

/// What one bound says of its type's sizedness ([`Builder::bound`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sizing {
    /// `?Sized`: no clause.
    Relaxed,
    /// A trait bound, its trait.
    Trait(LibKey),
    /// A lifetime bound (`T: 'a`) or `use<..>`.
    NoTrait,
}

/// What [`Builder::predicates`] found besides clauses: the item's `where const` conditions, by
/// their const ids in the item's text, for the caller to key and evaluate.
pub struct Preds {
    pub clauses: Vec<PClause, Global>,
    pub conds: Vec<lf::ConstIdx, Global>,
}

impl Preds {
    pub fn new() -> Preds {
        Preds { clauses: Vec::new_in(Global), conds: Vec::new_in(Global) }
    }
}

impl Default for Preds {
    fn default() -> Self {
        Preds::new()
    }
}

fn place(v: &View<'_>, r: Result<blaze_lower::BodyRange, lf::BadRow>) -> Result<Range, Why> {
    Ok(v.site(r?)?)
}

/// A `for<..>` binder (on a bound, a where clause, a `dyn` bound, a fn pointer type): the
/// Reference, "Higher-ranked trait bounds" and "Function pointer types": it binds lifetimes,
/// and this checker erases every lifetime (`&'a T` is `Ref(T)`), so the binder goes with them.
/// A type or const in it (`for<T>`, the unstable non-lifetime binders) is refused by name.
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
fn lifetime_args(v: &View<'_>, args: lf::List) -> Result<bool, Why> {
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
    let mut nested = Vec::new_in(Global);
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
    for &n in v.rows(seg.args)? {
        match lf::GenericArg::decode(n)? {
            lf::GenericArg::Type(t) => given.push(make(tys, t)?),
            lf::GenericArg::Lifetime(_) => {}
            // A lifetime argument of a generic associated type is erased like every lifetime
            // (`Pattern<Searcher<'a>: Clone>`, core/src/str/iter.rs:612), as `P::Searcher<'a>`
            // is in a type.
            lf::GenericArg::AssocType { name, args, ty } if lifetime_args(v, args)? => binds.push((Name(v.name(name)?), make(tys, ty)?)),
            lf::GenericArg::AssocBound { name, args, bounds } if lifetime_args(v, args)? => nested.push((Name(v.name(name)?), bounds)),
            lf::GenericArg::AssocBound { .. } => return Err(Why::Unsupported("an associated type bound with type or const arguments (`Item<T>: Bound`)", r)),
            lf::GenericArg::Const(_) => return Err(Why::Unsupported("a const generic argument in a bound", r)),
            _ => return Err(Why::Unsupported("this generic argument in a bound", r)),
        }
    }
    Ok((given, binds, nested))
}

impl<'b, 'n, H: Heads> Builder<'b, 'n, H> {
    /// The bound `b` on `self_ty` as clauses into `out`: the trait's, and a projection for each
    /// `Name = Type` argument; what it says of sizedness (`sized` is the `sized` lang trait).
    #[expect(clippy::too_many_arguments, reason = "the bound, where it is written, and the out list")]
    pub fn bound(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, b: lf::Bound, self_ty: PTy, sized: Option<LibKey>, out: &mut Vec<PClause, Global>) -> Result<Sizing, Why> {
        // The bound, then each associated type bound written in it, in writing order (a work
        // list: `Tr<A: B<C: D>>` nests). The Reference, "Trait and lifetime bounds" (associated
        // item constraints, RFC 2289): `T: Tr<Assoc: Bounds>` is `T: Tr` and
        // `<T as Tr>::Assoc: Bounds`; core: `R: [const] Try<Output = T, Residual: Residual<..>>`.
        const MOST_NESTED: usize = 256;
        let mut todo: Vec<(lf::Bound, PTy), Global> = Vec::new_in(Global);
        let first = self.one_bound(tys, env, v, b, self_ty, sized, out, &mut todo)?;
        let mut i = 0;
        while i < todo.len() {
            if i == MOST_NESTED {
                let lf::Bound::Trait { ty, .. } = b else { break };
                return Err(Why::Unsupported("associated type bounds nested past 256", place(v, v.type_range(ty))?));
            }
            let (nb, on) = todo[i];
            if self.one_bound(tys, env, v, nb, on, sized, out, &mut todo)? == Sizing::Relaxed {
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
    fn one_bound(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, b: lf::Bound, self_ty: PTy, sized: Option<LibKey>, out: &mut Vec<PClause, Global>, todo: &mut Vec<(lf::Bound, PTy), Global>) -> Result<Sizing, Why> {
        let lf::Bound::Trait { ty, binder, mods } = b else { return Ok(Sizing::NoTrait) };
        let r = place(v, v.type_range(ty))?;
        lifetimes_only(v, binder, r)?;
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
        let (given, binds, nested) = match segs.last() {
            Some(last) => segment_args(tys, v, last, pr, &mut |tys, t| self.ty(tys, env, v, t))?,
            None => (Vec::new_in(Global), Vec::new_in(Global), Vec::new_in(Global)),
        };
        let key = match self.bound_target(env, abs, &texts, pr)? {
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
                self.alias_clauses(tys, alias, pr, self_ty, &given, out)?;
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
        out.push(PClause::Trait { trait_: self.heads.checker(key), args });
        // `Name = T` is a projection on the trait among `key` and its supertraits that declares
        // `Name` (`Fn(A) -> B`'s `Output` is `FnOnce`'s).
        for (name, term) in binds {
            let shown = v.text_at(v.path_range(p)?)?;
            let (owner, oargs) = self.assoc_owner(tys, key, args, name, pr, shown)?.ok_or_else(|| Why::AssocNotFound(shown.into(), pr))?;
            out.push(PClause::Projection { trait_: self.heads.checker(owner), args: oargs, name, term });
        }
        // `Name: Bounds`: each bound on `<self_ty as Owner>::Name`, the owner found as for
        // `Name = T`.
        for (name, list) in nested {
            let shown = v.text_at(v.path_range(p)?)?;
            let (owner, oargs) = self.assoc_owner(tys, key, args, name, pr, shown)?.ok_or_else(|| Why::AssocNotFound(shown.into(), pr))?;
            let proj = intern(tys, PNode::Alias { trait_: self.heads.checker(owner), name, args: oargs })?;
            for &row in v.rows(list)? {
                todo.push((lf::Bound::decode(row)?, proj));
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

    /// The trait bounds on type parameters written in `v` (on its own parameters, from index
    /// `base`, and in its `where` clauses on any parameter in scope), appended to `out`, which
    /// holds the container's: what `T::X` is looked up through. Each is lowered with the ones
    /// before it in scope. Only the trait and its arguments are read here; the bound's other
    /// parts (`Name = Type`, `?Sized`, `!Trait`) are [`Builder::predicates`]'s to take or refuse.
    pub fn param_bounds(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, base: u32, out: &mut Vec<ParamBound, Global>) -> Result<(), Why> {
        let mut i = base;
        for &n in v.rows(v.list(head::GENERICS))? {
            // A const parameter takes an index too (`heads::param_names`), and has no bounds.
            match lf::GenericParam::decode(n)? {
                lf::GenericParam::Type { bounds, .. } => {
                    for &b in v.rows(bounds)? {
                        self.param_bound(tys, env, v, lf::Bound::decode(b)?, i, out)?;
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
            let Some(&(_, param)) = env.params.iter().find(|(n, _)| *n == Name::of(t)) else { continue };
            for &b in v.rows(bounds)? {
                self.param_bound(tys, env, v, lf::Bound::decode(b)?, param, out)?;
            }
        }
        Ok(())
    }

    fn param_bound(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, b: lf::Bound, param: u32, out: &mut Vec<ParamBound, Global>) -> Result<(), Why> {
        let lf::Bound::Trait { ty, mods, .. } = b else { return Ok(()) };
        // `?Sized` and `!Trait` declare no associated type of the parameter.
        if mods.maybe || mods.negative {
            return Ok(());
        }
        let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(()) };
        let e = Env { bounds: Bounds::Building(out), ..*env };
        let (abs, texts, segs, pr) = path_texts_of(v, p, true)?;
        let given = match segs.last() {
            Some(last) => segment_args(tys, v, last, pr, &mut |tys, t| self.ty(tys, &e, v, t))?.0,
            None => Vec::new_in(Global),
        };
        let me = intern(tys, PNode::Param(param))?;
        match self.bound_target(&e, abs, &texts, pr)? {
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
    /// The predicates of a fn's anonymous type parameters (`env.anon`, its `impl Trait`
    /// parameter types): each one's bounds as written on it, and its implied `Sized` unless
    /// `?Sized` is written, as for a declared type parameter.
    pub fn anon_predicates(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, sized: Option<LibKey>, out: &mut Preds) -> Result<(), Why> {
        for &(t, i) in env.anon {
            let lf::TypeRef::Impl(bounds) = v.ty(t)? else {
                return Err(Why::Program(format!("anonymous parameter {i} is no `impl Trait` type").into_boxed_str(),place(v, v.type_range(t))?));
            };
            let me = intern(tys, PNode::Param(i))?;
            let mut stated = false;
            for &b in v.rows(bounds)? {
                let s = self.bound(tys, env, v, lf::Bound::decode(b)?, me, sized, &mut out.clauses)?;
                stated |= self.states_size(s, sized)?;
            }
            if let (false, Some(s)) = (stated, sized) {
                let args = intern_list(tys, &[me])?;
                out.clauses.push(PClause::Trait { trait_: self.heads.checker(s), args });
            }
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "the item's text, where it is written, and the out lists")]
    pub fn predicates(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'n>, base: u32, self_bounds: Option<PTy>, sized: Option<LibKey>, out: &mut Preds) -> Result<(), Why> {
        // The Reference, "Trait and lifetime bounds": every type parameter is `Sized` unless its
        // bounds say otherwise (`?Sized`, or a sizedness trait: [`Self::states_size`]), on the
        // parameter or in a `where` clause of the same item. Each own parameter, and the index
        // of its implied `Sized` clause while no bound has said otherwise.
        let mut own: Vec<(PTy, Option<usize>), Global> = Vec::new_in(Global);
        let mut i = base;
        for &n in v.rows(v.list(head::GENERICS))? {
            // A const parameter takes an index too (`heads::param_names`) and is never `Sized`.
            if let lf::GenericParam::Const { .. } = lf::GenericParam::decode(n)? {
                i += 1;
                continue;
            }
            if let lf::GenericParam::Type { bounds, .. } = lf::GenericParam::decode(n)? {
                let me = intern(tys, PNode::Param(i))?;
                let mut stated = false;
                for &b in v.rows(bounds)? {
                    let s = self.bound(tys, env, v, lf::Bound::decode(b)?, me, sized, &mut out.clauses)?;
                    stated |= self.states_size(s, sized)?;
                }
                let mut at = None;
                if let (false, Some(s)) = (stated, sized) {
                    let args = intern_list(tys, &[me])?;
                    at = Some(out.clauses.len());
                    out.clauses.push(PClause::Trait { trait_: self.heads.checker(s), args });
                }
                own.push((me, at));
                i += 1;
            }
        }
        if let Some(me) = self_bounds {
            for &b in v.rows(v.list(head::BOUNDS))? {
                let b = lf::Bound::decode(b)?;
                if let lf::Bound::Trait { ty, .. } = b {
                    if self.bound(tys, env, v, b, me, sized, &mut out.clauses)? == Sizing::Relaxed {
                        // The Reference, "Trait and lifetime bounds": `?` relaxes the implied
                        // bound of a type parameter or associated type only; `Self` has none.
                        return Err(Why::Unsupported("`?Sized` on a trait's `Self`", place(v, v.type_range(ty))?));
                    }
                }
            }
        }
        let mut dropped = Vec::new_in(Global);
        for &n in v.rows(v.list(head::WHERE))? {
            match lf::WherePred::decode(n)? {
                lf::WherePred::Const { cond } => out.conds.push(cond),
                lf::WherePred::Lifetime { .. } => {}
                lf::WherePred::Type { binder, ty, bounds } => {
                    let r = place(v, v.type_range(ty))?;
                    lifetimes_only(v, binder, r)?;
                    let me = self.ty(tys, env, v, ty)?;
                    let mine = own.iter().find(|(t, _)| *t == me).copied();
                    for &b in v.rows(bounds)? {
                        let s = self.bound(tys, env, v, lf::Bound::decode(b)?, me, sized, &mut out.clauses)?;
                        match mine {
                            // A where clause on the item's own parameter takes its default away.
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
            }
        }
        dropped.sort_unstable();
        dropped.dedup();
        for &at in dropped.iter().rev() {
            out.clauses.remove(at);
        }
        Ok(())
    }
}
