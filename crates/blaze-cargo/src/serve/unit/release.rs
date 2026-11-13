//! What a build lets go of its front end once nothing still to compile reads it (B5, #806,
//! #734): a crate's bodies' inference and trait solver caches once every unit of it compiled,
//! and the MIR, constant values and solver caches of every crate once no unit is between its
//! phases or memory is short. Without it a
//! build's MIR, inference and constants stay resident to its end (coord-102: about 6 GiB of MIR
//! and 3 GiB of inference of 21 small crates).
//!
//! A drop needs the front end's database exclusive (a value is read in place, `blaze_query`'s
//! row rule 3), so it is a write of the job's [`super::UnitWriter`] as a build script's run is,
//! applied by `Writer::apply` once no reader is alive:
//!
//! - **With another write.** A wave of build scripts' runs or proc macros' loads already stops
//!   the readers; the crates finished by then are let go with it, at no stop of their own.
//! - **When no unit is open.** Every unit that began has compiled, so no reader is left to
//!   wait for, and every MIR held is of a finished crate or of none.
//! - **When memory is short** ([`Release::pressed`]): the system says so, or the gate's core
//!   is past its line with nothing cheaper left to give (`job::giving`,
//!   `blaze_graph::giveback::Rung::Values`). The release waits for the readers under way
//!   (none is cancelled or stopped: a task's own memory is the gate's to bound, by wounds),
//!   and the front end's values kept by bytes are swept, every one
//!   ([`blaze_query::db::Storage::sweep_values`] under caps of 0), not only at the next view.
//!   The walks codegen keeps for the next build are the kept front end's
//!   ([`hir_ty_codegen::v2::Walks`]) and go with it.
//! - **Once the front end is kept**, its build over: every MIR goes then, of every goal and
//!   under any limit or none ([`Release::end`]). MIR is the request's: a value of the build
//!   that made it, read again by the next build only as its memo (verified against, made
//!   again where read), so nothing of it outlives the build that made it. This release is
//!   never on the loop and never before the build's answer: it is kept with the front end
//!   and run by the task that stores the front end, which holds it alone as the writer did
//!   (`Steps::release_kept`). It only lowers memory, so the checkout's next build never waits
//!   for it: a request that waits for the front end has it handed back unreleased, and that
//!   build's own releases, at the latest its end's, let the MIR go. When the gate's core is
//!   past its line the release runs all the same, before that build is given the front end:
//!   memory short stays a reason that stops for nothing but the answer.
//!
//! A value let go keeps its memo, so a read of it later in the build, or by the next build of
//! the head, makes it again and backdates it: nothing read can differ, a re-read costs time and
//! is named by its query's reruns.
//!
//! **An edit's build keeps what it made** ([`Finished::edit`]): a build that found units kept
//! and made at most [`EDIT_CRATES`] crates lets go of no crate's inference by name and of no
//! MIR, mid-build or at its end, unless memory makes a release due. The crate an edit made is
//! the one the next edit makes again, and that build's check reads every body of it: the
//! lints of bodies (`blaze-diagnostics`' `lints/bodies.rs`, one loop over the crate's bodies)
//! read each body's inference, its built MIR (liveness) and its runtime MIR (the known-panics
//! lint and a `Drop`'s recursion), and the dead-code pass reads each body's inference. With
//! the values let go, every edit inferred and lowered every body of the edited crate again,
//! on one thread: ripgrep, one line of `rg::main`, 2026-11-10: `infer_body_query` executed
//! 1,165 of which 2 changed, `mir_body_query` 1,166 (1), `runtime_mir_body_query` 1,164 (1),
//! `unit bodies` 782 ms. What is kept is bounded: the values of at most [`EDIT_CRATES`]
//! crates a build, and of those only what the next two builds read again
//! ([`blaze_query::db::Storage::end_epoch`], run at every build's end, lets go of what the
//! last two did not read); memory short takes it all as before. Idleness takes none of it: a
//! daemon in no edit loop exits whole instead (`crate::serve::idle`'s rule), past 64 times its
//! front end's last remake (30 s to 30 minutes).
//!
//! **Every other build that compiles holds the inference of the workspace's crates it
//! finished last** ([`Release::held`]): a cold build and a rebuild of many crates let go of a
//! dependency's inference as the crate finishes, as before, and of a workspace member's
//! (`CrateOrigin::Local`: a crate an edit can touch) only once [`EDIT_CRATES`] members
//! finished after it. So such a build's end leaves the inference an edit's build's end
//! leaves: that of at most [`EDIT_CRATES`] crates, the ones at the top of the workspace
//! (the program, and what depends on the rest), which any edit makes again. Until 2026-11-11
//! it left none, and the first edit after a cold build made every body's inference of the
//! edited crate again in the lints of bodies, one body after another on one thread, since
//! the bodies' check memos (`hir::check::bodies::body_check_clean`) hold and so no check
//! task reads a body on the pool (ripgrep, 64 cores: the first body edit after a cold build
//! 5.7 s, each later one 0.95 s). The bound: a build holds the inference of at most
//! [`EDIT_CRATES`] crates more than it did, from each one's last unit to the build's end and
//! past it under [`blaze_query::db::Storage::end_epoch`]'s rule; memory short lets go of the
//! held crates with the rest, and an idle daemon's exit with the whole process (`crate::serve::idle`).
//! Its MIR goes at its end as before, every crate's by its tag
//! (`hir::evict::drop_mir` keeps none by crate), so the first edit still lowers the edited
//! crate's built and runtime MIR again where the lints read them: `mir_body_query` and
//! `runtime_mir_body_query` under `evicted_asked` in that build's `query_counts`. A check
//! releases as it did.
//!
//! A check (`JobKind::Check`) keeps its inference: what it made, and what the next check of the
//! head reads again. Its MIR goes as its front end is kept, as every build's does, and memory
//! makes a release of a check due mid-build as of any build: then the MIR, constant values and
//! solver caches of every crate go and the values kept by bytes are swept, but no crate's
//! inference is let go by name. A check that let go of nothing (the
//! release before slowcheck, and before this, a check under no limit) kept every MIR it made in
//! the kept front end, for the next check and the one after (MIR queries are `lru = 0`: let go
//! only when asked), so each check grew blaze's heap by what it made (build 19 of coord
//! nfcheck: 127 GiB live against a budget of 59, 110 GiB of it MIR), and the plane, which grants
//! a task memory only when what the process holds leaves room, then ran every unit's phase
//! alone: one worker of 16 busy for the whole 43-minute check.

