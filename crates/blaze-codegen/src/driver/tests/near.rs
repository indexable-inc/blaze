//! The walk's frontier form ([`Build::near`]): one function made ready to compile from its own
//! describe and those of what it refers to, held to what a whole walk makes of it.

use std::sync::Arc;

use super::*;

fn launched() -> Policy {
    policy(OptLevel::O0, Mode::Launched)
}

/// A program of 3 to 9 functions and up to 2 statics: `main` (1) a member's, calling the next
/// function and some of the others (a callee before it too: recursion); each other one a
/// member's or a dependency's (which calls dependencies' alone), a member's naming some
/// static, and a function building in some of the dependency's functions after it that it
/// calls.
pub(super) fn drawn(tape: &blaze_sim::Tape) -> Program {
    let count: u32 = tape.int_in(3..=9u32, "functions");
    let statics: u32 = tape.int_in(0..=2u32, "statics");
    let mut program = Program::default();
    for id in 0..statics {
        program.statics.insert(id, (id as u8, None));
    }
    let dependency: Vec<bool> =
        (1..=count).map(|id| id != 1 && tape.int_in(0..=1u8, "a member's") == 0).collect();
    for id in 1..=count {
        let mut calls: Vec<u32> = Vec::new();
        for callee in 1..=count {
            // A dependency's code refers to no workspace code: its image would change with it.
            if callee == id || (dependency[(id - 1) as usize] && !dependency[(callee - 1) as usize]) {
                continue;
            }
            if callee == id + 1 || tape.int_in(0..=3u8, "skips a call") == 0 {
                calls.push(callee);
            }
        }
        // Built in: a dependency's function after it (so nothing builds itself in), which no
        // edit patches.
        let built_in: Vec<u32> = calls
            .iter()
            .enumerate()
            .filter(|(_, callee)| **callee > id && dependency[(**callee - 1) as usize])
            .filter(|_| tape.int_in(0..=1u8, "not built in") == 0)
            .map(|(place, _)| place as u32)
            .collect();
        let named: Vec<u32> = (0..statics)
            .filter(|_| !dependency[(id - 1) as usize] && tape.int_in(0..=2u8, "names no static") == 0)
            .collect();
        let function = program.function(id, 0x10 + id as u8, &calls);
        function.built_in = built_in;
        function.statics = named;
        if id == 1 {
            function.symbol = Some("main".to_owned());
            continue;
        }
        if dependency[(id - 1) as usize] {
            function.origin = Origin::Dependency(Package::new("dep"));
            function.owner = Owner::Dependency;
        }
    }
    program
}

/// What is at each place, a function by its key whether or not through its slot: a launched
/// program's calls are all through slots, and which the whole walk calls direct is its own.
fn places(refs: &[Target]) -> Vec<Target> {
    refs.iter()
        .map(|it| match it {
            Target::Direct(key) => Target::Function(*key),
            other => other.clone(),
        })
        .collect()
}

/// The functions a frontier form of `id` describes, by the program's own tables: it, what it
/// builds in at any depth, and what each of those calls.
fn expected_describes(program: &Program, id: u32) -> std::collections::BTreeSet<u32> {
    let (mut whole, mut queue) = (std::collections::BTreeSet::new(), vec![id]);
    while let Some(at) = queue.pop() {
        if !whole.insert(at) {
            continue;
        }
        let function = &program.functions[&at];
        queue.extend(function.built_in.iter().map(|place| function.calls[*place as usize]));
    }
    let mut described = whole.clone();
    for at in &whole {
        described.extend(program.functions[at].calls.iter().copied());
    }
    described
}

