//! [`load`]: a proc-macro crate's macros, each an expander that runs the macro's own function
//! ([`run`](crate::run)) and keeps what it gave in the store.
//!
//! A call is keyed by the macro's code (its crate's whole closure and this server's version,
//! [`code`](crate::code)), what it is given (name, kind, input and attribute tokens, spans as
//! shapes, the calling crate's variables and directory, the toolchain: `proc_macro_api::kept::given`),
//! and what it read (files and variables, traced by [`os`](crate::os) and looked up again on
//! a hit). Not kept: one that asked the database about a span, read what can't be traced,
//! emitted a diagnostic, failed, or ran with state from the calls before it. Of these, one
//! that ran with state of a crate that settles, and moved none on, still reports what it read,
//! as this checkout's, once it gave the same from the fresh state as from the settled one and
//! every first call left one settled state (`same_from_either_state`, one more run of it): its
//! output is then what its own inputs give, whichever calls ran before it. A crate that settles
//! keeps only calls so shown; one shown otherwise is told to the store as stateful.
//! Any other run with state has unknown reads: a session starts again each revision while
//! earlier calls may come from a memo, and body-level calls run in no fixed order.
//!
//! Each expansion also tells the front end whether its session's state took part
//! ([`StateUse`], [`state_use`]): not for a stateless crate's call, nor for a call of a crate
//! that settles shown to give the same from either state, whose expansions the database reuses
//! like any other; for every other call of a crate with a session, whose expansion then runs
//! again in each revision, in the order its readers ask, so its session is built again in order.
//!
//! State: a macro whose run changed a static outside the standard library has state between
//! calls, as it has under rustc, where each calling crate's compilation loads it once. Each
//! calling crate's calls of it are a session, in the order they expand, started again with each
//! revision of the database. Statics that hold no address are given to the next call as they
//! were left; when one holds an address, which no fresh run can be given, the session's earlier
//! calls are run again on the same evaluator before the next one.
//!
//! A crate's wasm module ([`load_wasm`]) runs each expansion in a fresh instance given the
//! statics the session's calls before it left, and says which it changed, as the evaluator
//! does (`wasm.rs`); once a static holds an address of the heap, the session's earlier calls
//! run again in order in that instance before the next, as on the evaluator. Its expansions
//! and its verdict are kept apart from the evaluator's, under the module's id.
//!
//! A call is kept and found under the state it started from too: none for a call whose
//! session's calls before it left nothing, else a digest of the statics they left, by item key
//! and bytes ([`start_digest`]). It is kept only when it ran from that state and left it as it
//! was, so a hit is what a run from the same state would give and leaves the session as a run
//! would. A call after the calls before it are run again (a static holds an address) is never
//! kept or found.
//!
//! Whether a crate's macros keep state is learned per engine, and kept in the store
//! (`verdict_run`): unseen, its calls expand in a session (`requires_session`), in
//! declaration order; once a build ran some and none left state ([`Macros::flush`]), it is
//! stateless and its calls run in any order, each from nothing; once a call's first run changed
//! the fresh state and none changed what a call before it left (proc-macro2's one-time
//! `detection` is such), it settles: its calls still run in order, and after a session's
//! first call, each is found under the state that call left; once one changed what a call
//! before it left, it is stateful for good (`proc_macro_api::kept::verdict`).

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use base_db::{Crate, Db, Nonce, ProcMacroLoadingError};
use blaze_graph::Key;
use blaze_hash::SharedMap;
use blaze_ids::ContentId;
use blaze_usercode::{
    RunKey,
    index::{self, FindError, Found},
    roots::{Role, RootMap, Roots, verify},
    trace::{Seen, assemble_seen},
};
use hir_def::{FunctionId, StaticId, nameres::crate_def_map, unstable_features::UnstableFeatures};
use hir_expand::proc_macro::{
    Expanded, ExpansionReads, ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
    ProcMacroKind, ProcMacroLoadResult, StateUse,
};
use blaze_core::lock::{Lock, LockGuard};
use intern::Symbol;
use proc_macro_api::kept::verdict::{
    self as verdicts, Left, SETTLES, STATEFUL, STATELESS, Verdict,
};
use proc_macro_api::kept::{
    Call, Keep, Keeping, Keeps, Mode, Put, Spans, checked, given, put, read, written,
};

use crate::{
    code,
    keep::Calling,
    protocol,
    run::{self, Bridge, Expansion, Ran},
    stream::Stream,
    turn::{Turn, Turns},
    wasm,
};

/// What a daemon's proc macros share: where their expansions are kept, and every library
/// loaded, which [`Macros::flush`] tells the store of. Made once by the daemon and lent to every
/// [`load`] and [`load_wasm`]; two in one process see neither each other's store nor each
/// other's libraries. Its libraries are held weakly, so a library holding it makes no cycle.
pub struct Macros {
    keeps: Keeps,
    /// Where the daemon keeps the packages it fetches (`blaze_manifest::home::sources`): a
    /// read under it is of the same file in every checkout ([`RootMap::with_sources`]). None
    /// where nothing is fetched (a test, a bench): such a read is then this checkout's.
    sources: Option<PathBuf>,
    libraries: Lock<Vec<Weak<Library>>>,
    /// The daemon's wasm modules ([`load_wasm`]'s), made with it: or why Wasmtime made no
    /// engine, which every wasm macro's load then says.
    modules: Result<wasm::Modules, String>,
}

const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Macros>();
};

impl Macros {
    pub fn new(keeps: Keeps, sources: Option<PathBuf>) -> Arc<Macros> {
        // alloc: once per daemon, or per test that loads macros
        Arc::new(Macros {
            keeps,
            sources,
            libraries: Lock::new("macro.libraries", Vec::new()),
            modules: wasm::Modules::new(),
        })
    }

    /// The daemon's wasm modules, which a proc macro built for wasm loads through
    /// ([`load_wasm`]): or why there are none, Wasmtime having made no engine.
    pub fn modules(&self) -> Result<&wasm::Modules, &str> {
        self.modules.as_ref().map_err(String::as_str)
    }

    /// Macros that keep and find nothing, for `why` (a bench, a test of the evaluator alone),
    /// which each expansion that runs says of its reads.
    pub fn nothing(why: &'static str) -> Arc<Macros> {
        Macros::new(Keeps::Nothing(why), None)
    }

    /// Where its expansions are kept.
    pub fn keeps(&self) -> &Keeps {
        &self.keeps
    }

    /// Tells the store of each crate whose macros ran in this build and left no state that it
    /// is stateless: called once a build has expanded everything, on the thread of the store's
    /// owner, which then publishes this thread's puts (`proc_macro_api::kept::take_puts`).
    /// Writes nothing itself and blocks on nothing.
    pub fn flush(&self) {
        if let Keeps::Nothing(_) = self.keeps {
            return;
        }
        let mut libraries = self.libraries.lock();
        libraries.retain(|it| it.strong_count() != 0);
        for library in libraries.iter().filter_map(Weak::upgrade) {
            // Another build that saw one leave state may have told the store since the load.
            if library.seen.flush() && verdict(&self.keeps, &library.runs) == Ok(Verdict::Unseen) {
                tell(&library, STATELESS);
            }
        }
    }
}

impl fmt::Debug for Macros {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keeps = match &self.keeps {
            Keeps::In(keeping) => format!("in a store, {:?}", keeping.mode), // alloc: debug output
            Keeps::Nothing(why) => format!("nothing: {why}"),                // alloc: debug output
        };
        f.debug_struct("Macros")
            .field("keeps", &keeps)
            .field("libraries", &self.libraries.lock().len())
            .finish()
    }
}

/// What the macros of one proc-macro crate share.
struct Library {
    /// The daemon's, which says where this crate's expansions are kept.
    macros: Arc<Macros>,
    krate: Crate,
    /// The crate's name, as an error about one of its macros says it.
    name: Box<str>,
    toolchain: ContentId,
    engine: Engine,
    /// The code as it was at [`load`], which is asked again each time the crate compiles.
    loaded: ContentId,
    /// What its macros run, [`Engine::runs`] of `loaded`: what the key of a crate that calls
    /// them covers ([`ProcMacroExpander::code`]: dependency images, goal facts) and what its
    /// verdict is kept under ([`verdict_run`]).
    runs: ContentId,
    code: code::Memo,
    /// What the store said of the crate's state when it was loaded, and what its expansions
    /// that ran (not found) did since.
    seen: verdicts::Seen,
    /// Each calling crate's session: its calls run in order, each that runs from its state or
    /// changes it holding its turn in [`Library::turns`].
    sessions: SharedMap<Crate, Arc<SessionOf>>,
    /// The turns of its sessions ([`crate::turn`]): scheduler holds, so a call waiting for one
    /// holds no thread while the call holding it runs its macro.
    turns: Turns,
    /// Each calling crate's roots as its expansions are kept against them ([`Library::kept_roots`]).
    kept_roots: SharedMap<Crate, Arc<KeptRoots>>,
    /// For a crate that settles, the settled state its sessions' first calls left, the first
    /// seen since it loaded ([`Evaluated::same_from_either_state`]): another first call that
    /// leaves another state shows the state depends on which call ran first.
    settled: Lock<Option<Settled>>,
}