#![forbid(unsafe_code)]

use std::{alloc::Global, hash::Hash, sync::atomic::{AtomicU32, Ordering}};

/// The front end releases run since the daemon started, every build's: the build view's
/// `releases` ([`crate::serve::job::doing::memory`]), a counter that only a release moves.
pub(crate) static RELEASES: AtomicU32 = AtomicU32::new(0);

use base_db::Crate;
use blaze_hash::HashMap;
use blaze_lower::{Crates, Lowered};
use blaze_query::db::{Database as _, MemoryUsage};

#[cfg(test)]
mod tests;

/// What the memory gate's core holds, and its size (`blaze_mem::gate`): what a release's line
/// says before and after it. The core is where a front end's values live: what a release
/// lowers, exactly (every commit is counted as it is made).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Core {
    pub(crate) used: u64,
    /// `u64::MAX` for a gate with no core size.
    pub(crate) size: u64,
}

impl Core {
    /// The process's gate's core now: two atomic loads (`Gate::core`).
    pub(crate) fn now() -> Core {
        let (size, used) = blaze_mem::gate::process().core();
        Core { used, size }
    }

    /// Whether it is past its give-back line (`blaze_graph::giveback::line`).
    pub(crate) fn over(self) -> bool {
        self.used > blaze_graph::giveback::line(self.size)
    }
}

/// Clears a stop of `lowered`'s readers (`blaze_query::db::Storage::stop_readers`), with the
/// front end exclusive: no reader is left, and the tasks posted next read on. Nothing where none
/// was made: since the gate (2026-11-13) nothing here stops readers for memory.
pub(crate) fn resume_readers(lowered: &mut Lowered) {
    let db = lowered.served_db();
    if db.storage().readers_stopped() {
        db.storage_mut().resume_readers();
    }
}

/// What codegen's tasks left on the global heap and nobody freed: with no codegen task
/// running, that is what codegen keeps between builds. Its kept walks are its kept front
/// end's ([`hir_ty_codegen::v2::Walks`], in [`super::KeptParts`]) and leave with it; no process
/// static holds one. An estimate and an upper bound (`blaze_graph::giveback::Rung::Caches`).
pub(crate) fn kept_caches() -> u64 {
    blaze_mem::tag::snapshot().get(blaze_mem::Tag::Codegen).heap
}

/// Gives back what codegen's freed heap the allocator still keeps; the bytes the codegen tag's
/// heap fell by. Codegen's kept walks are not let go here: they are their kept front end's
/// ([`hir_ty_codegen::v2::Walks`]) and leave whole with it, as nothing else names their ids.
pub(crate) fn give_kept_caches() -> u64 {
    let before = kept_caches();
    blaze_mem::tag::give_back_heap();
    let gave = before.saturating_sub(kept_caches());
    let kept_walks = hir_ty_codegen::v2::kept_walks();
    tracing::info!(target: "blaze", gave, kept_walks, "codegen's freed heap was given back");
    gave
}

