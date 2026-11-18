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
//!   go of when no checkout runs it for blaze-cas `ROOT_LIFE_NS`.
//!
//! What a POINTER is to the sweep (blaze-cas core.rs `POINTER_TAG`, `live_of`, `copy_live`): a
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
use std::alloc::Global;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, ROOT_LIFE_NS, Sliced};
use blaze_core::Driver;

use crate::start_list;

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

/// The proof that a root of class `C` names some keys from the next commit on: only the
/// functions of this module and `PlainPlace::pin` make one (rule 76).
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

/// A program's root for one request shape: `code.<64 hex of the start list's name>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ProgramRoot([u8; PROGRAM_ROOT_LEN]);

const PROGRAM_WORD: &[u8; 5] = b"code.";
const PROGRAM_ROOT_LEN: usize = 5 + 64;

impl ProgramRoot {
    /// The program root of the start list `list`: `code.` and the 64 hex of the list name's key,
    /// so made from the same parts as [`start_list::ListName`] (the request shape, which holds
    /// the command and the program), per user and NOT per checkout (rule 30: the root and the
    /// list it names are one fact with one lifetime). Any run of that program from any checkout
    /// renews it; code records are keyed from text and shared across checkouts anyway.
    pub(crate) fn of(list: &start_list::ListName) -> ProgramRoot {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = [0u8; PROGRAM_ROOT_LEN];
        name[..5].copy_from_slice(PROGRAM_WORD);
        for (i, byte) in list.id().0.iter().enumerate() {
            name[5 + 2 * i] = HEX[usize::from(byte >> 4)];
            name[5 + 2 * i + 1] = HEX[usize::from(byte & 15)];
        }
        ProgramRoot(name)
    }

    /// The root's name: ASCII by construction.
    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("a program root's name is ASCII hex by construction")
    }
}

/// Why a root could not be set, by class and name (rule 19).
#[derive(Debug)]
pub(crate) enum RootError {
    /// The store refused the index or the root.
    Store { class: &'static str, root: ProgramRoot, why: LentError },
    /// The start list's keys could not be read back.
    List(start_list::ListError),
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootError::Store { class, root, why } => write!(f, "the root `{}` of {class} was not set: {why}", root.as_str()),
            RootError::List(why) => write!(f, "the program's root: {why}"),
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
/// of every key the walk served or made (`walked`, the code store's record of this run) and
/// every key the start list just written names (`list`: its bytes, `None` when the run kept no
/// list), named by the program root of `name` ([`ProgramRoot::of`]) for blaze-cas `ROOT_LIFE_NS` (renewed by each run that
/// names it again). Called once a run, after the list is written and before the commit after
/// the answer: the index, the root and the run's records ride that one flip.
pub(crate) fn keep_program<D: Driver>(lent: &Lent<D, Global>, name: &start_list::ListName, list: Option<&[u8]>, walked: &[CasKey]) -> Result<(Rooted<Program>, ProgramKept), RootError> {
    let root = ProgramRoot::of(name);
    let mut keys: std::vec::Vec<CasKey, Global> = std::vec::Vec::with_capacity_in(walked.len() + 64, Global);
    keys.extend_from_slice(walked);
    let mut kept = ProgramKept::default();
    if let Some(bytes) = list {
        kept.stale_rows = start_list::rooted_keys(lent, name, bytes, &mut keys).map_err(RootError::List)?;
    }
    // The index sorts and dedups (blaze-cas `Plain::index`); its cap is the store's refusal.
    let store = |why| RootError::Store { class: Program::WHAT, root, why };
    let index = lent.index(&keys).map_err(store)?;
    let at = lent.root_through(root.as_str(), &[index], ROOT_LIFE_NS).map_err(store)?;
    Ok((Rooted::new(at), kept))
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
        )
    }
}

impl CleanupLine {
    /// The line of the command whose store is `lent`, after its commit: the roots it set and
    /// the keys they name (blaze-cas `Plain::named`), its folded slice (`slice`, the commit's
    /// `take_sliced`: an aborted one swept nothing), what its commits carried forward, and the
    /// leases the head names (each flip drops the expired ones).
    pub(crate) fn of<D: Driver>(lent: &Lent<D, Global>, slice: Option<Sliced>) -> CleanupLine {
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
        }
    }
}
