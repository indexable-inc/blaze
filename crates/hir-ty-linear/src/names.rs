//! How an error names a value, its type and where it leaves: made only when one is reported.

use base_db::Db;
use hir_def::{
    HasModule, VariantId,
    expr_store::ExpressionStore,
    hir::{BindingId, ExprId},
    signatures::VariantFields,
};
use hir_ty::{
    display::{DisplayTarget, HirDisplay},
    next_solver::{Ty, TyKind},
};
use hir_ty_mir::mir::{LocalId, MirBody, PlaceElem, PlaceTy, ProjectionElem};
use rustc_type_ir::inherent::IntoKind;

/// The expressions of the item `body` is a body of.
pub(crate) fn store<'db>(db: &'db Db, body: &MirBody<'db>) -> &'db ExpressionStore {
    ExpressionStore::of(db, body.owner.expression_store_owner(db))
}

/// The variable `local` is, if it is one.
pub(crate) fn binding_of(body: &MirBody<'_>, local: LocalId) -> Option<BindingId> {
    body.binding_locals.iter().find(|&(_, &it)| it == local).map(|(binding, _)| binding)
}

/// A variable's name.
pub(crate) fn binding(db: &Db, body: &MirBody<'_>, binding: BindingId) -> String {
    store(db, body)[binding].name.as_str().to_owned() // alloc: error path
}

/// `ty` as an error shows it.
pub(crate) fn ty<'db>(db: &'db Db, body: &MirBody<'db>, ty: Ty<'db>) -> String {
    ty_in(db, body.owner.krate(db), ty)
}

/// `ty` as an error in `krate` shows it.
pub(crate) fn ty_in<'db>(db: &'db Db, krate: base_db::Crate, ty: Ty<'db>) -> String {
    ty.display(db, DisplayTarget::from_crate(db, krate)).to_string()
}

/// The place `local` with `elems` as an error names it: the variable or what it is, then each
/// field by name, each variant as `(as Variant)`, each element at its index.
pub(crate) fn place<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    local: LocalId,
    elems: &[PlaceElem],
) -> String {
    let mut out = match binding_of(body, local) {
        Some(it) => binding(db, body, it),
        None if local == hir_ty_mir::mir::return_slot() => "the return value".to_owned(), // alloc: error path
        None => match body.param_locals.iter().position(|&it| it == local) {
            Some(at) => format!("parameter {}", at + 1), // alloc: error path
            None => "a temporary".to_owned(),            // alloc: error path
        },
    };
    let interner = hir_ty::next_solver::DbInterner::new_with(db, body.owner.krate(db));
    let infcx = {
        use hir_ty::next_solver::{TypingMode, infer::DbInternerInferExt};
        interner.infer_ctxt().build(TypingMode::non_body_analysis())
    };
    let env = {
        use hir_ty::db::HirDatabase;
        db.trait_environment(body.owner.generic_def(db))
    };
    let env = hir_ty::comptime_env::owner_env(db, body.owner, &infcx, env);
    let mut at = PlaceTy::from_ty(body.locals[local].ty.as_ref());
    for elem in elems {
        match *elem {
            ProjectionElem::Deref => out.insert(0, '*'),
            ProjectionElem::Field(index) => {
                let named = match (at.variant_id, at.ty.kind()) {
                    (Some(variant), _) => field_name(db, variant, index.0),
                    (None, TyKind::Adt(def, _)) => match def.def_id() {
                        hir_def::AdtId::StructId(id) => field_name(db, id.into(), index.0),
                        _ => None,
                    },
                    _ => None,
                };
                match named {
                    Some(name) => {
                        out.push('.');
                        out.push_str(&name);
                    }
                    None => {
                        out.push('.');
                        out.push_str(&index.0.to_string());
                    }
                }
            }
            ProjectionElem::Downcast(variant) => {
                let name = match variant {
                    VariantId::EnumVariantId(it) => {
                        let loc = it.loc(db);
                        loc.parent.enum_variants(db).variant_name_by_id(it)
                    }
                    _ => None,
                };
                if let Some(name) = name {
                    out = format!("({out} as {})", name.as_str()); // alloc: error path
                }
            }
            ProjectionElem::ConstantIndex { offset, from_end: false, .. } => {
                out.push_str(&format!("[{offset}]"));
            }
            ProjectionElem::ConstantIndex { offset, from_end: true, .. } => {
                out.push_str(&format!("[len - {offset}]"));
            }
            ProjectionElem::Index(_) => out.push_str("[_]"),
            // The field of the `for const` loop's current iteration.
            ProjectionElem::ReflectedField(_) => out.push_str(".[_]"),
            ProjectionElem::Subslice { from, to } => out.push_str(&format!("[{from}..{to}]")),
        }
        at = at.projection_ty(&infcx, elem, env);
    }
    out
}

fn field_name(db: &Db, variant: VariantId, index: u32) -> Option<String> {
    let fields = VariantFields::of(db, variant).fields();
    let (_, data) = fields.iter().nth(index as usize)?;
    let name = data.name.as_str();
    // A tuple field's name is its index.
    Some(name.to_owned()) // alloc: error path
}

/// Whether the `return` `expr` is the one a `?` lowers to.
pub(crate) fn is_try<'db>(db: &'db Db, body: &MirBody<'db>, expr: ExprId) -> bool {
    let (_, source_map) =
        ExpressionStore::with_source_map(db, body.owner.expression_store_owner(db));
    source_map
        .expr_syntax(expr)
        .is_ok_and(|it| it.value.syntax_node_ptr().kind() == syntax::SyntaxKind::TRY_EXPR)
}