/// What a kept front end's query storage holds, from counters
/// (`blaze_query::db::Storage::held_bytes`): what letting go of the whole front end gives
/// back, at least (`blaze_graph::giveback::Rung::FrontEnd`).
pub(crate) fn kept_held(lowered: &Lowered) -> u64 {
    lowered.db().storage().held_bytes()
}

/// What the values a kept front end keeps by bytes weigh now
/// (`blaze_query::db::Storage::weighed_bytes`, summed): what it can give and the engine makes
/// again from the memos (`blaze_graph::giveback::Rung::Values`). The front end is the
/// caller's alone, as a kept one is the loop's.
pub(crate) fn kept_values(lowered: &mut Lowered) -> u64 {
    lowered.served_db().storage_mut().weighed_bytes().iter().sum()
}

/// Lets go of about `bytes` of the values a kept front end keeps by bytes, the least recently
/// verified first, each tag by its part of what they weigh
/// (`blaze_query::db::Storage::sweep_values` under caps lowered by that much); what went, as
/// the weights say. Every value let go keeps its memo: a read makes it again and backdates
/// it. The pages go back to the system at once on this thread, and on each other as it next
/// allocates or idles (`blaze_mem::tag::give_back_heap`); the purge the give-back rule asks
/// after reaches the rest.
pub(crate) fn give_kept_values(lowered: &mut Lowered, bytes: u64) -> u64 {
    let db = lowered.served_db();
    let weights = db.storage_mut().weighed_bytes();
    let before: u64 = weights.iter().sum();
    if before == 0 || bytes == 0 {
        return 0;
    }
    let keep = before.saturating_sub(bytes);
    let mut caps = [u64::MAX; blaze_mem::Tag::COUNT];
    for (cap, &weight) in caps.iter_mut().zip(&weights) {
        if weight != 0 {
            let part = u128::from(weight) * u128::from(keep) / u128::from(before);
            *cap = u64::try_from(part).unwrap_or(u64::MAX);
        }
    }
    db.storage_mut().sweep_values(&caps);
    blaze_mem::tag::give_back_heap();
    let after: u64 = db.storage_mut().weighed_bytes().iter().sum();
    before.saturating_sub(after)
}

/// The most crates a build may make and still be an edit's ([`Finished::edit`]): the crate
/// edited and the few a changed interface reaches at once. Past it the build is a rebuild,
/// whose crates' values go as each finishes, as a cold build's do.
pub(crate) const EDIT_CRATES: usize = 4;

/// Where each unit of a build stands, for [`Finished`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stand {
    NotBegun,
    /// Its first task started and its compile has not ended.
    Open,
    Compiled,
}

/// Which crates of a build every unit of has compiled, and whether a unit is open. Keyed by
/// `K`, a crate (`base_db::Crate` in the daemon), so it is told without a database.
pub(crate) struct Finished<K> {
    /// Each unit's crate; none for a build script's run.
    of: Vec<Option<K>>,
    stands: Vec<Stand>,
    /// Each crate's units whose compile has not ended.
    left: HashMap<K, u32, Global>, // alloc: one entry a crate, once per build
    /// Crates whose every unit compiled, in the order the last one did, not taken yet.
    ready: Vec<K>,
    /// The units found kept as they began ([`Finished::kept`]): replayed from the store, with
    /// nothing of the front end made for them in this build.
    kept: Vec<bool>,
    /// The crates a unit of which compiled without being found kept: what this build made
    /// something of in its front end.
    made: HashMap<K, (), Global>, // alloc: one entry a crate made, once per build
    /// Whether a crate of `ready` is one of `made`: a release now has something of this
    /// build's to let go ([`Finished::has_made`]).
    ready_made: bool,
    /// Units [`Stand::Open`]. A unit that panicked stays open (no compile ends it), and the
    /// build's MIR then goes only when memory is short or the front end is kept.
    open: u32,
    /// Units found kept ([`Finished::kept`]), each once.
    found_kept: u32,
}

impl<K: Copy + Eq + Hash> Finished<K> {
    /// A build whose unit `unit` is of crate `of[unit]`.
    pub(crate) fn new(of: Vec<Option<K>>) -> Finished<K> {
        let mut left = HashMap::new_in(Global); // alloc: as the field
        for krate in of.iter().flatten() {
            *left.entry(*krate).or_insert(0) += 1;
        }
        let stands = vec![Stand::NotBegun; of.len()]; // alloc: a stand a unit, once per build
        let kept = vec![false; of.len()]; // alloc: a flag a unit, once per build
        Finished {
            of,
            stands,
            left,
            ready: Vec::new(),
            kept,
            made: HashMap::new_in(Global), // alloc: as the field
            ready_made: false,
            open: 0,
            found_kept: 0,
        }
    }

