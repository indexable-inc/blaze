//! A launched run's start and its first calls ([`Build::start`], [`Build::one`],
//! [`Build::one_static`]): made with no walk, held to what the whole build makes.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use super::*;

fn launched() -> Policy {
    policy(OptLevel::O0, Mode::Launched)
}

/// What each place names, a function by its key whether or not through its slot, and by the
/// key the whole build kept for it where dedupe merged it: a launched program calls every
/// function through its slot and merges no code, and which the whole walk calls direct and
/// which it merges are its own (the frontier form's module doc).
fn plain_target(target: &Target, merged: &BTreeMap<InstanceKey, InstanceKey>) -> Target {
    let named = |key: &InstanceKey| *merged.get(key).unwrap_or(key);
    match target {
        Target::Direct(key) | Target::Function(key) => Target::Function(named(key)),
        other => other.clone(),
    }
}

fn plain(function: &CompiledFunction, merged: &BTreeMap<InstanceKey, InstanceKey>) -> CompiledFunction {
    CompiledFunction {
        key: *merged.get(&function.key).unwrap_or(&function.key),
        relocs: function.relocs.map(|it| plain_target(it, merged)),
        ..function.clone()
    }
}

fn plain_static(value: &Static, merged: &BTreeMap<InstanceKey, InstanceKey>) -> Static {
    let init = match &value.init {
        Init::Bytes { bytes, relocs } => {
            Init::Bytes { bytes: bytes.clone(), relocs: relocs.map(|it| plain_target(it, merged)) }
        }
        zeroed => zeroed.clone(),
    };
    Static { init, ..value.clone() }
}

/// A program of [`super::near::drawn`]'s, with perhaps its function 2 defined weakly under a
/// name (when nothing builds it in) and perhaps a static in `.init_array` naming one of its
/// functions, a root. Gives the program and its static roots.
fn drawn(tc: &hegel::TestCase) -> (Program, Vec<u32>) {
    use hegel::generators as gs;
    let mut program = super::near::drawn(tc);
    let count = program.functions.len() as u32;
    let built_in_two = program
        .functions
        .values()
        .any(|it| it.built_in.iter().any(|place| it.calls[*place as usize] == 2));
    if !built_in_two && tc.draw(gs::integers::<u8>().max_value(1)) == 0 {
        let two = program.functions.get_mut(&2).unwrap();
        two.symbol = Some("weak_two".to_owned());
        two.linkage = crate::Linkage::Weak;
    }
    let mut static_roots = Vec::new();
    if tc.draw(gs::integers::<u8>().max_value(1)) == 0 {
        let named = tc.draw(gs::integers::<u32>().min_value(2).max_value(count));
        program.statics.insert(9, (0x99, Some(".init_array".to_owned())));
        program.static_calls.insert(9, vec![named]);
        static_roots.push(9);
    }
    (program, static_roots)
}

