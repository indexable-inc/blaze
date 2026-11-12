//! Dependency crates read from their images in the daemon's store (docs/v2/plan/design/frontend.md
//! item 5): [`hir_def::deps`] gives a front end each eligible crate's def map, lang items and
//! signatures from its image in place of resolving it, and writes the images the store lacks
//! once the crates are resolved. This is its store: the job's [`blaze_cas::Reader`] to read
//! from, in place, and the puts it asks for, handed to the loop, which alone writes the store.
//!
//! - A front end is served the crates of the units every run of whose closure is applied as
//!   its job is lowered, and each batch of runs and proc macros applied serves those it made
//!   ready or a loaded proc macro reaches ([`serve`], adding to what was served); a crate that
//!   depends on a proc macro not built yet is not eligible. A front end kept across builds is
//!   served at the same points as one freshly lowered: its [`Session`] keeps the key each crate
//!   was last served or resolved under, and a serve asks the store again only of a crate whose
//!   key moved, so a run applied again with other output (a variable only a build script
//!   reads, such as `CC`, changed) takes a served crate's stale image away (rule 21).
//! - Its images are written once the head's next build lowers into the front end kept
//!   ([`store`]): every unit of the build it was kept from has compiled, so what each crate
//!   resolves to is memoized already.
//! - `BLAZE_DEP_IMAGES=check` resolves every crate served as well, and `verify` writes again
//!   what was read; either fails the build, once it has ended, on any difference from the
//!   images ([`differences`]): the daily differential run.
//! - The same store, through one adapter per database ([`Session`]), keeps what else is
//!   computed once for everyone: the dependencies' check outcomes and checked bodies
//!   ([`blaze_diagnostics::cache`]), their constants' values (`hir_ty::consteval::deps`) and
//!   global trait goals' answers (`hir_ty::next_solver::goal_store`). Each is served with the
//!   images and stored where they are.

use std::sync::Arc;

use base_db::{Crate, Db, Env};
use blaze_api::{ApiError, Diagnostic, Level, ServedCrate, ServedPart};
use blaze_cas::{Key, Kind};
use blaze_core::lock::Lock;
use blaze_diagnostics::{Diag, cache};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use blaze_lower::Lowered;
use blaze_units::query::EnvReads;
use hir_def::deps::{Held, ImageBuf, ImageStore, Mode, Served};
use hir_ty::{body_store, next_solver::goal_store};
use std::alloc::Global;

use super::{Job, compile, graph};
use crate::serve::work::{KeptPut, Put};

pub(crate) const IMAGES: &str = "BLAZE_DEP_IMAGES";
pub(crate) const ITEMS: &str = "BLAZE_ITEM_STORE";

/// What the front end's images were served with, over every [`serve`] of it, kept with it
/// until they are stored.
pub(crate) struct Kept {
    pub(crate) served: Served,
    pub(crate) mode: Mode,
}

impl Kept {
    /// What a later [`serve`] of the same front end served, added: a crate's latest key holds,
    /// and a crate keyed again is a hit or stale only as that serve found it.
    fn add(&mut self, more: Served) {
        let Served { keys, hits, stale, build, lookups } = more;
        // Every serve of one front end is by one blaze: what `store` makes the keys again with.
        self.served.build = build;
        // Over the front end's life here; a build's are the session's, which `store` says.
        self.served.lookups.add(lookups);
        self.served.hits.retain(|it| !keys.contains_key(it));
        self.served.stale.retain(|it| !keys.contains_key(it));
        self.served.keys.extend(keys);
        self.served.hits.extend(hits);
        self.served.hits.sort_unstable();
        self.served.hits.dedup();
        self.served.stale.extend(stale);
    }
}

/// A dependency crate a build checked, with the lint flags it was checked with and what the
/// check gave: its outcome and checked bodies are stored with the front end's images
/// ([`store`]).
pub(crate) struct Later {
    pub(crate) krate: Crate,
    pub(crate) flags: Vec<String>,
    pub(crate) diags: Vec<Diag>,
    /// Its errors break its items ([`blaze_diagnostics::CrateCheck::items_broken`]).
    pub(crate) items_broken: bool,
    /// Why its outcome can't be stored ([`cache::encode`]'s refusal), if it can't.
    pub(crate) refused: Option<String>,
}

/// The units of `job` that `waits` has ready: what a front end freshly lowered is served first.
pub(crate) fn ready(job: &Job, waits: &graph::Waits) -> Vec<u32> {
    (0..job.graph.len() as u32).filter(|&unit| waits.ready(unit)).collect() // alloc: setup, once per front end
}

