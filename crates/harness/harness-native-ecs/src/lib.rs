//! A typed, owning entity-component store over flecs (the vendored hyperion world layer).
//!
//! [`Store`] owns one flecs world. Components are plain Rust values (`Default + Clone + 'static`), registered anonymously
//! on first use with type hooks that construct, drop, clone and move them, so a component can own heap data. Zero-sized
//! types are tags. Reads borrow the store (`&self`) and anything that can move storage takes `&mut self`, so a reference
//! returned by [`Store::get`] or [`Store::children`] can never outlive the storage it points into.
//!
//! Hierarchy is flecs `ChildOf` with `OrderedChildren`: [`Store::spawn_under`] appends a child to its parent's ordered
//! list, [`Store::children`] reads that list in order and [`Store::despawn`] deletes an entity with all its descendants.
//! There are no systems: callers apply changes and read state directly.
//!
//! The world lives on the thread that made it (`Store` is neither `Send` nor `Sync`). flecs's OS API (allocation, clock,
//! log, abort) is routed to Rust once per process, before the first world: allocation goes to Rust's global allocator,
//! log lines to [`set_log_sink`], and an abort panics with the last error flecs logged.
//!
//! # Checked before flecs
//!
//! flecs checks its arguments in C and calls `os_abort` when one is wrong. A panic cannot unwind out of that `extern "C"`
//! hook, so a flecs check failing is a process abort that no `catch_unwind` (such as the one in [`Home::call`]) can catch.
//! So every argument flecs would refuse is checked here first, in Rust: each writer has a `try_` form that returns
//! [`StoreError`], and a plain form that panics with it (a panic that unwinds and can be caught). Removing something from
//! an entity that is gone (`remove`, `unrelate`, `despawn`) has nothing to do and does nothing, like a read of a stale
//! key. A flecs abort that still happens is a bug in these checks.
#![allow(unsafe_code)]

use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hyperion_ecs::sys;
pub use hyperion_ecs::{Id, pair};

mod dynamic;
pub use dynamic::{DynamicError, DynamicId, DynamicInfo, MAX_DYNAMIC_SIZE};

#[cfg(not(target_arch = "wasm32"))]
mod home;
#[cfg(not(target_arch = "wasm32"))]
pub use home::Home;

/// One entity. `repr(transparent)` over the flecs id, so a slice of flecs ids is a slice of entities. The default
/// (id 0) names no entity. Only a [`Store`] makes one, so an entity is always an id some store handed out.
#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Entity(Id);

impl Entity {
    pub fn raw(self) -> Id {
        self.0
    }
}

/// What a component must be: built empty by `Default` when flecs makes room for it, cloned when flecs copies it.
pub trait Component: Default + Clone + 'static {}
impl<T: Default + Clone + 'static> Component for T {}

/// The most ancestors an entity may have. flecs asserts `depth < FLECS_DAG_DEPTH_MAX + 1` when it records a parent
/// (`flecs_component_update_childof_w_depth`); measured on 4.1.6 (same limit and assert in 4.2.0): an entity with 128 ancestors is fine, a 129th aborts.
pub const MAX_DEPTH: usize = sys::FLECS_DAG_DEPTH_MAX as usize;

/// The largest alignment a component may have. flecs stores components in blocks it carves from `ecs_os_malloc`
/// (16-aligned, hyperion-flecs-c) at an offset of one `ecs_block_allocator_block_t` (two pointers) and steps of
/// multiples of 16 (`flecs_ballocator_init`), so a column starts 16-aligned on 64-bit targets and only 8-aligned on
/// wasm32. [`Store::component`] refuses a larger alignment at compile time:
///
/// ```compile_fail,E0080
/// #[derive(Default, Clone)]
/// #[repr(align(32))]
/// struct Wide(u8);
///
/// let mut s = harness_native_ecs::Store::new();
/// let e = s.spawn();
/// s.set(e, Wide(1));
/// ```
pub const MAX_ALIGN: usize = {
    let block_header = size_of::<sys::ecs_block_allocator_block_t>();
    let header_align = 1 << block_header.trailing_zeros();
    let malloc_align = hyperion_flecs_c::ALIGN;
    if header_align < malloc_align {
        header_align
    } else {
        malloc_align
    }
};

