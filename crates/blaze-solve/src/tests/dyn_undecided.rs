//! The control of the undecided-dyn stop (rule 43): with the bug planted (an undecided
//! principal taken as not dyn compatible, the state the tempo run of 2026-11-18 met),
//! `dyn_undecided_stops` answers No where it must stop, and the oracle (each case answers as
//! written) catches it. The decided control beside it answers the same both ways.

use blaze_sim::{Config, control};

use super::named_cases;
use super::plant::{self, Plant};

#[test]
fn control_dyn_undecided_as_no() {
    control(Config::cases(2), Plant::None, Plant::DynUndecidedAsNo, "an object case answers wrong", |t, p| {
        plant::with(p, || named_cases(t, &["dyn_undecided_stops", "dyn_undecided_control"]))
    });
}
