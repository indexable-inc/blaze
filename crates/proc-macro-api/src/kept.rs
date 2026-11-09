//! Proc-macro expansions kept in the store (docs/v2/plan.md), so that no
//! macro is expanded twice for the same inputs, on this machine or, through the store's
//! remote, another.
//!
//! An expansion is a run of user code ([`blaze_usercode::RunKey`]): its code is the content ID
//! of the macro's library, and what it is given ([`given`]) is the macro's name and kind, its
//! input and attribute tokens, the variables of the crate it expands for, the directory it
//! runs in, the toolchain and the proc-macro ABI. Spans are given relative: each as its byte
//! range from the item it is anchored to and which of the call's anchors and syntax contexts
//! it has, numbered as they first appear, so the next call with the same input in another
//! database or revision has the same key. The value is the output with its spans written the
//! same way ([`written`]). What else it read (files, variables of the shell, a listing) the
//! host that ran it traced, and the result is found and kept by exactly those through
//! [`blaze_usercode::index`]. `blaze-macro-eval`, which runs macro code, keys, finds and puts
//! its expansions with what is here, through the [`Keeps`] its daemon made and lent it.
//!
//! The key, what the expansion read and its shape are written against the calling crate's
//! roots ([`roots`]: its package, `CARGO_MANIFEST_DIR`, and its `OUT_DIR`), so the same call of
//! the same macro on the same crate in another checkout is found (docs/v2/plan/design/store.md,
//! "Portable keys"). Every expansion is kept against where those roots are, and is given to
//! another checkout only once a second one expanded it the same (`blaze_usercode::roots::verify`);
//! one whose output, read variables or given strings still hold a root once ported ([`left`],
//! a literal the macro made of `CARGO_MANIFEST_DIR`) is kept here alone.
//!
//! Not kept: an expansion that asked the database about a span (its text, file or line),
//! since the key does not say what it was told; one whose output has an anchor or a context
//! its input has not, which the next call could not be given back; one that read what can't
//! be traced (the clock, randomness); and one that failed. `BLAZE_PROC_MACRO_CACHE=check`
//! expands every call it finds kept anyway, and an output other than the kept one is an error
//! naming the macro.
//!
//! An expansion runs inside a query on a worker, many at once, so it writes nothing shared: it
//! reads the store, and what it keeps it leaves in its worker's own buffer for the store's
//! owner ([`puts`]); its statistics are its worker's ([`tally`]).

pub mod env;
pub mod puts;
pub mod tally;
pub mod verdict;

use std::{
    cell::Cell,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use base_db::Env;
use blaze_cas::Reader;
use blaze_ids::ContentId;
use blaze_usercode::{
    Given, RunKind,
    index::{self, FindError},
    inputs,
    roots::{Role, RoleSet, Rooted, Roots},
    trace::Inputs,
};
use hir_expand::proc_macro::ProcMacroKind;
use span::{Span, SpanAnchor, SyntaxContext};

pub use self::env::{Environment, SET};
pub use self::puts::{Keep, Published, Put, put, take_puts};
// Only `Keeping::publish` and `Keeps::publish` publish from outside: a durable put published
// any other way would neither be the daemon's nor move its epoch. The tests' own publish:
#[cfg(test)]
use self::puts::publish;
use crate::bridge::wire::{self, Parts};

/// The version of what a macro is given and gives back that expansions are kept under: a
/// change to either bumps it, so nothing kept before is found.
const ABI: u32 = 9;

/// What the content ID of a macro's library is made with.
pub const LIBRARY: &str = "blaze v2 proc-macro library 1";

/// The variable that says to check kept expansions.
pub const CACHE: &str = "BLAZE_PROC_MACRO_CACHE";
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// What is found is used.
    Keep,
    /// What is found is expanded again and compared.
    Check,
}

impl Mode {
    /// The mode `value` of [`CACHE`] says.
    pub fn of(value: Option<&str>) -> Result<Mode, String> {
        match value {
            None => Ok(Mode::Keep),
            Some("check") => Ok(Mode::Check),
            Some(other) => Err(format!(
                // alloc: error path
                "{CACHE}={other:?} is not a mode: every expansion that can be kept is, and \
                 `check` expands what was kept again and compares"
            )),
        }
    }
}

