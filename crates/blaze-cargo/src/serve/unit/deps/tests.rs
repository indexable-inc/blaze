use std::sync::atomic::{AtomicU32, Ordering};

use blaze_core::{
    FileName, run_until_idle,
    sim::{Faults, Sim},
};

use super::*;

fn opened(sim: &mut Sim) -> blaze_cas::Store {
    let config = blaze_cas::Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 40 };
    let mut effects = Vec::new();
    let mut store = blaze_cas::Store::open(config, &mut effects);
    run_until_idle(sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    });
    assert!(store.ready(), "{:?}", store.broken());
    store
}

fn key(n: u8) -> ContentId {
    ContentId::of("blaze v2 serve deps test", &[n])
}

#[test]
fn an_image_is_put_once_for_the_loop_and_other_bytes_under_its_key_are_an_error() {
    let mut sim = Sim::new(1, Faults::default());
    let mut store = opened(&mut sim);
    let reader = store.reader();
    let images = Images::new(reader.clone());
    assert!(images.get(&key(1)).unwrap().is_none());
    images.put(key(1), b"image one".to_vec()).unwrap();
    images.put(key(1), b"image one".to_vec()).unwrap();
    assert!(images.put(key(1), b"another".to_vec()).unwrap_err().contains("two images"));
    images.put(key(2), b"image two".to_vec()).unwrap();
    let puts = images.take_puts();
    assert!(images.take_puts().is_empty(), "a take empties what was asked for");
    let keys: Vec<Key> = puts.iter().map(|it| it.key).collect();
    assert_eq!(keys, [Key(key(1)), Key(key(2))]);
    assert!(puts.iter().all(|it| it.kind == Kind::Query));

    // Once the loop has put them, they are read in place and not asked for again.
    for put in puts {
        store.put(put.key, put.kind, put.bytes).unwrap();
    }
    let images = Images::new(reader);
    let found = images.get(&key(1)).unwrap().expect("put by the loop");
    assert_eq!(found.bytes(), b"image one");
    images.put(key(2), b"image two".to_vec()).unwrap();
    assert!(images.put(key(2), b"changed".to_vec()).unwrap_err().contains("other bytes"));
    assert!(images.puts.into_inner().is_empty());
}

#[test]
fn blaze_dep_images_is_check_verify_or_unset() {
    let env = |value: &str| vec![("PATH".to_owned(), "/bin".to_owned()), (IMAGES.to_owned(), value.to_owned())];
    assert_eq!(mode_in(&[]).unwrap(), Mode::Serve);
    assert_eq!(mode_in(&env("check")).unwrap(), Mode::Check);
    assert_eq!(mode_in(&env("verify")).unwrap(), Mode::Verify);
    let error = mode_in(&env("off")).unwrap_err();
    assert!(error.what.starts_with("BLAZE_DEP_IMAGES=off is not a value blaze takes"), "{}", error.what);
}

#[test]
fn a_build_that_serves_is_never_failed_for_differences() {
    let kept = Kept { served: Served::default(), mode: Mode::Serve };
    assert!(differences(&kept).is_ok());
}

#[test]
fn every_task_of_a_batch_runs_once_on_the_pool() {
    let ran: Vec<AtomicU32> = (0..200).map(|_| AtomicU32::new(0)).collect();
    on_pool(ran.len(), &|at| _ = ran[at].fetch_add(1, Ordering::Relaxed));
    assert!(ran.iter().all(|it| it.load(Ordering::Relaxed) == 1));
    on_pool(0, &|_| panic!("no task to run"));
}

