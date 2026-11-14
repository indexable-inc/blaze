//! A snapshot attached to the thread, for `Debug` of interned and input ids (which hold no
//! database): `attach(db, || format!("{id:?}"))` prints the fields, unattached the id.

use std::{cell::Cell, ptr};

use crate::{
    Database, Fact, Gid, Input, Interned, Kind, Memo,
    trie::{Entry, Head},
};

thread_local! {
    static ATTACHED: Cell<*const Memo> = const { Cell::new(ptr::null()) };
}

/// Runs `f` with `db`'s snapshot attached to this thread.
pub fn attach<D: Database + ?Sized, R>(db: &D, f: impl FnOnce() -> R) -> R {
    struct Restore(*const Memo);
    impl Drop for Restore {
        fn drop(&mut self) {
            ATTACHED.with(|it| it.set(self.0));
        }
    }
    let memo = ptr::from_ref(db.memo());
    let _restore = Restore(ATTACHED.with(|it| it.replace(memo)));
    f()
}

/// Runs `f` with the attached snapshot, if any.
pub fn with_attached<R>(f: impl FnOnce(Option<&Memo>) -> R) -> R {
    // SAFETY: an attached snapshot outlives the `attach` call that attached it, which is on
    // this thread's stack below.
    let memo = unsafe { ATTACHED.with(Cell::get).as_ref() };
    f(memo)
}

/// The fields of interned `id` in the attached snapshot, `None` unattached or where it holds
/// no such id. Lists no read: printing is not a dependency.
pub fn attached_interned<T: Interned, R>(id: Gid, f: impl FnOnce(Option<&T::Fields>) -> R) -> R {
    with_attached(|memo| {
        let head = memo.and_then(|memo| memo.try_head(id, T::KIND));
        // SAFETY: an entry of `T::KIND` is an `Entry<T::Fields, ()>`.
        f(head.map(|head| &unsafe { Entry::<T::Fields, ()>::of(head) }.key))
    })
}

/// The fields of input `id` in the attached snapshot, as [`attached_interned`].
pub fn attached_input<T: Input, R>(id: Gid, f: impl FnOnce(Option<&T::Fields>) -> R) -> R {
    with_attached(|memo| {
        let head = memo.and_then(|memo| memo.try_head(id, T::KIND));
        // SAFETY: an input of `T::KIND` is an `Entry<(), (Fact, T::Fields)>`.
        f(head.map(|head| &unsafe { Entry::<(), (Fact, T::Fields)>::of(head) }.value.1))
    })
}

impl Memo {
    /// The head of `id` if this snapshot holds it and it is of `kind`.
    pub(crate) fn try_head(&self, id: Gid, kind: &'static Kind) -> Option<&Head> {
        let head = if id.is_spill() {
            self.spill().local(id.local())
        } else {
            self.try_slot(id.arena())?.local(id.local())
        }?;
        ptr::eq(head.kind, kind).then_some(head)
    }
}