/// The eligible crates of `units` (and their dependencies) whose key moved since the front
/// end's database was last served ([`images`]), served from the store through `session`,
/// adding to what `kept` holds of this job's earlier serves: their images, then their check
/// outcomes and checked bodies, then their constants' values; nothing when no key moved.
/// Called with the front end exclusive (no task reads it), in every job, freshly lowered or
/// kept: once for the units ready when it is lowered, and once per batch of runs and proc
/// macros applied, with the units they made ready and those a loaded macro reaches.
///
/// The invariant: when this returns, no crate at or below a ready unit's crate is awaited
/// (`hir_def::deps::awaited`), whatever `units` holds. A unit is built once `waits` has it
/// ready, and its build reads its crate and everything below, so the serve is where a ready
/// unit's crates are released, and it asks about them itself ([`asked`]) rather than leave
/// each caller to list them: `waits` is a parameter, so no serve can be written without it.
/// It then checks what it promised ([`blocked_ready`]): a ready unit still blocked is an
/// error naming the unit and the crate it waits for, at the write, never a reader's
/// `Unmaintained` later.
pub(crate) fn serve(
    lowered: &mut Lowered,
    job: &Job,
    waits: &graph::Waits,
    units: &[u32],
    session: &mut Session,
    kept: &mut Option<Kept>,
) -> Result<(), ApiError> {
    let mode = mode_in(&mut session.env, &job.input.config.invocation.env)?;
    let build = front_end()?;
    let ready = || {
        (0..job.graph.len() as u32)
            .filter(|&unit| waits.ready(unit))
            .filter_map(|unit| Some((unit, job.crates.krate(unit as usize)?)))
    };
    // alloc: once per serve, one entry per unit's crate
    let written: Vec<Crate> =
        units.iter().filter_map(|&unit| job.crates.krate(unit as usize)).collect();
    let crates = asked(lowered, &written, ready().map(|(_, krate)| krate));
    let served = images(lowered, &crates, build, session, mode)?;
    if let Some((unit, krate, awaited)) = blocked_ready(lowered, ready()) {
        let db = lowered.db();
        // alloc: an error, once
        return Err(ApiError::new(
            format!(
                "{} is ready to build and its crate `{}` still waits for the records of crate `{}`                  ({awaited:?}) after the serve that asked about every ready unit:                  `hir_def::deps::awaited::release` left a crate awaited below one it was asked about",
                lowered.what(unit as usize),
                krate.named(db),
                awaited.named(db),
            ),
            None,
        ));
    }
    let Some(served) = served else { return Ok(()) };
    match kept {
        Some(kept) => kept.add(served),
        None => *kept = Some(Kept { served, mode }),
    }
    let Some(checks) = Arc::get_mut(&mut session.checks) else {
        return Err(ApiError::new(
            "the dependencies' checks are served while a task still reads them",
            None,
        ));
    };
    let db = lowered.db();
    let flags = imaged(job, units.iter().map(|&unit| unit as usize), &session.keyed)?;
    let asked: Vec<(Crate, cache::Inputs<'_>)> =
        flags.iter().map(|(krate, flags)| (*krate, cache::Inputs::of(db, flags))).collect(); // alloc: once per serve
    let served = cache::serve(lowered.served_db(), &asked, &*session.images, checks);
    // Taken whether or not the serve stopped: what it searched before an error was searched.
    session.lookups.checks.add(std::mem::take(&mut checks.lookups));
    served.map_err(|why| ApiError::new(why, None))?;
    let db = lowered.db();
    let consts = (0..job.graph.len())
        .filter_map(|unit| job.crates.krate(unit))
        .filter(|&krate| cache::image_key(db, krate).is_some())
        .map(|krate| (krate, checks.keys.get(&krate).copied()))
        .collect(); // alloc: once per serve, one entry per crate read from an image
    // dyn: the constants' store keeps it for the database's life: the same `Arc` each build
    let adapter: Arc<dyn ImageStore + Send + Sync> = session.images.clone(); // clone: an `Arc`, once per serve
    hir_ty::consteval::deps::set(lowered.served_db(), adapter, consts, mode);
    stamp(lowered, build, session);
    Ok(())
}

/// What a serve asks the store about: `written`, the crates of the units its batch made
/// ready or reached, and every crate of `ready` (each ready unit's crate) that is blocked
/// (`hir_def::deps::awaited::blocked`: it or a crate below it is awaited), sorted, each once.
/// `written` alone when nothing is awaited: one load.
///
/// The invariant it gives [`serve`]: a release of what it returns leaves no crate of `ready`
/// blocked. Asking about `written` alone (the rule until 2026-11-12) leaves blocked every
/// ready unit the batch did not list; the smoke's edition step on tree 3a3fde543b stopped
/// nine units with `no maintained resolution for crate libc .. it waits for the records of
/// crate libc`. Which write left `libc` awaited there is not known (no trace was taken):
/// this rule does not depend on knowing it.
pub(crate) fn asked(
    lowered: &Lowered,
    written: &[Crate],
    ready: impl Iterator<Item = Crate>,
) -> Vec<Crate> {
    let mut asked = written.to_vec(); // alloc: once per serve
    if !lowered.any_awaited() {
        return asked;
    }
    let blocked = hir_def::deps::awaited::blocked(lowered.db());
    asked.extend(ready.filter(|krate| blocked.contains(krate)));
    asked.sort_unstable();
    asked.dedup();
    asked
}

/// The first of `ready` (each ready unit with its crate) whose crate is blocked, with the
/// awaited crate that blocks it: what [`serve`] checks it left none of. One load when nothing
/// is awaited.
pub(crate) fn blocked_ready(
    lowered: &Lowered,
    ready: impl Iterator<Item = (u32, Crate)>,
) -> Option<(u32, Crate, Crate)> {
    if !lowered.any_awaited() {
        return None;
    }
    let db = lowered.db();
    let blocked = hir_def::deps::awaited::blocked(db);
    ready
        .filter(|(_, krate)| blocked.contains(krate))
        .find_map(|(unit, krate)| Some((unit, krate, hir_def::deps::awaited::blocker(db, krate)?)))
}

/// What a [`serve`] that served images leaves: what this serve wrote (images, lang items, check
/// outcomes, constants) is no input of a key, so the keys it walked stand at the view it
/// leaves, and a later serve of those crates there walks nothing ([`images`]). Under debug
/// assertions that is checked: each crate walked keys there as `keyed` holds it.
pub(crate) fn stamp(lowered: &Lowered, build: ContentId, session: &mut Session) {
    #[cfg(debug_assertions)]
    session.walked.assert_stands(lowered.db(), build, &session.keyed);
    session.walked.stamp(lowered.db().nonce_and_revision(), build);
}

/// The images of `crates`' eligible crates and their dependencies whose key ([`hir_def::deps::keys`])
/// is not the one `session` last served or resolved them under, served through `session` by
/// `build` in `mode` ([`Lowered::serve_deps`]): what that serve did of those crates alone, or
/// `None`, nothing written, when no key moved: with no walk when every crate of `crates` was
/// keyed at the database's present view, or by a [`serve`] that left it (no write came
/// between), and with a walk of only the crates no serve at that view keyed yet when some
/// were not ([`Walked::keys`]: the keys of one view are made once). A crate whose
/// key moved loses the image it was read from: it is read from the image under its new key,
/// or resolved. A crate whose key did not move is asked of the store again only as a
/// dependency of one whose key did, and is not imaged again.
pub(crate) fn images(
    lowered: &mut Lowered,
    crates: &[Crate],
    build: ContentId,
    session: &mut Session,
    mode: Mode,
) -> Result<Option<Served>, ApiError> {
    let at = lowered.db().nonce_and_revision();
    // While a crate of the front end is awaited (`hir_def::deps::awaited`: added, its records
    // not asked for yet), every serve releases what it is asked about, whether or not a key
    // moved: a crate with no key (a workspace member) is only ever released this way.
    let awaiting = lowered.any_awaited();
    if session.walked.covers(at, build, crates) {
        if awaiting {
            release(lowered, crates, build, session, mode)?;
        }
        return Ok(None);
    }
    // The crates this walk is the first to key at this view, and their dependencies: those
    // keyed here before are in `keyed` under the keys they have now ([`Walked`]).
    let keys = {
        let span = tracing::info_span!(
            target: "blaze",
            "dependency image keys",
            crates = crates.len(),
            subject = ?at.1,
            known = tracing::field::Empty,
            keyed = tracing::field::Empty,
        )
        .entered();
        session.walked.walks += 1;
        let (known, keys) = session.walked.keys(lowered.db(), at, build, crates);
        span.record("known", known);
        span.record("keyed", keys.len());
        keys
    };
    let mut moved: Vec<Crate> = keys
        .iter()
        .filter(|&(krate, key)| session.keyed.get(krate) != Some(key))
        .map(|(&krate, _)| krate)
        .collect(); // alloc: once per serve, one per crate whose key moved
    // What the serve asked and what moved, kept with the build (`blaze explain edit_work`).
    // A key asked is the database's memo's answer (`hir_def::deps`): how many were made
    // again is `query_counts`' `executed` of that memo, which the view prints beside these.
    tracing::info!(
        target: "blaze",
        stage = blaze_query::db::edit_work::IMAGE_KEYS,
        asked = keys.len() as u64,
        moved = moved.len() as u64,
        "edit work"
    );
    // Why each crate is asked of the store again, for the first few: the key it was last
    // served or resolved under and the key it has now (rule 19: a serve that changes how a
    // crate is read says what moved).
    {
        let db = lowered.db();
        for krate in moved.iter().take(8) {
            tracing::info!(
                target: "blaze",
                krate = %krate.named(db),
                was = ?session.keyed.get(krate),
                now = ?keys.get(krate),
                "a dependency crate's image key moved since it was last served or resolved"
            );
        }
    }
    if moved.is_empty() {
        // `keyed` holds every key walked already.
        session.walked.note(at, build, crates.iter().chain(keys.keys()));
        if awaiting {
            release(lowered, crates, build, session, mode)?;
        }
        return Ok(None);
    }
    moved.sort_unstable();
    // The images and, of each crate read from one, its impl tables
    // (`hir_ty::method_resolution::frozen`), in one write: the pass that first sees a crate
    // read from its image finds its impls served too, and keys none of them.
    let images = &*session.images;
    let trigger = base_db::Trigger::named("the dependencies' images and impl tables served");
    let served = lowered.batch(trigger, |lowered| {
        // The crates no maintainer was shown before this write.
        let unseen = hir_def::deps::awaited::blocked(lowered.db());
        let served = lowered
            .serve_deps(crates, &moved, build, images, mode)
            .map_err(super::lower_error)?;
        let tabled =
            hir_ty::method_resolution::frozen::serve(lowered.served_db(), &served, images, mode)
                .map_err(|why| ApiError::new(why, None))?;
        if tabled.iter().any(|it| !unseen.contains(it)) {
            // A crate the impls maintainer held came to be read from tables, or stopped: it
            // drops or makes that crate's state in a pass over the whole graph.
            lowered.served_db().note_crates_changed();
        }
        Ok::<_, ApiError>(served)
    });
    let served = match served {
        Ok(served) => served,
        Err(error) => {
            // The walk keyed crates `keyed` does not hold: nothing of it is kept, so the next
            // serve walks them again and finds them moved (rule 21).
            session.walked.forget();
            return Err(error);
        }
    };
    debug_assert!(session.images.take_puts().is_empty(), "serving puts nothing");
    // The walk is noted only once `keyed` holds its keys: a serve that failed above leaves no
    // record that a later serve at this view could skip on with the old keys (rule 21).
    session.keyed.extend(keys.iter().map(|(&krate, &key)| (krate, key)));
    session.walked.note(at, build, crates.iter().chain(keys.keys()));
    // Written: what was walked stands again only once [`serve`] is done and stamps its view.
    session.walked.at = None;
    let Served { keys, hits, stale, build, lookups } = served;
    // What this serve's searches of the store came to, for the build's count ([`store`]).
    session.lookups.images.add(lookups);
    let of_moved = |krate: &Crate| moved.binary_search(krate).is_ok();
    // alloc: once per serve that moved a key, one entry per crate whose key moved
    Ok(Some(Served {
        keys: keys.into_iter().filter(|(krate, _)| of_moved(krate)).collect(),
        hits: hits.into_iter().filter(of_moved).collect(),
        stale: stale.into_iter().filter(of_moved).collect(),
        build,
        lookups,
    }))
}

/// Releases `asked` and what they depend on from the front end's awaited crates with nothing
/// fetched ([`Lowered::serve_deps`] with no crate to fetch): what a serve that found no key
/// moved still owes the crates it was asked about. Each released crate gets its first run in
/// that write; nothing is written, and the view stands, when none of them was awaited.
fn release(
    lowered: &mut Lowered,
    asked: &[Crate],
    build: ContentId,
    session: &mut Session,
    mode: Mode,
) -> Result<(), ApiError> {
    let before = lowered.db().nonce_and_revision();
    lowered.serve_deps(asked, &[], build, &*session.images, mode).map_err(super::lower_error)?;
    let now = lowered.db().nonce_and_revision();
    if now != before && session.walked.at == Some((before.0, before.1, build)) {
        // The release wrote the awaited set alone, which is no input of a key: the keys
        // walked at the view before it stand at the view it leaves.
        session.walked.at = Some((now.0, now.1, build));
    }
    Ok(())
}

/// Each crate of `units` that `keyed` has a key of (an eligible dependency), once, with the lint
/// flags its unit is checked with, as `compile`'s `make` gives them to the check: what its
/// outcome is keyed by. One read from no image is among them, so [`cache::serve`] forgets what
/// it was served under an image it lost.
fn imaged(
    job: &Job,
    units: impl Iterator<Item = usize>,
    keyed: &HashMap<Crate, ContentId, Global>,
) -> Result<Vec<(Crate, Vec<String>)>, ApiError> {
    let mut crates: Vec<(Crate, usize)> = units
        .filter_map(|unit| Some((job.crates.krate(unit)?, unit)))
        .filter(|(krate, _)| keyed.contains_key(krate))
        .collect(); // alloc: once per serve
    crates.sort_by_key(|it| it.0);
    crates.dedup_by_key(|it| it.0);
    crates
        .into_iter()
        .map(|(krate, unit)| Ok((krate, compile::unit_lint_flags(job, unit)?)))
        .collect() // alloc: as above
}

/// What a front end's database keeps of the store for its life: one adapter over it, its goal
/// store ([`goal_store::install`]), its item store ([`body_store::install`], none under
/// `BLAZE_ITEM_STORE=off`) and the dependencies' check outcomes it was served. Dropping it
/// uninstalls both stores.
pub(crate) struct Session {
    installed: goal_store::Installed,
    items: Option<body_store::Installed>,
    images: Arc<Images>,
    checks: Arc<cache::Served>,
    /// Why [`hir_def::deps::store`] wrote no image of each crate it last refused, for the
    /// report of the builds that read the database after ([`served`]).
    refused: HashMap<Crate, String, Global>,
    /// What the session and its serves read of the requests' environment (its stores' modes):
    /// the front end it is kept with serves a later request only while each has the value
    /// read (`super::env_moved`).
    env: EnvReads,
    /// The key ([`hir_def::deps::keys`]) each eligible crate of the database was last served or
    /// resolved under, over every job of the front end it is kept with: [`images`] serves
    /// again exactly the crates whose key moved since.
    keyed: HashMap<Crate, ContentId, Global>,
    /// The crates [`images`] keyed and the database's view they were keyed at, or that the
    /// [`serve`] that served them left: a later serve of those crates at the same view (no
    /// write came between, so each key is the one `keyed` holds) walks nothing.
    walked: Walked,
    /// What the serves' searches of the store came to since the last [`store`], which takes
    /// and says them with the flushes' ([`record_lookups`]).
    lookups: Lookups,
}

/// [`Session::lookups`]: `blaze_record`'s counts of the searches of the read-filing slots, by
/// what was searched for.
#[derive(Default)]
struct Lookups {
    /// The dependency crates' images ([`Served::lookups`]).
    images: blaze_record::Counts,
    /// Their check outcomes and checked bodies ([`cache::Served::lookups`]).
    checks: blaze_record::Counts,
}

/// What each step of the item store is called in a note and an event, by `body_store::Step`
/// as an index.
const ITEM_STEPS: [&str; body_store::Step::COUNT] =
    ["inference", "MIR", "borrow check", "constant"];

/// Says what the lookups through one store kept in `blaze_record`'s form came to, as one kept
/// event `record lookups` in the profile: the `store` (`goals`, `items`, `dep images`,
/// `dep checks`), its `step` where the store keeps its steps apart (an [`ITEM_STEPS`] name,
/// else empty), then `lookups`, `hits`, `lists` (read-lists read) and `reads` (reads valued,
/// each a hash made again), always in that order. Said when nothing was looked up too: a
/// store that was asked nothing says zero, so a count that is missing is a build that did
/// not get here.
fn record_lookups(store: &'static str, step: &'static str, counts: blaze_record::Counts) {
    let blaze_record::Counts { lookups, hits, lists, reads } = counts;
    tracing::info!(target: "blaze", store, step, lookups, hits, lists, reads, "record lookups");
}

/// [`Session::walked`]: the crates keyed at one view of one database by one `build`, each of
/// whose keys (and its dependencies') `keyed` holds; no view while a serve that moved a key is
/// writing.
///
/// Each key is the database's memo (`hir_def::deps`' `crate_image_key`): a walk asks the memo
/// of each crate, which makes a key again only when something the key was made from changed,
/// so the walk after an edit of a workspace member hashes no dependency's files. What is
/// kept here is which crates a serve at this view asked, so a batch asks none twice.
///
/// The walk itself is kept with them (`walk`), so the keys of one view are made once: a
/// build's serves come a batch of units at a time (eight in an edit of ripgrep, each of which
/// walked every crate again from nothing, 52-58 ms on the loop's thread, 425 ms before the
/// edited unit began), and each now walks only the crates no serve at this view asked for.
/// What the kept walk is keyed by is `at`: the database (its nonce), its view (no input was
/// written since; a serve's own writes are no input of a key, which [`stamp`] asserts) and
/// `build`, the code identity of the front end that makes the images. Those are all a key is
/// a function of ([`hir_def::deps::KeyWalk`]), so a walk kept under them is complete (rule
/// 21); when any moves the walk is cleared with the crates.
struct Walked {
    at: Option<(base_db::Nonce, blaze_query::db::View, ContentId)>,
    crates: HashSet<Crate, Global>,
    /// The walk at `at`: every crate a serve there walked, with its key.
    walk: hir_def::deps::KeyWalk,
    /// How many walks [`images`] ran over the session's life: what a test of the skip counts.
    walks: u64,
}

impl Walked {
    fn new() -> Walked {
        Walked {
            at: None,
            crates: HashSet::new_in(Global),
            walk: hir_def::deps::KeyWalk::new(),
            walks: 0,
        }
    }

    /// That each crate walked keys at `db`'s present view by `build` as `keyed` holds it: what
    /// [`stamp`] rests on, that a serve's writes are no input of a key. A write that becomes
    /// one (a file set, a crate's env, a source root) would make every later skip serve stale
    /// images, so it fails here, naming the crate (rule 21).
    #[cfg(debug_assertions)]
    fn assert_stands(&self, db: &Db, build: ContentId, keyed: &HashMap<Crate, ContentId, Global>) {
        let mut crates: Vec<Crate> = self.crates.iter().copied().collect(); // alloc: debug assertions only, once per serve that served
        crates.sort_unstable();
        let now = hir_def::deps::keys(db, &crates, build);
        let mut moved: Vec<(Crate, Option<ContentId>, ContentId)> = now
            .iter()
            .filter(|&(krate, key)| keyed.get(krate) != Some(key))
            .map(|(&krate, &key)| (krate, keyed.get(&krate).copied(), key))
            .collect(); // alloc: debug assertions only, as above
        moved.sort_unstable_by_key(|it| it.0);
        assert!(
            moved.is_empty(),
            "a dependency serve wrote an input of a crate's image key, so the walk it stamps would \
             skip on stale keys (rule 21): (crate, key walked, key now) {moved:?}"
        );
    }

    /// Whether every crate of `crates` was keyed at `at` by `build`.
    fn covers(
        &self,
        at: (base_db::Nonce, blaze_query::db::View),
        build: ContentId,
        crates: &[Crate],
    ) -> bool {
        self.at == Some((at.0, at.1, build)) && crates.iter().all(|it| self.crates.contains(it))
    }

    /// The keys of the eligible crates among `crates` and their dependencies that no walk at
    /// `at` by `build` keyed before, and how many crates the walk kept from those before knew
    /// already. A walk kept at another view, or left by a serve that never stamped its view
    /// (it failed past its images), is forgotten first, with the crates noted there.
    fn keys(
        &mut self,
        db: &Db,
        at: (base_db::Nonce, blaze_query::db::View),
        build: ContentId,
        crates: &[Crate],
    ) -> (usize, HashMap<Crate, ContentId, Global>) {
        if self.at != Some((at.0, at.1, build)) {
            self.forget();
        }
        let known = self.walk.len();
        (known, self.walk.more(db, crates, build))
    }

    /// Nothing is keyed at any view: the next serve walks every crate it asks for.
    fn forget(&mut self) {
        self.at = None;
        self.crates.clear();
        self.walk.clear();
    }

    /// `crates` (those asked and the dependencies keyed with them) keyed at `at` by `build`:
    /// added to those keyed there, or in place of those keyed at another view.
    fn note<'a>(
        &mut self,
        at: (base_db::Nonce, blaze_query::db::View),
        build: ContentId,
        crates: impl Iterator<Item = &'a Crate>,
    ) {
        let at = Some((at.0, at.1, build));
        if self.at != at {
            self.at = at;
            self.crates.clear();
        }
        self.crates.extend(crates.copied());
    }

    /// The view `at` a serve that moved a key left, by `build`: what it walked stands there.
    fn stamp(&mut self, at: (base_db::Nonce, blaze_query::db::View), build: ContentId) {
        if self.at.is_none() {
            self.at = Some((at.0, at.1, build));
        }
    }
}

