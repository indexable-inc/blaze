//! `hand_written_visitor`: a family of walks written by hand, one per type, each calling the
//! same thing on every field: three or more impls in a file whose method of one name
//! (`visit*`, `walk*`, `fold*`, `for_each*`, `children`, `accept`), or of one trait, is
//! [`Shape::Uniform`] over its type's fields. Reported once per family, at its first impl.
//!
//! Each such body is the type's field list again; a field added later is a walk that silently
//! skips it. The end state is one generic walk over `for const f in fields_of::<T>()`
//! (reflection-first), which a type opts out of field by field. Advice, no fix.

use std::alloc::Global;

use syntax::view::{AstNode as _, HasName as _};

use super::{Impl, Method, uniform::Shape};
use crate::{
    blaze::{Lint, std_mistakes::report},
    check::Cx,
};

const LINT: Lint = Lint::HandWrittenVisitor;

/// The fewest types a family has to be reported.
const FAMILY: usize = 3;

/// Whether a method named `name` is a walk.
fn walks(name: &str) -> bool {
    let stem = ["visit", "walk", "fold", "for_each"].iter().any(|stem| {
        name.strip_prefix(stem).is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
    });
    stem || matches!(name, "children" | "accept")
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, impls: &[Impl<'t>]) {
    if !cx.on_blaze(LINT) {
        return;
    }
    // (trait, method) -> the impls with a uniform walk of that name, in file order.
    let mut families: Vec<((Option<&str>, &str), Vec<(&Impl<'t>, &Method<'t>), Global>), Global> =
        Vec::new_in(Global);
    for imp in impls {
        for method in &imp.methods {
            if !walks(method.name) || method.analysis.shape != Shape::Uniform {
                continue;
            }
            let key = (imp.trait_name, method.name);
            match families.iter_mut().find(|(it, _)| *it == key) {
                Some((_, members)) => {
                    if !members.iter().any(|(it, _)| it.adt == imp.adt) {
                        members.push((imp, method));
                    }
                }
                None => {
                    let mut members: Vec<(&Impl<'t>, &Method<'t>), Global> = Vec::new_in(Global);
                    members.push((imp, method));
                    families.push((key, members));
                }
            }
        }
    }
    for ((trait_name, name), members) in &families {
        if members.len() < FAMILY {
            continue;
        }
        let Some(&(_, first)) = members.first() else { continue };
        let mut types: Vec<String, Global> = Vec::new_in(Global);
        types.extend(members.iter().map(|(imp, _)| format!("`{}`", imp.type_name)));
        let of = match trait_name {
            Some(it) => format!(" of `{it}`"),
            None => String::new(),
        };
        let message = format!(
            "`{name}`{of} is written by hand for {} types ({}), each calling the same thing on \
             every field",
            members.len(),
            types.join(", "),
        );
        let range = match first.node.name() {
            Some(it) => it.syntax().text_range(),
            None => first.node.syntax().text_range(),
        };
        report(
            cx,
            LINT,
            first.node.syntax(),
            range,
            message,
            "each walk is its type's field list again, and one a field is added to silently skips \
             it: the end state is one generic walk over `for const f in fields_of::<T>()`, which a \
             type opts out of field by field",
            None,
        );
    }
}
