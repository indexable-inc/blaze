//! `Self` of an impl of any type, and value paths through an associated type.
//!
//! The Reference, "Paths", "`Self`" (`paths.qualifiers.type-self.impl`): "In an implementation,
//! it refers to the type being implemented." That holds whatever the implementing type is: a
//! type parameter (`impl<T: Packable> Storable for T`), an array, a primitive, a tuple, a
//! reference. The world answers the `Self` of such an impl with [`Def::SelfTy`]; its type is
//! the impl header's self type as the body sees it ([`Machine::impl_self_ty`]), and
//! `Self::f`, `Self::CONST` are that type's associated items, found by the one home for them
//! (rules/lit_path.rs `assoc_value`: a parameter's bounds first, as for `T::f`).
//!
//! `P::Assoc::name` ([`Machine::assoc_ty_of_prefix`]). The Reference, "Associated Items",
//! "Associated Types": "Associated types are type aliases associated with another type."
//! "Trait and lifetime bounds" (`bound.satisfaction`), given `Ty: Trait`: "In the body of a
//! generic function, methods from `Trait` can be called on `Ty` values. Likewise associated
//! constants on the `Trait` can be used. Associated types from `Trait` can be used." So
//! `T::Assoc` is the associated type `Assoc` of the one bound of `T` that declares it, and
//! `T::Assoc::name` an associated item of THAT type (the same section: "`trait A { type B:
//! Copy; }` is equivalent to `trait A where Self::B: Copy { type B; }`": the bounds of the
//! declaration hold for it). The compiler's error index states the three refusals:
//! E0220 "The associated type used was not defined in the trait.", E0221 "An attempt was made
//! to retrieve an associated type, but the type was ambiguous." (two traits of the bounds
//! declare the name) and E0223 (the same sentence, for a prefix that is no parameter and no
//! `Self`: "we cannot simply use `Struct::X`", the qualified `<Struct as Trait>::X` is asked).
//!
//! Which traits `Self::Assoc` looks in is rustc's `lower_assoc_path` (rustc_hir_analysis
//! hir_ty_lowering), the Reference being silent: in a trait, the trait's own bounds on `Self`
//! (the world answers `Self` there as parameter 0); in an impl of a trait, that trait and its
//! supertraits at the header's arguments, never the bounds of the self type; in an inherent
//! impl there is none (E0223).

use blaze_own::{Alloc, Vec};

