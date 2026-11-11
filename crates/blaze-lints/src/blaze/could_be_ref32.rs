//! `could_be_ref32` (in `blaze::edition`, allowed by default): a field of a struct stored
//! many times that holds a `#[blaze::program_lifetime]` type as `&'static T`, `Arc<T>`,
//! `Rc<T>` or `Box<T>`, itself or as the element of the field's collection or `Option`
//! (docs/v2/plan/design/facts.md "The two lints"). A `blaze_mem::Ref32<T>` is 4 bytes where
//! each of those is 8, `Option<Ref32<T>>` is 4 too, and copying one writes no count.
//!
//! Found: a field of a struct of the crate, outside test code and macro output, whose type
//! (or one type argument of it: `Vec<Arc<T>>`, `HashMap<K, &'static T>`, `Option<Box<T>>`)
//! is one of the four pointers to an ADT declared `#[blaze::program_lifetime]`, where the
//! struct is stored many times: it is the element of a collection somewhere in the crate
//! (`packable`'s `packable_uses`), or is program-lifetime itself (a node of a tree is).
//! A local, a parameter and a return are not fields and are never looked at: a pointer that
//! lives on the stack saves nothing by being narrow.
//!
//! The fix is machine-applicable in one case: the field is a plain `&'static T` (not inside
//! a collection), no other crate can name it, nothing assigns to it after the struct is
//! built (`frozen_collection`'s `mutated_fields`), no pattern of the crate takes the struct
//! apart, and every record literal of the struct writes the field as `r.get()` of a
//! `Ref32<T>`. Then the field becomes `blaze_mem::Ref32<T>`, each literal writes `r`, and
//! each read `s.f` becomes `s.f.get()`: the same `&'static T` everywhere it was read.
//! Anything else gets the finding without a fix.

use base_db::Db;
use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::{
    AdtId, FieldId, StructId, VariantId,
    expr_store::Body,
    hir::{Expr, Pat},
};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::{Lint, frozen_collection, general_facts, packable};
use crate::{check::Cx, diag, diag::Applicability};

const LINT: Lint = Lint::CouldBeRef32;

/// How a field holds the program-lifetime type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Static,
    Arc,
    Rc,
    Box,
}

impl Held {
    fn said(self) -> &'static str {
        match self {
            Held::Static => "a `&'static` reference",
            Held::Arc => "an `Arc`, with two counts beside the value and a write for every clone",
            Held::Rc => "an `Rc`, with two counts beside the value and a write for every clone",
            Held::Box => "a `Box`",
        }
    }
}

/// The program-lifetime ADT `ty` points to and how, if it is one of the four pointers.
/// `statics`: the field's written type names `'static`, which a reference in it then has.
fn pointer(db: &Db, ty: &hir::Type<'_>, statics: bool) -> Option<(hir::Adt, Held)> {
    let declared = |adt: hir::Adt| {
        hir_ty_pure::program_lifetime::declared(db, AdtId::from(adt)).then_some(adt)
    };
    let Some(adt) = ty.as_adt() else {
        // A reference: no ADT itself, one under it.
        let under = ty.strip_references().as_adt()?;
        return statics.then(|| declared(under)).flatten().map(|it| (it, Held::Static));
    };
    let held = if general_facts::std_adt(db, adt, "sync", &["Arc"]) {
        Held::Arc
    } else if general_facts::std_adt(db, adt, "rc", &["Rc"]) {
        Held::Rc
    } else if general_facts::std_adt(db, adt, "boxed", &["Box"]) {
        Held::Box
    } else {
        return None;
    };
    let pointee = ty.type_arguments().next()?.as_adt()?;
    declared(pointee).map(|it| (it, held))
}

