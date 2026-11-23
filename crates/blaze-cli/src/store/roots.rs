//! What keeps a record after its command (runtime.md design point 1 and row C5; blaze-cas
//! plain.rs, "Compaction"): under a reach sweep (`Liveness::Reached`) a record lives while an
//! unexpired root reaches it, so every record a later command reads must be named by a root
//! of its LIFETIME CLASS, and the class is a type (rule 76): a writer makes a record visible
//! after its command only through the one function of its class, which hands back
//! [`Rooted<C>`], the proof of which root names it and so which event lets it go.
//!
//! The classes:
//! - [`Answer`]: a checkout's answer for one request shape (the drive's sealed last answer,
//!   the engine's pointer, pages and task records, the rows' segments): named by the checkout's
//!   roots (`PlainPlace::pin`), let go of when the checkout is not built for blaze-cas
//!   `ROOT_LIFE_NS`.
//! - [`Program`]: a program's compiled code per (program, request shape) and user, like the
//!   start list it names: the code store's entries and pointers the run reached, and the start
//!   list's pointer and record ([`keep_program`]), named through ONE index record by the root
//!   [`ProgramRoot`] (made from the list's name), so a program that reached 100,000 code
//!   records costs a root of one key; renewed by a run of that program from any checkout, let
//!   go of when no checkout runs it for blaze-cas `ROOT_LIFE_NS`. It names the certificate's
//!   run tables too (`run_ids`, `run_names`: [`start_list::Today::tables`]).
//! - [`Toolchain`]: the toolchain's records for every project on it, by the root `tool.<id>`
//!   ([`keep_toolchain`]): std's compiled code (`CodeClass::Toolchain` writes), and from the
//!   engine's answer std's names records with the expansion records beside them; renewed by
//!   every command that reaches them, let go of after [`TOOLCHAIN_LIFE_NS`] unrenewed.
//!
//! The ENGINE path's records, each with the root that reaches it (the census):
//! - each standing row's task record (names, tree, outlines, heads, body results, lint
//!   findings, the unit graph), the answer's pages and the printed lines: the answer's
//!   `.pages` root, through one index (engine_answer.rs `save`, `CommandStore::pin_through`);
//! - a names record's expansion records (`ReservedTag::Expansion`): the index filed beside the names
//!   record (engine_store.rs `put_all`, blaze-cas `Plain::index_beside`), named by the same
//!   root; std's also by the toolchain root;
//! - the sealed pointer: the answer's root itself (`store.pin(name, ..)`);
//! - dependency heads: `engine-dependency-heads.<hex>`, one per tool set (so per toolchain),
//!   renewed by every request that lists heads (engine_seeds.rs);
//! - the rows-form answer's segments and roots: its checkout and tree roots
//!   (engine_answer_rows.rs `write_effects`).
//!
//! What a POINTER is to the sweep (blaze-keyed `ReservedTag::Pointer`; blaze-cas core.rs `live_of`, `copy_live`): a
//! record under its NAME's key, live only when a root reaches that name, and then only its
//! newest value; its TARGET is not followed. So a code key that is a pointer is kept by naming
//! both the code key and the content key it points at: [`keep_program`] names each (the walk's
//! `pointed` hits record both, a start-list row is resolved through `Lent::pointed` at exit).
//!
//! The program's keys and the start list are ONE fact (rule 30): the start list written at exit
//! is read back by [`start_list::rooted_keys`], which names the list's own pointer and record
//! and each code row's key and target; the walk's keys are what [`HomeCodeStore`] served or
//! made this run (it records every hit and every put as it answers them: the only way code
//! reaches the store). Nothing else decides what the program root names.
//!
//! [`HomeCodeStore`]: crate::code_store::HomeCodeStore

use core::fmt;
use core::marker::PhantomData;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, ROOT_LIFE_NS, Sliced};
use blaze_keyed::kind::{TagReach, reach_of};
use blaze_core::Driver;
use blaze_toolchain::ToolchainId;

use crate::start_list;
use crate::store::plain_place::StoreAlloc;

/// A lifetime class: which root names a record of it, so which event lets it go.
pub(crate) trait Class {
    /// The class in words, for a refusal and the cleanup line.
    const WHAT: &'static str;
}

/// A checkout's answer for one request shape (class (a) of the census).
#[derive(Debug)]
pub(crate) enum Answer {}

/// A program's code under one checkout and request shape (class (b)).
#[derive(Debug)]
pub(crate) enum Program {}

impl Class for Answer {
    const WHAT: &'static str = "a checkout's answer";
}

impl Class for Program {
    const WHAT: &'static str = "a program's code";
}

