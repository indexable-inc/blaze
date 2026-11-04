//! The untyped, id-based access layer: host and kernel code only.
//!
//! Module code reads and writes components through the typed layer (`hyperion_ecs_sdk::typed`: `World::get::<T>`, `query!`), where the component id is
//! resolved from `T::NAME` and checked against `T`'s layout hash, and where a type can only be named if it comes from an `-api` crate the module
//! depends on or from the module itself (docs/module-architecture.md 4.6, C1 and C2). These traits are the old `World::get(e, comp_id)` /
//! `Iter::field(index)` surface: an id that came from a name `lookup`, a hand-numbered term index. They stay for the host (kernel systems
//! over host-owned components, the flecs script loader) and are NOT part of the module-facing SDK: `hyperion-lint` rejects
//! `hyperion_ecs::untyped` in `crates/modules/**` and in the libraries a module links (L1), and the SDK re-exports of `hyperion-module` leave
//! it out. The mobs.Gait fault of 2026-10-09 (a term inserted at index 5, a reader still on the old index) was this surface.
use crate::component::Error;
use crate::query::QueryBuilder;
#[cfg(feature = "system")]
use crate::system::SystemBuilder;
use crate::system::{Iter, ObserverBuilder};
use crate::sys;
use crate::world::{Id, World, pair};

/// Id-based component access, system and query builders over hand-built terms.
pub trait UntypedWorld: Sized + Copy {
    /// Start a system named `name` (relative to the current scope: inside a [`ModuleCtx`](crate::ModuleCtx) it is a child of `systems.<module>`).
    #[cfg(feature = "system")]
    fn system(self, name: &str) -> SystemBuilder;
    fn observer(self, name: &str) -> ObserverBuilder;
    /// `name`: a named query entity becomes a child of the current scope (so it dies with the module's `systems.<name>`).
    fn query(self, name: Option<&str>) -> QueryBuilder;
    /// Set component `comp` on `e` (copies `size_of::<T>()` bytes). `T` must be the registered type of `comp`.
    fn set_by_id<T: Copy>(self, e: Id, comp: Id, v: &T);
    /// [`set_by_id`](Self::set_by_id) for a forwarder: a generic fn or a trait-impl method (which cannot carry `#[image_sink]`) that is itself a sink of the same value, so the
    /// image check runs at ITS callers. Not for a value built in the calling fn: that goes through `set_by_id`.
    fn set_by_id_fwd<T: Copy>(self, e: Id, comp: Id, v: &T);
    fn get_by_id<T: Copy>(self, e: Id, comp: Id) -> Option<T>;
    /// Copy component `comp` of `e` into `out` (exactly the component's registered size, else false); false when `e` is dead or lacks it.
    /// For tables whose Rust type is not `Copy` (service tables: the bytes are read, never a value copied out through the type).
    fn get_bytes_by_id(self, e: Id, comp: Id, out: &mut [u8]) -> bool;
    /// Set component `comp` on `e` from raw bytes (exactly the component's registered size): a table with no `Copy` Rust type (service tables).
    /// Deliberately NOT an `#[image_sink]`: service tables are fn-pointer tables of the publishing image, retracted by the host after every `deinit`.
    fn set_bytes_by_id(self, e: Id, comp: Id, bytes: &[u8]);
    /// Borrowed mutable access; valid until the next structural change to `e`.
    ///
    /// # Safety
    /// `T` is the registered type of `comp`; the reference must not outlive the next world mutation.
    unsafe fn get_mut_by_id<'a, T>(self, e: Id, comp: Id) -> Option<&'a mut T>;
    /// Singleton: component `comp` stored on its own entity.
    fn set_singleton_by_id<T: Copy>(self, comp: Id, v: &T);
    /// Set a singleton from raw bytes (a table with no `Copy` Rust type, registered with `register_raw`).
    fn set_singleton_bytes(self, comp: Id, bytes: &[u8]);
    fn get_singleton_by_id<T: Copy>(self, comp: Id) -> Option<T>;
    fn set_pair_by_id<T: Copy>(self, e: Id, rel: Id, tgt: Id, v: &T);
    fn get_pair_by_id<T: Copy>(self, e: Id, rel: Id, tgt: Id) -> Option<T>;
}