/// The settled state of a crate that settles: its digest ([`start_digest`]), and the statics,
/// of the database `at` (a static's ID is of its database), to run a call from it.
struct Settled {
    digest: ContentId,
    at: Nonce,
    statics: Vec<(StaticId, Vec<u8>)>,
}

/// A calling crate's roots with their other spellings and machine-wide directories, and what
/// they were made of: the database's revision (a root's links are read again in each), the
/// package and `OUT_DIR` and the passed `CARGO_HOME` and `HOME`. Made again when any differs.
struct KeptRoots {
    at: (Nonce, blaze_query::db::View),
    made_of: [Option<String>; 2],
    map: RootMap,
}

/// What a library's macros run on.
pub(crate) enum Engine {
    /// The MIR evaluator, through the bridge's functions in the crate's `proc_macro`.
    Evaluator(Bridge),
    /// The crate's wasm module ([`load_wasm`]). Each expansion is a fresh instance, given the
    /// statics the calls before it left.
    Wasm(wasm::Library),
}

impl Engine {
    /// What the macros of the crate whose code is `loaded` run on this engine: on the
    /// evaluator, the crate's code, which the front end's code runs (`BLAZE_FRONT_END_CODE` is
    /// in every key beside it); in a wasm module, the crate's code and the module
    /// ([`wasm::Library::runs`]), which blaze's code generator made, and which the front end's
    /// code identity leaves out: a fix of the code generator is another module, and so another
    /// key of every crate that calls its macros.
    pub(crate) fn runs(&self, loaded: ContentId) -> ContentId {
        match self {
            Engine::Evaluator(_) => loaded,
            Engine::Wasm(module) => module.runs(loaded),
        }
    }
}

/// A call that panicked holding a session or the sessions leaves them as they were at the panic,
/// and the next call takes them as they are.
impl std::panic::RefUnwindSafe for Library {}

/// The run a verdict of the crate whose code is `code` (its [`Library::runs`]) is kept
/// under, in the store expansions are kept in, with nothing read. Each verdict has its own key,
/// so none is ever put under a key that holds another, and a crate seen stateful by one build
/// and stateless by another is stateful ([`verdict`]).
fn verdict_run(code: &ContentId, verdict: &[u8]) -> RunKey {
    // 3: a change from the fresh state alone settles, no longer stateful.
    let given = blaze_ids::Hasher::new("whether a proc-macro crate keeps state between calls 3")
        .part(verdict)
        .finish();
    RunKey { code: *code, given }
}

/// Whether the store says `verdict` of the crate whose code is `code`.
fn said(keeping: &Keeping, code: &ContentId, verdict: &[u8]) -> Result<bool, String> {
    let guard = blaze_cas::enter();
    let env = base_db::Env::default();
    match index::find(
        &keeping.expansions.store,
        &guard,
        &verdict_run(code, verdict),
        &mut Calling::new(&env),
    ) {
        Ok(Found::Hit { value, .. }) if value == verdict => Ok(true),
        // alloc: error path
        Ok(Found::Hit { value, .. }) => {
            Err(format!("the store keeps {value:?} as its verdict, not {verdict:?}"))
        }
        Ok(Found::Miss { .. }) => Ok(false),
        // alloc: error path
        Err(FindError::Corrupt(corrupt)) => Err(corrupt.to_string()),
        // alloc: error path
        Err(FindError::NotAShape { key, why }) => {
            Err(format!("the store's {key:?} is not a shape: {why}"))
        }
        // alloc: error path
        Err(FindError::Inputs(unreadable)) => Err(unreadable.to_string()),
        // alloc: error path
        Err(FindError::TooManyShapes { run }) => {
            Err(format!("verdict run {run:?} has more than {} shapes", index::MOST_SHAPES))
        }
    }
}

/// What the store `keeps` says of the crate whose code is `code`. Keeping nothing (a bench, a
/// test that keeps nothing), nothing is known and nothing is found.
fn verdict(keeps: &Keeps, code: &ContentId) -> Result<Verdict, String> {
    let Keeps::In(keeping) = keeps else { return Ok(Verdict::Unseen) };
    if said(keeping, code, STATEFUL)? {
        return Ok(Verdict::Stateful);
    }
    if said(keeping, code, SETTLES)? {
        return Ok(Verdict::Settles);
    }
    Ok(if said(keeping, code, STATELESS)? { Verdict::Stateless } else { Verdict::Unseen })
}

/// Leaves `verdict` of `library` for the store's owner to publish with this thread's puts.
fn tell(library: &Library, verdict: &'static [u8]) {
    if let Keeps::Nothing(_) = library.macros.keeps {
        return;
    }
    put(Put {
        keep: Keep::Run {
            run: verdict_run(&library.runs, verdict),
            traced: blaze_core::Traced::default(),
        },
        value: verdict.to_vec(), // alloc: once per crate per build, a verdict
        // alloc: as above
        what: format!("the verdict of the proc macros of `{}`", library.name),
    });
}

impl Library {
    /// Notes what a run `left`: one that showed more state than the store said is told the
    /// store with the run's puts. One that left state where the store said the crate keeps
    /// none is an error: this build's calls of it ran, or were reused, as if each started from
    /// nothing.
    fn saw(&self, left: Left) -> Result<(), ProcMacroExpansionError> {
        let Some((verdict, contradicted)) = self.seen.saw(left) else { return Ok(()) };
        tell(self, verdict);
        if contradicted {
            // alloc: error path
            return Err(ProcMacroExpansionError::System(format!(
                "the proc macros of `{}` keep state between expansions, which the store said they \
                 did not: this build's expansions of them may not be what rustc gives; they expand \
                 in a session from the next load",
                self.name
            )));
        }
        Ok(())
    }

    /// The session of `calling` in the database's revision now, started again when it was of
    /// another.
    fn session(&self, db: &Db, calling: Crate) -> Arc<SessionOf> {
        let now = db.nonce_and_revision();
        // clone: the session's handle, held for one call
        let session = Arc::clone(self.sessions.pin().get_or_insert_with(calling, SessionOf::new));
        {
            let mut held = session.lock();
            if held.at != Some(now) {
                *held = Session { at: Some(now), ..Session::default() };
            }
        }
        session
    }

    /// The turn of `session`, `calling`'s, for a call that runs from its state or changes it:
    /// waited for through the scheduler while another call holds it ([`crate::turn`]), the
    /// wait naming the session. An error says why it was not given: a call of the session asked
    /// for inside the run of another on the same job, or one starved of its turn.
    fn turn<'s>(
        &'s self,
        db: &Db,
        calling: Crate,
        session: &'s SessionOf,
    ) -> Result<SessionTurn<'s>, ProcMacroExpansionError> {
        // alloc: once per call that holds a session's turn, the name its waits and errors say
        let said = format!("the proc-macro session of `{}` for `{}`", self.name, calling.named(db));
        let turn = self
            .turns
            .take(session.turn, &mut |it| it.push_str(&said))
            .map_err(|why| system(why.said(&said)))?;
        let mut held = session.lock();
        let calls = std::mem::take(&mut held.calls);
        Ok(SessionTurn { session, at: held.at, calls, _turn: turn })
    }

    /// The roots `calling`'s expansion with `roots` is kept against, given `passed`: made once
    /// per calling crate and revision ([`KeptRoots`]), not once per expansion, as making them
    /// reads each root's links.
    fn kept_roots(&self, db: &Db, calling: Crate, roots: Roots<'_>) -> Arc<KeptRoots> {
        let at = db.nonce_and_revision();
        // The sources directory is the daemon's, the same for every call: not in `made_of`.
        let made_of = [roots.get(Role::Package), roots.get(Role::OutDir)];
        let same = |kept: &KeptRoots| {
            kept.at == at && kept.made_of.iter().map(Option::as_deref).eq(made_of.iter().copied())
        };
        let pinned = self.kept_roots.pin();
        if let Some(kept) = pinned.get(&calling).filter(|it| same(it)) {
            return Arc::clone(kept); // clone: the roots' handle, held for one expansion
        }
        let kept = Arc::new(KeptRoots {
            // alloc: once per calling crate and revision
            at,
            made_of: made_of.map(|it| it.map(str::to_owned)), // alloc: as above
            map: roots.to_map(|map| match &self.macros.sources {
                Some(sources) => map.with_sources(sources),
                None => map,
            }),
        });
        pinned.insert(calling, Arc::clone(&kept)); // clone: as above
        kept
    }
}

