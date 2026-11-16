//! [`Group`]: the machines of a cycle of modules, solved together. Each member reads the
//! others live (their declarations and their state as it is), so the group reaches the least
//! fixpoint of the cycle: a name is bound only where some member's text binds it, never by
//! the members justifying each other. It then writes one scope per member, each with every
//! read it used, its fellows' included: `settle_reads` runs each member's final pass alone
//! with its fellows answered from the group's finished scopes, so the record is the same
//! whatever the schedule and names no fellow.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::read::{Reply, Step};
use crate::resolver::{Outbox, Ran, Resolver};
use crate::scope::Resolved;

/// The machines of one cycle, sorted by module.
pub struct Group<'a, N> {
    members: AVec<'a, Resolver<'a, N>>,
    out: Outbox<'a, N>,
    alloc: Alloc<'a>,
    finished: bool,
}

impl<'a, N: Name> Group<'a, N> {
    /// A group of `members` (at least one, each of another module, none finished), allocating
    /// in `alloc`. What the members were answered they keep; what they decided they keep too.
    pub fn new_in(
        members: AVec<'a, Resolver<'a, N>>,
        alloc: Alloc<'a>,
    ) -> Result<Group<'a, N>, ResolveError> {
        let mut members = members;
        if members.is_empty() {
            return Err(ResolveError::BadGroup { module: None });
        }
        members.sort_unstable_by_key(|m| m.module);
        for pair in members.windows(2) {
            if pair[0].module == pair[1].module {
                return Err(ResolveError::BadGroup { module: Some(pair[0].module) });
            }
        }
        for m in members.iter_mut() {
            if m.finished {
                return Err(ResolveError::Finished { module: m.module });
            }
            // What a member waited on may now be a fellow, read live: every item is evaluated
            // again.
            m.block_all();
            // A member's own worklist is of it alone: the group's is made at its first step.
            m.out.work = None;
        }
        Ok(Group { members, out: Outbox::new_in(alloc), alloc, finished: false })
    }

    /// The members' modules, sorted.
    pub fn modules(&self) -> impl Iterator<Item = ItemKey> + '_ {
        self.members.iter().map(|m| m.module)
    }

    /// One step of the protocol for the whole group: every member takes `replies`. `Done`
    /// holds one result per member, in the order of [`Group::modules`].
    pub fn step<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        replies: &[Reply<'_, N>],
    ) -> Result<Step<'_, N, AVec<'a, Resolved<'a, N>>>, ResolveError> {
        if self.finished {
            return Err(ResolveError::Finished { module: self.members[0].module });
        }
        for m in self.members.iter_mut() {
            m.absorb(replies)?;
        }
        match crate::work::run(self.members.as_mut_slice(), facts, replies, &mut self.out, self.alloc)? {
            Ran::Ask => Ok(Step::Ask(&self.out.asks)),
            Ran::Expand => Ok(Step::Expand(&self.out.expand)),
            Ran::Cycle => Ok(Step::Cycle(&self.out.cycle)),
            Ran::Done(all) => {
                self.finished = true;
                Ok(Step::Done(all))
            }
        }
    }

    /// The work of every step so far.
    pub fn stats(&self) -> crate::resolver::FillStats {
        let mut stats = self.out.stats;
        for m in self.members.iter() {
            stats.add(&m.out.stats);
        }
        stats
    }

    /// The members back, to join a larger group when this one meets another cycle.
    pub fn into_members(self) -> AVec<'a, Resolver<'a, N>> {
        self.members
    }
}
