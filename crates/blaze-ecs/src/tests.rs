use core::cell::Cell;

use blaze_own::{Alloc, Box as OwnBox, Vec as OwnVec, format_in, testing::heap};
use blaze_sim::{Broke, Config};

use crate::map::FlatMap;
use crate::{
    CachedQuery, EcsError, Entity, Event, EventKind, Id, Phase, Pipeline, QuerySpec, SystemDesc,
    World,
};

fn alloc() -> Alloc<'static> {
    heap().alloc()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pos {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Vel {
    dx: i32,
    dy: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Health(i32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Damage(i32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Armor(i32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LocalPos {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct WorldPos {
    x: i32,
    y: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StepLog {
    order: [u8; 8],
    len: usize,
}

impl StepLog {
    fn push(&mut self, tag: u8) {
        if let Some(slot) = self.order.get_mut(self.len) {
            *slot = tag;
            self.len += 1;
        }
    }
}

#[test]
fn archetype_transitions_edges_swap_remove_and_stale_entities() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let pos_id = w.register_copy_component::<Pos>("Pos").expect("reg Pos");
    let vel_id = w.register_copy_component::<Vel>("Vel").expect("reg Vel");

    // Duplicate registration rejected.
    assert_eq!(w.register_copy_component::<Pos>("Pos2"), Err(EcsError::ComponentAlreadyRegistered(pos_id)));

    let e1 = w.spawn().expect("e1");
    let e2 = w.spawn().expect("e2");
    let e3 = w.spawn().expect("e3");

    w.set(e1, Pos { x: 1, y: 2 }).expect("set e1 pos");
    w.set(e1, Vel { dx: 10, dy: 20 }).expect("set e1 vel");

    let tables_after_e1 = w.table_count();

    // e2 and e3 traverse cached table-graph edges without creating new tables.
    w.set(e2, Pos { x: 3, y: 4 }).expect("set e2 pos");
    w.set(e2, Vel { dx: 30, dy: 40 }).expect("set e2 vel");
    w.set(e3, Pos { x: 5, y: 6 }).expect("set e3 pos");
    w.set(e3, Vel { dx: 50, dy: 60 }).expect("set e3 vel");

    assert_eq!(w.table_count(), tables_after_e1);
    let (t1, _) = w.entity_table(e1).expect("t1");
    let (t2, _) = w.entity_table(e2).expect("t2");
    let (t3, _) = w.entity_table(e3).expect("t3");
    assert_eq!(t1, t2);
    assert_eq!(t2, t3);

    // Verify table-graph edges are populated.
    let empty_tbl = w.table(crate::TableId(0)).expect("empty");
    assert!(empty_tbl.add_edge_count() >= 1);

    // Safe disjoint SoA column borrowing (`columns_1_mut_1_ref` and `columns_2_mut`).
    {
        let tbl = w.table_mut(t1).expect("tbl");
        assert_eq!(tbl.columns_2_mut::<Pos, Pos>(pos_id, pos_id), Err(EcsError::AliasedComponentAccess(pos_id)));
        let (positions, velocities) =
            tbl.columns_1_mut_1_ref::<Pos, Vel>(pos_id, vel_id).expect("disjoint borrow");
        for (p, v) in positions.iter_mut().zip(velocities.iter()) {
            p.x += v.dx;
            p.y += v.dy;
        }
    }
    assert_eq!(w.get::<Pos>(e1).expect("get"), Some(&Pos { x: 11, y: 22 }));
    assert_eq!(w.get::<Pos>(e2).expect("get"), Some(&Pos { x: 33, y: 44 }));
    assert_eq!(w.get::<Pos>(e3).expect("get"), Some(&Pos { x: 55, y: 66 }));

    // Dense swap-remove: removing Vel from middle entity e2 moves e3 into e2's old row.
    assert!(w.remove::<Vel>(e2).expect("remove vel"));
    assert!(!w.has::<Vel>(e2).expect("has vel"));
    assert_eq!(w.get::<Pos>(e2).expect("e2 pos preserved"), Some(&Pos { x: 33, y: 44 }));
    assert_eq!(w.get::<Pos>(e3).expect("e3 pos intact"), Some(&Pos { x: 55, y: 66 }));
    assert_eq!(w.get::<Vel>(e3).expect("e3 vel intact"), Some(&Vel { dx: 50, dy: 60 }));
    w.verify_invariants().expect("invariants after remove");

    // Despawn e1 and verify generational stale-entity rejection + slot reuse.
    let old_e1_slot = e1.slot();
    let old_e1_gen = e1.generation();
    w.despawn(e1).expect("despawn e1");
    assert!(!w.is_alive(e1));
    assert_eq!(w.get::<Pos>(e1), Err(EcsError::DeadEntity(e1)));
    assert_eq!(w.set(e1, Pos { x: 0, y: 0 }), Err(EcsError::DeadEntity(e1)));
    assert_eq!(w.despawn(e1), Err(EcsError::DeadEntity(e1)));

    let e4 = w.spawn().expect("e4");
    assert_eq!(e4.slot(), old_e1_slot);
    assert_eq!(e4.generation(), old_e1_gen + 1);
    assert!(w.is_alive(e4));
    assert!(!w.is_alive(e1));
    w.verify_invariants().expect("invariants at end");
}

#[test]
fn prefab_inheritance_chain_override_instantiate_and_query_exclusion() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let hp_id = w.register_copy_component::<Health>("Health").expect("reg Health");
    let _dmg_id = w.register_copy_component::<Damage>("Damage").expect("reg Damage");
    let _arm_id = w.register_copy_component::<Armor>("Armor").expect("reg Armor");

    let is_a = w.is_a();

    // Multi-level prefab chain: Boss -> Actor -> Base
    let base = w.spawn_prefab("Base").expect("base");
    w.set(base, Health(100)).expect("base hp");
    w.set(base, Armor(5)).expect("base armor");

    let actor = w.spawn_prefab("Actor").expect("actor");
    w.add_pair(actor, is_a, base).expect("actor IsA base");
    w.set(actor, Damage(15)).expect("actor dmg");
    w.set(actor, Armor(10)).expect("actor overrides armor");

    let boss = w.spawn_prefab("Boss").expect("boss");
    w.add_pair(boss, is_a, actor).expect("boss IsA actor");
    w.set(boss, Health(500)).expect("boss overrides hp");

    // `get_inherited` walks Boss -> Actor -> Base iteratively.
    assert_eq!(w.get_inherited::<Health>(boss).expect("boss hp"), Some(&Health(500)));
    assert_eq!(w.get_inherited::<Damage>(boss).expect("boss dmg"), Some(&Damage(15)));
    assert_eq!(w.get_inherited::<Armor>(boss).expect("boss armor"), Some(&Armor(10)));
    assert_eq!(w.get_inherited::<Health>(actor).expect("actor hp"), Some(&Health(100)));
    assert_eq!(w.get_inherited::<Armor>(base).expect("base armor"), Some(&Armor(5)));

    // `IsA` cycle rejection: Base cannot inherit from Boss.
    assert_eq!(
        w.add_pair(base, is_a, boss),
        Err(EcsError::HierarchyCycle { entity: base, relation: is_a, target: boss })
    );

    // Default queries exclude `PREFAB` entities.
    let spec_default = QuerySpec::new_in(a).with_component(hp_id).expect("spec");
    assert_eq!(w.query_entities(&spec_default).expect("q").len(), 0);

    // Including prefabs finds `Base` and `Boss` (which directly hold Health).
    let spec_with_prefabs =
        QuerySpec::new_in(a).with_component(hp_id).expect("spec").include_prefabs(true);
    assert_eq!(w.query_entities(&spec_with_prefabs).expect("q").len(), 2);

    // Instantiate from Boss: copies nearest-ancestor components and links `(IsA, boss)`.
    let raid_boss = w.instantiate(boss).expect("instantiate boss");
    assert!(w.has_pair(raid_boss, is_a, boss).expect("has IsA"));
    assert!(!w.has_tag(raid_boss, w.prefab()).expect("not prefab"));
    assert_eq!(w.get::<Health>(raid_boss).expect("hp"), Some(&Health(500)));
    assert_eq!(w.get::<Damage>(raid_boss).expect("dmg"), Some(&Damage(15)));
    assert_eq!(w.get::<Armor>(raid_boss).expect("armor"), Some(&Armor(10)));

    // Per-instance override does not mutate the prefab.
    w.set(raid_boss, Health(420)).expect("override hp");
    assert_eq!(w.get::<Health>(raid_boss).expect("hp"), Some(&Health(420)));
    assert_eq!(w.get::<Health>(boss).expect("boss unchanged"), Some(&Health(500)));

    // Default query now returns only the live instance, not the prefabs.
    let matched = w.query_entities(&spec_default).expect("q");
    assert_eq!(matched.as_slice(), &[raid_boss]);
    w.verify_invariants().expect("invariants");
}

#[test]
fn hierarchy_exclusive_reparent_cycle_cascade_transforms_and_delete_cascade() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let local_id = w.register_copy_component::<LocalPos>("LocalPos").expect("LocalPos");
    let _world_id = w.register_copy_component::<WorldPos>("WorldPos").expect("WorldPos");
    let child_of = w.child_of();

    // SolarSystem -> Planet -> Moon, plus a second parent Galaxy for re-parenting.
    let galaxy = w.spawn_named("Galaxy").expect("galaxy");
    let sun = w.spawn_named("SolarSystem").expect("sun");
    let planet = w.spawn_named("Planet").expect("planet");
    let moon = w.spawn_named("Moon").expect("moon");

    w.set(galaxy, LocalPos { x: 1000, y: 0 }).expect("galaxy pos");
    w.set(sun, LocalPos { x: 100, y: 10 }).expect("sun pos");
    w.set(planet, LocalPos { x: 20, y: 5 }).expect("planet pos");
    w.set(moon, LocalPos { x: 3, y: 1 }).expect("moon pos");

    w.add_pair(planet, child_of, sun).expect("planet -> sun");
    w.add_pair(moon, child_of, planet).expect("moon -> planet");

    // Cycle rejection: sun cannot become child of moon or itself.
    assert_eq!(
        w.add_pair(sun, child_of, moon),
        Err(EcsError::HierarchyCycle { entity: sun, relation: child_of, target: moon })
    );
    assert_eq!(
        w.add_pair(sun, child_of, sun),
        Err(EcsError::HierarchyCycle { entity: sun, relation: child_of, target: sun })
    );

    // Exclusive re-parenting: moving `sun` under `galaxy` replaces any previous parent.
    w.add_pair(sun, child_of, galaxy).expect("sun -> galaxy");
    assert_eq!(w.parent_of(sun).expect("parent"), Some(galaxy));
    let other_root = w.spawn_named("OtherRoot").expect("other");
    w.add_pair(sun, child_of, other_root).expect("reparent sun");
    assert_eq!(w.parent_of(sun).expect("new parent"), Some(other_root));
    assert!(!w.has_pair(sun, child_of, galaxy).expect("old parent removed"));
    w.add_pair(sun, child_of, galaxy).expect("back to galaxy");

    // Cascade query visits Galaxy -> SolarSystem -> Planet -> Moon strictly top-down.
    let cascade_spec =
        QuerySpec::new_in(a).with_component(local_id).expect("spec").cascade(child_of);
    let ordered = w.query_cascade(&cascade_spec).expect("cascade");
    assert_eq!(ordered.as_slice(), &[galaxy, sun, planet, moon]);

    // Propagate transforms using cascade order.
    for &e in ordered.as_slice() {
        let local = *w.get::<LocalPos>(e).expect("local").expect("present");
        let parent_world = match w.parent_of(e).expect("parent") {
            Some(p) => w.get::<WorldPos>(p).expect("p world").copied().unwrap_or(WorldPos { x: 0, y: 0 }),
            None => WorldPos { x: 0, y: 0 },
        };
        w.set(e, WorldPos { x: parent_world.x + local.x, y: parent_world.y + local.y })
            .expect("set world pos");
    }
    assert_eq!(w.get::<WorldPos>(galaxy).expect("g"), Some(&WorldPos { x: 1000, y: 0 }));
    assert_eq!(w.get::<WorldPos>(sun).expect("s"), Some(&WorldPos { x: 1100, y: 10 }));
    assert_eq!(w.get::<WorldPos>(planet).expect("p"), Some(&WorldPos { x: 1120, y: 15 }));
    assert_eq!(w.get::<WorldPos>(moon).expect("m"), Some(&WorldPos { x: 1123, y: 16 }));

    // `OnDeleteCascade`: despawning `sun` recursively despawns `planet` and `moon`, leaving `galaxy` alive.
    w.despawn(sun).expect("despawn sun");
    assert!(w.is_alive(galaxy));
    assert!(!w.is_alive(sun));
    assert!(!w.is_alive(planet));
    assert!(!w.is_alive(moon));
    assert_eq!(w.children_of(galaxy).expect("children").len(), 0);
    w.verify_invariants().expect("invariants");
}

#[test]
fn symmetric_transitive_relations_and_wildcard_queries() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    let ally = w.spawn_named("Ally").expect("Ally");
    w.add_tag(ally, w.symmetric()).expect("Ally symmetric");

    let located_in = w.spawn_named("LocatedIn").expect("LocatedIn");
    w.add_tag(located_in, w.transitive()).expect("LocatedIn transitive");

    let alice = w.spawn_named("Alice").expect("Alice");
    let bob = w.spawn_named("Bob").expect("Bob");
    let carol = w.spawn_named("Carol").expect("Carol");

    // Symmetric pair mirrors automatically on add and remove.
    w.add_pair(alice, ally, bob).expect("alice ally bob");
    assert!(w.has_pair(alice, ally, bob).expect("a->b"));
    assert!(w.has_pair(bob, ally, alice).expect("b->a"));

    w.add_pair(alice, ally, carol).expect("alice ally carol");
    assert!(w.has_pair(carol, ally, alice).expect("c->a"));

    // Wildcard queries: `(Ally, *)` finds Alice, Bob, Carol; `(*, Alice)` finds Bob and Carol.
    let wc = w.wildcard();
    let q_any_ally = QuerySpec::new_in(a).with_pair(ally, wc).expect("q");
    let ally_entities = w.query_entities(&q_any_ally).expect("run");
    assert_eq!(ally_entities.len(), 3);

    let q_to_alice = QuerySpec::new_in(a).with_pair(wc, alice).expect("q");
    let to_alice = w.query_entities(&q_to_alice).expect("run");
    assert_eq!(to_alice.len(), 2);
    assert!(to_alice.as_slice().contains(&bob));
    assert!(to_alice.as_slice().contains(&carol));

    // Removing symmetric pair removes both directions.
    assert!(w.remove_pair(alice, ally, bob).expect("rm"));
    assert!(!w.has_pair(alice, ally, bob).expect("a->b gone"));
    assert!(!w.has_pair(bob, ally, alice).expect("b->a gone"));

    // Despawning Carol cleans up Alice's `(Ally, Carol)` pair cleanly.
    w.despawn(carol).expect("despawn carol");
    assert!(!w.has_pair(alice, ally, carol).expect("cleaned up"));

    // Transitive relation chain: Room -> Tower -> City -> Kingdom
    let room = w.spawn_named("Room").expect("Room");
    let tower = w.spawn_named("Tower").expect("Tower");
    let city = w.spawn_named("City").expect("City");
    let kingdom = w.spawn_named("Kingdom").expect("Kingdom");

    w.add_pair(room, located_in, tower).expect("r->t");
    w.add_pair(tower, located_in, city).expect("t->c");
    w.add_pair(city, located_in, kingdom).expect("c->k");

    assert!(w.has_transitive(room, located_in, tower).expect("1 hop"));
    assert!(w.has_transitive(room, located_in, city).expect("2 hops"));
    assert!(w.has_transitive(room, located_in, kingdom).expect("3 hops"));
    assert!(!w.has_transitive(kingdom, located_in, room).expect("reverse false"));
    w.verify_invariants().expect("invariants");
}

#[test]
fn cached_queries_incremental_sync_and_reactive_observers() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let pos_id = w.register_copy_component::<Pos>("Pos").expect("Pos");
    let vel_id = w.register_copy_component::<Vel>("Vel").expect("Vel");
    let tag_a = w.spawn_named("TagA").expect("TagA");
    let child_of = w.child_of();
    let wc = w.wildcard();

    // Observers: OnAdd/OnSet/OnRemove for Pos, plus wildcard pair `(ChildOf, *)`.
    let obs_add_pos = w.observe(EventKind::OnAdd, Id::Component(pos_id)).expect("obs add");
    let obs_set_pos = w.observe(EventKind::OnSet, Id::Component(pos_id)).expect("obs set");
    let obs_rem_pos = w.observe(EventKind::OnRemove, Id::Component(pos_id)).expect("obs rem");
    let obs_child_wild =
        w.observe(EventKind::OnAdd, Id::Pair(child_of, wc)).expect("obs child wildcard");

    // Use a bounded cap observer to verify cap enforcement.
    let obs_capped = w.observe_with_cap(EventKind::OnSet, Id::Component(pos_id), 2).expect("cap");

    let spec = QuerySpec::new_in(a)
        .with_component(pos_id)
        .expect("w pos")
        .with_component(vel_id)
        .expect("w vel");
    let mut cached = CachedQuery::new(spec, a);

    // Initial sync scans existing tables (0 matches so far).
    let scanned_initial = w.sync_query(&mut cached).expect("sync 0");
    assert!(scanned_initial > 0);
    assert_eq!(w.query_tables(&mut cached).expect("qt").len(), 0);
    // Steady-state sync scans 0 tables!
    assert_eq!(w.sync_query(&mut cached).expect("sync steady"), 0);

    let p = w.spawn().expect("p");
    let e1 = w.spawn().expect("e1");
    w.set(e1, Pos { x: 1, y: 1 }).expect("set pos 1");
    w.set(e1, Pos { x: 2, y: 2 }).expect("set pos 2");
    w.set(e1, Pos { x: 3, y: 3 }).expect("set pos 3");
    w.set(e1, Vel { dx: 1, dy: 0 }).expect("set vel");
    w.add_pair(e1, child_of, p).expect("e1 child of p");

    // Incremental sync scans ONLY newly created tables and matches both `[Pos, Vel]` and
    // `[Pos, Vel, (ChildOf, p)]` (1 of which is non-empty).
    let scanned_after_e1 = w.sync_query(&mut cached).expect("sync 1");
    assert!(scanned_after_e1 > 0);
    let tids1 = w.query_tables(&mut cached).expect("qt");
    assert_eq!(tids1.len(), 2);
    let non_empty_1 = tids1.iter().filter(|&&tid| !w.table(tid).expect("t").is_empty()).count();
    assert_eq!(non_empty_1, 1);
    assert_eq!(w.sync_query(&mut cached).expect("sync steady 2"), 0);

    // Adding a new archetype `[Pos, Vel, TagA]` incrementally adds a third matched table (2 non-empty).
    let e2 = w.spawn().expect("e2");
    w.set(e2, Pos { x: 10, y: 20 }).expect("e2 pos");
    w.set(e2, Vel { dx: -1, dy: 1 }).expect("e2 vel");
    w.add_tag(e2, tag_a).expect("e2 tag_a");
    let tids2 = w.query_tables(&mut cached).expect("qt 2");
    assert_eq!(tids2.len(), 3);
    let non_empty_2 = tids2.iter().filter(|&&tid| !w.table(tid).expect("t").is_empty()).count();
    assert_eq!(non_empty_2, 2);

    // Verify observer logs.
    assert_eq!(w.observer_events(obs_add_pos).expect("add").len(), 2);
    assert_eq!(w.observer_events(obs_set_pos).expect("set").len(), 4);
    assert_eq!(w.observer_events(obs_capped).expect("cap").len(), 2);
    assert_eq!(w.observer_dropped_count(obs_capped), Some(2));
    assert_eq!(
        w.observer_events(obs_child_wild).expect("wc"),
        &[Event { kind: EventKind::OnAdd, entity: e1, id: Id::Pair(child_of, p) }]
    );

    w.remove::<Pos>(e1).expect("rem pos");
    assert_eq!(
        w.observer_events(obs_rem_pos).expect("rem"),
        &[Event { kind: EventKind::OnRemove, entity: e1, id: Id::Component(pos_id) }]
    );
}

#[test]
fn observer_callback_invoked_synchronously() {
    use blaze_own::{Rc as OwnRc, Share as _};

    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let pos_id = w.register_copy_component::<Pos>("Pos").expect("Pos");

    let hit = OwnRc::new_in(Cell::new(0u32), a);
    let hit_cb = hit.share();
    let cb: crate::ObserverCallback<Alloc<'static>> = OwnBox::new_in(
        move |_ev: Event| {
            hit_cb.set(hit_cb.get().saturating_add(1));
        },
        a,
    );
    let obs_id = w
        .observe_with_callback(EventKind::OnAdd, Id::Component(pos_id), cb)
        .expect("observe cb");
    let e = w.spawn().expect("e");
    w.set(e, Pos { x: 7, y: 8 }).expect("set");
    w.set(e, Pos { x: 9, y: 10 }).expect("set again does not re-fire OnAdd");
    assert_eq!(hit.get(), 1);
    assert_eq!(w.observer_events(obs_id).expect("events").len(), 1);
}

fn sys_load(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    if let Some(log_ent) = world.entity_by_name("Log") {
        if let Some(log) = world.get_mut::<StepLog>(log_ent)? {
            log.push(1);
        }
    }
    Ok(())
}

fn sys_integrate_pos(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    let pos_id = world.component_id::<Pos>().ok_or(EcsError::UnknownComponent(crate::ComponentId(0)))?;
    let vel_id = world.component_id::<Vel>().ok_or(EcsError::UnknownComponent(crate::ComponentId(1)))?;
    let spec = QuerySpec::new_in(world.alloc())
        .with_component(pos_id)?
        .with_component(vel_id)?;
    let mut cached = CachedQuery::new(spec, world.alloc());
    let tids = world.query_tables(&mut cached)?;
    for &tid in tids {
        let tbl = world.table_mut(tid)?;
        let (positions, velocities) = tbl.columns_1_mut_1_ref::<Pos, Vel>(pos_id, vel_id)?;
        for (p, v) in positions.iter_mut().zip(velocities.iter()) {
            p.x += v.dx;
            p.y += v.dy;
        }
    }
    if let Some(log_ent) = world.entity_by_name("Log") {
        if let Some(log) = world.get_mut::<StepLog>(log_ent)? {
            log.push(2);
        }
    }
    Ok(())
}

fn sys_regen_health(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    let hp_id = world.component_id::<Health>().ok_or(EcsError::UnknownComponent(crate::ComponentId(2)))?;
    let spec = QuerySpec::new_in(world.alloc()).with_component(hp_id)?;
    let mut cached = CachedQuery::new(spec, world.alloc());
    let tids = world.query_tables(&mut cached)?;
    for &tid in tids {
        let tbl = world.table_mut(tid)?;
        let hps = tbl.column_mut::<Health>(hp_id)?;
        for h in hps.iter_mut() {
            h.0 += 1;
        }
    }
    if let Some(log_ent) = world.entity_by_name("Log") {
        if let Some(log) = world.get_mut::<StepLog>(log_ent)? {
            log.push(3);
        }
    }
    Ok(())
}

fn sys_store(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    if let Some(log_ent) = world.entity_by_name("Log") {
        if let Some(log) = world.get_mut::<StepLog>(log_ent)? {
            log.push(4);
        }
    }
    Ok(())
}

#[test]
fn phased_pipeline_scheduling_batches_and_conflict_detection() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let pos_id = w.register_copy_component::<Pos>("Pos").expect("Pos");
    let vel_id = w.register_copy_component::<Vel>("Vel").expect("Vel");
    let hp_id = w.register_copy_component::<Health>("Health").expect("Health");
    let _log_id = w.register_copy_component::<StepLog>("StepLog").expect("StepLog");

    let log_ent = w.spawn_named("Log").expect("Log");
    w.set(log_ent, StepLog { order: [0; 8], len: 0 }).expect("set log");

    let e = w.spawn().expect("e");
    w.set(e, Pos { x: 10, y: 20 }).expect("pos");
    w.set(e, Vel { dx: 3, dy: -2 }).expect("vel");
    w.set(e, Health(50)).expect("hp");

    let mut pipe = Pipeline::new_in(a);

    // Self-conflict (reading and writing Pos in the same system) is rejected!
    let bad = SystemDesc::new_in("bad", Phase::OnUpdate, sys_load, a)
        .read(Id::Component(pos_id))
        .expect("r")
        .write(Id::Component(pos_id))
        .expect("w");
    assert_eq!(
        pipe.add_system(bad),
        Err(EcsError::SystemAccessConflict { system: "bad", id: Id::Component(pos_id) })
    );

    // Register systems across phases out of order; pipeline sorts by Phase::ALL.
    pipe.add_system(
        SystemDesc::new_in("store", Phase::OnStore, sys_store, a)
            .read(Id::Component(pos_id))
            .expect("r"),
    )
    .expect("add store");

    // Two disjoint systems in `OnUpdate`: `integrate` writes Pos / reads Vel; `regen` writes Health.
    pipe.add_system(
        SystemDesc::new_in("integrate", Phase::OnUpdate, sys_integrate_pos, a)
            .write(Id::Component(pos_id))
            .expect("w")
            .read(Id::Component(vel_id))
            .expect("r"),
    )
    .expect("add integrate");

    pipe.add_system(
        SystemDesc::new_in("regen", Phase::OnUpdate, sys_regen_health, a)
            .write(Id::Component(hp_id))
            .expect("w"),
    )
    .expect("add regen");

    // A third `OnUpdate` system reading Pos conflicts with `integrate` -> must go in batch 1!
    pipe.add_system(
        SystemDesc::new_in("post_pos_reader", Phase::OnUpdate, sys_load, a)
            .read(Id::Component(pos_id))
            .expect("r"),
    )
    .expect("add reader");

    pipe.add_system(SystemDesc::new_in("load", Phase::OnLoad, sys_load, a)).expect("add load");

    let schedule = pipe.batches().expect("schedule");
    assert_eq!(schedule.phases.len(), 3);
    assert_eq!(schedule.phases.get(0).expect("p0").phase, Phase::OnLoad);
    let update_sched = schedule.phases.get(1).expect("p1");
    assert_eq!(update_sched.phase, Phase::OnUpdate);
    // Batch 0: `integrate` + `regen` (pairwise disjoint -> parallel batch of 2!)
    // Batch 1: `post_pos_reader` (conflicts with `integrate`'s write of Pos)
    assert_eq!(update_sched.batches.len(), 2);
    assert_eq!(update_sched.batches.get(0).expect("b0").systems.len(), 2);
    assert_eq!(update_sched.batches.get(1).expect("b1").systems.len(), 1);
    assert_eq!(schedule.phases.get(2).expect("p2").phase, Phase::OnStore);

    pipe.run_tick(&mut w).expect("tick");
    assert_eq!(w.get::<Pos>(e).expect("pos"), Some(&Pos { x: 13, y: 18 }));
    assert_eq!(w.get::<Health>(e).expect("hp"), Some(&Health(51)));
    let log = *w.get::<StepLog>(log_ent).expect("log").expect("some");
    // OnLoad(1), OnUpdate batch0(2, 3), OnUpdate batch1(1), OnStore(4)
    assert_eq!(log.order.get(..log.len), Some(&[1u8, 2, 3, 1, 4][..]));
}

