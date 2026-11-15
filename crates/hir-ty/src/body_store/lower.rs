//! A body's lowering as a step of the item store (docs/v2/plan/design/restore.md, "Bodies,
//! inference and MIR as records", the plan for `Step::Lower`): [`Step::Lower`], what the body's
//! lowering gives that its later steps need before they run, found by the item's own content
//! and the facts the lowering read, with no lowering.
//!
//! - **What is kept.** [`Lowered`]: how many expressions, patterns and bindings the body has,
//!   each closure, `async` block and coroutine written in it (its expression, its kind, and
//!   whether it is the body's own: not inside an anonymous constant of its own), whether
//!   lowering found an error and whether the body calls a location macro. No position: a span
//!   or an offset in the file is in no part of it.
//! - **What it is for.** Its value ([`Lowered::value`]) is the key its answer is filed under:
//!   the base key, then each fact's value the lowering's shape lists. A later step of the body
//!   that reads the body's lowering (`Body::of` and the rows of it, lane item-hash's
//!   `LOWERING_ROWS`) reads the body's `STEP_LOWER` fact instead, valued by this, so its own
//!   key changes exactly when something the lowering read did, and is had without lowering:
//!   a hit here runs no lowering. The MIR owners an entry keys ([`super::entries::places`])
//!   are read from [`Lowered::closures`], with no inference.
//! - **No store.** With no item store installed it is made by lowering and has no value.
//!
//! The invariant: equal keys are equal bytes. The key is the base key and the facts the
//! lowering read; a second answer under one key is refused by the flush.

use base_db::Db;
use blaze_format::{Part, SchemaHash, schema_of};
use blaze_ids::ContentId;
use hir_def::{
    DefWithBodyId,
    expr_store::{Body, ExpressionStore, StoreVisitor},
    hir::{ClosureKind, CoroutineKind, Expr, ExprId, PatId},
};

use super::{Lookup, REGISTRY, Step, mir::kind_code};

/// The schema of a lowering's answer: the layout of [`encode`]'s bytes.
const SCHEMA: SchemaHash = schema_of(&[Part::Str("blaze v2 item store lowering 1")]);

/// A closure, `async` block or coroutine written in a body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Closure {
    /// Its expression.
    pub expr: ExprId,
    /// Its kind as written, as `mir::kind_code` numbers it.
    pub kind: u8,
    /// The tag of its MIR owner's place (`mir::Place::tag`) when an entry keys its MIR (a
    /// closure, an `async` closure, an `async` block): 1 for a closure, 2 for a coroutine; 0
    /// for one whose MIR no entry keys (a `gen` coroutine, an old-style one).
    pub tag: u8,
    /// Whether it is the body's own: reached from the body's root and parameters without
    /// entering an anonymous constant other than an inline `const { .. }` block, so the body's
    /// own inference gives it a type (an inline block is inferred inside the body).
    pub own: bool,
}

/// What a body's lowering gives that its later steps need before they run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lowered {
    pub exprs: u32,
    pub pats: u32,
    pub bindings: u32,
    /// Each closure, `async` block and coroutine of the body's store, in the order of their
    /// expressions.
    pub closures: Box<[Closure]>,
    /// Whether lowering found an error (`Body::has_lowering_errors`).
    pub errors: bool,
    /// Whether the body calls a location macro (`mir::calls_location_macro`).
    pub located: bool,
    /// The key the answer is filed under, found or offered: what the body's `STEP_LOWER`
    /// fact is valued by. `None` with no store, for a body with no identity, and for a run the
    /// store refused: a step that read the lowering is then refused too.
    pub value: Option<ContentId>,
}

impl Lowered {
    /// The places of the MIR owners an entry keys (`mir::owners`), from the closures alone:
    /// [`super::mir::Place::BODY`], then each own closure of a keyed kind, in the order of
    /// their expressions.
    pub fn places(&self) -> impl Iterator<Item = super::mir::Place> + '_ {
        let closures = self.closures.iter().filter(|it| it.own && it.tag != 0).map(|it| {
            super::mir::Place {
                tag: it.tag,
                anon: super::mir::NONE,
                expr: u64::from(it.expr.into_raw().into_u32()),
                kind: u64::from(it.kind),
            }
        });
        std::iter::once(super::mir::Place::BODY).chain(closures)
    }
}

