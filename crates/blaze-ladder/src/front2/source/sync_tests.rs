//! READ-SYNC-B's proof: the fields of [`Records`] that the bodies' read side used to write
//! through a shared borrow (the `key_beginning` cache and the three planted bugs) are now
//! `Sync`, so they no longer stop `&Records` being sent to the pool.
//!
//! The check is at compile time: `assert_sync` takes only a `T: Sync`, so with any of these
//! fields back in a `Cell` or `RefCell` (core's `Cell<T>` and `RefCell<T>` are `!Sync`: their
//! docs, "Cell<T> ... is not Sync", on the toolchain shelf's `core/src/cell.rs`) this module does
//! not compile, which is the failing form. The run prints
//! `test front2::source::sync_tests::the_read_side_fields_of_records_are_sync ... ok`.
//!
//! What this does NOT prove, by name: `Records` as a whole is not `Sync` yet. Still in it:
//! `frame: &Frame` (blaze-mem's bump, `Cell`s), `log: &Log` (`RefCell`/`Cell`), `incoherent`
//! (a `Cell` set once by `name_lang_crates`), and blaze-scope's `RecordNames`/`RecordItems`
//! (`OnceCell`s and an `Alloc`, neither `Sync`) with a loader over `&dyn Load`. When those
//! move, the whole assertion `assert_sync::<Records<'static>>()` goes in this test.

use blaze_id::ItemKey;

use super::Records;

/// Compiles only for a `T: Sync`.
fn assert_sync<T: Sync>(_: &T) {}

/// Each read-side field of READ-SYNC-B, by reference: a compile-time check that each is `Sync`.
fn read_side_fields_are_sync(records: &Records<'_>) {
    let no_impls: &Option<usize> = &records.no_impls;
    let no_bucket_impl: &Option<ItemKey> = &records.no_bucket_impl;
    let no_item: &Option<ItemKey> = &records.no_item;
    assert_sync(no_impls);
    assert_sync(no_bucket_impl);
    assert_sync(no_item);
}

#[test]
fn the_read_side_fields_of_records_are_sync() {
    // The check above is the compile; naming it here makes it part of this test's build.
    let check: fn(&Records<'_>) = read_side_fields_are_sync;
    let _ = check;
}