/// Why the store refused a write. Each is something flecs would abort the process on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Not a live entity of this store: deleted, `Entity::default()`, or from another store.
    NotAlive(Entity),
    /// `parent` is `child` itself or one of its descendants.
    Cycle { child: Entity, parent: Entity },
    /// The move or spawn would give some entity more than [`MAX_DEPTH`] ancestors.
    TooDeep { parent: Entity },
    /// `set_child_order` was given a list that is not exactly the parent's children.
    NotItsChildren(Entity),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotAlive(e) => write!(f, "{e:?} is not a live entity of this store"),
            StoreError::Cycle { child, parent } => {
                write!(f, "{parent:?} is {child:?} or below it: a parent cycle")
            }
            StoreError::TooDeep { parent } => write!(
                f,
                "a child under {parent:?} would have more than {MAX_DEPTH} ancestors"
            ),
            StoreError::NotItsChildren(p) => {
                write!(f, "the order given is not exactly the children of {p:?}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

#[track_caller]
fn must<R>(r: Result<R, StoreError>) -> R {
    r.unwrap_or_else(|e| panic!("{e}"))
}

/// A flecs-owned world with Rust components. See the crate docs.
pub struct Store {
    world: *mut sys::ecs_world_t,
    ids: HashMap<TypeId, Id>,
    dynamics: Vec<dynamic::DynamicComp>,
    _not_send: PhantomData<*mut ()>,
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

impl Store {
    pub fn new() -> Store {
        init_os_api();
        // SAFETY: the OS API is set; ecs_mini creates a world with the core only (no addon imports).
        let world = unsafe { sys::ecs_mini() };
        assert!(!world.is_null(), "flecs could not create a world");
        Store {
            world,
            ids: HashMap::new(),
            dynamics: Vec::new(),
            _not_send: PhantomData,
        }
    }

    /// The flecs id of `T`, registering it on first use.
    pub fn component<T: Component>(&mut self) -> Id {
        if let Some(&id) = self.ids.get(&TypeId::of::<T>()) {
            return id;
        }
        let id = register::<T>(self.world);
        self.ids.insert(TypeId::of::<T>(), id);
        id
    }

    fn known<T: 'static>(&self) -> Option<Id> {
        self.ids.get(&TypeId::of::<T>()).copied()
    }

    /// A new empty entity at the root.
    pub fn spawn(&mut self) -> Entity {
        // SAFETY: world live.
        Entity(unsafe { sys::ecs_new(self.world) })
    }

    /// A new entity appended to `parent`'s ordered children. Panics with the [`StoreError`] of [`Store::try_spawn_under`].
    #[track_caller]
    pub fn spawn_under(&mut self, parent: Entity) -> Entity {
        must(self.try_spawn_under(parent))
    }

    /// A new entity appended to `parent`'s ordered children; refused when `parent` is not alive or the child would be
    /// deeper than [`MAX_DEPTH`].
    pub fn try_spawn_under(&mut self, parent: Entity) -> Result<Entity, StoreError> {
        self.check(parent)?;
        if self.ancestors_of(parent) + 1 > MAX_DEPTH {
            return Err(StoreError::TooDeep { parent });
        }
        self.ensure_ordered(parent);
        // SAFETY: world live; `parent` is a live entity (checked) with OrderedChildren, so the child is appended in order.
        Ok(Entity(unsafe {
            sys::ecs_new_w_id(self.world, pair(sys::EcsChildOf, parent.0))
        }))
    }

    /// Move `e` (and its subtree) to the end of `parent`'s ordered children (a child already under `parent` stays where
    /// it is). Panics with the [`StoreError`] of [`Store::try_reparent`].
    #[track_caller]
    pub fn reparent(&mut self, e: Entity, parent: Entity) {
        must(self.try_reparent(e, parent))
    }

    /// As [`Store::reparent`]; refused when either is not alive, `parent` is `e` or below it, or the subtree would end
    /// up deeper than [`MAX_DEPTH`].
    pub fn try_reparent(&mut self, e: Entity, parent: Entity) -> Result<(), StoreError> {
        self.check(e)?;
        self.check(parent)?;
        let mut depth = 0;
        let mut at = parent;
        while at != Entity::default() {
            if at == e {
                return Err(StoreError::Cycle { child: e, parent });
            }
            depth += 1;
            at = self.parent(at).unwrap_or_default();
        }
        // `depth` counts `parent` and its ancestors: what `e` would have.
        if depth + self.height(e) > MAX_DEPTH {
            return Err(StoreError::TooDeep { parent });
        }
        self.ensure_ordered(parent);
        // SAFETY: world live, both live (checked), no cycle and within the depth flecs allows; ChildOf is exclusive, so
        // adding the pair replaces the old parent.
        unsafe { sys::ecs_add_id(self.world, e.0, pair(sys::EcsChildOf, parent.0)) };
        Ok(())
    }

    /// How many ancestors `e` has.
    fn ancestors_of(&self, e: Entity) -> usize {
        let mut n = 0;
        let mut at = e;
        while let Some(p) = self.parent(at) {
            n += 1;
            at = p;
        }
        n
    }

    /// How many levels lie below `e` (0 for a leaf).
    fn height(&self, e: Entity) -> usize {
        self.children(e)
            .iter()
            .map(|&c| 1 + self.height(c))
            .max()
            .unwrap_or(0)
    }

    fn ensure_ordered(&mut self, parent: Entity) {
        // SAFETY: world live, `parent` checked live by the caller; the tag is added before the first child, so flecs keeps
        // every child in its list.
        unsafe {
            if !sys::ecs_has_id(self.world, parent.0, sys::EcsOrderedChildren) {
                sys::ecs_add_id(self.world, parent.0, sys::EcsOrderedChildren);
            }
        }
    }

    /// `e` is a live entity this store made: alive, not one of flecs's own entities and not a component. Every write
    /// checks it first (see the crate docs, "Checked before flecs").
    fn is_writable(&self, e: Entity) -> bool {
        (e.0 & 0xFFFF_FFFF) >= u64::from(sys::EcsFirstUserEntityId)
            && self.is_alive(e)
            // SAFETY: world live, `e` alive; the EcsComponent id is an immutable flecs global.
            && !unsafe { sys::ecs_has_id(self.world, e.0, sys::FLECS_IDEcsComponentID_) }
    }

    fn check(&self, e: Entity) -> Result<(), StoreError> {
        if self.is_writable(e) {
            Ok(())
        } else {
            Err(StoreError::NotAlive(e))
        }
    }

    /// Delete `e` and every descendant, dropping their components. Does nothing when `e` is not alive.
    pub fn despawn(&mut self, e: Entity) {
        if !self.is_writable(e) {
            return;
        }
        // SAFETY: world live, `e` a live entity of ours.
        unsafe { sys::ecs_delete(self.world, e.0) }
    }

    /// Delete every child of `parent` (and their subtrees), keeping `parent`.
    pub fn despawn_children(&mut self, parent: Entity) {
        let kids = self.children(parent).to_vec();
        for k in kids {
            self.despawn(k);
        }
    }

    pub fn is_alive(&self, e: Entity) -> bool {
        // SAFETY: world live; flecs answers for any nonzero id (it compares the whole id, generation included).
        e.0 != 0 && unsafe { sys::ecs_is_alive(self.world, e.0) }
    }

    /// Set `T` on `e`, replacing any old value. Panics with the [`StoreError`] of [`Store::try_set`].
    #[track_caller]
    pub fn set<T: Component>(&mut self, e: Entity, value: T) {
        must(self.try_set(e, value))
    }

    /// Set `T` on `e`, replacing any old value; refused when `e` is not alive.
    pub fn try_set<T: Component>(&mut self, e: Entity, value: T) -> Result<(), StoreError> {
        let id = self.component::<T>();
        self.check(e)?;
        if size_of::<T>() == 0 {
            // SAFETY: world live, `e` checked; a tag has no storage.
            unsafe { sys::ecs_add_id(self.world, e.0, id) };
            return Ok(());
        }
        // SAFETY: `e` checked live; ecs_ensure_id returns the entity's constructed `T` (default-built by the ctor hook if
        // new), valid until the next structural change; we assign through it at once.
        unsafe {
            let p = sys::ecs_ensure_id(self.world, e.0, id, size_of::<T>()).cast::<T>();
            assert!(!p.is_null(), "flecs refused a component on {e:?}");
            *p = value;
            sys::ecs_modified_id(self.world, e.0, id);
        }
        Ok(())
    }

    /// Add the tag (or default value) `T` to `e` when it does not have it. Panics with the [`StoreError`] of
    /// [`Store::try_add`].
    #[track_caller]
    pub fn add<T: Component>(&mut self, e: Entity) {
        must(self.try_add::<T>(e))
    }

    /// Add the tag (or default value) `T` to `e` when it does not have it; refused when `e` is not alive.
    pub fn try_add<T: Component>(&mut self, e: Entity) -> Result<(), StoreError> {
        self.check(e)?;
        if !self.has::<T>(e) {
            self.try_set(e, T::default())?;
        }
        Ok(())
    }

    /// `T` on `e`; `None` when `e` lacks it or is not alive (a stale key reads as nothing).
    pub fn get<T: Component>(&self, e: Entity) -> Option<&T> {
        let id = self.known::<T>()?;
        if size_of::<T>() == 0 || !self.is_alive(e) {
            return None;
        }
        // SAFETY: world live; the pointer is to an initialised `T` that stays put while `self` is borrowed (structural changes
        // need `&mut self`).
        unsafe { sys::ecs_get_id(self.world, e.0, id).cast::<T>().as_ref() }
    }

    pub fn get_mut<T: Component>(&mut self, e: Entity) -> Option<&mut T> {
        let id = self.known::<T>()?;
        if size_of::<T>() == 0 || !self.is_alive(e) {
            return None;
        }
        // SAFETY: as `get`, and `&mut self` makes the reference unique.
        unsafe {
            sys::ecs_get_mut_id(self.world, e.0, id)
                .cast::<T>()
                .as_mut()
        }
    }

    /// `T` on `e`, inserting `T::default()` first when missing. Panics with the [`StoreError`] of [`Store::try_ensure`].
    #[track_caller]
    pub fn ensure<T: Component>(&mut self, e: Entity) -> &mut T {
        must(self.try_ensure::<T>(e))
    }

    /// `T` on `e`, inserting `T::default()` first when missing; refused when `e` is not alive.
    pub fn try_ensure<T: Component>(&mut self, e: Entity) -> Result<&mut T, StoreError> {
        let id = self.component::<T>();
        assert!(size_of::<T>() != 0, "ensure on a tag");
        self.check(e)?;
        // SAFETY: as `try_set`; the borrow of `self` keeps the storage in place.
        unsafe {
            let p = sys::ecs_ensure_id(self.world, e.0, id, size_of::<T>()).cast::<T>();
            assert!(!p.is_null(), "flecs refused a component on {e:?}");
            Ok(&mut *p)
        }
    }

    pub fn has<T: Component>(&self, e: Entity) -> bool {
        // SAFETY: world live.
        self.is_alive(e)
            && self
                .known::<T>()
                .is_some_and(|id| unsafe { sys::ecs_has_id(self.world, e.0, id) })
    }

    /// Remove `T` from `e`; nothing to do when `e` is not alive.
    pub fn remove<T: Component>(&mut self, e: Entity) {
        if let Some(id) = self.known::<T>()
            && self.is_writable(e)
        {
            // SAFETY: world live, `e` a live entity of ours; the dtor hook drops the value.
            unsafe { sys::ecs_remove_id(self.world, e.0, id) }
        }
    }

    /// `e`'s parent, if it has one.
    pub fn parent(&self, e: Entity) -> Option<Entity> {
        if !self.is_alive(e) {
            return None;
        }
        // SAFETY: world live.
        let p = unsafe { sys::ecs_get_parent(self.world, e.0) };
        (p != 0).then_some(Entity(p))
    }

    /// `parent`'s children in order (empty when it has none).
    pub fn children(&self, parent: Entity) -> &[Entity] {
        if !self.is_alive(parent) {
            return &[];
        }
        // SAFETY: world live. For a parent with OrderedChildren flecs returns its child vector, which only changes on a
        // structural change (`&mut self`). Entity is repr(transparent) over the id.
        unsafe {
            if !sys::ecs_has_id(self.world, parent.0, sys::EcsOrderedChildren) {
                return &[];
            }
            let list = sys::ecs_get_ordered_children(self.world, parent.0);
            if list.ids.is_null() || list.count <= 0 {
                return &[];
            }
            std::slice::from_raw_parts(list.ids.cast::<Entity>(), list.count as usize)
        }
    }

    /// Reorder `parent`'s children. Panics with the [`StoreError`] of [`Store::try_set_child_order`].
    #[track_caller]
    pub fn set_child_order(&mut self, parent: Entity, order: &[Entity]) {
        must(self.try_set_child_order(parent, order))
    }

    /// Reorder `parent`'s children; refused unless `parent` is alive and `order` holds exactly its current children.
    pub fn try_set_child_order(
        &mut self,
        parent: Entity,
        order: &[Entity],
    ) -> Result<(), StoreError> {
        self.check(parent)?;
        let mut now = self.children(parent).to_vec();
        let mut given = order.to_vec();
        now.sort_unstable();
        given.sort_unstable();
        if now != given {
            return Err(StoreError::NotItsChildren(parent));
        }
        if order.is_empty() {
            // flecs refuses an empty list (and a parent with no ordered children); there is nothing to reorder.
            return Ok(());
        }
        // SAFETY: world live; `order` is exactly the parent's ordered children (checked above), as flecs requires.
        unsafe {
            sys::ecs_set_child_order(
                self.world,
                parent.0,
                order.as_ptr().cast(),
                order.len() as i32,
            )
        };
        Ok(())
    }

    /// Add the relationship `(R, target)` to `e`. `R` is a tag type; declare it exclusive with [`Store::exclusive`] when an
    /// entity has at most one target. Panics with the [`StoreError`] of [`Store::try_relate`].
    #[track_caller]
    pub fn relate<R: Component>(&mut self, e: Entity, target: Entity) {
        must(self.try_relate::<R>(e, target))
    }

    /// Add the relationship `(R, target)` to `e`; refused when either is not alive.
    pub fn try_relate<R: Component>(
        &mut self,
        e: Entity,
        target: Entity,
    ) -> Result<(), StoreError> {
        let r = self.component::<R>();
        self.check(e)?;
        // A pair holds only the low 32 bits of its target, so a stale target would name whatever reuses its index.
        self.check(target)?;
        // SAFETY: world live, both live entities of ours.
        unsafe { sys::ecs_add_id(self.world, e.0, pair(r, target.0)) };
        Ok(())
    }

    /// The target of `(R, *)` on `e`.
    pub fn target<R: Component>(&self, e: Entity) -> Option<Entity> {
        let r = self.known::<R>()?;
        if !self.is_alive(e) {
            return None;
        }
        // SAFETY: world live.
        let t = unsafe { sys::ecs_get_target(self.world, e.0, r, 0) };
        (t != 0).then_some(Entity(t))
    }

    /// Remove `(R, target)` from `e`; nothing to do when either is not alive (flecs drops pairs to a deleted target, and a
    /// stale target's index may name another entity now).
    pub fn unrelate<R: Component>(&mut self, e: Entity, target: Entity) {
        if let Some(r) = self.known::<R>()
            && self.is_writable(e)
            && self.is_writable(target)
        {
            // SAFETY: world live, both live entities of ours.
            unsafe { sys::ecs_remove_id(self.world, e.0, pair(r, target.0)) }
        }
    }

    /// Mark relationship `R` exclusive (one target per entity). Call before any entity uses it: flecs cannot change a
    /// trait of a relationship in use, so a late call panics.
    #[track_caller]
    pub fn exclusive<R: Component>(&mut self) {
        let r = self.component::<R>();
        // SAFETY: the handle is used only for this call, while the world is live.
        let w = unsafe { hyperion_ecs::World::from_raw(self.world.cast()) };
        assert!(
            w.traits(r, &[hyperion_ecs::Trait::Exclusive]),
            "{} is already in use; declare it exclusive first",
            std::any::type_name::<R>()
        );
    }

    /// Every entity that has `T`, in storage order.
    pub fn each<T: Component>(&self) -> Vec<Entity> {
        let Some(id) = self.known::<T>() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // SAFETY: the iterator is driven to completion; `entities` is valid for `count` ids until the next call.
        unsafe {
            let mut it = sys::ecs_each_id(self.world, id);
            while sys::ecs_each_next(&mut it) {
                out.extend_from_slice(std::slice::from_raw_parts(
                    it.entities.cast::<Entity>(),
                    it.count as usize,
                ));
            }
        }
        out
    }

    /// Number of entities alive in the world (flecs's own entities included).
    pub fn alive(&self) -> usize {
        // SAFETY: world live; reads the entity index header.
        unsafe { sys::ecs_get_entities(self.world).alive_count as usize }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: the world is ours and no reference into it outlives `self`.
        unsafe { sys::ecs_fini(self.world) };
    }
}

fn register<T: Component>(world: *mut sys::ecs_world_t) -> Id {
    // Checked when `T` is instantiated, so a component flecs cannot store aligned fails the build for that target.
    const {
        assert!(
            size_of::<T>() == 0 || align_of::<T>() <= MAX_ALIGN,
            "a component's alignment is above what flecs storage guarantees (MAX_ALIGN)"
        );
        assert!(
            size_of::<T>() <= i32::MAX as usize,
            "a component is larger than flecs sizes allow"
        );
    }
    // SAFETY: a zeroed desc is flecs's "unset"; the anonymous entity is fresh; hook functions match the flecs signatures
    // and only touch `count` values of `T` at the given pointers.
    unsafe {
        let e = sys::ecs_new(world);
        let mut d: sys::ecs_component_desc_t = std::mem::zeroed();
        d.entity = e;
        if size_of::<T>() != 0 {
            d.type_.size = size_of::<T>() as i32;
            d.type_.alignment = align_of::<T>() as i32;
            d.type_.hooks.ctor = Some(ctor::<T>);
            d.type_.hooks.dtor = Some(dtor::<T>);
            d.type_.hooks.copy = Some(copy::<T>);
            d.type_.hooks.move_ = Some(move_::<T>);
        }
        let id = sys::ecs_component_init(world, &d);
        assert!(
            id != 0,
            "flecs refused component {}",
            std::any::type_name::<T>()
        );
        id
    }
}

unsafe extern "C" fn ctor<T: Component>(
    ptr: *mut c_void,
    count: i32,
    _ti: *const sys::ecs_type_info_t,
) {
    let p = ptr.cast::<T>();
    for i in 0..count as usize {
        // SAFETY: flecs hands `count` uninitialised, aligned slots.
        unsafe { p.add(i).write(T::default()) };
    }
}

unsafe extern "C" fn dtor<T: Component>(
    ptr: *mut c_void,
    count: i32,
    _ti: *const sys::ecs_type_info_t,
) {
    // SAFETY: flecs hands `count` initialised values it will not use again.
    unsafe {
        std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
            ptr.cast::<T>(),
            count as usize,
        ))
    };
}