/// Where expansions are kept, and what they are keyed by besides their call.
pub struct Expansions {
    /// Read to find an expansion, with no lock, under a guard taken for that read alone;
    /// never written by one: what an expansion keeps goes to its worker's buffer
    /// ([`take_puts`]), which the store's owner [`publish`]es.
    pub store: Reader,
    /// The toolchain whose proc-macro ABI and library the macros are built with.
    pub toolchain: ContentId,
    /// What the user's shell passes through to the expansions, which they are checked against.
    pub environment: Arc<Environment>,
}

/// Where a daemon keeps its expansions, and how: made once by the daemon ([`Keeping::new`]) and
/// lent to every reader, so each of two daemons in one process reads its own store.
pub struct Keeping {
    pub expansions: Expansions,
    pub mode: Mode,
    /// How many records that retract a value verified across checkouts this daemon published
    /// ([`Keeping::retractions_now`]).
    pub retractions: Retractions,
    /// The [`Keep::Durable`] puts of its expansions not published yet ([`Keeping::put`]): this
    /// daemon's own, so another daemon in the process never publishes them into its store,
    /// and taken only by the store's owner ([`Keeping::publish`]).
    durable: crossbeam_queue::SegQueue<Put>,
}

/// What a warm front end is kept by, besides its units and config: whether a value verified
/// across checkouts that it may have been given has been retracted since (rule 21). Such a
/// value is a tracked expansion and, through `hir_def::deps`, a crate's image; neither is asked
/// again while its database is kept, so a record put later would not reach it. The store of
/// expansions has one writer, its daemon (`blaze-cargo`'s `daemon.rs` opens it once), so every
/// record is one this daemon publishes ([`Keeping::publish`]), and each publish of one moves
/// the epoch once the record is in the store; a kept front end is lowered into only while
/// the epoch is the one it was made at.
///
/// The invariant: a front end stamped `e` (the epoch its lowering read before it expanded
/// anything) has seen every record whose bump is at most `e`. A bump comes after its record
/// is readable, and the lowering reads the epoch before it reads the store, so a record
/// counted in `e` was there for every lookup it made; a record whose bump is past `e` makes
/// the epoch another, and the front end is not reused. Bumping before the record is in the
/// store would break it: a lowering that read the new epoch in between would be given the
/// retracted value and kept under the stamp that counts its record.
///
/// Values that arrive from elsewhere never bump it. That is safe because no retraction record
/// arrives: the daemon opens the store of expansions with no layers (`daemon.rs`, `layers:
/// Vec::new()`), and the global cache's fetches (`blaze-cargo`'s `serve/unit/script/global.rs`)
/// bring a build script's shape slots and results, each proven public by
/// `blaze_global::Rule::run`, never a record (a value verified across checkouts is refused
/// before any store keeps it, `blaze_global::public::fetchable`). So every record in this store
/// went through [`Keeping::publish`]. A remote layer, or a fetch of records into this store, is
/// a change that must bump the epoch for each record it brings, after the record is readable;
/// without that, a kept front end would keep a value another machine's record retracted.
///
/// Whether the front end was given the value the record retracts is not asked (a record is
/// rare: another checkout's copy differed), so this is one atomic load per lowering, with
/// nothing that grows.
#[derive(Default)]
pub struct Retractions {
    /// How many records were published: a front end kept at another epoch may have been given
    /// a value retracted since.
    epoch: AtomicU64,
}

impl Retractions {
    /// Counts a record, once it is in the store ([`Retractions`]'s invariant).
    pub fn retracted(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// The epoch now.
    pub fn now(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

/// Whether a reader's expansions are kept: in a daemon's store, or nowhere, for a named reason.
pub enum Keeps {
    In(Keeping),
    /// Nothing is kept or found (a bench, a test of the evaluator alone): why, as each expansion
    /// that ran says it of its reads.
    Nothing(&'static str),
}

/// What a reader that needs a daemon's store was told by a [`Keeps::Nothing`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotKept {
    pub why: &'static str,
}

impl std::fmt::Display for NotKept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proc-macro expansions are not kept here: {}", self.why)
    }
}

impl std::error::Error for NotKept {}

/// The inputs of an expansion as they are now: the machine's files, and the environment it is
/// sent ([`Environment::seen`]). A variable a shape names is looked up where it is, so a hit
/// allocates nothing; the whole environment is worked out only for a run that read all of it.
struct Seen<'a> {
    passed: &'a [(String, String)],
    call: &'a Env,
    made: Option<inputs::Env>,
    /// What file, directory and listing reads are asked through: no variable is read there.
    none: inputs::Env,
}

