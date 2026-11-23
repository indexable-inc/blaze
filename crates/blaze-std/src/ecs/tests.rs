use core::cell::Cell;

use blaze_own::{Alloc, Box as OwnBox, Vec as OwnVec, format_in, testing::heap};
use blaze_sim::{Broke, Config};

use super::map::FlatMap;
use super::{
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
    let empty_tbl = w.table(super::TableId(0)).expect("empty");
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
    let cb: super::ObserverCallback<Alloc<'static>> = OwnBox::new_in(
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
    let pos_id = world.component_id::<Pos>().ok_or(EcsError::UnknownComponent(super::ComponentId(0)))?;
    let vel_id = world.component_id::<Vel>().ok_or(EcsError::UnknownComponent(super::ComponentId(1)))?;
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
    let hp_id = world.component_id::<Health>().ok_or(EcsError::UnknownComponent(super::ComponentId(2)))?;
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

// -----------------------------------------------------------------------------
// Zero-String Typed Flecs 4.2 Ergonomic API Tests
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Shield(i32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Wrapper<T>(T);

struct Alive;
struct Stunned;

struct AllyRel;
impl super::Relation for AllyRel {
    const SYMMETRIC: bool = true;
}

struct LocatedInRel;
impl super::Relation for LocatedInRel {
    const TRANSITIVE: bool = true;
}

struct BaseUnit;
impl super::Prefab for BaseUnit {
    type Parent = super::NoParent;
    type Components = (Health, Armor);

    fn components() -> Self::Components {
        (Health(100), Armor(5))
    }
}

struct DragonBoss;
impl super::Prefab for DragonBoss {
    type Parent = BaseUnit;
    type Components = (Health, Shield);

    fn components() -> Self::Components {
        (Health(500), Shield(80))
    }
}

#[test]
fn short_type_name_and_lazy_auto_registration() {
    use super::typed::short_type_name_str;

    assert_eq!(short_type_name_str("my_game::Pos"), "Pos");
    assert_eq!(short_type_name_str("bstd::ecs::tests::Health"), "Health");
    assert_eq!(short_type_name_str("foo::Bar<baz::Qux>"), "Bar<baz::Qux>");
    assert_eq!(short_type_name_str("Pos"), "Pos");
    assert_eq!(super::short_type_name::<Pos>(), "Pos");
    assert_eq!(super::short_type_name::<Health>(), "Health");
    let wrapped = super::short_type_name::<Wrapper<Pos>>();
    assert!(wrapped.starts_with("Wrapper<"));
    assert!(wrapped.ends_with('>'));

    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    // Zero manual registration: first `set` auto-registers `Pos` with short name "Pos".
    let e = w.spawn().expect("e");
    w.set(e, Pos { x: 4, y: 9 }).expect("lazy set Pos");
    let pos_id = w.component_id::<Pos>().expect("Pos registered");
    assert_eq!(w.component_meta(pos_id).expect("meta").name(), "Pos");
    assert_eq!(w.component::<Pos>().expect("idempotent"), pos_id);

    // Dead entity `set` must fail with `DeadEntity` without polluting component registry.
    let dead = w.spawn().expect("dead");
    w.despawn(dead).expect("despawn");
    assert_eq!(w.set(dead, Damage(7)), Err(EcsError::DeadEntity(dead)));
    assert_eq!(w.component_id::<Damage>(), None);

    // `register` / `register_copy` short auto-named aliases reject duplicates.
    let dmg_id = w.register::<Damage>().expect("reg Damage");
    assert_eq!(w.component_meta(dmg_id).expect("meta").name(), "Damage");
    assert_eq!(w.register::<Damage>(), Err(EcsError::ComponentAlreadyRegistered(dmg_id)));

    let arm_id = w.register_copy::<Armor>().expect("reg copy Armor");
    assert!(w.component_meta(arm_id).expect("meta").can_copy);

    // `set_copy` registers copyable automatically AND upgrades previously plain `Pos` for prefabs!
    let prefab = w.spawn_prefab("Unit").expect("prefab");
    w.set_copy(prefab, Pos { x: 11, y: 22 }).expect("upgrade Pos to copyable");
    w.set_copy(prefab, Vel { dx: 1, dy: -1 }).expect("lazy copy Vel");
    let inst = w.instantiate(prefab).expect("instantiate");
    assert_eq!(w.get::<Pos>(inst).expect("pos"), Some(&Pos { x: 11, y: 22 }));
    assert_eq!(w.get::<Vel>(inst).expect("vel"), Some(&Vel { dx: 1, dy: -1 }));
    w.verify_invariants().expect("invariants");
}

#[test]
fn typed_prefab_hierarchy_fluent_entity_tags_and_relations() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    let root = w.spawn_ent().expect("root").named("Root").expect("name").id();
    let other = w.spawn_ent().expect("other").named("Companion").expect("name").id();

    let dragon = w
        .spawn_of::<DragonBoss>()
        .expect("spawn_of DragonBoss")
        .with(Pos { x: 10, y: 0 })
        .expect("with Pos")
        .child_of(root)
        .expect("child_of")
        .tag::<Alive>()
        .expect("tag Alive")
        .relate::<AllyRel>(other)
        .expect("relate Ally")
        .named("Smaug")
        .expect("named")
        .id();

    // Inherited from BaseUnit + overridden/added by DragonBoss + direct Pos.
    assert_eq!(w.get::<Health>(dragon).expect("hp"), Some(&Health(500)));
    assert_eq!(w.get::<Shield>(dragon).expect("shield"), Some(&Shield(80)));
    assert_eq!(w.get::<Armor>(dragon).expect("armor"), Some(&Armor(5)));
    assert_eq!(w.get::<Pos>(dragon).expect("pos"), Some(&Pos { x: 10, y: 0 }));
    assert_eq!(w.entity_by_name("Smaug"), Some(dragon));

    // Built-in ChildOf relation maps directly to `w.child_of()`.
    assert_eq!(w.relation_entity::<super::ChildOf>().expect("child_of"), w.child_of());
    assert_eq!(w.relation_entity::<super::IsA>().expect("is_a"), w.is_a());
    assert_eq!(w.parent_of(dragon).expect("parent"), Some(root));
    assert!(w.has_relation::<super::ChildOf>(dragon, root).expect("has ChildOf"));

    // Typed tags work cleanly.
    assert!(w.has_tag_typed::<Alive>(dragon).expect("alive"));
    assert!(!w.has_tag_typed::<Stunned>(dragon).expect("not stunned"));
    assert!(w.remove_tag_typed::<Alive>(dragon).expect("untag"));
    assert!(!w.has_tag_typed::<Alive>(dragon).expect("removed"));
    w.entity_mut(dragon).expect("mut").tag::<Alive>().expect("re-tag");
    assert!(w.has_tag_typed::<Alive>(dragon).expect("alive again"));

    // Symmetric typed relation mirrors automatically.
    assert!(w.has_relation::<AllyRel>(dragon, other).expect("d->o"));
    assert!(w.has_relation::<AllyRel>(other, dragon).expect("o->d"));
    assert!(w.unrelate::<AllyRel>(dragon, other).expect("unrelate"));
    assert!(!w.has_relation::<AllyRel>(other, dragon).expect("mirrored removal"));

    // Transitive typed relation works iteratively across multiple hops.
    let room = w.spawn().expect("room");
    let tower = w.spawn().expect("tower");
    let city = w.spawn().expect("city");
    w.relate::<LocatedInRel>(room, tower).expect("r->t");
    w.relate::<LocatedInRel>(tower, city).expect("t->c");
    assert!(w.has_transitive_relation::<LocatedInRel>(room, city).expect("transitive"));
    assert!(!w.has_transitive_relation::<LocatedInRel>(city, room).expect("not reverse"));

    // Prefab entities themselves are excluded from default `each`.
    let mut health_count = 0usize;
    w.each::<&Health>(|_| {
        health_count += 1;
    })
    .expect("each Health");
    assert_eq!(health_count, 1);

    // `spawn_bundle` / `spawn_with` insert tuples with zero prior registration.
    let bundled = w.spawn_bundle((Pos { x: 1, y: 2 }, Vel { dx: 3, dy: 4 })).expect("bundle");
    assert_eq!(w.get::<Pos>(bundled).expect("p"), Some(&Pos { x: 1, y: 2 }));
    assert_eq!(w.get::<Vel>(bundled).expect("v"), Some(&Vel { dx: 3, dy: 4 }));
    w.verify_invariants().expect("invariants");
}

#[test]
fn typed_queries_chunks_3_and_4_components_filters_and_aliasing_rejection() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    let ally_target = w.spawn().expect("ally_target");

    // Prefab with Pos + Vel must be excluded from default `each` / `each_chunk`.
    let ship_prefab = w.spawn_prefab("ShipPrefab").expect("prefab");
    w.set_copy(ship_prefab, Pos { x: 999, y: 999 }).expect("prefab pos");
    w.set_copy(ship_prefab, Vel { dx: 100, dy: 100 }).expect("prefab vel");

    // Archetype 1: (Pos, Vel) + Alive + (AllyRel, ally_target)
    let e1 = w
        .spawn_with((Pos { x: 1, y: 2 }, Vel { dx: 10, dy: 20 }))
        .expect("e1")
        .tag::<Alive>()
        .expect("alive")
        .relate::<AllyRel>(ally_target)
        .expect("ally")
        .id();

    // Archetype 2: (Pos, Vel, Health, Armor) + Alive
    let e2 = w
        .spawn_with((Pos { x: 5, y: 6 }, Vel { dx: 2, dy: 3 }, Health(40), Armor(7)))
        .expect("e2")
        .tag::<Alive>()
        .expect("alive")
        .id();

    // Archetype 3: (Pos, Vel, Health, Armor) + Alive + Stunned
    let e3 = w
        .spawn_with((Pos { x: 10, y: 10 }, Vel { dx: -1, dy: -2 }, Health(80), Armor(12)))
        .expect("e3")
        .tag::<Alive>()
        .expect("alive")
        .tag::<Stunned>()
        .expect("stunned")
        .id();

    // 1-line `each::<(&mut Pos, &Vel)>` across all 3 live archetypes (excluding prefab).
    w.each::<(&mut Pos, &Vel)>(|(p, v)| {
        p.x += v.dx;
        p.y += v.dy;
    })
    .expect("each 2");
    assert_eq!(w.get::<Pos>(e1).expect("e1"), Some(&Pos { x: 11, y: 22 }));
    assert_eq!(w.get::<Pos>(e2).expect("e2"), Some(&Pos { x: 7, y: 9 }));
    assert_eq!(w.get::<Pos>(e3).expect("e3"), Some(&Pos { x: 9, y: 8 }));
    assert_eq!(w.get::<Pos>(ship_prefab).expect("prefab untouched"), Some(&Pos { x: 999, y: 999 }));

    // `each_chunk` visits contiguous SoA slices per archetype table.
    let mut chunk_tables = 0usize;
    w.each_chunk::<(&mut Pos, &Vel)>(|(ps, vs)| {
        chunk_tables += 1;
        for (p, v) in ps.iter_mut().zip(vs.iter()) {
            p.x += v.dx;
            p.y += v.dy;
        }
    })
    .expect("each_chunk");
    assert_eq!(chunk_tables, 3);
    assert_eq!(w.get::<Pos>(e1).expect("e1"), Some(&Pos { x: 21, y: 42 }));

    // `each_with_entity` yields exact Entity handles alongside components.
    let mut visited = OwnVec::new_in(a);
    w.each_with_entity::<(&Pos, &Vel)>(|ent, _| {
        visited.push(ent);
    })
    .expect("each_with_entity");
    assert_eq!(visited.len(), 3);
    assert!(visited.as_slice().contains(&e1));
    assert!(visited.as_slice().contains(&e2));
    assert!(visited.as_slice().contains(&e3));

    // 3-component queries.
    w.each::<(&mut Health, &Pos, &Armor)>(|(hp, pos, arm)| {
        hp.0 += pos.x + arm.0;
    })
    .expect("each 3");
    assert_eq!(w.get::<Health>(e2).expect("e2 hp"), Some(&Health(40 + 9 + 7)));
    assert_eq!(w.get::<Health>(e3).expect("e3 hp"), Some(&Health(80 + 8 + 12)));

    // 4-component queries (`(&mut A, &B, &C, &D)` and `(&A, &B, &C, &D)`).
    w.each::<(&mut Pos, &Vel, &Health, &Armor)>(|(p, v, hp, arm)| {
        p.x += v.dx + hp.0 - arm.0;
    })
    .expect("each 4 mut");
    let mut sum_x = 0i32;
    w.each::<(&Pos, &Vel, &Health, &Armor)>(|(p, _, _, _)| {
        sum_x += p.x;
    })
    .expect("each 4 ref");
    assert_eq!(
        sum_x,
        w.get::<Pos>(e2).expect("e2").expect("s").x + w.get::<Pos>(e3).expect("e3").expect("s").x
    );

    // Filtered `TypedQuery` builder: `.with_tag`, `.without`, `.with_relation`, `.include_prefabs`.
    let non_stunned_alive = w
        .query::<(&Pos, &Vel)>()
        .with_tag::<Alive>()
        .expect("with Alive")
        .without::<Stunned>()
        .expect("without Stunned")
        .count()
        .expect("count");
    assert_eq!(non_stunned_alive, 2);

    let allied_count = w
        .query::<(&Pos, &Vel)>()
        .with_relation::<AllyRel>(ally_target)
        .expect("with Ally")
        .count()
        .expect("count");
    assert_eq!(allied_count, 1);

    let with_prefabs = w.query::<(&Pos, &Vel)>().include_prefabs().count().expect("count");
    assert_eq!(with_prefabs, 4);

    // Aliasing conflicts rejected with `EcsError::ConflictingAccess`.
    let pos_id = w.component_id::<Pos>().expect("Pos id");
    assert_eq!(
        w.each::<(&mut Pos, &Pos)>(|_| {}),
        Err(EcsError::ConflictingAccess { id: Id::Component(pos_id) })
    );
    assert_eq!(
        w.each::<(&Pos, &mut Pos)>(|_| {}),
        Err(EcsError::ConflictingAccess { id: Id::Component(pos_id) })
    );
    assert_eq!(
        w.each::<(&mut Pos, &mut Pos)>(|_| {}),
        Err(EcsError::ConflictingAccess { id: Id::Component(pos_id) })
    );
    assert_eq!(
        w.each::<(&mut Pos, &Vel, &Pos)>(|_| {}),
        Err(EcsError::ConflictingAccess { id: Id::Component(pos_id) })
    );
    assert_eq!(
        w.each::<(&mut Pos, &Vel, &Health, &Pos)>(|_| {}),
        Err(EcsError::ConflictingAccess { id: Id::Component(pos_id) })
    );

    // Duplicate shared reads `(&Pos, &Pos)` are valid and never conflict.
    let mut dup_read_hits = 0usize;
    w.each::<(&Pos, &Pos)>(|(a, b)| {
        assert_eq!(a, b);
        dup_read_hits += 1;
    })
    .expect("shared duplicate reads ok");
    assert_eq!(dup_read_hits, 3);
    w.verify_invariants().expect("invariants");
}

