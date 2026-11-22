//! Lock ranks: locks taken in one order, by type (std.md, "Concurrency: what to reach for").
//!
//! A [`RankedLock<T, R>`] has a rank `R` ([`Rank::LEVEL`]). It is taken with a token, an
//! [`Order`]: the task's own (`cx.lock_order()`, rank [`Unranked`]) or the one a guard of a
//! lower rank lends ([`RankedGuard::order`]). Taking a lock whose rank is not above the
//! token's is a compile error, so two tasks can never take two ranked locks in opposite orders.
//!
//! ```text
//! enum Store {} impl Rank for Store { const LEVEL: u32 = 10; const NAME: &'static str = "store"; }
//! enum Index {} impl Rank for Index { const LEVEL: u32 = 20; const NAME: &'static str = "index"; }
//!
//! let mut order = cx.lock_order().expect("the task's token");
//! let mut store = STORE.lock(&mut order)?;          // Unranked (0) < Store (10)
//! let index = INDEX.lock(store.order())?;            // Store (10) < Index (20)
//! // STORE.lock(index.order()) does not compile: Index (20) is not below Store (10).
//! order.cx();                                        // does not compile while `store` lives
//! ```
//!
//! **A guard is not held across a wait.** The task's token is the one way, in code written
//! against it, to its `Cx` ([`Order::cx`]); a guard borrows the token, so while a guard lives
//! there is no `&Cx` to wait with, and a lock of a higher rank is the one wait it allows. This
//! binds a function handed `&mut Order` and no `&Cx`; code that also holds the bare `cx` is
//! held to it by convention until blaze's check, "a value that is not `Send`, or a guard, live
//! across a call that can wait", refuses it everywhere (rule 27).
//!
//! What the types enforce: the order (a `const` check of the two ranks, which both builders
//! read; it fails when the call is compiled for its ranks), one token a task
//! ([`Cx::lock_order`] gives `None` while it is out), and no second lock from one token while
//! the first is held (the guard borrows it mutably). By convention: that code reaches `Cx`
//! through the token. [`crate::sync::Lock`] stays beside it for a lock never held with another;
//! moving one is changing its type to `RankedLock<T, R>` and its `lock(cx)` to `lock(&mut order)`.

use core::{
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

use crate::{
    Stopped,
    cx::Cx,
    sync::{Lock, LockGuard},
};

/// A lock's place in the order: a lock is taken only with a token of a lower `LEVEL`.
pub trait Rank {
    const LEVEL: u32;
    /// What the order's errors call it.
    const NAME: &'static str;
}

/// The rank of a task's own token, below every lock.
pub enum Unranked {}

impl Rank for Unranked {
    const LEVEL: u32 = 0;
    const NAME: &'static str = "unranked";
}

/// A token: permission to take a lock of a rank above `R`. The task's own, of rank
/// [`Unranked`], is from [`Cx::lock_order`]; a guard of rank `R` lends one of `R`.
pub struct Order<'cx, R: Rank> {
    cx: &'cx Cx,
    /// Whether this is the task's own, given back to its `Cx` when dropped.
    root: bool,
    rank: PhantomData<R>,
}

impl Cx {
    /// The task's lock-order token, or `None` while it is out (one per task; it comes back when
    /// dropped).
    pub fn lock_order(&self) -> Option<Order<'_, Unranked>> {
        // `then`, not `then_some`: a token made and dropped on refusal would give back the one
        // that is out.
        self.take_order().then(|| Order { cx: self, root: true, rank: PhantomData })
    }
}

impl<'cx> Order<'cx, Unranked> {
    /// The task's context, for its waits: borrowed from the token, so not while a guard taken
    /// with it lives.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cx(&self) -> &Cx {
        self.cx
    }
}

impl<R: Rank> Drop for Order<'_, R> {
    fn drop(&mut self) {
        if self.root {
            self.cx.give_order();
        }
    }
}

/// A lock of rank `R`. See the module's doc.
pub struct RankedLock<T, R: Rank> {
    lock: Lock<T>,
    rank: PhantomData<fn() -> R>,
}

/// A held [`RankedLock`]: the value, and a token of its rank for the locks above it.
pub struct RankedGuard<'o, T, R: Rank> {
    guard: LockGuard<'o, T>,
    order: Order<'o, R>,
}

impl<T, R: Rank> RankedLock<T, R> {
    /// A lock of rank `R` named `name` over `value`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(name: &'static str, value: T) -> RankedLock<T, R> {
        RankedLock { lock: Lock::new(name, value), rank: PhantomData }
    }

    /// The lock, taken with `order`, a token of a rank below `R` (a compile error otherwise),
    /// waited for until the task's deadline as [`Lock::lock`] is. The token is lent to the
    /// guard while it lives.
    pub fn lock<'o, B: Rank>(&'o self, order: &'o mut Order<'_, B>) -> Result<RankedGuard<'o, T, R>, Stopped> {
        let () = const {
            assert!(B::LEVEL < R::LEVEL, "bstd::sync::ranks: a lock is taken only with a token of a lower rank");
        };
        let cx: &'o Cx = order.cx;
        let guard = self.lock.lock(cx)?;
        Ok(RankedGuard { guard, order: Order { cx, root: false, rank: PhantomData } })
    }

    /// The lock if it is free now, with `order` as [`RankedLock::lock`]'s; never waits.
    pub fn try_lock<'o, B: Rank>(&'o self, order: &'o mut Order<'_, B>) -> Option<RankedGuard<'o, T, R>> {
        let () = const {
            assert!(B::LEVEL < R::LEVEL, "bstd::sync::ranks: a lock is taken only with a token of a lower rank");
        };
        let cx: &'o Cx = order.cx;
        let guard = self.lock.try_lock()?;
        Some(RankedGuard { guard, order: Order { cx, root: false, rank: PhantomData } })
    }

    /// The lock's name.
    pub fn name(&self) -> &'static str {
        self.lock.name()
    }

    /// The value, through the only reference to the lock: no wait, no token.
    pub fn get_mut(&mut self) -> &mut T {
        self.lock.get_mut()
    }
}

impl<'o, T, R: Rank> RankedGuard<'o, T, R> {
    /// The token of this lock's rank, for a lock above it.
    pub fn order(&mut self) -> &mut Order<'o, R> {
        &mut self.order
    }
}

impl<T, R: Rank> Deref for RankedGuard<'_, T, R> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T, R: Rank> DerefMut for RankedGuard<'_, T, R> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<R: Rank> fmt::Debug for Order<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Order({}, rank {})", R::NAME, R::LEVEL)
    }
}

impl<T, R: Rank> fmt::Debug for RankedLock<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RankedLock({}, rank {} {})", self.lock.name(), R::NAME, R::LEVEL)
    }
}
