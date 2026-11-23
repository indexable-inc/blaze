//! Core ECS [`World`] managing generational entities, registered components, archetype tables,
//! table-graph edges, relations/traits, prefab inheritance, queries, observers, and snapshots.

use core::alloc::Allocator;
use core::any::TypeId;
use core::fmt::Write as _;

use blaze_own::{AllocFailure, String as OwnString, Vec as OwnVec};

use crate::data::{Int, Map, Value};

use super::column::{
    AnyColumn, ComponentMeta, downcast_typed_mut, enable_copy_any, enable_reflect_any,
    make_column_plain, validate_reflected_value,
};
use super::error::EcsError;
use super::fetch::{QueryFetch, TypedQuery};
use super::id::{Builtins, ComponentId, Entity, Id, TableId, u32_to_usize, usize_to_u32};
use super::map::{FlatMap, FlatSet};
use super::observer::{DEFAULT_OBSERVER_CAP, Event, EventKind, Observer, ObserverCallback, ObserverId};
use super::query::{CachedQuery, QuerySpec};
use super::reflect::{ReflectComponent, ReflectError};
use super::table::{ArchetypeKey, Table, move_row_between_tables};
use super::typed::{
    Bundle, ChildOf, EntityMut, IsA, MAX_PREFAB_DEPTH, Prefab, PrefabInit, Relation,
    prefab_init_for, short_type_name,
};

struct EntitySlot<A: Allocator + Copy> {
    generation: u32,
    alive: bool,
    table_id: TableId,
    row: usize,
    name: Option<OwnString<A>>,
}

/// Complete Flecs 4.2-equivalent archetype Entity-Component-System world backed by an explicit
/// caller-supplied `Allocator`.
pub struct World<A: Allocator + Copy + 'static> {
    alloc: A,
    builtins: Builtins,
    slots: OwnVec<EntitySlot<A>, A>,
    free_slots: OwnVec<u32, A>,
    names: FlatMap<OwnString<A>, Entity, A>,
    components: OwnVec<ComponentMeta<A>, A>,
    component_by_type: FlatMap<TypeId, ComponentId, A>,
    tag_by_type: FlatMap<TypeId, Entity, A>,
    relation_by_type: FlatMap<TypeId, Entity, A>,
    prefab_by_type: FlatMap<TypeId, Entity, A>,
    tables: OwnVec<Table<A>, A>,
    table_by_archetype: FlatMap<ArchetypeKey<A>, TableId, A>,
    observers: OwnVec<Observer<A>, A>,
    live_count: usize,
    #[cfg(test)]
    plant_skip_swap_row_update: bool,
}