    /// Unit `unit` was found kept as it began: its record and outputs are in the store, and
    /// its compile replays them. Told before its compile ends; a unit not told counts as made.
    pub(crate) fn kept(&mut self, unit: usize) {
        if let Some(kept) = self.kept.get_mut(unit)
            && !std::mem::replace(kept, true)
        {
            self.found_kept += 1;
        }
    }

    /// Whether the build is, so far, an edit's: it found a unit kept (the store holds an
    /// earlier build's results of it, so this is not a first build) and made at most
    /// [`EDIT_CRATES`] crates. What such a build made is what the next edit's build reads
    /// again (the module), so it is not let go by name. A build that goes on to make more
    /// stops being one, and the crates it finished meanwhile, never taken, go then.
    pub(crate) fn edit(&self) -> bool {
        self.found_kept > 0 && self.made.len() <= EDIT_CRATES
    }

    /// Unit `unit`'s first task started. A build script's run has no crate and no compile to
    /// end it, so it never opens.
    pub(crate) fn began(&mut self, unit: usize) {
        if self.of.get(unit).copied().flatten().is_none() {
            return;
        }
        let Some(stand) = self.stands.get_mut(unit) else { return };
        if *stand == Stand::NotBegun {
            *stand = Stand::Open;
            self.open += 1;
        }
    }

    /// Unit `unit`'s compile ended, ok or not; a unit told twice counts once.
    pub(crate) fn compiled(&mut self, unit: usize) {
        let Some(stand) = self.stands.get_mut(unit) else { return };
        match *stand {
            Stand::Compiled => return,
            Stand::Open => self.open -= 1,
            Stand::NotBegun => {}
        }
        *stand = Stand::Compiled;
        let Some(krate) = self.of[unit] else { return };
        if !self.kept.get(unit).copied().unwrap_or(false) {
            self.made.insert(krate, ());
        }
        if let Some(left) = self.left.get_mut(&krate) {
            *left -= 1;
            if *left == 0 {
                self.ready.push(krate); // alloc: a crate a build, at most
                self.ready_made |= self.made.contains_key(&krate);
            }
        }
    }

    /// Whether no unit is open: every unit that began has compiled.
    pub(crate) fn idle(&self) -> bool {
        self.open == 0
    }

    pub(crate) fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Whether a crate finished since the last take is one this build made something of: a
    /// unit of it compiled that was not found kept. A crate whose units were all found kept
    /// is finished and holds nothing of this build's to let go.
    pub(crate) fn has_made(&self) -> bool {
        self.ready_made
    }

    /// The crates finished since the last take.
    pub(crate) fn take(&mut self) -> Vec<K> {
        self.ready_made = false;
        std::mem::take(&mut self.ready)
    }
}

/// A job's releases of its front end: what is finished, and whether memory makes one due.
/// Keyed by `K` as [`Finished`] is, a crate in the daemon.
pub(crate) struct Release<K = Crate> {
    finished: Finished<K>,
    /// Memory made a release due ([`Release::pressed`]).
    pressed: bool,
    /// Whether the build compiles code. A check's front end lets go mid-build only when memory
    /// makes a release due, and then of no crate's inference by name.
    compiles: bool,
    /// The build is over and its front end is to be kept ([`Release::end`]): the next release
    /// lets go of every MIR, whatever the goal and memory.
    ended: bool,
    /// The workspace's crates finished whose inference is held for the next edit, the first
    /// finished first: at most [`EDIT_CRATES`], the last to finish ([`Release::plan_of`]).
    /// Empty for a check and for an edit's build, which name no crate.
    held: Vec<K>,
}

/// What one release lets go of, told by [`Release::plan`].
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Plan<K> {
    /// The crates finished since the last release whose bodies' inference goes: all but the
    /// workspace's crates the build holds ([`Release::held`]), with each held one that
    /// [`EDIT_CRATES`] workspace crates finished after, or that memory takes. Always
    /// empty for a check, whose inference the next check of the head reads again, and for an
    /// edit's build ([`Finished::edit`]) that memory does not press.
    pub(crate) crates: Vec<K>,
    /// Every MIR and constant's value goes too: no unit is open, or memory is short, or the
    /// build is over; never for an edit's build that memory does not press, whose end
    /// (`ended` without `mir`) lets go of the trait solver's caches and the unread values
    /// alone.
    pub(crate) mir: bool,
    /// Memory made it due: the values kept by bytes are swept, every one.
    pub(crate) pressed: bool,
    /// The build is over ([`Release::end`]): its MIR goes with it.
    pub(crate) ended: bool,
}