/// The edits of the fix for the plain `&'static T` field `field` of `def`, written `written`
/// at `ty_range`, or `None` where the module docs say there is no fix.
fn fix(
    cx: &Cx<'_, '_>,
    def: hir::Struct,
    field: hir::Field,
    written: &str,
    ty_range: TextRange,
) -> Option<Vec<(diag::Span, String)>> {
    let db = cx.db();
    let krate = cx.krate().base();
    let id = FieldId::from(field);
    let pointee = written.strip_prefix("&'static ")?.trim();
    if matches!(field.visibility(db), hir::Visibility::Public)
        || frozen_collection::mutated_fields(db, krate).contains(id)
    {
        return None;
    }
    let variant = VariantId::StructId(StructId::from(def));
    // alloc: the fix's edits, one per write and read of the field
    let mut edits = vec![(cx.span(ty_range), format!("blaze_mem::Ref32<{pointee}>"))];
    let mut written_once = false;
    for body_def in frozen_collection::crate_bodies(db, krate) {
        let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(body_def));
        let (body, infer) = (analysis.body(), analysis.infer());
        // A pattern that takes the struct apart binds the field as what it was.
        if body.pats().any(|(pat, kind)| {
            matches!(kind, Pat::Record { .. } | Pat::TupleStruct { .. })
                && infer.variant_resolution_for_pat(pat) == Some(variant)
        }) {
            return None;
        }
        let mut place = None;
        let mut at = |expr| {
            let (file, offset) = match place {
                Some(it) => it,
                None => {
                    let file = analysis.tree().1.file_id()?.file_id(db);
                    *place.insert((file, Body::anchor_offset(db, body_def)?))
                }
            };
            Some((file, analysis.expr_range(expr)? + offset))
        };
        for (expr, kind) in body.exprs() {
            match kind {
                Expr::RecordLit { fields, .. }
                    if infer.variant_resolution_for_expr(expr) == Some(variant) =>
                {
                    let all = variant.fields(db);
                    let init = fields.iter().find(|it| all.field(&it.name) == Some(id.local_id))?;
                    // `r.get()` of a `Ref32`: the literal keeps `r`.
                    let Expr::MethodCall { receiver, .. } = body.expr(init.expr) else {
                        return None;
                    };
                    let callee = general_facts::callee(db, &analysis, init.expr)?;
                    let of_ref32 = callee.self_adt.is_some_and(|it| {
                        general_facts::crate_adt(db, it, "blaze_mem", &["Ref32"])
                    });
                    if callee.name != "get" || !of_ref32 {
                        return None;
                    }
                    let (file, whole) = at(init.expr)?;
                    let (_, inner) = at(*receiver)?;
                    let text = db.file_text(file).text(db);
                    let kept = text.get(usize::from(inner.start())..usize::from(inner.end()))?;
                    edits.push((diag::of_file(file, whole), kept.to_owned()));
                    written_once = true;
                }
                Expr::Field { .. }
                    if infer.field_resolution(expr) == Some(either::Either::Left(id)) =>
                {
                    let (file, range) = at(expr)?;
                    edits.push((diag::of_file(file, TextRange::empty(range.end())), ".get()".to_owned()));
                }
                _ => {}
            }
        }
    }
    written_once.then_some(edits)
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let krate = cx.krate().base();
    let own = AdtId::StructId(StructId::from(def));
    let many = hir_ty_pure::program_lifetime::declared(db, own)
        || packable::packable_uses(db, krate).of(StructId::from(def)).is_some();
    if !many {
        return;
    }
    let target = cx.krate().to_display_target(db);
    let name = def.name(db);
    for (index, field) in def.fields(db).into_iter().enumerate() {
        let Some(node) = frozen_collection::field_node(item, &field.name(db), index) else {
            continue;
        };
        let Some(range) = cx.range(node) else { continue };
        let statics = cx.snippet(range).contains("&'static ");
        let ty = field.ty(db);
        let direct = pointer(db, &ty, statics);
        let found =
            direct.or_else(|| ty.type_arguments().find_map(|it| pointer(db, &it, statics)));
        let Some((pointee, held)) = found else { continue };
        let pointee = pointee.name(db);
        let message = format!(
            "the field `{}` of `{}`, which is stored many times, holds the program-lifetime `{}` as `{}`: {}; a `Ref32<{}>` is 4 bytes, half of it, and a copy of one writes nothing",
            field.name(db).as_str(),
            name.as_str(),
            pointee.as_str(),
            ty.display(db, target),
            held.said(),
            pointee.as_str(),
        );
        // The fix is for a plain `&'static T` field alone.
        let edits = (direct.is_some() && held == Held::Static)
            .then(|| {
                let ty_node = ast::RecordField::cast(node)?.ty()?.syntax();
                let ty_range = cx.range(ty_node)?;
                fix(cx, def, field, cx.snippet(ty_range), ty_range)
            })
            .flatten();
        let span = cx.span(range);
        let diagnostic = cx.blaze(LINT, node, span, message);
        if let Some(edits) = edits {
            diagnostic.multipart_suggestion(
                "hold it as a `Ref32`: the literals already have one, and each read takes its `&'static`",
                edits,
                Applicability::MachineApplicable,
            );
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}
