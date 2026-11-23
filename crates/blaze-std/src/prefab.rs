//! Zero-copy Shared Prefabs & Reactive Templates (`bstd::prefab`).
//!
//! Brings Flecs-grade Prefab memory sharing and Flecs-Script-grade parameterized reactive
//! Templates natively into Blaze — `#![no_std]`, `#![forbid(unsafe_code)]`, explicit
//! `A: AllocatorClone`, zero global heap, and zero runtime DSL interpreter.
//!
//! # Architecture
//!
//! 1. **Nominal components + Type-Set Prefabs ([`Prefab`])**:
//!    - Components are distinct `struct` types (`struct Flex { .. }`, `struct Padding { .. }`).
//!    - A [`Prefab<Shared, Own>`] splits components into two compile-time sets:
//!      - `Shared: UniqueSet` — stored **once** per archetype via `blaze_own::Rc<Shared, A>`
//!        ([`blaze_own::LocalShare`], ~0.5 ns non-atomic sharing, 0 bytes per entity).
//!      - `Own` — auto-overridden per-instance components stored densely per entity.
//!    - Prefab inheritance (`prefab Box : ui.Column, Plate`) is compile-time Type-Set composition
//!      via [`Prefab::extend_shared`] (enforcing `Lacks<C>`) and [`Prefab::override_shared`].
//! 2. **Split Shared/Own Archetypes ([`Archetype`])**:
//!    - Spawning 10,000 entities from a prefab allocates **0 bytes** per entity for `Shared`
//!      components and packs `Own` densely in `blaze_own::Vec<Own, A>`.
//!    - Updating the prefab's shared component (`Archetype::set_shared`) writes one address and
//!      immediately updates every non-overridden instance in O(1).
//!    - Overriding a shared component on one entity ([`Archetype::override_component`]) promotes
//!      that entity into a target archetype owning its private copy while all other entities
//!      continue sharing at 0 bytes.
//!    - Iteration ([`Archetype::for_each_mut`]) resolves shared vs owned storage **at compile
//!      time** (zero runtime `is_self` branch).
//! 3. **Parameterized Reactive Templates ([`Reactive`])**:
//!    - Pairs seamlessly with [`crate::record_fn!`] / [`crate::call!`] named arguments with
//!      defaults (`fn obj_row(ui: &mut UiWorld, p: { name: &str, depth: u32 = 0, open: bool = false })`).
//!    - Returns statically typed `Slots` (O(1) field access, replacing runtime `SlotOf` pairs).
//!    - Supports true **retractions** (removing optional components/children when a prop flips
//!      `true -> false`) while preserving live runtime/animation state on untouched slots.

#![forbid(unsafe_code)]

use core::alloc::AllocatorClone;

use blaze_own::{LocalShare, Rc, Vec};

use crate::type_set::{Cons, Has, Lacks, Pluck, UniqueSet};

/// Generational handle to an entity living inside an [`Archetype`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EntityId {
    /// Slot index into the archetype's sparse index table.
    pub index: u32,
    /// Generation counter preventing ABA use-after-despawn bugs.
    pub generation: u32,
}

impl EntityId {
    #[inline(always)]
    pub const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

/// A static, composable Prefab splitting components into:
/// - `Shared`: stored once across all instances (0 bytes per entity).
/// - `Own`: per-entity auto-overridden defaults copied on spawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefab<Shared: UniqueSet, Own> {
    shared: Shared,
    own_default: Own,
}

impl<Shared: UniqueSet, Own> Prefab<Shared, Own> {
    /// Creates a new prefab after asserting at compile time that all `Shared` member types are
    /// unique (rejecting duplicate component types).
    #[inline]
    pub fn new(shared: Shared, own_default: Own) -> Self {
        <Shared as UniqueSet>::assert_unique();
        Self { shared, own_default }
    }

