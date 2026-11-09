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
        let Served { keys, hits, stale, build } = more;
        // Every serve of one front end is by one blaze: what `store` makes the keys again with.
        self.served.build = build;
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
pub(crate) fn serve(
    lowered: &mut Lowered,
    job: &Job,
    units: &[u32],
    session: &mut Session,
    kept: &mut Option<Kept>,
) -> Result<(), ApiError> {
    let mode = mode_in(&mut session.env, &job.input.config.invocation.env)?;
    let build = front_end()?;
    // alloc: once per serve, one entry per unit's crate
    let crates: Vec<Crate> =
        units.iter().filter_map(|&unit| job.crates.krate(unit as usize)).collect();
    let Some(served) = images(lowered, &crates, build, session, mode)? else { return Ok(()) };
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
    cache::serve(lowered.served_db(), &asked, &*session.images, checks)
        .map_err(|why| ApiError::new(why, None))?;
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
/// between). A crate whose
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
    if session.walked.covers(at, build, crates) {
        return Ok(None);
    }
    let keys = {
        let _span = tracing::info_span!(target: "blaze", "dependency image keys", crates = crates.len(), subject = ?at.1).entered();
        session.walked.walks += 1;
        hir_def::deps::keys(lowered.db(), crates, build)
    };
    let mut moved: Vec<Crate> = keys
        .iter()
        .filter(|&(krate, key)| session.keyed.get(krate) != Some(key))
        .map(|(&krate, _)| krate)
        .collect(); // alloc: once per serve, one per crate whose key moved
    if moved.is_empty() {
        // `keyed` holds every key walked already.
        session.walked.note(at, build, crates.iter().chain(keys.keys()));
        return Ok(None);
    }
    moved.sort_unstable();
    let served =
        lowered.serve_deps(&moved, build, &*session.images, mode).map_err(super::lower_error)?;
    debug_assert!(session.images.take_puts().is_empty(), "serving puts nothing");
    // The walk is noted only once `keyed` holds its keys: a serve that failed above leaves no
    // record that a later serve at this view could skip on with the old keys (rule 21).
    session.keyed.extend(keys.iter().map(|(&krate, &key)| (krate, key)));
    session.walked.note(at, build, crates.iter().chain(keys.keys()));
    // Written: what was walked stands again only once [`serve`] is done and stamps its view.
    session.walked.at = None;
    let Served { keys, hits, stale, build } = served;
    let of_moved = |krate: &Crate| moved.binary_search(krate).is_ok();
    // alloc: once per serve that moved a key, one entry per crate whose key moved
    Ok(Some(Served {
        keys: keys.into_iter().filter(|(krate, _)| of_moved(krate)).collect(),
        hits: hits.into_iter().filter(of_moved).collect(),
        stale: stale.into_iter().filter(of_moved).collect(),
        build,
    }))
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
}

/// [`Session::walked`]: the crates keyed at one view of one database by one `build`, each of
/// whose keys (and its dependencies') `keyed` holds; no view while a serve that moved a key is
/// writing.
struct Walked {
    at: Option<(base_db::Nonce, blaze_query::db::View, ContentId)>,
    crates: HashSet<Crate, Global>,
    /// How many walks [`images`] ran over the session's life: what a test of the skip counts.
    walks: u64,
}

impl Walked {
    fn new() -> Walked {
        Walked { at: None, crates: HashSet::new_in(Global), walks: 0 }
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
    let images = Arc::new(Images::new(store.clone(), Arc::clone(macros)));
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
    pub(crate) fn item_steps(&self) -> Option<[body_store::StepCounts; 3]> {
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
/// with what their flushes did ([`noted`]). Called on a worker as the front end is kept
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
    let mut stage = |what: &'static str| {
        let what: Arc<str> = Arc::from(what);
        puts.extend(
            images.take_puts().into_iter().map(|put| KeptPut { put, what: Arc::clone(&what) }),
        );
    };
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

/// The `-v` note of a build: what [`store`]'s flushes did for the head's last front end it
/// lowered into (`flushes`: the goals' ([`flushed`]), then the bodies' steps the head's last
/// build offered, and why any were kept out), then what this build's item store lookups
/// found, step by step (`steps`, from [`Session::item_steps`]). None when there is neither.
pub(crate) fn noted(
    flushes: Option<&Flushes>,
    steps: Option<&[body_store::StepCounts; 3]>,
) -> Option<Diagnostic> {
    let note = flushes.map(|it| flushed(&it.goals));
    let mut message = String::new();
    if let Some(items) = flushes.and_then(|it| it.items.as_ref()) {
        message.push_str("; ");
        items_flushed(items, &mut message);
    }
    if let Some(steps) = steps {
        message.push_str("; the item store, this build:");
        for (at, (counts, step)) in
            steps.iter().zip(["inference", "MIR", "borrow check"]).enumerate()
        {
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

/// What the item store's flush did, written after `message`'s end.
fn items_flushed(items: &body_store::Flushed, message: &mut String) {
    let body_store::Flushed {
        written,
        kept,
        slots_full,
        refused,
        unhashable,
        no_identity,
        unshareable,
        unencodable,
        dropped,
        steps: _,
    } = items;
    // alloc: under `-v`, once per build that stored a front end
    message.push_str(&format!(
        "the item store: {written} written, {kept} kept, {slots_full} with every slot full"
    ));
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
        inexpressible,
        unhashable,
        too_long,
        dropped,
    } = flushed;
    // alloc: under `-v`, once per build that stored a front end
    let mut message = format!(
        "the persistent goal cache, of the last build: {written} written, {kept} kept, {slots_full} with every slot full"
    );
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
    /// What it asked to keep, in the order asked; a write path, once per crate it images and
    /// per goal a flush writes.
    puts: Lock<Vec<Put>>,
}

impl Images {
    fn new(store: blaze_cas::Reader, macros: Arc<blaze_macro_eval::Macros>) -> Images {
        Images { store, macros, puts: Lock::new("serve.deps.images.puts", Vec::new()) }
    }

    /// What it was asked to keep since the last take, in the order asked.
    fn take_puts(&self) -> Vec<Put> {
        std::mem::take(&mut *self.puts.lock())
    }
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

impl ImageStore for Images {
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
        let guard = blaze_cas::enter();
        match self.store.get(&guard, &Key(key)) {
            Ok(Some(entry)) if entry.bytes == value.as_slice() => return Ok(()),
            Ok(Some(_)) => return Err(format!("the store holds other bytes under {key:?}")),
            Ok(None) => {}
            Err(corrupt) => return Err(corrupt.to_string()),
        }
        let mut puts = self.puts.lock();
        match puts.iter().find(|it| it.key == Key(key)) {
            Some(put) if put.bytes == value => {}
            Some(_) => return Err(format!("two images were made under {key:?}")),
            None => puts.push(Put { key: Key(key), kind: Kind::Query, bytes: value }),
        }
        Ok(())
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