impl Session {
    /// [`Session::env`].
    pub(crate) fn env_read(&self) -> &EnvReads {
        &self.env
    }

    /// How many walks of the crates' keys its serves ran ([`Walked::walks`]).
    pub(crate) fn walks(&self) -> u64 {
        self.walked.walks
    }
}

/// The session of `lowered`'s database over `store`, its goal store and item store installed
/// before any query of it solves, the item store as the job's `env` asks ([`items_in`]): a
/// database made freshly, once. What a kept expansion read is worked out by the daemon's
/// `macros`.
pub(crate) fn session(
    lowered: &Lowered,
    store: &blaze_cas::Reader,
    macros: &Arc<blaze_macro_eval::Macros>,
    env: &[(String, String)],
) -> Result<Session, ApiError> {
    let mut read = EnvReads::default();
    let items_mode = items_in(&mut read, env)?;
    // alloc: setup, once per database; clone: an `Arc` each
    let paged = pages_in(&mut read, env)?;
    let images = Arc::new(Images::new(store.clone(), Arc::clone(macros)).reading_pages(paged));
    // dyn: the goal store keeps both for the database's life; called once per database
    let adapter: Arc<dyn ImageStore + Send + Sync> = images.clone(); // clone: an `Arc`, once per database
    // dyn: hir-ty's goal store takes its facts as `dyn` (blaze-items depends on hir-ty, which can't name `Goals`); made once per database
    let facts: Arc<dyn goal_store::GoalFacts> = Arc::new(blaze_items::goals::Goals); // alloc: setup, once per database
    let installed = goal_store::install(lowered.db(), adapter, facts, front_end()?)
        .map_err(|why| ApiError::new(format!("the persistent goal cache: {why}"), None))?; // alloc: error path
    let items = match items_mode {
        None => None,
        Some(mode) => {
            // dyn: as the goal store's, kept by the item store for the database's life
            let adapter: Arc<dyn ImageStore + Send + Sync> = images.clone(); // clone: an `Arc`, once per database
            // dyn: hir-ty's item store takes its facts as `dyn` (blaze-item-hash depends on hir-ty); made once per database
            let facts: Arc<dyn body_store::BodyFacts> = Arc::new(blaze_item_hash::Bodies); // alloc: setup, once per database
            Some(body_store::install(lowered.db(), adapter, facts, front_end()?, mode).map_err(
                |why| {
                    ApiError::new(format!("the item store: {why}"), None) // alloc: error path
                },
            )?)
        }
    };
    let checks = Arc::new(cache::Served::default()); // alloc: setup, once per database
    Ok(Session {
        installed,
        items,
        images,
        checks,
        refused: HashMap::new_in(Global),
        env: read,
        keyed: HashMap::new_in(Global),
        walked: Walked::new(),
        lookups: Lookups::default(),
    })
}