fn sys_typed_move(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    world.each::<(&mut Pos, &Vel)>(|(p, v)| {
        p.x += v.dx;
        p.y += v.dy;
    })
}

fn sys_typed_heal(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    world.each::<&mut Health>(|hp| {
        hp.0 += 5;
    })
}

fn sys_typed_audit(world: &mut World<Alloc<'static>>) -> Result<(), EcsError> {
    let mut checksum = 0i32;
    world.each::<(&Pos, &Health)>(|(p, hp)| {
        checksum += p.x + p.y + hp.0;
    })?;
    if let Some(sink) = world.entity_by_name("AuditSink") {
        world.set(sink, Damage(checksum))?;
    }
    Ok(())
}

#[test]
fn typed_pipeline_add_query_automatic_disjoint_batching() {
    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    let e = w
        .spawn_with((Pos { x: 1, y: 2 }, Vel { dx: 4, dy: 6 }, Health(20)))
        .expect("e")
        .id();
    let sink = w.spawn_named("AuditSink").expect("sink");

    let mut pipe = Pipeline::new_in(a);

    // Self-conflicting typed query rejected before adding to pipeline.
    let pos_id = w.component_id::<Pos>().expect("Pos id");
    assert_eq!(
        pipe.add_query::<(&mut Pos, &Pos)>(&mut w, Phase::OnUpdate, "bad", sys_typed_move),
        Err(EcsError::ConflictingAccess { id: Id::Component(pos_id) })
    );

    // Zero manual `.read()` / `.write()` calls: extracted automatically from `QueryFetch`!
    pipe.add_query::<(&mut Pos, &Vel)>(&mut w, Phase::OnUpdate, "move", sys_typed_move)
        .expect("add move");
    pipe.add_query::<&mut Health>(&mut w, Phase::OnUpdate, "heal", sys_typed_heal)
        .expect("add heal");
    pipe.add_query::<(&Pos, &Health)>(&mut w, Phase::OnUpdate, "audit", sys_typed_audit)
        .expect("add audit");

    let schedule = pipe.batches().expect("batches");
    assert_eq!(schedule.phases.len(), 1);
    let update = schedule.phases.get(0).expect("OnUpdate");
    // Batch 0: `move` (writes Pos, reads Vel) + `heal` (writes Health) -> disjoint parallel batch!
    // Batch 1: `audit` (reads Pos, Health) -> conflicts with both writes in batch 0!
    assert_eq!(update.batches.len(), 2);
    assert_eq!(update.batches.get(0).expect("b0").systems.len(), 2);
    assert_eq!(update.batches.get(1).expect("b1").systems.len(), 1);

    pipe.run_tick(&mut w).expect("run_tick");
    assert_eq!(w.get::<Pos>(e).expect("pos"), Some(&Pos { x: 5, y: 8 }));
    assert_eq!(w.get::<Health>(e).expect("hp"), Some(&Health(25)));
    assert_eq!(w.get::<Damage>(sink).expect("checksum"), Some(&Damage(5 + 8 + 25)));
}

