//! A lazy build ([`Build::run_later`]): every function walked and checked, only the entry and
//! what the start sections name compiled, the rest by [`Build::compile_one`] as a whole build
//! would have compiled them.

use std::sync::atomic::Ordering;

use super::*;

fn live() -> Policy {
    policy(OptLevel::O0, Mode::Live)
}

fn counts(machine: &Machine) -> (usize, usize) {
    (machine.prepared.load(Ordering::Relaxed), machine.emitted.load(Ordering::Relaxed))
}

#[test]
fn a_lazy_build_checks_every_function_and_compiles_only_its_entry() {
    let (program, policy) = (program(), live());
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let built = Build::new(&machine, &program, &policy, &memo).run_later(&[(1, member())], &[], &[0], &store).unwrap();
    assert_eq!(built.entry, built.roots);
    // The walk's order: a level at a time, each in the order its callers name it.
    let items: Vec<ItemKey> = built.walked.iter().map(|it| it.item).collect();
    assert_eq!(items, [1, 2, 3, 4, 5].map(item));
    assert!(built.walked.iter().all(|it| built.inputs.contains(&(it.key, it.code))));
    assert_eq!(built.functions.len(), 1);
    assert_eq!(built.functions[0].key, built.entry[0]);
    // Every function's IR was built for its check; only the entry's was compiled.
    assert_eq!(counts(&machine), (5, 1));
    assert_eq!(built.stats.instances, 5);
}

#[test]
fn compile_one_gives_what_a_whole_build_compiles() {
    let (program, policy) = (program(), live());
    let whole = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap();
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    let before = counts(&machine);
    for walked in &built.walked {
        let function = build.compile_one(walked.key, &store).unwrap();
        let want = whole.functions.iter().find(|it| it.key == walked.key).unwrap();
        assert_eq!(&function, want);
    }
    // The entry is the memo's, by the stored key its compile ahead landed under; 2's IR again
    // and its compile; 3 takes 2's template by the shape its check kept, building nothing; 4
    // and 5 compile.
    let after = counts(&machine);
    assert_eq!((after.0 - before.0, after.1 - before.1), (3, 3));
}

#[test]
fn what_compile_kept_keeps_makes_the_next_lazy_build_compile_nothing() {
    let (program, policy) = (program(), live());
    let mut store = Store::default();
    {
        let (machine, memo) = (Machine::default(), Memo::new());
        let mut build = Build::new(&machine, &program, &policy, &memo);
        let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
        store.keep(&built.puts);
        for walked in &built.walked {
            let (_, puts) = build.compile_kept(walked.key, &store).unwrap();
            store.keep(&puts);
        }
    }
    let warm = Machine::default();
    let built = Build::new(&warm, &program, &policy, &Memo::new()).run_later(&[(1, member())], &[], &[0], &store).unwrap();
    assert_eq!(counts(&warm), (0, 0));
    assert_eq!(built.stats.stored, 1);
    assert_eq!(built.walked.len(), 5);
}

#[test]
fn a_function_that_fails_its_check_fails_the_lazy_build_as_it_fails_a_whole_one() {
    let policy = live();
    let mut program = program();
    program.functions.get_mut(&4).unwrap().body = FAILS_ITS_CHECK;
    let whole = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap_err();
    let lazy = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run_later(&[(1, member())], &[], &[0], &Store::default())
        .unwrap_err();
    assert_eq!(lazy, whole);
    assert_eq!(lazy.len(), 1);
    assert_eq!(lazy[0].item, item(4));
}

