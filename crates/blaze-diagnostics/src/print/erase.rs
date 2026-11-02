//! rustc's `ClosureEraser`, the fold `ty_to_string` gives a `let`'s type before printing it in
//! "consider giving `x` an explicit type" (`compiler/rustc_trait_selection/src/error_reporting/
//! infer/need_type_info.rs`, MIT or Apache-2.0).
//!
//! rustc folds the type with its inference variables still in it. After writeback an undecided
//! variable is an error, so "holds a variable" is "references an error" here, a region's
//! included. The printed type comes as two of the same shape: `undecided`, whose errors decide
//! what is erased, and `named`, the same type with the variables made for parameters named by
//! them (rustc's `ty_infer_name_resolver`), which is what is kept.

use base_db::Db;
use hir_def::{GenericDefId, hir::generics::GenericParamDataRef};
use hir_ty::next_solver::{
    Const, ConstKind, DbInterner, ErrorGuaranteed, GenericArg, GenericArgKind, GenericArgs, Region, Ty, TyKind,
};
use rustc_type_ir::{
    TypeFolder, TypeSuperFoldable as _, TypeSuperVisitable as _, TypeVisitable as _, TypeVisitableExt as _,
    TypeVisitor,
    inherent::{IntoKind as _, Ty as _},
};
use smallvec::SmallVec;

/// rustc's `ClosureEraser` of `named`, decided by `undecided`: a closure becomes its `fn`
/// pointer; an ADT that holds no variable becomes `_`; in one that does, an argument that holds
/// one is folded, one whose parameter has a default is kept and any other type is `_`; any other
/// type that holds a variable, and the whole type, is folded; the rest is `_`. Lifetimes and
/// constants are kept.
pub(super) fn erase<'t>(db: &'t Db, interner: DbInterner<'t>, undecided: Ty<'t>, named: Ty<'t>) -> Ty<'t> {
    Eraser { db, interner }.ty(1, undecided, named)
}

/// What rustc's `where_x_is_kind` counts: the undecided types and constants of `ty` erased as
/// [`erase`] erases it, 0 when the whole type is.
pub(super) fn undecided_count<'t>(db: &'t Db, interner: DbInterner<'t>, ty: Ty<'t>) -> usize {
    let erased = erase(db, interner, ty, ty);
    if erased.is_ty_error() {
        return 0;
    }
    let mut count = Count(0);
    erased.visit_with(&mut count);
    count.0
}

struct Eraser<'t> {
    db: &'t Db,
    interner: DbInterner<'t>,
}

impl<'t> Eraser<'t> {
    /// `fold_ty` of a type at `depth`, the whole type at 1.
    fn ty(&self, depth: usize, undecided: Ty<'t>, named: Ty<'t>) -> Ty<'t> {
        match (undecided.kind(), named.kind()) {
            (_, TyKind::Closure(..)) => {
                let sig = named.callable_sig(self.interner).expect("a closure has a signature");
                Ty::new_fn_ptr(self.interner, sig)
            }
            (TyKind::Adt(_, args), _) if !args.references_error() => self.unknown(),
            (TyKind::Adt(def, args), TyKind::Adt(_, named_args)) => {
                let generics = hir_ty::generics::generics(self.db, GenericDefId::AdtId(def.def_id()));
                let args = generics.iter_self().zip(args.iter().zip(named_args.iter())).map(|((_, param), (arg, named))| {
                    let has_default = match param {
                        GenericParamDataRef::TypeParamData(it) => it.default.is_some(),
                        GenericParamDataRef::ConstParamData(it) => it.default.is_some(),
                        GenericParamDataRef::LifetimeParamData(_) => false,
                    };
                    if arg.references_error() {
                        self.arg(depth, arg, named)
                    } else if has_default {
                        named
                    } else if let GenericArgKind::Type(_) = named.kind() {
                        self.unknown().into()
                    } else {
                        named
                    }
                });
                Ty::new(self.interner, TyKind::Adt(def, GenericArgs::new_from_iter(args)))
            }
            _ if undecided.references_error() || depth == 1 => {
                let mut children = Children(SmallVec::new());
                undecided.super_visit_with(&mut children);
                named.super_fold_with(&mut Fold { eraser: self, depth, undecided: children.0.into_iter() })
            }
            _ => self.unknown(),
        }
    }

    /// `fold_with` of an argument: a type one deeper; a lifetime or constant is kept, as rustc's
    /// `fold_region` and `fold_const` keep them.
    fn arg(&self, depth: usize, undecided: GenericArg<'t>, named: GenericArg<'t>) -> GenericArg<'t> {
        match (undecided.kind(), named.kind()) {
            (GenericArgKind::Type(undecided), GenericArgKind::Type(named)) => self.ty(depth + 1, undecided, named).into(),
            _ => named,
        }
    }

    /// rustc's fresh variable, printed `_`.
    fn unknown(&self) -> Ty<'t> {
        Ty::new_error(self.interner, ErrorGuaranteed)
    }
}

/// The children `super_fold_with` reaches of a type, in its order: `undecided`'s, beside the
/// `named` type's being folded.
struct Fold<'e, 't> {
    eraser: &'e Eraser<'t>,
    depth: usize,
    undecided: smallvec::IntoIter<[GenericArg<'t>; 4]>,
}

impl<'t> Fold<'_, 't> {
    fn next(&mut self) -> GenericArg<'t> {
        self.undecided.next().expect("a named type has the shape of its undecided one")
    }
}

impl<'t> TypeFolder<DbInterner<'t>> for Fold<'_, 't> {
    fn cx(&self) -> DbInterner<'t> {
        self.eraser.interner
    }

    fn fold_ty(&mut self, named: Ty<'t>) -> Ty<'t> {
        let undecided = self.next().ty().expect("a named type has the shape of its undecided one");
        self.eraser.ty(self.depth + 1, undecided, named)
    }

    fn fold_region(&mut self, r: Region<'t>) -> Region<'t> {
        self.next();
        r
    }

    fn fold_const(&mut self, c: Const<'t>) -> Const<'t> {
        self.next();
        c
    }
}

/// A type's children, in the order `super_visit_with` and `super_fold_with` reach them.
struct Children<'t>(SmallVec<[GenericArg<'t>; 4]>);

impl<'t> TypeVisitor<DbInterner<'t>> for Children<'t> {
    type Result = ();

    fn visit_ty(&mut self, t: Ty<'t>) {
        self.0.push(t.into());
    }

    fn visit_region(&mut self, r: Region<'t>) {
        self.0.push(r.into());
    }

    fn visit_const(&mut self, c: Const<'t>) {
        self.0.push(c.into());
    }
}

/// rustc's `walk().filter_map(TyOrConstInferVar::maybe_from_generic_arg).count()`, of errors.
struct Count(usize);

impl<'t> TypeVisitor<DbInterner<'t>> for Count {
    type Result = ();

    fn visit_ty(&mut self, t: Ty<'t>) {
        match t.kind() {
            TyKind::Error(_) => self.0 += 1,
            _ if t.references_error() => t.super_visit_with(self),
            _ => {}
        }
    }

    fn visit_const(&mut self, c: Const<'t>) {
        match c.kind() {
            ConstKind::Error(_) => self.0 += 1,
            _ if c.references_error() => c.super_visit_with(self),
            _ => {}
        }
    }
}
