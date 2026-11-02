//! The persistent maps and sets the resolver copies at each backtracking point (cargo's are
//! im-rc's): imbl's, counted with `Rc` rather than its default `Arc`, since a resolve runs on one
//! thread and is cloned at every decision.

use imbl::shared_ptr::RcK;

pub(crate) type HashMap<K, V, S> = imbl::GenericHashMap<K, V, S, RcK>;
pub(crate) type HashSet<A, S> = imbl::GenericHashSet<A, S, RcK>;
pub(crate) type OrdMap<K, V> = imbl::GenericOrdMap<K, V, RcK>;
pub(crate) type OrdSet<A> = imbl::GenericOrdSet<A, RcK>;

pub(crate) use imbl::hashmap::Entry;
