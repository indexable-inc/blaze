//! PROOF of async-P8-4 (5): every slot a coroutine keeps across a suspension names a flow local
//! (walk/state_names.rs; blaze-lir coroutine_region.rs), on this file's native runner.
//!
//! The program is the parent's `two` (`async fn two() -> u64 { let a: u64 = 40; let r: u64 =
//! twice().await; a + r }`), built into a state laid from TYPES by `lay_state` over the flow's
//! saved locals, keyed by identity as the flow layer emits them (blaze-flow-lower
//! identity.rs): `a` (`Binding(a)`, a `u64`), and the `.await`'s future (`Temp(await,
//! Awaitee)`, a `Twice`, 16 bytes). (Which locals the flow saves for such a body is the
//! flow-lower test `the_awaitee_is_saved_and_the_pin_is_not`; here the table is written out.)
//!
//! - The run: the split refuses any slot read after the suspension that no name places, so a
//!   build that succeeds IS one whose kept slots all name a flow local; it then runs as the
//!   unlaid build does (ready(42) at poll 3, `twice()` called once).
//! - THE PLANT (`StatePlant::BuilderSlot`): each `.await` makes a slot of the walk's own before
//!   its loop and reads it after the resume. Refused by name at the `.await`.
//! - The control: the same table without `a`'s row: `a`'s home is a slot the walk made that the
//!   table does not name, read after the resume (`a + r`): refused by name.

use std::alloc::Global;
use std::vec::Vec as StdVec;

use blaze_flow_lower::identity::{Identity, Role};
use blaze_own::Alloc;
use blaze_typeck::{BinOp, Capture, ExprId, PatId};

use super::{Prog, Ran, Script, async_body, async_fn, bin, call_await, let_u64, lit, open_world, run, run_laid};
use crate::BuildError;
use crate::coroutine_state::{LaidState, StateName, StateNames, StatePlan, StatePlant, UNNAMED_ACROSS, lay_state};

/// A state laid from a written table: each saved local's identity, layout and whether it needs
/// drop, in the recipe's order.
pub(super) struct Laid {
    laid: LaidState,
    rows: StdVec<StateName>,
    plant: StatePlant,
}

impl Laid {
    /// `saved` laid after an upvars' tuple of `env`, with `points` suspensions.
    fn new(env: (u64, u8), saved: &[(Identity, (u64, u8), bool)], points: u32, plant: StatePlant) -> Laid {
        let sizes: StdVec<(u64, u8)> = saved.iter().map(|s| s.1).collect();
        let flags: StdVec<bool> = saved.iter().map(|s| s.2).collect();
        let l = lay_state(env, &sizes, &flags, points, Alloc::global(), Global).unwrap_or_else(|e| panic!("lay_state: {e}"));
        let mut rows: StdVec<StateName> = saved
            .iter()
            .enumerate()
            .map(|(i, s)| StateName { identity: s.0, offset: l.field[i], size: s.1.0 as u32, flag: l.flag[i] })
            .collect();
        rows.sort_unstable_by_key(|r| r.identity);
        Laid { laid: l.state, rows, plant }
    }

    pub(super) fn plan(&self) -> StatePlan<'_> {
        let names = StateNames::new(&self.rows, self.laid.region).unwrap_or_else(|e| panic!("the written table: {e}"));
        StatePlan { laid: self.laid, names, plant: self.plant }
    }
}

/// `two`'s body in `b`, giving the root, `a`'s pattern and the `.await`.
fn two(b: &mut blaze_typeck::fixture::Bb, lits: &mut StdVec<(ExprId, u128)>) -> (ExprId, PatId, ExprId) {
    let forty = lit(b, lits, 40);
    let (s0, a) = let_u64(b, "a", forty);
    let awaited = call_await(b, "twice");
    let (s1, r) = let_u64(b, "r", awaited);
    let (ae, re) = (b.local(a), b.local(r));
    let sum = bin(b, BinOp::Add, ae, re);
    let body = b.block(&[s0, s1], Some(sum));
    (b.async_block(body, Capture::Move), a, awaited)
}

/// Prints "state names: laid ready(42) at poll 3; builder-made slot refused at the await;
/// without a's row refused (expression N)".
#[test]
fn every_slot_kept_across_a_suspension_names_a_flow_local() {
    let mut w = open_world();
    let item = async_fn(&mut w, "two", "two_future");
    w.fx.finish();
    let mut b = async_body();
    let mut lits = StdVec::new();
    let (root, a, awaited) = two(&mut b, &mut lits);
    let progs = [Prog { item, b: &b, root, lits: &lits }];
    let unlaid = run(&w, &progs, Script::Complete);
    let saved = [
        (Identity::Binding(a), (8, 3), false),
        (Identity::Temp(awaited, Role::Awaitee), (16, 3), false),
    ];

    // The run: built into the laid state, every kept slot named; it behaves as the unlaid build.
    let laid = Laid::new((0, 0), &saved, 1, StatePlant::None);
    let ran = run_laid(&w, &progs, Script::Complete, Some(&laid)).unwrap_or_else(|e| panic!("the laid build: {e}"));
    assert_eq!(ran, unlaid);
    assert_eq!(ran, Ran { value: 42, polls: 3, drops: 0, made: 1 });

    // THE PLANT: a slot of the walk's own across the suspension, refused at the `.await`.
    let planted = Laid::new((0, 0), &saved, 1, StatePlant::BuilderSlot);
    match run_laid(&w, &progs, Script::Complete, Some(&planted)) {
        Err(BuildError::Unsupported { expr, what }) => {
            assert_eq!((expr, what), (awaited, UNNAMED_ACROSS), "the planted slot is named at its `.await`");
        }
        other => panic!("the planted builder-made slot went uncaught: {other:?}"),
    }

    // The control: no row for `a`, whose home is read after the resume.
    let without_a = Laid::new((0, 0), &saved[1..], 1, StatePlant::None);
    let refused = match run_laid(&w, &progs, Script::Complete, Some(&without_a)) {
        Err(BuildError::Unsupported { expr, what }) if what == UNNAMED_ACROSS => expr,
        other => panic!("a's slot, unnamed and read after the resume, went uncaught: {other:?}"),
    };
    std::println!(
        "state names: laid ready({}) at poll {}; builder-made slot refused at the await; without a's row refused (expression {})",
        ran.value, ran.polls, refused.0
    );
}
