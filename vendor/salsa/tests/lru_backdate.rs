#![cfg(feature = "inventory")]

//! A value the LRU dropped, computed again from the inputs it had, is backdated: what read it
//! is verified rather than run again.

use std::sync::atomic::{AtomicUsize, Ordering};

use salsa::{Database, Durability, Setter};

#[salsa::input]
struct N {
    #[returns(copy)]
    value: u32,
}

static TENS: AtomicUsize = AtomicUsize::new(0);
static READ: AtomicUsize = AtomicUsize::new(0);

#[salsa::tracked(returns(copy), lru = 1)]
fn tens(db: &dyn Database, n: N) -> u32 {
    TENS.fetch_add(1, Ordering::Relaxed);
    n.value(db) / 10
}

#[salsa::tracked(returns(copy))]
fn read_tens(db: &dyn Database, n: N) -> u32 {
    READ.fetch_add(1, Ordering::Relaxed);
    tens(db, n) + 1
}

#[test]
fn computed_again_is_backdated() {
    let mut db = salsa::DatabaseImpl::new();
    let (a, b) = (N::new(&db, 1), N::new(&db, 2));
    assert_eq!(read_tens(&db, a), 1);

    // The same value from another input: backdated to the first revision.
    a.set_value(&mut db).to(2);
    assert_eq!(tens(&db, a), 0);
    assert_eq!(tens(&db, b), 0);

    // The LRU keeps one value: `tens(a)`'s is dropped at the next revision, and computed
    // again it is still of the first revision.
    db.synthetic_write(Durability::LOW);
    assert_eq!(tens(&db, a), 0);
    assert_eq!(TENS.load(Ordering::Relaxed), 4);

    db.synthetic_write(Durability::LOW);
    assert_eq!(read_tens(&db, a), 1);
    assert_eq!(READ.load(Ordering::Relaxed), 1);

    a.set_value(&mut db).to(25);
    assert_eq!(read_tens(&db, a), 3);
    assert_eq!(READ.load(Ordering::Relaxed), 2);
}
