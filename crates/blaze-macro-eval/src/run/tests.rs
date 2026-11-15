use hir_def::{AdtId, ModuleDefId, test_items};
use hir_expand::fixture;
use hir_ty::next_solver::{DbInterner, GenericArgs, Ty};
use hir_ty_mir::mir::MirEvalError;

use super::stop_of;

/// An evaluator error that holds a type, made as a failing expansion makes one, is rendered
/// by the run with its type named, though nothing has attached the database: the daemon's
/// loop runs `run` outside any attach.
#[test]
fn an_evaluator_error_names_its_type_outside_an_attach() {
    let db = fixture::db("pub struct Wrapper { pub inner: [u8] }").into_db();
    let krate = fixture::test_crate(&db);
    let error = blaze_query::db::attach(&db, || {
        let adt = test_items::declarations(&db, krate)
            .into_iter()
            .find_map(|it| match it {
                ModuleDefId::AdtId(adt @ AdtId::StructId(_)) => Some(adt),
                _ => None,
            })
            .expect("the fixture's struct");
        let ty = Ty::new_adt(DbInterner::new_with(&db, krate), adt, GenericArgs::empty());
        MirEvalError::TypeIsUnsized(ty.store(), "a test")
    });
    let stop = stop_of(&db, &error);
    assert!(stop.0.starts_with("stopped in the evaluator: "), "{stop}");
    assert!(stop.0.contains("Wrapper is unsized"), "the type is named: {stop}");
}

fn ran(output: Result<crate::stream::Stream, String>) -> super::Ran {
    super::Ran {
        output,
        asked: None,
        tracked: Vec::new(),
        emitted: Vec::new(),
        seen: Vec::new(),
        unkeyable: None,
        statics: Vec::new(),
    }
}

/// One run is one `macro expand` span ([`blaze_profile::span::MacroExpand`]) of its macro and
/// crate that ends with what it spent and how it ended, as the flight recorder reads it back.
#[test]
fn a_run_is_one_macro_expand_span_naming_its_macro_crate_steps_peak_and_outcome() {
    use blaze_profile::{
        flight,
        span::{self, MacroExpand, MacroExpanded, Outcome},
    };

    use super::{Over, Spent, expand_span, ran_on};
    blaze_profile::install();
    let calling = "blaze_api run test";
    std::thread::spawn(move || {
        let span = expand_span("serde_derive::Serialize", calling);
        let spent = Spent { evaluated: true, steps: 4_200_000, peak_bytes: 48 << 20, over: None };
        ran_on(span, &spent, &Ok(ran(Ok(crate::stream::Stream::default()))));
        let stopped = expand_span("serde_derive::Deserialize", calling);
        let spent = Spent {
            evaluated: true,
            steps: super::STEPS,
            peak_bytes: 1 << 20,
            over: Some(Over::Steps),
        };
        ran_on(stopped, &spent, &Err(crate::guest::Stop("ran out".to_owned())));
        let compiled = expand_span("serde_derive::Serialize", calling);
        ran_on(compiled, &Spent::default(), &Ok(ran(Ok(crate::stream::Stream::default()))));
    })
    .join()
    .unwrap();
    let mut read = flight::Read::default();
    flight::window(std::time::Duration::from_secs(60), &mut read);
    let runs: Vec<span::Typed<MacroExpand>> = read
        .spans
        .iter()
        .filter_map(|it| span::read::<MacroExpand>(it).unwrap())
        .filter(|it| it.fields.krate.text().as_deref() == Some(calling))
        .collect();
    let path = |at: usize| runs[at].fields.path.text();
    assert_eq!(runs.len(), 3, "each run is one span");
    assert_eq!(path(0).as_deref(), Some("serde_derive::Serialize"));
    assert_eq!(
        runs.iter().map(|it| it.ended).collect::<Vec<_>>(),
        [
            Some(MacroExpanded {
                steps: Some(4_200_000),
                peak_bytes: Some(48 << 20),
                outcome: Outcome::Ok
            }),
            Some(MacroExpanded {
                steps: Some(super::STEPS),
                peak_bytes: Some(1 << 20),
                outcome: Outcome::StepLimit
            }),
            Some(MacroExpanded { steps: None, peak_bytes: None, outcome: Outcome::Ok }),
        ],
        "a compiled run counts no steps"
    );
}

#[test]
fn a_run_stopped_by_a_budget_names_the_budget_and_a_panic_is_not_an_error() {
    use hir_ty::mir::Budget;

    use blaze_profile::span::Outcome;

    use super::{Over, Spent, outcome_of, over_budget};
    let steps = MirEvalError::OverBudget { spent: Budget::Steps(super::STEPS), at: None };
    let memory = MirEvalError::OverBudget { spent: Budget::Memory(super::MEMORY), at: None };
    assert_eq!(
        (over_budget(&steps), over_budget(&memory)),
        (Some(Over::Steps), Some(Over::Memory))
    );
    assert_eq!(over_budget(&MirEvalError::MachineStop), None);
    let spent = Spent { evaluated: true, ..Spent::default() };
    assert_eq!(outcome_of(&Ok(ran(Err("boom".to_owned()))), &spent), Outcome::Panic);
    let stop = || Err(crate::guest::Stop("stopped".to_owned()));
    assert_eq!(
        outcome_of(&stop(), &Spent { over: Some(Over::Memory), ..spent }),
        Outcome::MemoryLimit
    );
    assert_eq!(outcome_of(&stop(), &spent), Outcome::Error);
}
