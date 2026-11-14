//! `repeated_field_list`: a struct of three fields or more whose whole field list is written
//! out, field by field, in three or more methods of the file: each a body [`Shape::Uniform`]
//! over every field (a `clone`, a `fmt`, an `encode`, a `reset`, a `merge`). Reported once, at
//! the struct.
//!
//! Each such body is the declaration again, and a field added later is an edit in every one of
//! them, found only by reading them all. The finding names the methods. The fix is per body:
//! a derive where one writes it (`hand_written_derivable` says which), a declarative macro over
//! the field list today, and in the end state one generic body over
//! `for const f in fields_of::<T>()` each. Advice, no fix.

use std::alloc::Global;

use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Impl, Items, uniform::Shape};
use crate::{
    blaze::{Lint, std_mistakes::report},
    check::Cx,
};

const LINT: Lint = Lint::RepeatedFieldList;

/// The fewest fields a struct has, and the fewest bodies that list them, to be reported.
const LEAST: usize = 3;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, items: &Items<'t>, impls: &[Impl<'t>]) {
    if !cx.on_blaze(LINT) {
        return;
    }
    for &strukt in &items.structs {
        let Some(adt) = cx.adt_def(ast::Adt::Struct(strukt)) else { continue };
        let mut lists: Vec<String, Global> = Vec::new_in(Global);
        let mut fields = 0;
        for imp in impls.iter().filter(|it| it.adt == adt) {
            fields = imp.fields.len();
            for method in &imp.methods {
                let analysis = &method.analysis;
                if analysis.shape == Shape::Uniform && analysis.covered.len() == imp.fields.len() {
                    lists.push(format!("`{}`", method.name));
                }
            }
        }
        if fields < LEAST || lists.len() < LEAST {
            continue;
        }
        let Some(name) = strukt.name() else { continue };
        let message = format!(
            "the {fields} fields of `{}` are written out one by one in {} methods ({})",
            name.text(),
            lists.len(),
            lists.join(", "),
        );
        report(
            cx,
            LINT,
            strukt.syntax(),
            name.syntax().text_range(),
            message,
            "each is the declaration again, and a field added later is an edit in every one: \
             derive what a derive writes, and give the rest one body over the field list (a \
             declarative macro today, `for const f in fields_of::<T>()` in the end state)",
            None,
        );
    }
}
