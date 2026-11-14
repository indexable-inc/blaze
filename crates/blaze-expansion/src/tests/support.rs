//! What the tests share: an in-memory store of the core's [`Store`], the cases a control runs,
//! and how a filing's refusal is judged.

use std::{
    alloc::{Allocator, Global},
    cell::{Cell, RefCell},
};

use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Key, Kind, Namespace, ToolId},
    outside::OutsideError,
    store::{Compacted, Generation, InProcess, Put as Filing, Read, RootRef, Store, StoreError},
};
use blaze_sim::{Broke, Config};

use crate::machine::Refusal;

/// The cases a control runs (seeds 0..128, as the hand-rolled control looped) before it says its
/// plant went uncaught.
pub(crate) const CONTROL: Config = Config::cases(128);

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