/// What the job's `BLAZE_ITEM_STORE` asks of the item store: unset serves what it holds,
/// `verify` runs each step it holds as well and stops the build on a difference, `off`
/// installs none; any other value is an error naming it.
fn items_in(
    read: &mut EnvReads,
    env: &[(String, String)],
) -> Result<Option<body_store::Mode>, ApiError> {
    match read.var(env, ITEMS, "the item store's mode").map_err(|why| ApiError::new(why, None))? {
        None => Ok(Some(body_store::Mode::Serve)),
        Some("verify") => Ok(Some(body_store::Mode::Verify)),
        Some("off") => Ok(None),
        Some(value) => Err(ApiError::new(
            // alloc: error path
            format!(
                "{ITEMS}={value} is not a value blaze takes: `verify` runs each body's inference, MIR and \
                 borrow check the item store holds as well and fails on a difference, `off` uses no item \
                 store, unset reads it"
            ),
            None,
        )),
    }
}

/// The switch of def map pages (docs/v2/plan/design/restore.md, "Def maps read in place"):
/// set to `1`, a dependency's def map is kept as pages beside its image and read from them, a
/// name at a time through the store's pool; unset, the image's def map part is decoded whole,
/// as before. Off until the round's measurements say on; then the image's part goes and this
/// with it. It is a read of the session ([`EnvReads`]), so a request that sets it otherwise
/// lets the kept front end go.
const PAGES: &str = "BLAZE_DEF_MAP_PAGES";

/// What the job's [`PAGES`] asks: unset or `0` reads def maps from their images, `1` from
/// their pages; any other value is an error naming it.
fn pages_in(read: &mut EnvReads, env: &[(String, String)]) -> Result<bool, ApiError> {
    match read.var(env, PAGES, "whether def maps are read from pages").map_err(|why| ApiError::new(why, None))? {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) => Err(ApiError::new(
            // alloc: error path
            format!(
                "{PAGES}={value} is not a value blaze takes: `1` reads a dependency's def map from its \
                 pages, a name at a time; `0` or unset decodes it whole from its image"
            ),
            None,
        )),
    }
}

/// Once a build has ended: the first error the item store met in it (a record that does not
/// open, a store that could not be read, under `verify` a stored result that differs), as the
/// build's error.
pub(crate) fn item_errors(session: &Session) -> Result<(), ApiError> {
    match &session.items {
        Some(items) => items.errors().map_err(|why| ApiError::new(why, None)),
        None => Ok(()),
    }
}

impl Session {
    /// What this build's item store lookups found, step by step; None with no item store.
    pub(crate) fn item_steps(&self) -> Option<[body_store::StepCounts; 4]> {
        self.items.as_ref().map(body_store::Installed::steps)
    }

