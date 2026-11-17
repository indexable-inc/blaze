//! A record value (`{ a: 1, .. }`) or record pattern (`{ a, .. }`, blaze's
//! `#![blaze::records]`) the type checker refuses: in a crate that does not turn records on,
//! with no record type known for it, naming a field the record lacks or naming one twice,
//! leaving a field out, or taking a default where there is none. Each names the field or the
//! function and the rule that refuses it. blaze's own; rustc has no records.

use hir::RecordProblem;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::RecordUseRefused) -> Vec<Found> {
    let span = cx.span_of_ptr(d.node);
    let (message, help) = match &d.problem {
        RecordProblem::NotEnabled => (
            "a record is written in a crate without records".to_owned(), // alloc: one error
            "write `#![blaze::records]` among the inner attributes that open the crate root, \
             outside `cfg_attr`",
        ),
        RecordProblem::TypeUnknown => (
            "the type of this record is not known where `..` is written".to_owned(), // alloc: one error
            "a record's type comes from a literal that writes every field or from an \
             annotation, never from how its fields are used: annotate it, or write every field",
        ),
        RecordProblem::NotARecord => (
            "a record is written where the type expected is not a record".to_owned(), // alloc: one error
            "`{ a: x, .. }` and the record pattern `{ a, .. }` are of a record type \
             `{ a: T, .. }`; a struct is written with its name",
        ),
        RecordProblem::NoSuchField(field) => (
            format!("the record has no field `{}`", field.display(cx.db, cx.edition)),
            "a record's fields are those of its type, whatever order they are written in",
        ),
        RecordProblem::DuplicateField(field) => (
            format!("the field `{}` is written twice", field.display(cx.db, cx.edition)),
            "a record is its set of fields: a name is one field",
        ),
        RecordProblem::MissingField(field) => (
            format!("the field `{}` is not written", field.display(cx.db, cx.edition)),
            "without `..` a record value or pattern names every field; a pattern ends with \
             `..` to skip the rest, and an argument with `..` to take the callee's defaults",
        ),
        RecordProblem::MissingDefault { field, .. } => {
            let callee = match &d.callee {
                // alloc: the function's name, for one error
                Some(callee) => callee.display(cx.db, cx.edition).to_string(),
                None => "the function called".to_owned(), // alloc: one error
            };
            (
                format!(
                    "the field `{}` is left out and `{callee}` declares no default for it",
                    field.display(cx.db, cx.edition),
                ),
                "`..` takes the defaults the callee declares (`field: T = value` in its \
                 parameter's record type); a field with none is always written",
            )
        }
        RecordProblem::NoCallee => (
            "`..` takes a function's defaults, and this record is not an argument of a call \
             to a function by name"
                .to_owned(), // alloc: one error
            "a function pointer, a closure and a `Fn` value have no defaults, which belong to \
             a function's declaration: write every field, or call the function by name",
        ),
    };
    let mut diag = Diag::error(None, message, span);
    diag.help(help);
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
