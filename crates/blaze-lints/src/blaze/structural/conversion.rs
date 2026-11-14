//! `field_wise_conversion` and `conversion_misses_field`: a conversion between two structs
//! written field by field, `impl From<A> for B` whose `from` builds `B { x: a.x, y: a.y, .. }`,
//! or a method `to_*`, `into_*` or `as_*` of `A` that builds a `B` from `self`'s fields.
//!
//! `conversion_misses_field` is the bug form, read against the two types' field names:
//!
//! - a crossed field: `x: a.y` where `A` has an `x` of its own and `B` a `y` (two fields swapped
//!   or one copied twice), which compiles whenever the two have one type;
//! - a dropped field: `A` and `B` both have a `z` and `from` never reads `a.z` (it fills `z`
//!   with something else, or leaves it to `..rest`).
//!
//! A field left out on purpose is destructured as `z: _` with `// left out: <why>`.
//!
//! `field_wise_conversion` is the advice form: a correct field-by-field copy of three fields or
//! more. Two types that carry the same fields are often one concept twice (rule
//! one-concept-one-home); where they must stay two, the end state is one generic conversion
//! over `fields_of::<A>()`, matched by name. No fix for either: which field was meant is the
//! author's.

use std::alloc::Global;

use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{
    Impl, LEFT_OUT, listed,
    uniform::{self, Form, Scope, Shape},
};
use crate::{
    blaze::{Lint, exempt, std_mistakes::report},
    check::Cx,
};

/// The fewest fields a conversion copies one by one to be advised on.
const LEAST: usize = 3;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, impls: &[Impl<'t>]) {
    if !cx.on_blaze(Lint::FieldWiseConversion) && !cx.on_blaze(Lint::ConversionMissesField) {
        return;
    }
    for imp in impls {
        for method in &imp.methods {
            let Some(found) = conversion(cx, imp, method.node) else { continue };
            judge(cx, imp, method.node, &found);
        }
    }
}

/// A conversion's two sides and its analysis.
struct Conversion<'t> {
    source_name: String,
    target_name: String,
    source: Vec<String, Global>,
    target: Vec<String, Global>,
    base: &'t str,
    body: uniform::Body<'t>,
    analysis: uniform::Analysis,
}

/// `f` of `imp` read as a conversion, when it is one.
fn conversion<'t>(cx: &Cx<'_, 't>, imp: &Impl<'t>, f: ast::Fn<'t>) -> Option<Conversion<'t>> {
    let db = cx.db();
    let name = f.name()?.text();
    let def = cx.fn_def(f)?;
    let block = f.body()?;
    let (source_adt, target_adt, base) = if imp.is_std("From") && name == "from" {
        let params = def.params_without_self(db);
        let [param] = params.as_slice() else { return None };
        let source = param.ty().strip_references().as_adt()?;
        let base = *uniform::param_names(f).first()?;
        (source, imp.adt, base)
    } else if imp.trait_name.is_none()
        && ["to_", "into_", "as_"].iter().any(|it| name.starts_with(it))
        && f.param_list().and_then(|it| it.self_param()).is_some()
    {
        let target = def.ret_type(db).as_adt()?;
        (imp.adt, target, "self")
    } else {
        return None;
    };
    let (hir::Adt::Struct(source_struct), hir::Adt::Struct(target_struct)) = (source_adt, target_adt)
    else {
        return None;
    };
    if source_adt == target_adt {
        return None;
    }
    let mut source: Vec<String, Global> = Vec::new_in(Global);
    source.extend(source_struct.fields(db).into_iter().map(|it| it.name(db).as_str().to_owned()));
    let mut target: Vec<String, Global> = Vec::new_in(Global);
    target.extend(target_struct.fields(db).into_iter().map(|it| it.name(db).as_str().to_owned()));
    let source_name = source_adt.name(db).as_str().to_owned();
    let target_name = target_adt.name(db).as_str().to_owned();
    let mut fields: Vec<&str, Global> = Vec::new_in(Global);
    fields.extend(source.iter().map(String::as_str));
    let mut owners: Vec<&str, Global> = Vec::new_in(Global);
    owners.extend(target.iter().map(String::as_str));
    let mut makers: Vec<&str, Global> = Vec::new_in(Global);
    makers.push(&target_name);
    if target_adt == imp.adt {
        makers.push("Self");
    }
    let bases = [base];
    let scope = Scope { fields: &fields, owners: &owners, bases: &bases, makers: &makers };
    let text = cx.text();
    let excused = |range: TextRange| exempt::commented(text, range, LEFT_OUT);
    let body = uniform::split(&scope, block, &excused)?;
    if !matches!(body.form, Form::Record | Form::Tuple) {
        return None;
    }
    let analysis = uniform::analyse(&scope, &body, 2);
    Some(Conversion { source_name, target_name, source, target, base, body, analysis })
}

fn judge<'t>(cx: &mut Cx<'_, 't>, imp: &Impl<'t>, f: ast::Fn<'t>, found: &Conversion<'t>) {
    let Some(name) = f.name() else { return };
    let range = name.syntax().text_range();
    let mut source: Vec<&str, Global> = Vec::new_in(Global);
    source.extend(found.source.iter().map(String::as_str));
    let has_target = |field: &str| found.target.iter().any(|it| it == field);
    let base = found.base;
    let mut said: Vec<String, Global> = Vec::new_in(Global);
    for &(label, read) in &found.analysis.crossed {
        let (Some(label), Some(read)) = (source.get(label), source.get(read)) else { continue };
        if has_target(read) {
            said.push(format!("fills `{label}` from `{base}.{read}`"));
        }
    }
    let mut read: Vec<bool, Global> = Vec::new_in(Global);
    read.resize(source.len(), false);
    for term in &found.body.terms {
        for &at in &term.reads {
            read[at] = true;
        }
    }
    let mut dropped: Vec<usize, Global> = Vec::new_in(Global);
    for (at, field) in source.iter().enumerate() {
        if !read[at] && has_target(field) && !found.body.left_out.contains(&at) {
            dropped.push(at);
        }
    }
    if !dropped.is_empty() {
        said.push(format!(
            "never reads {} of `{}`, though `{}` has the same-named field",
            listed(&source, &dropped),
            found.source_name,
            found.target_name,
        ));
    }
    let what = format!("`{}` of `{}`", name.text(), imp.said());
    if !said.is_empty() {
        if !cx.on_blaze(Lint::ConversionMissesField) {
            return;
        }
        let message = format!("{what} {}", said.join(", and "));
        report(
            cx,
            Lint::ConversionMissesField,
            f.syntax(),
            range,
            message,
            "a field-by-field conversion copies each field into its namesake: a crossed or \
             dropped field compiles whenever the types agree, and is wrong; fill each field from \
             its namesake, or leave one out on purpose with `field: _` and `// left out: <why>` in \
             a destructuring of the source",
            None,
        );
        return;
    }
    let analysis = &found.analysis;
    if !cx.on_blaze(Lint::FieldWiseConversion)
        || analysis.shape != Shape::Uniform
        || analysis.covered.len() < LEAST
    {
        return;
    }
    let message = format!(
        "{what} copies {} fields of `{}` into `{}` one by one",
        analysis.covered.len(),
        found.source_name,
        found.target_name,
    );
    let note = format!(
        "two types with the same fields are often one concept twice: make them one; where they \
         must stay two, the end state is one generic conversion over `fields_of::<{}>()`, \
         matched by name, so a field added to both is copied with no edit",
        found.source_name,
    );
    report(cx, Lint::FieldWiseConversion, f.syntax(), range, message, &note, None);
}