impl Release<Crate> {
    /// For a build of `units` units, of the crates `crates` names; `compiles` unless it is a
    /// check.
    pub(crate) fn new(crates: &Crates, units: usize, compiles: bool) -> Release {
        let of = (0..units).map(|unit| crates.krate(unit)).collect(); // alloc: a crate a unit, once per build
        Release::of(Finished::new(of), compiles)
    }

    /// With the front end exclusive (no reader alive): lets go of what the finished crates
    /// keep, and of every MIR when no unit is open or memory made this release due, then under
    /// memory pressure sweeps every value kept by bytes. A span names what it let go and what
    /// the gate's core held before and after (rule 16).
    pub(crate) fn release(&mut self, lowered: &mut Lowered) {
        let db = lowered.served_db();
        let plan = {
            // A workspace member is a crate an edit can touch: its origin, an input's field.
            let db: &base_db::Db = &*db;
            self.plan_of(|krate: &Crate| krate.data(db).origin.is_local())
        };
        let Some(Plan { crates, mir, pressed, ended }) = plan else { return };
        RELEASES.fetch_add(1, Ordering::Relaxed);
        let before = Core::now();
        // Which crates' inference goes and which is held, by name: the counts alone did not
        // say whether the crate the next edit makes again was among them (rule 16).
        let (let_go, kept) = (names(db, &crates), names(db, self.held()));
        let span = tracing::info_span!(
            target: "blaze",
            "front end release",
            pressed,
            ended,
            crates = crates.len(),
            let_go = let_go.as_str(),
            kept_crates = self.held().len(),
            kept = kept.as_str(),
            mir,
            core = before.used,
            core_size = before.size,
            bodies = tracing::field::Empty,
            inference = tracing::field::Empty,
            mir_dropped = tracing::field::Empty,
            const_eval = tracing::field::Empty,
            solver_caches = tracing::field::Empty,
            intern_tables = tracing::field::Empty,
            unread = tracing::field::Empty,
            kept_describes = tracing::field::Empty,
            left = tracing::field::Empty,
            query_storage = tracing::field::Empty,
        );
        let _release = span.enter();
        let mut dropped = hir::evict::drop_crates_after_store(db, &crates, mir);
        if ended && !mir {
            // An edit's build ends: its inference and MIR stay for the next edit, and the
            // trait solver's caches, which are of this revision alone, go as at any end.
            dropped.solver_caches += hir::evict::drop_every_solver_cache(db);
        }
        if pressed {
            // Every value kept by bytes: a cap of 0 a tag. Each keeps its memo, so a read makes
            // it again. Until 2026-11-13 the caps were each tag's fair share of the limit
            // (`admission::Squeeze::caps`), a proportion the gate deleted: memory short now
            // means the core is past its line with nothing cheaper left, or the system says so.
            db.storage_mut().sweep_values(&[0; blaze_mem::Tag::COUNT]);
        }
        // The build is over: what neither it nor the build before it read of the front end's
        // evictable values goes, the results of revisions no edit since has asked for. Only
        // here, with the build's reads all made; a build that wrote nothing lets nothing go.
        let unread = match ended {
            true => db.storage_mut().end_epoch(),
            false => blaze_query::db::Unread::default(),
        };
        // And what its runs added to the reverse edges is packed, 4 bytes a reader: here,
        // with the answer given and the front end exclusive, never at a view's boundary on a
        // build's path unless the lists doubled the index.
        if ended {
            db.storage_mut().pack_reverse();
            // And what its pulls left for the next changed-set walk is settled: each row the
            // build made again said it changed, and the next edit's walk would start from all
            // of them (a hello-world's first edit: 381,137 rows asked and 381,101 set apart
            // for a write of one file, 1.9 million memos verified, 2026-11-12). Here, after
            // the answer, each costs a load a reader and runs nothing.
            db.storage_mut().settle_changed_set();
            // No seal pass here yet. `Storage::seal_quiet_groups` exists and is tested, but on
            // 2026-11-12 its refinement test (`db::tests::sealed::
            // a_sealed_database_answers_as_an_unsealed_one_under_random_writes`) found three
            // stale answers in three runs, each from a different hole (a reader missed in a
            // broken seal; a floor made over an untracked row; a row made after the seal and
            // missed by the next write's walk). A real build must not answer from a mechanism
            // its own oracle still breaks, so no build seals until that test has passed with
            // its case budget raised a hundredfold on a named commit; the pass is then called
            // here again, once a build, after the settle.
        }
        // Asked for by name (a diagnostic, a walk of every row): which rows are of which kind
        // and the fields that hold a library's rows below `HIGH`, into the daemon's log.
        if ended && std::env::var_os("BLAZE_DURABILITY_CENSUS").is_some() {
            crate::serve::said::logged!(INFO,
                "blaze serve: durability census: {}",
                db.storage().durability_census()
            );
        }
        // What was let go goes back to the system now, not when mimalloc's purge delay and the
        // next page taken get to it: this thread's heaps and every freed page at once, each
        // other thread's heaps as it next allocates or idles (memcap5).
        blaze_mem::tag::give_back_heap();
        let after = Core::now();
        span.record("bodies", dropped.bodies);
        span.record("inference", dropped.inference);
        span.record("mir_dropped", dropped.mir);
        span.record("const_eval", dropped.const_eval);
        span.record("solver_caches", dropped.solver_caches);
        span.record("intern_tables", dropped.intern_tables);
        span.record("unread", unread.values);
        span.record("left", after.used);
        // What the query storage still holds after the drops. On a release memory made due, by
        // ingredient (memcap6): its rows in use are a walk of every function's rows made, a
        // read of every row page, which the build's readers wait out (the front end is
        // exclusive), so it runs only when memory is what made it due. Otherwise
        // the total alone, from counters.
        let storage = if pressed {
            let mut usage = Vec::new(); // alloc: the release's report, an entry an ingredient
            db.storage().memory_usage(&mut usage);
            Holders::of(usage)
        } else {
            Holders::total(db.storage().held_bytes())
        };
        span.record("query_storage", storage.total);
        // One line each release in the daemon's log, on its standard error as the memory line
        // is: why, what went, the gate's core before and after against its size (B5), and the
        // query storage's largest holders.
        crate::serve::said::logged!(INFO,
            "blaze serve: {}",
            Said {
                crates: crates.len(),
                held: self.held().len(),
                mir,
                pressed,
                ended,
                dropped,
                unread,
                before,
                after,
                storage,
            }
        );
    }
}

