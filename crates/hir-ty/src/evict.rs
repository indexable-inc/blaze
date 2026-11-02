//! Dropping the values of per-body results once what was made of them is stored (#806, #734).
//!
//! A dropped value keeps its memo: its fingerprint and the edges it read. Verifying it walks those
//! edges without the value, so a dependent of an unchanged body stays valid, and a read of it
//! computes it again and backdates by fingerprint, so a later edit cuts off early as if the value
//! had been kept (`evict_values` and `evict_values_of`, which every query with `lru = N` has;
//! docs/v2/plan/lanes/db/query-unify.md, "LRU and eviction").
//!
//! A closure's captures and signature are its body's inference result, dropped with it. Enum
//! discriminants are not dropped: an `i128` is smaller than the fingerprint kept in its place.

use base_db::Db;
use hir_def::{DefWithBodyId, StaticId};

use crate::{
    consteval::{anon_const_eval_query, const_eval_query, const_eval_static_query},
    db::HirDatabase,
    infer::{infer_anon_const_body_query, infer_body_query},
};

/// Drops the inference results of `bodies`. What `bodies` names that was never inferred is left
/// as it is.
pub fn drop_inference(db: &mut Db, bodies: &[DefWithBodyId]) {
    infer_body_query::evict_values_of(db, bodies.iter().copied());
}

/// Drops the inference results of every body, and of every constant of a signature and of a
/// body's array length or repeat count.
pub fn drop_all_inference(db: &mut Db) {
    infer_body_query::evict_values(db);
    infer_anon_const_body_query::evict_values(db);
}

/// Drops the values of `statics`. What `statics` names that was never evaluated is left as it
/// is.
pub fn drop_statics(db: &mut Db, statics: &[StaticId]) {
    const_eval_static_query::evict_values_of(db, statics.iter().copied());
}

/// Drops the value of every constant and static evaluated: named constants with each set of
/// generic arguments, anonymous constants (array lengths, repeat counts, const arguments,
/// `const` blocks) and statics.
pub fn drop_all_const_eval(db: &mut Db) {
    const_eval_query::evict_values(db);
    anon_const_eval_query::evict_values(db);
    const_eval_static_query::evict_values(db);
}

/// Drops every per-body result of hir-ty: what [`drop_all_inference`] and
/// [`drop_all_const_eval`] drop.
pub fn drop_all(db: &mut Db) {
    drop_all_inference(db);
    drop_all_const_eval(db);
}