use crate::body::Segment;
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Stop};
use crate::ids::ItemKey;
use crate::machine::{Lower, Machine};
use crate::ty::{Cause, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

/// The prefix of a value path through an associated type (`P` of `P::Assoc::name`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AssocPrefix {
    /// A generic parameter of the body's item (`Self` in a trait is parameter 0).
    Param(u32),
    /// `Self` in an impl.
    ImplSelf,
    /// Any other type: a struct's name, a primitive.
    Other,
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The self type of the impl the body is a member of, as the body sees it (the impl's
    /// parameters are the first of the body's own); `None` when the body is in no impl.
    pub(crate) fn impl_self_ty(&mut self, cause: Cause) -> Result<Option<Ty>, Stop> {
        let Some(parent) = self.reads.assoc_parent(self.body.owner)? else { return Ok(None) };
        let Some(h) = self.reads.impl_header(parent.container)? else { return Ok(None) };
        let t = self.lower(h.types, h.self_ty, Lower::Body, cause)?;
        Ok(Some(self.try_structurally_resolve(t)?))
    }

    /// The arguments `Self` stands at where the world answered it with the ADT `adt`
    /// (`Def::Adt`, the segment `Self` alone): those of the impl's own self type as the body
    /// sees them. The Reference, "Paths", "`Self`": "In an implementation, it refers to the
    /// type being implemented", so `Self` in `impl<const N: usize> C<N>` is `C<N>` at the
    /// impl's `N`, never `C` at fresh arguments (those are for a written name, `C::H`, through
    /// `explicit_args`). `None` after `UnresolvedPath` naming `Self`: the body is in no impl,
    /// or the impl's self type is not `adt`.
    pub(crate) fn impl_self_adt_args(&mut self, at: At, cause: Cause, adt: ItemKey) -> Result<Option<TyList>, Stop> {
        let self_name = Some(crate::ids::Name::of("Self"));
        let Some(self_ty) = self.impl_self_ty(cause)? else {
            self.diag(DiagCode::UnresolvedPath, at, About::path(self_name));
            return Ok(None);
        };
        match self.view(self_ty) {
            TyView::Adt(k, args) if k == adt => Ok(Some(args)),
            _ => {
                self.diag(DiagCode::UnresolvedPath, at, About::found_maybe_named(self_ty, self_name));
                Ok(None)
            }
        }
    }

    /// The type `P::Assoc` of the value path `P::Assoc::name` at `at`: the associated type
    /// `assoc` of the one candidate trait of `prefix` that declares it, as a projection the
    /// solver normalizes. `None`: refused and reported, naming `assoc` (E0220: no candidate
    /// declares it; E0221: two do; E0223: the prefix has no candidate traits at all).
    ///
    /// Arguments written on the associated type's segment (`T::Out::<u8>::f`, a generic
    /// associated type's own) are not checked yet: a typed stop, never a guess.
    pub(crate) fn assoc_ty_of_prefix(&mut self, at: At, cause: Cause, prefix: AssocPrefix, assoc: Segment) -> Result<Option<Ty>, Stop> {
        if assoc.has_args {
            return Err(CheckError::Unported("a value path with generic arguments on an associated type's segment (`T::Assoc::<A>::f`)").into());
        }
        let name = Some(assoc.name);
        let mut cands: Vec<ItemKey, Alloc<'a>> = Vec::new_in(self.alloc);
        // The impl's own trait at the arguments its header writes.
        let mut own: Option<(ItemKey, TyList)> = None;
        let self_ty = match prefix {
            AssocPrefix::Param(i) => {
                self.param_bound_traits(i, &mut cands)?;
                self.mk(TyView::Param(i))?
            }
            AssocPrefix::ImplSelf => {
                let header = match self.reads.assoc_parent(self.body.owner)? {
                    Some(parent) => self.reads.impl_header(parent.container)?,
                    None => None,
                };
                // `Self` where the body is in no impl: the path names nothing.
                let Some(h) = header else {
                    self.diag(DiagCode::UnresolvedPath, at, About::path(name));
                    return Ok(None);
                };
                let self_ty = self.lower(h.types, h.self_ty, Lower::Body, cause)?;
                let Some(tr) = h.trait_ else {
                    self.diag(DiagCode::AmbiguousAssocTy, at, About::found_maybe_named(self_ty, name));
                    return Ok(None);
                };
                let rest = h.types.list(h.trait_args.list())?;
                let mut args: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(rest.len() + 1, self.alloc);
                args.push(self_ty);
                for &p in rest {
                    args.push(self.lower(h.types, p, Lower::Body, cause)?);
                }
                own = Some((tr, self.mk_list(&args)?));
                cands.push(tr);
                self.elaborate_bound_traits(&mut cands, 0)?;
                self_ty
            }
            AssocPrefix::Other => {
                self.diag(DiagCode::AmbiguousAssocTy, at, About::path(name));
                return Ok(None);
            }
        };
        // The candidates that declare the name: each trait is listed once, so two hits are two
        // declarations (a handful of bounds: the scan is short).
        let mut picked: Option<(ItemKey, u32)> = None;
        let mut count = 0u32;
        for &tr in cands.iter() {
            let Some(def) = self.reads.trait_def(tr)? else {
                self.diag(DiagCode::UnresolvedPath, at, About::found_maybe_named(self_ty, name));
                return Ok(None);
            };
            if def.assoc_types.iter().any(|a| a.name == assoc.name) {
                count += 1;
                if picked.is_none() {
                    picked = Some((tr, def.generics));
                }
            }
        }
        let (tr, generics) = match (count, picked) {
            (1, Some(p)) => p,
            (0, _) | (_, None) => {
                self.diag(DiagCode::NoAssocTy, at, About::found_maybe_named(self_ty, name));
                return Ok(None);
            }
            _ => {
                self.diag(DiagCode::AmbiguousAssocTyInBounds, at, About::found_maybe_named(self_ty, name));
                return Ok(None);
            }
        };
        let trait_args = match own {
            Some((o, args)) if o == tr => args,
            // A bound's trait, or a supertrait of the impl's: its arguments after `Self` are
            // the solver's to find from the bound that holds.
            _ => {
                let rest = self.fresh_generic_args(tr, 1, generics.saturating_sub(1))?;
                let self_list = self.mk_list(&[self_ty])?;
                self.concat(self_list, rest)?
            }
        };
        // A generic associated type named without its own arguments: E0107, reported there.
        if !self.alias_arity(tr, assoc.name, self.ty.list_len(trait_args), cause)? {
            return Ok(None);
        }
        self.register_trait(tr, trait_args, cause)?;
        let projected = self.project(tr, trait_args, assoc.name, cause)?;
        Ok(Some(self.try_structurally_resolve(projected)?))
    }
}
