//! Budgets (docs/v2/plan/design/std.md, "Budgets ride the same way"): a capability with a
//! limit is narrower than one without, and exceeding it is a typed outcome, never a stall.
//! [`crate::net::Net::limit`] is a `Net`'s byte limit and [`crate::fs::Dir::quota`] a
//! directory's; both count in a [`Budget`], bstd's own accounting, and end in an [`Over`].
//!
//! A budget made from a capability that has one already is inside it: bytes are taken from
//! both, so a narrowing can only lower what is left, never raise it.
//!
//! Counting is atomics only: every take, give-back and look is `#[blaze::noalloc]` and
//! `#[blaze::nonblocking]`, checked, so a budget never adds a wait or an allocation to the
//! call it meters.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

/// A budget that had no room for what was asked of it: its limit, what was used of it then,
/// and what was asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Over {
    pub limit: u64,
    pub used: u64,
    pub asked: u64,
}

impl core::fmt::Display for Over {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Over { limit, used, asked } = self;
        write!(f, "{asked} more bytes would pass its limit of {limit} ({used} used)")
    }
}

/// A count of bytes against a limit, shared by every capability narrowed from the one it was
/// set on.
pub(crate) struct Budget {
    limit: u64,
    used: AtomicU64,
    /// The budget of the capability this one was narrowed from, if it had one.
    outer: Option<Arc<Budget>>,
}

impl Budget {
    /// A budget of `limit` bytes inside `outer`.
    #[blaze::nonblocking]
    pub(crate) fn new(limit: u64, outer: Option<Arc<Budget>>) -> Arc<Budget> {
        // alloc: once per limit set.
        Arc::new(Budget { limit, used: AtomicU64::new(0), outer })
    }

    /// `bytes` taken from this budget alone, if it has room.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn take_here(&self, bytes: u64) -> Result<(), Over> {
        let took = self.used.try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|after| *after <= self.limit)
        });
        match took {
            Ok(_) => Ok(()),
            Err(used) => Err(Over { limit: self.limit, used, asked: bytes }),
        }
    }

    /// `bytes` taken from this budget and from each it is inside, or from none of them: the
    /// first with no room says so.
    ///
    /// The contract: a refusal names what was asked, and what it says was used never passes
    /// the limit it names.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn take(&self, bytes: u64) -> (r: Result<(), Over>)
        ensures match &r {
            Ok(()) => true,
            Err(over) => over.asked == bytes && over.used <= over.limit,
        }
    {
        self.take_here(bytes)?;
        let Some(outer) = &self.outer else { return Ok(()) };
        if let Err(over) = outer.take(bytes) {
            self.used.fetch_sub(bytes, Ordering::AcqRel);
            return Err(over);
        }
        Ok(())
    }

    /// `bytes` taken before and not used after all, given back here and to each it is inside.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn give_back(&self, bytes: u64) {
        let mut at = Some(self);
        while let Some(budget) = at {
            budget.used.fetch_sub(bytes, Ordering::AcqRel);
            at = budget.outer.as_deref();
        }
    }

    /// The most bytes a take could still be given: the least left of this and each it is
    /// inside.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn left(&self) -> u64 {
        let mut left = u64::MAX;
        let mut at = Some(self);
        while let Some(budget) = at {
            left = left.min(budget.limit.saturating_sub(budget.used.load(Ordering::Acquire)));
            at = budget.outer.as_deref();
        }
        left
    }

    /// That this budget has no room for `asked` bytes, as it stands.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn over(&self, asked: u64) -> Over {
        Over { limit: self.limit, used: self.used.load(Ordering::Acquire), asked }
    }
}

impl core::fmt::Debug for Budget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Budget({} of {} used)", self.used.load(Ordering::Acquire), self.limit)
    }
}
