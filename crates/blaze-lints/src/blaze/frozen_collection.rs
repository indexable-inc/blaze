//! `frozen_collection`: a field of a struct, of type `Vec<T, A>`, `String` or
//! `blaze_mem::text::String<A>`, that nothing in the crate changes once the struct is built. A
//! `Box<[T], A>` (`Box<str, A>`) holds the same items in two words, not three (no capacity),
//! and says in its type that they are frozen.
//!
//! Only a field other crates can't name is checked (any visibility but `pub`: private,
//! `pub(super)`, `pub(in ..)`, `pub(crate)`), so every place that can reach it is in the crate,
//! and the crate's every body is read for it ([`mutated_fields`], once per crate): its
//! functions, consts and statics, those of its traits and impls (what derives and other macros
//! wrote included), and those of the items nested in their blocks. A field is changed when a
//! body:
//!
//! - borrows it mutably: `&mut s.f`, `&raw mut s.f`, or an adjustment that does (`s.f.push(x)`,
//!   `s.f.iter_mut()`, a `DerefMut` to its slice);
//! - assigns to it or to a place inside it: `s.f = ..`, `s.f += ..`, `s.f[i] = ..`, a
//!   destructuring assignment naming it;
//! - binds it, or what is inside it, by `ref mut` (written, or by the default binding mode under
//!   a `&mut`) in a struct pattern, or in any tuple-struct pattern of the struct;
//! - uses it as the scrutinee of a `match`, an `if let` or a `let` in a body that binds anything
//!   by `ref mut` (not followed further).
//!
//! A whole-struct overwrite (`*self = Self::new()`) changes no field: the field is still frozen
//! as a value. Moving the field out (`self.f` by value) is not a change. Not checked: a struct
//! with a `#[repr(..)]` (its layout may be a contract), what a macro wrote, and code the crate's
//! configuration leaves out (`#[cfg(test)]` code of a non-test build). No fix: the type change
//! ripples into every constructor (`into_boxed_slice()`, `into_boxed_str()`).

use base_db::Db;
use blaze_hash::HashSet;
use hir::{HasVisibility as _, HirDisplay as _};
use hir_def::{
    DefWithBodyId, FieldId,
    expr_store::Body,
    hir::{BinaryOp, Expr, ExprId, Pat, PatId, UnaryOp, type_ref::Mutability},
    nameres::crate_def_map,
};
use hir_ty::{Adjust, BindingMode, ByRef};
use std::alloc::Global;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use super::{Lint, facts};
use crate::check::{self, Cx};

const LINT: Lint = Lint::FrozenCollection;

/// How many expressions up a field's place is followed to what is done with it; past it the
/// field counts as changed.
const UP: usize = 16;

/// The fields of a crate some body of it may change.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Mutated(HashSet<FieldId, Global>);

impl Mutated {
    /// Whether some body of the crate may change `field` (`jagged_vec` asks too).
    pub(crate) fn contains(&self, field: FieldId) -> bool {
        self.0.contains(&field)
    }
}

/// The fields `krate`'s bodies change, as the module docs say, once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn mutated_fields(db: &Db, krate: base_db::Crate) -> Mutated {
    let mut mutated = Mutated::default();
    for def in crate_bodies(db, krate) {
        changed_in(db, def, &mut mutated.0);
    }
    mutated
}

/// Every body of `krate`: its functions, consts and statics, those of its traits and impls
/// (what derives and other macros wrote included), and those of the items nested in their
/// blocks. The crate-wide scans (`mutated_fields`, `goose_fields::goose_fields`) read these.
pub(crate) fn crate_bodies(db: &Db, krate: base_db::Crate) -> Vec<DefWithBodyId> {
    // alloc: the crate's bodies, once per crate and edit
    let mut defs = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        check::bodies_of(db, module.scope(db), &mut defs);
    }
    let mut next = 0;
    while let Some(&def) = defs.get(next) {
        next += 1;
        for (_, block) in Body::of(db, def).blocks(db) {
            for (_, module) in block.modules(db) {
                check::bodies_of(db, module.scope(db), &mut defs);
            }
        }
    }
    defs
}