impl<'a> Seen<'a> {
    /// The value [`Environment::seen`] gives `name`: the call's, over what is passed, over
    /// what blaze sets for every expansion ([`SET`]). A name that is not UTF-8 is none of them.
    fn value(&self, name: &std::ffi::OsStr) -> Option<&'a str> {
        let name = name.to_str()?;
        if let Some((_, value)) = self.call.iter().find(|it| it.0 == name) {
            return Some(value);
        }
        if let Some((_, value)) = self.passed.iter().rev().find(|it| it.0 == name) {
            return Some(value.as_str());
        }
        SET.iter().find(|it| it.0 == name).map(|it| it.1)
    }
}

impl Inputs for Seen<'_> {
    type Error = inputs::Unreadable;
    fn entry(
        &mut self,
        path: &std::path::Path,
    ) -> Result<blaze_usercode::trace::Entry, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.entry(path)
    }

    fn file(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.file(path)
    }

    fn dir(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.dir(path)
    }

    fn env(&mut self, name: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
        self.value(name).map(std::ffi::OsStr::new)
    }

    fn env_all(&mut self) -> &[(std::ffi::OsString, std::ffi::OsString)] {
        let (passed, call) = (self.passed, self.call);
        // alloc: a run that read the whole environment: it is sorted once per call, as its key needs.
        self.made
            .get_or_insert_with(|| inputs::Env::new(Environment::seen(passed, call.iter())))
            .vars()
    }

    fn names(
        &mut self,
        path: &std::path::Path,
        out: &mut blaze_usercode::trace::Names<'_>,
    ) -> Result<bool, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.names(path, out)
    }
}

/// What `check` compares a kept expansion by, read in place: its bytes' hash.
pub fn checked(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze v2 proc-macro checked expansion 1", bytes)
}

impl Keeping {
    /// Keeps every proc-macro expansion of its daemon in `expansions`, in the mode [`CACHE`]
    /// says. Says why not when [`CACHE`] has a value that is not a mode.
    pub fn new(expansions: Expansions) -> Result<Keeping, String> {
        let var = |name: &str| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not UTF-8")), // alloc: error path
        };
        let mode = Mode::of(var(CACHE)?.as_deref())?;
        Ok(Keeping {
            expansions,
            mode,
            retractions: Retractions::default(),
            durable: crossbeam_queue::SegQueue::new(),
        })
    }

    /// [`Retractions::now`]: what a warm front end is lowered into only while it is the epoch
    /// the front end was made at.
    pub fn retractions_now(&self) -> u64 {
        self.retractions.now()
    }

    /// Leaves `put` for the store's owner: a [`Keep::Durable`] one in this daemon's own queue,
    /// which the owner's next [`Keeping::publish`] takes, never in a thread's buffer
    /// ([`puts::put`]). So it waits on no thread (a task that gave way, a pool thread no task
    /// ran on, a fiber that moved), and no task's take of its puts carries it off: only the
    /// owner takes it, and gives its ticket to the jobs not ended in the same call. The put
    /// happens before the task that made it ends, which is before its job's end publishes
    /// (`plane_finished`), so the job that made it is among them.
    pub fn put(&self, put: Put) {
        match put.keep {
            Keep::Durable { .. } => self.durable.push(put),
            _ => puts::put(put),
        }
    }

    /// [`puts::publish`] of `puts` and every durable put of this daemon's not published yet
    /// ([`Keeping::put`]), from the store's one owner. When the batch held a durable put, the
    /// epoch moves after the publish ([`Retractions::retracted`], whose invariant this keeps):
    /// a lowering that reads the new epoch finds the record, and one that read the old epoch is
    /// stamped with it and not reused. It moves whatever the put's outcome: a record's result
    /// key is readable before its shape slots are kept (`blaze_usercode::index::keep_in`), so a
    /// put refused as corrupt or past `MOST_SHAPES` may still have made its record visible, and
    /// a front end stamped before it must not be kept. An extra bump costs only a rebuilt front
    /// end; a missing one would keep a retracted value.
    pub fn publish(&self, store: &mut blaze_cas::Store, puts: Vec<Put>) -> Published {
        self.publish_then(store, puts, |_, _| {})
    }

    /// [`Keeping::publish`], calling `before_bump` with the store and the epoch once the puts
    /// are in it and before the epoch moves: what a test checks the order by.
    fn publish_then(
        &self,
        store: &mut blaze_cas::Store,
        mut puts: Vec<Put>,
        before_bump: impl FnOnce(&blaze_cas::Store, u64),
    ) -> Published {
        puts.extend(std::iter::from_fn(|| self.durable.pop()));
        let durable = puts.iter().any(|put| matches!(put.keep, Keep::Durable { .. }));
        let published = puts::publish(store, puts);
        if durable {
            before_bump(store, self.retractions.now());
            self.retractions.retracted();
        }
        published
    }

    /// The hash of the values the names of `shape` (an `ExpansionReads::Traced` shape) have
    /// now, for an expansion given the variables `env`: what `hir_def::deps` files a crate's
    /// image by. The same hash the expansion reported when its values were these.
    pub fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String> {
        reads_with(self.expansions.environment.passed(), Some(&self.expansions.store), env, shape)
    }
}