#[test]
fn what_a_start_section_names_is_compiled_ahead_and_nothing_else_is() {
    let policy = live();
    let mut program = program();
    program.statics.insert(7, (0xaa, Some(".init_array".to_owned())));
    program.static_calls.insert(7, vec![5]);
    program.statics.insert(8, (0xbb, None));
    program.static_calls.insert(8, vec![4]);
    let built = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run_later(&[(1, member())], &[7, 8], &[0], &Store::default())
        .unwrap();
    let key = |id: u32| built.walked.iter().find(|it| it.item == item(id)).unwrap().key;
    let mut compiled: Vec<InstanceKey> = built.functions.iter().map(|it| it.key).collect();
    compiled.sort_unstable();
    let mut want = vec![key(1), key(5)];
    want.sort_unstable();
    assert_eq!(compiled, want);
    assert_eq!(built.statics.len(), 2);
    assert!(runs_at_start(".init_array.00099") && runs_at_start("__DATA,__mod_init_func"));
    assert!(!runs_at_start(".init_arrays") && !runs_at_start(".data"));
}

#[test]
fn a_build_whose_output_is_a_file_is_never_lazy() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Output));
    let machine = Machine::default();
    let errors = Build::new(&machine, &program, &policy, &Memo::new())
        .run_later(&[(1, member())], &[], &[0], &Store::default())
        .unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].what.contains("compiled whole"), "{}", errors[0].what);
    assert_eq!(counts(&machine), (0, 0));
}

#[test]
fn an_entry_that_is_no_root_is_an_error() {
    let (program, policy) = (program(), live());
    let errors = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run_later(&[(1, member())], &[], &[3], &Store::default())
        .unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].what.contains("root 3, of 1"), "{}", errors[0].what);
}

#[test]
fn compile_one_names_what_it_cannot_compile_alone() {
    let (program, release) = (program(), policy(OptLevel::O3, Mode::Output));
    let (machine, memo) = (Machine::default(), Memo::new());
    let mut build = Build::new(&machine, &program, &release, &memo);
    let built = build.run(&[(1, member())], &Store::default()).unwrap();
    let error = build.compile_one(built.roots[0], &Store::default()).unwrap_err();
    assert_eq!(error.item, item(1));
    assert!(error.what.contains("whole build"), "{}", error.what);
    let unknown = InstanceKey(ContentId([9; 32]));
    let error = build.compile_one(unknown, &Store::default()).unwrap_err();
    assert_eq!(error.unit, unknown);
    assert!(error.what.contains("no function of the build's walk"), "{}", error.what);
}

/// The daemon's store is behind a trait object at its edge: `compile_one` takes it unsized.
#[test]
fn compile_one_reads_a_store_behind_a_trait_object() {
    let (program, policy) = (program(), live());
    let mut store = Store::default();
    let (machine, memo) = (Machine::default(), Memo::new());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    store.keep(&built.puts);
    let lookup: &dyn Lookup = &store;
    let before = counts(&machine);
    let entry = build.compile_one(built.entry[0], lookup).unwrap();
    assert_eq!(entry, built.functions[0]);
    assert_eq!(counts(&machine), before);
}

/// `memo_hit` is `compile_kept`'s memo branch alone: what it gives on a hit, and `None` for
/// each reason it has none, building no IR and compiling nothing either way. It takes no
/// store, so it reads none.
#[test]
fn memo_hit_is_compile_kept_from_the_memo_and_nothing_else() {
    let (program, policy) = (program(), live());
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    let key = |id: u32| built.walked.iter().find(|it| it.item == item(id)).unwrap().key;
    let before = counts(&machine);
    assert!(build.memo_hit(InstanceKey(ContentId([0xab; 32]))).is_none(), "no function of the walk");
    assert!(build.memo_hit(built.entry[0]).is_none(), "compiled ahead, so its check kept no shape");
    assert!(build.memo_hit(key(4)).is_none(), "its shape, which the memo has nothing of yet");
    assert_eq!(counts(&machine), before, "a probe builds no IR and compiles nothing");

    let kept = build.compile_kept(key(4), &store).unwrap();
    let after = counts(&machine);
    let hit = build.memo_hit(key(4)).expect("its shape is in the memo now").unwrap();
    assert_eq!(hit, kept, "the instance and its template's put, as `compile_kept` gives them");
    // 3 is 2's shape: once 2 is compiled, 3 is a hit, as `compile_kept` would take it.
    build.compile_kept(key(2), &store).unwrap();
    let after_two = counts(&machine);
    let three = build.memo_hit(key(3)).expect("2's template").unwrap();
    assert_eq!(counts(&machine), after_two);
    assert_eq!(three, build.compile_kept(key(3), &store).unwrap());
    assert_eq!(counts(&machine), after_two, "`compile_kept` took the same branch");
    assert!(after.1 > before.1);
}

