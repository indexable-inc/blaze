//! [`World`] (the host's, owning) and [`WorldRef`] (a borrowed handle: what a system, a view or a module gets).
use crate::flecs::{self, FlecsTrait};
use crate::IntoId;
use hyperion_ecs::Trait;
use crate::query::{QueryBuilder, QueryTuple};
use crate::script::ScriptBuilder;
use crate::system::{ObserverBuilder, SystemBuilder};
use crate::{Access, ComponentId, Entity, EntityView, Get, Query, host};
use core::marker::PhantomData;
use hyperion_ecs::sys;
use std::ffi::CString;

/// Path separator of `entity_named` and `lookup` (flecs-rust's): `den::app` is `app` under `den`.
const SEP: &core::ffi::CStr = c"::";

/// A borrowed world handle. Inside a system it is the system's stage, so its writes are deferred to the end of the system.
#[derive(Clone, Copy)]
pub struct WorldRef<'a> {
    w: hyperion_ecs::World,
    _world: PhantomData<&'a ()>,
}

impl<'a> WorldRef<'a> {
    pub(crate) fn new(w: hyperion_ecs::World) -> Self {
        Self { w, _world: PhantomData }
    }

    /// How many entities are alive (flecs's own included): the entity index's count, no query.
    #[must_use]
    pub fn alive_count(self) -> usize {
        // SAFETY: the world is live for 'a; this reads the entity index header.
        unsafe { sys::ecs_get_entities(self.w.as_ptr()).alive_count as usize }
    }

    /// A view of a world another image made (a module gets the host's at init).
    ///
    /// # Safety
    /// `p` is a live `ecs_world_t*` for `'a`.
    #[must_use]
    pub unsafe fn from_ptr(p: *mut core::ffi::c_void) -> Self {
        // SAFETY: the caller's contract.
        Self::new(unsafe { hyperion_ecs::World::from_raw(p) })
    }

    /// The raw hyperion handle (module glue and the loader).
    #[must_use]
    pub fn raw(self) -> hyperion_ecs::World {
        self.w
    }

    /// A new, empty entity.
    #[must_use]
    pub fn entity(self) -> EntityView<'a> {
        EntityView::new(self.w, self.w.new_entity())
    }

    /// The entity at `path` (`den::app`), created with its parents when missing.
    #[must_use]
    pub fn entity_named(self, path: &str) -> EntityView<'a> {
        let p = CString::new(path).expect("an entity path without NUL");
        // SAFETY: zeroed desc is flecs's "unset"; the strings outlive the call (flecs copies the names).
        let id = unsafe {
            let mut d: sys::ecs_entity_desc_t = core::mem::zeroed();
            d.name = p.as_ptr();
            d.sep = SEP.as_ptr();
            d.root_sep = SEP.as_ptr();
            sys::ecs_entity_init(self.w.as_ptr(), &d)
        };
        EntityView::new(self.w, id)
    }

    #[must_use]
    pub fn entity_from_id<I: Into<Entity>>(self, id: I) -> EntityView<'a> {
        EntityView::new(self.w, id.into().0)
    }

    /// The entity at `path`, if there is one.
    #[must_use]
    pub fn try_lookup(self, path: &str) -> Option<EntityView<'a>> {
        let p = CString::new(path).ok()?;
        // SAFETY: live world; the strings outlive the call.
        let id = unsafe { sys::ecs_lookup_path_w_sep(self.w.as_ptr(), 0, p.as_ptr(), SEP.as_ptr(), SEP.as_ptr(), true) };
        (id != 0).then(|| EntityView::new(self.w, id))
    }

    /// The entity at `path`, or the 0 entity (on which every read answers nothing).
    #[must_use]
    pub fn lookup(self, path: &str) -> EntityView<'a> {
        self.try_lookup(path).unwrap_or(EntityView::new(self.w, 0))
    }

    /// Register component `T` (host only; idempotent).
    pub fn component<T: ComponentId>(self) -> ComponentView<'a> {
        ComponentView { w: self.w, id: host::register::<T>(self.w), _world: PhantomData }
    }

    /// [`component`](Self::component), stating its world name (always the type's name).
    pub fn component_named<T: ComponentId>(self, name: &str) -> ComponentView<'a> {
        assert_eq!(name, T::NAME, "a component's world name is its type's name");
        self.component::<T>()
    }

    /// `T`'s id in this world (0 when it is not there).
    #[must_use]
    pub fn component_id<T: ComponentId>(self) -> Entity {
        Entity(host::resolve::<T>(self.w))
    }

    /// Set singleton `v` (stored on `T`'s own entity).
    pub fn set<T: ComponentId>(self, v: T) {
        let c = host::resolve::<T>(self.w);
        if c != 0 {
            EntityView::new(self.w, c).set(v);
        }
    }

    /// A query over `Q` (a component borrow or a tuple of them).
    #[must_use]
    pub fn new_query<Q: QueryTuple>(self) -> Query<'a, Q> {
        self.query::<Q>().build()
    }

    #[must_use]
    pub fn query<Q: QueryTuple>(self) -> QueryBuilder<'a, Q> {
        QueryBuilder::new(self.w)
    }

    /// A system named `name` over `Q`, run once per `progress` in the default phase.
    #[must_use]
    pub fn system_named<Q: QueryTuple + 'static>(self, name: &str) -> SystemBuilder<'a, Q> {
        SystemBuilder::new(self.w, name)
    }

    /// An observer named `name` over `Q`: runs when one of its [`Event`](crate::Event)s happens to an entity it matches.
    #[must_use]
    pub fn observer_named<Q: QueryTuple + 'static>(self, name: &str) -> ObserverBuilder<'a, Q> {
        ObserverBuilder::new(self.w, name)
    }

    /// A script entity at `path` (`scene::tokens`).
    #[must_use]
    pub fn script_named(self, path: &str) -> ScriptBuilder<'a> {
        ScriptBuilder::new(self, path)
    }

    /// Run every system once with `dt` seconds passed (0: a pass in which no time passes). A panic in a system is raised
    /// again here.
    pub fn progress_time(self, dt: f32) -> bool {
        // flecs reads a delta of 0 as "measure it": the systems would be handed the wall time since the last such pass.
        let more = self.w.progress(dt.max(f32::MIN_POSITIVE));
        host::rethrow();
        more
    }

    /// Run one system (a [`manual`](crate::SystemBuilder::manual) one, or any) once with `dt` seconds passed, its writes
    /// deferred to its end as in `progress`. A panic in it is raised again here.
    pub fn run_system(self, system: Entity, dt: f32) {
        if system.0 == 0 || !self.w.is_alive(system.0) {
            return;
        }
        // SAFETY: live world and system entity; no parameter is passed.
        unsafe { sys::ecs_run(self.w.as_ptr(), system.0, dt, core::ptr::null_mut()) };
        host::rethrow();
    }
}