    /// Reads a shared component `C` from this prefab (order-independent lookup).
    #[inline]
    pub fn get_shared<C, I>(&self) -> &C
    where
        Shared: Has<C, I>,
    {
        self.shared.get()
    }

    /// Borrow the whole shared component set.
    #[inline]
    pub const fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Borrow the auto-overridden per-instance default bundle.
    #[inline]
    pub const fn own_default(&self) -> &Own {
        &self.own_default
    }

    /// Prefab inheritance: extends `Shared` with a new component `C` not already present
    /// (`Shared: Lacks<C>`).
    #[inline]
    pub fn extend_shared<C>(self, comp: C) -> Prefab<Cons<C, Shared>, Own>
    where
        Shared: Lacks<C>,
        Cons<C, Shared>: UniqueSet,
    {
        Prefab::new(Cons { head: comp, tail: self.shared }, self.own_default)
    }

    /// Prefab specialization: overrides an existing shared component `C` inherited from a base
    /// prefab (e.g. `Frigate` overriding `Spaceship`'s `Defense`).
    #[inline]
    pub fn override_shared<C, I>(mut self, comp: C) -> Self
    where
        Shared: Has<C, I>,
    {
        *self.shared.get_mut() = comp;
        self
    }

    /// Adds a new auto-overridden per-instance component `C` to `Own` (`Own: Lacks<C>`).
    #[inline]
    pub fn extend_own<C>(self, default_comp: C) -> Prefab<Shared, Cons<C, Own>>
    where
        Own: Lacks<C>,
    {
        Prefab { shared: self.shared, own_default: Cons { head: default_comp, tail: self.own_default } }
    }

    /// Consumes the prefab into its `(shared, own_default)` parts.
    #[inline]
    pub fn into_parts(self) -> (Shared, Own) {
        (self.shared, self.own_default)
    }
}

/// Sparse slot entry mapping an [`EntityId`] index to its dense row in [`Archetype::own`].
#[derive(Clone, Copy, Debug)]
struct Slot {
    generation: u32,
    /// `u32::MAX` when the slot is free.
    row: u32,
}

const FREE_ROW: u32 = u32::MAX;

/// Split Shared/Own Archetype Table:
/// - `Shared` lives **once** in `Rc<Shared, A>` (`LocalShare`, ~0.5 ns share, 0 bytes/entity).
/// - `Own` lives densely in `Vec<Own, A>` with O(1) swap-remove despawn and generational handles.
pub struct Archetype<Shared: UniqueSet, Own, A: AllocatorClone + Copy> {
    shared: Rc<Shared, A>,
    own_default: Own,
    own: Vec<Own, A>,
    row_to_slot: Vec<u32, A>,
    slots: Vec<Slot, A>,
    free_slots: Vec<u32, A>,
    alloc: A,
}

/// Error returned when an [`Archetype`] cannot allocate a new `u32` slot/row index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArchetypeFull;

impl core::fmt::Display for ArchetypeFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("archetype reached u32::MAX entity capacity")
    }
}

impl core::error::Error for ArchetypeFull {}

/// Converts a `u32` entity/row index to `usize` without a narrowing `as` cast.
#[inline(always)]
fn idx_usize(i: u32) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

impl<Shared: UniqueSet, Own, A: AllocatorClone + Copy> Archetype<Shared, Own, A> {
    /// Creates an empty archetype table backed by `prefab` in allocator `alloc`.
    pub fn from_prefab_in(prefab: Prefab<Shared, Own>, alloc: A) -> Self {
        let (shared, own_default) = prefab.into_parts();
        Self {
            shared: Rc::new_in(shared, alloc),
            own_default,
            own: Vec::new_in(alloc),
            row_to_slot: Vec::new_in(alloc),
            slots: Vec::new_in(alloc),
            free_slots: Vec::new_in(alloc),
            alloc,
        }
    }