impl fmt::Debug for Library {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Library")
            .field("name", &self.name)
            .field("loaded", &self.loaded)
            .finish_non_exhaustive()
    }
}

/// One calling crate's session of a library: its state, and what its turn is held under.
struct SessionOf {
    /// Locked only to read or write its fields, never across a run, a query or any other wait
    /// (standard-structures, no-wait-across-thread-locals): a call that runs from the state or
    /// changes it holds the session's turn ([`Library::turn`]) instead.
    state: Lock<Session>,
    /// The session's turn's key in [`Library::turns`]: one of the process's, from a count.
    turn: Key,
}

impl SessionOf {
    fn new() -> Arc<SessionOf> {
        static SESSIONS: AtomicU64 = AtomicU64::new(0);
        let mut turn = [0u8; 32];
        turn[..8].copy_from_slice(&SESSIONS.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        turn[8..].copy_from_slice(b"a macro session's turn\0\0");
        // alloc: once per calling crate of a proc-macro crate
        Arc::new(SessionOf { state: Lock::new("macro.session.state", Session::default()), turn: Key(turn) })
    }

    /// Its state, for a field's read or write: never held across a wait.
    fn lock(&self) -> LockGuard<'_, Session> {
        self.state.lock()
    }
}

/// A call's hold of its session's turn ([`Library::turn`]). The session's calls (what runs
/// again before each next call when a static holds an address) are taken out of it for the
/// turn, as only the turn's holder runs them or adds to them, and put back as it ends, also on
/// an unwind, unless the session was started again for another revision meanwhile.
struct SessionTurn<'s> {
    session: &'s SessionOf,
    /// The revision the session was of when the turn was taken.
    at: Option<(Nonce, blaze_query::db::View)>,
    calls: Vec<Kept>,
    // Dropped after `drop` has put the calls back: the next holder finds them there.
    _turn: Turn<'s>,
}

impl SessionTurn<'_> {
    /// The session's state for a field's update, `None` when it was started again for another
    /// revision since the turn was taken: nothing of this turn's call is kept in that one.
    fn update(&self) -> Option<LockGuard<'_, Session>> {
        let held = self.session.lock();
        (held.at == self.at).then_some(held)
    }
}

impl Drop for SessionTurn<'_> {
    fn drop(&mut self) {
        let calls = std::mem::take(&mut self.calls);
        if let Some(mut held) = self.update() {
            held.calls = calls;
        }
    }
}

/// One calling crate's calls of a library's macros in one revision.
#[derive(Default)]
struct Session {
    at: Option<(Nonce, blaze_query::db::View)>,
    /// The statics the calls so far left that hold no address.
    statics: Vec<(StaticId, Vec<u8>)>,
    /// Whether a static they left holds an address: then [`Session::calls`] are run again.
    holds: bool,
    /// The calls so far, while [`Session::holds`] can still become true or is.
    calls: Vec<Kept>,
    /// The calls run in this revision, at their turn.
    taken: usize,
    /// Calls run ahead of their turn ([`ProcMacroExpander::speculate`]), each from the statics
    /// it names, for its turn to take if the session is then in that state.
    ahead: Vec<Ahead>,
    /// Whether a call's turn found the session moved on from the state its run ahead started
    /// in: then no call of this session runs ahead again in this revision, since each would
    /// likely run twice.
    serial: bool,
}

/// What a call run ahead of its turn gave: its run's result, or the panic it unwound with,
/// which its turn unwinds with as a run at its turn would have.
type AheadRan = Result<Result<Ran, ProcMacroExpansionError>, Box<dyn std::any::Any + Send>>; // dyn: a panic's payload, as `catch_unwind` gives it

/// A call run ahead of its turn, from `preset`.
struct Ahead {
    this: Kept,
    preset: Vec<(StaticId, Vec<u8>)>,
    ran: AheadRan,
}

impl Session {
    /// What `this`'s run ahead gave, when it ran from the statics the session has now: then
    /// what its turn gives is what a run at its turn gives. A run ahead from other statics is
    /// dropped, and no call of the session runs ahead again in this revision.
    fn take_ahead(&mut self, this: &Kept) -> Option<AheadRan> {
        if self.holds {
            return None;
        }
        let at = self.ahead.iter().position(|it| it.this == *this)?;
        let ahead = self.ahead.swap_remove(at);
        if ahead.preset == self.statics {
            return Some(ahead.ran);
        }
        self.serial = true;
        self.ahead.clear();
        None
    }
}

/// The state a call ran from: after its session's calls before it run again (`replayed`), or
/// the statics they left ([`start_digest`]).
#[derive(Clone, Copy)]
struct From {
    replayed: bool,
    start: Option<ContentId>,
}

/// Why what a call of a crate whose session's state may matter read is unknown: unseen, or
/// stateful.
const UNSETTLED: &str = "its macro keeps a session that does not settle: what it gives may depend on the calls before it";

/// Why what a call of a crate that settles gave may depend on its session's state: it was not
/// shown to give the same from the fresh state as from the settled one.
const UNPROVEN: &str = "its macro's crate settles, and this call was not shown to give the same from the fresh state as from the settled one: what it gives may depend on whether it ran first";

/// Whether what a call of a crate with `verdict` gave depends on its session's state, as the
/// front end is told it ([`StateUse`]): never for a stateless crate; for one that settles, not
/// once `proven` (the call gave the same from either state, [`Evaluated::same_from_either_state`],
/// or it was found under a key kept only for such calls); always for a crate whose state is not
/// known or keeps moving, whose call may give otherwise after other calls before it.
fn state_use(verdict: Verdict, proven: bool) -> StateUse {
    match verdict {
        Verdict::Stateless => StateUse::Fixed,
        Verdict::Settles if proven => StateUse::Fixed,
        Verdict::Settles => StateUse::Session(UNPROVEN),
        Verdict::Unseen | Verdict::Stateful => StateUse::Session(UNSETTLED),
    }
}

/// What a run of a session did, for [`session_reads_known`].
#[derive(Clone, Copy, Debug)]
struct SessionRun {
    /// It ran after its session's calls before it again.
    replayed: bool,
    /// It ran from the fresh state: the session it ran in had no state yet.
    fresh: bool,
    /// It changed a static.
    changed: bool,
    /// A static it left holds an address.
    left_address: bool,
}

/// Whether what a run with session state read is known: whether its output is what its own
/// inputs give whatever ran before it. Only for a crate that settles (`verdict`, the store's
/// at load: the first call's change of the fresh state is the only change), and only for a
/// run that moved no state on ([`Left::Moved`], as `run` tells it the store: it ran after its
/// calls before it again, or changed a state they left, or left an address). A session starts
/// again each revision while the calls before a run may come from the database's memo, and
/// body-level calls run one at a time in no fixed order, so for any other crate the state a
/// run starts from is not fixed by the calling crate's inputs: a stateful crate's call that
/// ran from the fresh state after a write (its calls before it from the memo) gives what it
/// would as the first call, not what rustc gives.
fn session_reads_known(verdict: Verdict, run: SessionRun) -> bool {
    let moved = run.replayed || (run.changed && !(run.fresh && !run.left_address));
    verdict == Verdict::Settles && !moved
}

/// A digest of the statics `statics` a session's calls left, each by its item key and its
/// bytes, in item-key order: `None` for none, the fresh state, whose keys say no state.
fn start_digest(db: &Db, statics: &[(StaticId, Vec<u8>)]) -> Option<ContentId> {
    if statics.is_empty() {
        return None;
    }
    // alloc: once per call with state, its statics in key order
    let mut keyed: Vec<(blaze_items::ItemKey, &[u8])> = statics
        .iter()
        .map(|(id, bytes)| {
            (blaze_items::item_key(db, blaze_items::ItemId::Static(*id)), bytes.as_slice())
        })
        .collect();
    keyed.sort_unstable_by(|a, b| a.0.0.0.cmp(&b.0.0.0));
    let mut h = blaze_ids::Hasher::new("the statics a proc macro's session left 1");
    for (key, bytes) in keyed {
        h.id(&key.0).part(bytes);
    }
    Some(h.finish())
}