/// A registered component, to declare its traits.
pub struct ComponentView<'a> {
    w: hyperion_ecs::World,
    id: hyperion_ecs::Id,
    _world: PhantomData<&'a ()>,
}

impl ComponentView<'_> {
    /// Declare trait `F` (before any entity or query uses the component; refused with a log line after).
    pub fn add_trait<F: FlecsTrait>(self) -> Self {
        self.w.traits(self.id, &[F::TRAIT]);
        self
    }

    /// `(With, id)`: an entity that gets this component also gets `id`. The one trait flecs allows on a component in use.
    pub fn with(self, id: impl IntoId) -> Self {
        let id = id.into_id(self.w);
        if id != 0 {
            self.w.traits(self.id, &[Trait::With(id)]);
        }
        self
    }

    /// `(OneOf, parent)`: a pair of this relationship must target a child of `parent` (an enum entity).
    pub fn one_of(self, parent: impl IntoId) -> Self {
        let id = parent.into_id(self.w);
        if id != 0 {
            self.w.traits(self.id, &[Trait::OneOf(id)]);
        }
        self
    }

    /// Make this component a child of `parent` (an enum entity whose relationship is `OneOf` it) and keep it findable by its bare
    /// name: reparenting moves its path to `parent.Name`, and every image resolves a component by its root name (a miss
    /// registers a second, unrelated component of that name, and the pair then targets a non-child), so the bare name becomes
    /// a world alias.
    pub fn child_of_aliased(self, parent: impl IntoId) -> Self {
        let parent = parent.into_id(self.w);
        assert!(parent != 0 && self.w.is_alive(parent), "child_of_aliased a parent that is not alive");
        // SAFETY: live world and entities; flecs owns the returned name string and copies the alias; an immutable flecs global.
        unsafe {
            let name = sys::ecs_get_name(self.w.as_ptr(), self.id);
            assert!(!name.is_null(), "child_of_aliased a component without a name");
            let name = std::ffi::CStr::from_ptr(name).to_owned();
            sys::ecs_add_id(self.w.as_ptr(), self.id, hyperion_ecs::pair(sys::EcsChildOf, parent));
            sys::ecs_set_alias(self.w.as_ptr(), self.id, name.as_ptr());
        }
        self
    }

    /// Keep this component findable by its bare name from inside any scope: a script resolves a name from the scope it is
    /// in outward, so inside `ui` the name `Node` finds the prefab `ui.Node` before the root component, while a world alias
    /// is looked up first (as flecs's builtins, `OrderedChildren`, are).
    pub fn aliased(self) -> Self {
        // SAFETY: live world and entity; flecs owns the returned name string and copies the alias.
        unsafe {
            let name = sys::ecs_get_name(self.w.as_ptr(), self.id);
            assert!(!name.is_null(), "aliased a component without a name");
            let name = std::ffi::CStr::from_ptr(name).to_owned();
            sys::ecs_set_alias(self.w.as_ptr(), self.id, name.as_ptr());
        }
        self
    }

    /// `OneOf`: a pair of this relationship must target one of its own children.
    pub fn one_of_self(self) -> Self {
        self.w.traits(self.id, &[Trait::OneOfSelf]);
        self
    }

    /// `(OnDelete, action)`: what deleting this component or relationship does to the entities that have it.
    pub fn on_delete(self, action: flecs::Cleanup) -> Self {
        self.w.traits(self.id, &[Trait::OnDelete(action)]);
        self
    }

    /// `(OnDeleteTarget, action)`: what deleting a pair's target does to the entities that have the pair (`Delete` is
    /// ownership, as `ChildOf`).
    pub fn on_delete_target(self, action: flecs::Cleanup) -> Self {
        self.w.traits(self.id, &[Trait::OnDeleteTarget(action)]);
        self
    }

    /// Say that scene scripts and cells may write the component: it is not den-native's own ([`HOST_OWNED`](crate::HOST_OWNED),
    /// which every component has from its registration until this).
    pub fn writable(self) -> Self {
        let host = self.w.lookup(crate::HOST_OWNED);
        if host != 0 {
            // SAFETY: live world; `self.id` is a registered component.
            unsafe { sys::ecs_remove_id(self.w.as_ptr(), self.id, host) };
        }
        self
    }

    #[must_use]
    pub fn id(&self) -> Entity {
        Entity(self.id)
    }
}

