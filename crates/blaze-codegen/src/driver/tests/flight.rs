//! Builds of one memo at once, as a job's build scripts and proc macros are compiled: a
//! function they share (`syn`'s, `quote`'s, `cc`'s) is prepared and compiled once between
//! them, by whichever reaches it first, and each build makes what it would alone.

use std::{sync::Barrier, time::Duration};

use parking_lot::Mutex;

use super::*;

/// [`Machine`] whose compiles take a while, so builds started together overlap, counting each
/// function it prepares and compiles by key.
#[derive(Default)]
struct Slow {
    machine: Machine,
    prepared: Mutex<Vec<InstanceKey>>,
    emitted: Mutex<Vec<InstanceKey>>,
}

impl Prepare for Slow {
    type Frontend = Program;
    type Prepared = Prepared;
    type Export = u8;

    fn prepare(
        &self,
        program: &Program,
        unit: &Unit,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<(Shape, Prepared), CodegenError> {
        self.prepared.lock().push(unit.instance.key());
        self.machine.prepare(program, unit, spec, id, refs)
    }

    fn summarize(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: &mut Prepared,
        refs: &crate::template::Refs,
    ) -> Result<(Summary, u8), CodegenError> {
        self.machine.summarize(unit, spec, prepared, refs)
    }

    fn emit(
        &self,
        unit: &Unit,
        spec: &Spec,
        prepared: Prepared,
        refs: &crate::template::Refs,
        imports: Option<(&Imports, &[Import<'_, u8>])>,
    ) -> Result<Emitted, CodegenError> {
        blaze_graph::spin::busy(Duration::from_millis(10));
        self.emitted.lock().push(unit.instance.key());
        self.machine.emit(unit, spec, prepared, refs, imports)
    }

    fn masked(&self, unit: &Unit, prepared: &Prepared, refs: &crate::template::Refs) -> Option<Masked> {
        self.machine.masked(unit, prepared, refs)
    }

    fn merge(
        &self,
        group: &Group,
        members: Vec<(&Unit, &Spec, Prepared, &crate::template::Refs)>,
    ) -> Result<fold::Merged, CodegenError> {
        self.machine.merge(group, members)
    }

    fn compile_static(
        &self,
        program: &Program,
        spec: &Spec,
        id: &u32,
        refs: &crate::template::Refs,
    ) -> Result<Static, CodegenError> {
        self.machine.compile_static(program, spec, id, refs)
    }
}

/// Three proc macros' expanders (1, 2, 3) that each reach `syn` (20), `proc-macro2` (21) and
/// `quote` (22), and two build scripts' `main`s (4, 5) that each reach `cc` (23) and `libc`
/// (24): ten functions, five of them shared, of the `shared` origin.
fn compile_time(shared: fn(Package) -> Origin) -> Program {
    let mut program = Program::default();
    for (root, calls) in [(1, &[20, 22][..]), (2, &[20, 22]), (3, &[20, 22]), (4, &[23]), (5, &[23])] {
        let function = program.function(root, root as u8, calls);
        function.symbol = Some(format!("root{root}"));
        function.origin = root_origin(root);
    }
    for (id, name, calls) in
        [(20, "syn", &[21][..]), (21, "proc-macro2", &[]), (22, "quote", &[21]), (23, "cc", &[24]), (24, "libc", &[])]
    {
        program.function(id, id as u8, calls).origin = shared(Package::new(name));
    }
    program
}

fn root_origin(root: u32) -> Origin {
    Origin::BuildTime(Package::new(&format!("unit{root}")))
}

/// Builds `roots` at once, each of its own thread and store, all of one memo.
fn at_once(slow: &Slow, program: &Program, policy: &Policy, memo: &Memo, roots: &[u32]) -> Vec<Built> {
    let start = Barrier::new(roots.len());
    std::thread::scope(|scope| {
        let builds: Vec<_> = roots
            .iter()
            .map(|&root| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    Build::new(slow, program, policy, memo).run(&[(root, root_origin(root))], &Store::default()).unwrap()
                })
            })
            .collect();
        builds.into_iter().map(|it| it.join().unwrap()).collect()
    })
}

/// What `root` builds alone, of a memo of its own.
fn alone(program: &Program, policy: &Policy, root: u32) -> Built {
    Build::new(&Machine::default(), program, policy, &Memo::new())
        .run(&[(root, root_origin(root))], &Store::default())
        .unwrap()
}

fn sorted(keys: &Mutex<Vec<InstanceKey>>) -> Vec<InstanceKey> {
    let mut keys = keys.lock().clone();
    keys.sort();
    keys
}

