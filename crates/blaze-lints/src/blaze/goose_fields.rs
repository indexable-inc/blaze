//! What the field lints drawn from Goose (`wide_index`, `bounded_heap_field`,
//! `enum_vec_padded`) ask of a crate: for every field, how the crate's bodies use it and what
//! its struct literals build it from, read once per crate ([`goose_fields`]).
//!
//! A use is classified by the expression right above the field's place (`s.f`):
//!
//! - the index of `c[s.f]` (through one `as` cast) or the argument of `c.get(s.f)` /
//!   `get_mut` / `get_unchecked*`: an index use, with the container (`c`) when it is a field
//!   itself (`self.items`, `arena.nodes`), so a lint can ask that every index goes into one;
//! - the base of `s.f[i]`, or the receiver of a positional method (`get`, `insert`, `swap`,
//!   `first`, ..): indexed;
//! - the receiver of `push`, `push_str`, `extend`, `extend_from_slice`, `append` or `clear`:
//!   pushed; of `iter` or `into_iter`, or the iterable of a `for` (`for x in &s.f`): iterated;
//!   of `len` or `is_empty`: measured;
//! - an assignment's target (`s.f = ..`, `s.f += ..`): written;
//! - anything else (a borrow handed on, a comparison, a move, `iter_mut`, `sort`): other.
//!
//! Uses in the bodies of `#[automatically_derived]` impls (`Debug`, `Clone`, `PartialEq`,
//! serde's) are not counted: the fix a lint suggests derives the same traits. Their struct
//! literals are counted: a derived `Default` builds an empty value, a derived `Clone` copies the
//! field it is of, and serde's `Deserialize` builds one from input, which is unbounded.
//!
//! A construction is a field's initializer in a struct literal (`S { f: .. }`). Its bound, in
//! elements for a `Vec` and bytes for a `String`, is known for a literal or an empty value:
//! `String::from("..")`, `"..".to_string()` (`to_owned`, `into`), `String::new()`, `Vec::new()`,
//! `Default::default()`, `vec![a, b]`, `vec![x; 4]`, `Vec::from([..])`, `[..].to_vec()`, and a
//! `clone()` of the same field. Anything else is unbounded.

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::{
    AttrDefId, DefWithBodyId, FieldId, ItemContainerId, Lookup as _,
    attrs::AttrFlags,
    expr_store::Body,
    hir::{Array, BinaryOp, Expr, ExprId, Literal, UnaryOp},
    signatures::FunctionSignature,
};
use std::alloc::Global;

use super::{alloc::written_callee, facts, frozen_collection};

/// Where a field's index uses go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Into {
    /// No index use yet.
    #[default]
    Nothing,
    /// Every index use goes into this field (a `Vec` or a slice).
    One(FieldId),
    /// Into more than one container, or one that is not a field.
    Many,
}

/// The bound of what a field is built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Built {
    /// At most this many elements (a `Vec`) or bytes (a `String`) at every literal so far.
    AtMost(u64),
    /// Some literal builds it from something unbounded.
    Unbounded,
}

impl Default for Built {
    fn default() -> Built {
        Built::AtMost(0)
    }
}

/// What the crate does with one field, as the module docs say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FieldFacts {
    /// How many times it is an index (`c[s.f]`, `c.get(s.f)`).
    pub(crate) as_index: u32,
    pub(crate) into: Into,
    pub(crate) indexed: bool,
    pub(crate) pushed: bool,
    pub(crate) iterated: bool,
    pub(crate) measured: bool,
    pub(crate) written: bool,
    pub(crate) other: bool,
    pub(crate) built: Built,
    /// How many struct literals build it.
    pub(crate) sites: u32,
}

/// Every field some body of the crate uses or builds, and how.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct GooseFields(HashMap<FieldId, FieldFacts, Global>);

impl GooseFields {
    /// What the crate does with `field`; the default (nothing) when no body names it.
    pub(crate) fn of(&self, field: FieldId) -> FieldFacts {
        self.0.get(&field).copied().unwrap_or_default()
    }
}

/// How far up a field's place a cast or a borrow is looked through.
const UP: usize = 4;

