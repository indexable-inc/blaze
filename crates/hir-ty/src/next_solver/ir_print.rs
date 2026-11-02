//! Things related to IR printing in the next-trait-solver.

use base_db::Db;
use hir_def::signatures::{ConstSignature, TraitSignature, TypeAliasSignature};
use rustc_type_ir::{self as ty, AliasTermKind, ir_print::IrPrint};


use super::SolverDefId;
use super::interner::DbInterner;

fn alias_def_id<'db>(kind: AliasTermKind<DbInterner<'db>>) -> SolverDefId<'db> {
    match kind {
        AliasTermKind::ProjectionTy { def_id } => def_id.into(),
        AliasTermKind::InherentTy { def_id } => def_id.into(),
        AliasTermKind::OpaqueTy { def_id } => def_id.into(),
        AliasTermKind::FreeTy { def_id } => def_id.into(),
        AliasTermKind::AnonConst { def_id } => def_id.into(),
        AliasTermKind::ProjectionConst { def_id } => def_id.into(),
        AliasTermKind::FreeConst { def_id } => def_id.into(),
        AliasTermKind::InherentConstSelf { def_id } => def_id.into(),
        AliasTermKind::InherentConstImpl { def_id } => def_id.into(),
    }
}

/// Writes `{what}({name}[{args:?}])`: a type alias or const by its name, anything else (an
/// opaque type, an anonymous constant) by its id.
fn print_alias<'db>(
    db: &Db,
    what: &str,
    def_id: SolverDefId<'db>,
    args: super::GenericArgs<'db>,
    fmt: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    match def_id {
        SolverDefId::TypeAliasId(id) => {
            write!(fmt, "{what}({:?}[{args:?}])", TypeAliasSignature::of(db, id).name.as_str())
        }
        SolverDefId::ConstId(id) => match &ConstSignature::of(db, id).name {
            Some(name) => write!(fmt, "{what}({:?}[{args:?}])", name.as_str()),
            None => write!(fmt, "{what}({id:?}[{args:?}])"),
        },
        id => write!(fmt, "{what}({id:?}[{args:?}])"),
    }
}