/// Adds the fields the body of `def` changes to `out`.
fn changed_in(db: &Db, def: DefWithBodyId, out: &mut HashSet<FieldId, Global>) {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let body = analysis.body();
    let infer = analysis.infer();
    let ref_mut = |pat: PatId| matches!(infer.binding_mode(pat), Some(BindingMode(ByRef::Yes(m), _)) if m.is_mut());
    let any_ref_mut = body.pats().any(|(id, pat)| matches!(pat, Pat::Bind { .. }) && ref_mut(id));
    let mut parents = None;
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Field { .. }) {
            continue;
        }
        let Some(either::Either::Left(field)) = infer.field_resolution(id) else { continue };
        if out.contains(&field) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        if changes(&analysis, parents, id, any_ref_mut) {
            out.insert(field);
        }
    }
    if !any_ref_mut {
        return;
    }
    let binds_mut = |pat: PatId| {
        let mut found = false;
        body.walk_pats(pat, &mut |it| {
            found |= matches!(body.pat(it), Pat::Bind { .. }) && ref_mut(it)
        });
        found
    };
    for (id, pat) in body.pats() {
        let Some(variant) = infer.variant_resolution_for_pat(id) else { continue };
        match pat {
            Pat::Record { args, .. } => {
                let fields = variant.fields(db);
                for arg in args {
                    if binds_mut(arg.pat)
                        && let Some(local_id) = fields.field(&arg.name)
                    {
                        out.insert(FieldId { parent: variant, local_id });
                    }
                }
            }
            Pat::TupleStruct { args, .. } if args.iter().any(|&it| binds_mut(it)) => {
                for (local_id, _) in variant.fields(db).fields().iter() {
                    out.insert(FieldId { parent: variant, local_id });
                }
            }
            _ => {}
        }
    }
}

/// Whether the field place `expr` is changed where it is used: borrowed mutably, assigned, or
/// the place of a projection that is.
fn changes(
    analysis: &hir::BodyAnalysis<'_>,
    parents: &facts::Parents,
    expr: ExprId,
    any_ref_mut: bool,
) -> bool {
    let body = analysis.body();
    let mut at = expr;
    for _ in 0..UP {
        if mut_adjusted(analysis, at) {
            return true;
        }
        let Some(parent) = parents.of_expr(at) else { return false };
        match body.expr(parent) {
            Expr::Ref { mutability: Mutability::Mut, .. } => return true,
            Expr::Ref { .. } => return false,
            Expr::Assignment { value, .. } => return value != at,
            Expr::BinaryOp { lhs, op: Some(BinaryOp::Assignment { .. }), .. } => return lhs == at,
            Expr::Field { .. } | Expr::UnaryOp { op: UnaryOp::Deref, .. } => at = parent,
            Expr::Index { base, .. } if base == at => at = parent,
            // A `let`'s initializer, a block's value, a scrutinee: changed only through a
            // `ref mut` binding, which is not followed.
            Expr::Block { .. } | Expr::Match { .. } | Expr::Let { .. } => return any_ref_mut,
            _ => return false,
        }
    }
    true
}

/// Whether `expr` is adjusted through a mutable borrow or a `DerefMut`.
fn mut_adjusted(analysis: &hir::BodyAnalysis<'_>, expr: ExprId) -> bool {
    let Some(adjustments) = analysis.infer().expr_adjustment(expr) else { return false };
    adjustments.iter().any(|it| match &it.kind {
        Adjust::Borrow(_) => {
            let ty = analysis.ty(it.target.as_ref());
            ty.is_mutable_reference() || matches!(ty.as_raw_ptr(), Some((_, hir::Mutability::Mut)))
        }
        Adjust::Deref(Some(deref)) => deref.0.is_mut(),
        _ => false,
    })
}

/// What a frozen field can be instead.
#[derive(Clone, Copy)]
enum Kind {
    Vec,
    String,
}