/// Code and records settled per toolchain: std's compiled code, the getter and ender (class
/// (c)), kept by the toolchain's root ([`keep_toolchain`]).
#[derive(Debug)]
pub(crate) enum Toolchain {}

impl Class for Toolchain {
    const WHAT: &'static str = "the toolchain's code";
}

/// A test suite's kept incremental results, head, postings and state (test_cache.rs).
#[derive(Debug)]
pub(crate) enum TestSuite {}

impl Class for TestSuite {
    const WHAT: &'static str = "a test suite's kept results";
}

/// The proof that a root of class `C` names some keys from the next commit on: only the
/// functions of this module and `PlainPlace::pin`/`pin_through` make one (rule 76).
#[derive(Debug)]
pub(crate) struct Rooted<C: Class> {
    /// The root's record key.
    #[allow(dead_code, reason = "the proof's root, read by the reach tests; the cleanup line counts roots from the store (blaze-cas `Plain::named`)")]
    pub(crate) root: CasKey,
    class: PhantomData<C>,
}

impl<C: Class> Rooted<C> {
    pub(crate) fn new(root: CasKey) -> Rooted<C> {
        Rooted { root, class: PhantomData }
    }
}

/// A root's name: a five-byte class word and the 64 hex of the 32-byte id it is kept by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RootName([u8; ROOT_NAME_LEN]);

const ROOT_NAME_LEN: usize = 5 + 64;

impl RootName {
    /// The program root of the start list `list`: `code.` and the 64 hex of the list name's key,
    /// so made from the same parts as [`start_list::ListName`] (the request shape, which holds
    /// the command and the program), per user and NOT per checkout (rule 30: the root and the
    /// list it names are one fact with one lifetime). Any run of that program from any checkout
    /// renews it; code records are keyed from text and shared across checkouts anyway.
    pub(crate) fn program(list: &start_list::ListName) -> RootName {
        RootName::of(b"code.", &list.id().0)
    }

    /// The toolchain root of `toolchain`: `tool.` and the 64 hex of its id. Every project on one
    /// toolchain shares it, so std's compiled code made for one is found by the next.
    pub(crate) fn toolchain(toolchain: ToolchainId) -> RootName {
        RootName::of(b"tool.", &toolchain.0)
    }

    /// The floor root of the published set `set` (blaze-remote published/set.rs `SetName::id`):
    /// `pubf.` and the 64 hex of the id. It names the one record holding the highest
    /// generation this home has taken for the set (store/published.rs `keep_floor`), kept as
    /// long as the toolchain's records are.
    pub(crate) fn published_floor(set: &blaze_ids::ContentId) -> RootName {
        RootName::of(b"pubf.", &set.0)
    }

    /// A test suite's kept results root: `test.` and the 64 hex of the suite's id.
    pub(crate) fn test_suite(suite: &blaze_ids::ContentId) -> RootName {
        RootName::of(b"test.", &suite.0)
    }

    fn of(word: &[u8; 5], id: &[u8; 32]) -> RootName {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = [0u8; ROOT_NAME_LEN];
        name[..5].copy_from_slice(word);
        for (i, byte) in id.iter().enumerate() {
            name[5 + 2 * i] = HEX[usize::from(byte >> 4)];
            name[5 + 2 * i + 1] = HEX[usize::from(byte & 15)];
        }
        RootName(name)
    }

    /// The root's name: ASCII by construction.
    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("a root's name is ASCII by construction")
    }
}

/// Why a root could not be set, by class and name (rule 19).
#[derive(Debug)]
pub enum RootError {
    /// The store refused the index or the root.
    Store { class: &'static str, root: RootName, why: LentError },
    /// The start list's keys could not be read back.
    List(start_list::ListError),
    /// The root named `root` holds a record that is not an indexed root's, or names an index
    /// record of another tag: its keys cannot be carried into the renewed root.
    NotIndexed { class: &'static str, root: RootName, key: CasKey },
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootError::Store { class, root, why } => write!(f, "the root `{}` of {class} was not set: {why}", root.as_str()),
            RootError::List(why) => write!(f, "the program's root: {why}"),
            RootError::NotIndexed { class, root, key } => {
                write!(f, "the root `{}` of {class} names {:?}, which is not an indexed root or an index record (blaze-keyed `ReservedTag::IndexedRoot`, `ReservedTag::Index`)", root.as_str(), key.0)
            }
        }
    }
}

impl core::error::Error for RootError {}

/// What [`keep_program`] named.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ProgramKept {
    /// Of the start list's code rows, those whose key the store no longer holds a target for:
    /// named by key alone, counted (rule 3: nothing quietly left out).
    pub(crate) stale_rows: u32,
}