impl<'db> IrPrint<ty::AliasTy<Self>> for DbInterner<'db> {
    fn print(t: &ty::AliasTy<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(t: &ty::AliasTy<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        crate::next_solver::interner::fmt_with_attached_db(fmt, "AliasTy", |db, fmt| {
            print_alias(db, "AliasTy", alias_def_id(t.kind.into()), t.args, fmt)
        })
    }
}

impl<'db> IrPrint<ty::AliasTerm<Self>> for DbInterner<'db> {
    fn print(t: &ty::AliasTerm<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(t: &ty::AliasTerm<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        crate::next_solver::interner::fmt_with_attached_db(fmt, "AliasTerm", |db, fmt| {
            print_alias(db, "AliasTerm", alias_def_id(t.kind), t.args, fmt)
        })
    }
}
impl<'db> IrPrint<ty::TraitRef<Self>> for DbInterner<'db> {
    fn print(t: &ty::TraitRef<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(t: &ty::TraitRef<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        crate::next_solver::interner::fmt_with_attached_db(fmt, "TraitRef", |db, fmt| {
            let trait_ = t.def_id.0;
            let self_ty = &t.args.as_slice()[0];
            let trait_args = &t.args.as_slice()[1..];
            if trait_args.is_empty() {
                write!(fmt, "{:?}: {}", self_ty, TraitSignature::of(db, trait_).name.as_str())
            } else {
                write!(
                    fmt,
                    "{:?}: {}<{:?}>",
                    self_ty,
                    TraitSignature::of(db, trait_).name.as_str(),
                    trait_args
                )
            }
        })
    }
}
impl<'db> IrPrint<ty::TraitClause<Self>> for DbInterner<'db> {
    fn print(t: &ty::TraitClause<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(t: &ty::TraitClause<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match t.polarity {
            ty::ClausePolarity::Positive => write!(fmt, "{:?}", t.trait_ref),
            ty::ClausePolarity::Negative => write!(fmt, "!{:?}", t.trait_ref),
        }
    }
}
impl<'db> IrPrint<ty::HostEffectClause<Self>> for DbInterner<'db> {
    fn print(
        t: &ty::HostEffectClause<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::HostEffectClause<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let prefix = match t.constness {
            ty::BoundConstness::Const => "const",
            ty::BoundConstness::Maybe => "[const]",
        };
        write!(fmt, "{prefix} {:?}", t.trait_ref)
    }
}
impl<'db> IrPrint<ty::ExistentialTraitRef<Self>> for DbInterner<'db> {
    fn print(
        t: &ty::ExistentialTraitRef<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::ExistentialTraitRef<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        crate::next_solver::interner::fmt_with_attached_db(fmt, "ExistentialTraitRef", |db, fmt| {
            let trait_ = t.def_id.0;
            write!(
                fmt,
                "ExistentialTraitRef({:?}[{:?}])",
                TraitSignature::of(db, trait_).name.as_str(),
                t.args
            )
        })
    }
}
impl<'db> IrPrint<ty::ExistentialProjection<Self>> for DbInterner<'db> {
    fn print(
        t: &ty::ExistentialProjection<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::ExistentialProjection<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        crate::next_solver::interner::fmt_with_attached_db(fmt, "ExistentialProjection", |db, fmt| {
            fmt.write_str("ExistentialProjection(")?;
            print_alias(db, "", t.def_id.into(), t.args, fmt)?;
            write!(fmt, " -> {:?})", t.term)
        })
    }
}
impl<'db> IrPrint<ty::ProjectionClause<Self>> for DbInterner<'db> {
    fn print(
        t: &ty::ProjectionClause<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::ProjectionClause<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        crate::next_solver::interner::fmt_with_attached_db(fmt, "ProjectionClause", |db, fmt| {
            fmt.write_str("ProjectionClause(")?;
            let def_id = alias_def_id(t.projection_term.kind);
            print_alias(db, "", def_id, t.projection_term.args, fmt)?;
            write!(fmt, " -> {:?})", t.term)
        })
    }
}
impl<'db> IrPrint<ty::NormalizesTo<Self>> for DbInterner<'db> {
    fn print(t: &ty::NormalizesTo<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::NormalizesTo<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(fmt, "NormalizesTo({} -> {:?})", t.alias, t.term)
    }
}
impl<'db> IrPrint<ty::SubtypePredicate<Self>> for DbInterner<'db> {
    fn print(
        t: &ty::SubtypePredicate<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::SubtypePredicate<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(fmt, "{:?} <: {:?}", t.a, t.b)
    }
}
impl<'db> IrPrint<ty::CoercePredicate<Self>> for DbInterner<'db> {
    fn print(t: &ty::CoercePredicate<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &ty::CoercePredicate<Self>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(fmt, "CoercePredicate({:?} -> {:?})", t.a, t.b)
    }
}
impl<'db> IrPrint<ty::FnSig<Self>> for DbInterner<'db> {
    fn print(t: &ty::FnSig<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(t: &ty::FnSig<Self>, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tys = t.inputs_and_output.as_slice();
        let (output, inputs) = tys.split_last().unwrap();
        write!(fmt, "fn({:?}) -> {:?}", inputs, output)
    }
}

impl<'db> IrPrint<rustc_type_ir::PatternKind<DbInterner<'db>>> for DbInterner<'db> {
    fn print(
        t: &rustc_type_ir::PatternKind<DbInterner<'db>>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        Self::print_debug(t, fmt)
    }

    fn print_debug(
        t: &rustc_type_ir::PatternKind<DbInterner<'db>>,
        fmt: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match t {
            ty::PatternKind::Range { start, end } => write!(fmt, "{:?}..={:?}", start, end),
            ty::PatternKind::Or(list) => write!(fmt, "or({:?})", list),
            ty::PatternKind::NotNull => fmt.write_str("!null"),
        }
    }
}