    /// The dependencies' check outcomes the database was served, for a job's units to read.
    pub(crate) fn checks(&self) -> Arc<cache::Served> {
        Arc::clone(&self.checks)
    }
}

/// What the job's `BLAZE_DEP_IMAGES` asks done with an image: unset serves it, `check`
/// resolves its crate as well, `verify` writes again what was read; any other value is an
/// error naming it.
fn mode_in(read: &mut EnvReads, env: &[(String, String)]) -> Result<Mode, ApiError> {
    match read
        .var(env, IMAGES, "the dependency images' mode")
        .map_err(|why| ApiError::new(why, None))?
    {
        None => Ok(Mode::Serve),
        Some("check") => Ok(Mode::Check),
        Some("verify") => Ok(Mode::Verify),
        Some(value) => Err(ApiError::new(
            format!(
                "{IMAGES}={value} is not a value blaze takes: `check` resolves each dependency read from its \
                 image as well, `verify` writes again what was read, unset reads it"
            ),
            None,
        )),
    }
}

/// Once a build has ended: under `check` or `verify`, each difference its crates showed from
/// their images, as the build's error. Nothing is recorded when serving.
pub(crate) fn differences(kept: &Kept) -> Result<(), ApiError> {
    if kept.mode == Mode::Serve {
        return Ok(());
    }
    let found = hir_def::nameres::frozen::differences();
    if found.is_empty() {
        return Ok(());
    }
    let what = format!(
        "{IMAGES}: {} dependency images differ from what their crates resolve to:\n{}",
        found.len(),
        found.join("\n")
    );
    Err(ApiError::new(what, None))
}

/// What the build a front end was kept from leaves to keep, written from what `lowered`
/// computed, as the puts for the loop to make: the images of the crates `kept` keyed and found
/// none of (a crate whose image would not cover every input is refused by
/// [`hir_def::deps::store`] and resolved again in each build); the outcomes and checked bodies
/// of the dependency crates it checked (`later`, their paths named from `root`), after their
/// dependencies'; the constants' values they evaluated; the goals and bodies' steps offered,
/// with what their flushes did ([`noted`]). A goal or a step its flush would not keep is in
/// what the flush did and stops nothing here ([`Flushes::take_refusal`]). Called on a worker as the front end is kept
/// ([`crate::serve::work::Steps::store_kept`]), which no task reads then, and its puts made
/// durable before the build that kept it answers.
pub(crate) fn store(
    lowered: &Lowered,
    kept: Option<&Kept>,
    session: &mut Session,
    later: &[Later],
    root: &str,
) -> Result<(Vec<KeptPut>, Flushes), ApiError> {
    let images = &*session.images;
    let stored = match kept {
        Some(kept) => Some(lowered.store_deps(&kept.served, images).map_err(super::lower_error)?),
        None => None,
    };
    let db = lowered.db();
    // The front-end records of the toolchain's own crates and of bstd (each crate's image and
    // its def map pages) are handed to the store's collector to pin, as their unit records
    // are (`compile::pin_kept`): kept whatever their last use (docs/gc.md). On use, not only
    // on write: a crate whose image this build found is queued as one it wrote is, so a store
    // filled by an earlier blaze, or imported, has them pinned by the first build that reads
    // them. Queuing a key a pin already names writes nothing (`Collects::build_ended`).
    let found = kept.iter().flat_map(|kept| &kept.served.hits).filter_map(|krate| {
        Some((*krate, cache::image_key(db, *krate)?))
    });
    let wrote = stored.iter().flat_map(|stored| &stored.filed).map(|(krate, filed)| (*krate, *filed));
    // alloc: once per front end stored, one per crate imaged
    let imaged: Vec<(Crate, ContentId)> = found.chain(wrote).collect();
    for &(krate, filed) in &imaged {
        let name = match &krate.data(db).origin {
            base_db::CrateOrigin::Lang(_) => crate::serve::collect::TOOLCHAIN,
            base_db::CrateOrigin::Local { name: Some(name), .. }
            | base_db::CrateOrigin::Library { name, .. }
                if name.as_str() == "bstd" =>
            {
                crate::serve::collect::BSTD
            }
            _ => continue,
        };
        // The crate's front-end records, from the one list of their keys, and the
        // read-filing records its image is found through (a slot that holds nothing names
        // no record, and a pin of it keeps nothing).
        let slots = kept
            .and_then(|kept| kept.served.keys.get(&krate))
            .into_iter()
            .flat_map(|key| crate::serve::collect::filing_keys(*key));
        crate::serve::collect::keep(name, crate::serve::collect::front_keys(filed).chain(slots));
    }
    // Each stage's puts taken as it ends, so each says what it is in an error ([`KeptPut`]).
    let mut puts = Vec::new(); // alloc: once per front end stored, one per value put
    // alloc: once per front end stored, one per image written
    let mut filed: HashMap<ContentId, Crate, Global> = HashMap::new_in(Global);
    if let Some(stored) = &stored {
        filed.extend(stored.filed.iter().map(|(krate, key)| (*key, *krate)));
    }
    let filing: Arc<str> = Arc::from("a read-filing record of a dependency crate's def-map image");
    for put in images.take_puts() {
        let what = match filed.get(&put.key.0) {
            // alloc: once per image put
            Some(krate) => {
                Arc::from(format!("the def-map image of dependency crate {}", krate.named(db)))
            }
            None => Arc::clone(&filing),
        };
        puts.push(KeptPut { put, what });
    }
    // The per-item entries asked since the last store, by whichever query made each: during
    // the serves and the build's tasks. One asked after this is written by the next store.
    let entry: Arc<str> =
        Arc::from("a per-item entry (a file's entry set, a macro definition's source or tokens)");
    puts.extend(
        images.take_entries().into_iter().map(|put| KeptPut { put, what: Arc::clone(&entry) }),
    );
    let mut stage = |what: &'static str| {
        let what: Arc<str> = Arc::from(what);
        puts.extend(
            images.take_puts().into_iter().map(|put| KeptPut { put, what: Arc::clone(&what) }),
        );
    };
    // Each imaged crate's impl tables (`hir_ty::method_resolution::frozen`), beside its
    // image: the next front end reads the crate's impls from them and keys none. A crate
    // read from tables already writes nothing. One whose tables do not encode has none kept,
    // and every front end maintains its impls from its def map: said here, naming it.
    let untabled = hir_ty::method_resolution::frozen::store(db, &imaged, images)
        .map_err(|why| ApiError::new(why, Some(root.to_owned())))?; // alloc: error path
    for (krate, why) in &untabled {
        tracing::warn!(
            target: "blaze",
            krate = %krate.named(db),
            why = %why,
            "a dependency crate's impl tables do not encode: none are kept, and each front end makes them from its def map"
        );
    }
    stage("the impl tables of a dependency crate");
    let checked: Vec<cache::Checked<'_>> = later
        .iter()
        .filter_map(|it| {
            // None: its image was refused, and its outcome is for the same reason.
            let image = cache::image_key(db, it.krate)
                .or_else(|| stored.as_ref()?.filed.get(&it.krate).copied())?;
            Some(cache::Checked {
                krate: it.krate,
                image,
                inputs: cache::Inputs::of(db, &it.flags),
                diags: &it.diags,
                items_broken: it.items_broken,
            })
        })
        .collect(); // alloc: once per front end stored, one per crate checked
    let error = |why: String| ApiError::new(why, Some(root.to_owned())); // alloc: error path
    let kept_checks = cache::store(db, &checked, &session.checks, root, images).map_err(error)?;
    stage("a check outcome or checked bodies of a dependency crate checked in this build");
    let mut bases = session.checks.keys.clone(); // clone: once per front end stored
    bases.extend(kept_checks.keys);
    hir_ty::consteval::deps::drain(db, &bases).map_err(error)?;
    stage("a constant's value a dependency crate evaluated in this build");
    let goals = session
        .installed
        .flush()
        .map_err(|why| error(format!("the persistent goal cache: {why}")))?; // alloc: error path
    stage("a goal of the persistent goal cache");
    let items =
        session.items.as_ref().map(body_store::Installed::flush).transpose().map_err(error)?;
    stage("a body's steps of the item store");
    if let Some(items) = &items {
        // In the profile, not only under `-v`: after a restart every body's inference and MIR
        // should be a hit here, and a build where they ran says by these counts whether
        // nothing was stored (`ran` with no refusal), the step was kept out (`refused`, the
        // read named most first) or the body has no identity (restore.md, "What an edit
        // needs").
        let [infer, mir, borrowck, ..] = &items.steps;
        let (read, bodies) = items.refused.first().map_or(("", 0), |&(read, bodies)| (read, bodies));
        tracing::info!(
            target: "blaze",
            infer_hits = infer.hits,
            infer_ran = infer.ran,
            infer_refused = infer.refused,
            mir_hits = mir.hits,
            mir_ran = mir.ran,
            mir_refused = mir.refused,
            borrowck_hits = borrowck.hits,
            borrowck_ran = borrowck.ran,
            borrowck_refused = borrowck.refused,
            written = items.written as u64,
            kept = items.kept as u64,
            slots_full = items.slots_full as u64,
            no_identity = items.no_identity,
            unshareable = items.unshareable,
            unencodable = items.unencodable,
            dropped = items.dropped,
            most_refused_read = read,
            most_refused_bodies = bodies,
            "item store flushed"
        );
    }
    // What the lookups through each store came to, in the profile (`record lookups`): the
    // two stores' since their last flush, and the serves' since the last store. A build
    // whose front end is not stored leaves its counts to the next that is.
    record_lookups("goals", "", goals.lookups);
    if let Some(items) = &items {
        for (step, counts) in ITEM_STEPS.into_iter().zip(items.lookups) {
            record_lookups("items", step, counts);
        }
    }
    let serves = std::mem::take(&mut session.lookups);
    record_lookups("dep images", "", serves.images);
    record_lookups("dep checks", "", serves.checks);
    if let Some(stored) = &stored {
        for krate in &stored.written {
            session.refused.remove(krate);
        }
        // alloc: once per front end stored, one per crate refused an image
        session.refused.extend(stored.refused.iter().map(|(krate, why)| (*krate, why.to_string())));
    }
    Ok((puts, Flushes { goals, items }))
}

