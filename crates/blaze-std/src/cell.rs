//! Cells (docs/v2/plan/design/std.md, "Shared mutation"): [`Cell`] as Rust's, [`RawCell`] for
//! trusted code (Rust's `UnsafeCell`), and token cells.
//!
//! A [`TokenCell<T, K>`] holds data; a [`Token<K>`] is the permission to touch every cell of
//! its kind `K`. [`TokenCell::get`] takes `&Token<K>` and [`TokenCell::get_mut`] takes
//! `&mut Token<K>`, so the borrow checker refuses a write while any other borrow through the
//! token lives, across every cell of the kind, with no run-time flag and no panic. Put the
//! token behind one `Lock` and that lock guards a whole graph of cells.
//!
//! # Why a token is one of a kind
//!
//! Everything rests on one fact: for each kind there is at most one `Token`. A kind is a
//! [`Brand<'id>`], and the only way to get a `Token<Brand<'id>>` is [`Token::scope`], which
//! calls its closure once with a token whose `'id` the closure must accept for every lifetime
//! (`for<'id>`): inside, `'id` is a fresh lifetime no other value names (GhostCell's
//! generative brand). `Brand` is invariant in `'id`, so a token of one scope can never be taken
//! for another's, nor a nested scope's for its outer one's. `Token` is neither `Clone` nor
//! `Copy`, its field is private, and nothing else in bstd makes one: no counter, no static,
//! nothing at run time. A token, and every cell of its kind, cannot leave its scope, since the
//! scope's result type cannot name `'id`.
//!
//! The one `unsafe` of bstd is in this file: the two views of a cell's value through its
//! token, and the `Sync` of a cell.
//!
//! Two live `get_mut` through one token are refused as the crate compiles (E0499). Kept here
//! as a fixture: bstd builds with `doctest = false`, so it is not run.
//!
//! ```compile_fail,E0499
//! use bstd::cell::{Token, TokenCell};
//!
//! Token::scope(|mut token| {
//!     let a = TokenCell::new(1);
//!     let b = TokenCell::new(2);
//!     let first = a.get_mut(&mut token);
//!     let second = b.get_mut(&mut token);
//!     *first += *second;
//! });
//! ```

use core::{fmt, marker::PhantomData};

pub use core::cell::{Cell, UnsafeCell as RawCell};

/// The kind of a scope's token and cells: invariant in `'id` (it is both read and returned
/// by the `fn` it names), and `Send + Sync` (a `fn` pointer is), so a token moves between
/// threads as any value does.
pub struct Brand<'id>(PhantomData<fn(&'id ()) -> &'id ()>);

/// The permission to touch every [`TokenCell`] of kind `K`: read through `&Token<K>`, write
/// through `&mut Token<K>`. Made only by [`Token::scope`], one per kind.
pub struct Token<K> {
    kind: PhantomData<K>,
}

impl Token<()> {
    /// Calls `f` with the one token of a fresh kind, and returns what it returns.
    #[blaze::pure]
    pub fn scope<R>(f: impl for<'id> FnOnce(Token<Brand<'id>>) -> R) -> R {
        f(Token { kind: PhantomData })
    }
}

impl<K> fmt::Debug for Token<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token")
    }
}

/// A value read and written through the [`Token`] of its kind `K`.
pub struct TokenCell<T: ?Sized, K> {
    kind: PhantomData<K>,
    value: RawCell<T>,
}

impl<T, K> TokenCell<T, K> {
    #[blaze::pure]
pub const fn new(value: T) -> TokenCell<T, K> {
        TokenCell { kind: PhantomData, value: RawCell::new(value) }
    }

    #[blaze::pure]
pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized, K> TokenCell<T, K> {
    /// The value, read while `token` is lent shared.
    #[blaze::nopanic]
    pub fn get<'a>(&'a self, token: &'a Token<K>) -> &'a T {
        let _ = token;
        // SAFETY: a `&mut T` into a cell of kind `K` exists only as `get_mut`'s result, which
        // borrows the kind's one `Token` uniquely for as long as it lives (uniqueness: the
        // module's docs, upheld by `Token::scope` being the only constructor). `token` is
        // borrowed shared for `'a` here, so no such `&mut T` is alive during `'a`. The other
        // way to reach the value mutably, `value_mut`, takes `&mut self`, which the `&'a self`
        // borrow excludes for `'a`.
        unsafe { &*self.value.get() }
    }

    /// The value, written while `token` is lent unique: no other cell of the kind can be read
    /// or written until the result is gone.
    #[blaze::nopanic]
    pub fn get_mut<'a>(&'a self, token: &'a mut Token<K>) -> &'a mut T {
        let _ = token;
        // SAFETY: every reference into a cell of kind `K` made through a token (`get`,
        // `get_mut`) borrows the kind's one `Token` for its whole life (uniqueness: the
        // module's docs, upheld by `Token::scope`). `token` is borrowed uniquely for `'a`
        // here, so no other such reference, to this cell or any other of the kind, is alive
        // during `'a`; `value_mut` needs `&mut self`, which the `&'a self` borrow excludes.
        // So this `&mut T` is the only reference to the value for `'a`.
        unsafe { &mut *self.value.get() }
    }

    /// The value, written through unique access to the cell itself: no token needed.
    #[blaze::pure]
    pub fn value_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

// SAFETY: a `&TokenCell<T, K>` on another thread reaches the value only through `get`, with a
// `&Token<K>` on that thread too, as a `&T` (shared across threads: `T: Sync`), or through
// `get_mut`, with the kind's one `&mut Token<K>`, which only one thread holds at a time: a
// write of `T` from whichever thread holds the token, after other threads' reads ended (moving
// a `T`'s writes between threads: `T: Send`). The same bounds as `RwLock<T>`'s `Sync`.
unsafe impl<T: ?Sized + Send + Sync, K: Sync> Sync for TokenCell<T, K> {}

impl<T: ?Sized, K> fmt::Debug for TokenCell<T, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenCell")
    }
}