impl UntypedWorld for World {
    #[cfg(feature = "system")]
    fn system(self, name: &str) -> SystemBuilder {
        SystemBuilder::new(self, name)
    }
    fn observer(self, name: &str) -> ObserverBuilder {
        ObserverBuilder::new(self, name)
    }
    fn query(self, name: Option<&str>) -> QueryBuilder {
        QueryBuilder::new(self, name)
    }
    fn set_by_id<T: Copy>(self, e: Id, comp: Id, v: &T) {
        set_raw(self, e, comp, v)
    }
    fn set_by_id_fwd<T: Copy>(self, e: Id, comp: Id, v: &T) {
        set_raw(self, e, comp, v)
    }
    fn get_by_id<T: Copy>(self, e: Id, comp: Id) -> Option<T> {
        // flecs aborts (ecs_assert) on id 0; a lookup that found nothing is 0, so a missing component reads as absent (e2e 2026-10-09: interact died on mc.InteractExt)
        // A dead entity (a player that disconnected while a module still holds its id) also aborts in flecs: read it as absent
        // (SIGABRT of zmcrs-root 2026-10-09: windows sys_tick -> player_info on a left player).
        if e == 0 || comp == 0 || !self.is_alive(e) {
            return None;
        }
        if !crate::check::size_ok_read(self, comp, size_of::<T>(), core::any::type_name::<T>()) {
            return None;
        }
        // SAFETY: the pointer is valid until the next structural change; we copy out immediately.
        unsafe {
            let p = sys::ecs_get_id(self.as_ptr(), e, comp) as *const T;
            (!p.is_null()).then(|| p.read_unaligned())
        }
    }
    fn get_bytes_by_id(self, e: Id, comp: Id, out: &mut [u8]) -> bool {
        if e == 0 || comp == 0 || !self.is_alive(e) {
            return false;
        }
        if !crate::check::size_ok_read(self, comp, out.len(), "service table bytes") {
            return false;
        }
        // SAFETY: the pointer is valid until the next structural change; the bytes are copied out at once, `out` has the registered size.
        unsafe {
            let p = sys::ecs_get_id(self.as_ptr(), e, comp) as *const u8;
            if p.is_null() {
                return false;
            }
            core::ptr::copy_nonoverlapping(p, out.as_mut_ptr(), out.len());
        }
        true
    }
    fn set_bytes_by_id(self, e: Id, comp: Id, bytes: &[u8]) {
        if !crate::check::size_ok(self, comp, bytes.len(), "service table bytes") {
            return;
        }
        // SAFETY: `bytes` is valid for its length; flecs copies it.
        unsafe { sys::ecs_set_id(self.as_ptr(), e, comp, bytes.len(), bytes.as_ptr().cast()) };
    }
    unsafe fn get_mut_by_id<'a, T>(self, e: Id, comp: Id) -> Option<&'a mut T> {
        if e == 0 || comp == 0 {
            return None;
        }
        if !crate::check::size_ok_read(self, comp, size_of::<T>(), core::any::type_name::<T>()) {
            return None;
        }
        unsafe { (sys::ecs_get_mut_id(self.as_ptr(), e, comp) as *mut T).as_mut() }
    }
    fn set_singleton_by_id<T: Copy>(self, comp: Id, v: &T) {
        set_raw(self, comp, comp, v)
    }
    fn set_singleton_bytes(self, comp: Id, bytes: &[u8]) {
        // SAFETY: `bytes` is valid for its length; flecs copies it (the caller passes exactly the component's size).
        unsafe { sys::ecs_set_id(self.as_ptr(), comp, comp, bytes.len(), bytes.as_ptr().cast()) };
    }
    fn get_singleton_by_id<T: Copy>(self, comp: Id) -> Option<T> {
        self.get_by_id(comp, comp)
    }
    fn set_pair_by_id<T: Copy>(self, e: Id, rel: Id, tgt: Id, v: &T) {
        set_raw(self, e, pair(rel, tgt), v)
    }
    fn get_pair_by_id<T: Copy>(self, e: Id, rel: Id, tgt: Id) -> Option<T> {
        self.get_by_id(e, pair(rel, tgt))
    }
}

/// Column access by hand-numbered term index.
pub trait UntypedIter {
    /// Column `index` as a slice: `count` rows for a self-sourced term, one element for a shared (singleton or entity-sourced) term.
    ///
    /// # Safety
    /// `T` must be the registered type of the term; no other reference to the same column may be alive.
    unsafe fn field<T>(&mut self, index: i8) -> &mut [T];
}

impl UntypedIter for Iter<'_> {
    unsafe fn field<T>(&mut self, index: i8) -> &mut [T] {
        unsafe {
            // A column of another size than `T` is a layout mismatch (a stale build): fault the module instead of letting flecs abort.
            if !crate::check::field_ok(self.raw, index, size_of::<T>(), core::any::type_name::<T>()) {
                return &mut [];
            }
            let p = sys::ecs_field_w_size(self.raw, size_of::<T>(), index) as *mut T;
            if p.is_null() {
                return &mut [];
            }
            let n = if sys::ecs_field_is_self(self.raw, index) { self.raw.count as usize } else { 1 };
            core::slice::from_raw_parts_mut(p, n)
        }
    }
}

#[cfg(feature = "meta")]
/// Registering a component by an explicit world name and a recipe (a provider side that has no typed `ApiComponent`), kept next to the
/// other untyped entry points so the typed layer stays the only module-facing one.
pub fn ensure_registered(w: World, name: &str, size: usize, align: usize, hash: u64, recipe: &'static hyperion_layout::layout::Recipe) -> Result<Id, Error> {
    if let Some(have) = crate::component::world_layout_hash(w, name) {
        return if have == hash { Ok(w.lookup(name)) } else { Err(Error::LayoutChanged { name: name.into(), world: Some(have), built_against: hash }) };
    }
    crate::component::register_reflected(w, name, size, align, hash, recipe)
}

/// The one body of the typed sets: a plain fn, so the generic value is forwarded here and the sink check applies at the callers of the `#[image_sink]` trait methods.
fn set_raw<T: Copy>(w: World, e: Id, comp: Id, v: &T) {
    if !crate::check::size_ok(w, comp, size_of::<T>(), core::any::type_name::<T>()) {
        return;
    }
    // SAFETY: v is valid for size_of::<T>() bytes; flecs copies it.
    unsafe { sys::ecs_set_id(w.as_ptr(), e, comp, size_of::<T>(), (v as *const T).cast()) };
}