/// What the shape of the reads of an expansion verified across checkouts starts with
/// ([`verified_reads`]): then the store's key of the record that would retract it
/// ([`blaze_usercode::index::depends_key`]), then the shape.
const VERIFIED_SHAPE: &[u8] = b"blaze v2 verified expansion reads 1\0";

/// The reads an expansion found verified across checkouts reports ([`index::Found::Hit`]'s
/// `verified`): its `shape` and `values`, and whether the store has `depends`, the record that
/// retracts it. Its values change once the record is put, so what `hir_def::deps` filed by
/// them is made again, and the expansion is looked up again (and not given the retracted
/// value).
pub fn verified_reads(
    depends: &blaze_cas::Key,
    shape: &[u8],
    values: ContentId,
) -> (Vec<u8>, ContentId) {
    // alloc: once per verified hit, the shape the query reporting it owns
    let shape = [VERIFIED_SHAPE, &depends.0.0[..], shape].concat();
    (shape, retracted_or_not(values, false))
}

fn retracted_or_not(values: ContentId, retracted: bool) -> ContentId {
    blaze_ids::Hasher::new("blaze v2 verified expansion values 1")
        .id(&values)
        .update(&[u8::from(retracted)])
        .finish()
}

/// [`reads_in`], with the store that says whether a verified expansion's reads
/// ([`verified_reads`]) were retracted since: none (a test that keeps nothing) refuses such a
/// shape, naming why.
fn reads_with(
    passed: &[(String, String)],
    store: Option<&Reader>,
    env: &Env,
    shape: &[u8],
) -> Result<ContentId, String> {
    let Some(rest) = shape.strip_prefix(VERIFIED_SHAPE) else {
        return plain_reads(passed, env, shape);
    };
    let Some((depends, inner)) = rest.split_first_chunk::<32>() else {
        return Err("a verified expansion's reads with no record key".to_owned()); // alloc: error path
    };
    let Some(store) = store else {
        // alloc: error path
        return Err("a verified expansion's reads are worked out with the store, to see whether it was retracted; none is here".to_owned());
    };
    let values = plain_reads(passed, env, inner)?;
    let guard = blaze_cas::enter();
    let depends = blaze_cas::Key(ContentId(*depends));
    let retracted = store.get(&guard, &depends).map_err(|corrupt| corrupt.to_string())?.is_some();
    Ok(retracted_or_not(values, retracted))
}

impl Keeps {
    /// Where its expansions are kept, or the [`NotKept`] a reader that needs a store fails
    /// with.
    pub fn keeping(&self) -> Result<&Keeping, NotKept> {
        match self {
            Keeps::In(keeping) => Ok(keeping),
            Keeps::Nothing(why) => Err(NotKept { why }),
        }
    }