#[test]
fn memo_hit_is_none_for_a_function_of_no_dev_backend() {
    let (program, release) = (program(), policy(OptLevel::O3, Mode::Output));
    let (machine, memo) = (Machine::default(), Memo::new());
    let mut build = Build::new(&machine, &program, &release, &memo);
    let built = build.run(&[(1, member())], &Store::default()).unwrap();
    assert!(build.memo_hit(built.roots[0]).is_none(), "an LLVM function is never compiled lazily");
}

/// What a lazy run keeps for its first calls, owned past its build: the program it borrowed is
/// free again, and each first call compiles what a whole build compiles, against the front end
/// and backend it is handed.
#[test]
fn an_owned_walk_compiles_what_a_whole_build_compiles() {
    let (program, policy) = (program(), live());
    let whole = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap();
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    let later = build.into_later();
    for walked in &built.walked {
        let (function, _) = later.compile_kept(&machine, &program, &memo, walked.key, &store).unwrap();
        let want = whole.functions.iter().find(|it| it.key == walked.key).unwrap();
        assert_eq!(&function, want);
    }
}

/// `memo_hit` and `memo_put` together are `compile_kept`'s memo branch, and a probe that
/// misses allocates nothing.
#[test]
fn memo_hit_and_memo_put_are_compile_kept_from_the_memo() {
    let (program, policy) = (program(), live());
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    let key = |id: u32| built.walked.iter().find(|it| it.item == item(id)).unwrap().key;
    let later = build.into_later();
    let unknown = InstanceKey(ContentId([0xab; 32]));
    let missed = blaze_alloc_count::assert_at_most(0, "a memo probe that misses", || {
        (later.memo_hit(&memo, key(4)).is_none(), later.memo_hit(&memo, unknown).is_none())
    });
    assert_eq!(missed, (true, true));
    assert!(later.memo_put(&memo, key(4)).is_none());

    let kept = later.compile_kept(&machine, &program, &memo, key(4), &store).unwrap();
    let before = counts(&machine);
    let hit = later.memo_hit(&memo, key(4)).expect("its shape is in the memo now").unwrap();
    let put = later.memo_put(&memo, key(4)).expect("the hit's template");
    assert_eq!((hit, vec![put]), kept);
    assert_eq!(counts(&machine), before, "a probe builds no IR and compiles nothing");
}

/// The key decides: a function whose body changed before its first call is refused, naming
/// it and both code hashes, while one the edit left alone compiles at the later revision, and
/// one whose template the memo has by its recorded shape is that template whatever changed.
#[test]
fn a_first_call_after_an_edit_compiles_only_what_still_has_its_key() {
    let (mut program, policy) = (program(), live());
    let whole = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap();
    let want = |key: InstanceKey| whole.functions.iter().find(|it| it.key == key).unwrap().clone();
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    let key = |id: u32| built.walked.iter().find(|it| it.item == item(id)).unwrap().key;
    let code = |id: u32| built.walked.iter().find(|it| it.item == item(id)).unwrap().code;
    let later = build.into_later();
    later.compile_kept(&machine, &program, &memo, key(2), &store).unwrap();

    for id in [3, 4] {
        program.functions.get_mut(&id).unwrap().body ^= 0x01;
    }
    let before = counts(&machine);
    let error = later.compile_kept(&machine, &program, &memo, key(4), &store).unwrap_err();
    assert_eq!((error.unit, error.item), (key(4), item(4)));
    assert!(error.what.starts_with("changed before its first call"), "{}", error.what);
    assert!(error.what.contains(&format!("{:?}", code(4))), "{}", error.what);
    assert_eq!(counts(&machine), before, "refused before its IR is built");

    let (five, _) = later.compile_kept(&machine, &program, &memo, key(5), &store).unwrap();
    assert_eq!(five, want(key(5)));
    let (three, _) = later.compile_kept(&machine, &program, &memo, key(3), &store).unwrap();
    assert_eq!(three, want(key(3)), "2's template, by the shape 3's check recorded");
}