/// The program's code kept for the next command of this program and request shape: one index
/// of every key the walk served or made (`walked`, the code store's record of this run), every
/// key the start list just written names (`list`: its bytes, `None` when the run kept no list)
/// and the certificate's run tables (`today.tables`), named by the program root of
/// `today.name` ([`RootName::program`]) for blaze-cas `ROOT_LIFE_NS` (renewed by each run that
/// names it again). Called once a run, after the list is written and before the commit after
/// the answer: the index, the root and the run's records ride that one flip.
pub(crate) fn keep_program<D: Driver>(lent: &Lent<D, StoreAlloc>, today: &start_list::Today, list: Option<&[u8]>, walked: &[CasKey]) -> Result<(Rooted<Program>, ProgramKept), RootError> {
    let root = RootName::program(&today.name);
    let mut keys: std::vec::Vec<CasKey, StoreAlloc> = std::vec::Vec::with_capacity_in(walked.len() + 64, *lent.alloc());
    keys.extend_from_slice(walked);
    keys.extend_from_slice(&today.tables);
    let mut kept = ProgramKept::default();
    if let Some(bytes) = list {
        kept.stale_rows = start_list::rooted_keys(lent, &today.name, bytes, &mut keys).map_err(RootError::List)?;
    }
    // The index sorts and dedups (blaze-cas `Plain::index`); its cap is the store's refusal.
    let store = |why| RootError::Store { class: Program::WHAT, root, why };
    let index = lent.index(&keys).map_err(store)?;
    let at = lent.root_through(root.as_str(), &[index], ROOT_LIFE_NS).map_err(store)?;
    Ok((Rooted::new(at), kept))
}

/// Pins a test suite's kept incremental state (pointer, state record, head, postings, result
/// records) under `test.<id>` for [`ROOT_LIFE_NS`].
pub(crate) fn keep_test_suite<D: Driver>(lent: &Lent<D, StoreAlloc>, suite: &blaze_ids::ContentId, keys: &[CasKey]) -> Result<Rooted<TestSuite>, RootError> {
    let root = RootName::test_suite(suite);
    let store = |why| RootError::Store { class: TestSuite::WHAT, root, why };
    let index = lent.index(keys).map_err(store)?;
    let at = lent.root_through(root.as_str(), &[index], ROOT_LIFE_NS).map_err(store)?;
    Ok(Rooted::new(at))
}

/// A toolchain root's life: twice a program's, and renewed by every command that reaches a
/// record of its class (decided 2026-11-17).
pub(crate) const TOOLCHAIN_LIFE_NS: u64 = 2 * ROOT_LIFE_NS;

/// The most index records a toolchain root names before they are folded into one: each command
/// that makes toolchain code adds one, so the root stays one record of at most this many keys.
pub(crate) const TOOLCHAIN_INDEXES_FOLD: usize = 64;

/// The toolchain's code kept for every later command on `toolchain`, whichever program or
/// project: the root `tool.<id>` ([`RootName::toolchain`]) names the index records it named
/// before (an expired root names nothing: its records are gone or going) and, when this
/// command made toolchain code (`made`, the code store's toolchain-class writes), one more
/// index of those keys; past [`TOOLCHAIN_INDEXES_FOLD`] indexes they are folded into one.
/// Set again for [`TOOLCHAIN_LIFE_NS`] by every command that calls it (every `blaze run`: a run
/// reaches the toolchain's getter and ender at the least): equal bytes within a day of
/// the last renewal write nothing (blaze-cas `set_root`). Answers `None` when there is nothing
/// to name: no earlier root and nothing made. `beside`: index records made with records of
/// `made` (blaze-cas `Plain::index_beside`: a names record's expansion records), named by the
/// root as they are, so reach keeps what they hold too (one level).
pub(crate) fn keep_toolchain<D: Driver>(lent: &Lent<D, StoreAlloc>, toolchain: ToolchainId, made: &[CasKey], beside: &[CasKey]) -> Result<Option<Rooted<Toolchain>>, RootError> {
    let root = RootName::toolchain(toolchain);
    let store = |why| RootError::Store { class: Toolchain::WHAT, root, why };
    let not_indexed = |key| RootError::NotIndexed { class: Toolchain::WHAT, root, key };
    let mut indexes: std::vec::Vec<CasKey, StoreAlloc> = std::vec::Vec::new_in(*lent.alloc());
    if let Some(key) = lent.root_named(root.as_str()) {
        let Some(held) = lent.get(&key).map_err(store)? else { return Err(not_indexed(key)) };
        if reach_of(held.tag) != TagReach::Indexes {
            return Err(not_indexed(key));
        }
        indexes.extend(held.bytes.chunks_exact(32).map(|it| CasKey(blaze_ids::ContentId(it.try_into().expect("32 bytes")))));
    }
    if !made.is_empty() {
        indexes.push(lent.index(made).map_err(store)?);
    }
    indexes.extend_from_slice(beside);
    // An equal index made again (the same keys, the same key) is one index (rule 25).
    indexes.sort_unstable();
    indexes.dedup();
    if indexes.is_empty() {
        return Ok(None);
    }
    if indexes.len() > TOOLCHAIN_INDEXES_FOLD {
        // One index of every key the indexes hold: its cap is the store's refusal.
        let mut keys: std::vec::Vec<CasKey, StoreAlloc> = std::vec::Vec::new_in(*lent.alloc());
        for index in &indexes {
            let Some(held) = lent.get(index).map_err(store)? else { return Err(not_indexed(*index)) };
            if reach_of(held.tag) != TagReach::IndexKeys {
                return Err(not_indexed(*index));
            }
            keys.extend(held.bytes.chunks_exact(32).map(|it| CasKey(blaze_ids::ContentId(it.try_into().expect("32 bytes")))));
        }
        indexes.clear();
        indexes.push(lent.index(&keys).map_err(store)?);
    }
    let at = lent.root_through(root.as_str(), &indexes, TOOLCHAIN_LIFE_NS).map_err(store)?;
    Ok(Some(Rooted::new(at)))
}