    /// [`Keeping::reads_now`] of its daemon's expansions. A [`Keeps::Nothing`] kept no
    /// expansion, so no shape is of one: that is its [`NotKept`], said.
    pub fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String> {
        // alloc: error path
        self.keeping().map_err(|not| not.to_string())?.reads_now(env, shape)
    }

    /// [`Keeping::retractions_now`] of its daemon's expansions; a [`Keeps::Nothing`] gave no
    /// verified value, so its epoch never moves.
    pub fn retractions_now(&self) -> u64 {
        match self {
            Keeps::In(keeping) => keeping.retractions_now(),
            Keeps::Nothing(_) => 0,
        }
    }

    /// [`Keeping::publish`]; a [`Keeps::Nothing`] has no durable puts of its own, so `puts`
    /// as they are ([`puts::publish`]).
    pub fn publish(&self, store: &mut blaze_cas::Store, puts: Vec<Put>) -> Published {
        match self {
            Keeps::In(keeping) => keeping.publish(store, puts),
            Keeps::Nothing(_) => puts::publish(store, puts),
        }
    }

    /// Counts a record published some other way than [`Keeps::publish`] (a build script's,
    /// `blaze-cargo`'s runs): [`Retractions::retracted`].
    pub fn retracted(&self) {
        if let Keeps::In(keeping) = self {
            keeping.retractions.retracted();
        }
    }
}

/// [`Keeping::reads_now`] for expansions passed `passed`: what a test that keeps no expansions
/// works a shape's values out with. The shape is written against the roots of the call's
/// crate ([`roots`]), as the expansion's was.
pub fn reads_in(passed: &[(String, String)], env: &Env, shape: &[u8]) -> Result<ContentId, String> {
    reads_with(passed, None, env, shape)
}

fn plain_reads(passed: &[(String, String)], env: &Env, shape: &[u8]) -> Result<ContentId, String> {
    let mut seen = Seen { passed, call: env, made: None, none: inputs::Env::default() };
    let mut now = Rooted::new(roots(env), &mut seen);
    match index::values(shape, &mut now) {
        Ok(Some(values)) => Ok(values),
        // A directory that is no longer one: no value it had is this.
        Ok(None) => Ok(ContentId::of("blaze v2 expansion reads gone", shape)),
        Err(FindError::NotAShape { why, .. }) => Err(format!("a shape that does not read: {why}")), // alloc: error path
        Err(FindError::Inputs(unreadable)) => Err(format!("{unreadable}")), // alloc: error path
        Err(FindError::Corrupt(corrupt)) => Err(format!("{corrupt}")),      // alloc: error path
        // Never from `values`, which reads one shape: named all the same.
        Err(FindError::TooManyShapes { run }) => {
            Err(format!("run {run:?} has more than {} shapes", index::MOST_SHAPES))
        } // alloc: error path
    }
}

/// The anchors and syntax contexts of a call's span parts, in the order they first appear: what
/// its output is written against. Its room goes back to the thread's shelf when it is dropped,
/// so the next call's takes no new memory.
#[derive(Default, Debug)]
pub struct Spans {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
}

thread_local! {
    /// The room of the last [`Spans`] dropped on this thread.
    // alloc: none, an empty shelf; what it keeps is grown by the calls.
    static SHELF: Cell<Option<(Vec<SpanAnchor>, Vec<SyntaxContext>)>> = const { Cell::new(None) };
    /// What a key, a kept value and its reading reuse: taken for one of them, so a call made
    /// inside another (an expansion answering a sub-request) gets fresh ones.
    static SCRATCH: Cell<Option<Box<Scratch>>> = const { Cell::new(None) };
}

impl Spans {
    fn shelved() -> Spans {
        let (mut anchors, mut contexts) = SHELF.take().unwrap_or_default();
        anchors.clear();
        contexts.clear();
        Spans { anchors, contexts }
    }
}

impl Drop for Spans {
    fn drop(&mut self) {
        let room = (std::mem::take(&mut self.anchors), std::mem::take(&mut self.contexts));
        // A thread that is ending keeps nothing.
        _ = SHELF.try_with(|shelf| shelf.set(Some(room)));
    }
}

