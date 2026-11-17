//! [`Once`]'s proofs.
//!
//! - `model` (only under `--cfg blaze_check`, run by `verify.sh model blaze-std`): two model
//!   threads ask one [`OnceCore`] for its value, each spinning with a yield while the other
//!   holds the making; under every interleaving blaze-check explores (DPOR, three preemptions),
//!   `make` runs exactly once and both get the same value, and the search explored more than one
//!   execution (so a pass is not a model that never interleaved). Control:
//!   [`OncePlant::ClaimSkipsRecheck`], which the search must find.
//! - `world` (a plain build): on the simulation, through the task's `Cx`: a value made once and
//!   kept; a failed make publishes nothing and the next asker makes it; an asker that finds the
//!   making held waits for it under its deadline and stops with `Stopped::Deadline`, calling
//!   nothing.

#[cfg(blaze_check)]
mod model {
    use blaze_check::{
        Builder, Strategy,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        },
        thread,
    };

    use super::super::once::{Claim, OnceCore, OncePlant, Wake};

    /// The model's wake: nothing; a waiter spins with a yield, a schedule point of the model.
    struct Spin;

    impl Wake for Spin {
        fn wake(&self) {}
    }

    /// One asker: the value, made by it when it claims the making (counted in `makes`).
    fn ask(core: &OnceCore<u32>, makes: &AtomicU32) -> u32 {
        loop {
            match core.claim(&Spin) {
                Claim::Done(value) => return *value,
                Claim::Busy => thread::yield_now(),
                Claim::Claimed(making) => {
                    let made = makes.fetch_add(1, Ordering::SeqCst) + 1;
                    return *making.publish(made * 10);
                }
            }
        }
    }

    /// Two askers race; `make` must run once and both see its value.
    fn race(plant: OncePlant) {
        let core = Arc::new(OnceCore::<u32>::new(plant));
        let makes = Arc::new(AtomicU32::new(0));
        let other = {
            let (core, makes) = (Arc::clone(&core), Arc::clone(&makes));
            thread::spawn(move || ask(&core, &makes))
        };
        let mine = ask(&core, &makes);
        let theirs = other.join().expect("the other asker ends");
        assert_eq!(makes.load(Ordering::SeqCst), 1, "make ran other than once");
        assert_eq!(mine, theirs, "the two askers got different values");
    }

    /// Prints `test sync::once_tests::model::model_once_makes_once_under_two_tasks ... ok`.
    #[test]
    fn model_once_makes_once_under_two_tasks() {
        let searched = Builder::new().search(Strategy::Dpor, || race(OncePlant::None)).expect("the search runs");
        assert!(searched.found.is_none(), "the model found: {:?}", searched.found.map(|it| it.report));
        assert!(searched.executions > 1, "the search explored {} execution(s): nothing interleaved, so it proves nothing", searched.executions);
    }

    /// The control. Prints
    /// `test sync::once_tests::model::model_once_catches_a_claim_that_skips_the_recheck ... ok`.
    #[test]
    fn model_once_catches_a_claim_that_skips_the_recheck() {
        let searched =
            Builder::new().search(Strategy::Dpor, || race(OncePlant::ClaimSkipsRecheck)).expect("the search runs");
        assert!(searched.found.is_some(), "the model did not find the second make in {} executions", searched.executions);
    }
}

#[cfg(not(blaze_check))]
mod world {
    use core::{convert::Infallible, time::Duration};

    use crate::{
        Stopped,
        cx::Cx,
        io::Io,
        mem::TASK_FRAME,
        sync::{Once, OnceError},
    };

    /// Longer than any wait here: only the deadlines the tests set end them.
    const BUDGET: Duration = Duration::from_secs(3600);

    fn root(io: &Io) -> Cx {
        io.cx(BUDGET, TASK_FRAME).expect("a test's root frame is made")
    }

    /// Prints `test sync::once_tests::world::once_makes_once_and_keeps_it ... ok`.
    #[test]
    fn once_makes_once_and_keeps_it() {
        let io = Io::sim(1);
        let cx = root(&io);
        let once = Once::<u32>::new("test.once");
        assert_eq!(once.get(), None);
        assert_eq!(once.get_or_make(&cx, |_| Ok::<u32, Infallible>(7)), Ok(&7));
        let again = once.get_or_make(&cx, |_| -> Result<u32, Infallible> { panic!("made a second time") });
        assert_eq!(again, Ok(&7));
        assert_eq!(once.get(), Some(&7));
        assert_eq!(once.into_inner(), Some(7));
    }

    /// Prints `test sync::once_tests::world::a_failed_make_publishes_nothing_and_the_next_asker_makes ... ok`.
    #[test]
    fn a_failed_make_publishes_nothing_and_the_next_asker_makes() {
        let io = Io::sim(2);
        let cx = root(&io);
        let once = Once::<u32>::new("test.once");
        assert_eq!(once.get_or_make(&cx, |_| Err::<u32, &str>("no")), Err(OnceError::Failed { once: "test.once", error: "no" }));
        assert_eq!(once.get(), None, "a failed make published something");
        assert_eq!(once.get_or_make(&cx, |_| Ok::<u32, &str>(8)), Ok(&8));
    }

    /// Prints `test sync::once_tests::world::an_asker_waits_for_the_maker_until_its_deadline ... ok`.
    #[test]
    fn an_asker_waits_for_the_maker_until_its_deadline() {
        let io = Io::sim(3);
        let cx = root(&io);
        let clock = io.need_clock().expect("the simulation has a clock");
        let once = Once::<u32>::new("test.once");
        // Another task's making, in flight.
        let held = once.claim_for_test().expect("an empty Once is claimed");
        let short = cx.until(clock.after(Duration::from_millis(5)));
        let found = once.get_or_make(&short, |_| -> Result<u32, Infallible> { panic!("made while another held the making") });
        assert!(matches!(found, Err(OnceError::Stopped(Stopped::Deadline { .. }))), "{found:?}");
        // The claim ends unpublished: the next asker makes it.
        drop(held);
        assert_eq!(once.get_or_make(&cx, |_| Ok::<u32, Infallible>(9)), Ok(&9));
    }
}