/// A tier-up compiles the function alone at each tier, under the tier's own key, and refuses
/// a tier the toolchain has no backend for and a function edited since the image.
#[test]
fn tier_up_compiles_a_function_alone_at_each_tier() {
    let (mut program, mut policy) = (program(), live());
    policy.toolchain.stencils = Some("ab".repeat(32));
    let (machine, memo, mut store) = (Machine::default(), Memo::new(), Store::default());
    let mut build = Build::new(&machine, &program, &policy, &memo);
    let built = build.run_later(&[(1, member())], &[], &[0], &store).unwrap();
    let key = |built: &Built, id: u32| built.walked.iter().find(|it| it.item == item(id)).unwrap().key;
    let (four, five) = (key(&built, 4), key(&built, 5));
    let later = build.into_later();
    let tiers = [(Backend::Stencil, true), (Backend::Cranelift { speed: false }, true), (Backend::Llvm { opt: 3 }, false)];
    let mut keys = vec![four];
    for (to, counted) in tiers {
        let (function, puts) = later.tier_up(&machine, &program, &memo, four, to, counted, &store).unwrap();
        assert!(!keys.contains(&function.key), "{to:?}: a tier is its own instance");
        assert_eq!(function.code[0], 0x44);
        keys.push(function.key);
        store.keep(&puts);
        let before = counts(&machine);
        let again = later.tier_up(&machine, &program, &memo, four, to, counted, &store).unwrap();
        assert_eq!(again, (function, Vec::new()), "{to:?}: the store's, kept by the first");
        assert_eq!(counts(&machine), before);
    }

    let mut without = policy.clone();
    without.toolchain.llvm = None;
    let mut build = Build::new(&machine, &program, &without, &memo);
    let no_llvm = build.run_later(&[(1, member())], &[], &[0], &Store::default()).unwrap();
    let to_llvm = |later: &Later<u32, u32>| {
        later.tier_up(&machine, &program, &memo, key(&no_llvm, 4), Backend::Llvm { opt: 3 }, false, &Store::default())
    };
    assert!(to_llvm(&build.into_later()).unwrap_err().what.contains("without it"));

    // No build tiered 5 up, so its stencil tier would be built from its new body: refused.
    program.functions.get_mut(&5).unwrap().body = 0x56;
    let error = later.tier_up(&machine, &program, &memo, five, Backend::Stencil, true, &Store::default()).unwrap_err();
    assert!(error.what.starts_with("changed before its first call"), "{}", error.what);
}

/// Two lazy builds of one memo, as two runs of a job: what the first compiled ahead, the
/// second takes from the memo by its stored key, preparing nothing for it.
#[test]
fn a_second_lazy_build_of_the_memo_takes_what_the_first_compiled_ahead() {
    let (program, policy) = (program(), live());
    let memo = Memo::new();
    let first = Build::new(&Machine::default(), &program, &policy, &memo)
        .run_later(&[(1, member())], &[], &[0], &Store::default())
        .unwrap();
    assert_eq!(first.stats.compiled, 1);
    let machine = Machine::default();
    let second = Build::new(&machine, &program, &policy, &memo)
        .run_later(&[(1, member())], &[], &[0], &Store::default())
        .unwrap();
    assert_eq!((second.stats.flown, second.stats.compiled), (1, 0));
    assert_eq!(second.functions, first.functions);
    // Only the checks of the four functions compiled later.
    assert_eq!(counts(&machine), (4, 0));
}