/// A call's span parts, its trees' relative columns and their shapes, and the room a kept tree
/// off a word is copied into: each as long as the longest it held.
#[derive(Default)]
struct Scratch {
    parts: Parts,
    input: Vec<u8>,
    attrs: Vec<u8>,
    shapes: Vec<u8>,
    room: Vec<u8>,
}

fn with_scratch<R>(f: impl FnOnce(&mut Scratch) -> R) -> R {
    // alloc: the first call on a thread, or one inside another, makes its room.
    let mut scratch = SCRATCH.take().unwrap_or_default();
    let done = f(&mut scratch);
    SCRATCH.set(Some(scratch));
    done
}

/// The number of `it` among `all`, in the order first seen: a new one is pushed. A scan: a
/// call's anchors and contexts are few.
pub fn number<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
    let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
        all.push(it);
        all.len() - 1
    });
    at as u32
}

/// The roots an expansion for a crate given the variables `env` is written against
/// ([`blaze_usercode::roots`]): the crate's package (`CARGO_MANIFEST_DIR`) and its `OUT_DIR`,
/// as cargo gives them. A crate with neither has none: its paths stay absolute.
pub fn roots(env: &Env) -> Roots<'_> {
    let get = |name: &str| env.iter().find(|it| it.0 == name).map(|it| it.1);
    let named = [(Role::Package, get("CARGO_MANIFEST_DIR")), (Role::OutDir, get("OUT_DIR"))];
    Roots::new(named.into_iter().filter_map(|(role, path)| Some((role, path?))))
}

/// The roots `roots` (the calling crate's, [`roots`], with their spellings) still in a string
/// `call`'s key is given once ported: its directory and its crate's variables
/// ([`blaze_usercode::roots::Roots::left`]). Such a key is this checkout's string, so the
/// expansion is kept here alone. Asked as an expansion is kept, not as one is found.
pub fn left(call: &Call<'_>, roots: &Roots<'_>) -> RoleSet {
    call.env
        .iter()
        .map(|(_, value)| roots.left(value.as_bytes()))
        .fold(roots.left(call.current_dir.as_bytes()), RoleSet::union)
}

/// One call of a macro, as its key is made of it.
pub struct Call<'a> {
    pub name: &'a str,
    pub kind: ProcMacroKind,
    pub subtree: &'a tt::TopSubtree,
    pub attrs: Option<&'a tt::TopSubtree>,
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
    pub current_dir: &'a str,
}

fn kind_byte(kind: ProcMacroKind) -> u8 {
    match kind {
        ProcMacroKind::CustomDerive => 0,
        ProcMacroKind::Attr => 1,
        ProcMacroKind::Bang => 2,
    }
}

/// What `call` gives the macro, for its [`RunKey::given`] with the toolchain `toolchain`, and
/// the anchors and contexts its output is written against.
///
/// The trees are hashed as their own columns made relative ([`wire::relative`]: each span
/// part an index of the call's, the def, call and mixed sites' first), and the parts as the
/// numbers of their anchors and contexts in the order they first appear, then each site's
/// range and part, so the same tokens in another file or item have the same key. One pass over
/// each tree's columns, into buffers kept per thread: a key allocates nothing.
///
/// [`RunKey::given`]: blaze_usercode::RunKey::given
///
/// The directory it runs in and the crate's variables are written against the crate's
/// [`roots`], which the key is given the roles of, so the call in another checkout has the
/// same key.
pub fn given(toolchain: &ContentId, call: &Call<'_>) -> (Given, Spans) {
    let mut spans = Spans::shelved();
    let mut given = Given::new(RunKind::ProcMacro);
    let roots = roots(call.env);
    given
        .part("name", call.name.as_bytes())
        .part("kind", &[kind_byte(call.kind)])
        .part("abi", &ABI.to_le_bytes())
        .id("toolchain", toolchain)
        .roots(&roots)
        .ported("dir", call.current_dir.as_bytes(), &roots);
    with_scratch(|scratch| {
        let Scratch { parts, input, attrs, shapes, .. } = scratch;
        parts.clear();
        input.clear();
        attrs.clear();
        shapes.clear();
        let sites = [call.def_site, call.call_site, call.mixed_site].map(|site| {
            let part = parts.part(site.anchor, site.ctx);
            [site.range.start().into(), site.range.len().into(), part]
        });
        wire::relative(call.subtree, parts, input);
        if let Some(it) = call.attrs {
            wire::relative(it, parts, attrs);
        }
        for &(anchor, ctx) in parts.all() {
            for it in [number(&mut spans.anchors, anchor), number(&mut spans.contexts, ctx)] {
                shapes.extend_from_slice(&it.to_le_bytes());
            }
        }
        for it in sites.as_flattened() {
            shapes.extend_from_slice(&it.to_le_bytes());
        }
        let attrs: &[u8] = if call.attrs.is_some() { attrs } else { b"-" };
        given
            .part("input columns", input.as_slice())
            .part("attrs columns", attrs)
            .part("span parts", shapes.as_slice());
    });
    given.env_keyed_ported(
        call.env.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())),
        roots,
    );
    (given, spans)
}