    /// Creates an archetype table sharing an existing `LocalShare` `Rc<Shared, A>` handle
    /// (~0.5 ns, zero allocation for `Shared`).
    pub fn from_shared_handle_in(shared: &Rc<Shared, A>, own_default: Own, alloc: A) -> Self {
        Self {
            shared: shared.local_share(),
            own_default,
            own: Vec::new_in(alloc),
            row_to_slot: Vec::new_in(alloc),
            slots: Vec::new_in(alloc),
            free_slots: Vec::new_in(alloc),
            alloc,
        }
    }

    /// Number of live entities in this archetype.
    #[inline]
    pub fn len(&self) -> usize {
        self.own.len()
    }

    /// Whether this archetype has zero live entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.own.is_empty()
    }

    /// Borrow the single shared component bundle for this archetype (0 bytes per entity).
    #[inline]
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Obtain another single-thread `LocalShare` handle to the shared component bundle (~0.5 ns).
    #[inline]
    pub fn shared_handle(&self) -> Rc<Shared, A> {
        self.shared.local_share()
    }

    /// Reads one shared component `C` from this archetype in O(1).
    #[inline]
    pub fn get_shared<C, I>(&self) -> &C
    where
        Shared: Has<C, I>,
    {
        self.shared.get()
    }

    /// Replaces the single shared bundle for this archetype in O(1); every entity in this
    /// archetype immediately observes the updated shared values.
    pub fn set_shared(&mut self, updated: Shared) {
        self.shared = Rc::new_in(updated, self.alloc);
    }

    /// Spawns a new entity using this prefab's auto-overridden `Own` default (requires `Own: Copy`).
    pub fn spawn(&mut self) -> Result<EntityId, ArchetypeFull>
    where
        Own: Copy,
    {
        let own = self.own_default;
        self.spawn_with(own)
    }

    /// Spawns a new entity with custom per-instance `own` components while sharing `Shared` at 0 bytes.
    pub fn spawn_with(&mut self, own: Own) -> Result<EntityId, ArchetypeFull> {
        let row = u32::try_from(self.own.len()).map_err(|_| ArchetypeFull)?;
        if row == FREE_ROW {
            return Err(ArchetypeFull);
        }
        let (slot_idx, generation) = if let Some(reused) = self.free_slots.pop() {
            if let Some(slot) = self.slots.get_mut(idx_usize(reused)) {
                slot.row = row;
                (reused, slot.generation)
            } else {
                return Err(ArchetypeFull);
            }
        } else {
            let idx = u32::try_from(self.slots.len()).map_err(|_| ArchetypeFull)?;
            if idx == FREE_ROW {
                return Err(ArchetypeFull);
            }
            self.slots.push(Slot { generation: 0, row });
            (idx, 0)
        };
        self.own.push(own);
        self.row_to_slot.push(slot_idx);
        Ok(EntityId::new(slot_idx, generation))
    }

    /// Whether `id` currently refers to a live entity in this archetype.
    #[inline]
    pub fn is_alive(&self, id: EntityId) -> bool {
        self.slots
            .get(idx_usize(id.index))
            .is_some_and(|s| s.row != FREE_ROW && s.generation == id.generation)
    }

    /// Borrows `id`'s private `Own` components if alive.
    #[inline]
    pub fn get_own(&self, id: EntityId) -> Option<&Own> {
        let slot = self.slots.get(idx_usize(id.index))?;
        if slot.row == FREE_ROW || slot.generation != id.generation {
            return None;
        }
        self.own.get(idx_usize(slot.row))
    }

    /// Uniquely borrows `id`'s private `Own` components if alive.
    #[inline]
    pub fn get_own_mut(&mut self, id: EntityId) -> Option<&mut Own> {
        let slot = self.slots.get(idx_usize(id.index))?;
        if slot.row == FREE_ROW || slot.generation != id.generation {
            return None;
        }
        self.own.get_mut(idx_usize(slot.row))
    }

    /// O(1) swap-remove despawn of `id`, returning its owned components and invalidating old handles.
    pub fn despawn(&mut self, id: EntityId) -> Option<Own> {
        let slot = self.slots.get_mut(idx_usize(id.index))?;
        if slot.row == FREE_ROW || slot.generation != id.generation {
            return None;
        }
        let row = idx_usize(slot.row);
        let row_u32 = slot.row;
        slot.row = FREE_ROW;
        slot.generation = slot.generation.wrapping_add(1);
        self.free_slots.push(id.index);

        let removed = self.own.remove_swap(row);
        self.row_to_slot.remove_swap(row);
        if let Some(&moved_slot) = self.row_to_slot.get(row)
            && let Some(moved) = self.slots.get_mut(idx_usize(moved_slot))
        {
            moved.row = row_u32;
        }
        Some(removed)
    }

    /// Dense zero-branch iteration broadcasting `&Shared` once per table alongside each entity's `&Own`.
    #[inline]
    pub fn for_each(&self, mut f: impl FnMut(EntityId, &Shared, &Own)) {
        let shared = &*self.shared;
        for (&slot_idx, own) in self.row_to_slot.iter().zip(self.own.iter()) {
            if let Some(slot) = self.slots.get(idx_usize(slot_idx)) {
                f(EntityId::new(slot_idx, slot.generation), shared, own);
            }
        }
    }

    /// Dense zero-branch mutable iteration broadcasting `&Shared` once per table alongside `&mut Own`.
    #[inline]
    pub fn for_each_mut(&mut self, mut f: impl FnMut(EntityId, &Shared, &mut Own)) {
        let shared = &*self.shared;
        for (&slot_idx, own) in self.row_to_slot.iter().zip(self.own.iter_mut()) {
            if let Some(slot) = self.slots.get(idx_usize(slot_idx)) {
                f(EntityId::new(slot_idx, slot.generation), shared, own);
            }
        }
    }

    /// Component override (archetype promotion): removes `id` from `self` and moves it into
    /// `target` with a private overridden copy of `C` (`Own` extended with `C`), while all other
    /// entities in `self` continue sharing `C` at 0 bytes.
    pub fn override_component<C, I, TargetShared>(
        &mut self,
        id: EntityId,
        override_value: C,
        target: &mut Archetype<TargetShared, Cons<C, Own>, A>,
    ) -> Option<EntityId>
    where
        Shared: Pluck<C, I, Remainder = TargetShared>,
        TargetShared: UniqueSet,
    {
        let own = self.despawn(id)?;
        target.spawn_with(Cons { head: override_value, tail: own }).ok()
    }
}

