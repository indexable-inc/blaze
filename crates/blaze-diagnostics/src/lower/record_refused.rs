//! A record type (`{ a: T, b: U }`, blaze's `#![blaze::records]`) hir-ty refuses: one written
//! in a crate that does not turn the syntax on, one core gives no type to, a field named twice,
//! or a field's default where no default is read. Each names the record or the field and the
//! rule that refuses it. blaze's own; rustc has no records.

use hir::RecordRefusal;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::RecordRefused) -> Vec<Found> {
    let ty = cx.node_view(d.ty);
    let text = ty.value.syntax().text().to_string(); // alloc: the type's text, for one error
    // The field a refusal is about, by its place among the fields written: its name's text
    // and range, or the whole record's when the tree has no such field.
    let field = |at: u32| {
        let fields = match ty.value {
            ast::Type::RecordType(record) => record.record_field_list(),
            _ => None,
        };
        let name = fields
            .and_then(|list| list.fields().nth(at as usize))
            .and_then(|field| field.name());
        match name {
            // alloc: the field's name, for one error
            Some(name) => (name.syntax().text().to_string(), name.syntax().text_range()),
            None => (text.clone(), ty.value.syntax().text_range()), // clone: one error's text
        }
    };
    let whole = ty.value.syntax().text_range();
    let (message, help, range) = match d.reason {
        RecordRefusal::NotEnabled => (
            format!("the record type `{text}` is written in a crate without records"),
            "write `#![blaze::records]` among the inner attributes that open the crate root, \
             outside `cfg_attr`",
            whole,
        ),
        RecordRefusal::NoLangItem => (
            format!("the record type `{text}` has no type: core has no `Record`"),
            "build against blaze's core, which declares `#[lang = \"record\"]`",
            whole,
        ),
        RecordRefusal::DuplicateField(at) => {
            let (name, range) = field(at);
            (
                format!("the record type `{text}` names the field `{name}` twice"),
                "a record is its set of fields: a name is one field",
                range,
            )
        }
        RecordRefusal::DefaultOutsideParameter(at) => {
            let (name, range) = field(at);
            (
                format!(
                    "the field `{name}` of the record type `{text}` has a default, and the \
                     record is not the type of a function's parameter"
                ),
                "a default is what a call takes for a field it leaves out (`f({ a: 1, .. })`), \
                 so only a function parameter's record has them; remove the `= ..`",
                range,
            )
        }
        RecordRefusal::DefaultInTraitImpl(at) => {
            let (name, range) = field(at);
            (
                format!(
                    "the field `{name}` of the record type `{text}` has a default in a method \
                     of a trait impl"
                ),
                "a trait method's defaults are its declaration's, which a call on any \
                 implementor reads; write the default in the trait and remove this `= ..`",
                range,
            )
        }
        RecordRefusal::ExternAbi => (
            format!(
                "the record type `{text}` is the type of a parameter of a function whose ABI \
                 is not Rust's"
            ),
            "no ABI but Rust's says how a record is passed; name the type with a \
             `#[repr(C)]` struct",
            whole,
        ),
    };
    let mut diag = Diag::error(None, message, cx.span(ty.with_value(range)));
    diag.help(help);
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