/// The output as it is kept: its columns made relative ([`wire::relative`]), each span part an
/// index of the output's own, then the numbers of each part's anchor and context among the
/// call's `spans` (two words), then how many parts there are. `None` when it has an anchor or
/// a context the call has not. The tree is first, so the value's start is its start.
pub fn written(output: &tt::TopSubtree, spans: &Spans) -> Option<Vec<u8>> {
    with_scratch(|scratch| {
        let parts = &mut scratch.parts;
        parts.clear();
        // alloc: the value kept, once as an expansion is kept.
        let mut value = Vec::new();
        wire::relative(output, parts, &mut value);
        for &(anchor, ctx) in parts.all() {
            let anchor = spans.anchors.iter().position(|it| *it == anchor)? as u32;
            let context = spans.contexts.iter().position(|it| *it == ctx)? as u32;
            for it in [anchor, context] {
                value.extend_from_slice(&it.to_le_bytes());
            }
        }
        value.extend_from_slice(&(parts.len() as u32).to_le_bytes());
        Some(value)
    })
}

/// What [`written`] wrote, against the call's `spans`: one checked pass over the columns into
/// the tree, its one allocation, with a tree off a word copied into the thread's room.
pub fn read(bytes: &[u8], spans: &Spans) -> Result<tt::TopSubtree, String> {
    // alloc: error path.
    let cut = || "not a kept expansion: its table of span parts is cut short".to_owned();
    let (rest, count) = bytes.split_last_chunk::<4>().ok_or_else(cut)?;
    let table = (u32::from_le_bytes(*count) as usize)
        .checked_mul(8)
        .filter(|&len| len <= rest.len())
        .ok_or_else(cut)?;
    let (tree, table) = rest.split_at(rest.len() - table);
    with_scratch(|scratch| {
        let Scratch { parts, room, .. } = scratch;
        parts.clear();
        for (at, row) in table.as_chunks::<8>().0.iter().enumerate() {
            let field =
                |i: usize| u32::from_le_bytes(row[i..i + 4].try_into().expect("four bytes"));
            let anchor = spans.anchors.get(field(0) as usize);
            let ctx = spans.contexts.get(field(4) as usize);
            let Some((&anchor, &ctx)) = anchor.zip(ctx) else {
                // alloc: error path.
                return Err(format!(
                    "a kept expansion's span part {at} is of an anchor or context its call has not"
                ));
            };
            if parts.part(anchor, ctx) as usize != at {
                // alloc: error path.
                return Err(format!("a kept expansion's span part {at} is an earlier one again"));
            }
        }
        // alloc: error path.
        wire::read_in(tree, parts, room).map_err(|error| format!("a kept expansion: {error}"))
    })
}

/// The first expansion that failed for a reason that is not the macro's own (the store
/// refused what it kept): what is said of it.
static SERVER_FAILED: OnceLock<String> = OnceLock::new();

fn system(said: &str) {
    // alloc: error path, said once and kept
    if SERVER_FAILED.get().is_none() && SERVER_FAILED.set(said.to_owned()).is_ok() {
        eprintln!("proc macros: {said}");
    }
}

