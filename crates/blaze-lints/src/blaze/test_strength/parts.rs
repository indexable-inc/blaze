//! The parts of a function's output that a finding names, from its type, and which of them the
//! observations of every test cover.
//!
//! A part is a path ([`Step`]s) from the value's top: the fields of the workspace's structs and of
//! tuples, through references and through the payload of `Option::Some` and `Result::Ok` (the
//! value a test means when it unwraps). Another crate's types (`String`, `Vec`), enums of the
//! workspace and anything past [`NESTING`] are not looked into: they are one part each. An
//! `Err` payload is `error_path_untested`'s, not this module's.

use base_db::Db;

use super::flow::{Path, Step};
use crate::blaze::facts;

/// How many levels of fields are looked into.
const NESTING: usize = 3;

/// Whether the observation `seen` covers all of the part at `path`: it ends in
/// [`Step::Whole`] or in no terminal step, and is `path` or a part `path` is inside.
fn covers(seen: &[Step], path: &[Step]) -> bool {
    let (base, whole) = match seen.split_last() {
        Some((Step::Whole, base)) => (base, true),
        Some((last, _)) if last.terminal() => (seen, false),
        _ => (seen, true),
    };
    whole && path.starts_with(base)
}

/// Whether `seen` looks at something strictly inside the part at `path`, or at one property of
/// it (its variant, its length).
fn inside(seen: &[Step], path: &[Step]) -> bool {
    seen.len() > path.len() && seen.starts_with(path)
}

/// What the tests observe of one output.
pub(crate) struct Seen<'a>(pub(crate) &'a [Path]);

impl Seen<'_> {
    pub(crate) fn covered(&self, path: &[Step]) -> bool {
        self.0.iter().any(|it| covers(it, path))
    }

    pub(crate) fn partly(&self, path: &[Step]) -> bool {
        self.0.iter().any(|it| inside(it, path))
    }

    /// Whether anything at all of the output is observed.
    pub(crate) fn any(&self) -> bool {
        !self.0.is_empty()
    }
}

/// The parts of a value of type `ty` that `seen` does not observe, where some sibling or an
/// enclosing part is observed: each named as a finding shows it (`` `Config.port` ``). Empty when
/// the value is observed whole or not at all.
pub(crate) fn unobserved(db: &Db, ty: &hir::Type<'_>, seen: &Seen<'_>) -> Vec<String> {
    let mut out = Vec::new(); 
    walk(db, &ty.strip_references(), &mut Vec::new(), &type_name(db, ty), 0, seen, &mut out); 
    out
}

fn type_name(db: &Db, ty: &hir::Type<'_>) -> String {
    match ty.strip_references().as_adt() {
        Some(adt) => adt.name(db).as_str().to_owned(), 
        None => "the value".to_owned(), 
    }
}

/// The payload of `Some` or `Ok` of `ty` and its path step, when `ty` is core's `Option` or
/// `Result`.
pub(crate) fn wrapped<'db>(db: &'db Db, ty: &hir::Type<'db>) -> Option<(hir::Type<'db>, [Step; 2])> {
    let adt = ty.as_adt()?;
    let variant = if facts::adt_is(db, adt, "core", "Option") {
        1
    } else if facts::adt_is(db, adt, "core", "Result") {
        0
    } else {
        return None;
    };
    Some((ty.type_arguments().next()?, [Step::Variant(variant), Step::Field(0)]))
}

/// Whether `ty` is core's `Result`.
pub(crate) fn is_result(db: &Db, ty: &hir::Type<'_>) -> bool {
    ty.strip_references().as_adt().is_some_and(|it| facts::adt_is(db, it, "core", "Result"))
}

/// Whether `ty` is a struct of a workspace crate: one whose fields a finding may name.
fn local_struct(db: &Db, ty: &hir::Type<'_>) -> bool {
    match ty.as_adt() {
        Some(adt @ hir::Adt::Struct(_)) => adt.module(db).krate(db).origin(db).is_local(),
        Some(hir::Adt::Enum(_) | hir::Adt::Union(_)) | None => false,
    }
}

fn walk<'db>(
    db: &'db Db,
    ty: &hir::Type<'db>,
    path: &mut Path,
    label: &str,
    depth: usize,
    seen: &Seen<'_>,
    out: &mut Vec<String>,
) {
    if depth > NESTING || seen.covered(path) || !seen.partly(path) {
        return;
    }
    if let Some((payload, step)) = wrapped(db, ty) {
        let len = path.len();
        path.extend_from_slice(&step);
        walk(db, &payload.strip_references(), path, label, depth, seen, out);
        path.truncate(len);
        return;
    }
    
    let fields: Vec<(String, hir::Type<'db>, u32)> = if ty.is_tuple() {
        ty.tuple_fields(db)
            .into_iter()
            .enumerate()
            .map(|(at, it)| (format!("{label}.{at}"), it, at as u32))
            .collect()
    } else if local_struct(db, ty) {
        ty.fields(db)
            .into_iter()
            .map(|(field, it)| {
                (format!("{label}.{}", field.name(db).as_str()), it, field.index() as u32)
            })
            .collect()
    } else {
        return;
    };
    for (name, field_ty, index) in fields {
        if field_ty.is_unit() {
            continue;
        }
        path.push(Step::Field(index));
        if !seen.covered(path) {
            match seen.partly(path) {
                true => walk(db, &field_ty.strip_references(), path, &name, depth + 1, seen, out),
                false => out.push(name),
            }
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::{covers, inside};
    use crate::blaze::test_strength::flow::Step;

    /// The invariant: an observation covers a part exactly when it reads all of a part that holds
    /// it; a terminal step other than `Whole` covers nothing, only looks inside.
    #[test]
    fn coverage_reads_whole_parts_and_properties_apart() {
        let field = [Step::Field(1)];
        assert!(covers(&[], &field), "the whole value covers its fields");
        assert!(covers(&[Step::Whole], &field), "a value compared whole covers its fields");
        assert!(covers(&[Step::Field(1), Step::Whole], &field));
        assert!(!covers(&[Step::Field(0)], &field), "a sibling covers nothing of it");
        assert!(!covers(&[Step::Discr], &[]), "the variant is not the value");
        assert!(!covers(&[Step::Len], &[]), "the length is not the value");
        assert!(inside(&[Step::Len], &[]), "the length looks inside the value");
        assert!(inside(&[Step::Field(1), Step::Field(0)], &field));
        assert!(!inside(&field, &field), "a part is not inside itself");
    }
}