#[test]
fn builds_at_once_prepare_and_compile_each_shared_function_once() {
    let (program, policy) = (compile_time(Origin::BuildTime), policy(OptLevel::O0, Mode::Output));
    let roots = [1u32, 2, 3, 4, 5];
    let (slow, memo) = (Slow::default(), Memo::new());
    // Each build its own store, empty: units of one job put what they compiled only when
    // they end, so none of them finds another's in the store.
    let built = at_once(&slow, &program, &policy, &memo, &roots);

    let (prepared, emitted) = (sorted(&slow.prepared), sorted(&slow.emitted));
    assert_eq!(emitted.len(), 10, "five roots and five shared functions, each compiled once: {emitted:?}");
    assert!(emitted.windows(2).all(|it| it[0] != it[1]), "a function compiled twice: {emitted:?}");
    assert_eq!(prepared, emitted, "a function prepared by a build that did not compile it");
    let compiled: usize = built.iter().map(|it| it.stats.compiled).sum();
    let flown: usize = built.iter().map(|it| it.stats.flown).sum();
    // Every build makes all it reaches: 4 functions a proc macro, 3 a build script.
    assert_eq!((compiled, flown), (10, 3 * 4 + 2 * 3 - 10));
    assert_eq!(memo.flights().landed(), 10);

    // Each build made what it makes alone.
    for (&root, built) in roots.iter().zip(&built) {
        let alone = alone(&program, &policy, root);
        assert_eq!(built.functions, alone.functions, "root {root}");
        assert_eq!(built.puts, alone.puts, "root {root}");
    }
}

#[test]
fn a_build_after_the_others_takes_their_templates_and_prepares_nothing() {
    let (program, policy) = (compile_time(Origin::BuildTime), policy(OptLevel::O0, Mode::Output));
    let memo = Memo::new();
    Build::new(&Machine::default(), &program, &policy, &memo).run(&[(1, root_origin(1))], &Store::default()).unwrap();
    let later = Slow::default();
    let built = Build::new(&later, &program, &policy, &memo).run(&[(2, root_origin(2))], &Store::default()).unwrap();
    // Its own root is all it prepares and compiles; syn, proc-macro2 and quote landed.
    assert_eq!(sorted(&later.emitted).len(), 1);
    assert_eq!(sorted(&later.prepared).len(), 1);
    assert_eq!((built.stats.compiled, built.stats.flown), (1, 3));
}

#[test]
fn release_builds_at_once_compile_each_shared_function_once() {
    // The shared crates at O3 as the shipped std is, by LLVM; the expanders at Cranelift.
    let (program, policy) = (compile_time(Origin::Dependency), policy(OptLevel::O3, Mode::Output));
    let roots = [1u32, 2, 3];
    let (slow, memo) = (Slow::default(), Memo::new());
    let built = at_once(&slow, &program, &policy, &memo, &roots);
    let emitted = sorted(&slow.emitted);
    // A release compile is by its imports' blob key: the same imports, one compile.
    assert!(emitted.windows(2).all(|it| it[0] != it[1]), "a release compile done twice: {emitted:?}");
    assert_eq!(emitted.len(), 3 + 3, "three roots and syn, proc-macro2 and quote: {emitted:?}");
    for (&root, built) in roots.iter().zip(&built) {
        assert_eq!(built.functions, alone(&program, &policy, root).functions, "root {root}");
    }
}

/// A release build after another of the same units takes its summaries and its compiles from
/// the memo: it prepares, summarizes and compiles nothing, and makes and keeps what the first
/// did.
#[test]
fn a_release_build_after_another_takes_its_summaries_and_prepares_nothing() {
    let (program, policy) = (compile_time(Origin::Dependency), policy(OptLevel::O3, Mode::Output));
    let memo = Memo::new();
    let first =
        Build::new(&Machine::default(), &program, &policy, &memo).run(&[(1, root_origin(1))], &Store::default()).unwrap();
    let again = Slow::default();
    let built = Build::new(&again, &program, &policy, &memo).run(&[(1, root_origin(1))], &Store::default()).unwrap();
    assert_eq!((sorted(&again.prepared).len(), sorted(&again.emitted).len()), (0, 0));
    assert_eq!(again.machine.summarized.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(built.functions, first.functions);
    assert_eq!(built.puts, first.puts, "the summaries and templates it took, kept in its own store too");
}

/// What a build used of the memo is what a collection keeps for it: kept, the same build
/// again compiles nothing; another build's, dropped, is compiled again.
#[test]
fn a_collection_keeps_what_a_build_used_and_drops_the_rest() {
    let (program, policy) = (compile_time(Origin::BuildTime), policy(OptLevel::O0, Mode::Output));
    let mut memo = Memo::new();
    let first =
        Build::new(&Machine::default(), &program, &policy, &memo).run(&[(1, root_origin(1))], &Store::default()).unwrap();
    let other =
        Build::new(&Machine::default(), &program, &policy, &memo).run(&[(4, root_origin(4))], &Store::default()).unwrap();
    assert!(!first.memo_used.is_empty() && first.memo_used.windows(2).all(|it| it[0] < it[1]));
    memo.retain(|it| first.memo_used.binary_search(it).is_ok());

    let again = Slow::default();
    let built = Build::new(&again, &program, &policy, &memo).run(&[(1, root_origin(1))], &Store::default()).unwrap();
    assert_eq!((built.stats.compiled, built.stats.flown), (0, first.stats.compiled));
    assert!(sorted(&again.prepared).is_empty());
    let dropped = Slow::default();
    let built = Build::new(&dropped, &program, &policy, &memo).run(&[(4, root_origin(4))], &Store::default()).unwrap();
    assert_eq!((built.stats.compiled, built.stats.flown), (other.stats.compiled, 0));
}
