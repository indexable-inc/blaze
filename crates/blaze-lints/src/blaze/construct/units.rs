//! `unit_mismatch` and `primitive_meaning`: a quantity whose unit lives only in its name.
//!
//! `unit_mismatch` (deny in intent, held at warn until a baseline exists): a `+`, `-`, `+=`,
//! `-=` or comparison of two numbers (integers or floats, by their inferred types) whose names
//! carry different units: `elapsed_ms + timeout_secs`, `len_bytes < limit_kib`. A name is the
//! local's, the field's or the method's (`as_millis`) the operand is, looked through casts,
//! borrows and derefs. Nothing but the names says the two are in different units, so nothing
//! stops the sum; `Duration` (or a byte-count newtype) makes it a type error.
//!
//! `primitive_meaning`: a parameter or field of a bare integer or float type whose name carries
//! a unit of time (`timeout_ms: u64`), or the identity of a thing (`node_id: u32`). Every
//! caller must know the unit or which thing's ids it is, by convention; `Duration` or a typed id
//! (`struct NodeId(u32)`, `la_arena::Idx<Node>`) says it once, in the signature.
//!
//! Not looked at: test code, what a macro wrote, a trait impl's method (its signature is the
//! trait's), and a `#[repr(C)]` struct's fields (a layout another program reads).

use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId,
    hir::{ArithOp, BinaryOp, CmpOp, Expr, ExprId},
};
use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

use super::{Found, item_on, report, report_item, words, written};
use crate::{
    blaze::{Lint, per_item},
    check::Cx,
};

const MISMATCH: Lint = Lint::UnitMismatch;
const MEANING: Lint = Lint::PrimitiveMeaning;

/// How many casts, borrows and derefs an operand is looked through for its name.
const NAME_DEPTH: usize = 4;

/// A unit a name's word says: its dimension and its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Unit {
    dimension: Dimension,
    name: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dimension {
    Time,
    Size,
}

/// The words of a name that say its unit.
const UNITS: &[(&[&str], Unit)] = &[
    (&["ns", "nanos", "nanoseconds", "nsec", "nsecs"], Unit { dimension: Dimension::Time, name: "nanoseconds" }),
    (&["us", "micros", "microseconds", "usec", "usecs"], Unit { dimension: Dimension::Time, name: "microseconds" }),
    (&["ms", "millis", "milliseconds", "msec", "msecs"], Unit { dimension: Dimension::Time, name: "milliseconds" }),
    (&["secs", "seconds", "sec"], Unit { dimension: Dimension::Time, name: "seconds" }),
    (&["mins", "minutes"], Unit { dimension: Dimension::Time, name: "minutes" }),
    (&["hours", "hrs"], Unit { dimension: Dimension::Time, name: "hours" }),
    (&["bits"], Unit { dimension: Dimension::Size, name: "bits" }),
    (&["bytes"], Unit { dimension: Dimension::Size, name: "bytes" }),
    (&["kib", "kb"], Unit { dimension: Dimension::Size, name: "KiB" }),
    (&["mib", "mb"], Unit { dimension: Dimension::Size, name: "MiB" }),
    (&["gib", "gb"], Unit { dimension: Dimension::Size, name: "GiB" }),
];

/// The words of a name that say it is the identity of a thing.
const IDENTITY: &[&str] = &["id", "idx", "index"];

/// The unit `name` says, when exactly one does.
fn unit_of(name: &str) -> Option<Unit> {
    let mut found = None;
    for word in words(name) {
        for (spellings, unit) in UNITS {
            if spellings.iter().any(|it| word.eq_ignore_ascii_case(it)) {
                match found {
                    Some(it) if it != *unit => return None,
                    _ => found = Some(*unit),
                }
            }
        }
    }
    found
}

/// The type that carries `unit`, as a message says it.
fn carrier(unit: Unit) -> &'static str {
    match unit.dimension {
        Dimension::Time => "`core::time::Duration`",
        Dimension::Size => "a newtype per unit (`struct Bytes(u64)`, with `Bytes::from_kib`)",
    }
}

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(MISMATCH) {
        return;
    }
    let found = find(cx);
    report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let Expr::BinaryOp { lhs, rhs, op: Some(op) } = expr else { continue };
        let joins = matches!(
            op,
            BinaryOp::ArithOp(ArithOp::Add | ArithOp::Sub)
                | BinaryOp::Assignment { op: Some(ArithOp::Add | ArithOp::Sub) }
                | BinaryOp::CmpOp(CmpOp::Eq { .. } | CmpOp::Ord { .. })
        );
        if !joins || !written(analysis, id) {
            continue;
        }
        let number = |it: ExprId| {
            let ty = analysis.ty(infer.expr_ty(it)).strip_references();
            ty.is_int_or_uint() || ty.is_float()
        };
        if !number(lhs) || !number(rhs) {
            continue;
        }
        let Some((left, left_unit)) = named(db, def, body, lhs, NAME_DEPTH) else { continue };
        let Some((right, right_unit)) = named(db, def, body, rhs, NAME_DEPTH) else { continue };
        if left_unit == right_unit {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let carry = if left_unit.dimension == right_unit.dimension {
            carrier(left_unit).to_owned() // alloc: a message's part
        } else {
            format!("{} and {}, two types the operator does not join", carrier(left_unit), carrier(right_unit)) // alloc: a message's part
        };
        found.push(Found {
            lint: MISMATCH,
            range,
            what: format!(
                "`{left}` is in {} and `{right}` is in {}: this joins them as if they were one unit",
                left_unit.name, right_unit.name
            ), // alloc: a message
            carry,
            removes: "the unit kept by name: a sum of two units is a type error, and a conversion is a named call",
            fix: None,
        });
    }
    found
}