/// `crates` by name as a span says them, a comma between: the crates a release lets go of
/// the inference of, or holds it of.
fn names(db: &base_db::Db, crates: &[Crate]) -> String {
    use std::fmt::Write as _;
    let mut said = String::new(); // alloc: a release's span, a name a crate
    for krate in crates {
        if !said.is_empty() {
            said.push(',');
        }
        // A `String` takes every write.
        let _ = write!(said, "{}", krate.named(db));
    }
    said
}

/// The line a release says ([`Release::release`]): why it ran, what it let go and how much
/// the gate's core held before and after it, against its size, then what the query storage
/// holds.
pub(crate) struct Said {
    pub(crate) crates: usize,
    /// The workspace's crates whose inference the build holds for the next edit
    /// ([`Release::held`]).
    pub(crate) held: usize,
    pub(crate) mir: bool,
    pub(crate) pressed: bool,
    pub(crate) ended: bool,
    pub(crate) dropped: hir::evict::Dropped,
    /// What the build's end let go of that two builds left unread
    /// ([`blaze_query::db::Storage::end_epoch`]).
    pub(crate) unread: blaze_query::db::Unread,
    pub(crate) before: Core,
    pub(crate) after: Core,
    pub(crate) storage: Holders,
}

/// What the front end's query storage holds, said at the end of a release's line. On a release
/// memory made due, as [`blaze_query::db::Storage::memory_usage`] reports it, with its largest
/// holders: each ingredient and dependency graph column ranked by what it holds (its rows and
/// what its values hold beside them), with its rows made and, where fewer, its rows in use, so
/// a function's pages made for keys it never ran read apart (one page of 1024 rows per ID page
/// it has a key in). On any other, its total alone ([`blaze_query::db::Storage::held_bytes`]).
pub(crate) struct Holders {
    /// Every entry, most held first; none when only the total was read.
    usage: Vec<MemoryUsage>,
    /// What all of them hold.
    pub(crate) total: u64,
}

impl Holders {
    /// The holders said at most.
    const SHOWN: usize = 32;

    pub(crate) fn of(mut usage: Vec<MemoryUsage>) -> Holders {
        usage.sort_by(|a, b| b.held().cmp(&a.held()).then(a.name.cmp(b.name)));
        let total = usage.iter().map(MemoryUsage::held).sum();
        Holders { usage, total }
    }