#[test]
fn snapshot_tab_is_deterministic_and_complete() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let _pos = w.register_copy_component_with_schema::<Pos>("Pos", 0xCAFE_BABE_0000_0001).expect("Pos");
    let _vel = w.register_copy_component::<Vel>("Vel").expect("Vel");

    let ship = w.spawn_prefab("Ship").expect("Ship");
    w.set(ship, Pos { x: 0, y: 0 }).expect("ship pos");
    let s1 = w.instantiate(ship).expect("s1");
    w.set(s1, Vel { dx: 5, dy: 0 }).expect("s1 vel");

    let snap1 = w.snapshot_tab(a).expect("snap1");
    let snap2 = w.snapshot_tab(a).expect("snap2");
    assert_eq!(snap1.as_str(), snap2.as_str());
    let expected = "\
# blaze-ecs snapshot
entities_live\t10
tables\t7
components\t2
observers\t0
tags_attached\t3
pairs_attached\t1

## components
id\tname\tschema_id\tcopyable
0\tPos\t0xcafebabe00000001\ttrue
1\tVel\t-\ttrue

## archetypes
table\tentities\tcolumns\tadd_edges\tremove_edges\tarchetype
0\t7\t0\t2\t0\t[]
1\t0\t0\t1\t1\t[#Exclusive]
2\t1\t0\t0\t1\t[#Exclusive, #OnDeleteCascade]
3\t0\t0\t1\t1\t[#Prefab]
4\t1\t1\t0\t1\t[Pos, #Prefab]
5\t0\t1\t1\t0\t[Pos, (IsA, Ship)]
6\t1\t2\t0\t1\t[Pos, Vel, (IsA, Ship)]
";
    assert_eq!(snap1.as_str(), expected);
}

// -----------------------------------------------------------------------------
// Reference Oracle + `blaze_sim` Property Test & Planted-Bug Control
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct RefEntity {
    alive: bool,
    pos: Option<Pos>,
    hp: Option<Health>,
    parent: Option<Entity>,
    isa: Option<Entity>,
}

struct RefWorld<A: core::alloc::Allocator + Copy> {
    entities: FlatMap<Entity, RefEntity, A>,
}

impl<A: core::alloc::Allocator + Copy> RefWorld<A> {
    fn new_in(alloc: A) -> Self {
        Self { entities: FlatMap::new_in(alloc) }
    }

    fn get_inherited_hp(&self, start: Entity) -> Option<Health> {
        let mut cur = start;
        for _ in 0..64 {
            let rec = self.entities.get(&cur)?;
            if !rec.alive {
                return None;
            }
            if let Some(hp) = rec.hp {
                return Some(hp);
            }
            cur = rec.isa?;
        }
        None
    }

    fn child_of_creates_cycle(&self, child: Entity, parent: Entity) -> bool {
        if child == parent {
            return true;
        }
        let mut cur = parent;
        for _ in 0..64 {
            let Some(rec) = self.entities.get(&cur) else {
                return false;
            };
            if !rec.alive {
                return false;
            }
            let Some(next) = rec.parent else {
                return false;
            };
            if next == child {
                return true;
            }
            cur = next;
        }
        false
    }

    fn isa_creates_cycle(&self, child: Entity, target: Entity) -> bool {
        if child == target {
            return true;
        }
        let mut cur = target;
        for _ in 0..64 {
            let Some(rec) = self.entities.get(&cur) else {
                return false;
            };
            if !rec.alive {
                return false;
            }
            let Some(next) = rec.isa else {
                return false;
            };
            if next == child {
                return true;
            }
            cur = next;
        }
        false
    }

    fn despawn_cascade(&mut self, root: Entity, alloc: A) {
        let mut queue = OwnVec::new_in(alloc);
        queue.push(root);
        let mut head = 0usize;
        while let Some(&cur) = queue.get(head) {
            head += 1;
            if let Some(rec) = self.entities.get_mut(&cur) {
                if !rec.alive {
                    continue;
                }
                rec.alive = false;
                rec.pos = None;
                rec.hp = None;
                rec.parent = None;
                rec.isa = None;
            }
            // Collect children of `cur`.
            let mut kids = OwnVec::new_in(alloc);
            for &(e, rec) in self.entities.iter() {
                if rec.alive && rec.parent == Some(cur) {
                    kids.push(e);
                }
            }
            for &k in kids.as_slice() {
                queue.push(k);
            }
        }
        // Clear dangling `isa` links pointing to dead entities.
        let mut dead_isa_holders = OwnVec::new_in(alloc);
        for &(e, rec) in self.entities.iter() {
            if rec.alive {
                if let Some(target) = rec.isa {
                    if !self.entities.get(&target).is_some_and(|t| t.alive) {
                        dead_isa_holders.push(e);
                    }
                }
            }
        }
        for &e in dead_isa_holders.as_slice() {
            if let Some(rec) = self.entities.get_mut(&e) {
                rec.isa = None;
            }
        }
    }
}

fn run_ecs_model_case(tape: &blaze_sim::Tape, plant_skip_swap_row: bool) -> Result<(), Broke> {
    let a = alloc();
    let mut world = World::new_in(a)
        .map_err(|e| Broke::Oracle(format_in!(a, "world init failed: {e}").as_str().into()))?;
    world.set_plant_skip_swap_row_update(plant_skip_swap_row);
    let _pos_id = world
        .register_copy_component::<Pos>("Pos")
        .map_err(|e| Broke::Oracle(format_in!(a, "reg Pos: {e}").as_str().into()))?;
    let _hp_id = world
        .register_copy_component::<Health>("Health")
        .map_err(|e| Broke::Oracle(format_in!(a, "reg Health: {e}").as_str().into()))?;

    let child_of = world.child_of();
    let is_a = world.is_a();

    let mut ref_world = RefWorld::new_in(a);
    let mut history = OwnVec::<Entity, Alloc<'static>>::new_in(a);

    let steps = usize::try_from(tape.int_in(4..=48, "steps")).unwrap_or(8);
    for _ in 0..steps {
        let op = tape.below(8, "op");
        match op {
            // 0: spawn
            0 => {
                let e = world
                    .spawn()
                    .map_err(|er| Broke::Oracle(format_in!(a, "spawn: {er}").as_str().into()))?;
                history.push(e);
                let _ = ref_world.entities.insert(
                    e,
                    RefEntity { alive: true, pos: None, hp: None, parent: None, isa: None },
                );
            }
            // 1: set Pos
            1 => {
                if history.is_empty() {
                    continue;
                }
                let idx = usize::try_from(tape.below(u64::try_from(history.len()).unwrap_or(1), "e")).unwrap_or(0);
                let e = *history.get(idx).unwrap_or(&Entity::NULL);
                let x = i32::try_from(tape.int_in(-50..=50, "x")).unwrap_or(0);
                let y = i32::try_from(tape.int_in(-50..=50, "y")).unwrap_or(0);
                let res = world.set(e, Pos { x, y });
                let alive = ref_world.entities.get(&e).is_some_and(|r| r.alive);
                if alive {
                    res.map_err(|er| Broke::Oracle(format_in!(a, "set Pos: {er}").as_str().into()))?;
                    if let Some(r) = ref_world.entities.get_mut(&e) {
                        r.pos = Some(Pos { x, y });
                    }
                } else if res != Err(EcsError::DeadEntity(e)) {
                    return Err(Broke::Oracle("dead entity set Pos did not return DeadEntity".into()));
                }
            }
            // 2: set Health
            2 => {
                if history.is_empty() {
                    continue;
                }
                let idx = usize::try_from(tape.below(u64::try_from(history.len()).unwrap_or(1), "e")).unwrap_or(0);
                let e = *history.get(idx).unwrap_or(&Entity::NULL);
                let hp = i32::try_from(tape.int_in(1..=200, "hp")).unwrap_or(1);
                let res = world.set(e, Health(hp));
                let alive = ref_world.entities.get(&e).is_some_and(|r| r.alive);
                if alive {
                    res.map_err(|er| Broke::Oracle(format_in!(a, "set Health: {er}").as_str().into()))?;
                    if let Some(r) = ref_world.entities.get_mut(&e) {
                        r.hp = Some(Health(hp));
                    }
                } else if res != Err(EcsError::DeadEntity(e)) {
                    return Err(Broke::Oracle("dead entity set Health did not return DeadEntity".into()));
                }
            }
            // 3: remove Pos
            3 => {
                if history.is_empty() {
                    continue;
                }
                let idx = usize::try_from(tape.below(u64::try_from(history.len()).unwrap_or(1), "e")).unwrap_or(0);
                let e = *history.get(idx).unwrap_or(&Entity::NULL);
                let res = world.remove::<Pos>(e);
                let alive = ref_world.entities.get(&e).is_some_and(|r| r.alive);
                if alive {
                    let removed = res.map_err(|er| {
                        Broke::Oracle(format_in!(a, "remove Pos: {er}").as_str().into())
                    })?;
                    let expected = ref_world.entities.get(&e).is_some_and(|r| r.pos.is_some());
                    if removed != expected {
                        return Err(Broke::Oracle("remove Pos bool mismatch".into()));
                    }
                    if let Some(r) = ref_world.entities.get_mut(&e) {
                        r.pos = None;
                    }
                } else if res != Err(EcsError::DeadEntity(e)) {
                    return Err(Broke::Oracle("dead entity remove Pos did not return DeadEntity".into()));
                }
            }
            // 4: remove Health
            4 => {
                if history.is_empty() {
                    continue;
                }
                let idx = usize::try_from(tape.below(u64::try_from(history.len()).unwrap_or(1), "e")).unwrap_or(0);
                let e = *history.get(idx).unwrap_or(&Entity::NULL);
                let res = world.remove::<Health>(e);
                let alive = ref_world.entities.get(&e).is_some_and(|r| r.alive);
                if alive {
                    let removed = res.map_err(|er| {
                        Broke::Oracle(format_in!(a, "remove Health: {er}").as_str().into())
                    })?;
                    let expected = ref_world.entities.get(&e).is_some_and(|r| r.hp.is_some());
                    if removed != expected {
                        return Err(Broke::Oracle("remove Health bool mismatch".into()));
                    }
                    if let Some(r) = ref_world.entities.get_mut(&e) {
                        r.hp = None;
                    }
                } else if res != Err(EcsError::DeadEntity(e)) {
                    return Err(Broke::Oracle("dead entity remove Health did not return DeadEntity".into()));
                }
            }
            // 5: reparent ChildOf
            5 => {
                if history.len() < 2 {
                    continue;
                }
                let n = u64::try_from(history.len()).unwrap_or(1);
                let ci = usize::try_from(tape.below(n, "c")).unwrap_or(0);
                let pi = usize::try_from(tape.below(n, "p")).unwrap_or(0);
                let c = *history.get(ci).unwrap_or(&Entity::NULL);
                let p = *history.get(pi).unwrap_or(&Entity::NULL);
                let c_alive = ref_world.entities.get(&c).is_some_and(|r| r.alive);
                let p_alive = ref_world.entities.get(&p).is_some_and(|r| r.alive);
                if !c_alive || !p_alive {
                    continue;
                }
                let would_cycle = ref_world.child_of_creates_cycle(c, p);
                let res = world.add_pair(c, child_of, p);
                if would_cycle {
                    if !matches!(res, Err(EcsError::HierarchyCycle { .. })) {
                        return Err(Broke::Oracle("ChildOf cycle not rejected".into()));
                    }
                } else {
                    res.map_err(|er| Broke::Oracle(format_in!(a, "add ChildOf: {er}").as_str().into()))?;
                    if let Some(r) = ref_world.entities.get_mut(&c) {
                        r.parent = Some(p);
                    }
                }
            }
            // 6: set single IsA link (replacing previous IsA if any so reference model is a tree)
            6 => {
                if history.len() < 2 {
                    continue;
                }
                let n = u64::try_from(history.len()).unwrap_or(1);
                let ci = usize::try_from(tape.below(n, "c")).unwrap_or(0);
                let ti = usize::try_from(tape.below(n, "t")).unwrap_or(0);
                let c = *history.get(ci).unwrap_or(&Entity::NULL);
                let t = *history.get(ti).unwrap_or(&Entity::NULL);
                let c_alive = ref_world.entities.get(&c).is_some_and(|r| r.alive);
                let t_alive = ref_world.entities.get(&t).is_some_and(|r| r.alive);
                if !c_alive || !t_alive {
                    continue;
                }
                let old_isa = ref_world.entities.get(&c).and_then(|r| r.isa);
                if let Some(old_t) = old_isa {
                    let _ = world
                        .remove_pair(c, is_a, old_t)
                        .map_err(|er| Broke::Oracle(format_in!(a, "rm IsA: {er}").as_str().into()))?;
                    if let Some(r) = ref_world.entities.get_mut(&c) {
                        r.isa = None;
                    }
                }
                let would_cycle = ref_world.isa_creates_cycle(c, t);
                let res = world.add_pair(c, is_a, t);
                if would_cycle {
                    if !matches!(res, Err(EcsError::HierarchyCycle { .. })) {
                        return Err(Broke::Oracle("IsA cycle not rejected".into()));
                    }
                } else {
                    res.map_err(|er| Broke::Oracle(format_in!(a, "add IsA: {er}").as_str().into()))?;
                    if let Some(r) = ref_world.entities.get_mut(&c) {
                        r.isa = Some(t);
                    }
                }
            }
            // 7: despawn
            _ => {
                if history.is_empty() {
                    continue;
                }
                let idx = usize::try_from(tape.below(u64::try_from(history.len()).unwrap_or(1), "e")).unwrap_or(0);
                let e = *history.get(idx).unwrap_or(&Entity::NULL);
                let alive = ref_world.entities.get(&e).is_some_and(|r| r.alive);
                let res = world.despawn(e);
                if alive {
                    res.map_err(|er| Broke::Oracle(format_in!(a, "despawn: {er}").as_str().into()))?;
                    ref_world.despawn_cascade(e, a);
                } else if res != Err(EcsError::DeadEntity(e)) {
                    return Err(Broke::Oracle("dead entity despawn did not return DeadEntity".into()));
                }
            }
        }

        // Check structural table/entity bijection invariant after EVERY step!
        world
            .verify_invariants()
            .map_err(|er| Broke::Oracle(format_in!(a, "bijection invariant failed: {er}").as_str().into()))?;

        // Check full equivalence against the reference model after EVERY step!
        for &e in history.as_slice() {
            let Some(rec) = ref_world.entities.get(&e) else {
                continue;
            };
            if world.is_alive(e) != rec.alive {
                return Err(Broke::Oracle(format_in!(a, "liveness mismatch for {e}").as_str().into()));
            }
            if !rec.alive {
                continue;
            }
            let got_pos = world
                .get::<Pos>(e)
                .map_err(|er| Broke::Oracle(format_in!(a, "get Pos: {er}").as_str().into()))?
                .copied();
            if got_pos != rec.pos {
                return Err(Broke::Oracle(
                    format_in!(a, "Pos mismatch for {e}: got {got_pos:?} vs {:?}", rec.pos)
                        .as_str()
                        .into(),
                ));
            }
            let got_hp = world
                .get::<Health>(e)
                .map_err(|er| Broke::Oracle(format_in!(a, "get Health: {er}").as_str().into()))?
                .copied();
            if got_hp != rec.hp {
                return Err(Broke::Oracle(
                    format_in!(a, "Health mismatch for {e}: got {got_hp:?} vs {:?}", rec.hp)
                        .as_str()
                        .into(),
                ));
            }
            let got_inherited_hp = world
                .get_inherited::<Health>(e)
                .map_err(|er| Broke::Oracle(format_in!(a, "get_inherited Health: {er}").as_str().into()))?
                .copied();
            let exp_inherited_hp = ref_world.get_inherited_hp(e);
            if got_inherited_hp != exp_inherited_hp {
                return Err(Broke::Oracle(
                    format_in!(
                        a,
                        "inherited Health mismatch for {e}: got {got_inherited_hp:?} vs {exp_inherited_hp:?}"
                    )
                    .as_str()
                    .into(),
                ));
            }
            let got_parent = world
                .parent_of(e)
                .map_err(|er| Broke::Oracle(format_in!(a, "parent_of: {er}").as_str().into()))?;
            if got_parent != rec.parent {
                return Err(Broke::Oracle(
                    format_in!(a, "ChildOf parent mismatch for {e}: got {got_parent:?} vs {:?}", rec.parent)
                        .as_str()
                        .into(),
                ));
            }
            let parents = world
                .targets_of(e, child_of)
                .map_err(|er| Broke::Oracle(format_in!(a, "targets_of: {er}").as_str().into()))?;
            if parents.len() > 1 {
                return Err(Broke::Oracle("ChildOf exclusivity violated".into()));
            }
        }
    }
    Ok(())
}

/// The invariant: any sequence of spawns, despawns, component sets/removals, exclusive `ChildOf`
/// reparenting, and `IsA` inheritance links preserves exact entity liveness, direct and inherited
/// component values, `ChildOf` exclusivity, and table-row bijection against the reference model.
#[test]
fn property_world_matches_reference_model_and_bijection() {
    let config = Config { first: 2026_1123, ..Config::cases(128) };
    blaze_sim::check(config, |tape| run_ecs_model_case(tape, false));
}

#[test]
fn the_oracle_catches_stale_entity_row_update() {
    let config = Config { first: 2026_1123, ..Config::cases(128) };
    blaze_sim::control(config, false, true, "", |tape, plant| {
        run_ecs_model_case(tape, plant)
    });
}
