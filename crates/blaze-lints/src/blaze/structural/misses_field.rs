//! `impl_misses_field`: a field-wise body that leaves a field out without saying so, or two
//! field-wise impls of one type that cover different fields where the two must agree.
//!
//! **One body.** A method meant to cover every field (`Debug::fmt`, `clone_from`, `encode`,
//! `decode`, `persist`, `restore`, `write_to`, `read_from`, `to_bytes`, `from_bytes`,
//! `serialize`, `deserialize`, `visit*`, `walk*`, `fold*`, `weigh`, `ids_of`, `merge`, `reset`,
//! `validate`) whose body is [`Shape::AlmostUniform`] with a field missing: the template
//! covers the rest, so the missing one is a field added after the body was written. A `Debug`
//! that ends in `finish_non_exhaustive()` says it leaves fields out. `Hash`, `PartialEq`, `Ord`
//! and key builders are `key_ignores_field`'s, which checks them more strictly.
//!
//! **Two bodies.** For one type, among its derives and its field-wise impls (each covering the
//! fields its terms read):
//!
//! - `PartialEq` leaves out a field `Hash` covers: equal values can hash differently, which
//!   breaks every map keyed by the type. The reverse is legal (a hash may cover less);
//! - `Ord` and `PartialEq`, or `Ord` and a field-wise `PartialOrd`, cover different fields:
//!   `cmp` is `Equal` exactly when `==` holds, either way round;
//! - an encoder and its decoder (`encode`/`decode`, `write_to`/`read_from`,
//!   `to_bytes`/`from_bytes`, `persist`/`restore`, `serialize`/`deserialize`, `save`/`load`)
//!   cover different fields: a value does not survive the round trip.
//!
//! The marker of a field left out on purpose is `key_ignores_field`'s, made the one for every
//! field-wise body: destructure every field, `let Self { a, b, cache: _ } = self;`, with
//! `// left out: <why>` on the line of `cache: _`. No fix: which of the two ways is right is
//! the author's to say.

use std::alloc::Global;

use syntax::{TextRange, view::{AstNode as _, HasName as _}};

use super::{
    Impl, Items, Method, derived, listed,
    uniform::{Form, Shape},
};
use crate::{
    blaze::{Lint, std_mistakes::report},
    check::Cx,
};

const LINT: Lint = Lint::ImplMissesField;

/// The encoder and decoder pairs whose fields must agree.
const PAIRS: &[(&str, &str)] = &[
    ("encode", "decode"),
    ("write_to", "read_from"),
    ("to_bytes", "from_bytes"),
    ("persist", "restore"),
    ("serialize", "deserialize"),
    ("save", "load"),
];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, items: &Items<'t>, impls: &[Impl<'t>]) {
    if !cx.on_blaze(LINT) {
        return;
    }
    for imp in impls {
        for method in &imp.methods {
            one_body(cx, imp, method);
        }
    }
    let mut done: Vec<hir::Adt, Global> = Vec::new_in(Global);
    for imp in impls {
        if done.contains(&imp.adt) {
            continue;
        }
        done.push(imp.adt);
        let mut group: Vec<&Impl<'t>, Global> = Vec::new_in(Global);
        group.extend(impls.iter().filter(|it| it.adt == imp.adt));
        let derives = items.struct_of(cx, imp.adt).map(derived);
        two_bodies(cx, &group, derives.as_deref().unwrap_or_default());
    }
}

/// Whether a method named `name` of `imp` is meant to cover every field.
fn field_wise(imp: &Impl<'_>, name: &str) -> bool {
    let named = matches!(
        name,
        "clone_from"
            | "encode"
            | "decode"
            | "persist"
            | "restore"
            | "write_to"
            | "read_from"
            | "to_bytes"
            | "from_bytes"
            | "serialize"
            | "deserialize"
            | "weigh"
            | "ids_of"
            | "merge"
            | "reset"
            | "validate"
    );
    let walk = ["visit", "walk", "fold"].iter().any(|stem| {
        name.strip_prefix(stem).is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
    });
    named || walk || (name == "fmt" && imp.is_std("Debug"))
}

fn one_body<'t>(cx: &mut Cx<'_, 't>, imp: &Impl<'t>, method: &Method<'t>) {
    let analysis = &method.analysis;
    if !field_wise(imp, method.name)
        || analysis.shape != Shape::AlmostUniform
        || analysis.missing.is_empty()
    {
        return;
    }
    if let Form::Chain { after, .. } = &method.body.form
        && after.contains(&"finish_non_exhaustive")
    {
        return;
    }
    let fields = imp.field_names();
    let missing = listed(&fields, &analysis.missing);
    let covered = listed(&fields, &analysis.covered);
    let first = analysis.missing.first().and_then(|it| fields.get(*it)).copied().unwrap_or_default();
    let mut others: Vec<&str, Global> = Vec::new_in(Global);
    others.extend(fields.iter().copied().filter(|it| *it != first));
    let covered_names = others.join(", ");
    let message = format!(
        "`{}` of `{}` covers {covered} the same way but leaves out {missing}",
        method.name,
        imp.said(),
    );
    let note = format!(
        "a field-wise body covers every field: cover {missing} as the others are, or say it is \
         left out on purpose: destructure every field, `let Self {{ {covered_names}, {first}: _ }} = self;`, \
         with `// left out: <why>` on that line"
    );
    let range = name_range(method);
    report(cx, LINT, method.node.syntax(), range, message, &note, None);
}