/// `def`'s lowering as the item store keeps it: found by the item's content and the facts its
/// shape lists, with no lowering, or made by lowering and offered.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn lowering(db: &Db, def: DefWithBodyId) -> Lowered {
    if REGISTRY.is_empty() {
        return made(db, def);
    }
    match super::lookup_keyed(db, def, Step::Lower, SCHEMA, |_, bytes, key| decode(bytes, key)) {
        Lookup::Off => made(db, def),
        Lookup::Miss => {
            let (mut lowered, reads) = super::capture(db, || made(db, def));
            lowered.value = super::offer(db, def, Step::Lower, SCHEMA, reads, |_, out| {
                encode(&lowered, out);
                Ok(())
            });
            lowered
        }
        Lookup::Hit { value, verify: false } => value,
        Lookup::Hit { value, verify: true } => {
            let fresh = made(db, def);
            let fresh = Lowered { value: value.value, ..fresh };
            if fresh != value {
                super::fail(db, def, Step::Lower, format_args!("stored, it differs from lowering it"));
            }
            value
        }
    }
}

/// `def`'s lowering, made by lowering it: no value.
fn made(db: &Db, def: DefWithBodyId) -> Lowered {
    let body = Body::of(db, def);
    let count = |n: usize| u32::try_from(n).expect("a body of more than u32::MAX parts");
    let mut own = Own { store: &body.store, own: Vec::new() }; // alloc: the walk's list, once per lowering
    for param in body.params.iter() {
        own.on_pat(param.formal);
        own.on_pat(param.user_written);
    }
    own.on_expr(body.root_expr());
    let Own { own: reached, .. } = own;
    let closures = body
        .exprs()
        .filter_map(|(expr, written)| match written {
            Expr::Closure { closure_kind, .. } => Some(Closure {
                expr,
                kind: u8::try_from(kind_code(closure_kind)).expect("a closure kind's code is small"),
                tag: tag_of(closure_kind),
                own: reached.binary_search(&expr.into_raw().into_u32()).is_ok(),
            }),
            _ => None,
        })
        .collect(); // alloc: the memo's list, once per body
    Lowered {
        exprs: count(body.exprs().count()),
        pats: count(body.pats().count()),
        bindings: count(body.bindings().count()),
        closures,
        errors: Body::has_lowering_errors(db, def),
        located: super::mir::calls_location_macro(db, body),
        value: None,
    }
}

/// The tag of the place of a closure of `kind` when an entry keys its MIR (`mir::owners`),
/// 0 when none does.
fn tag_of(kind: ClosureKind) -> u8 {
    match kind {
        ClosureKind::Closure | ClosureKind::CoroutineClosure(CoroutineKind::Async) => 1,
        ClosureKind::Coroutine { kind: CoroutineKind::Async, .. } => 2,
        _ => 0,
    }
}

/// The walk that finds a body's own closures ([`Closure::own`]): every expression reached
/// from the root and the parameters, an inline `const { .. }` block's included, and no other
/// anonymous constant's (an array length, a const generic argument), which is inferred as a
/// body of its own.
struct Own<'a> {
    store: &'a ExpressionStore,
    /// The raw index of each closure reached, sorted at the end of the walk.
    own: Vec<u32>,
}

impl StoreVisitor for Own<'_> {
    fn on_expr(&mut self, expr: ExprId) {
        let store = self.store;
        match store.expr(expr) {
            Expr::Closure { .. } => {
                let raw = expr.into_raw().into_u32();
                // In expression order as the walk goes down; sorted for the lookup.
                match self.own.binary_search(&raw) {
                    Ok(_) => {}
                    Err(at) => self.own.insert(at, raw), // alloc: as the list above
                }
                store.visit_expr_children(expr, &mut *self);
            }
            // An inline block is the body's own: inferred inside it.
            Expr::Const(inner) => self.on_expr(inner),
            _ => store.visit_expr_children(expr, &mut *self),
        }
    }

    fn on_anon_const_expr(&mut self, expr: ExprId) {
        // Another body's: its closures are typed by its own inference.
        let _ = expr;
    }

    fn on_pat(&mut self, pat: PatId) {
        let store = self.store;
        store.visit_pat_children(pat, &mut *self);
    }
}

