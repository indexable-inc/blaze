//! Letting go of what hir-ty keeps outside the memo: the trait solver's shared caches and the
//! interner's tables.
//!
//! The memo's own values are not evicted one by one: a value lives as long as its arena, and
//! memory is bounded by dropping arenas (spec docs/v2/plan/design/front-end-on-memo.md, rule 3:
//! `lru` and `weigh` go with no replacement). The per-body drops that were here
//! (`drop_inference`, `drop_all_inference`, `drop_statics`, `drop_all_const_eval`, `drop_all`)
//! went with them.

use std::sync::atomic::{AtomicU32, Ordering};

use base_db::{Crate, Db};

/// Lets go of the trait solver's caches shared by the queries of `crates` (each crate's search
/// graph cache and global-goal table, with and without regions), or with `all` of every crate
/// of `db`: how many caches went. A goal asked again is solved again, its answer the same.
pub fn drop_solver_caches(db: &Db, crates: &[Crate], all: bool) -> usize {
    crate::next_solver::interner::drop_shared_solver_caches(db, crates, all)
}

/// Frees every table the trait solver's interner indexes grew out of whose grace has passed
/// (every task that may have been probing it has ended): how many. A table is otherwise freed
/// only as some index grows again, so after a build's last doublings about half of what the
/// indexes' live tables hold would stay; a release, with every reader stopped, frees them.
pub fn free_retired_intern_tables() -> usize {
    crate::next_solver::interner::table::collect()
}

/// How many front ends (databases a daemon keeps between builds, with what they interned) the
/// process holds: what a release of the interner's tables ([`release_process_tables`]) refuses
/// unless it is 0, as a kept database names rows a release frees.
static FRONT_ENDS: AtomicU32 = AtomicU32::new(0);

/// Counts one more front end kept ([`FRONT_ENDS`]): called as a daemon keeps a database past
/// the build that made it.
pub fn front_end_kept() {
    FRONT_ENDS.fetch_add(1, Ordering::AcqRel);
}

/// Counts a kept front end let go: `db`, about to be dropped, lets go of its trait solver's
/// shared caches (every crate's) and is no longer counted in [`FRONT_ENDS`]. Panics, naming
/// the count, when no front end was counted kept.
pub fn front_end_dropped(db: &Db) {
    crate::next_solver::interner::drop_shared_solver_caches(db, &[], true);
    take_one("front_end_dropped");
}

/// Counts a kept front end taken out of the keep by a build that goes on using its database:
/// it retires nothing (its shared solver caches stay for that build and the next at its
/// revision), and only takes it off [`FRONT_ENDS`]. The build's own tasks then hold the release
/// off (`table::refuse_unless_quiet`'s horizon); the caller counts the database again with
/// [`front_end_kept`] if it keeps it past the build, or drops it, before the process is idle.
/// Panics, naming the count, when no front end was counted kept.
pub fn front_end_taken() {
    take_one("front_end_taken");
}

fn take_one(by: &str) {
    let before = FRONT_ENDS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |kept| kept.checked_sub(1))
        .unwrap_or_else(|kept| {
            panic!(
                "`hir_ty::evict::{by}` with {kept} front ends counted kept \
                 (`hir_ty::evict::FRONT_ENDS`): each is counted once by `front_end_kept`"
            )
        });
    debug_assert!(before > 0);
}

/// [`FRONT_ENDS`] now.
pub(crate) fn front_ends_kept() -> u32 {
    FRONT_ENDS.load(Ordering::Acquire)
}

/// What a release of the interner's tables freed: how many tables it released and the bytes
/// their columns, indexes and retired tables held less what the kept rows hold after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    pub tables: usize,
    pub bytes: u64,
}

/// How many releases of the interner's tables there have been: what a process cache of rows
/// outside hir-ty keeps beside what it holds, starting over when it moved (codegen's
/// `erase_regions` map and its table of exports). A release runs only when no task does, so
/// the first use after it sees the move. A new cache of rows past a database is keyed by a
/// database nonce or by this, never by neither.
pub fn tables_released() -> u64 {
    crate::next_solver::interner::table::releases()
}

/// Lets go of every row of the trait solver's interner but those the process's own statics
/// name, and of everything in the process that names one: the solver caches of every query,
/// thread and database, the param-env cache of every thread (by [`tables_released`]), every
/// cache of rows that starts over by it, then every interner table, index and retired table
/// (`table::release_all`). What is left is the tables of a fresh process: a build after it
/// interns again what it reads and answers as a fresh process does.
///
/// Panics, naming what it found, unless no front end is kept ([`front_end_kept`]), the
/// calling thread is in no task and no task that began before it still runs: a stale row read
/// after a release names another value, a wrong answer, so it is refused rather than trusted.
pub fn release_process_tables() -> Released {
    crate::next_solver::interner::table::refuse_unless_quiet();
    crate::next_solver::interner::clear_tls_solver_cache();
    crate::next_solver::interner::table::release_all()
}
