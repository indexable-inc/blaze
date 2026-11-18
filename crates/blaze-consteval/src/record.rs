//! A constant's value as a record (consteval.md, "The record"): bytes plus relocations, in a
//! form with no address and no machine in it (rule 25), keyed from text before the work.
//!
//! The data form is blaze-build's (`blaze_build::data`, the one home of it, agreed with the
//! build-support lane): the builder places a record as data without converting it.

use core::alloc::Allocator;

use blaze_build::{Instance, InstanceKey};
pub use blaze_build::data::{DataAlloc, DataRecord, DataReloc, DataTarget};
use blaze_ids::{ContentId, Hasher};
use blaze_own::Vec;

/// The key of a value's record: the item's code identity and its ground generic arguments,
/// known from text before the evaluation runs (rule 65). Equal keys evaluate to equal bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey(pub ContentId);

const DOMAIN: &str = "blaze-consteval value 1";

impl RecordKey {
    /// The key of the value of the instance `instance` (blaze-build's key of the item's code
    /// and its arguments, written as trees).
    pub fn of(instance: InstanceKey) -> RecordKey {
        let mut h = Hasher::new(DOMAIN);
        h.id(&instance.0);
        RecordKey(h.finish())
    }
}

/// A fn instance a value's words name: the key a `DataTarget::Fn` word holds (a hash with no
/// inverse) and the instance the evaluator made it from: the fn item and its ground arguments,
/// or, for a closure coerced to a fn pointer, its `FnOnce::call_once` at `[closure, (inputs..)]`
/// (the instance blaze-build's `closure_fn_addr` takes for the same coercion). The arguments
/// are a list of the evaluation's ground table, the one the key was made over: a reader copies
/// them into its own table (blaze-ladder front2/consts.rs `Consts::fn_instance`), so a builder
/// that meets the word knows which instance to call without searching the program (the fn
/// is named in the constant's body, never in the function being built).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConstFn {
    pub key: InstanceKey,
    pub instance: Instance,
}

/// One constant's record: its value, what the evaluation read, by content (each other value's
/// key and each body's text hash), sorted, each once (rule 25), and the fn instances its words
/// name ([`ConstFn`], sorted by key, each once: exactly the keys of the value's
/// `DataTarget::Fn` words, a value read from another record's included). The record stands
/// while every read is unchanged (AGENTS.md: "valid by its key, or else by its read list").
pub struct ConstRecord<A: Allocator> {
    pub key: RecordKey,
    pub data: DataRecord<A>,
    pub reads: Vec<ContentId, A>,
    pub fns: Vec<ConstFn, A>,
}

impl<A: Allocator> ConstRecord<A> {
    /// The instance fn word `key` of this record names.
    pub fn fn_named(&self, key: InstanceKey) -> Option<ConstFn> {
        self.fns.binary_search_by_key(&key, |f| f.key).ok().map(|at| self.fns[at])
    }
}

impl<A: Allocator> core::fmt::Debug for ConstRecord<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ConstRecord {{ key: {:?}, allocs: [", self.key)?;
        for a in self.data.allocs.iter() {
            write!(f, "{{ bytes: {:?}, align: {}, mutable: {}, relocs: {:?} }}", a.bytes.as_slice(), a.align, a.mutable, a.relocs.as_slice())?;
        }
        write!(f, "], reads: {} }}", self.reads.len())
    }
}
