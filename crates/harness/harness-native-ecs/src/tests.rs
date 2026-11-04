use super::*;
use std::rc::Rc;

#[derive(Default, Clone, Debug, PartialEq)]
struct Name(String);

#[derive(Default, Clone, Copy, Debug, PartialEq)]
struct Tag;

#[derive(Default, Clone, Copy)]
struct InTurn;

#[derive(Default, Clone)]
struct Counted(#[allow(dead_code)] Option<Rc<()>>);

#[test]
fn components_own_heap_data_and_survive_table_moves() {
    let mut s = Store::new();
    let e = s.spawn();
    s.set(e, Name("hello".repeat(20)));
    // adding more components moves the entity between tables (move hooks)
    s.add::<Tag>(e);
    s.set(e, 5u32);
    assert_eq!(s.get::<Name>(e).unwrap().0, "hello".repeat(20));
    assert!(s.has::<Tag>(e));
    assert_eq!(s.get::<u32>(e), Some(&5));
    s.get_mut::<Name>(e).unwrap().0.push('!');
    s.remove::<Tag>(e);
    assert!(s.get::<Name>(e).unwrap().0.ends_with('!'));
    assert!(!s.has::<Tag>(e));
}

#[test]
fn drops_run_for_removed_replaced_and_deleted_values() {
    let rc = Rc::new(());
    let mut s = Store::new();
    let parent = s.spawn();
    let mut kids = vec![];
    for _ in 0..50 {
        let k = s.spawn_under(parent);
        s.set(k, Counted(Some(rc.clone())));
        kids.push(k);
    }
    assert_eq!(Rc::strong_count(&rc), 51);
    s.set(kids[0], Counted(None));
    assert_eq!(Rc::strong_count(&rc), 50);
    s.remove::<Counted>(kids[1]);
    assert_eq!(Rc::strong_count(&rc), 49);
    s.despawn(parent);
    assert_eq!(Rc::strong_count(&rc), 1);
    assert!(!s.is_alive(kids[5]));
    let e = s.spawn();
    s.set(e, Counted(Some(rc.clone())));
    drop(s);
    assert_eq!(Rc::strong_count(&rc), 1);
}

#[test]
fn ordered_children_keep_insertion_order_and_reorder() {
    let mut s = Store::new();
    let root = s.spawn();
    let kids: Vec<Entity> = (0..10)
        .map(|i| {
            let k = s.spawn_under(root);
            s.set(k, i as u32);
            // a component change on a child must not reorder it
            if i % 2 == 0 {
                s.add::<Tag>(k);
            }
            k
        })
        .collect();
    assert_eq!(s.children(root), &kids[..]);
    assert_eq!(s.parent(kids[3]), Some(root));
    s.despawn(kids[4]);
    let mut left: Vec<Entity> = kids.iter().copied().filter(|k| *k != kids[4]).collect();
    assert_eq!(s.children(root), &left[..]);
    left.reverse();
    s.set_child_order(root, &left);
    assert_eq!(s.children(root), &left[..]);
    let other = s.spawn();
    s.reparent(left[0], other);
    assert_eq!(s.children(other), &[left[0]]);
    assert_eq!(s.children(root).len(), 8);
    assert!(s.children(kids[0]).is_empty());
}

#[test]
fn relationships_and_each() {
    let mut s = Store::new();
    s.exclusive::<InTurn>();
    let t1 = s.spawn();
    let t2 = s.spawn();
    let item = s.spawn();
    s.relate::<InTurn>(item, t1);
    s.relate::<InTurn>(item, t2);
    assert_eq!(s.target::<InTurn>(item), Some(t2));
    s.set(item, Name("x".into()));
    let other = s.spawn();
    s.set(other, Name("y".into()));
    let mut all = s.each::<Name>();
    all.sort();
    let mut want = vec![item, other];
    want.sort();
    assert_eq!(all, want);
}

#[test]
fn many_worlds_on_many_threads() {
    let hs: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                let mut s = Store::new();
                let root = s.spawn();
                for j in 0..200u32 {
                    let k = s.spawn_under(root);
                    s.set(k, Name(format!("{i}-{j}")));
                }
                s.children(root)
                    .iter()
                    .map(|k| s.get::<Name>(*k).unwrap().0.clone())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for (i, h) in hs.into_iter().enumerate() {
        let names = h.join().unwrap();
        assert_eq!(names.len(), 200);
        assert_eq!(names[199], format!("{i}-199"));
    }
}

#[test]
fn stale_keys_read_as_nothing() {
    let mut s = Store::new();
    let root = s.spawn();
    let kid = s.spawn_under(root);
    s.set(kid, Name("k".into()));
    s.despawn(root);
    assert!(!s.is_alive(kid));
    assert_eq!(s.get::<Name>(kid), None);
    assert!(!s.has::<Name>(kid));
    assert!(s.children(root).is_empty());
    assert_eq!(s.parent(kid), None);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn a_store_at_home_answers_calls_from_other_threads_in_order() {
    let home = Home::spawn("world", || {
        let mut s = Store::new();
        let root = s.spawn();
        (s, root)
    })
    .unwrap();
    let local = String::from("borrowed");
    let n = home.call(|(s, root)| {
        let e = s.spawn_under(*root);
        s.set(e, Name(local.clone()));
        s.children(*root).len()
    });
    assert_eq!(n, 1);
    let workers: Vec<_> = (0..4)
        .map(|i| {
            let home = home.clone();
            std::thread::spawn(move || {
                for j in 0..50 {
                    home.call(|(s, root)| {
                        let e = s.spawn_under(*root);
                        s.set(e, Name(format!("{i}-{j}")));
                    });
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(home.call(|(s, root)| s.children(*root).len()), 201);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        home.call(|_| panic!("inside"));
    }));
    assert!(caught.is_err());
    assert_eq!(
        home.call(|(s, root)| s.children(*root).len()),
        201,
        "the thread lives on"
    );
}

/// C9: each of these used to reach a flecs check, which aborts the process from inside C where no
/// `catch_unwind` reaches. Now each is refused in Rust: `try_` returns the error, the plain form
/// panics with it (caught here), and the world goes on.
#[test]
fn writes_to_stale_entities_are_refused_not_aborted() {
    let mut s = Store::new();
    s.exclusive::<InTurn>();
    let live = s.spawn();
    let gone = s.spawn_under(live);
    s.set(gone, Name("x".into()));
    s.despawn(gone);
    let none = Entity::default();
    for bad in [gone, none] {
        let refused = StoreError::NotAlive(bad);
        assert_eq!(s.try_set(bad, Name("y".into())), Err(refused));
        assert_eq!(s.try_set(bad, Tag), Err(refused));
        assert_eq!(s.try_add::<Tag>(bad), Err(refused));
        assert_eq!(s.try_add::<u32>(bad), Err(refused));
        assert_eq!(s.try_ensure::<Name>(bad).err(), Some(refused));
        assert_eq!(s.try_relate::<InTurn>(bad, live), Err(refused));
        assert_eq!(s.try_relate::<InTurn>(live, bad), Err(refused));
        assert_eq!(s.try_spawn_under(bad), Err(refused));
        assert_eq!(s.try_reparent(bad, live), Err(refused));
        assert_eq!(s.try_reparent(live, bad), Err(refused));
        assert_eq!(s.try_set_child_order(bad, &[]), Err(refused));
        // removing from something gone has nothing to do
        s.remove::<Name>(bad);
        s.unrelate::<InTurn>(bad, live);
        s.unrelate::<InTurn>(live, bad);
        s.despawn(bad);
        s.despawn_children(bad);
    }
    type Write = Box<dyn Fn(&mut Store)>;
    let writers: Vec<(&str, Write)> = vec![
        (
            "set",
            Box::new(move |s: &mut Store| s.set(gone, Name("y".into()))),
        ),
        ("set tag", Box::new(move |s: &mut Store| s.set(gone, Tag))),
        ("add", Box::new(move |s: &mut Store| s.add::<u32>(gone))),
        (
            "ensure",
            Box::new(move |s: &mut Store| s.ensure::<Name>(gone).0.push('!')),
        ),
        (
            "relate",
            Box::new(move |s: &mut Store| s.relate::<InTurn>(gone, live)),
        ),
        (
            "relate to",
            Box::new(move |s: &mut Store| s.relate::<InTurn>(live, gone)),
        ),
        (
            "spawn_under",
            Box::new(move |s: &mut Store| {
                s.spawn_under(gone);
            }),
        ),
        (
            "reparent",
            Box::new(move |s: &mut Store| s.reparent(gone, live)),
        ),
        (
            "reparent to",
            Box::new(move |s: &mut Store| s.reparent(live, gone)),
        ),
        (
            "set_child_order",
            Box::new(move |s: &mut Store| s.set_child_order(gone, &[])),
        ),
    ];
    for (name, write) in &writers {
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(&mut s)));
        let message = caught.expect_err(name);
        let text = message
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or_default();
        assert!(text.contains("not a live entity"), "{name}: {text}");
    }
    let kid = s.spawn_under(live);
    s.set(kid, Name("after".into()));
    assert_eq!(s.children(live), &[kid]);
    assert_eq!(s.get::<Name>(kid).unwrap().0, "after");
}

/// An id that is alive in flecs but no entity of this store (flecs's own entities, a component,
/// an entity of another store) is refused too: writing to or deleting one aborts in flecs.
#[test]
fn ids_that_are_not_our_entities_are_refused() {
    let mut big = Store::new();
    let far = (0..2000).map(|_| big.spawn()).last().unwrap();
    let mut s = Store::new();
    let name = s.component::<Name>();
    // SAFETY of the test: `EcsChildOf` is an immutable flecs global.
    let builtin = Entity(unsafe { sys::EcsChildOf });
    for bad in [far, Entity(name), builtin] {
        assert_eq!(s.try_set(bad, 7u32), Err(StoreError::NotAlive(bad)));
        assert_eq!(s.try_spawn_under(bad), Err(StoreError::NotAlive(bad)));
        s.despawn(bad);
        s.remove::<Name>(bad);
    }
    let e = s.spawn();
    s.set(e, Name("still a component".into()));
    assert_eq!(s.get::<Name>(e).unwrap().0, "still a component");
}

/// C9: a parent keeps its OrderedChildren tag after its last child goes; a new child must still
/// append in order.
#[test]
fn spawn_under_a_parent_whose_children_all_went() {
    let mut s = Store::new();
    let p = s.spawn();
    let a = s.spawn_under(p);
    let b = s.spawn_under(p);
    s.despawn(a);
    s.despawn(b);
    assert!(s.children(p).is_empty());
    let c = s.spawn_under(p);
    let d = s.spawn_under(p);
    assert_eq!(s.children(p), &[c, d]);
    s.despawn_children(p);
    assert!(s.children(p).is_empty());
    let e = s.spawn_under(p);
    assert_eq!(s.children(p), &[e]);
    // the last child moved away rather than deleted
    let other = s.spawn();
    s.reparent(e, other);
    assert!(s.children(p).is_empty());
    let f = s.spawn_under(p);
    assert_eq!(s.children(p), &[f]);
    s.set_child_order(p, &[f]);
    assert_eq!(s.children(other), &[e]);
}

#[test]
fn parent_cycles_are_refused() {
    let mut s = Store::new();
    let a = s.spawn();
    let b = s.spawn_under(a);
    let c = s.spawn_under(b);
    for parent in [a, b, c] {
        assert_eq!(
            s.try_reparent(a, parent),
            Err(StoreError::Cycle { child: a, parent })
        );
    }
    assert_eq!(s.children(a), &[b]);
    // a legal move still works
    s.reparent(c, a);
    assert_eq!(s.children(a), &[b, c]);
}

/// flecs aborts past `MAX_DEPTH` ancestors; the store refuses first, for a spawn and for a move
/// that would push a subtree too deep.
#[test]
fn hierarchies_stop_at_max_depth() {
    let mut s = Store::new();
    let root = s.spawn();
    let mut deepest = root;
    for _ in 0..MAX_DEPTH {
        deepest = s.spawn_under(deepest);
    }
    assert_eq!(
        s.try_spawn_under(deepest),
        Err(StoreError::TooDeep { parent: deepest })
    );
    // a two-level subtree fits under the second-deepest parent only if it lands within the cap
    let top = s.spawn();
    let below = s.spawn_under(top);
    let parent = s.parent(deepest).unwrap();
    assert_eq!(
        s.try_reparent(top, parent),
        Err(StoreError::TooDeep { parent })
    );
    let grand = s.parent(parent).unwrap();
    s.reparent(top, grand);
    assert_eq!(s.parent(below), Some(top));
    assert_eq!(
        s.try_spawn_under(below),
        Err(StoreError::TooDeep { parent: below })
    );
    // the world still works after the refusals
    s.set(deepest, Name("deep".into()));
    assert_eq!(s.get::<Name>(deepest).unwrap().0, "deep");
}

#[test]
fn child_order_must_be_exactly_the_children() {
    let mut s = Store::new();
    let childless = s.spawn();
    s.set_child_order(childless, &[]);
    let p = s.spawn();
    let a = s.spawn_under(p);
    let b = s.spawn_under(p);
    let stranger = s.spawn();
    for bad in [&[a][..], &[a, a], &[a, stranger], &[a, b, stranger], &[]] {
        assert_eq!(
            s.try_set_child_order(p, bad),
            Err(StoreError::NotItsChildren(p))
        );
    }
    s.set_child_order(p, &[b, a]);
    assert_eq!(s.children(p), &[b, a]);
}

#[test]
fn a_late_exclusive_panics_instead_of_doing_nothing() {
    let mut s = Store::new();
    let a = s.spawn();
    let b = s.spawn();
    s.relate::<InTurn>(a, b);
    let late = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.exclusive::<InTurn>()));
    assert!(late.is_err());
}