// -------------------------------------------------------------------------------------------------
// Reactive Templates (parameterized procedural blueprints with true retractions)
// -------------------------------------------------------------------------------------------------

/// Summary of what a reactive template reconciliation performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReconcileReport {
    /// True if props were equal to the previous mount/update and zero work ran.
    pub skipped: bool,
    /// Number of component writes performed.
    pub updated: u32,
    /// Number of optional components/children retracted (removed) when a branch turned off.
    pub retracted: u32,
}

/// A live mounted template instance combining:
/// - `props`: last reconciled prop record (for exact equality short-circuit & per-field diffing),
/// - `slots`: statically typed child handles (replaces Flecs `SlotOf` runtime pairs),
/// - `state`: optional structural flags / runtime bookkeeping for exact retractions.
#[derive(Clone, Copy, Debug)]
pub struct Reactive<Props, Slots, State = ()> {
    props: Props,
    slots: Slots,
    state: State,
}

impl<Props, Slots, State> Reactive<Props, Slots, State> {
    /// Mounts a template into `world` with initial `props`.
    #[inline]
    pub fn mount<W>(
        world: &mut W,
        props: Props,
        build: impl FnOnce(&mut W, &Props) -> (Slots, State),
    ) -> Self {
        let (slots, state) = build(world, &props);
        Self { props, slots, state }
    }

