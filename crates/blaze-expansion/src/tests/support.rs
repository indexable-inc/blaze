//! What the tests share: an in-memory store of the core's [`Store`], a source of draws (Hegel's
//! or a seed's), and the outcome of a simulated run.

use std::{
    alloc::{Allocator, Global},
    cell::{Cell, RefCell},
    fmt,
};

use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Key, Kind, Namespace, ToolId},
    outside::OutsideError,
    store::{Compacted, Generation, InProcess, Put as Filing, Read, RootRef, Store, StoreError},
};
use blaze_rng::SplitMix64;
use hegel::{TestCase, generators as gs};

use crate::machine::Refusal;

/// Seeds a control runs before it says its plant went uncaught.
pub(crate) const CONTROL_SEEDS: u64 = 128;

/// The core's store in memory, every kind in one table by (tag, key), each record immutable
/// once filed. Single-threaded, as the simulations are.
pub(crate) struct MemStore {
    records: RefCell<HashMap<(u16, [u8; 32]), Aligned, Global>>,
    /// Records written (not those already there).
    puts: Cell<usize>,
}

impl MemStore {
    pub(crate) fn new() -> MemStore {
        MemStore { records: RefCell::new(HashMap::new_in(Global)), puts: Cell::new(0) }
    }

    pub(crate) fn puts(&self) -> usize {
        self.puts.get()
    }
}

impl Store for MemStore {
    type Held<A: Allocator> = Aligned;
    type Verified = InProcess;

    fn read<K: Kind, A: Allocator>(
        &self,
        key: &Key<K>,
        _frame: A,
    ) -> Result<Option<Read<InProcess, Aligned>>, StoreError> {
        let records = self.records.borrow();
        Ok(records.get(&(K::TAG.0, key.id().0)).map(|bytes| Read { bytes: bytes.clone(), from: InProcess }))
    }

    fn map<K: Kind>(&self, _key: &Key<K>) -> Result<Option<Read<InProcess, &[u8]>>, StoreError> {
        panic!("MemStore::map: no kind of this crate is Plain")
    }

    /// Every namespace in one table: the simulations file under one namespace ([`NAMESPACE`]).
    fn put<K: Kind>(&self, key: &Key<K>, _namespace: Namespace, bytes: &[u8]) -> Result<Filing, StoreError> {
        let mut records = self.records.borrow_mut();
        if let Some(filed) = records.get(&(K::TAG.0, key.id().0)) {
            return Ok(if &**filed == bytes { Filing::Same } else { Filing::Other });
        }
        records.insert((K::TAG.0, key.id().0), Aligned::copy_from(bytes));
        self.puts.set(self.puts.get() + 1);
        Ok(Filing::Written)
    }

    fn commit(&self) -> Result<Generation, StoreError> {
        Ok(Generation(0))
    }

    fn compact(&self, _roots: &[RootRef], _cap: u64) -> Result<Compacted, StoreError> {
        panic!("MemStore::compact: not used by these tests")
    }
}

/// The namespace every simulated record is filed in: a machine's own.
pub(crate) const NAMESPACE: Namespace = Namespace::LOCAL;

/// The toolchain every simulated call and run is keyed with.
pub(crate) fn tool() -> ToolId {
    ToolId {
        tool: ContentId::of("test tool", b"blaze"),
        target: ContentId::of("test target", b"aarch64-apple-darwin"),
        namespace: Namespace::PUBLIC,
    }
}

/// A refusal from filing a run's record, as a simulation sees it: other bytes at an address
/// are two values under one key and answers (the oracle's to judge); anything else is the
/// harness's.
pub(crate) fn filing(refusal: Refusal) -> Broke {
    match refusal {
        Refusal::TwoValues { .. } | Refusal::Outside { error: OutsideError::TwoValues { .. }, .. } => {
            Broke::Oracle(refusal.to_string())
        }
        other => Broke::Harness(other.to_string()),
    }
}

/// A source of draws: Hegel's (shrunk and replayed by Hegel) or a seed's (the controls).
pub(crate) trait Draw {
    /// In `lo..=hi`.
    fn int(&mut self, lo: u64, hi: u64) -> u64;

    fn coin(&mut self) -> bool {
        self.int(0, 1) == 1
    }

    /// In `0..n`, `n` at least one.
    fn under(&mut self, n: usize) -> usize {
        self.int(0, n as u64 - 1) as usize
    }
}

pub(crate) struct Drawn<'a>(pub(crate) &'a TestCase);

impl Draw for Drawn<'_> {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        self.0.draw(gs::integers::<u64>().min_value(lo).max_value(hi))
    }
}

impl Draw for SplitMix64 {
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        match (hi - lo).checked_add(1) {
            Some(n) => lo + SplitMix64::below(self, n),
            None => self.next_u64(),
        }
    }
}

/// How a simulated run broke: its oracle saw the invariant fail, or the harness itself did.
#[derive(Debug)]
pub(crate) enum Broke {
    Oracle(String),
    Harness(String),
}

impl fmt::Display for Broke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
            Broke::Harness(why) => write!(f, "the harness: {why}"),
        }
    }
}

/// Runs `run` on seeded plans until the oracle catches `plant` (test-the-cause on the test: a
/// green simulation with a blind oracle proves nothing).
pub(crate) fn control<P: fmt::Debug + Copy>(plant: P, run: impl Fn(&mut SplitMix64, P) -> Result<(), Broke>) {
    for seed in 0..CONTROL_SEEDS {
        match run(&mut SplitMix64::new(seed), plant) {
            Err(Broke::Oracle(why)) => {
                eprintln!("{plant:?} caught at seed {seed}: {why}");
                return;
            }
            Err(broke @ Broke::Harness(_)) => panic!("seed {seed}, {plant:?}: {broke}"),
            Ok(()) => {}
        }
    }
    panic!("the oracle caught {plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak");
}