/// The fields of `krate` and what its bodies do with them, once per crate.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn goose_fields(db: &Db, krate: base_db::Crate) -> GooseFields {
    let mut out = GooseFields(HashMap::new_in(Global));
    for def in frozen_collection::crate_bodies(db, krate) {
        read_body(db, def, &mut out.0);
    }
    out
}

/// Whether `def` is a function of an `#[automatically_derived]` impl.
fn derived(db: &Db, def: DefWithBodyId) -> bool {
    let DefWithBodyId::FunctionId(f) = def else { return false };
    let ItemContainerId::ImplId(imp) = f.lookup(db).container else { return false };
    AttrFlags::query(db, AttrDefId::ImplId(imp)).contains(AttrFlags::AUTOMATICALLY_DERIVED)
}

fn read_body(db: &Db, def: DefWithBodyId, out: &mut HashMap<FieldId, FieldFacts, Global>) {
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let body = analysis.body();
    let infer = analysis.infer();
    let derived = derived(db, def);
    let mut parents = None;
    for (id, expr) in body.exprs() {
        match expr {
            Expr::Field { .. } if !derived => {
                let Some(either::Either::Left(field)) = infer.field_resolution(id) else {
                    continue;
                };
                let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
                let facts = out.entry(field).or_default();
                classify(db, &analysis, parents, id, facts);
            }
            Expr::RecordLit { fields, .. } => {
                let Some(variant) = infer.variant_resolution_for_expr(id) else { continue };
                let all = variant.fields(db);
                for init in fields.iter() {
                    let Some(local_id) = all.field(&init.name) else { continue };
                    let field = FieldId { parent: variant, local_id };
                    let bound = bound_of(db, &analysis, init.expr, field);
                    let facts = out.entry(field).or_default();
                    facts.sites += 1;
                    facts.built = match (facts.built, bound) {
                        (Built::AtMost(a), Some(Some(b))) => Built::AtMost(a.max(b)),
                        // A clone of the field itself: no larger than what built it.
                        (built, Some(None)) => built,
                        _ => Built::Unbounded,
                    };
                }
            }
            _ => {}
        }
    }
}

/// The name of the function the call `call` resolves to.
fn called<'db>(
    db: &'db Db,
    body: &Body,
    infer: &hir_ty::InferenceResult<'db>,
    call: ExprId,
) -> Option<&'db str> {
    let (f, _) = written_callee(infer, body, call)?;
    Some(FunctionSignature::of(db, f).name.as_str())
}

/// Adds the use of the field place `place` to `facts`.
fn classify(
    db: &Db,
    analysis: &hir::BodyAnalysis<'_>,
    parents: &facts::Parents,
    place: ExprId,
    facts: &mut FieldFacts,
) {
    let body = analysis.body();
    let infer = analysis.infer();
    let mut at = place;
    let mut parent = parents.of_expr(at);
    // `c[s.f as usize]`: the cast is looked through for an index.
    let mut cast = false;
    if let Some(p) = parent
        && matches!(body.expr(p), Expr::Cast { .. })
    {
        cast = true;
        at = p;
        parent = parents.of_expr(p);
    }
    let Some(parent) = parent else {
        facts.other = true;
        return;
    };
    let mut index_into = |facts: &mut FieldFacts, container: ExprId| {
        facts.as_index += 1;
        let into = match field_of(analysis, container) {
            Some(it) => match facts.into {
                Into::Nothing => Into::One(it),
                Into::One(old) if old == it => Into::One(it),
                _ => Into::Many,
            },
            None => Into::Many,
        };
        facts.into = into;
    };
    match body.expr(parent) {
        Expr::Index { base, index } if index == at => index_into(facts, base),
        Expr::MethodCall { receiver, args, .. }
            if args.first() == Some(&at)
                && matches!(
                    called(db, body, infer, parent),
                    Some("get" | "get_mut" | "get_unchecked" | "get_unchecked_mut")
                ) =>
        {
            index_into(facts, receiver)
        }
        _ if cast => facts.other = true,
        Expr::Index { base, .. } if base == at => facts.indexed = true,
        Expr::MethodCall { receiver, .. } if receiver == at => {
            match called(db, body, infer, parent) {
                Some("push" | "push_str" | "extend" | "extend_from_slice" | "append" | "clear") => {
                    facts.pushed = true
                }
                Some("iter" | "into_iter") => facts.iterated = true,
                Some("len" | "is_empty") => facts.measured = true,
                Some(
                    "get" | "get_mut" | "first" | "last" | "first_mut" | "last_mut" | "insert"
                    | "remove" | "swap" | "swap_remove" | "split_at" | "split_at_mut",
                ) => facts.indexed = true,
                _ => facts.other = true,
            }
        }
        Expr::Assignment { value, .. } if value != at => facts.written = true,
        Expr::BinaryOp { lhs, op: Some(BinaryOp::Assignment { .. }), .. } if lhs == at => {
            facts.written = true
        }
        // `for x in &s.f` / `for x in s.f`: `IntoIterator::into_iter(..)` of the desugaring.
        Expr::Ref { .. } | Expr::Call { .. } => {
            let call = match body.expr(parent) {
                Expr::Ref { .. } => parents.of_expr(parent),
                _ => Some(parent),
            };
            let iterated = call.is_some_and(|call| {
                matches!(body.expr(call), Expr::Call { .. })
                    && called(db, body, infer, call) == Some("into_iter")
            });
            if iterated {
                facts.iterated = true;
            } else {
                facts.other = true;
            }
        }
        _ => facts.other = true,
    }
}