/// Why the frontier form of each function of `program` is not what the whole walk of `walked`
/// (the same program, or for a control another) makes of it; `None` when it is, everywhere.
fn differs(program: &mut Program, walked: &Program) -> Option<String> {
    let policy = launched();
    let (machine, memo) = (Machine::default(), Memo::new());
    let whole = Build::new(&machine, walked, &policy, &memo);
    let mut errors = Vec::new();
    let (nodes, _, _, _, _) = whole.walk(&[(1, member())], &[], &[], None, &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
    let built = Build::new(&Machine::default(), walked, &policy, &Memo::new())
        .run(&[(1, member())], &Store::default())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let described = Arc::new(Lock::new("test.codegen.driver.near_described", Vec::new()));
    let seen = Arc::clone(&described);
    program.watch = Some(Box::new(move |id| seen.lock().push(id)));
    let program = &*program;
    for (key, node) in nodes.iter() {
        let name = &node.described.name;
        described.lock().clear();
        let (machine, memo) = (Machine::default(), Memo::new());
        let alone = Build::new(&machine, program, &policy, &memo);
        let near = match alone.near(&(node.instance, node.requester)) {
            Ok(near) => near,
            Err(why) => return Some(format!("{name}: {}", why.what)),
        };
        if near.key != *key {
            return Some(format!("{name}: key {:?}, the whole walk's {key:?}", near.key));
        }
        let own = near.node();
        if own.unit != node.unit {
            return Some(format!("{name}: unit {:?}, the whole walk's {:?}", own.unit, node.unit));
        }
        if places(near.refs()) != places(&node.refs) {
            return Some(format!("{name}: places {:?}, the whole walk's {:?}", near.refs(), node.refs));
        }
        if own.built_in != node.built_in {
            return Some(format!("{name}: builds in {:?}, the whole walk's {:?}", own.built_in, node.built_in));
        }
        let asked: std::collections::BTreeSet<u32> = described.lock().iter().copied().collect();
        let expected = expected_describes(program, node.instance);
        if asked != expected {
            return Some(format!("{name}: described {asked:?}, where it and what it builds in refer to {expected:?}"));
        }
        if near.described != described.lock().len() {
            return Some(format!("{name}: counts {} describes of {}", near.described, described.lock().len()));
        }
        // Its code is the whole build's: of the function itself, or of the one equal to it
        // that the whole build kept for both.
        let kept = built.deduped.merged.iter().find(|it| *it.0 == *key).map_or(*key, |it| *it.1);
        let Some(compiled) = built.functions.iter().find(|it| it.key == kept) else {
            return Some(format!("{name}: the whole build compiled no {kept:?}"));
        };
        match alone.compile_near(&near, &Store::default()) {
            Ok((function, _)) if function.code == compiled.code && function.key == *key => {}
            Ok((function, _)) => {
                return Some(format!(
                    "{name}: code {:?} under {:?}, the whole build's {:?} under {key:?}",
                    function.code, function.key, compiled.code
                ));
            }
            Err(why) => return Some(format!("{name}: {}", why.what)),
        }
    }
    None
}

/// The refinement: the frontier form of a function is the whole walk's. For every function a
/// whole walk of a launched program reaches, [`Build::near`] gives the same key and unit, the
/// same thing at each place, the same built-in functions and the same code, and describes
/// only the function, what it builds in, and what those refer to.
#[test]
fn the_frontier_form_of_a_function_is_the_whole_walks() {
    blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
        let mut program = drawn(tape);
        let walked = program.copy();
        if let Some(why) = differs(&mut program, &walked) {
            panic!("{why}");
        }
    })
}

/// A control of [`the_frontier_form_of_a_function_is_the_whole_walks`]: a frontier form made
/// of a program whose callee is another is caught, by the key at its caller's place.
#[test]
fn the_oracle_catches_a_frontier_form_of_other_code() {
    let walked = program();
    assert_eq!(differs(&mut program(), &walked), None);
    let mut other = program();
    other.functions.get_mut(&4).unwrap().body = 0x45;
    let why = differs(&mut other, &walked).expect("a callee of other code is another key");
    assert!(why.contains("key") || why.contains("places"), "{why}");
}

/// Only a launched build's: any other's places say which callees are called directly, which
/// only a whole walk knows, so the ask is refused by name.
#[test]
fn a_frontier_form_is_refused_outside_a_launched_build() {
    let (program, policy) = (program(), policy(OptLevel::O0, Mode::Live));
    let (machine, memo) = (Machine::default(), Memo::new());
    let why = Build::new(&machine, &program, &policy, &memo).near(&(2, member())).err().expect("refused");
    assert!(why.what.contains("`f2` asked for alone") || why.what.contains("asked for alone"), "{}", why.what);
    assert!(why.what.contains("Live"), "{}", why.what);
}

/// A frontier form describes its function's callees and nothing they call: of `main` (1),
/// which calls 2 and 3, the functions 4 and 5 are not described.
#[test]
fn a_frontier_form_describes_no_callee_of_a_callee() {
    let (program, policy) = (program(), launched());
    let (machine, memo) = (Machine::default(), Memo::new());
    let near = Build::new(&machine, &program, &policy, &memo).near(&(1, member())).unwrap();
    assert_eq!(near.described, 3);
    assert_eq!(near.callees.iter().map(|it| it.asked.0).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(program.engine.describes.load(std::sync::atomic::Ordering::Relaxed), 3);
}
