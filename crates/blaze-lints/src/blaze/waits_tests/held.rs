//! `held_across_wait` over [`super`]'s fixtures: the thread-tied values `Send` lets through,
//! each held across `db.variance(..)`, which parks the fiber.

use super::check_in;

const MAIN: &str = "//- /src/lib.rs crate:main deps:engine,std,blaze_graph\n";

/// A `&u32` lent by `LocalKey::with` (as written, through a renamed import, and in a macro's
/// closure) used after a query, a `NoWait` live across one, and a guard that gives back to a
/// thread-local (stdx's `lend`, the 2026-10-31 crash) are findings; the same values ended or
/// read before the call, a `LocalKey` held by a type with no drop, and a `!Send` value (the
/// may-suspend check's own error) are none.
#[test]
fn held_across_wait() {
    let out = check_in(
        MAIN,
        r#"
use blaze_graph::sched::NoWait;
use engine::Db;
use std::thread::LocalKey;
use std::thread::LocalKey as Key;

static SLOT: LocalKey<u32> = LocalKey::new(slot);
static OTHER: Key<u32> = Key::new(slot);

fn slot() -> *const u32 {
    0 as *const u32
}

pub fn lent(db: &Db) -> u32 {
    SLOT.with(|slot| {
        let v = db.variance(*slot); //~ held_across_wait
        v + *slot
    })
}

pub fn lent_through_a_rename(db: &Db) -> u32 {
    OTHER.with(|it| {
        let v = db.variance(1); //~ held_across_wait
        v + *it
    })
}

pub fn read_before(db: &Db) -> u32 {
    let first = SLOT.with(|slot| *slot);
    db.variance(first)
}

pub fn dead_after(db: &Db) -> u32 {
    SLOT.with(|slot| db.variance(*slot))
}

macro_rules! with_slot {
    ($db:expr) => {
        SLOT.with(|slot| {
            let v = $db.variance(1);
            v + *slot
        })
    };
}

pub fn by_macro(db: &Db) -> u32 {
    with_slot!(db) //~ held_across_wait
}

pub fn no_wait(db: &Db) -> u32 {
    let _region = NoWait::new();
    db.variance(1) //~ held_across_wait
}

pub fn no_wait_ended(db: &Db) -> u32 {
    {
        let _region = NoWait::new();
    }
    db.variance(1)
}

pub struct GiveBack {
    slot: &'static LocalKey<u32>,
}

impl Drop for GiveBack {
    fn drop(&mut self) {
        self.slot.with(|_| {});
    }
}

pub fn lend(db: &Db) -> u32 {
    let _back = GiveBack { slot: &SLOT };
    db.variance(1) //~ held_across_wait
}

pub struct Names {
    slot: &'static LocalKey<u32>,
}

pub fn names_a_key(db: &Db) -> u32 {
    let names = Names { slot: &SLOT };
    let v = db.variance(1);
    let _ = names.slot;
    v
}

pub fn not_send(db: &Db) -> u32 {
    let _rc = engine::rc();
    db.variance(1)
}
"#,
        "-A clippy::all --deny=blaze::held_across_wait",
    );
    assert!(
        out.contains("`slot` (`&u32`), lent by `LocalKey::with` from this thread's storage, is tied to this thread"),
        "a lent reference is named with its type and lender:\n{out}"
    );
    assert!(out.contains("`_region` (`NoWait`), a `NoWait` region that must not wait"), "{out}");
    assert!(out.contains("a `GiveBack` whose drop gives back to a thread-local"), "{out}");
    assert!(
        out.contains("`variance` → `fetch` → `get_or_run` → `suspend`"),
        "the note names the path to the fiber's switch:\n{out}"
    );
}
