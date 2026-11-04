//! `pod_serialized_by_field` (in `blaze::pedantic_perf`, allowed by default): a `#[repr(C)]`
//! struct of plain numbers, with no padding, that implements serde's `Serialize` or
//! `Deserialize`. Its bytes already are a fixed layout: a `bytemuck::Pod` (or
//! `zerocopy::IntoBytes`/`FromBytes`) impl reads and writes a whole slice of them as one copy,
//! where serde visits every field of every value through the format's code. Goose keeps nested
//! data inline and moves it as bytes.
//!
//! Found: a struct of the crate with `#[repr(C)]` whose every field is an integer or a float, or
//! an array of them (no `bool` or `char`, whose bytes are not all valid values), whose layout's
//! size is the sum of its fields' (no padding, so every byte is a field's), and that has an impl
//! of `Serialize` or `Deserialize` of the crate `serde` (or `serde_core`). Not checked: test
//! code and what a macro wrote.
//!
//! Narrowed, and so allowed by default: the lint can't see whether the values are serialized
//! often enough to matter, nor whether the format needs serde's (a text format, a format with
//! a fixed byte order other than the machine's): a byte cast writes the machine's byte order.
//! No fix: the format is the author's choice.

use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Lint, facts};
use crate::check::Cx;

const LINT: Lint = Lint::PodSerializedByField;

/// Whether `ty` is an integer or a float, or an array of them.
fn plain(db: &base_db::Db, ty: &hir::Type<'_>) -> bool {
    if let Some((inner, _)) = ty.as_array(db) {
        return plain(db, &inner);
    }
    ty.is_int_or_uint() || ty.is_float()
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
    if !def.repr(db).is_some_and(|it| it.c()) {
        return;
    }
    let fields = def.fields(db);
    if fields.is_empty() {
        return;
    }
    let mut sum = 0u64;
    for field in &fields {
        let ty = field.ty(db);
        if !plain(db, &ty) {
            return;
        }
        let Ok(layout) = ty.layout(db) else { return };
        sum += layout.size();
    }
    let adt = hir::Adt::Struct(def);
    let Ok(layout) = adt.ty(db).layout(db) else { return };
    if layout.size() != sum {
        return;
    }
    let serde = hir::Impl::all_for_type(db, cx.krate(), adt.ty(db)).into_iter().find_map(|imp| {
        let trait_ = imp.trait_(db)?;
        let krate = trait_.module(db).krate(db);
        let name = trait_.name(db);
        (matches!(name.as_str(), "Serialize" | "Deserialize")
            && (facts::crate_is(db, krate, "serde") || facts::crate_is(db, krate, "serde_core")))
        .then_some(name)
    });
    let Some(trait_name) = serde else { return };
    let name = item.name().map_or(item.syntax(), |it| it.syntax());
    let Some(range) = cx.range(name) else { return };
    let message = format!(
        "`{}` is `#[repr(C)]` plain numbers with no padding, yet goes through serde's `{}` field by field",
        def.name(db).as_str(),
        trait_name.as_str()
    );
    // alloc: a message's part
    let note = "`#[derive(bytemuck::Pod, bytemuck::Zeroable)]` (or zerocopy's `IntoBytes`/`FromBytes`) lets a slice of \
                them be written and read as its bytes in one copy (Goose moves inline data as bytes); that writes the \
                machine's byte order"
        .to_owned();
    let span = cx.span(range);
    cx.blaze(LINT, name, span, message)
        .note(note)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}
