//! Associated type bounds on a generic associated type written with type or const arguments:
//! `T: Tr<Item<U>: Bound>`, `T: Tr<Item<'a, U>: Bound>`, `T: Tr<Buf<3>: Bound>`.
//!
//! The Reference, "Trait and lifetime bounds" (associated item constraints, RFC 2289): `T:
//! Tr<Assoc: Bounds>` is `T: Tr` and `<T as Tr>::Assoc: Bounds`; "Associated Items",
//! "Associated Types": "Associated types may include generic parameters and where clauses;
//! these are often referred to as generic associated types, or GATs". So `T: Tr<Item<U>:
//! Bound>` is `T: Tr` and `<T as Tr>::Item<U>: Bound`: each bound on the projection whose list
//! is the owner trait's arguments, `Self` first, then the associated type's own type and const
//! arguments in writing order (the solver's form for a generic associated type, gat.rs's THE
//! FORM; a lifetime argument is in no list, as everywhere in a portable type).
//!
//! The where-clause forms (`T::Item<U>: Bound`, `<T as Tr>::Item<'a, U>: Bound`) are types on
//! the left of a where clause and are made by the type builder (types.rs `param_assoc`,
//! `assoc_with_args`); this file is the angle form's half, called by preds.rs
//! `Builder::one_bound` beside [`Builder::gat_bindings`] (gat.rs), which makes the `Name<T> = X`
//! form the same way. A bound with lifetime arguments only (`Searcher<'a>: Clone`) is not made
//! here: `const_args_in_bounds::angle_args` hands it back as a plain `Name: Bounds`.
//!
//! A const argument (`Buf<3>`, `Buf<{ N }>`, or `Buf<N>` with `N` a const parameter in scope)
//! is made by the one constructor of every const argument ([`Builder::const_term`]); its type
//! is the associated type's declared const parameter's ([`Heads::assoc_const_params`], filed
//! with the trait's members). A one-name argument that is not a const parameter in scope is a
//! type (the Reference, "Generic parameters": "When there is ambiguity if a generic argument
//! could be resolved as either a type or const argument, it is always resolved as a type").

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf};
use blaze_surface::Range;
use blaze_typeck::{List, Name, PNode, PTy, PortableTys};

use super::preds::{SegArgs, lifetime_args};
use super::types::{Builder, Env, Heads, Why, intern, intern_list};