impl<A: Allocator + Copy + 'static> World<A> {
    /// Create a new ECS world in `alloc` with deterministic built-in relation and trait entities
    /// (`ChildOf`, `IsA`, `Prefab`, `Wildcard`, `Exclusive`, `Transitive`, `Symmetric`,
    /// `OnDeleteCascade`).
    pub fn new_in(alloc: A) -> Result<Self, EcsError> {
        let mut tables = OwnVec::new_in(alloc);
        let empty_arch = OwnVec::new_in(alloc);
        let empty_table = Table::new_in(TableId(0), empty_arch, &[], alloc)?;
        tables.try_push(empty_table).map_err(|r| EcsError::Alloc(r.why))?;

        let mut table_by_archetype = FlatMap::new_in(alloc);
        let _ = table_by_archetype.insert(ArchetypeKey(OwnVec::new_in(alloc)), TableId(0))?;

        let placeholder_builtins = Builtins {
            child_of: Entity::NULL,
            is_a: Entity::NULL,
            prefab: Entity::NULL,
            wildcard: Entity::NULL,
            exclusive: Entity::NULL,
            transitive: Entity::NULL,
            symmetric: Entity::NULL,
            on_delete_cascade: Entity::NULL,
        };

        let mut world = Self {
            alloc,
            builtins: placeholder_builtins,
            slots: OwnVec::new_in(alloc),
            free_slots: OwnVec::new_in(alloc),
            names: FlatMap::new_in(alloc),
            components: OwnVec::new_in(alloc),
            component_by_type: FlatMap::new_in(alloc),
            tag_by_type: FlatMap::new_in(alloc),
            relation_by_type: FlatMap::new_in(alloc),
            prefab_by_type: FlatMap::new_in(alloc),
            tables,
            table_by_archetype,
            observers: OwnVec::new_in(alloc),
            live_count: 0,
            #[cfg(test)]
            plant_skip_swap_row_update: false,
        };

        let child_of = world.spawn_named("ChildOf")?;
        let is_a = world.spawn_named("IsA")?;
        let prefab = world.spawn_named("Prefab")?;
        let wildcard = world.spawn_named("Wildcard")?;
        let exclusive = world.spawn_named("Exclusive")?;
        let transitive = world.spawn_named("Transitive")?;
        let symmetric = world.spawn_named("Symmetric")?;
        let on_delete_cascade = world.spawn_named("OnDeleteCascade")?;

        world.builtins = Builtins {
            child_of,
            is_a,
            prefab,
            wildcard,
            exclusive,
            transitive,
            symmetric,
            on_delete_cascade,
        };

        // `ChildOf` is exclusive and cascades deletion to children, matching Flecs 4.2.
        world.add_tag(child_of, exclusive)?;
        world.add_tag(child_of, on_delete_cascade)?;

        Ok(world)
    }

    /// Test-only planted bug hook for `blaze_sim::control`: when `true`, skips updating the
    /// swap-moved entity's `slot.row` on swap-remove.
    #[cfg(test)]
    pub(crate) fn set_plant_skip_swap_row_update(&mut self, plant: bool) {
        self.plant_skip_swap_row_update = plant;
    }

    /// Allocator handle owned by this world.
    #[inline]
    pub fn alloc(&self) -> A {
        self.alloc
    }

    /// All built-in relation and trait entity handles.
    #[inline]
    pub fn builtins(&self) -> Builtins {
        self.builtins
    }

    /// Built-in `ChildOf` exclusive hierarchy relation (`Exclusive` + `OnDeleteCascade`).
    #[inline]
    pub fn child_of(&self) -> Entity {
        self.builtins.child_of
    }

    /// Built-in `IsA` prefab inheritance relation.
    #[inline]
    pub fn is_a(&self) -> Entity {
        self.builtins.is_a
    }

    /// Built-in `Prefab` tag (prefabs are excluded from default queries).
    #[inline]
    pub fn prefab(&self) -> Entity {
        self.builtins.prefab
    }

    /// Built-in `Wildcard` (`*`) sentinel for queries and observers.
    #[inline]
    pub fn wildcard(&self) -> Entity {
        self.builtins.wildcard
    }

    /// Built-in `Exclusive` relation trait tag.
    #[inline]
    pub fn exclusive(&self) -> Entity {
        self.builtins.exclusive
    }

    /// Built-in `Transitive` relation trait tag.
    #[inline]
    pub fn transitive(&self) -> Entity {
        self.builtins.transitive
    }

    /// Built-in `Symmetric` relation trait tag.
    #[inline]
    pub fn symmetric(&self) -> Entity {
        self.builtins.symmetric
    }

    /// Built-in `OnDeleteCascade` relation trait tag.
    #[inline]
    pub fn on_delete_cascade(&self) -> Entity {
        self.builtins.on_delete_cascade
    }

    /// Number of currently alive entities (including built-in entities).
    #[inline]
    pub fn live_count(&self) -> usize {
        self.live_count
    }

    /// Number of registered component types.
    #[inline]
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    /// Number of archetype tables (always at least 1 for the empty table).
    #[inline]
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Read-only slice of all archetype tables.
    #[inline]
    pub fn tables(&self) -> &[Table<A>] {
        self.tables.as_slice()
    }

    /// Access an archetype table by [`TableId`].
    #[inline]
    pub fn table(&self, tid: TableId) -> Result<&Table<A>, EcsError> {
        self.tables.get(u32_to_usize(tid.0)).ok_or(EcsError::InvalidTable(tid))
    }

    /// Mutably access an archetype table by [`TableId`].
    #[inline]
    pub fn table_mut(&mut self, tid: TableId) -> Result<&mut Table<A>, EcsError> {
        self.tables.get_mut(u32_to_usize(tid.0)).ok_or(EcsError::InvalidTable(tid))
    }

    // -------------------------------------------------------------------------
    // Component Registration
    // -------------------------------------------------------------------------

    /// Register a non-copyable component type `T` using its auto-shortened Rust type name.
    #[inline]
    pub fn register<T: 'static>(&mut self) -> Result<ComponentId, EcsError> {
        self.register_component::<T>(short_type_name::<T>())
    }

    /// Register a `Copy` component type `T` using its auto-shortened Rust type name.
    #[inline]
    pub fn register_copy<T: Copy + 'static>(&mut self) -> Result<ComponentId, EcsError> {
        self.register_copy_component::<T>(short_type_name::<T>())
    }

    /// Register a live-reflected component type `T` using its auto-shortened Rust type name.
    #[inline]
    pub fn register_reflected<T: ReflectComponent + 'static>(
        &mut self,
    ) -> Result<ComponentId, EcsError> {
        self.register_reflected_named::<T>(short_type_name::<T>())
    }

    /// Register a live-reflected component type `T` under an explicit `name`.
    pub fn register_reflected_named<T: ReflectComponent + 'static>(
        &mut self,
        name: &str,
    ) -> Result<ComponentId, EcsError> {
        self.register_component_inner::<T>(
            name,
            None,
            None,
            Some(enable_reflect_any::<T, A>),
            false,
            true,
            Some(T::schema::<A>),
            Some(validate_reflected_value::<T, A>),
        )
    }

    /// Register a live-reflected `Copy` component type `T` using its auto-shortened Rust type name.
    #[inline]
    pub fn register_reflected_copy<T: ReflectComponent + Copy + 'static>(
        &mut self,
    ) -> Result<ComponentId, EcsError> {
        self.register_reflected_copy_named::<T>(short_type_name::<T>())
    }

    /// Register a live-reflected `Copy` component type `T` under an explicit `name`.
    pub fn register_reflected_copy_named<T: ReflectComponent + Copy + 'static>(
        &mut self,
        name: &str,
    ) -> Result<ComponentId, EcsError> {
        self.register_component_inner::<T>(
            name,
            None,
            Some(enable_copy_any::<T, A>),
            Some(enable_reflect_any::<T, A>),
            true,
            true,
            Some(T::schema::<A>),
            Some(validate_reflected_value::<T, A>),
        )
    }

    /// Return the [`ComponentId`] for `T`, lazily registering `T` under [`short_type_name::<T>()`]
    /// if not yet registered.
    pub fn component<T: 'static>(&mut self) -> Result<ComponentId, EcsError> {
        if let Some(cid) = self.component_id::<T>() {
            return Ok(cid);
        }
        self.register_component::<T>(short_type_name::<T>())
    }

    /// Return the [`ComponentId`] for `Copy` type `T`, lazily registering `T` as copyable under
    /// [`short_type_name::<T>()`] if not yet registered (and upgrading existing non-copyable
    /// metadata/columns so prefab instantiation copies `T` seamlessly without losing reflection).
    pub fn component_copy<T: Copy + 'static>(&mut self) -> Result<ComponentId, EcsError> {
        if let Some(cid) = self.component_id::<T>() {
            let needs_upgrade = self
                .components
                .get(u32_to_usize(cid.0))
                .is_some_and(|meta| !meta.can_copy);
            if needs_upgrade {
                if let Some(meta) = self.components.get_mut(u32_to_usize(cid.0)) {
                    meta.enable_copy_col = Some(enable_copy_any::<T, A>);
                    meta.can_copy = true;
                }
                for table in self.tables.as_mut_slice() {
                    if let Some(idx) = table.column_index(cid) {
                        if let Some(entry) = table.columns.get_mut(idx) {
                            let typed = downcast_typed_mut::<T, A>(entry.col.as_mut(), cid)?;
                            typed.enable_copy();
                        }
                    }
                }
            }
            return Ok(cid);
        }
        self.register_copy_component::<T>(short_type_name::<T>())
    }

    /// Return the [`ComponentId`] for reflected type `T`, lazily registering or upgrading `T` with
    /// live reflection enabled.
    pub fn component_reflected<T: ReflectComponent + 'static>(
        &mut self,
    ) -> Result<ComponentId, EcsError> {
        if let Some(cid) = self.component_id::<T>() {
            let needs_upgrade = self
                .components
                .get(u32_to_usize(cid.0))
                .is_some_and(|meta| !meta.reflected);
            if needs_upgrade {
                if let Some(meta) = self.components.get_mut(u32_to_usize(cid.0)) {
                    meta.enable_reflect_col = Some(enable_reflect_any::<T, A>);
                    meta.reflected = true;
                    meta.schema_fn = Some(T::schema::<A>);
                    meta.validate_value_fn = Some(validate_reflected_value::<T, A>);
                }
                for table in self.tables.as_mut_slice() {
                    if let Some(idx) = table.column_index(cid) {
                        if let Some(entry) = table.columns.get_mut(idx) {
                            let typed = downcast_typed_mut::<T, A>(entry.col.as_mut(), cid)?;
                            typed.enable_reflect();
                        }
                    }
                }
            }
            return Ok(cid);
        }
        self.register_reflected::<T>()
    }

    /// Return the [`ComponentId`] for reflected `Copy` type `T`, lazily registering or upgrading
    /// `T` with both copyability and live reflection enabled.
    pub fn component_reflected_copy<T: ReflectComponent + Copy + 'static>(
        &mut self,
    ) -> Result<ComponentId, EcsError> {
        let cid = self.component_reflected::<T>()?;
        let _ = self.component_copy::<T>()?;
        Ok(cid)
    }

    /// Register a non-copyable component type `T`.
    pub fn register_component<T: 'static>(&mut self, name: &str) -> Result<ComponentId, EcsError> {
        self.register_component_inner::<T>(name, None, None, None, false, false, None, None)
    }

    /// Register a non-copyable component type `T` with an optional Blaze record schema ID hook.
    pub fn register_component_with_schema<T: 'static>(
        &mut self,
        name: &str,
        schema_id: u64,
    ) -> Result<ComponentId, EcsError> {
        self.register_component_inner::<T>(
            name,
            Some(schema_id),
            None,
            None,
            false,
            false,
            None,
            None,
        )
    }

    /// Register a `Copy` component type `T` (enables value duplication during `instantiate`).
    pub fn register_copy_component<T: Copy + 'static>(
        &mut self,
        name: &str,
    ) -> Result<ComponentId, EcsError> {
        self.register_component_inner::<T>(
            name,
            None,
            Some(enable_copy_any::<T, A>),
            None,
            true,
            false,
            None,
            None,
        )
    }

    /// Register a `Copy` component type `T` with an optional Blaze record schema ID hook.
    pub fn register_copy_component_with_schema<T: Copy + 'static>(
        &mut self,
        name: &str,
        schema_id: u64,
    ) -> Result<ComponentId, EcsError> {
        self.register_component_inner::<T>(
            name,
            Some(schema_id),
            Some(enable_copy_any::<T, A>),
            None,
            true,
            false,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_component_inner<T: 'static>(
        &mut self,
        name: &str,
        schema_id: Option<u64>,
        enable_copy_col: Option<fn(&mut dyn AnyColumn<A>)>,
        enable_reflect_col: Option<fn(&mut dyn AnyColumn<A>)>,
        can_copy: bool,
        reflected: bool,
        schema_fn: Option<fn(A) -> Result<Value<A>, AllocFailure>>,
        validate_value_fn: Option<fn(&Value<A>, A) -> Result<(), ReflectError>>,
    ) -> Result<ComponentId, EcsError> {
        let tid = TypeId::of::<T>();
        if let Some(&existing) = self.component_by_type.get(&tid) {
            return Err(EcsError::ComponentAlreadyRegistered(existing));
        }
        let raw_id = usize_to_u32(self.components.len()).ok_or(EcsError::ComponentCapacityExceeded)?;
        let cid = ComponentId(raw_id);
        let owned_name = OwnString::try_from_str_in(name, self.alloc)?;
        self.components
            .try_push(ComponentMeta {
                id: cid,
                name: owned_name,
                type_id: tid,
                size: core::mem::size_of::<T>(),
                schema_id,
                make_column: make_column_plain::<T, A>,
                enable_copy_col,
                enable_reflect_col,
                can_copy,
                reflected,
                schema_fn,
                validate_value_fn,
            })
            .map_err(|r| EcsError::Alloc(r.why))?;
        let _ = self.component_by_type.insert(tid, cid)?;
        Ok(cid)
    }

    /// Look up the registered [`ComponentId`] for Rust type `T`.
    #[inline]
    pub fn component_id<T: 'static>(&self) -> Option<ComponentId> {
        self.component_by_type.get(&TypeId::of::<T>()).copied()
    }

    /// Look up a registered [`ComponentId`] by its human-readable component name.
    pub fn component_id_by_name(&self, name: &str) -> Option<ComponentId> {
        for meta in self.components.as_slice() {
            if meta.name() == name {
                return Some(meta.id);
            }
        }
        None
    }

    /// Inspect metadata for a registered [`ComponentId`].
    #[inline]
    pub fn component_meta(&self, cid: ComponentId) -> Option<&ComponentMeta<A>> {
        self.components.get(u32_to_usize(cid.0))
    }

    /// Read-only slice of all registered component metadata entries.
    #[inline]
    pub fn components(&self) -> &[ComponentMeta<A>] {
        self.components.as_slice()
    }

    /// Look up the currently live [`Entity`] occupying `slot`, if any.
    #[inline]
    pub fn entity_by_slot(&self, slot: u32) -> Option<Entity> {
        let entry = self.slots.get(u32_to_usize(slot))?;
        if entry.alive { Some(Entity::from_parts(slot, entry.generation)) } else { None }
    }

    // -------------------------------------------------------------------------
    // Entity Lifecycle
    // -------------------------------------------------------------------------

    /// Whether `entity` refers to a currently live entity (checks slot bounds and generation).
    #[inline]
    pub fn is_alive(&self, entity: Entity) -> bool {
        if entity.is_null() {
            return false;
        }
        match self.slots.get(u32_to_usize(entity.slot())) {
            Some(slot) => slot.alive && slot.generation == entity.generation(),
            None => false,
        }
    }

    #[inline]
    fn live_slot(&self, entity: Entity) -> Result<&EntitySlot<A>, EcsError> {
        if entity.is_null() {
            return Err(EcsError::DeadEntity(entity));
        }
        let slot = self.slots.get(u32_to_usize(entity.slot())).ok_or(EcsError::DeadEntity(entity))?;
        if slot.alive && slot.generation == entity.generation() {
            Ok(slot)
        } else {
            Err(EcsError::DeadEntity(entity))
        }
    }

    /// Return the current `(TableId, row)` of a live entity.
    #[inline]
    pub fn entity_table(&self, entity: Entity) -> Result<(TableId, usize), EcsError> {
        let slot = self.live_slot(entity)?;
        Ok((slot.table_id, slot.row))
    }

    /// Return the sorted canonical archetype slice of a live entity.
    #[inline]
    pub fn entity_archetype(&self, entity: Entity) -> Result<&[Id], EcsError> {
        let slot = self.live_slot(entity)?;
        let table = self.table(slot.table_id)?;
        Ok(table.archetype())
    }

    /// Spawn a new anonymous entity into the empty archetype table (`TableId(0)`).
    pub fn spawn(&mut self) -> Result<Entity, EcsError> {
        let empty_table = self.tables.get_mut(0).ok_or(EcsError::InvalidTable(TableId(0)))?;
        let row = empty_table.entities.len();
        let entity = if let Some(slot_idx) = self.free_slots.pop() {
            let slot = self
                .slots
                .get_mut(u32_to_usize(slot_idx))
                .ok_or(EcsError::EntityCapacityExceeded)?;
            let entity = Entity::from_parts(slot_idx, slot.generation);
            empty_table.entities.try_push(entity).map_err(|r| EcsError::Alloc(r.why))?;
            slot.alive = true;
            slot.table_id = TableId(0);
            slot.row = row;
            slot.name = None;
            entity
        } else {
            let slot_idx =
                usize_to_u32(self.slots.len()).ok_or(EcsError::EntityCapacityExceeded)?;
            let generation = 1u32;
            let entity = Entity::from_parts(slot_idx, generation);
            empty_table.entities.try_push(entity).map_err(|r| EcsError::Alloc(r.why))?;
            self.slots
                .try_push(EntitySlot {
                    generation,
                    alive: true,
                    table_id: TableId(0),
                    row,
                    name: None,
                })
                .map_err(|r| EcsError::Alloc(r.why))?;
            entity
        };
        self.live_count = self.live_count.saturating_add(1);
        Ok(entity)
    }

    /// Spawn a new uniquely named entity.
    pub fn spawn_named(&mut self, name: &str) -> Result<Entity, EcsError> {
        if self.names.get(name).is_some() {
            return Err(EcsError::DuplicateEntityName);
        }
        let entity = self.spawn()?;
        let key = OwnString::try_from_str_in(name, self.alloc)?;
        let stored = OwnString::try_from_str_in(name, self.alloc)?;
        if let Some(slot) = self.slots.get_mut(u32_to_usize(entity.slot())) {
            slot.name = Some(stored);
        }
        let _ = self.names.insert(key, entity)?;
        Ok(entity)
    }

    /// Look up a live entity by its unique name.
    #[inline]
    pub fn entity_by_name(&self, name: &str) -> Option<Entity> {
        self.names.get(name).copied()
    }

    /// Return the name of a live entity, if it was spawned with one.
    #[inline]
    pub fn entity_name(&self, entity: Entity) -> Option<&str> {
        self.live_slot(entity).ok()?.name.as_ref().map(|s| s.as_str())
    }

    /// Assign or replace the unique name of a live entity.
    pub fn set_name(&mut self, entity: Entity, name: &str) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        if let Some(&owner) = self.names.get(name) {
            if owner == entity {
                return Ok(());
            }
            return Err(EcsError::DuplicateEntityName);
        }
        let key = OwnString::try_from_str_in(name, self.alloc)?;
        let stored = OwnString::try_from_str_in(name, self.alloc)?;
        if let Some(slot) = self.slots.get_mut(u32_to_usize(entity.slot())) {
            if let Some(old_name) = slot.name.take() {
                let _ = self.names.remove(old_name.as_str());
            }
            slot.name = Some(stored);
        }
        let _ = self.names.insert(key, entity)?;
        Ok(())
    }

    /// Spawn a typed marker entity (tag, relation, or prefab), preferring its short type name and
    /// falling back to full type name or anonymous entity if the name is already taken.
    fn spawn_typed_entity(&mut self, short: &str, full: &str) -> Result<Entity, EcsError> {
        if self.names.get(short).is_none() {
            self.spawn_named(short)
        } else if self.names.get(full).is_none() {
            self.spawn_named(full)
        } else {
            self.spawn()
        }
    }

    /// Spawn a new entity and return a fluent [`EntityMut`] handle.
    #[inline]
    pub fn spawn_ent(&mut self) -> Result<EntityMut<'_, A>, EcsError> {
        let entity = self.spawn()?;
        Ok(EntityMut::new(self, entity))
    }

    /// Spawn a new entity initialized with component tuple `bundle` and return its [`Entity`] ID.
    pub fn spawn_bundle<B: Bundle>(&mut self, bundle: B) -> Result<Entity, EcsError> {
        let entity = self.spawn()?;
        bundle.insert_into(self, entity)?;
        Ok(entity)
    }

    /// Spawn a new entity initialized with component tuple `bundle` and return a fluent
    /// [`EntityMut`] handle.
    pub fn spawn_with<B: Bundle>(&mut self, bundle: B) -> Result<EntityMut<'_, A>, EcsError> {
        let entity = self.spawn_bundle(bundle)?;
        Ok(EntityMut::new(self, entity))
    }

    /// Instantiate a live entity from typed prefab `P` (lazily creating `P` and its ancestor
    /// prefab chain if needed) and return a fluent [`EntityMut`] handle.
    pub fn spawn_of<P: Prefab>(&mut self) -> Result<EntityMut<'_, A>, EcsError> {
        let prefab = self.prefab_entity::<P>()?;
        let inst = self.instantiate(prefab)?;
        Ok(EntityMut::new(self, inst))
    }

    /// Borrow an existing live entity as a fluent [`EntityMut`] handle.
    #[inline]
    pub fn entity_mut(&mut self, entity: Entity) -> Result<EntityMut<'_, A>, EcsError> {
        let _ = self.live_slot(entity)?;
        Ok(EntityMut::new(self, entity))
    }

    /// Spawn a named prefab entity tagged with `world.prefab()`.
    pub fn spawn_prefab(&mut self, name: &str) -> Result<Entity, EcsError> {
        let entity = self.spawn_named(name)?;
        self.add_tag(entity, self.builtins.prefab)?;
        Ok(entity)
    }

    /// Instantiate a live entity from `prefab`: attaches `(IsA, prefab)` and copies all
    /// copy-registered components from `prefab` and its `IsA` ancestor chain (nearest ancestor
    /// wins).
    pub fn instantiate(&mut self, prefab: Entity) -> Result<Entity, EcsError> {
        let _ = self.live_slot(prefab)?;
        if prefab == self.builtins.wildcard {
            return Err(EcsError::InvalidWildcardUse);
        }

        // 1. Collect nearest-ancestor copyable components iteratively via BFS over `IsA`.
        let step_cap = self.slots.len().saturating_add(1);
        let mut queue = OwnVec::new_in(self.alloc);
        let mut visited: FlatSet<Entity, A> = FlatSet::new_in(self.alloc);
        let mut seen_comps: FlatSet<ComponentId, A> = FlatSet::new_in(self.alloc);
        let mut sources: OwnVec<(ComponentId, TableId, usize), A> = OwnVec::new_in(self.alloc);

        queue.try_push(prefab).map_err(|r| EcsError::Alloc(r.why))?;
        let _ = visited.insert(prefab)?;
        let mut head = 0usize;
        let mut steps = 0usize;
        while let Some(&cur) = queue.get(head) {
            head += 1;
            steps += 1;
            if steps > step_cap {
                return Err(EcsError::TraversalBoundExceeded { limit: step_cap });
            }
            let Ok(slot) = self.live_slot(cur) else {
                continue;
            };
            let tid = slot.table_id;
            let row = slot.row;
            let table = self.table(tid)?;
            for &id in table.archetype() {
                if let Id::Component(cid) = id {
                    let can_copy = self.components.get(u32_to_usize(cid.0)).is_some_and(|m| m.can_copy);
                    if can_copy && !seen_comps.contains(&cid) {
                        let _ = seen_comps.insert(cid)?;
                        sources
                            .try_push((cid, tid, row))
                            .map_err(|r| EcsError::Alloc(r.why))?;
                    }
                } else if let Id::Pair(rel, target) = id {
                    if rel == self.builtins.is_a && !visited.contains(&target) {
                        let _ = visited.insert(target)?;
                        queue.try_push(target).map_err(|r| EcsError::Alloc(r.why))?;
                    }
                }
            }
        }

        sources.as_mut_slice().sort_unstable_by_key(|entry| entry.0);

        // 2. Spawn the instance and build its target archetype: `(IsA, prefab)` + copied components.
        let inst = self.spawn()?;
        let is_a_pair = Id::Pair(self.builtins.is_a, prefab);
        let mut target_arch = OwnVec::try_with_capacity_in(sources.len().saturating_add(1), self.alloc)?;
        for &(cid, _, _) in sources.as_slice() {
            target_arch.try_push(Id::Component(cid)).map_err(|r| EcsError::Alloc(r.why))?;
        }
        target_arch.try_push(is_a_pair).map_err(|r| EcsError::Alloc(r.why))?;
        target_arch.as_mut_slice().sort_unstable();

        let dst_tid = self.find_or_create_table(target_arch)?;

        // 3. Remove `inst` from TableId(0) (empty table) and update swapped row if any.
        let (inst_src_tid, inst_src_row) = self.entity_table(inst)?;
        let moved_in_empty = {
            let empty_tbl = self.table_mut(inst_src_tid)?;
            empty_tbl.swap_remove_drop_row(inst_src_row)?
        };
        if let Some(moved) = moved_in_empty {
            self.update_moved_row(moved, inst_src_row);
        }

        // 4. Push `inst` into `dst_tid` and copy every component value from its source table.
        let dst_row = {
            let dst_tbl = self.table_mut(dst_tid)?;
            let row = dst_tbl.entities.len();
            dst_tbl.entities.try_push(inst).map_err(|r| EcsError::Alloc(r.why))?;
            row
        };

        for &(cid, src_tid, src_row) in sources.as_slice() {
            if src_tid == dst_tid {
                return Err(EcsError::HierarchyCycle {
                    entity: inst,
                    relation: self.builtins.is_a,
                    target: prefab,
                });
            }
            let src_idx = u32_to_usize(src_tid.0);
            let dst_idx = u32_to_usize(dst_tid.0);
            let slice = self.tables.as_mut_slice();
            let (src_tbl, dst_tbl) = if src_idx < dst_idx {
                let (left, right) =
                    slice.split_at_mut_checked(dst_idx).ok_or(EcsError::InvalidTable(dst_tid))?;
                let s = left.get(src_idx).ok_or(EcsError::InvalidTable(src_tid))?;
                let d = right.get_mut(0).ok_or(EcsError::InvalidTable(dst_tid))?;
                (s, d)
            } else {
                let (left, right) =
                    slice.split_at_mut_checked(src_idx).ok_or(EcsError::InvalidTable(src_tid))?;
                let d = left.get_mut(dst_idx).ok_or(EcsError::InvalidTable(dst_tid))?;
                let s = right.get(0).ok_or(EcsError::InvalidTable(src_tid))?;
                (s, d)
            };
            let s_col_idx = src_tbl.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
            let d_col_idx = dst_tbl.column_index(cid).ok_or(EcsError::MissingComponent(cid))?;
            let s_col = src_tbl.columns.get(s_col_idx).ok_or(EcsError::MissingComponent(cid))?;
            let d_col = dst_tbl.columns.get_mut(d_col_idx).ok_or(EcsError::MissingComponent(cid))?;
            let _ = s_col.col.copy_row_to(src_tid, src_row, cid, d_col.col.as_mut())?;
        }

        if let Some(slot) = self.slots.get_mut(u32_to_usize(inst.slot())) {
            slot.table_id = dst_tid;
            slot.row = dst_row;
        }

        // 5. Fire observers for `(IsA, prefab)` and all copied components.
        self.emit_event(EventKind::OnAdd, inst, is_a_pair)?;
        for &(cid, _, _) in sources.as_slice() {
            self.emit_event(EventKind::OnAdd, inst, Id::Component(cid))?;
            self.emit_event(EventKind::OnSet, inst, Id::Component(cid))?;
        }

        Ok(inst)
    }

    /// Despawn `entity` (respecting `OnDeleteCascade`, cleaning up incoming/symmetric pairs on
    /// surviving entities, firing `OnRemove` observers, and bumping the entity's generation).
    ///
    /// 100% iterative with bounded traversal.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;

        // 1. Collect transitive `OnDeleteCascade` closure iteratively.
        let mut to_despawn = OwnVec::new_in(self.alloc);
        let mut scheduled: FlatSet<Entity, A> = FlatSet::new_in(self.alloc);
        to_despawn.try_push(entity).map_err(|r| EcsError::Alloc(r.why))?;
        let _ = scheduled.insert(entity)?;

        let mut head = 0usize;
        while let Some(&target) = to_despawn.get(head) {
            head += 1;
            for table in self.tables.as_slice() {
                if table.is_empty() {
                    continue;
                }
                let mut cascades = false;
                for &id in table.archetype() {
                    if let Id::Pair(rel, pair_target) = id {
                        if pair_target == target && self.entity_has_tag_raw(rel, self.builtins.on_delete_cascade) {
                            cascades = true;
                            break;
                        }
                    }
                }
                if cascades {
                    for &child in table.entities() {
                        if !scheduled.contains(&child) {
                            let _ = scheduled.insert(child)?;
                            to_despawn.try_push(child).map_err(|r| EcsError::Alloc(r.why))?;
                        }
                    }
                }
            }
        }

        // 2. Collect dangling pairs/tags referencing any entity in `scheduled` on surviving entities
        //    (covers `Symmetric` pairs, `IsA`, `LocatedIn`, custom tags, etc.).
        let mut cleanup_ids: OwnVec<(Entity, Id), A> = OwnVec::new_in(self.alloc);
        for table in self.tables.as_slice() {
            if table.is_empty() {
                continue;
            }
            for &id in table.archetype() {
                let references_dead = match id {
                    Id::Component(_) => false,
                    Id::Tag(t) => scheduled.contains(&t),
                    Id::Pair(r, t) => scheduled.contains(&r) || scheduled.contains(&t),
                };
                if references_dead {
                    for &holder in table.entities() {
                        if !scheduled.contains(&holder) {
                            cleanup_ids
                                .try_push((holder, id))
                                .map_err(|r| EcsError::Alloc(r.why))?;
                        }
                    }
                }
            }
        }

        for &(holder, id) in cleanup_ids.as_slice() {
            if self.is_alive(holder) {
                let _ = self.remove_id_internal(holder, id)?;
            }
        }

        // 3. Destroy every entity in `to_despawn`.
        for &dead in to_despawn.as_slice() {
            let Ok((tid, row)) = self.entity_table(dead) else {
                continue;
            };
            // Snapshot archetype IDs to emit `OnRemove` observers cleanly.
            let mut removed_ids = OwnVec::new_in(self.alloc);
            if let Ok(tbl) = self.table(tid) {
                for &id in tbl.archetype() {
                    removed_ids.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
            for &id in removed_ids.as_slice() {
                self.emit_event(EventKind::OnRemove, dead, id)?;
            }

            let moved = {
                let tbl = self.table_mut(tid)?;
                tbl.swap_remove_drop_row(row)?
            };
            if let Some(moved_entity) = moved {
                self.update_moved_row(moved_entity, row);
            }

            let slot_idx = dead.slot();
            if let Some(slot) = self.slots.get_mut(u32_to_usize(slot_idx)) {
                if let Some(name) = slot.name.take() {
                    let _ = self.names.remove(name.as_str());
                }
                slot.alive = false;
                let next_gen = slot.generation.wrapping_add(1);
                slot.generation = if next_gen == 0 { 1 } else { next_gen };
                slot.table_id = TableId(0);
                slot.row = 0;
            }
            self.free_slots.try_push(slot_idx).map_err(|r| EcsError::Alloc(r.why))?;
            self.live_count = self.live_count.saturating_sub(1);
        }

        Ok(())
    }

    #[inline]
    fn update_moved_row(&mut self, moved_entity: Entity, new_row: usize) {
        #[cfg(test)]
        if self.plant_skip_swap_row_update {
            return;
        }
        if let Some(slot) = self.slots.get_mut(u32_to_usize(moved_entity.slot())) {
            slot.row = new_row;
        }
    }

    // -------------------------------------------------------------------------
    // Component Read / Write / Remove
    // -------------------------------------------------------------------------

    /// Set (insert or overwrite) component `T` on `entity`, lazily registering `T` under
    /// [`short_type_name::<T>()`] on first use.
    pub fn set<T: 'static>(&mut self, entity: Entity, value: T) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let cid = self.component::<T>()?;
        self.set_by_id(entity, cid, value)
    }

    /// Set (insert or overwrite) `Copy` component `T` on `entity`, lazily registering `T` as
    /// copyable under [`short_type_name::<T>()`] so prefab instantiation duplicates it
    /// automatically.
    pub fn set_copy<T: Copy + 'static>(
        &mut self,
        entity: Entity,
        value: T,
    ) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let cid = self.component_copy::<T>()?;
        self.set_by_id(entity, cid, value)
    }

    /// Set (insert or overwrite) reflected component `T` on `entity`, lazily registering `T` with
    /// live reflection enabled.
    pub fn set_reflected<T: ReflectComponent + 'static>(
        &mut self,
        entity: Entity,
        value: T,
    ) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let cid = self.component_reflected::<T>()?;
        self.set_by_id(entity, cid, value)
    }

    /// Set (insert or overwrite) reflected `Copy` component `T` on `entity`, lazily registering `T`
    /// with both copyability and live reflection enabled.
    pub fn set_reflected_copy<T: ReflectComponent + Copy + 'static>(
        &mut self,
        entity: Entity,
        value: T,
    ) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let cid = self.component_reflected_copy::<T>()?;
        self.set_by_id(entity, cid, value)
    }

    /// Set (insert or overwrite) component `T` on `entity` using its pre-fetched [`ComponentId`].
    pub fn set_by_id<T: 'static>(
        &mut self,
        entity: Entity,
        cid: ComponentId,
        value: T,
    ) -> Result<(), EcsError> {
        let (src_tid, src_row) = self.entity_table(entity)?;
        let meta = self.components.get(u32_to_usize(cid.0)).ok_or(EcsError::UnknownComponent(cid))?;
        if meta.type_id != TypeId::of::<T>() {
            return Err(EcsError::ComponentTypeMismatch { component: cid });
        }
        let id = Id::Component(cid);
        let already_has = self.table(src_tid)?.has_id(id);
        if already_has {
            self.table_mut(src_tid)?.set_row(cid, src_row, value)?;
            self.emit_event(EventKind::OnSet, entity, id)?;
            return Ok(());
        }

        let dst_tid = self.get_or_create_add_edge(src_tid, id)?;
        let (dst_row, moved_in_src) = self.move_entity_tables(src_tid, src_row, dst_tid)?;
        self.table_mut(dst_tid)?.push_component(cid, value)?;

        if let Some(slot) = self.slots.get_mut(u32_to_usize(entity.slot())) {
            slot.table_id = dst_tid;
            slot.row = dst_row;
        }
        if let Some(moved) = moved_in_src {
            self.update_moved_row(moved, src_row);
        }

        self.emit_event(EventKind::OnAdd, entity, id)?;
        self.emit_event(EventKind::OnSet, entity, id)?;
        Ok(())
    }

    /// Borrow component `T` directly from `entity` (does not walk `IsA` prefabs).
    pub fn get<T: 'static>(&self, entity: Entity) -> Result<Option<&T>, EcsError> {
        let (tid, row) = self.entity_table(entity)?;
        let Some(cid) = self.component_id::<T>() else {
            return Ok(None);
        };
        let table = self.table(tid)?;
        if !table.has_id(Id::Component(cid)) {
            return Ok(None);
        }
        Ok(Some(table.get_row::<T>(cid, row)?))
    }

    /// Mutably borrow component `T` directly from `entity`.
    pub fn get_mut<T: 'static>(&mut self, entity: Entity) -> Result<Option<&mut T>, EcsError> {
        let (tid, row) = self.entity_table(entity)?;
        let Some(cid) = self.component_id::<T>() else {
            return Ok(None);
        };
        let table = self.table_mut(tid)?;
        if !table.has_id(Id::Component(cid)) {
            return Ok(None);
        }
        Ok(Some(table.get_row_mut::<T>(cid, row)?))
    }

    /// Borrow component `T` from `entity`, walking its `(IsA, prefab)` inheritance chain
    /// iteratively (nearest ancestor wins; per-entity values override prefabs).
    pub fn get_inherited<T: 'static>(&self, entity: Entity) -> Result<Option<&T>, EcsError> {
        let _ = self.live_slot(entity)?;
        let Some(cid) = self.component_id::<T>() else {
            return Ok(None);
        };
        let step_cap = self.slots.len().saturating_add(1);
        let mut queue = OwnVec::new_in(self.alloc);
        let mut visited: FlatSet<Entity, A> = FlatSet::new_in(self.alloc);
        queue.try_push(entity).map_err(|r| EcsError::Alloc(r.why))?;
        let _ = visited.insert(entity)?;

        let mut head = 0usize;
        let mut steps = 0usize;
        while let Some(&cur) = queue.get(head) {
            head += 1;
            steps += 1;
            if steps > step_cap {
                return Err(EcsError::TraversalBoundExceeded { limit: step_cap });
            }
            let Ok(slot) = self.live_slot(cur) else {
                continue;
            };
            let table = self.table(slot.table_id)?;
            if table.has_id(Id::Component(cid)) {
                return Ok(Some(table.get_row::<T>(cid, slot.row)?));
            }
            for &id in table.archetype() {
                if let Id::Pair(rel, target) = id {
                    if rel == self.builtins.is_a && !visited.contains(&target) {
                        let _ = visited.insert(target)?;
                        queue.try_push(target).map_err(|r| EcsError::Alloc(r.why))?;
                    }
                }
            }
        }
        Ok(None)
    }

    /// Check whether `entity` directly owns component `T`.
    pub fn has<T: 'static>(&self, entity: Entity) -> Result<bool, EcsError> {
        let (tid, _) = self.entity_table(entity)?;
        let Some(cid) = self.component_id::<T>() else {
            return Ok(false);
        };
        Ok(self.table(tid)?.has_id(Id::Component(cid)))
    }

    /// Remove component `T` from `entity`. Returns `Ok(true)` if removed, `Ok(false)` if absent.
    pub fn remove<T: 'static>(&mut self, entity: Entity) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let Some(cid) = self.component_id::<T>() else {
            return Ok(false);
        };
        self.remove_id_internal(entity, Id::Component(cid))
    }

    /// Remove component `cid` from `entity`.
    pub fn remove_by_id(&mut self, entity: Entity, cid: ComponentId) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        self.remove_id_internal(entity, Id::Component(cid))
    }

    // -------------------------------------------------------------------------
    // Tags & Relations (Pairs)
    // -------------------------------------------------------------------------

    /// Attach zero-sized tag `tag` to `entity`.
    pub fn add_tag(&mut self, entity: Entity, tag: Entity) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let _ = self.live_slot(tag)?;
        if tag == self.builtins.wildcard {
            return Err(EcsError::InvalidWildcardUse);
        }
        let _ = self.add_id_no_data(entity, Id::Tag(tag))?;
        Ok(())
    }

    /// Remove tag `tag` from `entity`.
    pub fn remove_tag(&mut self, entity: Entity, tag: Entity) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        self.remove_id_internal(entity, Id::Tag(tag))
    }

    /// Return whether `entity` has `tag`.
    pub fn has_tag(&self, entity: Entity, tag: Entity) -> Result<bool, EcsError> {
        let (tid, _) = self.entity_table(entity)?;
        Ok(self.table(tid)?.has_id(Id::Tag(tag)))
    }

    #[inline]
    fn entity_has_tag_raw(&self, entity: Entity, tag: Entity) -> bool {
        self.live_slot(entity)
            .ok()
            .and_then(|slot| self.table(slot.table_id).ok())
            .is_some_and(|tbl| tbl.has_id(Id::Tag(tag)))
    }

    /// Attach pair `(rel, target)` to `entity`.
    ///
    /// Enforces:
    /// - Hierarchy/inheritance cycle rejection for `ChildOf` and `IsA`.
    /// - `Exclusive` relation replacement (removes any previous `(rel, old_target)`).
    /// - `Symmetric` relation mirroring (also attaches `(rel, entity)` on `target`).
    pub fn add_pair(
        &mut self,
        entity: Entity,
        rel: Entity,
        target: Entity,
    ) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let _ = self.live_slot(rel)?;
        let _ = self.live_slot(target)?;
        if rel == self.builtins.wildcard || target == self.builtins.wildcard {
            return Err(EcsError::InvalidWildcardUse);
        }

        // Cycle rejection for `ChildOf` and `IsA` (iterative BFS).
        if rel == self.builtins.child_of || rel == self.builtins.is_a {
            if entity == target || self.can_reach_via_relation(target, rel, entity)? {
                return Err(EcsError::HierarchyCycle { entity, relation: rel, target });
            }
        }

        // Exclusive relation handling: replace existing target if different.
        if self.entity_has_tag_raw(rel, self.builtins.exclusive) {
            let mut old_targets = OwnVec::new_in(self.alloc);
            let (tid, _) = self.entity_table(entity)?;
            let table = self.table(tid)?;
            for &id in table.archetype() {
                if let Id::Pair(r, existing_t) = id {
                    if r == rel {
                        if existing_t == target {
                            return Ok(());
                        }
                        old_targets.try_push(existing_t).map_err(|r| EcsError::Alloc(r.why))?;
                    }
                }
            }
            for &old_t in old_targets.as_slice() {
                let _ = self.remove_pair(entity, rel, old_t)?;
            }
        }

        let _ = self.add_id_no_data(entity, Id::Pair(rel, target))?;

        // Symmetric relation mirroring (non-recursive).
        if self.entity_has_tag_raw(rel, self.builtins.symmetric) && entity != target {
            let _ = self.add_id_no_data(target, Id::Pair(rel, entity))?;
        }

        Ok(())
    }

    /// Remove pair `(rel, target)` from `entity` (and mirrors removal for `Symmetric` relations).
    pub fn remove_pair(
        &mut self,
        entity: Entity,
        rel: Entity,
        target: Entity,
    ) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let removed = self.remove_id_internal(entity, Id::Pair(rel, target))?;
        if self.entity_has_tag_raw(rel, self.builtins.symmetric)
            && entity != target
            && self.is_alive(target)
        {
            let _ = self.remove_id_internal(target, Id::Pair(rel, entity))?;
        }
        Ok(removed)
    }

    /// Check whether `entity` directly has pair `(rel, target)`.
    pub fn has_pair(
        &self,
        entity: Entity,
        rel: Entity,
        target: Entity,
    ) -> Result<bool, EcsError> {
        let (tid, _) = self.entity_table(entity)?;
        Ok(self.table(tid)?.has_id(Id::Pair(rel, target)))
    }

    /// Check whether `(entity, rel, target)` holds directly or transitively (when `rel` carries
    /// the `Transitive` trait tag). 100% iterative BFS with visited set.
    pub fn has_transitive(
        &self,
        entity: Entity,
        rel: Entity,
        target: Entity,
    ) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let _ = self.live_slot(rel)?;
        let _ = self.live_slot(target)?;
        if self.has_pair(entity, rel, target)? {
            return Ok(true);
        }
        if !self.entity_has_tag_raw(rel, self.builtins.transitive) {
            return Ok(false);
        }
        self.can_reach_via_relation(entity, rel, target)
    }

    fn can_reach_via_relation(
        &self,
        start: Entity,
        rel: Entity,
        goal: Entity,
    ) -> Result<bool, EcsError> {
        let step_cap = self.slots.len().saturating_add(1);
        let mut queue = OwnVec::new_in(self.alloc);
        let mut visited: FlatSet<Entity, A> = FlatSet::new_in(self.alloc);
        queue.try_push(start).map_err(|r| EcsError::Alloc(r.why))?;
        let _ = visited.insert(start)?;

        let mut head = 0usize;
        let mut steps = 0usize;
        while let Some(&cur) = queue.get(head) {
            head += 1;
            steps += 1;
            if steps > step_cap {
                return Err(EcsError::TraversalBoundExceeded { limit: step_cap });
            }
            let Ok(slot) = self.live_slot(cur) else {
                continue;
            };
            let table = self.table(slot.table_id)?;
            for &id in table.archetype() {
                if let Id::Pair(r, next) = id {
                    if r == rel {
                        if next == goal {
                            return Ok(true);
                        }
                        if !visited.contains(&next) {
                            let _ = visited.insert(next)?;
                            queue.try_push(next).map_err(|r| EcsError::Alloc(r.why))?;
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// Return all targets `t` such that `entity` has pair `(rel, t)`.
    pub fn targets_of(&self, entity: Entity, rel: Entity) -> Result<OwnVec<Entity, A>, EcsError> {
        let (tid, _) = self.entity_table(entity)?;
        let table = self.table(tid)?;
        let mut out = OwnVec::new_in(self.alloc);
        for &id in table.archetype() {
            if let Id::Pair(r, t) = id {
                if r == rel {
                    out.try_push(t).map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
        }
        Ok(out)
    }

    /// Return the exclusive `ChildOf` parent of `child`, if any.
    pub fn parent_of(&self, child: Entity) -> Result<Option<Entity>, EcsError> {
        let (tid, _) = self.entity_table(child)?;
        let table = self.table(tid)?;
        for &id in table.archetype() {
            if let Id::Pair(r, parent) = id {
                if r == self.builtins.child_of {
                    return Ok(Some(parent));
                }
            }
        }
        Ok(None)
    }

    /// Return all live entities with `(ChildOf, parent)` in deterministic order.
    pub fn children_of(&self, parent: Entity) -> Result<OwnVec<Entity, A>, EcsError> {
        let _ = self.live_slot(parent)?;
        let pair = Id::Pair(self.builtins.child_of, parent);
        let mut out = OwnVec::new_in(self.alloc);
        for table in self.tables.as_slice() {
            if table.has_id(pair) {
                for &child in table.entities() {
                    out.try_push(child).map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
        }
        out.as_mut_slice().sort_unstable();
        Ok(out)
    }

    // -------------------------------------------------------------------------
    // Typed Tags, Relations & Prefabs (Zero-String Ergonomic API)
    // -------------------------------------------------------------------------

    /// Return the cached entity for typed tag `T` without creating one if absent.
    #[inline]
    pub fn tag_entity_id<T: 'static>(&self) -> Option<Entity> {
        let ent = self.tag_by_type.get(&TypeId::of::<T>()).copied()?;
        if self.is_alive(ent) { Some(ent) } else { None }
    }

    /// Return (or lazily spawn and cache) the named tag entity for marker type `T`.
    pub fn tag_entity<T: 'static>(&mut self) -> Result<Entity, EcsError> {
        let tid = TypeId::of::<T>();
        if let Some(&existing) = self.tag_by_type.get(&tid) {
            if self.is_alive(existing) {
                return Ok(existing);
            }
        }
        let ent = self.spawn_typed_entity(short_type_name::<T>(), core::any::type_name::<T>())?;
        let _ = self.tag_by_type.insert(tid, ent)?;
        Ok(ent)
    }

    /// Attach typed marker tag `T` to `entity`.
    pub fn add_tag_typed<T: 'static>(&mut self, entity: Entity) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let tag = self.tag_entity::<T>()?;
        self.add_tag(entity, tag)
    }

    /// Check whether `entity` carries typed marker tag `T`.
    pub fn has_tag_typed<T: 'static>(&self, entity: Entity) -> Result<bool, EcsError> {
        let (tid, _) = self.entity_table(entity)?;
        let Some(tag) = self.tag_entity_id::<T>() else {
            return Ok(false);
        };
        Ok(self.table(tid)?.has_id(Id::Tag(tag)))
    }

    /// Remove typed marker tag `T` from `entity`.
    pub fn remove_tag_typed<T: 'static>(&mut self, entity: Entity) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let Some(tag) = self.tag_entity_id::<T>() else {
            return Ok(false);
        };
        self.remove_tag(entity, tag)
    }

    /// Return the cached entity for typed relation `R` without creating one if absent.
    #[inline]
    pub fn relation_entity_id<R: Relation>(&self) -> Option<Entity> {
        let tid = TypeId::of::<R>();
        if tid == TypeId::of::<ChildOf>() {
            return Some(self.builtins.child_of);
        }
        if tid == TypeId::of::<IsA>() {
            return Some(self.builtins.is_a);
        }
        let ent = self.relation_by_type.get(&tid).copied()?;
        if self.is_alive(ent) { Some(ent) } else { None }
    }

    /// Return (or lazily spawn, configure trait tags, and cache) the entity for typed [`Relation`]
    /// `R`. Built-in [`ChildOf`] and [`IsA`] map directly to `self.child_of()` / `self.is_a()`.
    pub fn relation_entity<R: Relation>(&mut self) -> Result<Entity, EcsError> {
        let tid = TypeId::of::<R>();
        if tid == TypeId::of::<ChildOf>() {
            return Ok(self.builtins.child_of);
        }
        if tid == TypeId::of::<IsA>() {
            return Ok(self.builtins.is_a);
        }
        if let Some(&existing) = self.relation_by_type.get(&tid) {
            if self.is_alive(existing) {
                return Ok(existing);
            }
        }
        let rel = self.spawn_typed_entity(short_type_name::<R>(), core::any::type_name::<R>())?;
        if R::EXCLUSIVE {
            self.add_tag(rel, self.builtins.exclusive)?;
        }
        if R::SYMMETRIC {
            self.add_tag(rel, self.builtins.symmetric)?;
        }
        if R::TRANSITIVE {
            self.add_tag(rel, self.builtins.transitive)?;
        }
        if R::ON_DELETE_CASCADE {
            self.add_tag(rel, self.builtins.on_delete_cascade)?;
        }
        let _ = self.relation_by_type.insert(tid, rel)?;
        Ok(rel)
    }

    /// Attach typed relation pair `(R, target)` to `entity`.
    pub fn relate<R: Relation>(&mut self, entity: Entity, target: Entity) -> Result<(), EcsError> {
        let _ = self.live_slot(entity)?;
        let _ = self.live_slot(target)?;
        let rel = self.relation_entity::<R>()?;
        self.add_pair(entity, rel, target)
    }

    /// Remove typed relation pair `(R, target)` from `entity`.
    pub fn unrelate<R: Relation>(
        &mut self,
        entity: Entity,
        target: Entity,
    ) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let Some(rel) = self.relation_entity_id::<R>() else {
            return Ok(false);
        };
        self.remove_pair(entity, rel, target)
    }

    /// Check whether `entity` directly carries typed relation pair `(R, target)`.
    pub fn has_relation<R: Relation>(
        &self,
        entity: Entity,
        target: Entity,
    ) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let Some(rel) = self.relation_entity_id::<R>() else {
            return Ok(false);
        };
        self.has_pair(entity, rel, target)
    }

    /// Check whether `(entity, R, target)` holds directly or transitively (when `R::TRANSITIVE`).
    pub fn has_transitive_relation<R: Relation>(
        &self,
        entity: Entity,
        target: Entity,
    ) -> Result<bool, EcsError> {
        let _ = self.live_slot(entity)?;
        let _ = self.live_slot(target)?;
        let Some(rel) = self.relation_entity_id::<R>() else {
            return Ok(false);
        };
        self.has_transitive(entity, rel, target)
    }

    /// Return the cached entity for typed prefab `P` without creating one if absent.
    #[inline]
    pub fn prefab_entity_id<P: Prefab>(&self) -> Option<Entity> {
        let ent = self.prefab_by_type.get(&TypeId::of::<P>()).copied()?;
        if self.is_alive(ent) { Some(ent) } else { None }
    }

    /// Return (or lazily create iteratively from root ancestor to leaf) the cached prefab entity
    /// for typed [`Prefab`] `P`.
    pub fn prefab_entity<P: Prefab>(&mut self) -> Result<Entity, EcsError> {
        let leaf_tid = TypeId::of::<P>();
        if let Some(&existing) = self.prefab_by_type.get(&leaf_tid) {
            if self.is_alive(existing) {
                return Ok(existing);
            }
        }

        // Collect unmaterialized ancestor chain iteratively (leaf -> root), zero recursion.
        let mut chain: OwnVec<PrefabInit<A>, A> = OwnVec::new_in(self.alloc);
        let mut cur = prefab_init_for::<P, A>();
        let mut prev_parent: Option<Entity> = None;

        loop {
            if chain.len() >= MAX_PREFAB_DEPTH {
                return Err(EcsError::TraversalBoundExceeded { limit: MAX_PREFAB_DEPTH });
            }
            let next_parent_fn = cur.parent;
            chain.try_push(cur).map_err(|r| EcsError::Alloc(r.why))?;
            let Some(parent_fn) = next_parent_fn else {
                break;
            };
            let next = parent_fn();
            if let Some(&cached_parent) = self.prefab_by_type.get(&next.type_id) {
                if self.is_alive(cached_parent) {
                    prev_parent = Some(cached_parent);
                    break;
                }
            }
            cur = next;
        }

        // Materialize prefabs root-to-leaf, linking `(IsA, parent)` and inserting copyable bundles.
        let mut result = Entity::NULL;
        while let Some(step) = chain.pop() {
            let ent = self.spawn_typed_entity(step.short_name, step.full_name)?;
            self.add_tag(ent, self.builtins.prefab)?;
            if let Some(parent_ent) = prev_parent {
                self.add_pair(ent, self.builtins.is_a, parent_ent)?;
            }
            (step.insert)(self, ent)?;
            let _ = self.prefab_by_type.insert(step.type_id, ent)?;
            prev_parent = Some(ent);
            result = ent;
        }

        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Typed 1-Line Queries & Filtered Query Builder
    // -------------------------------------------------------------------------

    /// Start a chainable filtered typed query for `Q` (`.with_tag`, `.without`, `.with_relation`,
    /// `.include_prefabs`, `.each`, `.each_chunk`, `.each_with_entity`).
    #[inline]
    pub fn query<Q: QueryFetch>(&mut self) -> TypedQuery<'_, Q, A> {
        TypedQuery::new(self)
    }

    /// Iterate all live non-prefab entities matching `Q` with zero heap allocation.
    pub fn each<Q: QueryFetch>(
        &mut self,
        mut f: impl for<'a> FnMut(Q::Item<'a>),
    ) -> Result<(), EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(());
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        for table in self.tables.as_mut_slice() {
            if table.is_empty() || table.has_id(Id::Tag(prefab_tag)) {
                continue;
            }
            if !table_has_all_components(table, ids) {
                continue;
            }
            Q::for_each_in_table(table, ids, &mut f)?;
        }
        Ok(())
    }

    /// Iterate contiguous SoA column slices for every non-empty, non-prefab archetype table
    /// matching `Q` with zero heap allocation.
    pub fn each_chunk<Q: QueryFetch>(
        &mut self,
        mut f: impl for<'a> FnMut(Q::Chunk<'a>),
    ) -> Result<(), EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(());
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        for table in self.tables.as_mut_slice() {
            if table.is_empty() || table.has_id(Id::Tag(prefab_tag)) {
                continue;
            }
            if !table_has_all_components(table, ids) {
                continue;
            }
            Q::for_each_chunk_in_table(table, ids, &mut f)?;
        }
        Ok(())
    }

    /// Iterate all live non-prefab entities matching `Q`, passing `(Entity, Q::Item<'_>)`, with
    /// zero heap allocation.
    pub fn each_with_entity<Q: QueryFetch>(
        &mut self,
        mut f: impl for<'a> FnMut(Entity, Q::Item<'a>),
    ) -> Result<(), EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(());
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        for table in self.tables.as_mut_slice() {
            if table.is_empty() || table.has_id(Id::Tag(prefab_tag)) {
                continue;
            }
            if !table_has_all_components(table, ids) {
                continue;
            }
            Q::for_each_with_entity_in_table(table, ids, &mut f)?;
        }
        Ok(())
    }

    /// Iterate all entities matching both `Q` and additional filter specification `extra`.
    pub fn each_filtered<Q: QueryFetch>(
        &mut self,
        extra: &QuerySpec<A>,
        mut f: impl for<'a> FnMut(Q::Item<'a>),
    ) -> Result<(), EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(());
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        let wildcard = self.builtins.wildcard;
        for table in self.tables.as_mut_slice() {
            if table.is_empty() || !table_has_all_components(table, ids) {
                continue;
            }
            if !extra.matches_table(table, prefab_tag, wildcard) {
                continue;
            }
            Q::for_each_in_table(table, ids, &mut f)?;
        }
        Ok(())
    }

    /// Iterate contiguous SoA chunks for all non-empty tables matching both `Q` and `extra`.
    pub fn each_chunk_filtered<Q: QueryFetch>(
        &mut self,
        extra: &QuerySpec<A>,
        mut f: impl for<'a> FnMut(Q::Chunk<'a>),
    ) -> Result<(), EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(());
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        let wildcard = self.builtins.wildcard;
        for table in self.tables.as_mut_slice() {
            if table.is_empty() || !table_has_all_components(table, ids) {
                continue;
            }
            if !extra.matches_table(table, prefab_tag, wildcard) {
                continue;
            }
            Q::for_each_chunk_in_table(table, ids, &mut f)?;
        }
        Ok(())
    }

    /// Iterate `(Entity, Q::Item<'_>)` for all entities matching both `Q` and `extra`.
    pub fn each_with_entity_filtered<Q: QueryFetch>(
        &mut self,
        extra: &QuerySpec<A>,
        mut f: impl for<'a> FnMut(Entity, Q::Item<'a>),
    ) -> Result<(), EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(());
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        let wildcard = self.builtins.wildcard;
        for table in self.tables.as_mut_slice() {
            if table.is_empty() || !table_has_all_components(table, ids) {
                continue;
            }
            if !extra.matches_table(table, prefab_tag, wildcard) {
                continue;
            }
            Q::for_each_with_entity_in_table(table, ids, &mut f)?;
        }
        Ok(())
    }

    /// Count all entities matching both `Q` and `extra`.
    pub fn count_filtered<Q: QueryFetch>(&self, extra: &QuerySpec<A>) -> Result<usize, EcsError> {
        Q::check_conflicts(self)?;
        let Some(fetch_ids) = Q::lookup_ids(self) else {
            return Ok(0);
        };
        let ids = fetch_ids.as_slice();
        let prefab_tag = self.builtins.prefab;
        let wildcard = self.builtins.wildcard;
        let mut total = 0usize;
        for table in self.tables.as_slice() {
            if table.is_empty() || !table_has_all_components(table, ids) {
                continue;
            }
            if !extra.matches_table(table, prefab_tag, wildcard) {
                continue;
            }
            total = total.saturating_add(table.len());
        }
        Ok(total)
    }

    // -------------------------------------------------------------------------
    // Queries, Cached Queries & Cascade Traversal
    // -------------------------------------------------------------------------

    /// Sync a [`CachedQuery`] against newly created tables and return its matched [`TableId`]
    /// slice with zero heap allocation in steady state.
    ///
    /// Because the returned slice borrows only `cached` (and not `self`), callers may immediately
    /// borrow tables mutably via `world.table_mut(tid)` inside the iteration loop.
    pub fn query_tables<'q>(
        &self,
        cached: &'q mut CachedQuery<A>,
    ) -> Result<&'q [TableId], EcsError> {
        let _ = cached.sync(
            self.tables.as_slice(),
            self.builtins.prefab,
            self.builtins.wildcard,
        )?;
        Ok(cached.matched_table_ids())
    }

    /// Sync a [`CachedQuery`] and return the number of newly scanned tables since the last sync.
    pub fn sync_query(&self, cached: &mut CachedQuery<A>) -> Result<usize, EcsError> {
        cached.sync(self.tables.as_slice(), self.builtins.prefab, self.builtins.wildcard)
    }

    /// Evaluate `spec` and return all matched entities.
    pub fn query_entities(&self, spec: &QuerySpec<A>) -> Result<OwnVec<Entity, A>, EcsError> {
        let mut out = OwnVec::new_in(self.alloc);
        for table in self.tables.as_slice() {
            if table.is_empty() {
                continue;
            }
            if spec.matches_table(table, self.builtins.prefab, self.builtins.wildcard) {
                for &entity in table.entities() {
                    out.try_push(entity).map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
        }
        Ok(out)
    }

    /// Evaluate `spec` and return matched entities sorted topologically parent-before-child
    /// (by iterative depth along `spec.cascade_relation().unwrap_or(self.child_of())`).
    pub fn query_cascade(&self, spec: &QuerySpec<A>) -> Result<OwnVec<Entity, A>, EcsError> {
        let rel = spec.cascade_relation().unwrap_or(self.builtins.child_of);
        let matched = self.query_entities(spec)?;
        let mut keyed: OwnVec<(usize, Entity), A> =
            OwnVec::try_with_capacity_in(matched.len(), self.alloc)?;
        for &entity in matched.as_slice() {
            let depth = self.hierarchy_depth(entity, rel)?;
            keyed.try_push((depth, entity)).map_err(|r| EcsError::Alloc(r.why))?;
        }
        keyed.as_mut_slice().sort_unstable_by_key(|&(depth, e)| (depth, e.raw()));
        let mut out = OwnVec::try_with_capacity_in(keyed.len(), self.alloc)?;
        for &(_, e) in keyed.as_slice() {
            out.try_push(e).map_err(|r| EcsError::Alloc(r.why))?;
        }
        Ok(out)
    }

    fn hierarchy_depth(&self, entity: Entity, rel: Entity) -> Result<usize, EcsError> {
        let step_cap = self.slots.len().saturating_add(1);
        let mut cur = entity;
        let mut depth = 0usize;
        while depth < step_cap {
            let Ok(slot) = self.live_slot(cur) else {
                break;
            };
            let table = self.table(slot.table_id)?;
            let mut next_parent = None;
            for &id in table.archetype() {
                if let Id::Pair(r, parent) = id {
                    if r == rel {
                        next_parent = Some(parent);
                        break;
                    }
                }
            }
            match next_parent {
                Some(p) if p != cur => {
                    cur = p;
                    depth += 1;
                }
                _ => break,
            }
        }
        if depth >= step_cap {
            return Err(EcsError::TraversalBoundExceeded { limit: step_cap });
        }
        Ok(depth)
    }

    // -------------------------------------------------------------------------
    // Reactive Observers
    // -------------------------------------------------------------------------

    /// Register a logging observer for `(kind, filter)` with default bounded capacity.
    /// `filter` may be a component, tag, exact pair, or wildcard pair `(rel, WILDCARD)` /
    /// `(WILDCARD, target)`.
    pub fn observe(&mut self, kind: EventKind, filter: Id) -> Result<ObserverId, EcsError> {
        self.observe_inner(kind, filter, DEFAULT_OBSERVER_CAP, None)
    }

    /// Register a logging observer with an explicit event buffer cap.
    pub fn observe_with_cap(
        &mut self,
        kind: EventKind,
        filter: Id,
        max_events: usize,
    ) -> Result<ObserverId, EcsError> {
        self.observe_inner(kind, filter, max_events, None)
    }

    /// Register an observer with both bounded logging and a synchronous non-reentrant callback.
    pub fn observe_with_callback(
        &mut self,
        kind: EventKind,
        filter: Id,
        callback: ObserverCallback<A>,
    ) -> Result<ObserverId, EcsError> {
        self.observe_inner(kind, filter, DEFAULT_OBSERVER_CAP, Some(callback))
    }

    fn observe_inner(
        &mut self,
        kind: EventKind,
        filter: Id,
        max_events: usize,
        callback: Option<ObserverCallback<A>>,
    ) -> Result<ObserverId, EcsError> {
        let raw = usize_to_u32(self.observers.len()).ok_or(EcsError::EntityCapacityExceeded)?;
        let id = ObserverId(raw);
        let obs = Observer::new_in(kind, filter, max_events, callback, self.alloc);
        self.observers.try_push(obs).map_err(|r| EcsError::Alloc(r.why))?;
        Ok(id)
    }

    /// Recorded events for observer `id`.
    #[inline]
    pub fn observer_events(&self, id: ObserverId) -> Option<&[Event]> {
        self.observers.get(u32_to_usize(id.0)).map(|obs| obs.records.as_slice())
    }

    /// Number of events dropped because observer `id` hit its bounded capacity.
    #[inline]
    pub fn observer_dropped_count(&self, id: ObserverId) -> Option<usize> {
        self.observers.get(u32_to_usize(id.0)).map(|obs| obs.dropped_events)
    }

    /// Clear recorded events for observer `id`.
    #[inline]
    pub fn clear_observer_events(&mut self, id: ObserverId) {
        if let Some(obs) = self.observers.get_mut(u32_to_usize(id.0)) {
            obs.records.clear();
            obs.dropped_events = 0;
        }
    }

    fn emit_event(&mut self, kind: EventKind, entity: Entity, id: Id) -> Result<(), EcsError> {
        let wildcard = self.builtins.wildcard;
        let event = Event { kind, entity, id };
        for obs in self.observers.as_mut_slice() {
            obs.dispatch(event, wildcard)?;
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Internal Table Graph Transitions
    // -------------------------------------------------------------------------

    fn add_id_no_data(&mut self, entity: Entity, id: Id) -> Result<bool, EcsError> {
        let (src_tid, src_row) = self.entity_table(entity)?;
        if self.table(src_tid)?.has_id(id) {
            return Ok(false);
        }
        let dst_tid = self.get_or_create_add_edge(src_tid, id)?;
        let (dst_row, moved_in_src) = self.move_entity_tables(src_tid, src_row, dst_tid)?;
        if let Some(slot) = self.slots.get_mut(u32_to_usize(entity.slot())) {
            slot.table_id = dst_tid;
            slot.row = dst_row;
        }
        if let Some(moved) = moved_in_src {
            self.update_moved_row(moved, src_row);
        }
        self.emit_event(EventKind::OnAdd, entity, id)?;
        Ok(true)
    }

    fn remove_id_internal(&mut self, entity: Entity, id: Id) -> Result<bool, EcsError> {
        let (src_tid, src_row) = self.entity_table(entity)?;
        if !self.table(src_tid)?.has_id(id) {
            return Ok(false);
        }
        let dst_tid = self.get_or_create_remove_edge(src_tid, id)?;
        let (dst_row, moved_in_src) = self.move_entity_tables(src_tid, src_row, dst_tid)?;
        if let Some(slot) = self.slots.get_mut(u32_to_usize(entity.slot())) {
            slot.table_id = dst_tid;
            slot.row = dst_row;
        }
        if let Some(moved) = moved_in_src {
            self.update_moved_row(moved, src_row);
        }
        self.emit_event(EventKind::OnRemove, entity, id)?;
        Ok(true)
    }

    fn get_or_create_add_edge(&mut self, src_tid: TableId, id: Id) -> Result<TableId, EcsError> {
        if let Some(&cached) = self.table(src_tid)?.add_edges.get(&id) {
            return Ok(cached);
        }
        let new_arch = {
            let src_arch = self.table(src_tid)?.archetype();
            let mut next = OwnVec::try_with_capacity_in(src_arch.len().saturating_add(1), self.alloc)?;
            let mut inserted = false;
            for &existing in src_arch {
                if !inserted && id < existing {
                    next.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
                    inserted = true;
                }
                next.try_push(existing).map_err(|r| EcsError::Alloc(r.why))?;
            }
            if !inserted {
                next.try_push(id).map_err(|r| EcsError::Alloc(r.why))?;
            }
            next
        };
        let dst_tid = self.find_or_create_table(new_arch)?;
        let _ = self.table_mut(src_tid)?.add_edges.insert(id, dst_tid)?;
        let _ = self.table_mut(dst_tid)?.remove_edges.insert(id, src_tid)?;
        Ok(dst_tid)
    }

    fn get_or_create_remove_edge(
        &mut self,
        src_tid: TableId,
        id: Id,
    ) -> Result<TableId, EcsError> {
        if let Some(&cached) = self.table(src_tid)?.remove_edges.get(&id) {
            return Ok(cached);
        }
        let new_arch = {
            let src_arch = self.table(src_tid)?.archetype();
            let mut next = OwnVec::try_with_capacity_in(src_arch.len(), self.alloc)?;
            for &existing in src_arch {
                if existing != id {
                    next.try_push(existing).map_err(|r| EcsError::Alloc(r.why))?;
                }
            }
            next
        };
        let dst_tid = self.find_or_create_table(new_arch)?;
        let _ = self.table_mut(src_tid)?.remove_edges.insert(id, dst_tid)?;
        let _ = self.table_mut(dst_tid)?.add_edges.insert(id, src_tid)?;
        Ok(dst_tid)
    }

    fn find_or_create_table(&mut self, sorted_arch: OwnVec<Id, A>) -> Result<TableId, EcsError> {
        if let Some(&existing) = self.table_by_archetype.get(sorted_arch.as_slice()) {
            return Ok(existing);
        }
        let raw_tid = usize_to_u32(self.tables.len()).ok_or(EcsError::EntityCapacityExceeded)?;
        let tid = TableId(raw_tid);
        let key = ArchetypeKey::from_slice_in(sorted_arch.as_slice(), self.alloc)?;
        let table = Table::new_in(tid, sorted_arch, self.components.as_slice(), self.alloc)?;
        self.tables.try_push(table).map_err(|r| EcsError::Alloc(r.why))?;
        let _ = self.table_by_archetype.insert(key, tid)?;
        Ok(tid)
    }

    fn move_entity_tables(
        &mut self,
        src_tid: TableId,
        src_row: usize,
        dst_tid: TableId,
    ) -> Result<(usize, Option<Entity>), EcsError> {
        if src_tid == dst_tid {
            return Err(EcsError::InvalidTable(dst_tid));
        }
        let src_idx = u32_to_usize(src_tid.0);
        let dst_idx = u32_to_usize(dst_tid.0);
        let slice = self.tables.as_mut_slice();
        if src_idx < dst_idx {
            let (left, right) =
                slice.split_at_mut_checked(dst_idx).ok_or(EcsError::InvalidTable(dst_tid))?;
            let src = left.get_mut(src_idx).ok_or(EcsError::InvalidTable(src_tid))?;
            let dst = right.get_mut(0).ok_or(EcsError::InvalidTable(dst_tid))?;
            move_row_between_tables(src, src_row, dst)
        } else {
            let (left, right) =
                slice.split_at_mut_checked(src_idx).ok_or(EcsError::InvalidTable(src_tid))?;
            let dst = left.get_mut(dst_idx).ok_or(EcsError::InvalidTable(dst_tid))?;
            let src = right.get_mut(0).ok_or(EcsError::InvalidTable(src_tid))?;
            move_row_between_tables(src, src_row, dst)
        }
    }

    // -------------------------------------------------------------------------
    // Live Reflection, Inspection, Patching & World Snapshots (`bstd::data::Value`)
    // -------------------------------------------------------------------------

    /// Inspect component `comp` on live `entity` as a [`Value`].
    ///
    /// Returns `Ok(None)` if `entity` does not possess `comp`, `Err(ReflectError::NotReflected)`
    /// if `entity` owns `comp` but `comp` was registered without reflection, or `Ok(Some(value))`.
    pub fn inspect_component_value(
        &self,
        entity: Entity,
        comp: ComponentId,
        alloc: A,
    ) -> Result<Option<Value<A>>, ReflectError> {
        let slot = self.live_slot(entity).map_err(|_| ReflectError::EntityNotFound(entity))?;
        let meta = self
            .component_meta(comp)
            .ok_or(ReflectError::Ecs(EcsError::UnknownComponent(comp)))?;
        let table = self.table(slot.table_id)?;
        if !table.has_id(Id::Component(comp)) {
            return Ok(None);
        }
        if !meta.reflected {
            return Err(ReflectError::NotReflected(comp));
        }
        table.row_to_value(comp, slot.row, alloc)
    }

    /// Patch an existing reflected component `comp` on `entity` in-place in its SoA column from
    /// `patch` (supporting partial map patches) and fire [`EventKind::OnSet`] observers.
    pub fn patch_component_value(
        &mut self,
        entity: Entity,
        comp: ComponentId,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let slot = self.live_slot(entity).map_err(|_| ReflectError::EntityNotFound(entity))?;
        let tid = slot.table_id;
        let row = slot.row;
        let meta = self
            .component_meta(comp)
            .ok_or(ReflectError::Ecs(EcsError::UnknownComponent(comp)))?;
        if !meta.reflected {
            return Err(ReflectError::NotReflected(comp));
        }
        if !self.table(tid)?.has_id(Id::Component(comp)) {
            return Err(ReflectError::MissingComponent(comp));
        }
        self.table_mut(tid)?.patch_row_from_value(comp, row, patch, alloc)?;
        self.emit_event(EventKind::OnSet, entity, Id::Component(comp))?;
        Ok(())
    }

    /// Patch an existing reflected component identified by registered `comp_name` on `entity`
    /// in-place and fire [`EventKind::OnSet`] observers.
    pub fn patch_component_by_name(
        &mut self,
        entity: Entity,
        comp_name: &str,
        patch: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let _ = self.live_slot(entity).map_err(|_| ReflectError::EntityNotFound(entity))?;
        let cid = self.component_id_by_name(comp_name).ok_or(ReflectError::ComponentNotFound)?;
        self.patch_component_value(entity, cid, patch, alloc)
    }

    /// Attach or update reflected component `comp_name` on `entity` from `val`, firing
    /// [`EventKind::OnAdd`] (if newly added) and [`EventKind::OnSet`] observers.
    ///
    /// Validates `val` before transitioning archetypes so invalid input never leaves the world in
    /// a partially mutated state.
    pub fn set_component_from_value(
        &mut self,
        entity: Entity,
        comp_name: &str,
        val: &Value<A>,
        alloc: A,
    ) -> Result<(), ReflectError> {
        let slot = self.live_slot(entity).map_err(|_| ReflectError::EntityNotFound(entity))?;
        let src_tid = slot.table_id;
        let src_row = slot.row;
        let cid = self.component_id_by_name(comp_name).ok_or(ReflectError::ComponentNotFound)?;
        let meta = self
            .component_meta(cid)
            .ok_or(ReflectError::Ecs(EcsError::UnknownComponent(cid)))?;
        if !meta.reflected {
            return Err(ReflectError::NotReflected(cid));
        }
        let id = Id::Component(cid);
        if self.table(src_tid)?.has_id(id) {
            self.table_mut(src_tid)?.patch_row_from_value(cid, src_row, val, alloc)?;
            self.emit_event(EventKind::OnSet, entity, id)?;
            return Ok(());
        }

        // Validate full decode BEFORE moving the entity to a new archetype table.
        meta.validate_value(val, alloc)?;

        let dst_tid = self.get_or_create_add_edge(src_tid, id)?;
        let (dst_row, moved_in_src) = self.move_entity_tables(src_tid, src_row, dst_tid)?;
        self.table_mut(dst_tid)?.push_component_from_value(cid, val, alloc)?;

        if let Some(s) = self.slots.get_mut(u32_to_usize(entity.slot())) {
            s.table_id = dst_tid;
            s.row = dst_row;
        }
        if let Some(moved) = moved_in_src {
            self.update_moved_row(moved, src_row);
        }

        self.emit_event(EventKind::OnAdd, entity, id)?;
        self.emit_event(EventKind::OnSet, entity, id)?;
        Ok(())
    }

    /// Snapshot a single live entity into a structured [`Value`] containing `id`, `slot`, `gen`,
    /// `table_id`, `name`, `is_prefab`, `parents`, `children`, `prefabs`, `tags`, `relations`,
    /// and `components` (with live reflected values or `"<opaque>"`).
    pub fn entity_to_value(&self, entity: Entity, alloc: A) -> Result<Value<A>, ReflectError> {
        let slot = self.live_slot(entity).map_err(|_| ReflectError::EntityNotFound(entity))?;
        let table = self.table(slot.table_id)?;

        let mut root = Map::new_in(alloc);
        let _ = root.insert(
            OwnString::try_from_str_in("id", alloc)?,
            Value::Int(Int::from_u64(entity.raw())),
        );
        let _ = root.insert(
            OwnString::try_from_str_in("slot", alloc)?,
            Value::Int(Int::from_u64(u64::from(entity.slot()))),
        );
        let _ = root.insert(
            OwnString::try_from_str_in("gen", alloc)?,
            Value::Int(Int::from_u64(u64::from(entity.generation()))),
        );
        let _ = root.insert(
            OwnString::try_from_str_in("table_id", alloc)?,
            Value::Int(Int::from_u64(u64::from(slot.table_id.0))),
        );
        let name_val = match slot.name.as_ref() {
            Some(n) => Value::Text(OwnString::try_from_str_in(n.as_str(), alloc)?),
            None => Value::Null,
        };
        let _ = root.insert(OwnString::try_from_str_in("name", alloc)?, name_val);

        let is_prefab = table.has_id(Id::Tag(self.builtins.prefab));
        let _ = root.insert(
            OwnString::try_from_str_in("is_prefab", alloc)?,
            Value::Bool(is_prefab),
        );

        let mut parents = OwnVec::new_in(alloc);
        let mut prefabs = OwnVec::new_in(alloc);
        let mut tags = OwnVec::new_in(alloc);
        let mut relations = OwnVec::new_in(alloc);
        let mut comps = Map::new_in(alloc);

        for &id in table.archetype() {
            match id {
                Id::Component(cid) => {
                    let comp_name = match self.component_meta(cid) {
                        Some(m) => OwnString::try_from_str_in(m.name(), alloc)?,
                        None => {
                            let mut fallback = OwnString::new_in(alloc);
                            let _ = write!(fallback, "Comp#{}", cid.0);
                            fallback
                        }
                    };
                    let val = match table.row_to_value(cid, slot.row, alloc)? {
                        Some(v) => v,
                        None => Value::Text(OwnString::try_from_str_in("<opaque>", alloc)?),
                    };
                    let _ = comps.insert(comp_name, val);
                }
                Id::Tag(tag_ent) => {
                    let label = self.format_entity_label(tag_ent, alloc)?;
                    tags.try_push(Value::Text(label)).map_err(|r| ReflectError::Alloc(r.why))?;
                }
                Id::Pair(rel, target) => {
                    if rel == self.builtins.child_of {
                        parents
                            .try_push(Value::Int(Int::from_u64(u64::from(target.slot()))))
                            .map_err(|r| ReflectError::Alloc(r.why))?;
                    } else if rel == self.builtins.is_a {
                        prefabs
                            .try_push(Value::Int(Int::from_u64(u64::from(target.slot()))))
                            .map_err(|r| ReflectError::Alloc(r.why))?;
                    }
                    let mut rel_obj = Map::new_in(alloc);
                    let _ = rel_obj.insert(
                        OwnString::try_from_str_in("relation", alloc)?,
                        Value::Text(self.format_entity_label(rel, alloc)?),
                    );
                    let _ = rel_obj.insert(
                        OwnString::try_from_str_in("relation_slot", alloc)?,
                        Value::Int(Int::from_u64(u64::from(rel.slot()))),
                    );
                    let _ = rel_obj.insert(
                        OwnString::try_from_str_in("target", alloc)?,
                        Value::Text(self.format_entity_label(target, alloc)?),
                    );
                    let _ = rel_obj.insert(
                        OwnString::try_from_str_in("target_slot", alloc)?,
                        Value::Int(Int::from_u64(u64::from(target.slot()))),
                    );
                    relations
                        .try_push(Value::Map(rel_obj))
                        .map_err(|r| ReflectError::Alloc(r.why))?;
                }
            }
        }

        let child_entities = self.children_of(entity)?;
        let mut children = OwnVec::try_with_capacity_in(child_entities.len(), alloc)?;
        for &c in child_entities.as_slice() {
            children
                .try_push(Value::Int(Int::from_u64(u64::from(c.slot()))))
                .map_err(|r| ReflectError::Alloc(r.why))?;
        }

        let _ = root.insert(OwnString::try_from_str_in("parents", alloc)?, Value::List(parents));
        let _ = root.insert(OwnString::try_from_str_in("children", alloc)?, Value::List(children));
        let _ = root.insert(OwnString::try_from_str_in("prefabs", alloc)?, Value::List(prefabs));
        let _ = root.insert(OwnString::try_from_str_in("tags", alloc)?, Value::List(tags));
        let _ = root.insert(
            OwnString::try_from_str_in("relations", alloc)?,
            Value::List(relations),
        );
        let _ = root.insert(OwnString::try_from_str_in("components", alloc)?, Value::Map(comps));
        Ok(Value::Map(root))
    }

    /// Snapshot the entire world into a deterministic [`Value`] containing `stats`, `components`
    /// (with `id`, `name`, `size`, `can_copy`, `reflected`, `schema`), `archetypes` (with `id`,
    /// `components`, `entity_count`, `column_count`, `add_edges`, `remove_edges`), `entities`,
    /// and `prefabs`.
    pub fn world_to_value(&self, alloc: A) -> Result<Value<A>, ReflectError> {
        let mut entities = OwnVec::new_in(alloc);
        let mut prefabs = OwnVec::new_in(alloc);
        let mut prefab_count = 0usize;

        for (slot_idx, slot) in self.slots.as_slice().iter().enumerate() {
            if !slot.alive {
                continue;
            }
            let Some(raw_slot) = usize_to_u32(slot_idx) else {
                continue;
            };
            let ent = Entity::from_parts(raw_slot, slot.generation);
            let ent_val = self.entity_to_value(ent, alloc)?;
            let is_prefab = self
                .table(slot.table_id)
                .is_ok_and(|t| t.has_id(Id::Tag(self.builtins.prefab)));
            if is_prefab {
                prefab_count = prefab_count.saturating_add(1);
                prefabs
                    .try_push(ent_val.copy_in(alloc))
                    .map_err(|r| ReflectError::Alloc(r.why))?;
            }
            entities.try_push(ent_val).map_err(|r| ReflectError::Alloc(r.why))?;
        }

        let mut stats = Map::new_in(alloc);
        let _ = stats.insert(
            OwnString::try_from_str_in("entities", alloc)?,
            Value::Int(Int::from_u64(u64::try_from(self.live_count).unwrap_or(u64::MAX))),
        );
        let _ = stats.insert(
            OwnString::try_from_str_in("archetypes", alloc)?,
            Value::Int(Int::from_u64(u64::try_from(self.tables.len()).unwrap_or(u64::MAX))),
        );
        let _ = stats.insert(
            OwnString::try_from_str_in("components", alloc)?,
            Value::Int(Int::from_u64(u64::try_from(self.components.len()).unwrap_or(u64::MAX))),
        );
        let _ = stats.insert(
            OwnString::try_from_str_in("prefabs", alloc)?,
            Value::Int(Int::from_u64(u64::try_from(prefab_count).unwrap_or(u64::MAX))),
        );
        let _ = stats.insert(
            OwnString::try_from_str_in("observers", alloc)?,
            Value::Int(Int::from_u64(u64::try_from(self.observers.len()).unwrap_or(u64::MAX))),
        );

        let mut comps = OwnVec::try_with_capacity_in(self.components.len(), alloc)?;
        for meta in self.components.as_slice() {
            let mut cobj = Map::new_in(alloc);
            let _ = cobj.insert(
                OwnString::try_from_str_in("id", alloc)?,
                Value::Int(Int::from_u64(u64::from(meta.id.0))),
            );
            let _ = cobj.insert(
                OwnString::try_from_str_in("name", alloc)?,
                Value::Text(OwnString::try_from_str_in(meta.name(), alloc)?),
            );
            let _ = cobj.insert(
                OwnString::try_from_str_in("size", alloc)?,
                Value::Int(Int::from_u64(u64::try_from(meta.size).unwrap_or(u64::MAX))),
            );
            let _ = cobj.insert(
                OwnString::try_from_str_in("can_copy", alloc)?,
                Value::Bool(meta.can_copy),
            );
            let _ = cobj.insert(
                OwnString::try_from_str_in("reflected", alloc)?,
                Value::Bool(meta.reflected),
            );
            let schema_val = match meta.schema(alloc)? {
                Some(s) => s,
                None => Value::Null,
            };
            let _ = cobj.insert(OwnString::try_from_str_in("schema", alloc)?, schema_val);
            comps.try_push(Value::Map(cobj)).map_err(|r| ReflectError::Alloc(r.why))?;
        }

        let mut archetypes = OwnVec::try_with_capacity_in(self.tables.len(), alloc)?;
        for table in self.tables.as_slice() {
            let mut aobj = Map::new_in(alloc);
            let _ = aobj.insert(
                OwnString::try_from_str_in("id", alloc)?,
                Value::Int(Int::from_u64(u64::from(table.id().0))),
            );
            let mut id_labels = OwnVec::try_with_capacity_in(table.archetype().len(), alloc)?;
            for &id in table.archetype() {
                let mut label = OwnString::new_in(alloc);
                self.write_id_label(&mut label, id);
                id_labels
                    .try_push(Value::Text(label))
                    .map_err(|r| ReflectError::Alloc(r.why))?;
            }
            let _ = aobj.insert(
                OwnString::try_from_str_in("components", alloc)?,
                Value::List(id_labels),
            );
            let _ = aobj.insert(
                OwnString::try_from_str_in("entity_count", alloc)?,
                Value::Int(Int::from_u64(u64::try_from(table.len()).unwrap_or(u64::MAX))),
            );
            let _ = aobj.insert(
                OwnString::try_from_str_in("column_count", alloc)?,
                Value::Int(Int::from_u64(u64::try_from(table.column_count()).unwrap_or(u64::MAX))),
            );
            let _ = aobj.insert(
                OwnString::try_from_str_in("add_edges", alloc)?,
                Value::Int(Int::from_u64(u64::try_from(table.add_edge_count()).unwrap_or(u64::MAX))),
            );
            let _ = aobj.insert(
                OwnString::try_from_str_in("remove_edges", alloc)?,
                Value::Int(Int::from_u64(
                    u64::try_from(table.remove_edge_count()).unwrap_or(u64::MAX),
                )),
            );
            archetypes.try_push(Value::Map(aobj)).map_err(|r| ReflectError::Alloc(r.why))?;
        }

        let mut root = Map::new_in(alloc);
        let _ = root.insert(OwnString::try_from_str_in("stats", alloc)?, Value::Map(stats));
        let _ = root.insert(OwnString::try_from_str_in("components", alloc)?, Value::List(comps));
        let _ = root.insert(
            OwnString::try_from_str_in("archetypes", alloc)?,
            Value::List(archetypes),
        );
        let _ = root.insert(OwnString::try_from_str_in("entities", alloc)?, Value::List(entities));
        let _ = root.insert(OwnString::try_from_str_in("prefabs", alloc)?, Value::List(prefabs));
        Ok(Value::Map(root))
    }

    fn format_entity_label(&self, entity: Entity, alloc: A) -> Result<OwnString<A>, AllocFailure> {
        if let Some(name) = self.entity_name(entity) {
            OwnString::try_from_str_in(name, alloc)
        } else {
            let mut out = OwnString::new_in(alloc);
            let _ = write!(out, "{entity}");
            Ok(out)
        }
    }

    // -------------------------------------------------------------------------
    // Deterministic Tabular Snapshot & Structural Invariant Verification
    // -------------------------------------------------------------------------

    /// Produce a deterministic tabular text summary of world statistics, registered components,
    /// archetype tables, transition edges, and entity/pair counts.
    pub fn snapshot_tab(&self, alloc: A) -> Result<OwnString<A>, EcsError> {
        let mut out = OwnString::new_in(alloc);
        let mut total_pairs = 0usize;
        let mut total_tags = 0usize;
        for table in self.tables.as_slice() {
            if table.is_empty() {
                continue;
            }
            for &id in table.archetype() {
                match id {
                    Id::Component(_) => {}
                    Id::Tag(_) => total_tags = total_tags.saturating_add(table.len()),
                    Id::Pair(_, _) => total_pairs = total_pairs.saturating_add(table.len()),
                }
            }
        }

        let _ = writeln!(
            out,
            "# blaze-ecs snapshot\nentities_live\t{}\ntables\t{}\ncomponents\t{}\nobservers\t{}\ntags_attached\t{}\npairs_attached\t{}",
            self.live_count,
            self.tables.len(),
            self.components.len(),
            self.observers.len(),
            total_tags,
            total_pairs,
        );

        let _ = writeln!(out, "\n## components\nid\tname\tschema_id\tcopyable");
        for meta in self.components.as_slice() {
            match meta.schema_id {
                Some(sid) => {
                    let _ = writeln!(
                        out,
                        "{}\t{}\t0x{:016x}\t{}",
                        meta.id.0,
                        meta.name(),
                        sid,
                        meta.can_copy
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "{}\t{}\t-\t{}",
                        meta.id.0,
                        meta.name(),
                        meta.can_copy
                    );
                }
            }
        }

        let _ = writeln!(
            out,
            "\n## archetypes\ntable\tentities\tcolumns\tadd_edges\tremove_edges\tarchetype"
        );
        for table in self.tables.as_slice() {
            let _ = write!(
                out,
                "{}\t{}\t{}\t{}\t{}\t[",
                table.id().0,
                table.len(),
                table.column_count(),
                table.add_edge_count(),
                table.remove_edge_count(),
            );
            for (idx, &id) in table.archetype().iter().enumerate() {
                if idx > 0 {
                    let _ = write!(out, ", ");
                }
                self.write_id_label(&mut out, id);
            }
            let _ = writeln!(out, "]");
        }
        Ok(out)
    }

    fn write_id_label(&self, out: &mut OwnString<A>, id: Id) {
        match id {
            Id::Component(cid) => {
                if let Some(meta) = self.component_meta(cid) {
                    let _ = write!(out, "{}", meta.name());
                } else {
                    let _ = write!(out, "Comp#{}", cid.0);
                }
            }
            Id::Tag(e) => {
                if let Some(name) = self.entity_name(e) {
                    let _ = write!(out, "#{name}");
                } else {
                    let _ = write!(out, "#{e}");
                }
            }
            Id::Pair(rel, target) => {
                let _ = write!(out, "(");
                if let Some(name) = self.entity_name(rel) {
                    let _ = write!(out, "{name}");
                } else {
                    let _ = write!(out, "{rel}");
                }
                let _ = write!(out, ", ");
                if let Some(name) = self.entity_name(target) {
                    let _ = write!(out, "{name}");
                } else {
                    let _ = write!(out, "{target}");
                }
                let _ = write!(out, ")");
            }
        }
    }

    /// Check internal bijection and column-alignment invariants across all slots and tables.
    pub fn verify_invariants(&self) -> Result<(), EcsError> {
        let mut live_seen = 0usize;
        for (slot_idx, slot) in self.slots.as_slice().iter().enumerate() {
            if !slot.alive {
                continue;
            }
            live_seen += 1;
            let raw_slot = usize_to_u32(slot_idx).ok_or(EcsError::EntityCapacityExceeded)?;
            let expected = Entity::from_parts(raw_slot, slot.generation);
            let tbl = self.table(slot.table_id)?;
            let actual = tbl.entities().get(slot.row).copied().ok_or(EcsError::RowOutOfBounds {
                table: slot.table_id,
                row: slot.row,
                len: tbl.len(),
            })?;
            if actual != expected {
                return Err(EcsError::RowOutOfBounds {
                    table: slot.table_id,
                    row: slot.row,
                    len: tbl.len(),
                });
            }
        }
        if live_seen != self.live_count {
            return Err(EcsError::EntityCapacityExceeded);
        }

        let mut table_entities_total = 0usize;
        for table in self.tables.as_slice() {
            table_entities_total += table.len();
            // Archetype must be strictly sorted.
            let arch = table.archetype();
            for pair in arch.windows(2) {
                if let [a, b] = *pair {
                    if a >= b {
                        return Err(EcsError::InvalidTable(table.id()));
                    }
                }
            }
            // Every column must match entity slice length.
            for col in table.columns.as_slice() {
                if col.col.len() != table.len() {
                    return Err(EcsError::RowOutOfBounds {
                        table: table.id(),
                        row: col.col.len(),
                        len: table.len(),
                    });
                }
            }
            // Every entity in table must point back to `(table.id(), row)`.
            for (row, &entity) in table.entities().iter().enumerate() {
                let slot = self.live_slot(entity)?;
                if slot.table_id != table.id() || slot.row != row {
                    return Err(EcsError::RowOutOfBounds {
                        table: table.id(),
                        row,
                        len: table.len(),
                    });
                }
            }
        }
        if table_entities_total != self.live_count {
            return Err(EcsError::EntityCapacityExceeded);
        }
        Ok(())
    }
}

#[inline]
fn table_has_all_components<A: Allocator + Copy + 'static>(
    table: &Table<A>,
    cids: &[ComponentId],
) -> bool {
    for &cid in cids {
        if !table.has_id(Id::Component(cid)) {
            return false;
        }
    }
    true
}
