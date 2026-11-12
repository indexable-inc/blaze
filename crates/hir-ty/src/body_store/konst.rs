//! A constant's value in the item store (AGENTS.md `granular-caching`; docs/v2/plan/design/
//! caches.md, "Unowned defects" 1): the fourth step of a body, [`Step::Const`], for a `const`
//! item with no generic arguments.
//!
//! Until 2026-11-12 a dependency's constant was kept under its crate's check key and its item
//! key (`consteval::deps::key`), and the check key hashes the crate's image and every
//! dependency's check key: one body edited anywhere in the crate, or in a crate below it, moved
//! the key of every constant above. Here a constant is an entry of its own, as a body's
//! inference is: under its item's identity ([`super::BodyFacts::identity`]) and the values of
//! the facts its evaluation read ([`super::record`]), found before anything of its body is
//! read. An edit of another item of its crate changes neither, so the entry is found.
//!
//! - **What is kept.** The value's bytes as `consteval::deps::codec` writes them: its type with
//!   no database id, its memory and its memory map. A value with a part that has no such form
//!   (a type naming a definition, a map holding a static or a vtable) is offered and refused,
//!   counted in the step's `refused`, and evaluated each time: never kept in part.
//! - **Which constants.** One with no generic arguments, whose value is the same for every
//!   asker (`consteval::deps`' module doc says why). An error is never kept. A constant whose
//!   evaluation read something no fact stands for (the body of a `const fn` it calls, until
//!   lane item-hash gives that read its `Code` fact) is refused by the read's name
//!   ([`super::Flushed::refused`]), as any step is.
//! - **A hit runs nothing.** [`constant`] asks the store before `run` is called: no MIR is
//!   built and nothing is interpreted. Under [`super::Mode::Verify`] it runs as well, and a
//!   value that differs from the stored one is the build's error, naming the constant.
//!
//! The invariant: equal keys are equal bytes. The key is the item's identity and the facts the
//! evaluation read; a second value under one key is refused by the flush.

use base_db::Db;
use blaze_format::{Part, SchemaHash, schema_of};
use hir_def::{ConstId, DefWithBodyId};

use super::{INSTALLED, Lookup, Step};
use crate::{
    consteval::{ConstEvalError, deps::codec},
    next_solver::StoredAllocation,
};

#[cfg(test)]
mod tests;

/// The schema of a constant's answer: the codec's version is the layout of its bytes.
pub(super) const SCHEMA: SchemaHash =
    schema_of(&[Part::Str("blaze v2 item store constant value 1"), Part::U64(codec::VERSION as u64)]);

/// `const_eval_query`'s store, for a constant with no generic arguments: `def`'s value, `run`
/// unless it is stored, and offered after `run` with what it read. With no item store
/// installed, one load and a branch.
#[inline]
pub(crate) fn constant<'db>(
    db: &'db Db,
    def: ConstId,
    run: impl FnOnce() -> Result<StoredAllocation, ConstEvalError<'db>>,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    if INSTALLED.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return run();
    }
    stored(db, def, run)
}

#[inline(never)]
fn stored<'db>(
    db: &'db Db,
    def: ConstId,
    run: impl FnOnce() -> Result<StoredAllocation, ConstEvalError<'db>>,
) -> Result<StoredAllocation, ConstEvalError<'db>> {
    let body = DefWithBodyId::ConstId(def);
    match lookup(db, body) {
        Lookup::Off => run(),
        Lookup::Miss => {
            let (value, reads) = blaze_query::db::capture_reads(db, run);
            // An error is not a value: the next build evaluates again and says it again.
            if let Ok(value) = &value {
                offer(db, body, value, reads);
            }
            value
        }
        Lookup::Hit { value, verify: false } => Ok(value),
        Lookup::Hit { value, verify: true } => {
            let fresh = run();
            if fresh.as_ref().ok() != Some(&value) {
                super::fail(
                    db,
                    body,
                    Step::Const,
                    format_args!("stored, it differs from evaluating it"),
                );
            }
            fresh
        }
    }
}

/// `def`'s stored value, made in this database.
pub(super) fn lookup(db: &Db, def: DefWithBodyId) -> Lookup<StoredAllocation> {
    super::lookup(db, def, Step::Const, SCHEMA, |_, bytes| {
        // alloc: the reason an answer does not read
        codec::decode_body(bytes).map(|value| value.store()).map_err(str::to_owned)
    })
}

/// After `def` was evaluated to `value`, reading `reads`: its answer, for the flush to store.
fn offer(
    db: &Db,
    def: DefWithBodyId,
    value: &StoredAllocation,
    reads: Option<blaze_query::db::Reads>,
) {
    super::offer(db, def, Step::Const, SCHEMA, reads, |_, out| match codec::encode(value.as_ref()) {
        Some(body) => {
            out.extend_from_slice(&body);
            Ok(())
        }
        // alloc: the reason a value is not kept
        None => Err("a part of the value has no form without a database's ids".to_owned()),
    });
}