// `'s` is the names source's.
impl<'b, 's, H: Heads, S: super::source::NamesSource<'s>> Builder<'b, H, S> {
    /// The associated type bounds with type or const arguments that the last segment `seg` of
    /// a bound's path writes (`Tr<Item<U>: Bound>`), the bound being `args[0]: key<args[1..]>`:
    /// for each, the projection `<args[0] as Owner<..>>::Item<U>`, `Owner` the trait among
    /// `key` and its supertraits that declares `Item` ([`Builder::assoc_owner`], as for `Name:
    /// Bounds`), and each of its bounds' rows given to `out` with that projection as its self
    /// type (preds.rs `Builder::bound` makes them, nested ones too, by its work list). The
    /// written count is checked against the declaration ([`Heads::assoc_generics`]; a count
    /// not on record is refused by name, never taken as written).
    ///
    /// Each own argument in writing order: a lifetime erased; a type made as a type; a const
    /// argument, or a one-name argument naming a const parameter in scope, made as a constant
    /// of the type the associated type declares for that position ([`Builder::gat_const_want`]).
    #[expect(clippy::too_many_arguments, reason = "the bound's trait reference, where it is written, and the out list")]
    pub(super) fn gat_bounds(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, seg: &lf::Segment, key: LibKey, args: List, r: Range, shown: &str, out: &mut dyn FnMut(lf::Bound, PTy)) -> Result<(), Why> {
        if seg.return_notation || seg.parenthesized || !seg.has_args {
            return Ok(());
        }
        for &n in v.rows(seg.args)? {
            let lf::GenericArg::AssocBound { name, args: own_rows, bounds } = lf::GenericArg::decode(n)? else { continue };
            if lifetime_args(v, own_rows)? {
                continue;
            }
            let name = Name(v.name(name)?);
            let Some((owner, oargs)) = self.assoc_owner(tys, key, args, name, r, shown)? else {
                return Err(Why::AssocNotFound(shown.into(), r));
            };
            let Some(declared) = self.heads.assoc_generics(owner, name) else {
                return Err(Why::Unsupported("an associated type bound with type or const arguments on an associated type whose own parameters are not on record", r));
            };
            // The owner's arguments first, then the associated type's own, in writing order.
            let (mut whole, _, _) = <SegArgs as Default>::default();
            let of_trait = tys.view().list(oargs).map_err(|_| Why::Program("a bound's trait arguments are past the type table".into(), r))?;
            whole.extend_from_slice(of_trait);
            let first_own = whole.len();
            for &a in v.rows(own_rows)? {
                // The argument's position among the associated type's own type and const
                // parameters (a lifetime is not counted, as `Heads::assoc_generics` does not).
                let index = (whole.len() - first_own) as u32;
                let want = |_: &mut PortableTys<'_>| self.gat_const_want(owner, name, index, r, shown);
                let made = match lf::GenericArg::decode(a)? {
                    lf::GenericArg::Lifetime(_) => continue,
                    lf::GenericArg::Type(t) => {
                        let at = v.type_range(t)?;
                        let text = v.text_at(at)?.trim();
                        let const_param = env.params.iter().find(|(p, _)| *p == Name::of(text)).is_some_and(|&(_, i)| env.consts.iter().any(|(c, _)| *c == i));
                        if const_param { self.const_term(tys, env, text, v.site(at)?, &want)? } else { self.ty(tys, env, v, t)? }
                    }
                    lf::GenericArg::Const(c) => {
                        let at = v.const_range(c)?;
                        self.const_term(tys, env, v.text_at(at)?, v.site(at)?, &want)?
                    }
                    lf::GenericArg::AssocType { .. } | lf::GenericArg::AssocConst { .. } | lf::GenericArg::AssocBound { .. } => {
                        return Err(Why::Program(format!("`{shown}`: a binding or a bound is written among the arguments of an associated type bound").into_boxed_str(), r));
                    }
                };
                whole.push(made);
            }
            let own = whole.len() - first_own;
            if declared as usize != own {
                return Err(Why::Program(
                    format!("`{shown}`: an associated type bound is written with {own} type or const arguments on an associated type that declares {declared}").into_boxed_str(),
                    r,
                ));
            }
            let whole = intern_list(tys, &whole)?;
            let proj = intern(tys, PNode::Alias { trait_: self.heads.checker(owner), name, args: whole })?;
            for &row in v.rows(bounds)? {
                out(lf::Bound::decode(row)?, proj);
            }
        }
        Ok(())
    }

    /// The declared type of the const parameter at own position `index` of the associated type
    /// `name` of `owner` ([`Heads::assoc_const_params`]). Not on record while the trait's
    /// members are not made: their need. Not on record after they are (its head was refused, or
    /// the bound is inside the trait's own members): refused by name, never guessed.
    fn gat_const_want(&self, owner: LibKey, name: Name, index: u32, r: Range, shown: &str) -> Result<PTy, Why> {
        match self.heads.assoc_const_params(owner, name) {
            Some(consts) => match consts.iter().find(|(i, _)| *i == index) {
                Some(&(_, t)) => Ok(t),
                None => Err(Why::Program(
                    format!("`{shown}`: argument {} of an associated type bound is a constant; that parameter of the associated type is not a const parameter", index + 1).into_boxed_str(),
                    r,
                )),
            },
            None if self.heads.trait_supers(owner).is_none() => Err(Why::Need(owner)),
            None => Err(Why::Unsupported(
                "a constant argument of a generic associated type whose const parameters are not on record (its head is refused, or the bound is inside its own trait's members)",
                r,
            )),
        }
    }
}