/// What [`store`]'s flushes did: the goal store's, and the item store's when one is installed.
pub(crate) struct Flushes {
    pub(crate) goals: goal_store::Flushed,
    pub(crate) items: Option<body_store::Flushed>,
}

impl Flushes {
    /// What the flushes would not keep, taken, to be said once: `None` when each kept every
    /// goal and body's step it was given. Such a record is a bug in a key or in the store
    /// (keys-cover-every-input), so it is said as a loss of the build's
    /// ([`crate::serve::work::Steps::kept_refusal`]); it is one record's all the same, and
    /// everything else the build left was kept and its front end with it
    /// (panics-fail-one-job): until 2026-11-10 one such step stopped [`store`], which lost
    /// every image, outcome, goal and step of the build and dropped its front end, so the
    /// checkout's next build lowered anew.
    pub(crate) fn take_refusal(&mut self) -> Option<String> {
        let items = self.items.as_mut().and_then(|it| Some((it.failed, it.failure.take()?)));
        let goals = self.goals.failure.take().map(|why| (self.goals.failed, why));
        let said = |what: &str, (failed, why): (usize, String)| {
            // alloc: a loss, once
            format!(
                "{what} did not keep {failed} of the records this build offered (every other was kept, and the \
                 front end with them), the first: {why}"
            )
        };
        match (items, goals) {
            (None, None) => None,
            (Some(items), None) => Some(said("the item store", items)),
            (None, Some(goals)) => Some(said("the persistent goal cache", goals)),
            (Some(items), Some(goals)) => Some(format!(
                "{}; {}",
                said("the item store", items),
                said("the persistent goal cache", goals)
            )), // alloc: a loss, once
        }
    }
}

/// The `-v` note of a build: what [`store`]'s flushes did for the head's last front end it
/// lowered into (`flushes`: the goals' ([`flushed`]), then the bodies' steps the head's last
/// build offered, and why any were kept out), then what this build's item store lookups
/// found, step by step (`steps`, from [`Session::item_steps`]). None when there is neither.
pub(crate) fn noted(
    flushes: Option<&Flushes>,
    steps: Option<&[body_store::StepCounts; 4]>,
) -> Option<Diagnostic> {
    let note = flushes.map(|it| flushed(&it.goals));
    let mut message = String::new();
    if let Some(items) = flushes.and_then(|it| it.items.as_ref()) {
        message.push_str("; ");
        items_flushed(items, &mut message);
    }
    if let Some(steps) = steps {
        message.push_str("; the item store, this build:");
        for (at, (counts, step)) in steps.iter().zip(ITEM_STEPS).enumerate() {
            let body_store::StepCounts { hits, ran, refused } = counts;
            let sep = if at == 0 { "" } else { ";" };
            message.push_str(&format!("{sep} {step} {hits} hit, {ran} ran, {refused} refused")); // alloc: under `-v`, once per build
        }
    }
    let mut note = match note {
        Some(note) => note,
        None if message.is_empty() => return None,
        None => Diagnostic {
            level: Level::Note,
            code: None,
            rendered: String::new(),
            message: String::new(),
            spans: Vec::new(),    // alloc: none
            children: Vec::new(), // alloc: none
            facts: Vec::new(),    // alloc: none
            unit: None,
            explanation: None,
        },
    };
    let message = match note.message.is_empty() {
        true => message.trim_start_matches("; "),
        false => message.as_str(),
    };
    note.message.push_str(message);
    note.rendered = format!("note: {}\n", note.message); // alloc: under `-v`, once per build
    Some(note)
}

/// What a flush says of a chain at its bound, written after `message`'s end: nothing when no
/// lookup met one and every shape was placed.
fn full_chain(lookups_full: usize, full: Option<&str>, message: &mut String) {
    if let Some(full) = full {
        // alloc: under `-v`, once per build that stored a front end
        message.push_str(&format!(" (the first: {full})"));
    }
    if lookups_full > 0 {
        // alloc: as above
        message.push_str(&format!("; {lookups_full} lookups met a chain with every slot full"));
    }
}

/// What the item store's flush did, written after `message`'s end.
fn items_flushed(items: &body_store::Flushed, message: &mut String) {
    let body_store::Flushed {
        written,
        kept,
        slots_full,
        lookups_full,
        lookups,
        full,
        refused,
        unhashable,
        no_identity,
        unshareable,
        unencodable,
        dropped,
        failed,
        failure: _,
        steps: _,
        scans,
    } = items;
    // alloc: under `-v`, once per build that stored a front end
    message.push_str(&format!(
        "the item store: {written} written, {kept} kept, {slots_full} with every slot full"
    ));
    full_chain(*lookups_full, full.as_deref(), message);
    // What the lookups since the last flush came to, step by step, as `blaze_record` counted
    // them: the event `record lookups` has the same.
    let by: Vec<String> =
        ITEM_STEPS.iter().zip(lookups).map(|(step, counts)| format!("{step} ({counts})")).collect(); // alloc: as above
    message.push_str(&format!("; lookups by step: {}", by.join(", "))); // alloc: as above
    if !scans.is_empty() {
        // Who read a whole scope (a listing, not a lookup of a name): each is a read no finer
        // than a module, so the rows that do it are named.
        let by: Vec<String> =
            scans.iter().map(|(step, row, count)| format!("`{row}` in {step} ({count})")).collect(); // alloc: as above
        message.push_str(&format!("; whole scopes read by {}", by.join(", "))); // alloc: as above
    }
    if *failed > 0 {
        // alloc: as above
        message.push_str(&format!("; {failed} the store would not keep (the daemon's log says which)"));
    }
    if !refused.is_empty() {
        let by: Vec<String> =
            refused.iter().map(|(read, count)| format!("`{read}` ({count})")).collect(); // alloc: as above
        message.push_str(&format!("; steps kept out by reads of {}", by.join(", "))); // alloc: as above
    }
    for (count, why) in [
        (unhashable, "with a fact that could not be hashed"),
        (no_identity, "of bodies with no identity"),
        (unshareable, "whose reads could not be captured"),
        (unencodable, "with no stored form"),
        (dropped, "dropped as their thread's buffer was full"),
    ] {
        if *count > 0 {
            message.push_str(&format!("; {count} {why}")); // alloc: as above
        }
    }
}