/// [`Lowered`]'s bytes: the three counts, a flags byte (errors, located), the number of
/// closures, then each closure's expression, kind, tag and whether it is own. Its value is no
/// part of them: it is the key the bytes are filed under.
#[expect(
    blaze::narrowed_len,
    reason = "the lowered record writes its closure count as a u32; a body with 2^32 closures needs a typed refusal from lowering"
)]
fn encode(lowered: &Lowered, out: &mut Vec<u8>) {
    let Lowered { exprs, pats, bindings, closures, errors, located, value: _ } = lowered;
    for word in [*exprs, *pats, *bindings] {
        out.extend_from_slice(&word.to_le_bytes()); // alloc: the store's answer buffer, reused
    }
    out.push(u8::from(*errors) | (u8::from(*located) << 1)); // alloc: as above
    let n = u32::try_from(closures.len()).expect("more than u32::MAX closures");
    out.extend_from_slice(&n.to_le_bytes()); // alloc: as above
    for Closure { expr, kind, tag, own } in closures.iter() {
        out.extend_from_slice(&expr.into_raw().into_u32().to_le_bytes()); // alloc: as above
        out.extend_from_slice(&[*kind, *tag, u8::from(*own)]); // alloc: as above
    }
}

/// The [`Lowered`] [`encode`] wrote as `bytes`, filed under `key`.
fn decode(bytes: &[u8], key: ContentId) -> Result<Lowered, String> {
    // alloc: the reason an answer does not read
    let short = || String::from("a lowering's answer ends early");
    let word = |at: usize| -> Result<u32, String> {
        let part = bytes.get(at..at + 4).ok_or_else(short)?;
        Ok(u32::from_le_bytes(part.try_into().expect("4 bytes")))
    };
    let flags = *bytes.get(12).ok_or_else(short)?;
    if flags & !0b11 != 0 {
        // alloc: as above
        return Err(format!("a lowering's answer has the flags {flags:#x}"));
    }
    let n = word(13)? as usize;
    let rest = bytes.get(17..).ok_or_else(short)?;
    if rest.len() != n * 7 {
        // alloc: as above
        return Err(format!("a lowering's answer of {n} closures has {} bytes for them", rest.len()));
    }
    let closures = rest
        .chunks_exact(7)
        .map(|it| {
            let raw = u32::from_le_bytes(it[..4].try_into().expect("4 bytes"));
            let own = match it[6] {
                0 => false,
                1 => true,
                // alloc: as above
                other => return Err(format!("a lowering's closure is own {other}")),
            };
            Ok(Closure {
                expr: ExprId::from_raw(la_arena::RawIdx::from_u32(raw)),
                kind: it[4],
                tag: it[5],
                own,
            })
        })
        .collect::<Result<Box<[Closure]>, String>>()?; // alloc: the memo's list
    Ok(Lowered {
        exprs: word(0)?,
        pats: word(4)?,
        bindings: word(8)?,
        closures,
        errors: flags & 1 != 0,
        located: flags & 2 != 0,
        value: Some(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant: [`decode`] reads back what [`encode`] wrote, the value being the key it
    /// is filed under; the control: a byte cut off is refused, never read as another lowering.
    #[test]
    fn a_lowering_reads_back_as_written() {
        let lowered = Lowered {
            exprs: 40,
            pats: 7,
            bindings: 3,
            closures: Box::new([
                Closure { expr: ExprId::from_raw(la_arena::RawIdx::from_u32(5)), kind: 0, tag: 1, own: true },
                Closure { expr: ExprId::from_raw(la_arena::RawIdx::from_u32(9)), kind: 4, tag: 2, own: false },
            ]),
            errors: false,
            located: true,
            value: None,
        };
        let mut bytes = Vec::new();
        encode(&lowered, &mut bytes);
        let key = ContentId([3; 32]);
        assert_eq!(decode(&bytes, key), Ok(Lowered { value: Some(key), ..lowered }));
        assert!(decode(&bytes[..bytes.len() - 1], key).is_err(), "a cut answer read");
    }
}