unsafe extern "C" fn copy<T: Component>(
    dst: *mut c_void,
    src: *const c_void,
    count: i32,
    _ti: *const sys::ecs_type_info_t,
) {
    let (d, s) = (dst.cast::<T>(), src.cast::<T>());
    for i in 0..count as usize {
        // SAFETY: both ranges hold initialised values; flecs never overlaps them.
        unsafe { (*d.add(i)).clone_from(&*s.add(i)) };
    }
}

unsafe extern "C" fn move_<T: Component>(
    dst: *mut c_void,
    src: *mut c_void,
    count: i32,
    _ti: *const sys::ecs_type_info_t,
) {
    let (d, s) = (dst.cast::<T>(), src.cast::<T>());
    for i in 0..count as usize {
        // SAFETY: both ranges hold initialised values (move leaves `src` valid but empty, as flecs expects).
        unsafe { *d.add(i) = std::mem::take(&mut *s.add(i)) };
    }
}

// ---- OS API, routed to Rust ----

static OS_API: Once = Once::new();
static LOG_SINK: AtomicUsize = AtomicUsize::new(0);
static CLOCK_NS: AtomicU64 = AtomicU64::new(0);

std::thread_local! {
    static LAST_ERROR: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Where flecs log lines go: `(level, text)` with flecs levels (-4 fatal, -3 error, -2 warning, 0 info, >0 debug). The
/// default drops them (a terminal view must not write to stderr).
pub fn set_log_sink(sink: fn(i32, &str)) {
    LOG_SINK.store(sink as usize, Ordering::Relaxed);
}

/// The last error flecs logged on this thread (what an abort would have panicked with), for [`harness_native_flecs::checked`]'s message.
#[must_use]
pub fn last_error() -> String {
    LAST_ERROR.with(|e| e.borrow().clone())
}

/// Set flecs's clock (nanoseconds) on targets without one (wasm32). flecs only reads it for its own bookkeeping.
pub fn set_clock_ns(ns: u64) {
    CLOCK_NS.store(ns, Ordering::Relaxed);
}

/// Route flecs's OS API to Rust (allocator, clock, log, abort), once per process. [`Store::new`] calls it; a host that makes
/// worlds of its own (den-native's, with the addons) calls it before its first one, so every world in the process shares it.
pub fn init_os_api() {
    OS_API.call_once(|| {
        check_layout();
        // SAFETY: done once, before any world exists; we copy the defaults and replace the hooks flecs calls.
        unsafe {
            sys::ecs_os_set_api_defaults();
            let mut api = sys::ecs_os_get_api();
            api.malloc_ = Some(os_malloc);
            api.calloc_ = Some(os_calloc);
            api.realloc_ = Some(os_realloc);
            api.free_ = Some(os_free);
            api.now_ = Some(os_now);
            api.get_time_ = Some(os_get_time);
            api.log_ = Some(os_log);
            api.abort_ = Some(os_abort);
            api.flags_ &= !sys::EcsOsApiLogWithColors;
            #[cfg(not(target_arch = "wasm32"))]
            {
                files::DISK_FOPEN.store(api.fopen_.map_or(0, |f| f as usize), Ordering::Relaxed);
                api.fopen_ = Some(files::os_fopen);
            }
            sys::ecs_os_set_api(&mut api);
        }
    });
}

unsafe extern "C" fn os_malloc(n: sys::ecs_size_t) -> *mut c_void {
    // SAFETY: plain allocation.
    unsafe { hyperion_flecs_c::den_flecs_malloc(n.max(0) as usize) }
}
unsafe extern "C" fn os_calloc(n: sys::ecs_size_t) -> *mut c_void {
    // SAFETY: plain allocation.
    unsafe { hyperion_flecs_c::den_flecs_calloc(1, n.max(0) as usize) }
}
unsafe extern "C" fn os_realloc(p: *mut c_void, n: sys::ecs_size_t) -> *mut c_void {
    // SAFETY: flecs only reallocs blocks it got from os_malloc/os_calloc.
    unsafe { hyperion_flecs_c::den_flecs_realloc(p, n.max(0) as usize) }
}
unsafe extern "C" fn os_free(p: *mut c_void) {
    // SAFETY: flecs only frees blocks it got from these hooks.
    unsafe { hyperion_flecs_c::den_flecs_free(p) }
}

fn clock_ns() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_nanos() as u64
    }
    #[cfg(target_arch = "wasm32")]
    {
        CLOCK_NS.load(Ordering::Relaxed)
    }
}

