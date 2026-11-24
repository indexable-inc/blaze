//! Generic associated types: what a bound's binding and an associated type's where clauses
//! say when the associated type has parameters of its own.
//!
//! The Reference, "Associated Items", "Associated Types": "Associated types may include generic
//! parameters and where clauses; these are often referred to as generic associated types, or
//! GATs", declared as `type Assoc<Params> where WhereBounds;` and defined by an impl as `type
//! Assoc<Params> = Type where WhereBounds;`. The Reference, "Paths", "Qualified paths": "Fully
//! qualified paths allow for disambiguating the path for trait implementations and for
//! specifying canonical paths": `<T as Tr<A>>::Out<B>` is `Out` of the impl of `Tr<A>` for `T`
//! at `B`.
//!
//! THE FORM. A projection lists the trait's arguments, `Self` first, then the associated type's
//! own type and const arguments in declaration order (`tc::AssocGenerics`; blaze-solve alias.rs
//! `step_impl`: "the impl's arguments, then the item's own (the goal's arguments after the
//! trait's)"). Lifetimes are in no list: a portable type holds no region (`&'a T` is `Ref(T)`),
//! so `type Iter<'a>` has no own parameter here and `Iter<'a>` no own argument, exactly as every
//! other lifetime argument is treated today (`preds::lifetime_args`, `own_generics`). The
//! borrow check reads lifetimes from the signatures' own records (lifetimes.rs), not from
//! these types.
//!
//! WHAT IS HERE.
//! - [`Builder::gat_bindings`]: `U: Tr<Out<T> = X>` (hybrid_array's `U: ArraySize<ArrayType<T> =
//!   [T; N]>`) is the projection clause `<U as Tr>::Out<T> == X`, its alias carrying `[U, T]`.
//!   A const argument written in braces or as a literal (`Out<3> = X`, `Out<{ N }> = X`) is
//!   refused under its own name; one written like a type (`Out<N> = X`) is made as a type and
//!   refused by the type builder when `N` is a const parameter.
//! - [`member_where`]: the where clauses of an associated type (in a trait or an impl). An
//!   outlives clause (`where Self: 'a`, `T: 'a`, `'a: 'b`) is READ AND KEPT ([`AssocWhere`],
//!   filed by the trait's head in `Maps::assoc_where`) and NOT CHECKED: nothing proves it where
//!   the associated type is used, and nothing requires it of an impl. That gap has one name,
//!   [`OUTLIVES_NOT_CHECKED`], which the kept clauses are listed under. A clause with a trait
//!   bound (`where T: Clone`), like a bound on an own parameter (`type Out<T: Clone>`), is one
//!   of the associated type's OWN clauses (decision D89 (b)): preds.rs
//!   `Builder::assoc_own_clauses` files them in the item's `Predicates` after its item bounds,
//!   and blaze-solve's well-formedness of the projection proves them at each use with the
//!   projection's arguments substituted. In an impl (`type Out<T: Clone> = Vec<T>;`) they are
//!   the value's assumptions. OPEN (heads.rs's impl-member path, another lane's file): that an
//!   impl's own clauses are implied by its trait's (the language's E0276, "impl has stricter
//!   requirements than trait") is not checked yet; an impl's value is not checked for
//!   well-formedness there either, so nothing yet rests on an impl's own clauses.

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf, head};
use blaze_surface::Range;
use blaze_typeck::{List, Name, PClause, PortableTys};

use super::preds::SegArgs;
use super::types::{Builder, Env, Heads, Why, intern_list};

/// The named gap of the outlives clauses kept by [`member_where`]: they are on record with
/// the trait's head and no check reads them yet.
pub const OUTLIVES_NOT_CHECKED: &str = "an outlives where clause on an associated type (`type Iter<'a> where Self: 'a`) is kept with the trait's head and not checked";

/// The most outlives clauses kept for one associated type (rule 18); more is refused by name.
pub const MOST_OUTLIVES: usize = 8;