fn name_range(method: &Method<'_>) -> TextRange {
    match method.node.name() {
        Some(it) => it.syntax().text_range(),
        None => method.node.syntax().text_range(),
    }
}

/// The fields one side covers: every field for a derive, else what the method's terms read.
struct Side<'a, 't> {
    what: &'static str,
    /// The hand-written method, `None` for a derive.
    method: Option<&'a Method<'t>>,
    /// Indices of the fields covered.
    covers: Vec<usize, Global>,
}

/// The side of trait `name` (method `method`) among `group` and `derives`.
fn side<'a, 't>(
    group: &[&'a Impl<'t>],
    derives: &[&str],
    name: &'static str,
    method: &str,
) -> Option<Side<'a, 't>> {
    let n = group.first()?.fields.len();
    if derives.contains(&name) {
        let mut covers: Vec<usize, Global> = Vec::new_in(Global);
        covers.extend(0..n);
        return Some(Side { what: name, method: None, covers });
    }
    let imp = group.iter().copied().find(|it| it.is_std(name))?;
    let found = imp.method(method)?;
    Some(Side { what: name, method: Some(found), covers: touched(found)? })
}

/// The side of the method `name`, in any impl of the group.
fn method_side<'a, 't>(group: &[&'a Impl<'t>], name: &'static str) -> Option<Side<'a, 't>> {
    let found = group.iter().find_map(|imp| imp.method(name))?;
    Some(Side { what: name, method: Some(found), covers: touched(found)? })
}

/// The fields a body reads field by field, by any template, and those it leaves out on
/// purpose; `None` for a body with no template or that hands `self` on whole (it may read the
/// rest). A body covering a single field of several is one: an `eq` of one field is the
/// commonest way to leave the rest out.
fn touched(method: &Method<'_>) -> Option<Vec<usize, Global>> {
    let analysis = &method.analysis;
    if method.body.whole || analysis.template.is_empty() {
        return None;
    }
    let mut out: Vec<usize, Global> = Vec::new_in(Global);
    let all = analysis
        .covered
        .iter()
        .chain(&analysis.odd)
        .copied()
        .chain(analysis.crossed.iter().flat_map(|&(label, read)| [label, read]))
        .chain(method.body.left_out.iter().copied());
    for it in all {
        if !out.contains(&it) {
            out.push(it);
        }
    }
    out.sort_unstable();
    Some(out)
}

fn two_bodies<'t>(cx: &mut Cx<'_, 't>, group: &[&Impl<'t>], derives: &[&str]) {
    let Some(first) = group.first() else { return };
    let fields = first.field_names();
    let ty = first.type_name;
    let eq = side(group, derives, "PartialEq", "eq");
    let hash = side(group, derives, "Hash", "hash");
    let ord = side(group, derives, "Ord", "cmp");
    let partial_ord = side(group, derives, "PartialOrd", "partial_cmp");
    if let (Some(eq), Some(hash)) = (&eq, &hash) {
        let skipped: Vec<usize, Global> = minus(&hash.covers, &eq.covers);
        if !skipped.is_empty() && eq.method.is_some() {
            let message = format!(
                "`PartialEq` of `{ty}` ignores {}, which its `Hash` covers: two values equal by \
                 `==` can hash differently",
                listed(&fields, &skipped),
            );
            let note = "`a == b` must imply `hash(a) == hash(b)`, or every map and set keyed by \
                        the type loses entries: cover the field in `eq`, or leave it out of \
                        `hash` too";
            at(cx, eq, message, note);
        }
    }
    for (one, other) in [(&ord, &eq), (&ord, &partial_ord)] {
        let (Some(one), Some(other)) = (one, other) else { continue };
        differ(cx, &fields, ty, one, other, "`cmp` is `Equal` exactly when the other says equal");
    }
    for &(encode, decode) in PAIRS {
        let (Some(one), Some(other)) = (method_side(group, encode), method_side(group, decode))
        else {
            continue;
        };
        differ(cx, &fields, ty, &one, &other, "a value must survive the round trip");
    }
}

/// Reports two sides of `ty` that must cover the same fields and do not.
fn differ<'t>(
    cx: &mut Cx<'_, 't>,
    fields: &[&str],
    ty: &str,
    one: &Side<'_, 't>,
    other: &Side<'_, 't>,
    why: &str,
) {
    // Reported at each hand-written side that leaves out what the other covers.
    for (short, long) in [(one, other), (other, one)] {
        let left = minus(&long.covers, &short.covers);
        if left.is_empty() || short.method.is_none() {
            continue;
        }
        let message = format!(
            "`{}` of `{ty}` leaves out {}, which `{}` covers",
            short.what,
            listed(fields, &left),
            long.what,
        );
        let note = format!(
            "{why}, so the two cover the same fields: cover it in `{}`, or leave it out of `{}` \
             too",
            short.what, long.what,
        );
        at(cx, short, message, &note);
    }
}

fn at<'t>(cx: &mut Cx<'_, 't>, side: &Side<'_, 't>, message: String, note: &str) {
    let Some(method) = side.method else { return };
    report(cx, LINT, method.node.syntax(), name_range(method), message, note, None);
}

/// What `a` has that `b` has not.
fn minus(a: &[usize], b: &[usize]) -> Vec<usize, Global> {
    let mut out: Vec<usize, Global> = Vec::new_in(Global);
    out.extend(a.iter().copied().filter(|it| !b.contains(it)));
    out
}
