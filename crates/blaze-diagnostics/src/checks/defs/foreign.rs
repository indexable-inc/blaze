//! E0044: a foreign function with type or const parameters.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/check.rs` (`check_item_type`, its
//! `DefKind::ForeignMod` arm) (MIT or Apache-2.0). A foreign type or static has no generics
//! past AST validation, so only functions are read.

use blaze_items::{Fact, ItemId};
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasGenericParams as _};

use crate::{
    cx::{Cx, def_range_view},
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn check(cx: &Cx<'_, '_>, func: hir::Function) -> Option<Found> {
    func.extern_block(cx.db)?;
    let source = cx.vsema.source(func)?;
    let list = source.value.generic_param_list()?;
    let (mut types, mut consts) = (0usize, 0usize);
    for param in list.type_or_const_params() {
        match param {
            ast::TypeOrConstParam::Type(_) => types += 1,
            ast::TypeOrConstParam::Const(_) => consts += 1,
        }
    }
    let (kinds, kinds_pl, example) = match (types, consts) {
        (0, 0) => return None,
        (_, 0) => ("type", "types", " like `u32`"),
        (0, _) => ("const", "consts", ""),
        _ => ("type or const", "types or consts", ""),
    };
    let eii = source.value.attrs().any(|it| it.simple_name() == Some("rustc_eii_foreign_item"));
    let name = if eii { "externally implementable items" } else { "foreign items" };
    let span = cx.span(source.with_value(def_range_view(source.value.syntax())));
    // alloc: the error's message
    let mut diag =
        Diag::error(Some("E0044"), format!("{name} may not have {kinds} parameters"), span.clone());
    // alloc: the error's label
    diag.span_label(span, format!("can't have {kinds} parameters"));
    // alloc: the error's help
    diag.help(format!("replace the {kinds} parameters with concrete {kinds_pl}{example}"));
    if let Ok(id) = func.try_into() {
        // alloc: the error's fact
        let what = format!("the signature of `{}`", cx.def_path(hir::ModuleDef::Function(func)));
        diag.fact(Fact::Signature(cx.key(ItemId::Function(id))), what);
    }
    Some(Found::new(diag, Phase::Items))
}