    /// Last reconciled props.
    #[inline]
    pub const fn props(&self) -> &Props {
        &self.props
    }

    /// Statically typed slot handles (`O(1)` field access).
    #[inline]
    pub const fn slots(&self) -> &Slots {
        &self.slots
    }

    /// Mutable access to template state.
    #[inline]
    pub const fn state(&self) -> &State {
        &self.state
    }

    /// Fine-grained reactive update:
    /// - Skips completely when `next_props == self.props`.
    /// - Otherwise invokes `diff(world, &prev_props, &next_props, &mut slots, &mut state)`,
    ///   which updates only changed components AND retracts optional components/children when a
    ///   branch turns off — preserving untouched runtime/animation state.
    pub fn reconcile<W>(
        &mut self,
        world: &mut W,
        next_props: Props,
        diff: impl FnOnce(&mut W, &Props, &Props, &mut Slots, &mut State) -> ReconcileReport,
    ) -> ReconcileReport
    where
        Props: PartialEq,
    {
        if self.props == next_props {
            return ReconcileReport { skipped: true, updated: 0, retracted: 0 };
        }
        let report = diff(world, &self.props, &next_props, &mut self.slots, &mut self.state);
        self.props = next_props;
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{declare_distinct, tset};

    // Nominal ECS components (`struct`, NOT `type` aliases!).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Node {
        ordered_children: bool,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Flex {
        gap: u32,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Padding(u32);
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Fill(u32);
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Position {
        x: i32,
        y: i32,
    }

    declare_distinct!(Node, Flex, Padding, Fill, Position);

    #[test]
    fn prefab_inheritance_zero_copy_sharing_and_override_promotion() {
        let a = blaze_own::testing::heap().alloc();

        // Base prefab `ui.Node`, extended to `ui.Row`, extended to `ui.Box` with overridden `Flex`.
        let ui_node = Prefab::new(tset! { Node { ordered_children: true } }, tset! { Position { x: 0, y: 0 } });
        let ui_row = ui_node.extend_shared(Flex { gap: 0 });
        let ui_box = ui_row
            .extend_shared(Padding(12))
            .extend_shared(Fill(0x222222))
            .override_shared(Flex { gap: 6 });

        assert_eq!(ui_box.get_shared::<Flex, _>().gap, 6);
        assert_eq!(ui_box.get_shared::<Padding, _>().0, 12);

        // Spawn 1,000 instances sharing `Shared` via LocalShare (0 bytes/entity for Shared!).
        let mut table = Archetype::from_prefab_in(ui_box, a);
        let mut first = EntityId::new(0, 0);
        for i in 0..1000i32 {
            let id = table.spawn_with(tset! { Position { x: i, y: i * 2 } }).expect("spawn");
            if i == 0 {
                first = id;
            }
        }
        assert_eq!(table.len(), 1000);
        assert_eq!(Rc::strong_count(&table.shared_handle()), 2);

        // Promote entity 0 to have its own private `Fill` while the other 999 keep sharing `Fill`.
        let (rest_shared, _) = {
            let (_fill, rest) = table.shared().pluck::<Fill, _>();
            (rest, ())
        };
        let mut overridden_table = Archetype::from_prefab_in(
            Prefab::new(rest_shared, tset! { Fill(0), Position { x: 0, y: 0 } }),
            a,
        );
        let promoted = table
            .override_component(first, Fill(0xFF00AA), &mut overridden_table)
            .expect("entity 0 promoted");
        assert_eq!(table.len(), 999);
        assert_eq!(overridden_table.len(), 1);
        assert!(!table.is_alive(first));

        // O(1) global update to the prefab's `Fill` updates all 999 shared instances at once,
        // while the 1 overridden entity keeps its private `0xFF00AA`!
        let updated_shared = Prefab::new(*table.shared(), ()).override_shared(Fill(0x445566)).into_parts().0;
        table.set_shared(updated_shared);

        let mut seen = 0usize;
        table.for_each(|_, shared, own| {
            assert_eq!(shared.get::<Fill, _>().0, 0x445566);
            assert_eq!(shared.get::<Flex, _>().gap, 6);
            assert!(own.get::<Position, _>().x >= 1);
            seen += 1;
        });
        assert_eq!(seen, 999);
        assert_eq!(overridden_table.get_own(promoted).unwrap().get::<Fill, _>().0, 0xFF00AA);
    }

    // End-to-end Reactive Template combining `bstd::record_fn!` named args with defaults + retractions.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum RowLook {
        On,
        Rest,
        Done,
    }

    struct MiniUiWorld {
        next_id: u32,
        /// Live component state for the row's disclose button.
        disclose_pressable: bool,
        /// Live component state for the row's title ink.
        title_ink: u32,
        /// Live animation state (must survive reactive prop updates untouched!).
        hover_amt: u32,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct RowSlots {
        root: u32,
        disclose: u32,
        name: u32,
    }

    crate::record_fn! {
        fn make_obj_row_props(
            _marker: (),
            p: {
                name: &'static str,
                depth: u32 = 0,
                kids: bool = false,
                open: bool = false,
                look: RowLook = RowLook::Rest,
            }
        ) -> make_obj_row_props::Args {
            p
        }
    }

    fn ink_for(look: RowLook) -> u32 {
        match look {
            RowLook::On => 0xFFFFFF,
            RowLook::Rest => 0xCCCCCC,
            RowLook::Done => 0x777777,
        }
    }

    #[test]
    fn reactive_template_with_named_default_props_and_retractions() {
        let mut world = MiniUiWorld {
            next_id: 0,
            disclose_pressable: false,
            title_ink: 0,
            hover_amt: 0,
        };

        // Instantiate using named args with defaults (`depth = 0, open = false, look = Rest`):
        let initial = crate::call!(make_obj_row_props((), { name: "agent-1", kids: true, .. }));
        let mut row = Reactive::mount(&mut world, initial, |w, p| {
            w.next_id += 3;
            w.disclose_pressable = p.kids;
            w.title_ink = ink_for(p.look);
            (RowSlots { root: 1, disclose: 2, name: 3 }, ())
        });

        assert!(world.disclose_pressable);
        assert_eq!(world.title_ink, 0xCCCCCC);
        assert_eq!(row.slots().name, 3);

        // Simulate runtime hover animation state.
        world.hover_amt = 85;

        // Reconciling with identical props is a zero-work no-op.
        let r_noop = row.reconcile(&mut world, initial, |_, _, _, _, _| {
            unreachable!("identical props must short-circuit")
        });
        assert!(r_noop.skipped);

        // Reconcile: `kids` turns `false` (triggers real retraction of `Pressable`!) and `look` turns `On`.
        let updated = crate::call!(make_obj_row_props((), {
            name: "agent-1",
            kids: false,
            look: RowLook::On,
            ..
        }));
        let report = row.reconcile(&mut world, updated, |w, prev, next, _slots, _| {
            let mut r = ReconcileReport::default();
            if prev.kids != next.kids {
                w.disclose_pressable = next.kids;
                if next.kids {
                    r.updated += 1;
                } else {
                    r.retracted += 1;
                }
            }
            if prev.look != next.look {
                w.title_ink = ink_for(next.look);
                r.updated += 1;
            }
            r
        });

        assert!(!report.skipped);
        assert_eq!(report.updated, 1);
        assert_eq!(report.retracted, 1);
        assert!(!world.disclose_pressable, "Pressable retracted when kids became false");
        assert_eq!(world.title_ink, 0xFFFFFF);
        assert_eq!(world.hover_amt, 85, "animation state preserved across reconciliation");
    }
}
