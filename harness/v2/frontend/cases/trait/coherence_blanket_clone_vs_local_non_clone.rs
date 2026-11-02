//@ expect: accept
//@ edition: 2021
// The same impls when the local type is not Clone: `Local: Clone` is knowable (only this crate
// could write it) and fails, so the impls do not overlap. Coherence-mode goals never use the
// global-goal table's shape answers.
#![crate_type = "lib"]

pub trait Tr {
    fn id(&self) -> u8;
}

pub struct Local;

impl<T: Clone> Tr for T {
    fn id(&self) -> u8 {
        1
    }
}

impl Tr for Local {
    fn id(&self) -> u8 {
        2
    }
}

pub fn ids() -> (u8, u8) {
    (Local.id(), 5u32.id())
}