/// Whether `ty` is a collection this lint knows.
fn kind(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<Kind> {
    let db = cx.db();
    let adt = ty.as_adt()?;
    let item =
        |name: &str| cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(name)) == Some(adt);
    if item("Vec") {
        return Some(Kind::Vec);
    }
    if item("String") || facts::adt_is(db, adt, "blaze_mem", "String") {
        return Some(Kind::String);
    }
    None
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(item.syntax()) {
        return;
    }
    if item.attrs().any(|it| it.simple_name() == Some("repr")) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    // alloc: the few candidate fields of one struct
    let mut candidates = Vec::new();
    for (index, field) in def.fields(db).into_iter().enumerate() {
        if matches!(field.visibility(db), hir::Visibility::Public) {
            continue;
        }
        let ty = field.ty(db);
        let Some(kind) = kind(cx, &ty) else { continue };
        // A `Vec<Vec<T>>` nothing changes is `jagged_vec`'s, whose flat layout is the better fix.
        if cx.on_blaze(Lint::JaggedVec) && super::jagged_vec::jagged(cx, &ty).is_some() {
            continue;
        }
        // A `Vec`/`String` built only from a few items is `bounded_heap_field`'s, whose inline
        // fix (no heap at all) is the better one.
        if super::bounded_heap_field::covers(cx, FieldId::from(field), &ty) {
            continue;
        }
        candidates.push((index, field, kind));
    }
    if candidates.is_empty() {
        return;
    }
    let mutated = mutated_fields(db, cx.krate().base());
    let target = cx.krate().to_display_target(db);
    let name = def.name(db);
    for (index, field, kind) in candidates {
        if mutated.0.contains(&FieldId::from(field)) {
            continue;
        }
        let Some(node) = field_node(item, &field.name(db), index) else { continue };
        let Some(range) = cx.range(node) else { continue };
        let ty = field.ty(db);
        let (instead, build) = match kind {
            Kind::Vec => ("`Box<[T]>` (`Box<[T], A>` with its allocator)", "`into_boxed_slice()`"),
            Kind::String => ("`Box<str>` (`Box<str, A>` with its allocator)", "`into_boxed_str()`"),
        };
        let message = format!(
            "the field `{}` of `{}` is a `{}` that nothing in the crate changes after the struct is built",
            field.name(db).as_str(),
            name.as_str(),
            ty.display(db, target)
        );
        let note = format!(
            "a {instead} holds the same items in two words, not three (no capacity), and says in its type that \
             they are frozen; build it with {build} where the struct is made"
        );
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The syntax of the field named `name`, the `index`th of `item`: a record field by its name, a
/// tuple field by its place (only when no field has an attribute that could `cfg` it out).
pub(super) fn field_node<'t>(
    item: ast::Struct<'t>,
    name: &hir::Name,
    index: usize,
) -> Option<ast::Node<'t>> {
    match item.field_list()? {
        ast::FieldList::RecordFieldList(list) => list
            .fields()
            .find(|it| it.name().is_some_and(|n| n.text() == name.as_str()))
            .map(|it| it.syntax()),
        ast::FieldList::TupleFieldList(list) => {
            if list.fields().any(|it| it.attrs().next().is_some()) {
                return None;
            }
            list.fields().nth(index).map(|it| it.syntax())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_with;

    const LINT: &str = "frozen_collection";

    #[test]
    fn a_private_vec_nothing_changes_is_found() {
        // Every literal builds `names` empty, which is `bounded_heap_field`'s when it is on (it
        // is by default); this asks of this lint alone.
        let found = perf_with(
            &format!("--warn=blaze::{LINT} --allow=blaze::bounded_heap_field"),
            r#"
pub struct Table {
    names: Vec<u32>,
    grown: Vec<u32>,
    pub open: Vec<u32>,
    reset: Vec<u32>,
    lent: Vec<u32>,
}
impl Table {
    pub fn new() -> Table {
        Table { names: Vec::new(), grown: Vec::new(), open: Vec::new(), reset: Vec::new(), lent: Vec::new() }
    }
    pub fn len(&self) -> usize { self.names.len() }
    pub fn add(&mut self, x: u32) { self.grown.push(x); }
    pub fn clear(&mut self) { self.reset = Vec::new(); }
    pub fn lend(&mut self) -> &mut Vec<u32> { &mut self.lent }
    pub fn replace(&mut self) { *self = Table::new(); }
}
"#,
        );
        assert!(
            found.contains(
                "the field `names` of `Table` is a `Vec<u32>` that nothing in the crate changes"
            ),
            "{found}"
        );
        assert_eq!(found.matches("that nothing in the crate changes").count(), 1, "{found}");
        assert!(found.contains("`Box<[T]>`"), "{found}");
    }
}