/// A call kept for running it again.
#[derive(PartialEq)]
struct Kept {
    function: FunctionId,
    /// The macro's place in its crate's `__blaze_proc_macros` list ([`Expansion::index`]).
    index: u32,
    attr: bool,
    inputs: Vec<tt::TopSubtree>,
    sites: [span::Span; 3],
    edition: span::Edition,
    forced_keywords: bool,
}

#[derive(Debug)]
struct Evaluated {
    library: Arc<Library>,
    name: Symbol,
    kind: ProcMacroKind,
    function: FunctionId,
    /// Its place in the crate's `__blaze_proc_macros` list, sorted by name, as a wasm module
    /// numbers its macros.
    index: u32,
}

fn refused(why: String) -> ProcMacroLoadingError {
    ProcMacroLoadingError::Refused(why.into_boxed_str())
}

/// The name of `krate` as its errors and spans say it.
/// The macros of `krate`, a proc-macro crate, each run on blaze's MIR evaluator in this
/// process for a crate built with `toolchain`. An error says why none can run: the crate is no
/// proc-macro crate, or its `proc_macro` is not the bridge blaze's server speaks. Its expansions
/// are kept where the daemon's `macros` keeps them.
pub fn load(
    db: &Db,
    krate: Crate,
    toolchain: ContentId,
    macros: &Arc<Macros>,
) -> ProcMacroLoadResult {
    load_on(db, krate, toolchain, macros, |_, bridge| Ok(Engine::Evaluator(bridge)))
}

/// The macros of `krate`, each run in a fresh instance of `module`, the crate built by blaze
/// for `wasm32-wasip1` with the bridge's glue (`guest/entry.rs`), under Wasmtime. The module
/// comes from `modules`, the daemon's, which compiles it or loads its machine code from the
/// store the daemon's `macros` keeps expansions in. An error says why none can run: [`load`]'s,
/// or the module's, naming the crate; nothing runs on the evaluator instead.
pub fn load_wasm(
    db: &Db,
    krate: Crate,
    toolchain: ContentId,
    macros: &Arc<Macros>,
    modules: &wasm::Modules,
    module: &[u8],
) -> ProcMacroLoadResult {
    load_on(db, krate, toolchain, macros, |name, _| {
        // alloc: error path, and once a crate the name its put says
        let what = format!("the wasm module of the proc-macro crate `{name}`");
        let mut library = wasm::Library::load(modules, &macros.keeps, module, &what)
            .map_err(|why| refused(format!("{what} {why}")))?; // alloc: error path
        library.watch(db, krate).map_err(|why| refused(format!("{what} {why}")))?; // alloc: error path
        Ok(Engine::Wasm(library))
    })
}

/// [`load`] and [`load_wasm`]: the crate checked, and its macros on the engine `engine` makes
/// of the crate's name and the bridge's functions found in its `proc_macro`.
fn load_on(
    db: &Db,
    krate: Crate,
    toolchain: ContentId,
    macros: &Arc<Macros>,
    engine: impl FnOnce(&str, Bridge) -> Result<Engine, ProcMacroLoadingError>,
) -> ProcMacroLoadResult {
    // alloc: once a proc-macro crate, the name its errors say
    let name = krate.named(db).to_string().into_boxed_str();
    if !crate_def_map(db, krate).is_proc_macro() {
        return Err(refused(format!("`{name}` is not a proc-macro crate"))); // alloc: error path
    }
    let Some(proc_macro) = protocol::proc_macro_of(db, krate) else {
        return Err(refused(format!(
            "the proc-macro crate `{name}` does not depend on `proc_macro`"
        ))); // alloc: error path
    };
    protocol::check(db, proc_macro)
        .map_err(|why| refused(format!("the proc-macro crate `{name}`: {why}")))?; // alloc: error path
    let bridge = Bridge::find(db, proc_macro)
        .map_err(|why| refused(format!("the proc-macro crate `{name}`: {why}")))?; // alloc: error path
    let code = code::Memo::default();
    let loaded = code.get(db, krate);
    let engine = engine(&name, bridge)?;
    let runs = engine.runs(loaded);
    let verdict = verdict(&macros.keeps, &runs)
        // alloc: error path
        .map_err(|why| {
            refused(format!("the proc-macro crate `{name}`: its verdict in the store: {why}"))
        })?;
    let library = Arc::new(Library {
        // alloc: once a proc-macro crate loaded
        macros: Arc::clone(macros), // clone: a refcount, once a proc-macro crate loaded
        krate,
        name,
        toolchain,
        engine,
        loaded,
        runs,
        code,
        seen: verdicts::Seen::new(verdict),
        sessions: SharedMap::default(),
        turns: Turns::default(),
        kept_roots: SharedMap::default(),
        settled: Lock::new("macro.library.settled", None),
    });
    macros.libraries.lock().push(Arc::downgrade(&library));
    // None for a proc-macro crate with none, which rustc builds: a use fails at the use.
    let functions = run::functions(db, krate);
    // A module numbers its macros by their place in `__blaze_proc_macros`, which is sorted by
    // name (`hir_ty_codegen::v2::roots::proc_macros`).
    // alloc: once a proc-macro crate, its macros' places
    let mut by_name: Vec<usize> = (0..functions.len()).collect();
    by_name.sort_by(|&a, &b| functions[a].0.as_str().cmp(functions[b].0.as_str()));
    let mut places = vec![0u32; functions.len()]; // alloc: as above
    for (place, &at) in by_name.iter().enumerate() {
        places[at] = place as u32;
    }
    Ok(functions
        .into_iter()
        .zip(places)
        .map(|((name, kind, function), index)| ProcMacro {
            name: name.clone(), // clone: a symbol, once a macro
            kind,
            // alloc: once a macro loaded
            expander: Arc::new(Evaluated {
                library: Arc::clone(&library),
                name,
                kind,
                function,
                index,
            }),
            disabled: false,
        })
        .collect()) // alloc: once a proc-macro crate, its macros
}

/// Whether `calling` enables `forced_keywords`, read without its def map: its proc macros
/// expand while that is built.
fn forced_keywords(db: &Db, calling: Crate) -> bool {
    UnstableFeatures::of_root(db, calling).forced_keywords
}

/// What a call of a macro of `kind` was given, as an error about it says it: the item a derive
/// or attribute is on (`struct Config`, `fn main`), found by its keyword among the input's
/// outermost tokens past its attributes and visibility, or a bang macro's input by its length.
fn input_item(kind: ProcMacroKind, input: &tt::TopSubtree) -> String {
    const ITEMS: [&str; 11] =
        ["struct", "enum", "union", "fn", "trait", "impl", "mod", "type", "const", "static", "use"];
    let keyword = |it: &tt::TtElement<'_>| match it {
        tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => {
            ITEMS.iter().find(|k| **k == ident.sym.as_str()).copied()
        }
        _ => None,
    };
    if kind != ProcMacroKind::Bang {
        let mut tokens = input.iter().peekable();
        while let Some(token) = tokens.next() {
            let Some(mut said) = keyword(&token) else { continue };
            // `const fn`, `unsafe impl`: the last keyword is the item's.
            while let Some(next) = tokens.peek().and_then(keyword) {
                said = next;
                tokens.next();
            }
            return match tokens.next() {
                // alloc: error path
                Some(tt::TtElement::Leaf(tt::Leaf::Ident(name))) => {
                    format!("`{said} {}`", name.sym.as_str())
                }
                // alloc: error path
                _ => format!("an item `{said} ...`"),
            };
        }
    }
    // alloc: error path
    format!("an input of {} token trees", input.iter().count())
}

/// A macro as an expansion's span names it: `m::answer`, its crate then its name.
struct MacroPath<'a>(&'a Evaluated);

impl fmt::Display for MacroPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.0.library.name, self.0.name.as_str())
    }
}

/// What an expansion was given, as [`input_item`] says it, for its span.
struct InputItem<'a>(ProcMacroKind, &'a tt::TopSubtree);

impl fmt::Display for InputItem<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&input_item(self.0, self.1))
    }
}

fn system(why: impl fmt::Display) -> ProcMacroExpansionError {
    // alloc: error path
    ProcMacroExpansionError::System(why.to_string())
}

impl Evaluated {
    /// This macro, as an error about it begins.
    fn what(&self) -> String {
        // alloc: an error, or what a put said owns
        format!("the proc macro `{}` of `{}`", self.name.as_str(), self.library.name)
    }