/// The `-v` note of what [`store`]'s flush of the persistent goal cache did with the goals the
/// head's last build offered, and why any were kept out.
pub(crate) fn flushed(flushed: &goal_store::Flushed) -> Diagnostic {
    let goal_store::Flushed {
        written,
        kept,
        slots_full,
        lookups_full,
        lookups,
        full,
        inexpressible,
        unhashable,
        too_long,
        dropped,
        failed,
        failure: _,
    } = flushed;
    // alloc: under `-v`, once per build that stored a front end
    let mut message = format!(
        "the persistent goal cache, of the last build: {written} written, {kept} kept, {slots_full} with every slot full"
    );
    full_chain(*lookups_full, full.as_deref(), &mut message);
    // What the lookups since the last flush came to, as `blaze_record` counted them: the
    // event `record lookups` has the same.
    message.push_str(&format!("; {lookups}")); // alloc: as above
    if !inexpressible.is_empty() {
        let by: Vec<String> =
            inexpressible.iter().map(|(query, count)| format!("`{query}` ({count})")).collect(); // alloc: as above
        message.push_str(&format!("; goals kept out by {}", by.join(", "))); // alloc: as above
    }
    for (count, why) in [
        (unhashable, "with a fact that could not be hashed"),
        (too_long, "too long to keep"),
        (dropped, "dropped as their thread's buffer was full"),
    ] {
        if *count > 0 {
            message.push_str(&format!("; {count} {why}")); // alloc: as above
        }
    }
    if *failed > 0 {
        // alloc: as above
        message.push_str(&format!("; {failed} the store would not keep (the daemon's log says which)"));
    }
    Diagnostic::note(message)
}

/// What `job`'s build was served of each of its library crates that is no workspace member,
/// once each, sorted by cargo package name ([`blaze_api::Worked::served`]): its check outcome
/// and its bodies each served from `session`'s store, checked this build, or refused, with
/// why. A crate read from no image is refused both, with the reason [`hir_def::deps::store`]
/// last gave for it, or [`cache::Refused::NoImage`]'s when it has given none.
pub(crate) fn served(db: &Db, job: &Job, session: &Session) -> Result<Vec<ServedCrate>, ApiError> {
    let later = job.later.lock();
    // alloc: once per build, one entry per dependency crate checked
    let outcomes: HashMap<Crate, Option<&str>, Global> =
        later.iter().map(|it| (it.krate, it.refused.as_deref())).collect();
    let mut seen = HashSet::new_in(Global);
    let mut lines = Vec::new(); // alloc: once per build, one line per dependency library
    for unit in 0..job.graph.len() {
        let Some(krate) = job.crates.krate(unit) else { continue };
        let u = job.unit(unit)?;
        if !u.target.is_lib() || !seen.insert(krate) {
            continue;
        }
        let package = job.package(u)?;
        if package.is_member {
            continue;
        }
        let package = package.name.clone(); // clone: once per line
        let line = match cache::image_key(db, krate) {
            None => {
                let why = match session.refused.get(&krate) {
                    Some(why) => why.clone(), // clone: once per line refused
                    None => cache::Refused::NoImage.to_string(),
                };
                ServedCrate {
                    package,
                    outcome: ServedPart::Refused,
                    bodies: ServedPart::Refused,
                    why: Some(why),
                }
            }
            Some(_) => {
                // What is not served of a crate built as its programs reach it is not checked either.
                let reached = super::compile::reached_only(job, u)?;
                let unserved = if reached { ServedPart::Refused } else { ServedPart::Checked };
                let refused = outcomes.get(&krate).copied().flatten();
                let outcome = match (session.checks.has(krate), refused) {
                    (true, _) => ServedPart::Served,
                    (false, Some(_)) => ServedPart::Refused,
                    (false, None) => unserved,
                };
                let bodies = match session.checks.keys.contains_key(&krate) {
                    true => ServedPart::Served,
                    false => unserved,
                };
                let why = match refused {
                    Some(why) => Some(why.to_owned()), // alloc: once per line refused
                    None if reached
                        && (outcome, bodies) != (ServedPart::Served, ServedPart::Served) =>
                    {
                        Some(super::compile::REACHED_ONLY.to_owned()) // alloc: as above
                    }
                    None => None,
                };
                ServedCrate { package, outcome, bodies, why }
            }
        };
        lines.push(line);
    }
    lines.sort_by(|a, b| a.package.cmp(&b.package));
    Ok(lines)
}

/// What images, the check outcomes, bodies and constants served beside them, and the goal and
/// item stores are keyed by: the content identity of the code that computes them and of how it
/// was built ([`crate::serve::front_end_code`], `blaze_codegen_code::FRONT_END`), which decides
/// what a crate resolves to, what a goal answers and what a body holds. Never a constant: a
/// fixed name-resolution bug must not go on serving the def maps the bug made; and never the
/// executable's bytes, so that an edit to the code generator or the CLI keeps them. The goal
/// and item stores' solver version is of this identity alone: hir-ty and the vendored trait
/// solver are in its closure, so a solver edit (in `src/` or a patch) misses both stores.
pub(crate) fn front_end() -> Result<ContentId, ApiError> {
    crate::serve::front_end_code().map(|it| it.0).map_err(|why| ApiError::new(why, None))
}

/// What makes unit records, by the code identity of each part ([`crate::serve::RecordMakers`]):
/// the front end's ([`front_end`]), the code generator's and that of blaze-cargo's closure past
/// the packages `blaze_codegen_code::RECORD` cuts. A record is made by blaze-cargo itself (its
/// plan, its flags, the lints', the code generator's, the linker's and the front end's outputs),
/// so its key takes all three; never the executable's bytes, so that an edit of blaze-engine or
/// of a cut package (the explorer, the plane's history, the client's rendering) keeps every
/// record. A record miss reruns the unit's check or compile against the images and stores, which
/// [`front_end`] keeps. The standard library's shared directory is keyed by them too, as it holds
/// std units' records and outputs alone.
pub(crate) fn record_makers() -> Result<crate::serve::RecordMakers, ApiError> {
    crate::serve::record_makers().map_err(|why| ApiError::new(why, None))
}

/// The store as [`hir_def::deps`] and the goal store read and write it.
struct Images {
    store: blaze_cas::Reader,
    /// The daemon's proc macros, whose kept expansions' reads [`ImageStore::reads_now`] works
    /// out again.
    macros: Arc<blaze_macro_eval::Macros>,
    /// What it asked to keep; a write path, once per crate it images and per goal and body's
    /// step a flush writes.
    puts: Lock<Puts>,
    /// The per-item entries it was asked to keep ([`hir_def::nameres::frozen::entries`]), apart
    /// from `puts`: an entry is kept by whichever query made it, during a serve or a build's
    /// tasks, where `puts` is written only by a flush and taken stage by stage.
    entries: Lock<Puts>,
    /// Whether def maps are kept as pages and read from them ([`PAGES`]).
    pages: bool,
}

/// What an [`Images`] was asked to keep since the last take: the puts in the order asked, and
/// where each key's is. A flush asks a put for the shape and the answer of every goal and
/// body's step a build offered (a few hundred thousand of a real workspace), and each put
/// looks for its key among those asked before: by the index, never a scan of them all.
///
/// A get does not read them: a value is in the store once the loop put it, after the flush
/// that asked. A flush that writes two values of one family of keys in turn (two shapes of
/// one base key) keeps what it placed itself (`goal_store::slots`).
struct Puts {
    list: Vec<Put>,
    at: HashMap<ContentId, usize, Global>,
}

impl Images {
    fn new(store: blaze_cas::Reader, macros: Arc<blaze_macro_eval::Macros>) -> Images {
        let puts = Puts { list: Vec::new(), at: HashMap::new_in(Global) }; // alloc: none until a put
        let entries = Puts { list: Vec::new(), at: HashMap::new_in(Global) }; // alloc: none until a put
        Images {
            store,
            macros,
            puts: Lock::new("serve.deps.images.puts", puts),
            entries: Lock::new("serve.deps.images.entries", entries),
            pages: false,
        }
    }

