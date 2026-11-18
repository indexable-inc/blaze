//! `test` runs its tests after its check (`Effect::StartTests`; docs/v2/plan/design/test.md
//! piece 3), as `run` starts its program after the gate (sim.rs `gates`): a check with no
//! error and no failed task starts them, once, and never a program; one with an error starts
//! nothing, and answers `Failed { started: false }`; the tests' end is the verdict. Then the
//! repeat with nothing changed stands on the certificate, runs no task, and starts the tests
//! exactly when the first did.
//!
//! THE CONTROL (rule 23): under `Plant::TestsOverErrors` the request starts the tests over a
//! check error, and the property names it ("tests started over").
//!
//! Expected lines (`cargo test -p blaze-drive sim::tests_start`):
//! `test sim::tests_start::tests_start_after_a_clean_check ... ok`,
//! `test sim::tests_start::control_tests_over_a_check_error_are_caught ... ok`.

use super::*;

fn tests_start(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    // An unchecked item holds no start of the tests (`Gate::Beside`, as main.rs asks `test`).
    world.unchecked = tape.one_in(2, "items not checked");
    let mut store = Store::default();
    let setup = Setup {
        workers: workers(tape),
        ask: Ask { command: Command::Test, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &[] },
        plant,
        tool: SIM_TOOL,
        moved: None,
        split: Split::PerKind,
        opens: tape.one_in(2, "open the files"),
    };
    let (answer, ended) = finished(request_with(tape, &world, None, &mut store, None, setup)?)?;
    let (_, checked) = fresh(tape, &world)?;
    let errors = checked.prints.iter().filter(|print| every_rule(print.1)).count();
    let failed = answer.failed;
    let may_start = errors == 0 && failed == 0;
    let judge = |ended: &Ended, verdict: Verdict, when: &str| -> Result<(), Broke> {
        oracle(!ended.started, || format!("{when}: `test` started a program"))?;
        oracle(!ended.tested || may_start, || format!("{when}: tests started over {errors} errors and {failed} failed tasks"))?;
        oracle(ended.tested == may_start, || format!("{when}: a check clean of errors and failures did not start the tests"))?;
        match verdict {
            Verdict::Passed => oracle(ended.tests_ok == Some(true), || format!("{when}: `test` passed and its tests did not")),
            Verdict::ProgramFailed => oracle(ended.tests_ok == Some(false), || format!("{when}: `test` failed as its tests, which passed or did not run")),
            Verdict::Failed { started, .. } => {
                oracle(!may_start, || format!("{when}: a failed verdict over a clean check"))?;
                oracle(!started && !ended.tested, || format!("{when}: a failed check whose tests started"))
            }
            other => Err(Broke::Oracle(format!("{when}: `test` answered {other:?}"))),
        }
    };
    judge(&ended, answer.verdict, "the first run")?;
    // The repeat, nothing changed: the certificate decides the start at its one read.
    let repeat = request_with(tape, &world, Some(&world), &mut store, ended.last.as_deref(), setup)?;
    if let Some(error) = &repeat.error {
        return Err(Broke::Oracle(format!("the repeat stopped with an error: {error}")));
    }
    oracle(repeat.runs.is_empty(), || format!("the repeat ran {:?}: the check's answer is found by key", repeat.runs))?;
    let again = repeat.answer.ok_or(Broke::Harness(String::from("the repeat gave no answer")))?;
    oracle(again.profile.certified, || String::from("the repeat did not stand on its certificate"))?;
    judge(&repeat, again.verdict, "the repeat")
}

#[test]
fn tests_start_after_a_clean_check() {
    check(SimConfig::cases(300), |tape| tests_start(tape, Plant::None));
}

#[test]
fn control_tests_over_a_check_error_are_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::TestsOverErrors, "tests started over", tests_start);
}