/// The field `expr` is, its borrows and derefs peeled.
fn field_of(analysis: &hir::BodyAnalysis<'_>, expr: ExprId) -> Option<FieldId> {
    let body = analysis.body();
    let mut at = expr;
    for _ in 0..UP {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
            Expr::Field { .. } => {
                return match analysis.infer().field_resolution(at) {
                    Some(either::Either::Left(it)) => Some(it),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// The bound `init` builds `field` with: `Some(Some(n))` a known bound, `Some(None)` a clone
/// of `field` itself, `None` unbounded or unknown.
fn bound_of(
    db: &Db,
    analysis: &hir::BodyAnalysis<'_>,
    init: ExprId,
    field: FieldId,
) -> Option<Option<u64>> {
    let body = analysis.body();
    let infer = analysis.infer();
    match body.expr(init) {
        Expr::Call { args, .. } => match (called(db, body, infer, init)?, args) {
            ("new" | "default" | "new_in", []) => Some(Some(0)),
            ("from", [arg]) => text_len(body, *arg).or_else(|| array_len(body, *arg)).map(Some),
            ("into_vec", [arg]) => array_len(body, *arg).map(Some),
            _ => None,
        },
        Expr::MethodCall { receiver, args: [], .. } => match called(db, body, infer, init)? {
            "to_string" | "to_owned" | "to_vec" | "into" => {
                text_len(body, receiver).or_else(|| array_len(body, receiver)).map(Some)
            }
            "clone" => (field_of(analysis, receiver) == Some(field)).then_some(None),
            _ => None,
        },
        _ => None,
    }
}

/// The length in bytes of the string literal `expr`.
fn text_len(body: &Body, expr: ExprId) -> Option<u64> {
    match body.expr(expr) {
        Expr::Literal(Literal::String(it)) => Some(it.as_str().len() as u64),
        _ => None,
    }
}

/// The length of the array literal `expr` (`[a, b]`, `[x; 4]`), through a borrow and the
/// `Box::new` / `box_new` of `vec![..]`.
fn array_len(body: &Body, expr: ExprId) -> Option<u64> {
    let mut at = expr;
    for _ in 0..UP {
        match body.expr(at) {
            Expr::Ref { expr, .. } => at = expr,
            Expr::Call { args: [arg], .. } => at = *arg,
            Expr::Array(Array::ElementList { elements }) => return Some(elements.len() as u64),
            Expr::Array(Array::Repeat { repeat, .. }) => {
                return match body.expr(repeat) {
                    Expr::Literal(Literal::Uint(n, _)) => u64::try_from(*n).ok(),
                    Expr::Literal(Literal::Int(n, _)) => u64::try_from(*n).ok(),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}
