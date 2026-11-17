//! The trait's arguments in a qualified value path `<T as Tr>::x` whose trait path writes
//! none: each parameter after `Self` takes its declared default, with `Self` and the
//! arguments before it substituted; only a parameter with no default is inferred.
//!
//! The trait in `<T as Tr>` is a path in type position, and a type path that leaves out a
//! defaulted parameter takes the default (the Reference, "Generic parameters": a type
//! parameter "may have a default"; "Paths", "Qualified paths": `<S as T1>::f` names `T1`'s
//! item for `S`). A plain value path `Tr::x` is not this: its trait's arguments are inferred
//! (rules/lit_path.rs `value_def_ty`, `Def::Fn`), as are an operator's (rules/op.rs).
//!
//! The pinned library relies on it: std/src/path.rs `impl_cmp_os_str!` writes, for
//! `impl PartialEq<&Path> for OsStr`, `fn eq(&self, other: &&Path) -> bool {
//! <Path as PartialEq>::eq(self.as_ref(), other) }`. With `Rhs = Self` taken that is
//! `PartialEq<Path>::eq(&Path, &Path)` and `other` coerces by deref; with `Rhs` inferred it
//! becomes `&Path` from `other`'s type and `Path: PartialEq<&Path>` has no impl (tempo's build
//! census 5: 34 bodies "can't compare `Path` with `{Rhs}`", and their `OsStr` and `str`
//! siblings).
//!
//! One home for the defaults of a trait named in a body (rule 30); the defaults of a type path
//! lowered from text are blaze-ladder front2/types.rs `with_defaults`, the same rule over the
//! same `TraitHead::defaults` the world hands as `TraitDef::defaults`.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Malformed, Stop};
use crate::ids::ItemKey;
use crate::machine::{Lower, Machine};
use crate::ty::{Cause, Ty, TyList, TypeCtx, VarKind};
use crate::world::{TraitDef, TraitDefaults, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// `[self_ty, A1..An]` for the trait `tr` (head `def`) of a qualified value path whose
    /// trait path writes no arguments: `Ai` is parameter `i`'s default lowered at the
    /// arguments before it (a default names only `Self` and earlier parameters: the language
    /// refuses a forward one, E0128), else a fresh variable (a const variable for a const
    /// parameter). A trait whose defaults the world did not read is refused by name
    /// ([`CheckError::TraitDefaultsNotKnown`]), never inferred: inferring is the wrong answer
    /// above, not a weaker one.
    pub(crate) fn qpath_trait_args(&mut self, tr: ItemKey, def: &TraitDef<'_>, self_ty: Ty, cause: Cause) -> Result<TyList, Stop> {
        let after_self = def.generics.saturating_sub(1);
        if after_self == 0 {
            return self.mk_list(&[self_ty]);
        }
        let defaults = match def.defaults {
            TraitDefaults::Known(d) => d,
            TraitDefaults::NotKnown => return Err(CheckError::TraitDefaultsNotKnown { trait_: tr }.into()),
        };
        if defaults.len() != after_self as usize {
            let len = u32::try_from(defaults.len()).unwrap_or(u32::MAX);
            return Err(CheckError::Malformed(Malformed::List { column: "trait defaults", start: 0, len }).into());
        }
        let consts = self.reads.const_params(tr)?;
        let mut args: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(def.generics as usize, self.alloc);
        args.push(self_ty);
        for (at, default) in defaults.iter().enumerate() {
            let i = at as u32 + 1;
            let t = match *default {
                Some(d) => {
                    let before = self.mk_list(&args)?;
                    self.lower(def.types, d, Lower::Args(before), cause)?
                }
                None => match consts.type_of(i) {
                    Some(_) => self.ty.fresh(VarKind::Const)?,
                    None => self.fresh()?,
                },
            };
            args.push(t);
        }
        self.mk_list(&args)
    }
}