/// F1: flecs storage is 16-aligned on 64-bit targets (8 on wasm32, where a 16-aligned component
/// does not compile; see the compile_fail example on `MAX_ALIGN`).
#[cfg(target_pointer_width = "64")]
#[test]
fn components_at_max_align_are_stored_aligned() {
    #[derive(Default, Clone, Debug, PartialEq)]
    #[repr(align(16))]
    struct Wide(u64, u8);
    assert_eq!(MAX_ALIGN, 16);
    let mut s = Store::new();
    let es: Vec<Entity> = (0..3000).map(|_| s.spawn()).collect();
    for (i, &e) in es.iter().enumerate() {
        s.set(e, Wide(i as u64, 1));
        if i % 3 == 0 {
            s.set(e, i as u32); // other tables, and moves between them
        }
        if i % 5 == 0 {
            s.add::<Tag>(e);
        }
    }
    for (i, &e) in es.iter().enumerate() {
        let w = s.get::<Wide>(e).unwrap();
        assert_eq!(std::ptr::from_ref(w) as usize % 16, 0, "entity {i}");
        assert_eq!(w.0, i as u64);
    }
}

#[test]
fn dynamic_components_are_bytes_keyed_by_name_and_survive_reregistering() {
    let mut s = Store::new();
    let e = s.spawn();
    let id = s.register_dynamic("demo_count", 8, 4, 7).unwrap();
    s.dynamic_set(e, id, &[1, 0, 0, 0, 2, 0, 0, 0]).unwrap();
    // The same name and layout is the same handle and the data is still there.
    assert_eq!(s.register_dynamic("demo_count", 8, 4, 7), Ok(id));
    assert_eq!(s.dynamic_get(e, id), Some(&[1, 0, 0, 0, 2, 0, 0, 0][..]));
    assert_eq!(s.dynamic_each(id), vec![e]);
    assert_eq!(
        s.dynamic_set(e, id, &[1]),
        Err(DynamicError::WrongSize { want: 8, got: 1 })
    );
    assert!(matches!(
        s.register_dynamic("demo_count", 12, 4, 9),
        Err(DynamicError::LayoutChanged { .. })
    ));
    assert_eq!(
        s.dynamic_get(e, id).unwrap().len(),
        8,
        "a refused layout changes nothing"
    );
    assert!(matches!(
        s.register_dynamic("not a name", 4, 4, 1),
        Err(DynamicError::BadName(_))
    ));
    assert!(matches!(
        s.register_dynamic("odd", 4, 3, 1),
        Err(DynamicError::BadLayout { .. })
    ));
}

#[test]
fn a_relayout_replaces_every_value_in_one_step() {
    let mut s = Store::new();
    let (a, b, c) = (s.spawn(), s.spawn(), s.spawn());
    let id = s.register_dynamic("c", 4, 4, 1).unwrap();
    for e in [a, b, c] {
        s.dynamic_set(e, id, &[9, 0, 0, 0]).unwrap();
    }
    // c is not converted: it loses the component.
    s.dynamic_relayout(id, 8, 4, 2, &[(a, vec![1; 8]), (b, vec![2; 8])])
        .unwrap();
    assert_eq!(s.dynamic_info(id).unwrap().size, 8);
    assert_eq!(s.dynamic_get(a, id), Some(&[1; 8][..]));
    assert_eq!(s.dynamic_get(b, id), Some(&[2; 8][..]));
    assert_eq!(s.dynamic_get(c, id), None);
    // A wrong-sized value is refused before anything changes.
    assert!(s.dynamic_relayout(id, 4, 4, 3, &[(a, vec![0; 8])]).is_err());
    assert_eq!(s.dynamic_info(id).unwrap().size, 8);
}
