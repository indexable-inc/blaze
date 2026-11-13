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

use crate::mem::{ByteLen, OutOfMemory, heap};

/// A budget that had no room for what was asked of it: its limit, what was used of it then,
/// and what was asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Over {
    pub limit: ByteLen,
    pub used: ByteLen,
    pub asked: ByteLen,
}

impl core::fmt::Display for Over {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Over { limit, used, asked } = self;
        write!(f, "{asked} more would pass its limit of {limit} ({used} used)")
    }
}

/// A count of bytes against a limit, shared by every capability narrowed from the one it was
/// set on. The count itself is a `u64` atomic; what goes in and out is a [`ByteLen`].
pub(crate) struct Budget {
    limit: ByteLen,
    used: AtomicU64,
    /// The budget of the capability this one was narrowed from, if it had one.
    outer: Option<Arc<Budget>>,
}

impl Budget {
    /// A budget of `limit` bytes inside `outer`, or [`OutOfMemory`] with no room for it.
    pub(crate) fn new(limit: ByteLen, outer: Option<Arc<Budget>>) -> Result<Arc<Budget>, OutOfMemory> {
        // alloc: once per limit set, shared by every capability narrowed from it.
        heap::shared(Budget { limit, used: AtomicU64::new(0), outer })
    }

    /// `bytes` taken from this budget alone, if it has room.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn take_here(&self, bytes: ByteLen) -> Result<(), Over> {
        let (asked, limit) = (bytes.get(), self.limit.get());
        let took = self.used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(asked).filter(|after| *after <= limit)
        });
        match took {
            Ok(_) => Ok(()),
            Err(used) => Err(Over { limit: self.limit, used: ByteLen::new(used), asked: bytes }),
        }
    }

    /// `bytes` taken from this budget and from each it is inside, or from none of them: the
    /// first with no room says so.
    ///
    /// The contract: a refusal names what was asked, and what it says was used never passes
    /// the limit it names.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn take(&self, bytes: ByteLen) -> (r: Result<(), Over>)
        ensures match &r {
            Ok(()) => true,
            Err(over) => over.asked == bytes && over.used <= over.limit,
        }
    {
        self.take_here(bytes)?;
        let Some(outer) = &self.outer else { return Ok(()) };
        if let Err(over) = outer.take(bytes) {
            self.used.fetch_sub(bytes.get(), Ordering::AcqRel);
            return Err(over);
        }
        Ok(())
    }

    /// `bytes` taken before and not used after all, given back here and to each it is inside.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn give_back(&self, bytes: ByteLen) {
        let mut at = Some(self);
        while let Some(budget) = at {
            budget.used.fetch_sub(bytes.get(), Ordering::AcqRel);
            at = budget.outer.as_deref();
        }
    }

    /// The most bytes a take could still be given: the least left of this and each it is
    /// inside.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    ///
    /// A budget never holds more used than its limit (a take past it is refused before it
    /// counts), so what is left of each is its limit less what it used; a count that says
    /// otherwise (a give-back of bytes never taken) is read as none left, never as more.
    pub(crate) fn left(&self) -> ByteLen {
        let mut left = ByteLen::new(u64::MAX);
        let mut at = Some(self);
        while let Some(budget) = at {
            let used = ByteLen::new(budget.used.load(Ordering::Acquire));
            let here = budget.limit.minus(used).unwrap_or(ByteLen::ZERO);
            left = left.at_most(here);
            at = budget.outer.as_deref();
        }
        left
    }

    /// That this budget has no room for `asked` bytes, as it stands.
    pub(crate) fn over(&self, asked: ByteLen) -> Over {
        Over { limit: self.limit, used: ByteLen::new(self.used.load(Ordering::Acquire)), asked }
    }
}

impl core::fmt::Debug for Budget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Budget({} of {} used)", self.used.load(Ordering::Acquire), self.limit.get())
    }
}