/// The command's cleanup line (the closing profile's, phases.rs, and `--stats`): the roots the
/// head names unexpired, the keys this command's roots name (and how many through index
/// records), what its one slice swept, the records its commit carried forward, and the leases
/// held.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct CleanupLine {
    pub roots: u32,
    pub keys: u64,
    pub indexed: u64,
    pub swept_bytes: u64,
    pub slices: u32,
    pub carried: u64,
    pub leases: u32,
    /// The put path's tally (blaze-cas `PutTally`, rule 16): up to the commit after the answer
    /// (the phases' commits and every copy before it), and from it on (that commit, the
    /// cleanup's merges, the seal's barriers). Set by plain_place.rs `after_answer`.
    pub put: PutLine,
}

/// The two stretches of a command's put path ([`CleanupLine::put`]).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct PutLine {
    pub before_answer: blaze_cas::plain::PutTally,
    pub after_answer: blaze_cas::plain::PutTally,
}

impl fmt::Display for CleanupLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cleanup      roots: {} named {} keys ({} through index records); swept {} bytes in {} slice{}; carried {}; leases {}",
            self.roots,
            self.keys,
            self.indexed,
            self.swept_bytes,
            self.slices,
            if self.slices == 1 { "" } else { "s" },
            self.carried,
            self.leases,
        )?;
        write!(f, "\nstore put    before the answer's commit: {}\nstore put    from the answer's commit on: {}", self.put.before_answer, self.put.after_answer)
    }
}

impl CleanupLine {
    /// The line of the command whose store is `lent`, after its commit: the roots it set and
    /// the keys they name (blaze-cas `Plain::named`), its folded slice (`slice`, the commit's
    /// `take_sliced`: an aborted one swept nothing), what its commits carried forward, and the
    /// leases the head names (each flip drops the expired ones).
    pub(crate) fn of<D: Driver>(lent: &Lent<D, StoreAlloc>, slice: Option<Sliced>) -> CleanupLine {
        let named = lent.named();
        let (swept_bytes, slices) = match slice {
            Some(slice) if slice.segments > 0 && !slice.aborted => (slice.bytes_in.saturating_sub(slice.bytes_out), 1),
            _ => (0, 0),
        };
        CleanupLine {
            roots: named.roots,
            keys: named.keys,
            indexed: named.indexed,
            swept_bytes,
            slices,
            carried: lent.carried(),
            leases: u32::try_from(lent.leases().len()).unwrap_or(u32::MAX),
            put: PutLine::default(),
        }
    }

    /// The part of the line known BEFORE the commit after the answer, for a profile written
    /// before it (`check` and `run` on the old path, main.rs): the roots this command set and
    /// the keys they name, and the leases. The slice and what the commit carries are known only
    /// after it: on `--stats` after the commit, and whole in a profile written after it (the
    /// engine's, engine_check.rs).
    pub(crate) fn before_commit<D: Driver>(lent: &Lent<D, StoreAlloc>) -> BeforeCommit {
        BeforeCommit(CleanupLine::of(lent, None))
    }
}

/// [`CleanupLine::before_commit`]: printed so it never says a slice ran or swept nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeforeCommit(pub CleanupLine);

impl fmt::Display for BeforeCommit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let it = &self.0;
        write!(
            f,
            "cleanup      before the commit: roots: {} named {} keys ({} through index records); leases {}; the slice and what the commit carried come after this profile (`--stats`)",
            it.roots, it.keys, it.indexed, it.leases,
        )
    }
}
