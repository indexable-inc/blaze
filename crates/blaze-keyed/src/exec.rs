//! Who makes a record: the executor, the driver's second policy type parameter
//! (`Driver<M: Memo, X: Executor>`), static like the memo.
//!
//! A task is an action: a plain value of (kind, key, the records it may read). The task's code is
//! handed only an [`ActionReader`] over those inputs, so it can read nothing else, and the same
//! code runs on a local worker or on a remote one that has only those records. Results are
//! records, filed in the store (remotely: returned as segments, not one object per record).
//!
//! The rule of where an action runs is the driver's, not the executor's
//! ([`crate::driver::stays_local`]): a key task, and every task of a request whose misses are
//! under [`LOCAL_TASKS`], run locally, because a round trip costs more than the task; a cold
//! fan-out goes remote in batches.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_ids::ContentId;

use crate::{
    kind::{Key, Kind, Tag},
    pool::Ticket,
    proof::{Holds, Snapshot},
    store::{Store, StoreError},
};

/// Under this many tasks a request runs all of them locally: the edit loop never waits on the
/// network.
pub const LOCAL_TASKS: u32 = 64;

/// One record, by kind and key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordRef {
    pub tag: Tag,
    pub key: ContentId,
}

impl RecordRef {
    pub fn of<K: Kind>(key: &Key<K>) -> RecordRef {
        RecordRef { tag: K::TAG, key: key.id() }
    }
}

/// What to make: the record of `made`, from exactly `inputs` (sorted).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Action<'a> {
    pub made: RecordRef,
    pub inputs: &'a [RecordRef],
}

/// A read outside the action's inputs: refused, naming the record and the action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotAnInput {
    pub asked: RecordRef,
    pub action: RecordRef,
}

impl fmt::Display for NotAnInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the action making kind {} {:?} read kind {} {:?}, which is not one of its inputs",
            self.action.tag.0, self.action.key, self.asked.tag.0, self.asked.key
        )
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReadError {
    NotAnInput(NotAnInput),
    Store(StoreError),
    /// A proved input the store no longer holds: the action cannot run here.
    Missing(RecordRef),
}

/// The only way a task reads: records named in its action, nothing else, each by its proof.
///
/// The invariants: every byte a task's output depends on came through [`ActionReader::read`] of
/// a record its action names; so the action's key and inputs determine the output, wherever it
/// ran. A read takes the input's [`Holds`] of this snapshot, so nothing is read that was not
/// proved, and nothing proved is checked again (the bytes come from a proved segment).
pub struct ActionReader<'a, 's, S: Store> {
    store: &'a S,
    action: Action<'a>,
    snap: Snapshot<'s>,
}

impl<'a, 's, S: Store> ActionReader<'a, 's, S> {
    pub fn new(store: &'a S, action: Action<'a>, snap: Snapshot<'s>) -> ActionReader<'a, 's, S> {
        ActionReader { store, action, snap }
    }

    pub fn action(&self) -> Action<'a> {
        self.action
    }

    pub fn snapshot(&self) -> Snapshot<'s> {
        self.snap
    }

    pub fn read<K: Kind, A: Allocator>(&self, input: &Holds<'s, K>, frame: A) -> Result<S::Held<A>, ReadError> {
        let key = input.key();
        let asked = RecordRef::of(&key);
        if self.action.inputs.binary_search(&asked).is_err() {
            return Err(ReadError::NotAnInput(NotAnInput { asked, action: self.action.made }));
        }
        let read = self.store.read(&key, frame).map_err(ReadError::Store)?;
        read.map(|read| read.bytes).ok_or(ReadError::Missing(asked))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ExecError {
    /// The remote service refused or failed, by name.
    Remote { what: &'static str, status: u32 },
}

/// Who runs actions: a policy. Static: the driver is monomorphized per executor, and nothing is
/// checked per action but the driver's own placement rule. The host submits local actions to its
/// [`crate::pool::Pool`] and remote ones through [`RemoteService`].
pub trait Executor {
    const NAME: &'static str;
    /// Whether actions may leave the machine: false folds every placement branch away.
    const REMOTE: bool;

    /// Actions that may be in flight at once.
    fn capacity(&self) -> u32;
}

/// The local pool: actions run on this machine's workers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Local {
    pub workers: u32,
}

impl Executor for Local {
    const NAME: &'static str = "local";
    const REMOTE: bool = false;

    fn capacity(&self) -> u32 {
        self.workers.max(1)
    }
}

/// What a remote execution service implements: it runs the same task code hermetically, from
/// the action's inputs only, files the results in the shared store under its namespace, and
/// answers with the segments that hold them.
pub trait RemoteService: Executor {
    /// Sends a batch of actions in one round trip.
    fn send(&self, batch: &[(Ticket, Action<'_>)]) -> Result<(), ExecError>;
}