/// What an outlives clause is written on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Outlived {
    /// `Self: 'a`.
    #[default]
    SelfTy,
    /// `T: 'a`, `Vec<T>: 'a`: the type as written, by its text's name.
    Type(Name),
    /// `'b: 'a`: the lifetime's name.
    Lifetime(Name),
}

/// One outlives clause of an associated type: `subject: 'lifetime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Outlives {
    pub subject: Outlived,
    /// The lifetime's name as the lowering interned it (`View::name`).
    pub lifetime: Name,
}

/// The where clauses kept for one associated type: its outlives clauses in writing order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssocWhere {
    clauses: [Outlives; MOST_OUTLIVES],
    len: u8,
}

impl Default for AssocWhere {
    fn default() -> AssocWhere {
        AssocWhere { clauses: [Outlives { subject: Outlived::SelfTy, lifetime: Name(0) }; MOST_OUTLIVES], len: 0 }
    }
}

impl AssocWhere {
    /// The clauses, in writing order.
    pub fn outlives(&self) -> &[Outlives] {
        &self.clauses[..self.len as usize]
    }

    /// Whether no clause is kept.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The clauses `kept` (at most [`MOST_OUTLIVES`], which the caller checked), as read back
    /// from a head record (head_record.rs, form 9).
    pub fn of(kept: &[Outlives]) -> AssocWhere {
        let mut w = AssocWhere::default();
        let n = kept.len().min(MOST_OUTLIVES);
        w.clauses[..n].copy_from_slice(&kept[..n]);
        w.len = n as u8;
        w
    }

    fn push(&mut self, o: Outlives, at: Range) -> Result<(), Why> {
        let Some(slot) = self.clauses.get_mut(self.len as usize) else {
            return Err(Why::Unsupported("more than 8 outlives where clauses on one associated type", at));
        };
        *slot = o;
        self.len += 1;
        Ok(())
    }
}

/// The where clauses of the associated type whose text `v` views (a trait's `type Iter<'a>
/// where Self: 'a;` or an impl's `type Iter<'a> = .. where Self: 'a;`): each outlives clause
/// kept ([`OUTLIVES_NOT_CHECKED`]); a trait bound passed over HERE because it is the
/// associated item's own clause, filed and proved by preds.rs `Builder::assoc_own_clauses`
/// (D89 (b)); a `use<..>` bound or a `where const` condition refused by name.
pub(super) fn member_where(v: &View<'_>, name: &str) -> Result<AssocWhere, Why> {
    let _ = name;
    let mut kept = AssocWhere::default();
    let list = v.list(head::WHERE);
    for (k, &row) in v.rows(list)?.iter().enumerate() {
        let at = v.site(v.row_range(list.start + k as u32)?)?;
        let (subject, bounds) = match lf::WherePred::decode(row)? {
            lf::WherePred::Type { ty, bounds, .. } => {
                let text = v.text_at(v.type_range(ty)?)?.trim();
                let subject = if text == "Self" { Outlived::SelfTy } else { Outlived::Type(Name::of(text)) };
                (subject, bounds)
            }
            lf::WherePred::Lifetime { name, bounds } => (Outlived::Lifetime(Name(v.name(name)?)), bounds),
            lf::WherePred::Const { .. } => return Err(Why::Unsupported("a `where const` condition on an associated type", at)),
            lf::WherePred::ForFields { .. } => return Err(Why::Unsupported("a `for<F in fields(..)>` bound on an associated type", at)),
        };
        for &b in v.rows(bounds)? {
            match lf::Bound::decode(b)? {
                lf::Bound::Lifetime(l) => kept.push(Outlives { subject, lifetime: Name(v.name(l)?) }, at)?,
                // A trait bound is one of the associated type's own clauses (decision D89 (b)):
                // in a trait, filed after its item bounds and proved at each use by the
                // projection's well-formedness (preds.rs `Builder::assoc_own_clauses`); in an
                // impl, assumed when the value is checked. Not kept here.
                lf::Bound::Trait { .. } => {}
                lf::Bound::Use(_) => return Err(Why::Unsupported("a `use<..>` bound in an associated type's where clause", at)),
            }
        }
    }
    Ok(kept)
}

