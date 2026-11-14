//! `write_only_field`: a field the workspace writes and production code never reads: what reads
//! it is only a derive (`Debug`, `Clone`, `PartialEq`, a serde impl), a test, or nothing. The
//! field costs every value its bytes, every constructor an argument and every reader a thought,
//! and carries nothing. A field only a serde impl reads reaches disk or the wire and is never
//! used: a different message, "stored and never used", since dropping it changes a format.
//!
//! Read off the use rows of the whole workspace ([`super::rows`]): reads by place (`x.f`, a
//! pattern binding `f`, `x.f += ..`) in every crate that can name the field, by role. The
//! built-in derives have no bodies, so their reads are never rows; a serde impl's reads are
//! rows of the `Stored` role.
//!
//! What it adds to `blaze::dead` (rustc's `dead_code` for fields, per crate): a field another
//! crate could read (`pub`, of an exported ADT), and a private field whose only reader is a
//! comparison derive or a serde impl, both of which `dead` counts as reads. A private field no
//! one reads at all is `dead`'s, and this lint leaves it. Not found: a field named `_..`, a
//! `PhantomData`, the fields of a `#[repr(..)]` ADT (a layout someone else reads), of an ADT
//! deriving `Hash`, `Ord` or `PartialOrd` (a key, whose fields are read by being compared and
//! hashed), and test code. No fix: delete the field and its writes, or read it.

use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint, derives, facts, has_attr,
    rows::{Role, Subject, Way},
    test_code, variants_of,
};
use crate::{blaze::facts as type_facts, check::Cx};

const LINT: Lint = Lint::WriteOnlyField;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT) || test_code::is_test_code(cx, adt.syntax()) || has_attr(adt.syntax(), "repr") {
        return;
    }
    let derived = derives(cx, adt.syntax());
    if derived.iter().any(|it| matches!(it.as_str(), "Hash" | "Ord" | "PartialOrd")) {
        return;
    }
    let compared = derived.iter().any(|it| it == "PartialEq");
    let db = cx.db();
    for (variant, _, fields) in variants_of(cx, adt) {
        let shape = hir_def::signatures::VariantFields::of(db, variant);
        // alloc: a variant's fields, with the name and place of each
        let fields: Vec<(ast::Node<'t>, String, Option<ast::Type<'t>>, u32)> = match fields {
            Some(ast::FieldList::RecordFieldList(list)) => list
                .fields()
                .filter_map(|field| {
                    let name = field.name()?.text().to_owned();
                    let (local, _) = shape.fields().iter().find(|(_, it)| it.name.as_str() == name)?;
                    Some((field.syntax(), name, field.ty(), local.into_raw().into_u32()))
                })
                .collect(),
            Some(ast::FieldList::TupleFieldList(list)) => list
                .fields()
                .enumerate()
                .filter_map(|(place, field)| {
                    Some((field.syntax(), place.to_string(), field.ty(), u32::try_from(place).ok()?))
                })
                .collect(),
            None => Vec::new(),
        };
        for (node, name, ty, index) in fields {
            if name.starts_with('_') || ty.is_some_and(|it| phantom(cx, it)) {
                continue;
            }
            let Some(subject) = Subject::of_field(db, variant, index) else { continue };
            let seen = facts(cx, subject);
            if seen.get(Role::Plain, Way::Read) > 0 {
                continue;
            }
            let stored = seen.get(Role::Stored, Way::Read) > 0;
            let tested = seen.get(Role::Test, Way::Read) > 0;
            if !(stored || compared || cx.is_exported(node)) {
                // A private field no one reads is `blaze::dead`'s.
                continue;
            }
            let (message, note) = if stored {
                (
                    format!("field `{name}` is stored and never used: only its serialization reads it"),
                    "it reaches disk or the wire and nothing reads it back for use; delete it with a format \
                     change, or read it where it was meant to be",
                )
            } else if tested {
                (
                    format!("field `{name}` is written and only tests read it"),
                    "production code never reads it: a test reading it checks a value nothing uses",
                )
            } else if compared {
                (
                    format!("field `{name}` is written and read only by derived `PartialEq` and `Debug`"),
                    "comparing or printing a value is no use of it: the field carries nothing",
                )
            } else {
                (
                    format!("field `{name}` is written and never read in the workspace"),
                    "no crate of the workspace reads it, through a place or a pattern",
                )
            };
            let Some(range) = cx.range(node) else { continue };
            super::report(cx, LINT, node, range, message, note, None);
        }
    }
}

/// Whether `ty` is `PhantomData`, which is never read.
fn phantom<'t>(cx: &Cx<'_, 't>, ty: ast::Type<'t>) -> bool {
    let db = cx.db();
    cx.resolve_type(ty)
        .and_then(|it| it.as_adt())
        .is_some_and(|adt| type_facts::adt_is(db, adt, "core", "PhantomData"))
}