unsafe extern "C" fn os_now() -> u64 {
    clock_ns()
}

unsafe extern "C" fn os_get_time(t: *mut sys::ecs_time_t) {
    let ns = clock_ns();
    // SAFETY: flecs passes a valid out pointer.
    unsafe {
        (*t).sec = (ns / 1_000_000_000) as u32;
        (*t).nanosec = (ns % 1_000_000_000) as u32;
    }
}

unsafe extern "C" fn os_log(level: i32, file: *const c_char, line: i32, msg: *const c_char) {
    let text = |p: *const c_char| {
        if p.is_null() {
            String::new()
        } else {
            // SAFETY: flecs passes NUL terminated strings valid for the call.
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    };
    let mut line_text = text(msg);
    if !file.is_null() {
        line_text = format!("{}:{line}: {line_text}", text(file));
    }
    if level < -2 {
        LAST_ERROR.with(|e| *e.borrow_mut() = line_text.clone());
    }
    let sink = LOG_SINK.load(Ordering::Relaxed);
    if sink != 0 {
        // SAFETY: only `set_log_sink` stores here, always a `fn(i32, &str)`.
        let f: fn(i32, &str) = unsafe { std::mem::transmute::<usize, fn(i32, &str)>(sink) };
        f(level, &line_text);
    }
}

unsafe extern "C" fn os_abort() {
    let last = LAST_ERROR.with(|e| e.borrow().clone());
    // A panic cannot unwind out of an extern "C" fn: this prints the message and aborts.
    panic!("flecs aborted: {last}");
}

/// The C and Rust views of the flecs structs Rust touches must agree on every target (the bindings were generated on
/// x86_64 Linux and are reused on wasm32).
fn check_layout() {
    let rust = [
        size_of::<sys::ecs_os_api_t>(),
        size_of::<sys::ecs_type_hooks_t>(),
        size_of::<sys::ecs_type_info_t>(),
        size_of::<sys::ecs_component_desc_t>(),
        size_of::<sys::ecs_entity_desc_t>(),
        size_of::<sys::ecs_entities_t>(),
        size_of::<sys::ecs_iter_t>(),
        size_of::<sys::ecs_time_t>(),
    ];
    for (i, r) in rust.iter().enumerate() {
        // SAFETY: a pure function of its argument.
        let c = unsafe { hyperion_flecs_c::den_flecs_layout(i as i32) };
        assert_eq!(
            c, *r,
            "flecs struct {i}: C says {c} bytes, the Rust bindings {r}"
        );
    }
}

#[cfg(test)]
mod tests;

/// Files compiled into the binary that flecs reads as if they were on disk: a managed script loaded by `filename` and the
/// scripts it `include`s (flecs resolves an include next to the including script's file name and opens it with the OS
/// API's `fopen`). A path that is not embedded goes to the disk as before.
#[cfg(not(target_arch = "wasm32"))]
mod files {
    use super::{Ordering, c_char, c_void, sys};
    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::sync::RwLock;
    use std::sync::atomic::AtomicUsize;

    static EMBEDDED: RwLock<Option<HashMap<&'static str, &'static str>>> = RwLock::new(None);
    /// The `fopen` flecs had before ours (0: the C library's).
    pub(super) static DISK_FOPEN: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" {
        fn fmemopen(buf: *mut c_void, size: usize, mode: *const c_char) -> *mut sys::FILE;
        fn fopen(path: *const c_char, mode: *const c_char) -> *mut sys::FILE;
    }

    pub(super) fn embed(list: &[(&'static str, &'static str)]) {
        let mut g = EMBEDDED.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.get_or_insert_with(HashMap::new).extend(list.iter().copied());
    }

    pub(super) unsafe extern "C" fn os_fopen(path: *const c_char, mode: *const c_char) -> *mut sys::FILE {
        // SAFETY: flecs passes a NUL-terminated path.
        let p = unsafe { CStr::from_ptr(path) }.to_str().unwrap_or("");
        let hit = EMBEDDED.read().ok().and_then(|g| g.as_ref().and_then(|m| m.get(p).copied()));
        if let Some(src) = hit {
            // Read-only: fmemopen with "r" never writes the buffer, and the text is 'static (an empty one opens at size 0).
            let ptr: *const c_char = if src.is_empty() { c" ".as_ptr() } else { src.as_ptr().cast() };
            // SAFETY: a static buffer of `src.len()` bytes, opened for reading only.
            return unsafe { fmemopen(ptr.cast_mut().cast(), src.len(), c"r".as_ptr()) };
        }
        match DISK_FOPEN.load(Ordering::Relaxed) {
            // SAFETY: the C library's fopen with flecs's arguments.
            0 => unsafe { fopen(path, mode) },
            // SAFETY: the hook flecs had installed, of this very type, stored as an address by `init_os_api`.
            f => unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(*const c_char, *const c_char) -> *mut sys::FILE>(f)(path, mode) },
        }
    }
}

/// Serve `list` (`(path, text)`: `scene/widgets/rows.flecs` and its source) to flecs as files: a script made with that
/// `filename`, or `include`d by one, reads the compiled-in text. Later lists add to earlier ones.
#[cfg(not(target_arch = "wasm32"))]
pub fn embed_files(list: &[(&'static str, &'static str)]) {
    files::embed(list);
}
