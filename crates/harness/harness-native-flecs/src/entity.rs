//! Entities: [`Entity`] (an id) and [`EntityView`] (an id in a world, with the operations on it).
//!
//! Every operation checks what flecs would abort on first: reads of a dead entity answer nothing, a component the world does not
//! have (id 0) is absent, and a write to a dead entity panics in Rust with the entity named rather than aborting inside flecs.
use crate::{Access, ComponentId, Get, IntoId, WorldRef, host};
use core::marker::PhantomData;
use hyperion_ecs::{Id, pair, sys};

/// An entity id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Entity(pub u64);

impl core::ops::Deref for Entity {
    type Target = u64;
    fn deref(&self) -> &u64 {
        &self.0
    }
}

impl From<u64> for Entity {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<Entity> for u64 {
    fn from(e: Entity) -> Self {
        e.0
    }
}

impl PartialEq<u64> for Entity {
    fn eq(&self, other: &u64) -> bool {
        self.0 == *other
    }
}

impl IntoId for Entity {
    fn into_id(self, _: hyperion_ecs::World) -> Id {
        self.0
    }
}

/// An entity of a world. `Copy`; valid while the world it came from is.
#[derive(Clone, Copy)]
pub struct EntityView<'a> {
    pub(crate) w: hyperion_ecs::World,
    pub(crate) id: Id,
    pub(crate) _world: PhantomData<&'a ()>,
}

impl core::fmt::Debug for EntityView<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "EntityView({})", self.id)
    }
}

impl IntoId for EntityView<'_> {
    fn into_id(self, _: hyperion_ecs::World) -> Id {
        self.id
    }
}

impl<'a> EntityView<'a> {
    pub(crate) fn new(w: hyperion_ecs::World, id: Id) -> Self {
        Self { w, id, _world: PhantomData }
    }

    #[must_use]
    pub fn id(self) -> Entity {
        Entity(self.id)
    }

