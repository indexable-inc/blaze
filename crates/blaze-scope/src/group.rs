//! [`Group`]: the machines of a cycle of modules, solved together. Each member reads the
//! others live (their declarations and their state as it is), so the group reaches the least
//! fixpoint of the cycle: a name is bound only where some member's text binds it, never by
//! the members justifying each other. It then writes one scope per member, each naming its
//! fellows, and each with the reads of modules outside the group it used.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::alloc::Allocator;

use blaze_own::Vec;
use blaze_id::ItemKey;

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::read::{Reply, Step};
use crate::resolver::{Outbox, Ran, Resolver, run};
use crate::scope::Resolved;

/// The machines of one cycle, sorted by module.
pub struct Group<N, A: Allocator + Clone> {
    members: Vec<Resolver<N, A>, A>,
    out: Outbox<N, A>,
    alloc: A,
    finished: bool,
}

impl<N: Name, A: Allocator + Clone> Group<N, A> {
    /// A group of `members` (at least one, each of another module, none finished), allocating
    /// in `alloc`. What the members were answered they keep; what they decided they keep too.
    pub fn new_in(members: Vec<Resolver<N, A>, A>, alloc: A) -> Result<Group<N, A>, ResolveError> {
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
            // What a member waited on may now be a fellow, read live.
            m.block_all();
        }
        Ok(Group { members, out: Outbox::new_in(&alloc), alloc, finished: false })
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
    ) -> Result<Step<'_, N, Vec<Resolved<N, A>, A>>, ResolveError> {
        if self.finished {
            return Err(ResolveError::Finished { module: self.members[0].module });
        }
        for m in self.members.iter_mut() {
            m.absorb(replies)?;
        }
        match run(self.members.as_mut_slice(), facts, &mut self.out, &self.alloc)? {
            Ran::Ask => Ok(Step::Ask(&self.out.asks)),
            Ran::Expand => Ok(Step::Expand(&self.out.expand)),
            Ran::Cycle => Ok(Step::Cycle(&self.out.cycle)),
            Ran::Done(all) => {
                self.finished = true;
                Ok(Step::Done(all))
            }
        }
    }

    /// The members back, to join a larger group when this one meets another cycle.
    pub fn into_members(self) -> Vec<Resolver<N, A>, A> {
        self.members
    }
}