/// The name `expr` has and the unit it says: a local's, a field's or a method's, through casts,
/// borrows and derefs.
fn named(
    db: &base_db::Db,
    def: DefWithBodyId,
    body: &hir_def::expr_store::Body,
    expr: ExprId,
    depth: usize,
) -> Option<(String, Unit)> {
    let name: String = match body.expr(expr) {
        Expr::Path(_) => body[per_item::local_of(db, def, body, expr)?].name.as_str().to_owned(), // alloc: a message's part
        Expr::Field { name, .. } => name.as_str().to_owned(), // alloc: a message's part
        Expr::MethodCall { method_name, .. } => method_name.as_str().to_owned(), // alloc: a message's part
        Expr::Cast { expr, .. } | Expr::Ref { expr, .. } | Expr::UnaryOp { expr, .. } if depth > 0 => {
            return named(db, def, body, expr, depth - 1);
        }
        _ => return None,
    };
    let unit = unit_of(&name)?;
    Some((name, unit))
}

/// The meaning a bare number's `name` gives it: a unit of time, or an identity.
fn meaning(name: &str) -> Option<(String, &'static str)> {
    if let Some(unit) = unit_of(name)
        && unit.dimension == Dimension::Time
    {
        return Some((
            format!("a time in {}", unit.name), // alloc: a message's part
            "`core::time::Duration` (made with `Duration::from_millis` and its siblings)",
        ));
    }
    let mut parts: Vec<&str> = words(name).collect(); // alloc: one name's words
    let last = parts.pop()?;
    if parts.is_empty() || !IDENTITY.iter().any(|it| last.eq_ignore_ascii_case(it)) {
        return None;
    }
    Some((
        format!("the identity of a {}", parts.join("_")), // alloc: a message's part
        "a typed id: a newtype per thing (`struct NodeId(u32)`) or `la_arena::Idx<Node>`",
    ))
}

/// Whether `ty` is a bare integer or float.
fn bare_number(ty: &hir::Type<'_>) -> bool {
    ty.is_int_or_uint() || ty.is_float()
}

pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !item_on(cx, MEANING, f.syntax()) || f.abi().is_some() {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return;
    }
    let Some(list) = f.param_list() else { return };
    let params = func.params_without_self(db);
    let fn_name = func.name(db);
    for (param, hir_param) in list.params().zip(params.iter()) {
        if !bare_number(hir_param.ty()) {
            continue;
        }
        let Some(name) = hir_param.name(db) else { continue };
        let Some((what, carry)) = meaning(name.as_str()) else { continue };
        let Some(ty) = param.ty() else { continue };
        let message = format!(
            "`fn {}` takes `{}: {}`, {what} that only the name says",
            fn_name.as_str(),
            name.as_str(),
            cx.source_of(ty.syntax())
        ); // alloc: a message
        report_item(
            cx,
            MEANING,
            ty.syntax(),
            message,
            carry.to_owned(), // alloc: a message's part
            "the convention every caller must know: a value in the wrong unit, or another thing's id, does not type-check",
        );
    }
}

pub(super) fn check_struct<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !item_on(cx, MEANING, item.syntax()) {
        return;
    }
    if item.attrs().any(|it| it.simple_name() == Some("repr")) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let owner = def.name(db);
    for (index, field) in def.fields(db).into_iter().enumerate() {
        if !bare_number(&field.ty(db)) {
            continue;
        }
        let name = field.name(db);
        let Some((what, carry)) = meaning(name.as_str()) else { continue };
        let Some(node) = crate::blaze::frozen_collection::field_node(item, &name, index) else {
            continue;
        };
        let message = format!(
            "the field `{}` of `{}` is a bare number holding {what} that only the name says",
            name.as_str(),
            owner.as_str()
        ); // alloc: a message
        report_item(
            cx,
            MEANING,
            node,
            message,
            carry.to_owned(), // alloc: a message's part
            "the convention every reader and writer of the field must know",
        );
    }
}