    #[must_use]
    pub fn world(self) -> WorldRef<'a> {
        WorldRef::new(self.w)
    }

    /// Alive in its world (0 never is).
    #[must_use]
    pub fn is_alive(self) -> bool {
        self.id != 0 && self.w.is_alive(self.id)
    }

    fn writable(self, what: &str) {
        assert!(self.is_alive(), "{what} on entity {} that is not alive", self.id);
    }

    /// Set component `v` (added if missing), moved into the world.
    pub fn set<T: ComponentId>(self, v: T) -> Self {
        self.writable("set");
        let c = host::resolve::<T>(self.w);
        if c == 0 {
            return self;
        }
        // SAFETY: live world and entity; `c` is `T`'s id there (name and layout hash checked), so the storage holds a `T`: an
        // existing value is assigned, a new slot (`is_new`) is uninitialised and written; flecs then runs OnSet.
        unsafe {
            let mut is_new = false;
            let p = sys::ecs_emplace_id(self.w.as_ptr(), self.id, c, size_of::<T>(), &raw mut is_new).cast::<T>();
            if !p.is_null() {
                if is_new { p.write(v) } else { *p = v }
                sys::ecs_modified_id(self.w.as_ptr(), self.id, c);
            }
        }
        self
    }

    /// Add an id: a tag, a component (default value), or a pair.
    #[allow(clippy::should_implement_trait, reason = "flecs-rust's name")]
    pub fn add(self, id: impl IntoId) -> Self {
        self.writable("add");
        let id = id.into_id(self.w);
        if id != 0 {
            self.w.add_id(self.id, id);
        }
        self
    }

    /// [`add`](Self::add) that says when flecs refused it (a pair that breaks a `OneOf`, `Final` or `Target` constraint, an id
    /// that is not valid): nothing is added then.
    pub fn try_add(self, id: impl IntoId) -> Result<Self, crate::FlecsError> {
        crate::checked(|| self.add(id))
    }

    pub fn remove(self, id: impl IntoId) -> Self {
        self.writable("remove");
        let id = id.into_id(self.w);
        if id != 0 {
            self.w.remove_id(self.id, id);
        }
        self
    }

    #[must_use]
    pub fn has(self, id: impl IntoId) -> bool {
        let id = id.into_id(self.w);
        id != 0 && self.is_alive() && self.w.has_id(self.id, id)
    }

    /// Whether the entity has `id` itself, not through a prefab it is an instance of: `(IsA, base)` holds only for the
    /// prefab it was made from, where [`has`](Self::has) also holds for that prefab's own bases.
    #[must_use]
    pub fn owns(self, id: impl IntoId) -> bool {
        let id = id.into_id(self.w);
        // SAFETY: live world and entity; `id` is a live id.
        id != 0 && self.is_alive() && unsafe { sys::ecs_owns_id(self.w.as_ptr(), self.id, id) }
    }

    fn ptr<Q: Access>(self) -> Option<*mut Q::C> {
        let c = Q::id(self.w);
        if c == 0 || !self.is_alive() {
            return None;
        }
        // SAFETY: live world and entity; `c` is the component's id there; null when the entity does not have it.
        let p = unsafe {
            if Q::MUT { sys::ecs_get_mut_id(self.w.as_ptr(), self.id, c) } else { sys::ecs_get_id(self.w.as_ptr(), self.id, c).cast_mut() }
        };
        (!p.is_null()).then_some(p.cast())
    }

    /// A copy of component `Q` (`&T`), if the entity has it.
    #[must_use]
    pub fn try_cloned<Q: Access>(self) -> Option<Q::C>
    where
        Q::C: Clone,
    {
        // SAFETY: `ptr` returns a live value of the component, read before any world change.
        self.ptr::<Q>().map(|p| unsafe { (*p).clone() })
    }

    /// A copy of the value of pair `(T, target)` (a relationship that is a component, set in a script as
    /// `(T, target): {..}`), if the entity has it.
    #[must_use]
    pub fn try_cloned_pair<T: ComponentId>(self, target: impl IntoId) -> Option<T> {
        let (r, t) = (host::resolve::<T>(self.w), target.into_id(self.w));
        if r == 0 || t == 0 || !self.is_alive() || size_of::<T>() == 0 {
            return None;
        }
        // SAFETY: live world and entity; the pair's data is `T` (flecs types a pair by its relationship when that is a
        // component), null when the entity does not have it; read before any world change.
        let p = unsafe { sys::ecs_get_id(self.w.as_ptr(), self.id, pair(r, t)) }.cast::<T>();
        (!p.is_null()).then(|| unsafe { (*p).clone() })
    }

    /// The direct children, in flecs's order. Collected first, so `f` may change the world.
    pub fn each_child(self, mut f: impl FnMut(EntityView<'a>)) {
        if !self.is_alive() {
            return;
        }
        for c in self.w.children(self.id) {
            f(EntityView::new(self.w, c));
        }
    }

    pub fn child_of(self, parent: impl IntoId) -> Self {
        // SAFETY: an immutable flecs global.
        let child_of = unsafe { sys::EcsChildOf };
        let p = parent.into_id(self.w);
        assert!(p != 0 && self.w.is_alive(p), "child_of a parent that is not alive");
        self.add(pair(child_of, p))
    }

    /// Inherit from `prefab` (`(IsA, prefab)`): its components show through on this entity until it sets its own. An entity
    /// may inherit from several prefabs; where two give the same component, which one shows is flecs's order, not the call's.
    pub fn is_a(self, prefab: impl IntoId) -> Self {
        // SAFETY: an immutable flecs global.
        let is_a = unsafe { sys::EcsIsA };
        let p = prefab.into_id(self.w);
        assert!(p != 0 && self.w.is_alive(p), "is_a a prefab that is not alive");
        self.add(pair(is_a, p))
    }

    #[must_use]
    pub fn parent(self) -> Option<EntityView<'a>> {
        if !self.is_alive() {
            return None;
        }
        // SAFETY: live world and entity.
        let p = unsafe { sys::ecs_get_parent(self.w.as_ptr(), self.id) };
        (p != 0).then(|| EntityView::new(self.w, p))
    }

    /// The `index`th target of relationship `rel` on the entity.
    #[must_use]
    pub fn target(self, rel: impl IntoId, index: i32) -> Option<EntityView<'a>> {
        let r = rel.into_id(self.w);
        if r == 0 || !self.is_alive() {
            return None;
        }
        // SAFETY: live world and entity; `r` is a live id.
        let t = unsafe { sys::ecs_get_target(self.w.as_ptr(), self.id, r, index) };
        (t != 0).then(|| EntityView::new(self.w, t))
    }

    /// Every target of relationship `rel` on the entity.
    pub fn each_target(self, rel: impl IntoId, mut f: impl FnMut(EntityView<'a>)) {
        let mut i = 0;
        while let Some(t) = self.target(rel, i) {
            f(t);
            i += 1;
        }
    }

    /// The entity's name, empty when it has none.
    #[must_use]
    pub fn name(self) -> String {
        if self.is_alive() { self.w.name(self.id).unwrap_or_default() } else { String::new() }
    }

    /// The entity's full path from the root, its ancestors' names joined by `.` as a script names it (`launch.intro.logo`;
    /// `#123` for an unnamed one), for errors that name the thing.
    #[must_use]
    pub fn path(self) -> String {
        if !self.is_alive() {
            return format!("#{} (not alive)", self.id);
        }
        // SAFETY: live world and entity; flecs returns a string it allocated with its OS API, freed here with the same.
        unsafe {
            let p = sys::ecs_get_path_w_sep(self.w.as_ptr(), 0, self.id, c".".as_ptr(), core::ptr::null());
            if p.is_null() {
                return format!("#{}", self.id);
            }
            let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            if let Some(free) = sys::ecs_os_api.free_ {
                free(p.cast());
            }
            s
        }
    }

    /// Name the entity (unique among its parent's children; a prefab's named children are named on every instance too).
    /// Inside a system the name lands at the end of the system, as other writes do.
    pub fn set_name(self, name: &str) -> Self {
        assert!(self.is_alive(), "set_name on entity {} that is not alive", self.id);
        let n = std::ffi::CString::new(name).expect("an entity name without NUL");
        // SAFETY: live world and entity; flecs copies the name.
        unsafe { sys::ecs_set_name(self.w.as_ptr(), self.id, n.as_ptr()) };
        self
    }

    /// The direct child named `name` (an instantiated prefab part, a keyed row), if there is one.
    #[must_use]
    pub fn lookup_child(self, name: &str) -> Option<EntityView<'a>> {
        if !self.is_alive() {
            return None;
        }
        let n = std::ffi::CString::new(name).ok()?;
        // SAFETY: live world and entity; the name outlives the call.
        let c = unsafe { sys::ecs_lookup_child(self.w.as_ptr(), self.id, n.as_ptr()) };
        (c != 0).then(|| EntityView::new(self.w, c))
    }

    /// Put the children of this `OrderedChildren` parent in `order`, which must hold exactly its current children (live ones,
    /// not those created in the running system). False, and nothing changes, when the parent is not ordered or the set
    /// differs: flecs would refuse it.
    pub fn set_child_order(self, order: &[u64]) -> bool {
        if !self.is_alive() || !self.has(crate::flecs::OrderedChildren::ID) {
            return false;
        }
        let mut now = self.w.children(self.id);
        let mut want = order.to_vec();
        now.sort_unstable();
        want.sort_unstable();
        if now != want {
            return false;
        }
        // SAFETY: live world and parent; `order` is exactly its children, checked above.
        unsafe { sys::ecs_set_child_order(self.w.as_ptr(), self.id, order.as_ptr(), order.len() as i32) };
        true
    }

    /// Delete the entity (and its children).
    pub fn destruct(self) {
        if self.is_alive() {
            self.w.delete(self.id);
        }
    }
}

impl<R> Get<R> for EntityView<'_> {
    fn try_get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> Option<R> {
        // SAFETY: the value lives in the world's storage for the call; `f` cannot change the world's structure through this view
        // without a world handle, and flecs keeps a component's storage in place until the entity's table changes.
        self.ptr::<Q>().map(|p| f(unsafe { Q::out(p) }))
    }

    fn get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> R {
        let id = self.id;
        self.try_get::<Q>(f).unwrap_or_else(|| panic!("entity {id} does not have {}", core::any::type_name::<Q::C>()))
    }
}