    /// The inputs of `call`: the attribute's arguments first for an attribute.
    fn inputs(&self, call: &ProcMacroCall<'_>) -> Vec<tt::TopSubtree> {
        // alloc: once per call, the trees kept for the session or made into streams
        let mut inputs = Vec::with_capacity(2);
        if self.kind == ProcMacroKind::Attr {
            match call.attrs {
                Some(attrs) => inputs.push(attrs.copy()),
                None => inputs.push(tt::TopSubtree::empty(tt::DelimSpan {
                    open: call.call_site,
                    close: call.call_site,
                })),
            }
        }
        inputs.push(call.subtree.copy());
        inputs
    }

    /// `call` as its session keeps it, to run again.
    fn kept(&self, db: &Db, calling: Crate, call: &ProcMacroCall<'_>) -> Kept {
        Kept {
            function: self.function,
            index: self.index,
            attr: self.kind == ProcMacroKind::Attr,
            inputs: self.inputs(call),
            sites: [call.def_site, call.call_site, call.mixed_site],
            edition: call.call_site.anchor.file_id.edition(),
            forced_keywords: forced_keywords(db, calling),
        }
    }

    /// Runs the call, after the session's calls before it when they must be run again, and
    /// keeps the state it leaves in the session: what it gave, and the state it ran from
    /// ([`From`]). It holds the session's turn while it runs from the session's state; a call
    /// of a session with none gives the turn back while it runs, as its calls run at once.
    fn run(
        &self,
        db: &Db,
        calling: Crate,
        call: &ProcMacroCall<'_>,
    ) -> Result<(Ran, From), ProcMacroExpansionError> {
        let library = &*self.library;
        let this = self.kept(db, calling, call);
        let session = library.session(db, calling);
        let mut turn = library.turn(db, calling, &session)?;
        let (stateless, holds, preset, ahead) = {
            let mut held = session.lock();
            held.taken += 1;
            let stateless = held.statics.is_empty() && !held.holds;
            let ahead = held.take_ahead(&this);
            // clone: the statics the call runs from, once per call of a session with state:
            // read outside the lock, which no query or run is made under
            let preset = if held.holds { Vec::new() } else { held.statics.clone() };
            (stateless, held.holds, preset, ahead)
        };
        let from =
            From { replayed: holds, start: if holds { None } else { start_digest(db, &preset) } };
        let ran = if let Some(ahead) = ahead {
            // Run ahead from the statics the session has now: what a run now would give.
            let ran = match ahead {
                Ok(ran) => ran?,
                Err(panic) => {
                    drop(turn);
                    std::panic::resume_unwind(panic);
                }
            };
            tracing::Span::current().record("ahead", true);
            if ran.asked {
                // What it asked the database of a span was read outside this query, which so
                // depends on nothing it read: run again in the next revision.
                blaze_query::db::report_untracked_read(
                    db,
                    blaze_query::db::Untracked {
                        what: "what a proc macro run ahead of its turn asked the database of a span, \
                               outside the expansion's query",
                    },
                );
            }
            ran
        } else if stateless {
            // Its calls run at once: the state of each is the start, which none of them change.
            drop(turn);
            let ran = self.run_on(db, calling, call, &[], &[], &this);
            turn = library.turn(db, calling, &session)?;
            ran?
        } else {
            let replayed: &[Kept] = if holds { &turn.calls } else { &[] };
            self.run_on(db, calling, call, &preset, replayed, &this)?
        };
        let left_holding = ran.statics.iter().any(|it| it.2);
        library.saw(match () {
            () if ran.statics.is_empty() && !from.replayed => Left::Nothing,
            () if stateless && !left_holding => Left::First,
            () => Left::Moved,
        })?;
        let Some(mut session) = turn.update() else { return Ok((ran, from)) };
        if !ran.statics.is_empty() {
            session.holds |= left_holding;
            if session.holds {
                // Its calls run again before each next one, from no preset: none runs ahead.
                session.ahead.clear();
            } else {
                for (st, bytes, _) in &ran.statics {
                    match session.statics.iter_mut().find(|it| it.0 == *st) {
                        // clone: a changed static, kept for the session's next call
                        Some(it) => it.1.clone_from(bytes),
                        None => session.statics.push((*st, bytes.clone())), // clone: as above
                    }
                }
            }
        }
        let keeps_calls = !session.statics.is_empty() || session.holds;
        drop(session);
        if keeps_calls {
            turn.calls.push(this);
        }
        Ok((ran, from))
    }

    /// Notes in the session of `calling` that `call`'s turn was found in the store, from the
    /// state the session has: it left that state as it was, and is a call the session's later
    /// ones may run after again.
    fn found(
        &self,
        db: &Db,
        calling: Crate,
        call: &ProcMacroCall<'_>,
    ) -> Result<(), ProcMacroExpansionError> {
        let session = self.library.session(db, calling);
        let mut turn = self.library.turn(db, calling, &session)?;
        let keeps_calls = {
            let mut held = session.lock();
            held.taken += 1;
            !held.statics.is_empty() || held.holds
        };
        if keeps_calls {
            turn.calls.push(self.kept(db, calling, call));
        }
        Ok(())
    }

    /// Runs `this` after `replayed`, from the statics `preset`.
    #[allow(clippy::too_many_arguments)]
    fn run_on(
        &self,
        db: &Db,
        calling: Crate,
        call: &ProcMacroCall<'_>,
        preset: &[(StaticId, Vec<u8>)],
        replayed: &[Kept],
        this: &Kept,
    ) -> Result<Ran, ProcMacroExpansionError> {
        // alloc: once per call, its streams and those of the calls run again
        let streams: Vec<Vec<Stream>> = replayed
            .iter()
            .chain(std::iter::once(this))
            .map(|it| it.inputs.iter().map(Stream::of_call).collect()) // alloc: once per run, its input streams
            .collect(); // alloc: once per run, as above
        let expansions: Vec<Expansion<'_>> = replayed
            .iter()
            .chain(std::iter::once(this))
            .zip(&streams)
            .map(|(it, inputs)| Expansion {
                function: it.function,
                index: it.index,
                attr: it.attr,
                inputs,
                sites: it.sites,
                edition: it.edition,
                forced_keywords: it.forced_keywords,
            })
            .collect(); // alloc: once per run, the expansions it runs
        let library = &*self.library;
        // alloc: once per call, the names its span carries
        let path = format!("{}::{}", library.name, self.name.as_str());
        let calling = calling.named(db).to_string(); // alloc: once per call, as `path`
        let span = run::expand_span(&path, &calling);
        // alloc: once per call, what its progress lines say
        let what = format!("the proc macro `{path}` for `{calling}`");
        let mut spent = run::Spent::default();
        let ran = match &library.engine {
            Engine::Evaluator(bridge) => run::run(
                db,
                &what,
                bridge,
                call.env,
                preset,
                call.current_dir,
                &expansions,
                &mut spent,
            ),
            Engine::Wasm(module) => {
                module.run(db, call.env, &expansions, preset, call.current_dir, &mut spent)
            }
        };
        run::ran_on(span, &spent, &ran);
        // An expansion that stopped (a budget, a refused call) names what it expanded and where,
        // beside the macro and what stopped it: a step limit says the steps it ran.
        ran.map_err(|stop| {
            let item = input_item(self.kind, call.subtree);
            system(format_args!("{}, expanding {item} in `{calling}`, {stop}", self.what()))
        })
    }

    /// Whether the session of `calling` can give its state to a run ahead of a call's turn: a
    /// call of the session ran in this revision, no static it left holds an address (then each
    /// call runs the earlier ones again), and no call's turn found its run ahead stale.
    fn speculates_in(&self, db: &Db, calling: Crate) -> bool {
        let session = self.library.session(db, calling);
        let held = session.lock();
        held.taken > 0 && !held.holds && !held.serial
    }