// `'s` is the names source's.
impl<'b, 's, H: Heads, S: super::source::NamesSource<'s>> Builder<'b, H, S> {
    /// The bindings of generic associated types with type arguments that the last segment
    /// `seg` of a bound's path writes (`Tr<Out<T> = X>`), the bound being `args[0]: key<args[1..]>`:
    /// each as the projection clause `<args[0] as Owner<..>>::Out<T> == X` given to `out`, `Owner`
    /// the trait among `key` and its supertraits that declares `Out`
    /// ([`Builder::assoc_owner`], as for `Name = T`), its list the owner's arguments then the
    /// binding's own. The written count is checked against the declaration
    /// ([`Heads::assoc_generics`]; a count not on record is refused by name, never taken as
    /// written).
    ///
    /// A binding whose arguments are lifetimes only (or none) is not made here:
    /// `const_args_in_bounds::angle_args` hands it back as a plain `Name = T`.
    #[expect(clippy::too_many_arguments, reason = "the bound's trait reference, where it is written, and the out list")]
    pub(super) fn gat_bindings(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, seg: &lf::Segment, key: LibKey, args: List, r: Range, shown: &str, out: &mut dyn FnMut(PClause)) -> Result<(), Why> {
        if seg.return_notation || seg.parenthesized || !seg.has_args {
            return Ok(());
        }
        for &n in v.rows(seg.args)? {
            let lf::GenericArg::AssocType { name, args: own_rows, ty } = lf::GenericArg::decode(n)? else { continue };
            // The trait's arguments first, then the binding's own, in writing order.
            let (mut all, _, _) = <SegArgs as Default>::default();
            for &a in v.rows(own_rows)? {
                match lf::GenericArg::decode(a)? {
                    lf::GenericArg::Lifetime(_) => {}
                    lf::GenericArg::Type(t) => all.push(self.ty(tys, env, v, t)?),
                    lf::GenericArg::Const(_) => {
                        return Err(Why::Unsupported("a const argument in a binding of a generic associated type (`Name<3> = U`)", r));
                    }
                    lf::GenericArg::AssocType { .. } | lf::GenericArg::AssocConst { .. } | lf::GenericArg::AssocBound { .. } => {
                        return Err(Why::Program("a binding is written among the arguments of an associated type's binding".into(), r));
                    }
                }
            }
            if all.is_empty() {
                continue;
            }
            let own = all.len();
            let name = Name(v.name(name)?);
            let Some((owner, oargs)) = self.assoc_owner(tys, key, args, name, r, shown)? else {
                return Err(Why::AssocNotFound(shown.into(), r));
            };
            let Some(declared) = self.heads.assoc_generics(owner, name) else {
                return Err(Why::Unsupported("a binding with type arguments of an associated type whose own parameters are not on record", r));
            };
            if declared as usize != own {
                return Err(Why::Program("a binding of a generic associated type is written with another count of type arguments than the associated type declares".into(), r));
            }
            let of_trait = tys.view().list(oargs).map_err(|_| Why::Program("a bound's trait arguments are past the type table".into(), r))?;
            // The own arguments were pushed first: put the trait's before them.
            let (mut whole, _, _) = <SegArgs as Default>::default();
            whole.extend_from_slice(of_trait);
            whole.extend_from_slice(&all);
            let whole = intern_list(tys, &whole)?;
            let term = self.ty(tys, env, v, ty)?;
            out(PClause::Projection { trait_: self.heads.checker(owner), args: whole, name, term });
        }
        Ok(())
    }
}