    /// The total alone, with no holder said.
    pub(crate) fn total(total: u64) -> Holders {
        Holders { usage: Vec::new(), total }
    }
}

/// A count of rows as a holder says it: `950`, `12.3k`, `3.19M`. The unit is picked after
/// rounding, so a count that rounds up to the next unit says it there (999,950 is `1.00M`, not
/// `1000.0k`).
pub(crate) struct Rows(pub(crate) u64);

impl std::fmt::Display for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rows = self.0;
        if rows < 1_000 {
            return write!(f, "{rows}");
        }
        // Tenths of a thousand, rounded half up.
        let tenths = rows.saturating_add(50) / 100;
        if tenths < 10_000 {
            return write!(f, "{}.{}k", tenths / 10, tenths % 10);
        }
        // Hundredths of a million, rounded half up.
        let hundredths = rows.saturating_add(5_000) / 10_000;
        write!(f, "{}.{:02}M", hundredths / 100, hundredths % 100)
    }
}

impl std::fmt::Display for Holders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        write!(f, "the query storage holds {:.2} GiB", gib(self.total))?;
        for (at, usage) in self.usage.iter().take(Self::SHOWN).enumerate() {
            f.write_str(if at == 0 { ": " } else { ", " })?;
            write!(f, "{} {:.2} ({} rows", usage.name, gib(usage.held()), Rows(usage.rows))?;
            if usage.used < usage.rows {
                write!(f, " made, {} used", Rows(usage.used))?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Said {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        let Said {
            crates,
            held,
            mir,
            pressed,
            ended,
            dropped,
            unread,
            before,
            after,
            storage: _,
        } = self;
        f.write_str("front end release (")?;
        match *pressed {
            true if before.over() => write!(
                f,
                "memory: blaze's core is past its line of {:.2} GiB",
                gib(blaze_graph::giveback::line(before.size))
            )?,
            true => f.write_str("memory: the system is short of it")?,
            false if *ended => f.write_str("the build is over")?,
            false if *mir => f.write_str("no unit is open")?,
            false => f.write_str("crates finished")?,
        }
        write!(
            f,
            "): let go of {} inference results of {} bodies in {crates} crates",
            dropped.inference, dropped.bodies
        )?;
        if *mir {
            write!(f, ", {} MIR bodies, {} constant values", dropped.mir, dropped.const_eval)?;
        } else if *ended {
            f.write_str(", no MIR (an edit's build keeps what it made for the next edit)")?;
        }
        write!(f, " and {} trait solver caches", dropped.solver_caches)?;
        if *held != 0 {
            write!(
                f,
                ", holding the inference of the {held} workspace crates finished last for the \
                 next edit"
            )?;
        }
        if *pressed {
            f.write_str(", and swept every value the front end keeps by bytes")?;
        }
        if unread.values != 0 {
            write!(f, ", and {} values the last two builds did not read", unread.values)?;
        }
        write!(f, "; blaze's core held {:.2} GiB, now {:.2} GiB", gib(before.used), gib(after.used))?;
        match before.used.checked_sub(after.used) {
            Some(freed) => write!(f, " ({:.2} freed)", gib(freed))?,
            None => write!(
                f,
                " ({:.2} more: other tasks grew meanwhile)",
                gib(after.used - before.used)
            )?,
        }
        match before.size {
            u64::MAX => f.write_str(", of no core size")?,
            size => write!(f, ", of a core of {:.2} GiB", gib(size))?,
        }
        write!(f, "; {}", self.storage)
    }
}

impl<K: Copy + Eq + Hash> Release<K> {
    pub(crate) fn of(finished: Finished<K>, compiles: bool) -> Release<K> {
        Release {
            finished,
            pressed: false,
            compiles,
            ended: false,
            held: Vec::new(), // alloc: none until a workspace crate finishes
        }
    }

    /// The workspace's crates whose inference this build holds, the first finished first: at
    /// most [`EDIT_CRATES`] ([`Release::plan_of`]).
    pub(crate) fn held(&self) -> &[K] {
        &self.held
    }

    pub(crate) fn began(&mut self, unit: usize) {
        self.finished.began(unit);
    }

    /// Unit `unit` was found kept as it began ([`Finished::kept`]).
    pub(crate) fn kept(&mut self, unit: usize) {
        self.finished.kept(unit);
    }

    /// Unit `unit` compiled, a check's as any build's.
    pub(crate) fn compiled(&mut self, unit: usize) {
        self.finished.compiled(unit);
    }

    /// Memory is short: the system says so, or the gate's core is past its line with nothing
    /// cheaper left to give (`job::giving`, which makes it due at most once a second). A
    /// release is due now: every MIR goes and every value kept by bytes is swept.
    pub(crate) fn pressed(&mut self) {
        self.pressed = true;
    }

    /// The build is over and its front end is kept, no reader alive: the next release lets go
    /// of every MIR, of a check as of any build, under any limit or none. What a build made of
    /// MIR is the build's: kept, it is only what the next build would verify its memos
    /// against, which needs the memo alone. That release is run after the build's answer, by
    /// the task that stores the kept front end (the module's list).
    pub(crate) fn end(&mut self) {
        self.ended = true;
    }

    /// Whether the build should stop its readers for a release: memory makes one due, the
    /// build is over, or the build compiles, is no edit's ([`Finished::edit`]), and no unit
    /// is open and a crate this build made something of is finished (no reader to wait for).
    /// An edit's build has nothing to let go until its end, so its loop stops no reader and
    /// runs no release for the crate it made.
    ///
    /// A crate whose units were all found kept makes none due. Until 2026-11-10 it did: a
    /// warm build finds every dependency's unit kept as it begins, so "no unit open and a
    /// crate finished" held before the first task, and the loop ran a whole release then
    /// (`UnitWriter::apply`: every crate's drop, the heap given back, two reads of the
    /// process's memory; about 100 ms of the first `loop turn` of every body edit) to let go
    /// of nothing this build had made. Such a crate still goes with the next release, one a
    /// made crate or memory makes due, or the build's end's, which no loop runs.
    pub(crate) fn due(&self) -> bool {
        self.pressed
            || self.ended
            || (self.compiles
                && self.finished.idle()
                && self.finished.has_made()
                && !self.finished.edit())
    }

    /// What a release now lets go of, taking it (a second plan is of what finished since);
    /// none when there is nothing. `member` says whether a crate is the workspace's, one an
    /// edit can touch. A check's is due to memory or to its end alone and names no
    /// crate: its MIR, constant values and solver caches go (and under memory, its values are
    /// swept), its inference kept for the next check of the head except as the sweep's caps
    /// take it. Every build's last, at its end, lets go of every MIR, but an edit's
    /// ([`Finished::edit`]) that memory does not press: that one names no crate and keeps its
    /// MIR, has no plan before its end, and leaves the crates it finished untaken, for a
    /// later plan of the same build should it make more crates than an edit does.
    ///
    /// The invariant: a plan that memory did not make due names a workspace crate only once
    /// [`EDIT_CRATES`] workspace crates finished after it, so the build holds, to its end and
    /// past it, the inference of the workspace crates it finished last, at most
    /// [`EDIT_CRATES`] of them: what an edit's build leaves (the module). A plan memory made
    /// due names every crate finished, the held ones first.
    pub(crate) fn plan_of(&mut self, member: impl Fn(&K) -> bool) -> Option<Plan<K>> {
        let pressed = std::mem::take(&mut self.pressed);
        let ended = std::mem::take(&mut self.ended);
        if !self.compiles {
            return (pressed || ended).then(|| Plan {
                crates: Vec::new(),
                mir: true,
                pressed,
                ended,
            });
        }
        if !pressed && self.finished.edit() {
            // alloc: an empty `Vec` allocates nothing
            return ended.then(|| Plan { crates: Vec::new(), mir: false, pressed, ended });
        }
        let finished = self.finished.take();
        let mir = pressed || ended || self.finished.idle();
        if finished.is_empty() && !pressed && !ended {
            return None;
        }
        let crates = match pressed {
            // Memory is short: what was held goes with what finished since.
            true => {
                let mut crates = std::mem::take(&mut self.held);
                crates.extend(finished);
                crates
            }
            false => self.hold(finished, member),
        };
        Some(Plan { crates, mir, pressed, ended })
    }

    /// [`Release::plan_of`] of a build none of whose crates is the workspace's: every crate
    /// finished goes.
    #[cfg(test)]
    pub(crate) fn plan(&mut self) -> Option<Plan<K>> {
        self.plan_of(|_| false)
    }

    /// Holds each of `finished` that is a `member`, in their order, and gives back what goes
    /// now: the others, and each held crate that [`EDIT_CRATES`] members finished after.
    fn hold(&mut self, finished: Vec<K>, member: impl Fn(&K) -> bool) -> Vec<K> {
        let mut goes = Vec::with_capacity(finished.len()); // alloc: a crate a build, at most
        for krate in finished {
            if !member(&krate) {
                goes.push(krate);
                continue;
            }
            if self.held.len() == EDIT_CRATES {
                goes.push(self.held.remove(0));
            }
            self.held.push(krate); // alloc: at most `EDIT_CRATES` entries a build
        }
        goes
    }
}