// -----------------------------------------------------------------------------
// Live `bstd::data` Reflection & Sans-IO Flecs Explorer HTTP/JSON/TAB Tests
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
struct Transform2D {
    x: f32,
    y: f32,
    active: bool,
}
super::reflect_struct!(Transform2D { x: f32, y: f32, active: bool });

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Velocity2D {
    vx: i32,
    vy: i32,
}
super::reflect_struct!(Velocity2D { vx: i32, vy: i32 });

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Score(u32);
super::reflect_struct!(Score(u32));

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct OpaqueSecret(u64);

fn parse_json_body(body: &[u8]) -> crate::data::Value<Alloc<'static>> {
    crate::json::read_in(body, crate::json::Limits::SHIPPED, alloc()).expect("valid json body")
}

#[test]
fn reflect_struct_to_from_value_schema_and_partial_patch() {
    use crate::data::{Cursor, FromValue, ToValue};
    use super::{ReflectComponent, ReflectError};

    let a = alloc();

    // Schema check for named struct, newtype struct, and primitives.
    let schema = Transform2D::schema(a).expect("schema");
    assert_eq!(schema.get("kind").and_then(|v| v.as_str()), Some("struct"));
    assert_eq!(schema.get("name").and_then(|v| v.as_str()), Some("Transform2D"));
    let fields = schema.get("fields").and_then(|v| v.as_list()).expect("fields");
    assert_eq!(fields.len(), 3);
    assert_eq!(fields.get(0).and_then(|f| f.get("name")).and_then(|v| v.as_str()), Some("x"));
    assert_eq!(fields.get(0).and_then(|f| f.get("type")).and_then(|v| v.as_str()), Some("f32"));
    assert_eq!(fields.get(2).and_then(|f| f.get("name")).and_then(|v| v.as_str()), Some("active"));
    assert_eq!(fields.get(2).and_then(|f| f.get("type")).and_then(|v| v.as_str()), Some("bool"));

    let prim_schema = i32::schema(a).expect("i32 schema");
    assert_eq!(prim_schema.get("kind").and_then(|v| v.as_str()), Some("primitive"));
    assert_eq!(prim_schema.get("type").and_then(|v| v.as_str()), Some("i32"));

    let score_schema = Score::schema(a).expect("Score schema");
    assert_eq!(score_schema.get("kind").and_then(|v| v.as_str()), Some("newtype"));
    assert_eq!(score_schema.get("type").and_then(|v| v.as_str()), Some("u32"));

    // ToValue -> FromValue round-trip.
    let mut tf = Transform2D { x: 1.5, y: -2.25, active: true };
    let val = tf.to_value(a);
    let cursor = Cursor::root(&val, a);
    let decoded = Transform2D::from_value(&cursor).expect("decode");
    assert_eq!(decoded, tf);

    // Partial map patch preserves untouched fields and accepts JSON int for f32!
    let patch = parse_json_body(br#"{"x": 42}"#);
    tf.patch_value(&patch, a).expect("partial patch");
    assert_eq!(tf, Transform2D { x: 42.0, y: -2.25, active: true });

    let patch_bool = parse_json_body(br#"{"active": false}"#);
    tf.patch_value(&patch_bool, a).expect("patch bool");
    assert_eq!(tf, Transform2D { x: 42.0, y: -2.25, active: false });

    // Unknown field is strictly rejected.
    let bad_field = parse_json_body(br#"{"nope": 1}"#);
    assert!(matches!(tf.patch_value(&bad_field, a), Err(ReflectError::Shape(_))));

    // Wrong field type is strictly rejected.
    let bad_type = parse_json_body(br#"{"x": "not-a-number"}"#);
    assert!(matches!(tf.patch_value(&bad_type, a), Err(ReflectError::Shape(_))));

    // Primitives (including isize/f32/f64) patch directly.
    let mut isz: isize = 5;
    let isz_patch = parse_json_body(b"-17");
    isz.patch_value(&isz_patch, a).expect("patch isize");
    assert_eq!(isz, -17);

    let mut score = Score(10);
    score.patch_value(&parse_json_body(b"99"), a).expect("patch score");
    assert_eq!(score, Score(99));
}

#[test]
fn world_reflection_patch_fires_on_set_and_snapshots_full_graph() {
    use super::ReflectError;

    let a = alloc();
    let mut w = World::new_in(a).expect("world");

    let tf_id = w.register_reflected_copy::<Transform2D>().expect("reg Transform2D");
    let _vel_id = w.register_reflected_copy::<Velocity2D>().expect("reg Velocity2D");
    let opaque_id = w.register_copy::<OpaqueSecret>().expect("reg OpaqueSecret");

    let obs_set = w.observe(EventKind::OnSet, Id::Component(tf_id)).expect("obs set");
    let obs_add = w.observe(EventKind::OnAdd, Id::Component(tf_id)).expect("obs add");

    let ship_prefab = w.spawn_prefab("ShipPrefab").expect("prefab");
    w.set_reflected_copy(ship_prefab, Transform2D { x: 0.0, y: 0.0, active: true })
        .expect("prefab tf");
    w.set_reflected_copy(ship_prefab, Velocity2D { vx: 5, vy: -3 }).expect("prefab vel");

    let fleet = w.spawn_named("Fleet").expect("fleet");
    let ship = w.instantiate(ship_prefab).expect("instantiate");
    w.set_name(ship, "Enterprise").expect("name ship");
    let child_of = w.child_of();
    w.add_pair(ship, child_of, fleet).expect("child_of fleet");
    let hero_tag = w.spawn_named("Hero").expect("Hero tag");
    w.add_tag(ship, hero_tag).expect("tag Hero");
    w.set_copy(ship, OpaqueSecret(0xDEAD)).expect("opaque");

    assert_eq!(w.observer_events(obs_add).expect("add").len(), 2);
    let sets_before = w.observer_events(obs_set).expect("set").len();

    // In-place partial patch via `patch_component_value` mutates SoA column & fires `OnSet`.
    let patch = parse_json_body(br#"{"x": 12.5}"#);
    w.patch_component_value(ship, tf_id, &patch, a).expect("patch tf");
    assert_eq!(
        w.get::<Transform2D>(ship).expect("get"),
        Some(&Transform2D { x: 12.5, y: 0.0, active: true })
    );
    assert_eq!(w.observer_events(obs_set).expect("set").len(), sets_before + 1);
    assert_eq!(
        w.observer_events(obs_set).expect("set").last().copied(),
        Some(Event { kind: EventKind::OnSet, entity: ship, id: Id::Component(tf_id) })
    );

    // Inspect reflected vs non-reflected component.
    let inspected = w.inspect_component_value(ship, tf_id, a).expect("inspect").expect("some");
    assert_eq!(inspected.get("x").and_then(|v| v.as_f64()), Some(12.5));
    assert_eq!(
        w.inspect_component_value(ship, opaque_id, a),
        Err(ReflectError::NotReflected(opaque_id))
    );
    assert_eq!(w.inspect_component_value(fleet, tf_id, a), Ok(None));

    // `entity_to_value` captures hierarchy, prefabs, tags, relations, reflected values & `<opaque>`.
    let ship_val = w.entity_to_value(ship, a).expect("entity_to_value");
    assert_eq!(ship_val.get("name").and_then(|v| v.as_str()), Some("Enterprise"));
    assert_eq!(ship_val.get("slot").and_then(|v| v.as_u64()), Some(u64::from(ship.slot())));
    assert_eq!(ship_val.get("is_prefab").and_then(|v| v.as_bool()), Some(false));
    let parents = ship_val.get("parents").and_then(|v| v.as_list()).expect("parents");
    assert_eq!(parents.len(), 1);
    assert_eq!(parents.get(0).and_then(|v| v.as_u64()), Some(u64::from(fleet.slot())));
    let prefabs = ship_val.get("prefabs").and_then(|v| v.as_list()).expect("prefabs");
    assert_eq!(prefabs.len(), 1);
    assert_eq!(prefabs.get(0).and_then(|v| v.as_u64()), Some(u64::from(ship_prefab.slot())));
    let tags = ship_val.get("tags").and_then(|v| v.as_list()).expect("tags");
    assert!(tags.iter().any(|t| t.as_str() == Some("Hero")));
    let comps = ship_val.get("components").and_then(|v| v.as_map()).expect("components");
    assert_eq!(
        comps.get("Transform2D").and_then(|c| c.get("x")).and_then(|v| v.as_f64()),
        Some(12.5)
    );
    assert_eq!(
        comps.get("Velocity2D").and_then(|c| c.get("vx")).and_then(|v| v.as_i64()),
        Some(5)
    );
    assert_eq!(comps.get("OpaqueSecret").and_then(|v| v.as_str()), Some("<opaque>"));

    // `world_to_value` captures stats, component schemas, archetypes, entities, and prefabs.
    let world_val = w.world_to_value(a).expect("world_to_value");
    let stats = world_val.get("stats").and_then(|v| v.as_map()).expect("stats");
    assert_eq!(stats.get("prefabs").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(stats.get("components").and_then(|v| v.as_u64()), Some(3));
    let w_comps = world_val.get("components").and_then(|v| v.as_list()).expect("comps");
    assert_eq!(w_comps.len(), 3);
    assert_eq!(
        w_comps.get(0).and_then(|c| c.get("reflected")).and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        w_comps.get(2).and_then(|c| c.get("reflected")).and_then(|v| v.as_bool()),
        Some(false)
    );
    let w_prefabs = world_val.get("prefabs").and_then(|v| v.as_list()).expect("prefabs");
    assert_eq!(w_prefabs.len(), 1);
    w.verify_invariants().expect("invariants");
}

#[test]
fn explorer_api_all_endpoints_and_error_cases() {
    use super::{EXPLORER_HTML, handle_request};

    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let tf_id = w.register_reflected_copy::<Transform2D>().expect("reg tf");
    let _vel_id = w.register_reflected_copy::<Velocity2D>().expect("reg vel");
    let _score_id = w.register_reflected_copy::<Score>().expect("reg score");
    let obs_set = w.observe(EventKind::OnSet, Id::Component(tf_id)).expect("obs set");

    // 1. GET / -> embedded HTML UI
    let res_ui = handle_request(&mut w, "GET", "/", b"", a).expect("GET /");
    assert_eq!(res_ui.status, 200);
    assert_eq!(res_ui.content_type, "text/html; charset=utf-8");
    assert_eq!(res_ui.body.as_slice(), EXPLORER_HTML.as_bytes());

    // 2. OPTIONS -> 204
    let res_opt = handle_request(&mut w, "OPTIONS", "/api/world", b"", a).expect("OPTIONS");
    assert_eq!(res_opt.status, 204);
    assert!(res_opt.body.is_empty());

    // 3. Unknown route -> 404
    let res_404 = handle_request(&mut w, "GET", "/api/does-not-exist", b"", a).expect("404");
    assert_eq!(res_404.status, 404);

    // Create a parent + prefab to test POST /api/entity with prefab + parent + tags + components.
    let root = w.spawn_named("SceneRoot").expect("root");
    let unit_prefab = w.spawn_prefab("UnitPrefab").expect("prefab");
    w.set_reflected_copy(unit_prefab, Velocity2D { vx: 2, vy: 3 }).expect("prefab vel");

    let post_body = format_in!(
        a,
        r#"{{"name":"Drone1","parent":{},"prefab":{},"tags":["Active","Scout"],"components":{{"Transform2D":{{"x":10.0,"y":20.0,"active":true}},"Score":50}}}}"#,
        root.slot(),
        unit_prefab.slot(),
    );
    let res_post = handle_request(&mut w, "POST", "/api/entity", post_body.as_str().as_bytes(), a)
        .expect("POST /api/entity");
    assert_eq!(res_post.status, 201);
    let created = parse_json_body(res_post.body.as_slice());
    let drone_slot = u32::try_from(created.get("slot").and_then(|v| v.as_u64()).expect("slot"))
        .expect("u32");
    assert_eq!(created.get("name").and_then(|v| v.as_str()), Some("Drone1"));
    assert_eq!(
        created
            .get("components")
            .and_then(|c| c.get("Velocity2D"))
            .and_then(|v| v.get("vx"))
            .and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(
        created.get("components").and_then(|c| c.get("Score")).and_then(|v| v.as_u64()),
        Some(50)
    );

    // Invalid POST /api/entity returns 400 and does NOT leak an entity!
    let live_before_bad_post = w.live_count();
    let res_bad_post = handle_request(
        &mut w,
        "POST",
        "/api/entity",
        br#"{"name":"Broken","components":{"Transform2D":{"x":"bad"}}}"#,
        a,
    )
    .expect("bad POST");
    assert_eq!(res_bad_post.status, 400);
    assert_eq!(w.live_count(), live_before_bad_post);

    // 4. GET /api/entity/<slot> (200 and 404)
    let ent_path = format_in!(a, "/api/entity/{drone_slot}");
    let res_get = handle_request(&mut w, "GET", ent_path.as_str(), b"", a).expect("GET entity");
    assert_eq!(res_get.status, 200);
    assert_eq!(parse_json_body(res_get.body.as_slice()).get("name").and_then(|v| v.as_str()), Some("Drone1"));
    assert_eq!(handle_request(&mut w, "GET", "/api/entity/99999", b"", a).expect("404").status, 404);

    // 5. PATCH /api/entity/<slot>/component/<name>: partial edit + OnSet + 400 + 404
    let sets_before = w.observer_events(obs_set).expect("obs").len();
    let patch_path = format_in!(a, "/api/entity/{drone_slot}/component/Transform2D");
    let res_patch =
        handle_request(&mut w, "PATCH", patch_path.as_str(), br#"{"y": 99.5}"#, a).expect("PATCH");
    assert_eq!(res_patch.status, 200);
    let patched = parse_json_body(res_patch.body.as_slice());
    assert_eq!(
        patched
            .get("components")
            .and_then(|c| c.get("Transform2D"))
            .and_then(|v| v.get("x"))
            .and_then(|v| v.as_f64()),
        Some(10.0)
    );
    assert_eq!(
        patched
            .get("components")
            .and_then(|c| c.get("Transform2D"))
            .and_then(|v| v.get("y"))
            .and_then(|v| v.as_f64()),
        Some(99.5)
    );
    assert_eq!(w.observer_events(obs_set).expect("obs").len(), sets_before + 1);

    // PATCH 400 invalid value / invalid JSON
    assert_eq!(
        handle_request(&mut w, "PATCH", patch_path.as_str(), br#"{"x": "oops"}"#, a).expect("400").status,
        400
    );
    assert_eq!(
        handle_request(&mut w, "PATCH", patch_path.as_str(), b"{broken", a).expect("400").status,
        400
    );
    // PATCH 404 missing component / missing entity
    let missing_comp = format_in!(a, "/api/entity/{drone_slot}/component/NoSuchComp");
    assert_eq!(handle_request(&mut w, "PATCH", missing_comp.as_str(), b"{}", a).expect("404").status, 404);
    assert_eq!(
        handle_request(&mut w, "PATCH", "/api/entity/99999/component/Transform2D", b"{}", a)
            .expect("404")
            .status,
        404
    );

    // 6. POST / DELETE tags
    let tag_path = format_in!(a, "/api/entity/{drone_slot}/tag/Selected");
    let res_tag = handle_request(&mut w, "POST", tag_path.as_str(), b"", a).expect("POST tag");
    assert_eq!(res_tag.status, 200);
    assert!(
        parse_json_body(res_tag.body.as_slice())
            .get("tags")
            .and_then(|v| v.as_list())
            .expect("tags")
            .iter()
            .any(|t| t.as_str() == Some("Selected"))
    );
    let res_untag =
        handle_request(&mut w, "DELETE", tag_path.as_str(), b"", a).expect("DELETE tag");
    assert_eq!(res_untag.status, 200);
    assert!(
        !parse_json_body(res_untag.body.as_slice())
            .get("tags")
            .and_then(|v| v.as_list())
            .expect("tags")
            .iter()
            .any(|t| t.as_str() == Some("Selected"))
    );

    // 7. POST / DELETE parent + 400 cycle rejection
    let child_post = format_in!(a, r#"{{"name":"SubDrone","parent":{drone_slot}}}"#);
    let res_child = handle_request(&mut w, "POST", "/api/entity", child_post.as_str().as_bytes(), a)
        .expect("spawn child");
    assert_eq!(res_child.status, 201);
    let child_slot = u32::try_from(
        parse_json_body(res_child.body.as_slice()).get("slot").and_then(|v| v.as_u64()).expect("slot"),
    )
    .expect("u32");

    // Cycle: trying to make `root` a child of `child_slot` must fail with 400!
    let cycle_path = format_in!(a, "/api/entity/{}/parent/{child_slot}", root.slot());
    let res_cycle = handle_request(&mut w, "POST", cycle_path.as_str(), b"", a).expect("cycle");
    assert_eq!(res_cycle.status, 400);

    // Clear parent on `drone_slot`, then re-attach to `root`.
    let clear_parent = format_in!(a, "/api/entity/{drone_slot}/parent");
    let res_clear = handle_request(&mut w, "DELETE", clear_parent.as_str(), b"", a).expect("clear");
    assert_eq!(res_clear.status, 200);
    assert_eq!(
        parse_json_body(res_clear.body.as_slice())
            .get("parents")
            .and_then(|v| v.as_list())
            .expect("parents")
            .len(),
        0
    );
    let reparent = format_in!(a, "/api/entity/{drone_slot}/parent/{}", root.slot());
    assert_eq!(handle_request(&mut w, "POST", reparent.as_str(), b"", a).expect("reparent").status, 200);

    // 8. GET /api/query?with=...&without=...
    let res_q1 = handle_request(
        &mut w,
        "GET",
        "/api/query?with=Transform2D,Velocity2D&without=Dead",
        b"",
        a,
    )
    .expect("query 1");
    assert_eq!(res_q1.status, 200);
    let q1 = parse_json_body(res_q1.body.as_slice());
    assert_eq!(q1.get("count").and_then(|v| v.as_u64()), Some(1));

    let res_q2 = handle_request(
        &mut w,
        "GET",
        "/api/query?with=Transform2D&without=Scout",
        b"",
        a,
    )
    .expect("query 2");
    assert_eq!(res_q2.status, 200);
    assert_eq!(parse_json_body(res_q2.body.as_slice()).get("count").and_then(|v| v.as_u64()), Some(0));

    // 9. GET /api/world and GET /api/world.tab (TAB round-trips losslessly!)
    let res_world = handle_request(&mut w, "GET", "/api/world", b"", a).expect("GET world");
    assert_eq!(res_world.status, 200);
    let world_json = parse_json_body(res_world.body.as_slice());

    let res_tab = handle_request(&mut w, "GET", "/api/world.tab", b"", a).expect("GET world.tab");
    assert_eq!(res_tab.status, 200);
    assert_eq!(res_tab.content_type, "text/plain; charset=utf-8");
    let tab_parsed =
        crate::tab::read(res_tab.body.as_slice(), crate::tab::Limits::SHIPPED, a).expect("parse tab");
    assert_eq!(tab_parsed, world_json);

    // 10. DELETE /api/entity/<root_slot> cascades to Drone1 and SubDrone!
    let del_root = format_in!(a, "/api/entity/{}", root.slot());
    let res_del = handle_request(&mut w, "DELETE", del_root.as_str(), b"", a).expect("DELETE");
    assert_eq!(res_del.status, 200);
    assert_eq!(
        parse_json_body(res_del.body.as_slice()).get("deleted").and_then(|v| v.as_u64()),
        Some(u64::from(root.slot()))
    );
    assert_eq!(handle_request(&mut w, "GET", ent_path.as_str(), b"", a).expect("dead").status, 404);
    let child_path = format_in!(a, "/api/entity/{child_slot}");
    assert_eq!(handle_request(&mut w, "GET", child_path.as_str(), b"", a).expect("dead child").status, 404);
    assert_eq!(handle_request(&mut w, "DELETE", del_root.as_str(), b"", a).expect("404 del").status, 404);
    w.verify_invariants().expect("invariants");
}

#[test]
fn explorer_http_1_1_wire_framing_round_trip() {
    use super::handle_http_request;

    let a = alloc();
    let mut w = World::new_in(a).expect("world");
    let _ = w.register_reflected_copy::<Transform2D>().expect("reg");
    let e = w.spawn_named("Beacon").expect("beacon");
    w.set_reflected_copy(e, Transform2D { x: 1.0, y: 2.0, active: true }).expect("set tf");

    // 1. Raw HTTP/1.1 PATCH request with Content-Length header & JSON body.
    let patch_body = br#"{"x":7.5,"active":false}"#;
    let raw_patch = format_in!(
        a,
        "PATCH /api/entity/{}/component/Transform2D HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        e.slot(),
        patch_body.len(),
        core::str::from_utf8(patch_body).expect("utf8"),
    );
    let raw_resp =
        handle_http_request(&mut w, raw_patch.as_str().as_bytes(), a).expect("http patch");
    let resp_str = core::str::from_utf8(raw_resp.as_slice()).expect("utf8 resp");
    assert!(resp_str.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(resp_str.contains("Content-Type: application/json\r\n"));
    assert!(resp_str.contains("Access-Control-Allow-Origin: *\r\n"));
    let (_, resp_body) = resp_str.split_once("\r\n\r\n").expect("header/body split");
    let parsed = parse_json_body(resp_body.as_bytes());
    assert_eq!(
        parsed
            .get("components")
            .and_then(|c| c.get("Transform2D"))
            .and_then(|t| t.get("x"))
            .and_then(|v| v.as_f64()),
        Some(7.5)
    );
    assert_eq!(
        w.get::<Transform2D>(e).expect("get"),
        Some(&Transform2D { x: 7.5, y: 2.0, active: false })
    );

    // 2. Raw HTTP/1.1 GET / serves HTML UI with accurate Content-Length.
    let raw_get = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let html_resp = handle_http_request(&mut w, raw_get, a).expect("http get /");
    let html_str = core::str::from_utf8(html_resp.as_slice()).expect("utf8 html");
    assert!(html_str.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(html_str.contains("Content-Type: text/html; charset=utf-8\r\n"));
    assert!(html_str.contains("Blaze ECS Explorer"));

    // 3. Malformed HTTP returns 400 Bad Request cleanly.
    let bad_resp = handle_http_request(&mut w, b"NOT_HTTP", a).expect("bad http");
    let bad_str = core::str::from_utf8(bad_resp.as_slice()).expect("utf8 bad");
    assert!(bad_str.starts_with("HTTP/1.1 400 Bad Request\r\n"));
}