#[test]
fn a_task_that_would_wait_on_itself_runs_again_and_another_panic_goes_on() {
    let ran: Vec<AtomicU32> = (0..8).map(|_| AtomicU32::new(0)).collect();
    on_pool(ran.len(), &|at| {
        if ran[at].fetch_add(1, Ordering::Relaxed) == 0 && at % 3 == 0 {
            std::panic::panic_any(blaze_query::db::Cancelled::WouldWaitOnItself { asked: None });
        }
    });
    let runs: Vec<u32> = ran.iter().map(|it| it.load(Ordering::Relaxed)).collect();
    assert_eq!(runs, [2, 1, 1, 2, 1, 1, 2, 1]);

    let caught = std::panic::catch_unwind(|| on_pool(4, &|at| assert_ne!(at, 2, "crate two")));
    let panic = caught.expect_err("the panic goes on");
    let message = panic.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
    assert!(message.contains("crate two"), "{message}");
}

#[test]
fn the_goal_note_names_every_count_that_kept_goals_out() {
    let plain = goal_store::Flushed { written: 3, kept: 5, ..goal_store::Flushed::default() };
    assert_eq!(flushed(&plain).message, "the persistent goal cache, of the last build: 3 written, 5 kept, 0 with every slot full");
    let out = goal_store::Flushed {
        slots_full: 1,
        inexpressible: vec![("impl_datum", 4), ("crate_env", 2)],
        too_long: 7,
        ..goal_store::Flushed::default()
    };
    let note = flushed(&out);
    assert_eq!(note.level, Level::Note);
    assert_eq!(
        note.message,
        "the persistent goal cache, of the last build: 0 written, 0 kept, 1 with every slot full; \
         goals kept out by `impl_datum` (4), `crate_env` (2); 7 too long to keep"
    );
}

#[test]
fn blaze_item_store_is_verify_off_or_unset() {
    let env = |value: &str| vec![(ITEMS.to_owned(), value.to_owned())];
    assert_eq!(items_in(&[]).unwrap(), Some(body_store::Mode::Serve));
    assert_eq!(items_in(&env("verify")).unwrap(), Some(body_store::Mode::Verify));
    assert_eq!(items_in(&env("off")).unwrap(), None);
    let error = items_in(&env("check")).unwrap_err();
    assert!(error.what.starts_with("BLAZE_ITEM_STORE=check is not a value blaze takes"), "{}", error.what);
}

#[test]
fn the_note_adds_the_item_store_counts_after_the_goals() {
    let goals = goal_store::Flushed { written: 3, kept: 5, ..goal_store::Flushed::default() };
    let alone = noted(Some(&Flushes { goals, items: None }), None).unwrap();
    assert_eq!(alone.message, "the persistent goal cache, of the last build: 3 written, 5 kept, 0 with every slot full");
    let items = body_store::Flushed {
        written: 2,
        kept: 9,
        refused: vec![("crate_env", 3)],
        no_identity: 4,
        ..body_store::Flushed::default()
    };
    let goals = goal_store::Flushed { written: 3, kept: 5, ..goal_store::Flushed::default() };
    let steps = [
        body_store::StepCounts { hits: 6, ran: 1, refused: 0 },
        body_store::StepCounts { hits: 5, ran: 2, refused: 1 },
        body_store::StepCounts::default(),
    ];
    let note = noted(Some(&Flushes { goals, items: Some(items) }), Some(&steps)).unwrap();
    assert_eq!(
        note.message,
        "the persistent goal cache, of the last build: 3 written, 5 kept, 0 with every slot full; \
         the item store: 2 written, 9 kept, 0 with every slot full; steps kept out by reads of \
         `crate_env` (3); 4 of bodies with no identity; the item store, this build: inference 6 hit, \
         1 ran, 0 refused; MIR 5 hit, 2 ran, 1 refused; borrow check 0 hit, 0 ran, 0 refused"
    );
    assert_eq!(note.rendered, format!("note: {}\n", note.message));
}

#[test]
fn a_first_build_notes_its_steps_alone() {
    assert!(noted(None, None).is_none());
    let steps = [body_store::StepCounts { hits: 0, ran: 3, refused: 0 }; 3];
    let note = noted(None, Some(&steps)).unwrap();
    assert_eq!(
        note.message,
        "the item store, this build: inference 0 hit, 3 ran, 0 refused; MIR 0 hit, 3 ran, 0 refused; \
         borrow check 0 hit, 3 ran, 0 refused"
    );
}