    /// Runs `call` ahead of its turn from the statics the session of `calling` has now, and
    /// keeps what it gave for its turn ([`Session::take_ahead`]). A panic is kept for its turn
    /// to unwind with; a query's cancellation goes on at once, keeping nothing.
    fn run_ahead(&self, db: &Db, calling: Crate, call: &ProcMacroCall<'_>) {
        if !self.speculates_in(db, calling) {
            return;
        }
        let session = self.library.session(db, calling);
        // clone: the statics a run ahead starts from, once per call run ahead
        let preset = session.lock().statics.clone();
        let this = self.kept(db, calling, call);
        let _span = tracing::info_span!(
            target: "blaze",
            "proc macro run ahead",
            r#macro = %MacroPath(self),
            input = %InputItem(self.kind, call.subtree),
        )
        .entered();
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run_on(db, calling, call, &preset, &[], &this)
        }));
        let ran = match ran {
            Err(payload) if payload.is::<blaze_query::db::Cancelled>() => {
                std::panic::resume_unwind(payload)
            }
            ran => ran,
        };
        let mut held = session.lock();
        // A turn already found a run ahead stale, or a static now holds an address: no turn
        // takes this one.
        if held.serial || held.holds {
            return;
        }
        held.ahead.push(Ahead { this, preset, ran });
    }

    /// The state the session of `calling` would start its next call from: `None` when the
    /// calls before it are run again (a static holds an address), else its [`start_digest`].
    fn start(&self, db: &Db, calling: Crate) -> Option<Option<ContentId>> {
        let session = self.library.session(db, calling);
        let statics = {
            let held = session.lock();
            if held.holds {
                return None;
            }
            // clone: the statics the next call starts from, once per kept call of a session
            // with state: digested outside the lock, as the digest asks the database
            held.statics.clone()
        };
        Some(start_digest(db, &statics))
    }

    /// The output of `ran`, or the error it ends in.
    fn output(
        &self,
        ran: &Ran,
        call_site: span::Span,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let errors: Vec<&str> =
            ran.emitted.iter().filter(|it| it.error).map(|it| it.text.as_str()).collect(); // alloc: once per call, the errors it emitted (none for most)
        for warning in ran.emitted.iter().filter(|it| !it.error) {
            eprintln!("{}: {}", self.what(), warning.text);
        }
        match &ran.output {
            Err(panic) => Err(ProcMacroExpansionError::Panic(panic.clone())), // clone: error path
            Ok(_) if !errors.is_empty() => Err(ProcMacroExpansionError::Panic(errors.join("\n"))),
            // clone: a stream shares its trees, so this copies a pointer
            Ok(stream) => Ok(stream.clone().into_top(call_site)),
        }
    }

    /// The lines of what `ran` read, with the paths it tracked made absolute against `dir`,
    /// and the files outside the source roots its macro's code was made from (`included`).
    fn lines<'a>(
        ran: &'a Ran,
        dir: &str,
        included: &[std::path::PathBuf],
        tracked: &'a mut Vec<String>,
    ) -> impl Iterator<Item = (Seen, &'a str)> {
        tracked.clear();
        tracked.extend(ran.tracked.iter().map(|path| {
            // alloc: once per tracked path, as an expansion is kept
            std::path::Path::new(dir).join(path).to_string_lossy().into_owned()
        }));
        // alloc: once per included file, as an expansion is kept
        tracked.extend(included.iter().map(|path| path.to_string_lossy().into_owned()));
        ran.seen
            .iter()
            .map(|(seen, what)| (*seen, what.as_str()))
            .chain(tracked.iter().map(|it| (Seen::Read, it.as_str())))
    }

    /// For a crate that settles: whether `ran`, which ran from `from` and gave `bytes`, gives
    /// the same from the other of the two states its sessions have, the fresh one and the
    /// settled one its first call leaves, and whether that settled state is the one every first
    /// call left since the crate loaded. The verdict is learned from what runs do to the state,
    /// not from what they give: an "emit once" macro (a flag, and a helper item only while it
    /// is not set) settles, yet its call gives otherwise as the first (after a reset, as from
    /// the memo, or as the first of body-level calls run in no order). `Err` says why what it
    /// read does not fix what it gave; a call shown to give otherwise from the two states, or
    /// a second settled state, tells the store the crate is stateful when `demote` (a crate
    /// loaded as settling), so no call of it is taken as fixed by its reads from the next load
    /// on. One more run of the call, from the
    /// other state, outside the session.
    fn same_from_either_state(
        &self,
        db: &Db,
        calling: Crate,
        call: &ProcMacroCall<'_>,
        ran: &Ran,
        from: From,
        bytes: Option<&[u8]>,
        spans: &Spans,
        demote: bool,
    ) -> Result<Result<(), &'static str>, ProcMacroExpansionError> {
        const UNSHOWN: &str = "its macro's crate settles, and it was not shown to give the same from the fresh state as from the settled one";
        const DIFFERS: &str = "its macro's crate settles, yet it gives otherwise from the fresh state than from the settled one, or its first calls leave other states: what it gives depends on whether it ran first";
        let library = &*self.library;
        let Some(bytes) = bytes else { return Ok(Err(UNSHOWN)) };
        let (at, _) = db.nonce_and_revision();
        // The settled state, and the statics to run the other state from: none for the fresh.
        let other: Vec<(StaticId, Vec<u8>)> = match (from.replayed, from.start) {
            (true, _) => return Ok(Err(UNSHOWN)),
            // From the fresh state: the settled one is what it left, or, when it left the state
            // as it was, the one first calls left before.
            (false, None) if ran.statics.iter().any(|it| it.2) => return Ok(Err(UNSHOWN)),
            (false, None) if !ran.statics.is_empty() => {
                // clone: once per first call of a session of a crate that settles
                let statics: Vec<(StaticId, Vec<u8>)> =
                    ran.statics.iter().map(|(id, bytes, _)| (*id, bytes.clone())).collect();
                let Some(digest) = start_digest(db, &statics) else { return Ok(Err(UNSHOWN)) };
                let mut settled = library.settled.lock();
                if settled.as_ref().is_some_and(|it| it.digest != digest) {
                    drop(settled);
                    if demote {
                        library.saw(Left::Moved)?;
                    }
                    return Ok(Err(DIFFERS));
                }
                // clone: as above
                *settled = Some(Settled { digest, at, statics: statics.clone() });
                drop(settled);
                statics
            }
            (false, None) => match &*library.settled.lock() {
                // clone: once per such call, the statics it runs from
                Some(it) if it.at == at => it.statics.clone(),
                _ => return Ok(Err(UNSHOWN)),
            },
            // From the settled state: the other is the fresh one.
            (false, Some(digest)) => {
                if library.settled.lock()
                    .as_ref()
                    .is_some_and(|it| it.digest != digest)
                {
                    if demote {
                        library.saw(Left::Moved)?;
                    }
                    return Ok(Err(DIFFERS));
                }
                Vec::new()
            }
        };
        let this = self.kept(db, calling, call);
        let again = {
            // Its own span, beside the run's `macro expand`: the extra run this check costs.
            let _span = tracing::info_span!(
                target: "blaze",
                "proc macro expansion again from the other state",
                r#macro = %MacroPath(self),
                from_fresh = other.is_empty(),
            )
            .entered();
            self.run_on(db, calling, call, &other, &[], &this)?
        };
        if again.asked || again.unkeyable.is_some() || again.statics.iter().any(|it| it.2) {
            return Ok(Err(UNSHOWN));
        }
        // What the extra run did to the state says the crate's state too: from the settled
        // state it changes nothing, and from the fresh one it leaves nothing or the settled
        // state. Anything else is a state that moves on.
        let left_other = match other.is_empty() {
            false => !again.statics.is_empty(),
            true => {
                // clone: once per such run, the statics it left, digested
                let left: Vec<(StaticId, Vec<u8>)> =
                    again.statics.iter().map(|(id, bytes, _)| (*id, bytes.clone())).collect();
                // Run from the fresh state only for a call that ran from the settled one.
                let digest = start_digest(db, &left);
                digest.is_some() && digest != from.start
            }
        };
        if left_other {
            if demote {
                library.saw(Left::Moved)?;
            }
            return Ok(Err(DIFFERS));
        }
        let gave = self.output(&again, call.call_site).ok().and_then(|it| written(&it, spans));
        match gave.as_deref() == Some(bytes) {
            true => Ok(Ok(())),
            false => {
                if demote {
                    library.saw(Left::Moved)?;
                }
                Ok(Err(DIFFERS))
            }
        }
    }

    /// Expands `call` from `calling`'s session, found or run, and states what it read and
    /// whether its session's state took part ([`Expanded`]) on every way out.
    fn expand_keyed(
        &self,
        db: &Db,
        calling: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        let library = &*self.library;
        // One span per expansion, kept or run: which macro, on what, by which engine. The
        // fields format only when a subscriber records them.
        let _span = tracing::info_span!(
            target: "blaze",
            "proc macro expansion",
            r#macro = %MacroPath(self),
            input = %InputItem(self.kind, call.subtree),
            engine = match library.engine {
                Engine::Evaluator(_) => "evaluator",
                Engine::Wasm(_) => "wasm",
            },
            ahead = tracing::field::Empty,
        )
        .entered();
        let verdict = library.seen.verdict();
        let keeping: &Keeping = match &library.macros.keeps {
            Keeps::In(keeping) => keeping,
            Keeps::Nothing(why) => {
                let (ran, _) = self.run(db, calling, &call)?;
                let tree = self.output(&ran, call.call_site)?;
                return Ok(Expanded {
                    tree,
                    reads: ExpansionReads::Unknown(*why),
                    state: state_use(verdict, false),
                });
            }
        };
        let start = self.start(db, calling);
        let keyed = Call {
            name: self.name.as_str(),
            kind: self.kind,
            subtree: call.subtree,
            attrs: call.attrs,
            env: call.env,
            def_site: call.def_site,
            call_site: call.call_site,
            mixed_site: call.mixed_site,
            current_dir: call.current_dir,
        };
        let (mut given, spans) = given(&library.toolchain, &keyed);
        // The server lexes the text a macro parses in the call site's edition; the spans'
        // shapes number their anchors, which says no edition.
        given.part("edition", &[call.call_site.anchor.file_id.edition() as u8]);
        // Whether each `k#` the macro lexes is gated, which the crate's attributes say.
        given.part("forced_keywords", &[forced_keywords(db, calling) as u8]);
        // A module's expansion is kept apart from the evaluator's, under the module it ran.
        if let Engine::Wasm(module) = &library.engine {
            given.part("wasm module", &module.id().0);
        }
        // The state it starts from, when calls before it left some (the module doc).
        if let Some(Some(digest)) = start {
            given.part("start state", &digest.0);
        }
        // A crate that settles keeps only a call shown to give the same from either of its
        // states ([`Self::same_from_either_state`]), and so does an unseen crate's call from a
        // state its session's first call left (its crate may settle): kept apart from what was
        // kept unshown, so a build that loads the crate as settling finds what an unseen
        // build kept and showed.
        let shown = match verdict {
            Verdict::Settles => true,
            Verdict::Unseen => matches!(start, Some(Some(_))),
            Verdict::Stateless | Verdict::Stateful => false,
        };
        if shown {
            given.part("same from either state", &[1]);
        }
        let key = RunKey { code: library.code.get(db, library.krate), given: given.finish() };
        let mut inputs = Calling::new(call.env);
        // What its key, reads and shape are written against: the calling crate's package and
        // `OUT_DIR` (`proc_macro_api::kept::roots`).
        let roots = proc_macro_api::kept::roots(call.env);
        let mut found = None;
        if start.is_some() {
            let guard = blaze_cas::enter();
            let mut rooted = blaze_usercode::roots::Rooted::new(roots, &mut inputs);
            match index::find_reads(&keeping.expansions.store, &guard, &key, &mut rooted) {
                Ok(Found::Hit { value, shape, values: Some(values), verified, .. }) => {
                    let reads = match verified {
                        // alloc: what the query reporting it owns
                        None => ExpansionReads::Traced { shape: shape.into(), values },
                        // Verified across checkouts: what it reports changes once it is
                        // retracted, so what was filed by it is made again; a warm front
                        // end is not lowered into again once a record is published
                        // (`Keeping::retractions_now`).
                        Some(unmarked) => {
                            let depends = index::depends_key(&key, &unmarked);
                            let (shape, values) =
                                proc_macro_api::kept::verified_reads(&depends, shape, values);
                            ExpansionReads::Traced { shape: shape.into(), values }
                        }
                    };
                    // A crate whose session's state is not known not to matter: what a call read
                    // does not fix what it gives (what calls ran before it does too).
                    let reads = match verdict {
                        Verdict::Stateless | Verdict::Settles => reads,
                        Verdict::Unseen | Verdict::Stateful => ExpansionReads::Unknown(UNSETTLED),
                    };
                    match keeping.mode {
                        Mode::Keep => {
                            let tree = read(value, &spans)
                                .map_err(|why| system(format_args!("{}: {why}", self.what())))?;
                            self.found(db, calling, &call)?;
                            // Found under its key, which a crate that settles keeps only shown
                            // calls under (`shown`).
                            return Ok(Expanded { tree, reads, state: state_use(verdict, true) });
                        }
                        Mode::Check => found = Some((checked(value), reads, verified)),
                    }
                }
                Ok(Found::Hit { values: None, .. }) => {
                    return Err(system(format_args!(
                        "{}: the store's lookup found a hit without its values",
                        self.what()
                    )));
                }
                Ok(Found::Miss { .. }) => {}
                Err(FindError::Corrupt(corrupt)) => {
                    return Err(system(format_args!("{}: {corrupt}", self.what())));
                }
                Err(FindError::NotAShape { key, why }) => {
                    return Err(system(format_args!(
                        "{}: the store's {key:?} is not a shape: {why}",
                        self.what()
                    )));
                }
                Err(FindError::Inputs(unreadable)) => {
                    return Err(system(format_args!("{}: {unreadable}", self.what())));
                }
                Err(FindError::TooManyShapes { run }) => {
                    return Err(system(format_args!(
                        "{}: run {run:?} has more than {} shapes",
                        self.what(),
                        index::MOST_SHAPES
                    )));
                }
            }
        }
        let (ran, from) = self.run(db, calling, &call)?;
        let output = self.output(&ran, call.call_site)?;
        let bytes = written(&output, &spans);
        // A check that found a value verified across checkouts expanding otherwise: the
        // record that retracts it is put (durable before the job is done), the expansion is
        // kept here alone below, and the check's error is what every way out gives.
        let mut retracted = false;
        if let Some((kept, reads, verified)) = found {
            if bytes.as_deref().map(checked) == Some(kept) {
                return Ok(Expanded { tree: output, reads, state: state_use(verdict, true) });
            }
            let Some(unmarked) = verified else {
                return Err(system(format_args!(
                    "{}: {}=check: it expanded otherwise than the store kept it, under a key of \
                     every input it was traced to read",
                    self.what(),
                    proc_macro_api::kept::CACHE
                )));
            };
            let record = verify::retraction(&key, &unmarked);
            keeping.put(Put {
                keep: Keep::Durable { run: record.run, traced: record.traced },
                value: record.value,
                what: self.what(),
            });
            retracted = true;
        }
        let done = |tree: tt::TopSubtree, reads: ExpansionReads, state: StateUse| match retracted {
            false => Ok(Expanded { tree, reads, state }),
            true => Err(system(format_args!(
                "{}: {}=check: it expanded otherwise than the expansion verified across checkouts \
                 for the same inputs: it depends on where its roots are, so it is recorded as doing \
                 so and no checkout is given that expansion again",
                self.what(),
                proc_macro_api::kept::CACHE
            ))),
        };
        // A way out before the crate's state is shown either way: its reads unknown, for `why`,
        // and its session's state taking part unless its crate keeps none.
        let unknown = |tree: tt::TopSubtree, why: &'static str| {
            done(tree, ExpansionReads::Unknown(why), state_use(verdict, false))
        };
        if ran.asked {
            return unknown(output, "it asked the database");
        }
        // Kept only from the state its key says, left as it was: a hit then gives what a run
        // gives and leaves the session as the run would. Otherwise it is not kept.
        let session = from.replayed || !ran.statics.is_empty() || start != Some(from.start);
        // What such a run read is reported, as this checkout's, only when its output is what
        // its own inputs give whatever ran before it: its crate settles (the first call's
        // change of the fresh state is the only change, and no call's output depends on
        // whether it ran first) and this run moved no state on (`Left::Moved`: it ran after
        // its calls before it again, or changed a state they left, or left an address). A
        // session's state is otherwise not fixed by the calling crate's inputs: it starts again
        // each revision while the calls before this one may come from the database's memo,
        // and body-level calls run one at a time but in no fixed order, so the state this one
        // ran from depends on which calls ran in this revision and when. Such a run's reads
        // are unknown, and no record is kept by them (as for a stateful or unseen crate).
        let left = SessionRun {
            replayed: from.replayed,
            fresh: !from.replayed && from.start.is_none(),
            changed: !ran.statics.is_empty(),
            left_address: ran.statics.iter().any(|it| it.2),
        };
        if session && !session_reads_known(verdict, left) {
            return unknown(
                output,
                "its macro keeps a session that does not settle, or it moved its session's state on: its output depends on the calls before it, which ran in no order a key fixes",
            );
        }
        if !ran.emitted.is_empty() {
            return unknown(output, "it emitted a diagnostic");
        }
        if ran.unkeyable.is_some() {
            return unknown(output, "what it read can't be keyed");
        }
        // alloc: once per kept expansion, its tracked paths made absolute
        let mut tracked = Vec::new();
        // alloc: as above, the files the macro's code included
        let mut included = Vec::new();
        code::included(db, library.krate, &mut included);
        let traced = assemble_seen(
            Self::lines(&ran, call.current_dir, &included, &mut tracked),
            &mut inputs,
        )
        .map_err(|why| system(format_args!("{}: {why}", self.what())))?;
        if index::untraceable(&traced) {
            return unknown(output, "it read what can't be traced (the clock, randomness)");
        }
        // Kept and reported written against the crate's roots, marked with every one, and
        // given to another checkout only once a second one expanded it the same
        // (`blaze_usercode::roots::verify`). No copy for another to check when the output,
        // a variable it read or a string its key was given still holds a root in any spelling
        // (a literal path the macro made), it read outside its roots and the machine-wide
        // places, blaze's fetched packages among them, or a root's links
        // moved since the memo was made (then it is made again).
        let kept = library.kept_roots(db, calling, roots);
        let view = kept.map.view();
        let marked =
            view.kept_bytes(&traced, bytes.as_deref(), proc_macro_api::kept::left(&keyed, &view));
        if marked.relinked {
            library.kept_roots.pin().remove(&calling);
        }
        // Whether what it read fixes what it gave: always for a crate with no state; for one
        // that settles, once it gave the same from either state; never for another with a
        // session, whose run from its start state, left as it was, is still kept (its key
        // holds the state) but reports its reads unknown.
        // Whether it may be kept: a call keyed as shown ([`Self::same_from_either_state`]) only
        // once it is. An unseen crate's call is shown to be kept, its reads unknown still, and
        // what it showed otherwise tells the store nothing: that is a crate loaded as settling.
        // And whether that was shown of its output: what [`state_use`] tells the front end of a
        // crate that settles.
        let (keep, unknown_why, proven) = match verdict {
            Verdict::Stateless => (true, None, false),
            Verdict::Settles => match self.same_from_either_state(
                db,
                calling,
                &call,
                &ran,
                from,
                bytes.as_deref(),
                &spans,
                true,
            )? {
                Ok(()) => (true, None, true),
                Err(why) => (false, Some(why), false),
            },
            Verdict::Unseen if shown => {
                let same = self.same_from_either_state(
                    db,
                    calling,
                    &call,
                    &ran,
                    from,
                    bytes.as_deref(),
                    &spans,
                    false,
                )?;
                (same.is_ok(), Some(UNSETTLED), false)
            }
            Verdict::Unseen | Verdict::Stateful => (true, Some(UNSETTLED), false),
        };
        let state = state_use(verdict, proven);
        if let Some(why) = unknown_why.filter(|_| !keep || session) {
            return done(output, ExpansionReads::Unknown(why), state);
        }
        // What it read, as the front end is told it: unknown while its crate's state is.
        let stated = |reads: ExpansionReads| match unknown_why {
            Some(why) => ExpansionReads::Unknown(why),
            None => reads,
        };
        let Some(value) = bytes.filter(|_| !session) else {
            // Nothing is kept (no bytes to keep, or it ran with state of a crate that settles):
            // what it read is stated as this checkout's.
            let reads = stated(ExpansionReads::Traced {
                shape: index::Shape::of(&marked.local).encode().into(), // alloc: once, as an expansion is kept
                values: index::VALUES.traced(&marked.local),
            });
            return done(output, reads, state);
        };
        let puts = match retracted {
            // Retracted: kept here alone, as no checkout is given what was verified (the
            // record is not in the store yet, so [`verify::keeps`] would not see it).
            // alloc: once, its one put
            true => vec![verify::Put { run: key, traced: marked.local, value, durable: false }],
            false => {
                let guard = blaze_cas::enter();
                let store = &keeping.expansions.store;
                let same = |theirs: &[u8], ours: &[u8]| -> Result<Option<String>, String> {
                    Ok((theirs != ours).then(|| "its output differs".to_owned())) // alloc: a difference said
                };
                let keeps = verify::keeps(
                    |key| Ok(store.get(&guard, key)?.map(|it| it.bytes)),
                    &view,
                    &key,
                    marked,
                    value,
                    same,
                );
                keeps
                    .map_err(|error| {
                        system(format_args!("{}: keeping it: {error:?}", self.what()))
                    })?
                    .0
            }
        };
        // What it states is the put made under its own key: here, marked with every root, or
        // verified, with the roots that were the same in every checkout compared, and then
        // also whether it was retracted since ([`proc_macro_api::kept::verified_reads`]). With
        // no put under its own key, no record names what it read.
        let reads = match puts.iter().find(|it| it.run == key) {
            Some(it) => {
                // alloc: once, as an expansion is kept
                let shape = index::Shape::of(&it.traced).encode();
                let values = index::VALUES.traced(&it.traced);
                stated(match index::split_verified(&it.value) {
                    None => ExpansionReads::Traced { shape: shape.into(), values },
                    Some((unmarked, _)) => {
                        let (shape, values) = proc_macro_api::kept::verified_reads(
                            &index::depends_key(&key, &unmarked),
                            &shape,
                            values,
                        );
                        ExpansionReads::Traced { shape: shape.into(), values }
                    }
                })
            }
            None => ExpansionReads::Unknown(
                "nothing of it was kept under its own key, so no record names what it read",
            ),
        };
        // A record that a value verified across checkouts depends on where its roots are (another
        // checkout's copy expanded otherwise) is durable before the job is done: a crash
        // before would forget it, and the expansion would be verified and given again.
        for it in puts {
            keeping.put(Put::of(it, self.what()));
        }
        done(output, reads, state)
    }
}