/// What is said of the first expansion in this process that failed for a reason that is not
/// the macro's own, when one did. What is built from it is not the program: a build fails
/// with this, at the point it is first seen (no-fallback).
pub fn server_failed() -> Option<&'static str> {
    SERVER_FAILED.get().map(String::as_str)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget {
    use blaze_cas::{Config, Store};
    use blaze_core::{
        FileName, Traced, run_until_idle,
        sim::{Faults, Sim},
    };
    use blaze_usercode::{RunKey, index::Found};

    use super::*;

    /// A call whose expansion is kept is found through [`Seen`], each variable its shape names
    /// looked up where it is and each file stamped, with no allocation: the hit is read in
    /// place.
    #[cfg(unix)]
    #[test]
    fn a_kept_expansion_is_found_with_no_allocation() {
        let mut sim = Sim::new(5, Faults::default());
        let mut effects = Vec::new();
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        }).expect("the effects end");
        assert!(store.ready());
        let lib = std::env::temp_dir().join(format!("blaze-kept-budget-{}", std::process::id()));
        std::fs::write(&lib, b"a macro's library").unwrap();
        let hash = inputs::hash_reader(b"a macro's library".as_slice()).unwrap();
        let run = RunKey {
            code: ContentId::of("blaze v2 test 1", b"macro"),
            given: ContentId::default(),
        };
        let traced = Traced {
            files: vec![(lib.clone(), Some(hash))],
            env: vec![
                ("CARGO_PKG_NAME".into(), Some("a".into())),
                ("PATH".into(), Some("/bin".into())),
                ("UNSET".into(), None),
            ],
            ..Traced::default()
        };
        index::keep(&mut store, &run, &traced, b"expansion").unwrap();
        let passed = [("PATH".to_owned(), "/bin".to_owned())];
        let call: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
        let mut seen =
            Seen { passed: &passed, call: &call, made: None, none: inputs::Env::default() };
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let values = index::VALUES.traced(&traced);
        let mut find = || {
            matches!(
                index::find_reads(&reader, &guard, &run, &mut seen),
                Ok(Found::Hit { value: b"expansion", values: Some(found), .. }) if found == values
            )
        };
        assert!(find());
        assert!(blaze_alloc_count::assert_at_most(
            0,
            "a kept expansion found through Seen",
            &mut find
        ));
        drop(guard);
        _ = std::fs::remove_file(&lib);
    }

    /// What an expansion that read a file reports is what [`reads_in`] works out of its shape
    /// while the file is as it was, found again on a hit, and not once the file has changed.
    #[cfg(unix)]
    #[test]
    fn the_reads_an_expansion_reports_are_worked_out_again_until_a_file_changes() {
        let mut sim = Sim::new(5, Faults::default());
        let mut effects = Vec::new();
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        }).expect("the effects end");
        assert!(store.ready());
        let read = std::env::temp_dir().join(format!("blaze-kept-reads-{}", std::process::id()));
        std::fs::write(&read, b"included").unwrap();
        let hash = inputs::hash_reader(b"included".as_slice()).unwrap();
        let traced = Traced {
            files: vec![(read.clone(), Some(hash))],
            env: vec![("CARGO_PKG_NAME".into(), Some("a".into())), ("UNSET".into(), None)],
            ..Traced::default()
        };
        let reported = index::VALUES.traced(&traced);
        let shape = index::Shape::of(&traced).encode();
        let call: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
        assert_eq!(reads_in(&[], &call, &shape), Ok(reported));

        let run = RunKey {
            code: ContentId::of("blaze v2 test 1", b"reads"),
            given: ContentId::default(),
        };
        index::keep(&mut store, &run, &traced, b"expansion").unwrap();
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let mut seen = Seen { passed: &[], call: &call, made: None, none: inputs::Env::default() };
        match index::find_reads(&reader, &guard, &run, &mut seen) {
            Ok(Found::Hit { shape: found, values, .. }) => {
                assert_eq!(found, shape.as_slice());
                assert_eq!(values, Some(reported));
            }
            other => panic!("{other:?}"),
        }
        drop(guard);

        let other: Env = [("CARGO_PKG_NAME".to_owned(), "b".to_owned())].into_iter().collect();
        assert_ne!(reads_in(&[], &other, &shape), Ok(reported));
        std::fs::write(&read, b"included, and changed").unwrap();
        assert_ne!(reads_in(&[], &call, &shape), Ok(reported));
        _ = std::fs::remove_file(&read);
    }
}