/// Why a start of `program` and its first calls, to a fixed point, are not what the whole build
/// of `walked` (the same program in a database of its own) makes; `None` when they are. The
/// lazy build has `planted` in it, for a control.
fn differs(
    program: &mut Program,
    walked: &mut Program,
    static_roots: &[u32],
    planted: Option<Planted>,
) -> Option<String> {
    let policy = launched();
    let watched = |program: &mut Program| {
        let described = Arc::new(Lock::new("test.codegen.driver.start_described", Vec::new()));
        let seen = Arc::clone(&described);
        program.watch = Some(Box::new(move |id| seen.lock().push(id)));
        described
    };
    let whole_described = watched(walked);
    let built = Build::new(&Machine::default(), &*walked, &policy, &Memo::new())
        .run_with_statics(&[(1, member())], static_roots, &[], &Store::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let merged = &built.deduped.merged;
    // The whole walk's items of each function: its own and those of what it builds in.
    let mut walk_errors = Vec::new();
    let (nodes, _, _, _, _) = Build::new(&Machine::default(), &*walked, &policy, &Memo::new())
        .walk(&[(1, member())], static_roots, &[], None, &mut walk_errors);
    assert!(walk_errors.is_empty(), "{walk_errors:?}");
    let items_of = |key: &InstanceKey| -> Option<Vec<ItemKey>> {
        let node = nodes.get(key)?;
        let mut items: Vec<ItemKey> = std::iter::once(node.unit.item)
            .chain(node.built_in.iter().filter_map(|it| nodes.get(it)).map(|it| it.unit.item))
            .collect();
        items.sort_unstable();
        items.dedup();
        Some(items)
    };
    let name = |key: &InstanceKey| {
        built.debug_names.iter().find(|it| it.0 == *key).map_or("a function of no name", |it| it.1.as_str())
    };

    let lazy_described = watched(program);
    let program = &*program;
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let mut lazy = Build::new(&machine, program, &policy, &memo);
    if let Some(bug) = planted {
        lazy = lazy.planting(bug);
    }
    let start = match lazy.start(&[(1, member())], static_roots, "main", &store) {
        Ok(start) => start,
        Err(errors) => return Some(format!("the start: {errors:?}")),
    };
    if start.functions.first().map(|it| it.key) != Some(start.entry) {
        return Some("the start's first function is not its entry".to_owned());
    }
    let mut functions: BTreeMap<InstanceKey, Arc<CompiledFunction>> = BTreeMap::new();
    // Each function's items as made alone, against the whole walk's: the first that differs.
    let mut items_differ: Option<String> = None;
    let mut held_to_walk = |key: InstanceKey, items: &[ItemKey]| {
        let whole = items_of(&key);
        if items_differ.is_none() && whole.as_deref() != Some(items) {
            items_differ = Some(format!("{key:?}: items {items:?}, the whole walk's {whole:?}"));
        }
    };
    let mut statics: BTreeMap<StaticId, Static> = BTreeMap::new();
    let (mut next, mut next_statics) = (Vec::new(), Vec::new());
    for one in start.functions {
        held_to_walk(one.key, &one.items);
        next.extend(one.callees);
        next_statics.extend(one.statics);
        functions.insert(one.key, one.function);
    }
    for one in start.statics {
        next.extend(one.callees);
        next_statics.extend(one.statics);
        statics.insert(one.id, one.value);
    }
    // The first calls: each function a made one names, and each static, until none is new.
    loop {
        if let Some(callee) = next.pop() {
            if functions.contains_key(&callee.key) {
                continue;
            }
            let one = match lazy.one(&callee.asked, &store) {
                Ok(one) => one,
                Err(why) => return Some(format!("f{}: {}", callee.asked.0, why.what)),
            };
            if one.key != callee.key {
                return Some(format!("f{}: made as {:?}, named by its caller {:?}", callee.asked.0, one.key, callee.key));
            }
            held_to_walk(one.key, &one.items);
            next.extend(one.callees);
            next_statics.extend(one.statics);
            functions.insert(one.key, one.function);
            continue;
        }
        if let Some((value, id)) = next_statics.pop() {
            if statics.contains_key(&id) {
                continue;
            }
            let one = match lazy.one_static(&value, &store) {
                Ok(one) => one,
                Err(why) => return Some(format!("S{value}: {}", why.what)),
            };
            next.extend(one.callees);
            next_statics.extend(one.statics);
            statics.insert(one.id, one.value);
            continue;
        }
        break;
    }

    drop(held_to_walk);
    if let Some(why) = items_differ {
        return Some(why);
    }
    let reached: BTreeSet<InstanceKey> = built.inputs.iter().map(|it| it.0).collect();
    let made: BTreeSet<InstanceKey> = functions.keys().copied().collect();
    if made != reached {
        return Some(format!("made {made:?}, the whole build reached {reached:?}"));
    }
    for (key, function) in &functions {
        let kept = *merged.get(key).unwrap_or(key);
        let Some(whole) = built.functions.iter().find(|it| it.key == kept) else {
            return Some(format!("{}: the whole build compiled no {kept:?}", name(key)));
        };
        let (alone, whole) = (plain(function, merged), plain(whole, merged));
        if alone != whole {
            return Some(format!("{}: made alone {alone:?}, the whole build's {whole:?}", name(key)));
        }
    }
    let whole_statics: BTreeMap<StaticId, &Static> =
        built.statics.iter().map(|it| ((it.item, it.instance), it)).collect();
    if whole_statics.keys().collect::<Vec<_>>() != statics.keys().collect::<Vec<_>>() {
        return Some(format!(
            "made the statics {:?}, the whole build {:?}",
            statics.keys().collect::<Vec<_>>(),
            whole_statics.keys().collect::<Vec<_>>()
        ));
    }
    for (id, value) in &statics {
        let (alone, whole) = (plain_static(value, merged), plain_static(whole_statics[id], merged));
        if alone != whole {
            return Some(format!("{id:?}: made alone {alone:?}, the whole build's {whole:?}"));
        }
    }
    let whole: BTreeSet<u32> = whole_described.lock().iter().copied().collect();
    let lazy: BTreeSet<u32> = lazy_described.lock().iter().copied().collect();
    if !lazy.is_subset(&whole) {
        return Some(format!("described {lazy:?}, past the whole walk's {whole:?}"));
    }
    None
}

/// The refinement: under [`Mode::Launched`], [`Build::start`], then [`Build::one`] of every
/// callee and [`Build::one_static`] of every static to a fixed point, give for each key a
/// function equal byte for byte to the whole build's ([`Build::run_with_statics`]; each place a
/// function's by its key, through its slot or not, and by the key dedupe kept) and for each id
/// an equal [`Static`]; each function's items are the whole walk's for it (its own and those of
/// what it builds in); they make exactly what the whole build reaches, and describe nothing the
/// whole walk does not.
#[hegel::test]
fn a_start_and_its_first_calls_make_the_whole_builds_code(tc: hegel::TestCase) {
    let (mut program, static_roots) = drawn(&tc);
    let mut walked = program.copy();
    if let Some(why) = differs(&mut program, &mut walked, &static_roots, None) {
        panic!("{why}");
    }
}

/// `main` (1) calls 2 and 3, 2 is defined weakly as `weak_two`, 2 calls 4, 3 calls 5.
fn weak_program() -> Program {
    let mut program = program();
    let two = program.functions.get_mut(&2).unwrap();
    two.symbol = Some("weak_two".to_owned());
    two.linkage = crate::Linkage::Weak;
    program
}

/// A control of [`a_start_and_its_first_calls_make_the_whole_builds_code`]: with
/// [`Planted::WeakByKey`], `main`'s call to the weakly defined `f2` names its key, where the
/// whole build calls it by its name, and the oracle names `f1`.
#[test]
fn the_oracle_catches_a_weak_callee_called_by_key() {
    let mut walked = weak_program();
    assert_eq!(differs(&mut weak_program(), &mut walked, &[], None), None);
    let mut walked = weak_program();
    let why = differs(&mut weak_program(), &mut walked, &[], Some(Planted::WeakByKey))
        .expect("a weak callee by key is another place");
    assert!(why.starts_with("f1:"), "{why}");
}

/// `main` (1) calls 2 and 3 and names the static 8; 2 calls 4, 3 calls 5; the static 7, a root
/// in `.init_array`, names 4.
fn started_program() -> Program {
    let mut program = program();
    program.functions.get_mut(&1).unwrap().statics = vec![8];
    program.statics.insert(7, (0x77, Some(".init_array".to_owned())));
    program.statics.insert(8, (0x88, None));
    program.static_calls.insert(7, vec![4]);
    program
}

/// Why `start` made other functions than its entry and those its start sections name: `None`
/// when it made exactly those, the entry first.
fn made_other_than_its_start(start: &Start<u32, u32>) -> Option<String> {
    if start.functions.first().map(|it| it.key) != Some(start.entry) {
        return Some("the entry is not the first function".to_owned());
    }
    let made: BTreeSet<InstanceKey> = start.functions.iter().map(|it| it.key).collect();
    let named: BTreeSet<InstanceKey> = (start.statics.iter())
        .filter(|it| it.value.section.as_deref().is_some_and(runs_at_start))
        .flat_map(|it| it.callees.iter().map(|callee| callee.key))
        .collect();
    if let Some(missing) = named.iter().find(|it| !made.contains(*it)) {
        return Some(format!("every `runs_at_start` callee is in `functions`: {missing:?} is not"));
    }
    if let Some(other) = made.iter().find(|it| **it != start.entry && !named.contains(*it)) {
        return Some(format!("{other:?} is made ahead, and is neither the entry nor a start section's"));
    }
    None
}

/// The invariant: a start compiles only its entry and the functions a static in an
/// initializer or finalizer section names, each once, and makes every root static and those
/// the made code names. Of [`started_program`], `main` and 4 are compiled, and nothing else.
/// Control: the `.init_array` static's function left out of the start is caught by name.
#[test]
fn a_start_makes_only_the_entry_and_what_start_sections_name() {
    let (program, policy) = (started_program(), launched());
    let (machine, memo) = (Machine::default(), Memo::new());
    let start = Build::new(&machine, &program, &policy, &memo)
        .start(&[(1, member())], &[7], "main", &Store::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    assert_eq!(made_other_than_its_start(&start), None);
    let mut bodies = machine.emitted_bodies.lock().clone();
    bodies.sort_unstable();
    assert_eq!(bodies, [0x11, 0x44], "main and what `.init_array` names, each compiled once");
    let ids: Vec<StaticId> = start.statics.iter().map(|it| it.id).collect();
    let mut expected = vec![(static_item(7), ContentId::default()), (static_item(8), ContentId::default())];
    expected.sort_unstable();
    assert_eq!(ids, expected, "the root static and the one `main` names, by id");
    assert_eq!(start.declared.iter().map(|it| it.asked.0).collect::<Vec<_>>(), [1]);
    assert_eq!(start.declared[0].symbol, Some(("main".to_owned(), crate::Linkage::Strong)));

    // The control: the start section's function left out.
    let mut short = start;
    short.functions.retain(|it| it.code != program.code(4));
    let why = made_other_than_its_start(&short).expect("the start section's function is missing");
    assert!(why.contains("every `runs_at_start` callee is in `functions`"), "{why}");

    // A program with no `main` is no program to start.
    let mut nameless = started_program();
    nameless.functions.get_mut(&1).unwrap().symbol = None;
    let errors = Build::new(&Machine::default(), &nameless, &policy, &Memo::new())
        .start(&[(1, member())], &[7], "main", &Store::default())
        .err()
        .expect("no entry");
    assert!(errors.iter().any(|it| it.what.contains("the build defines no `main`")), "{errors:?}");
}

/// The invariant: two roots defining one symbol are refused by the start, naming the symbol and
/// both, before anything is compiled, as a whole build refuses them. Control: the same roots
/// with the second definition under another symbol start.
#[test]
fn a_start_refuses_a_symbol_defined_twice_among_its_roots() {
    let policy = launched();
    let program_of = |second: &str| {
        let mut program = program();
        program.functions.get_mut(&4).unwrap().symbol = Some("__rust_panic_cleanup".to_owned());
        program.functions.get_mut(&5).unwrap().symbol = Some(second.to_owned());
        program
    };
    let roots = [(1, member()), (4, member()), (5, member())];
    let program = program_of("__rust_panic_cleanup");
    let machine = Machine::default();
    let errors = Build::new(&machine, &program, &policy, &Memo::new())
        .start(&roots, &[], "main", &Store::default())
        .err()
        .expect("refused");
    let whats: Vec<&str> = errors.iter().map(|it| it.what.as_str()).collect();
    assert_eq!(
        whats,
        ["the symbol `__rust_panic_cleanup` is defined twice in one program, by `f4` and by `f5`"],
        "{errors:?}"
    );
    assert_eq!(machine.emitted.load(Ordering::Relaxed), 0, "refused before anything is compiled");

    let program = program_of("__rust_start_panic");
    let started = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .start(&roots, &[], "main", &Store::default());
    assert!(started.is_ok(), "{:?}", started.err());
}

/// Only a launched build's: a start in any other build is refused by name.
#[test]
fn a_start_is_refused_outside_a_launched_build() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Live));
    let errors = Build::new(&Machine::default(), &program, &policy, &Memo::new())
        .start(&[(1, member())], &[], "main", &Store::default())
        .err()
        .expect("refused");
    assert!(errors[0].what.contains("asked for alone") && errors[0].what.contains("Live"), "{errors:?}");
}
