//! A stop set partway through a build ([`Frontend::stopped`]: its job cancelled, or the plane
//! asking it to give way) ends it within one function on each core: once the flag is set no
//! function is begun, in any of the per-function loops, and the build ends with `stopped`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

/// [`Machine`], which sets the program's stop once it has prepared or emitted that many.
struct Stopping<'p> {
    machine: Machine,
    stop: &'p AtomicBool,
    prepares: Option<usize>,
    emits: Option<usize>,
}

impl Stopping<'_> {
    fn after(&self, at: Option<usize>, count: &AtomicUsize) {
        if at.is_some_and(|at| count.load(Ordering::Relaxed) >= at) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn counts(&self) -> (usize, usize, usize) {
        let machine = &self.machine;
        let load = |it: &AtomicUsize| it.load(Ordering::Relaxed);
        (load(&machine.prepared), load(&machine.summarized), load(&machine.emitted))
    }
}

impl Prepare for Stopping<'_> {
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
        let made = self.machine.prepare(program, unit, spec, id, refs);
        self.after(self.prepares, &self.machine.prepared);
        made
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
        let made = self.machine.emit(unit, spec, prepared, refs, imports);
        self.after(self.emits, &self.machine.emitted);
        made
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

const LEAVES: u32 = 200;

/// `main` (1) calling 200 leaves, each its own shape, so each is its own compile.
fn wide() -> Program {
    let mut program = Program::default();
    let leaves: Vec<u32> = (2..2 + LEAVES).collect();
    program.function(1, 0x11, &leaves).symbol = Some("main".to_owned());
    for (n, id) in leaves.iter().enumerate() {
        program.function(*id, 1 + n as u8, &[]);
    }
    program
}

fn stopping(program: &Program, prepares: Option<usize>, emits: Option<usize>) -> Stopping<'_> {
    Stopping { machine: Machine::default(), stop: &program.stop, prepares, emits }
}

fn stopped_with(result: Result<Built, Vec<CodegenError>>) {
    let errors = result.expect_err("a stopped build gives no output");
    assert!(!errors.is_empty());
    assert!(errors.iter().all(|it| it.what.starts_with("stopped")), "{errors:?}");
}

/// What may begin after the stop: one function a core, each already past its check.
fn within_one_function_a_core(at: usize) -> usize {
    at + blaze_graph::par::threads()
}

#[test]
fn a_stop_while_preparing_ends_a_dev_build_before_any_compile() {
    let program = wide();
    let machine = stopping(&program, Some(10), None);
    let policy = policy(OptLevel::O0, Mode::Output);
    stopped_with(Build::new(&machine, &program, &policy, &Memo::new()).run(&[(1, member())], &Store::default()));
    let (prepared, _, emitted) = machine.counts();
    assert!(prepared >= 10 && prepared <= within_one_function_a_core(10), "{prepared} prepared");
    assert!((prepared as u32) < LEAVES, "the stop came before every prepare was begun");
    assert_eq!(emitted, 0, "nothing is compiled once stopped");
}

#[test]
fn a_stop_while_compiling_ends_a_dev_build_within_one_function() {
    let program = wide();
    let machine = stopping(&program, None, Some(5));
    let policy = policy(OptLevel::O0, Mode::Output);
    stopped_with(Build::new(&machine, &program, &policy, &Memo::new()).run(&[(1, member())], &Store::default()));
    let (prepared, _, emitted) = machine.counts();
    assert_eq!(prepared as u32, LEAVES + 1, "every IR was built before the first compile");
    assert!(emitted >= 5 && emitted <= within_one_function_a_core(5), "{emitted} compiled");
}

#[test]
fn a_stop_while_preparing_ends_a_release_build_before_any_summary_is_decided() {
    let program = wide();
    let machine = stopping(&program, Some(10), None);
    let policy = policy(OptLevel::O3, Mode::Output);
    stopped_with(Build::new(&machine, &program, &policy, &Memo::new()).run(&[(1, member())], &Store::default()));
    let (prepared, summarized, emitted) = machine.counts();
    assert!(prepared >= 10 && prepared <= within_one_function_a_core(10), "{prepared} prepared");
    assert!(summarized <= prepared);
    assert_eq!(emitted, 0, "nothing is compiled once stopped");
}

#[test]
fn a_stop_while_compiling_ends_a_release_build_within_one_function() {
    let program = wide();
    let machine = stopping(&program, None, Some(5));
    let policy = policy(OptLevel::O3, Mode::Output);
    stopped_with(Build::new(&machine, &program, &policy, &Memo::new()).run(&[(1, member())], &Store::default()));
    let (_, summarized, emitted) = machine.counts();
    assert_eq!(summarized as u32, LEAVES + 1, "every summary was made before the first compile");
    assert!(emitted >= 5 && emitted <= within_one_function_a_core(5), "{emitted} compiled");
}

#[test]
fn a_stop_while_checking_ends_a_lazy_build_within_one_function() {
    let program = wide();
    let machine = stopping(&program, Some(10), None);
    let policy = policy(OptLevel::O0, Mode::Live);
    let memo = Memo::new();
    let mut build = Build::new(&machine, &program, &policy, &memo);
    stopped_with(build.run_later(&[(1, member())], &[], &[0], &Store::default()));
    let (prepared, _, emitted) = machine.counts();
    assert!(prepared >= 10 && prepared <= within_one_function_a_core(10), "{prepared} prepared");
    assert_eq!(emitted, 0, "the entry is not compiled once stopped");
}

#[test]
fn an_unset_stop_changes_nothing() {
    let program = wide();
    let machine = stopping(&program, None, None);
    let policy = policy(OptLevel::O0, Mode::Output);
    let built = Build::new(&machine, &program, &policy, &Memo::new()).run(&[(1, member())], &Store::default()).unwrap();
    assert_eq!(built.functions.len() as u32, LEAVES + 1);
    assert_eq!(machine.counts(), (LEAVES as usize + 1, 0, LEAVES as usize + 1));
}