    /// These images, def maps kept as pages and read from them when `on` ([`PAGES`]).
    fn reading_pages(mut self, on: bool) -> Images {
        self.pages = on;
        self
    }

    /// What it was asked to keep since the last take, in the order asked.
    fn take_puts(&self) -> Vec<Put> {
        let mut puts = self.puts.lock();
        puts.at.clear();
        std::mem::take(&mut puts.list)
    }

    /// The per-item entries it was asked to keep since the last take, in the order asked.
    fn take_entries(&self) -> Vec<Put> {
        let mut puts = self.entries.lock();
        puts.at.clear();
        std::mem::take(&mut puts.list)
    }
}

impl Images {
    /// Asks the loop, in `into`, to keep `value` under `key` as a query's record: nothing when the store
    /// or an earlier ask of this flush holds these bytes under it, and an error naming the
    /// key when either holds other bytes (two values under one key: the key misses an input),
    /// saying how the two differ (`entries::differ`).
    /// The bytes are copied only when the put is new.
    fn ask(
        &self,
        into: &Lock<Puts>,
        key: ContentId,
        value: std::borrow::Cow<'_, [u8]>,
    ) -> Result<(), String> {
        use hir_def::nameres::frozen::entries::differ;
        let guard = blaze_cas::enter();
        match self.store.get(&guard, &Key(key)) {
            Ok(Some(entry)) if entry.bytes[..] == *value => return Ok(()),
            Ok(Some(entry)) => {
                let differ = differ(&entry.bytes[..], &value);
                // alloc: error path
                return Err(format!("the store holds other bytes under {key:?}: {differ}"));
            }
            Ok(None) => {}
            Err(corrupt) => return Err(corrupt.to_string()),
        }
        let mut puts = into.lock();
        let Puts { list, at } = &mut *puts;
        match at.get(&key).copied() {
            Some(asked) if list[asked].bytes[..] == *value => {}
            Some(asked) => {
                let differ = differ(&list[asked].bytes[..], &value);
                // alloc: error path
                return Err(format!("two values were made under {key:?} in this build: {differ}"));
            }
            None => {
                at.insert(key, list.len()); // alloc: the index, one entry per put of a flush
                // alloc: the put's bytes, until the loop writes them
                list.push(Put { key: Key(key), kind: Kind::Query, bytes: value.into_owned() });
            }
        }
        Ok(())
    }
}

/// Per-item entries (`hir_def::nameres::frozen::entries`: a file's entry set, a macro
/// definition's source and tokens) as plain values of the store, each under its own key as a
/// query's record. `set` is not used: an entry is found by its key alone, so one whose set
/// moved is found as before.
///
/// As a get of an image, a get does not read the puts asked and not yet written: an entry
/// asked twice before the store that writes it is made twice, to the same bytes
/// (`Images::ask` checks). Entries are asked into a list of their own (`Images::entries`),
/// which [`store`] takes once a build.
impl hir_def::nameres::frozen::entries::Entries for Images {
    // dyn: hir-def's reader of one entry, once per read
    fn get(
        &self,
        _set: &ContentId,
        key: &ContentId,
        with: &mut dyn FnMut(&[u8]),
    ) -> Result<bool, String> {
        let guard = blaze_cas::enter();
        match self.store.get(&guard, &Key(*key)) {
            Ok(Some(entry)) => {
                with(&entry.bytes[..]);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn put(&self, _set: &ContentId, key: ContentId, bytes: &[u8]) -> Result<(), String> {
        self.ask(&self.entries, key, std::borrow::Cow::Borrowed(bytes))
    }
}

/// Has `lowered`'s database keep and find its per-item entries in `session`'s store
/// (`hir_def::nameres::frozen::entries::set`): once, for a database made freshly, before any
/// crate of it is served. A write, as [`Lowered::ran`] is.
pub(crate) fn entries(lowered: &mut Lowered, session: &Session) {
    // clone: an `Arc`, once per database; dyn: the database's one holder of entries
    let held: Arc<dyn hir_def::nameres::frozen::entries::Entries> = session.images.clone();
    hir_def::nameres::frozen::entries::set(lowered.served_db(), held);
}

/// A value of the store held in place past the read ([`blaze_cas::Reader::pinned`]): verified
/// against its content hash, at a 16-byte boundary, and unmoved while it lives.
struct InStore(blaze_cas::Pinned);

impl Held for InStore {
    #[inline]
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A record of the store read a part at a time ([`blaze_cas::Reader::part`]): each read pins
/// the pages its part is on for as long as the read runs, and nothing between reads, so the
/// pool's sweep and its idle give-back take the pages no reader came back to.
struct InPool {
    store: blaze_cas::Reader,
    key: Key,
    len: u64,
    /// Keeps the record from the store's collector while a database reads it: its flags of
    /// use last two periods and its pack's age an hour, and a database may read no part for
    /// longer. Taken as the record is found, which is during the build that serves its crate
    /// (`blaze_cas::collect::Keep`'s rule).
    _kept: blaze_cas::collect::Keep,
}

impl hir_def::deps::PageSource for InPool {
    fn size(&self) -> u64 {
        self.len
    }

    // dyn: hir-def's reader of one segment, once per read
    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        let guard = blaze_cas::enter();
        match self.store.part(&guard, &self.key, at, len) {
            Ok(Some(bytes)) => {
                with(bytes);
                Ok(())
            }
            // A record that was there when its crate was served and is not now, though this
            // holds a keep of it: a bug in the collector or the keep. Said, never read as empty.
            // alloc: error path
            Ok(None) => Err(format!("the store no longer holds {:?}", self.key.0)),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

impl ImageStore for Images {
    fn paged(&self) -> bool {
        self.pages
    }

    // dyn: hir-def's holder of one record, once per crate per front end
    fn pages(&self, key: &ContentId) -> Result<Option<Arc<dyn hir_def::deps::PageSource>>, String> {
        let guard = blaze_cas::enter();
        // The one whole read of the record: it is checked against its content hash here, so
        // every part read after is of bytes that were. Its pages are let go with the guard.
        match self.store.get(&guard, &Key(*key)) {
            Ok(Some(entry)) => {
                let len = entry.bytes.len() as u64;
                // clone: a count of the store's reader; alloc: one holder per crate read from pages
                let _kept = self.store.keep(Key(*key));
                Ok(Some(Arc::new(InPool { store: self.store.clone(), key: Key(*key), len, _kept })))
            }
            Ok(None) => Ok(None),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        let guard = blaze_cas::enter();
        match self.store.pinned(&guard, &Key(*key)) {
            // dyn: hir-def's holder of one image, once per crate per front end
            Ok(Some(pinned)) => Ok(Some(Arc::new(InStore(pinned)))),
            Ok(None) => Ok(None),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        self.ask(&self.puts, key, std::borrow::Cow::Owned(value))
    }

    fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String> {
        self.macros.keeps().reads_now(env, shape)
    }

    // dyn: as `ImageStore::for_each`, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        on_pool(count, task);
    }
}

/// `task(at)` for each `at` in `0..count` on the pool, each in a frame of its own, as
/// [`base_db::Db::run_all`] runs a batch: a task that would wait on a query held by this
/// caller or a task below it runs again here, on this thread, after the batch
/// ([`blaze_graph::par::for_each_giving_way`]); the first other panic goes on from here once
/// every task has run.
// dyn: as `ImageStore::for_each`
fn on_pool(count: usize, task: &(dyn Fn(usize) + Sync)) {
    let task = |at: usize| blaze_mem::frame::task(blaze_mem::Tag::Query, |_| task(at));
    let again = blaze_graph::par::for_each_giving_way(count, task, |panic| {
        matches!(
            panic.downcast_ref::<blaze_query::db::Cancelled>(),
            Some(blaze_query::db::Cancelled::WouldWaitOnItself { .. })
        )
    });
    for at in again {
        task(at);
    }
}

#[cfg(test)]
pub(super) mod tests;
