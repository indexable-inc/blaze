//! Why a const argument has no value tree, for the error rustc's `eval_to_valtree` reports
//! where the constant is used. The solver (`evaluate_const`) turns such a constant into an
//! error constant and cannot report; this asks the same question once per anonymous
//! constant, with the arguments the solver evaluates it with.

use base_db::Db;
use hir_def::HasModule;
use rustc_type_ir::{TypeVisitableExt as _, inherent::GenericArgs as _};

use crate::{
    ParamEnvAndCrate,
    db::{AnonConstId, HirDatabase},
    next_solver::{Const, ConstExt as _, DbInterner, GenericArgs, StoredTy, ValTreeCreationError},
};

/// How many bytes of a string constant (a `&'static str` const argument) a diagnostic or a
/// debug name shows; past it, the rest is cut and its length said. `type_name`, which a
/// program can read, and source code always show all of it.
pub const STR_SHOWN: usize = 64;

/// What a diagnostic shows of the string constant `s`: all of it when it is at most
/// [`STR_SHOWN`] bytes, else its longest prefix of at most that many bytes that ends on a
/// `char` boundary, and `true` for cut.
pub fn shown_str(s: &str) -> (&str, bool) {
    if s.len() <= STR_SHOWN {
        return (s, false);
    }
    let mut end = STR_SHOWN;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// [`ValTreeCreationError`] with its types stored, but for the error already reported.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValTreeError {
    /// rustc's "maximum number of nodes exceeded in constant {path}".
    NodesOverflow,
    /// A value of this type, met inside the constant, has no value tree.
    NonSupportedType(StoredTy),
    /// The evaluated bytes are not a value of `ty`: `what`. No rustc counterpart: the
    /// evaluation and the reader of its bytes disagree.
    Unreadable { ty: StoredTy, what: &'static str },
}

/// The value-tree error of `def`'s value, `None` when it has a value tree, when its
/// evaluation fails (an error of its own), and when it depends on its owner's generic
/// parameters, as the solver only evaluates such a constant at a use with concrete arguments.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn anon_const_valtree_error<'db>(db: &'db Db, def: AnonConstId<'db>) -> Option<ValTreeError> {
    let loc = def.loc(db);
    let owner = loc.owner.generic_def(db);
    let krate = def.module(db).krate(db);
    let interner = DbInterner::new_with(db, krate);
    let args = GenericArgs::identity_for_item(interner, owner.into());
    // A constant that depends on its owner's parameters has a value only at a use with
    // concrete arguments, as one that may not name them at all.
    if (!loc.allow_using_generic_params || crate::consteval::anon_const_is_dependent(db, def))
        && (args.has_non_region_param() || args.has_non_region_placeholders())
    {
        return None;
    }
    let allocation = db.anon_const_eval(def, args, None).ok()?;
    let env = ParamEnvAndCrate { param_env: db.trait_environment(owner), krate };
    match Const::new_from_allocation(interner, &allocation, env) {
        Ok(_) | Err(ValTreeCreationError::ErrorGuaranteed(_)) => None,
        Err(ValTreeCreationError::NodesOverflow) => Some(ValTreeError::NodesOverflow),
        Err(ValTreeCreationError::NonSupportedType(ty)) => {
            Some(ValTreeError::NonSupportedType(ty.store()))
        }
        Err(ValTreeCreationError::Unreadable { ty, what }) => {
            Some(ValTreeError::Unreadable { ty: ty.store(), what })
        }
    }
}
