//! One lazy run: its plan, its stub region, and what to fetch first.
//!
//! The daemon makes a session for each dev run or test process it starts (after
//! [`crate::mode::allow`], which [`Session::new`] checks again), writes [`Stubs::bytes`] into
//! the run process's code memory, gives blaze-link each lazy function's entry for its slot
//! ([`Session::slots`]), compiles [`Plan::ahead`] with the image, and hands the session to a
//! [`crate::runner::Runner`] with what it should fetch before anything asks
//! ([`Session::first`]).

use std::fmt;

use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_items::CodeHash;

use crate::{
    LazyId,
    fetch::Priority,
    mode::{Purpose, Refused, allow},
    plan::Plan,
    stub::{self, Isa, Layout, StubError, Stubs},
};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SessionError {
    Refused(Refused),
    Stub(StubError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Refused(it) => it.fmt(f),
            SessionError::Stub(it) => it.fmt(f),
        }
    }
}

impl std::error::Error for SessionError {}

pub struct Session {
    plan: Plan,
    layout: Layout,
}

impl Session {
    /// The session of a build that `purpose` describes, whose functions `plan` divides, run on
    /// `isa`, its stubs calling the run process's hook at `hook` (0 to write the cell later).
    /// Refused for a release build or one that writes an output.
    pub fn new(purpose: &Purpose<'_>, plan: Plan, isa: &Isa, hook: u64) -> Result<(Session, Stubs), SessionError> {
        allow(purpose).map_err(SessionError::Refused)?;
        let stubs = stub::generate(isa, plan.len(), hook).map_err(SessionError::Stub)?;
        Ok((Session { plan, layout: stubs.layout }, stubs))
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Where `id`'s entry is, for a region written at `base` in the run process.
    pub fn entry(&self, base: u64, id: LazyId) -> u64 {
        base + u64::from(self.layout.entry(id))
    }

    /// Every lazy function and the address its slot starts at, for a region written at
    /// `base`: what blaze-link puts in each one's slot when it places the image.
    pub fn slots(&self, base: u64) -> impl Iterator<Item = (InstanceKey, u64)> + '_ {
        self.plan.lazy().iter().enumerate().map(move |(id, key)| (*key, self.entry(base, LazyId(id as u32))))
    }

    /// What to compile before anything asks: the lazy callees of what is compiled ahead
    /// (`ahead`, the entry and what runs at start), then what history says the run reaches
    /// (`reached`, the code hashes of the functions the last run of this test executed), each
    /// once, at the higher of its priorities.
    pub fn first(&self, ahead: &[CompiledFunction], reached: &[CodeHash]) -> Vec<(LazyId, Priority)> {
        // alloc: setup, once per run before it starts.
        let (mut callees, mut history, mut seen) = (Vec::new(), Vec::new(), self.plan.scratch());
        for function in ahead {
            self.plan.callees(function, &mut seen, &mut callees);
        }
        self.plan.reached(reached, &mut history);
        // alloc: setup, as above.
        let mut out: Vec<(LazyId, Priority)> = Vec::with_capacity(callees.len() + history.len());
        out.extend(callees.into_iter().map(|id| (id, Priority::Callee)));
        out.extend(history.into_iter().map(|id| (id, Priority::History)));
        out.retain(|(id, _)| {
            let (word, bit) = (id.index() / 64, 1u64 << (id.0 % 64));
            let first = seen[word] & bit == 0;
            seen[word] |= bit;
            first
        });
        out
    }
}
