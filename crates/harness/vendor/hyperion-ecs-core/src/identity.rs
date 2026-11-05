//! Component identity: a component type is named in the world by `T::NAME` and checked by its layout hash `T::HASH`; no `TypeId`, no
//! per-image statics that name a type (AGENTS.md 28). The id lookup is cached per image in a small lock-free table.
//!
//! Who may name a type: only an [`ApiComponent`] (listed in an `-api` crate the module depends on) or a [`LocalComponent`] (declared by the
//! module crate with `local_component!`). [`Resolve`] is the one bound the typed layers (component, query, system) use for both.
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
pub use hyperion_api_layouts::{ApiComponent, LocalComponent};
use hyperion_layout::Layout;
use hyperion_ecs::component::{self, Error};
use hyperion_ecs::world::{Id, World};

/// Marker: the type is an [`ApiComponent`].
pub struct ApiK;
/// Marker: the type is a [`LocalComponent`].
pub struct LocalK;

/// What the typed layer needs of a component type, whichever of the two kinds it is. Implemented by blanket impls for [`ApiComponent`]
/// (`K = ApiK`) and [`LocalComponent`] (`K = LocalK`); the parameter only keeps the two impls apart, callers never name it (inference picks the
/// one impl that applies).
pub trait Resolve<K>: Layout + 'static {
    /// The world name.
    const NAME: &'static str;
    /// `Some(crate)` for a [`LocalComponent`]: the only crate that may use it in a query.
    const LOCAL_CRATE: Option<&'static str>;
}

impl<T: ApiComponent + 'static> Resolve<ApiK> for T {
    const NAME: &'static str = <T as ApiComponent>::NAME;
    const LOCAL_CRATE: Option<&'static str> = None;
}

impl<T: LocalComponent + 'static> Resolve<LocalK> for T {
    const NAME: &'static str = <T as LocalComponent>::NAME;
    const LOCAL_CRATE: Option<&'static str> = Some(<T as LocalComponent>::CRATE);
}

/// The const check `query!` expands per field: a `LocalComponent` declared by another crate fails to compile.
pub const fn assert_home(local: Option<&str>, krate: &str) {
    if let Some(c) = local {
        assert!(hyperion_api_layouts::name_eq(c, krate), "a LocalComponent of another crate cannot be used in this query: move the type into an -api crate");
    }
}

// ------------------------------------------------------------------------------------------------------------------ id resolution

struct Slot {
    seq: AtomicU32,
    key: AtomicU64,
    id: AtomicU64,
}
// A set of 4 slots for each key, picked by the key's top bits: a frame that reads a few hundred component types (den's native UI) evicted its own entries from a
// table of one slot per key, two hot types sharing a slot, and paid the by-name lookup plus the layout hash read on every access.
const SETS: usize = 256;
const WAYS: usize = 4;
static CACHE: [Slot; SETS * WAYS] = [const { Slot { seq: AtomicU32::new(0), key: AtomicU64::new(0), id: AtomicU64::new(0) } }; SETS * WAYS];
/// Which slot of a full set the next new key takes.
static NEXT: AtomicU32 = AtomicU32::new(0);

fn set_of(key: u64) -> &'static [Slot] {
    let at = ((key >> 32) * SETS as u64 >> 32) as usize * WAYS;
    &CACHE[at..at + WAYS]
}

/// Bumped when a world of this image goes, so a later world at the same address does not hit the dead world's ids.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Forget every cached id: call when a world is freed (den frees its scratch worlds; hyperion's world lives as long as the process).
pub fn forget_worlds() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

fn cache_key(w: World, name: &'static str) -> u64 {
    let epoch = EPOCH.load(Ordering::Relaxed).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    ((name.as_ptr() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (w.as_ptr() as u64).rotate_left(17) ^ epoch) | 1
}

fn cache_get(key: u64) -> Option<Id> {
    set_of(key).iter().find_map(|s| {
        let s1 = s.seq.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return None;
        }
        let (k, id) = (s.key.load(Ordering::Relaxed), s.id.load(Ordering::Relaxed));
        core::sync::atomic::fence(Ordering::Acquire);
        (s.seq.load(Ordering::Relaxed) == s1 && k == key && id != 0).then_some(id)
    })
}

/// Remember `id` for `key`: in the slot that has the key or none, else in the next of a full set's slots in turn.
fn cache_put(key: u64, id: Id) {
    let set = set_of(key);
    let s = set
        .iter()
        .find(|s| {
            let k = s.key.load(Ordering::Relaxed);
            k == 0 || k == key
        })
        .unwrap_or_else(|| &set[NEXT.fetch_add(1, Ordering::Relaxed) as usize % WAYS]);
    let s0 = s.seq.load(Ordering::Relaxed);
    if s0 & 1 == 1 || s.seq.compare_exchange(s0, s0 + 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return;
    }
    s.key.store(key, Ordering::Relaxed);
    s.id.store(id, Ordering::Relaxed);
    s.seq.store(s0 + 2, Ordering::Release);
}

/// The world id of component `T` (found by `T::NAME`), after checking that the world's recorded layout hash equals `T::HASH`. Cached per image
/// (a component id carries a generation, so a deleted component's cached id fails the liveness check).
pub fn component_id<T: Resolve<K>, K>(w: World) -> Result<Id, Error> {
    let key = cache_key(w, T::NAME);
    if let Some(id) = cache_get(key) {
        if w.is_alive(id) {
            return Ok(id);
        }
    }
    let id = w.lookup(T::NAME);
    if id == 0 {
        return Err(Error::NotAComponent(T::NAME.into()));
    }
    let have = component::world_layout_hash(w, T::NAME);
    if have != Some(<T as Layout>::HASH) {
        return Err(Error::LayoutChanged { name: T::NAME.into(), world: have, built_against: <T as Layout>::HASH });
    }
    cache_put(key, id);
    Ok(id)
}

/// `component_id` for the infallible accessors: an absent provider is "feature absent" (id 0, accessors return `None`), a layout mismatch is a
/// module fault (the image is quarantined, the host rolls it back).
pub fn id_or_fault<T: Resolve<K>, K>(w: World) -> Id {
    match component_id::<T, K>(w) {
        Ok(id) => id,
        Err(Error::NotAComponent(_)) => 0,
        Err(e) => {
            hyperion_ecs::hooks::fault(T::NAME, &format!("{e}"));
            0
        }
    }
}