impl ProcMacroExpander for Evaluated {
    fn requires_session(&self) -> bool {
        self.library.seen.verdict() != Verdict::Stateless
    }

    /// The calls of a crate whose state is not known, settles or is kept run at their turns,
    /// on either engine.
    fn ordered(&self) -> bool {
        self.requires_session()
    }

    fn speculates(&self, db: &Db, calling_crate: Crate) -> bool {
        self.requires_session() && self.speculates_in(db, calling_crate)
    }

    fn speculate(&self, db: &Db, calling_crate: Crate, call: ProcMacroCall<'_>) {
        if self.requires_session() {
            self.run_ahead(db, calling_crate, &call);
        }
    }

    /// What its crate's macros run ([`Library::runs`]): a wasm module's expansions are another
    /// crate's key than the evaluator's, and another module's another again.
    fn code(&self) -> Option<ContentId> {
        Some(self.library.runs)
    }

    /// A call with no calling crate is its own session.
    fn expand(
        &self,
        db: &Db,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        let calling = self.library.krate;
        self.expand_keyed(db, calling, call)
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<Expanded, ProcMacroExpansionError> {
        self.expand_keyed(db, calling_crate, call)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        // dyn: the database's expander trait, the edge
        (other as &dyn std::any::Any) // dyn: as above, told apart by type
            .downcast_ref::<Evaluated>()
            .is_some_and(|other| {
                Arc::ptr_eq(&self.library, &other.library) && self.function == other.function
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionRun, Verdict, session_reads_known};

    const FRESH_FIRST: SessionRun =
        SessionRun { replayed: false, fresh: true, changed: true, left_address: false };
    const SETTLED: SessionRun =
        SessionRun { replayed: false, fresh: false, changed: false, left_address: false };

    /// A call of a crate that settles reports its reads, the first call's change of the fresh
    /// state included; a stateful or unseen crate's never does, not even a call that ran from
    /// the fresh state (after a write, `counter!`'s third call runs from it while the two
    /// before it come from the memo, and gives 0 where rustc gives 2); and a run that moved
    /// state on never does.
    #[test]
    fn only_a_settled_sessions_unmoved_runs_have_known_reads() {
        assert!(session_reads_known(Verdict::Settles, FRESH_FIRST));
        assert!(session_reads_known(Verdict::Settles, SETTLED));
        for verdict in [Verdict::Stateful, Verdict::Unseen] {
            assert!(!session_reads_known(verdict, FRESH_FIRST), "{verdict:?}");
            assert!(!session_reads_known(verdict, SETTLED), "{verdict:?}");
        }
        let moved = [
            SessionRun { replayed: true, ..SETTLED },
            SessionRun { changed: true, ..SETTLED },
            SessionRun { left_address: true, ..FRESH_FIRST },
        ];
        for run in moved {
            assert!(!session_reads_known(Verdict::Settles, run), "{run:?}");
        }
    }
}