/// The host's world: made by the host image, ended when dropped.
pub struct World {
    w: hyperion_ecs::World,
}

impl World {
    /// A world with every addon the linked flecs has, owned by this image, which becomes the host (it registers components).
    ///
    /// # Safety
    /// flecs's OS API must be set first (harness-native-ecs's `init_os_api`), and this image must be the one that links flecs.
    #[must_use]
    pub unsafe fn init_host() -> Self {
        host::mark_host();
        // SAFETY: the caller set the OS API.
        Self { w: unsafe { hyperion_ecs::World::init() } }
    }

    #[must_use]
    pub fn world_ref(&self) -> WorldRef<'_> {
        WorldRef::new(self.w)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // SAFETY: the world is ours; no handle into it outlives `self` (views borrow it).
        unsafe { self.w.fini() };
        hyperion_ecs_core::identity::forget_worlds();
    }
}

macro_rules! forward {
    ($($(#[$m:meta])* fn $name:ident $(<$($g:ident $(: $b:path)?),*>)? ($($a:ident: $t:ty),*) $(-> $r:ty)?;)*) => {
        impl World {
            $(
                $(#[$m])*
                pub fn $name $(<$($g $(: $b)?),*>)? (&self, $($a: $t),*) $(-> $r)? {
                    self.world_ref().$name $(::<$($g),*>)? ($($a),*)
                }
            )*
        }
    };
}

forward! {
    #[must_use] fn raw() -> hyperion_ecs::World;
    #[must_use] fn entity() -> EntityView<'_>;
    #[must_use] fn entity_named(path: &str) -> EntityView<'_>;
    #[must_use] fn entity_from_id<I: Into<Entity>>(id: I) -> EntityView<'_>;
    #[must_use] fn try_lookup(path: &str) -> Option<EntityView<'_>>;
    #[must_use] fn lookup(path: &str) -> EntityView<'_>;
    fn component<T: ComponentId>() -> ComponentView<'_>;
    fn component_named<T: ComponentId>(name: &str) -> ComponentView<'_>;
    #[must_use] fn component_id<T: ComponentId>() -> Entity;
    fn set<T: ComponentId>(v: T);
    #[must_use] fn new_query<Q: QueryTuple>() -> Query<'_, Q>;
    #[must_use] fn query<Q: QueryTuple>() -> QueryBuilder<'_, Q>;
    #[must_use] fn script_named(path: &str) -> ScriptBuilder<'_>;
    fn progress_time(dt: f32) -> bool;
    fn run_system(system: Entity, dt: f32);
}

impl World {
    #[must_use]
    pub fn system_named<Q: QueryTuple + 'static>(&self, name: &str) -> SystemBuilder<'_, Q> {
        self.world_ref().system_named::<Q>(name)
    }

    #[must_use]
    pub fn observer_named<Q: QueryTuple + 'static>(&self, name: &str) -> ObserverBuilder<'_, Q> {
        self.world_ref().observer_named::<Q>(name)
    }
}

/// Singletons: stored on their component's own entity, which must have them for `get`.
impl<R> Get<R> for WorldRef<'_> {
    fn try_get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> Option<R> {
        EntityView::new(self.w, Q::id(self.w)).try_get::<Q>(f)
    }

    fn get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> R {
        EntityView::new(self.w, Q::id(self.w)).get::<Q>(f)
    }
}

impl<R> Get<R> for &World {
    fn try_get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> Option<R> {
        self.world_ref().try_get::<Q>(f)
    }

    fn get<Q: Access>(self, f: impl FnOnce(Q::Out<'_>) -> R) -> R {
        self.world_ref().get::<Q>(f)
    }
}
