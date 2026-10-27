#![cfg(feature = "inventory")]

//! Tests that code using the builder's durability methods compiles.

use salsa::{Database, Durability, Setter};
use test_log::test;

#[salsa::input]
struct N {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
fn add3(db: &dyn Database, a: N, b: N, c: N) -> u32 {
    add(db, a, b) + c.value(db)
}

#[salsa::tracked(returns(copy))]
fn add(db: &dyn Database, a: N, b: N) -> u32 {
    a.value(db) + b.value(db)
}

#[test]
fn durable_to_less_durable() {
    let mut db = salsa::DatabaseImpl::new();

    let a = N::builder(11).value_durability(Durability::HIGH).new(&db);
    let b = N::builder(22).value_durability(Durability::HIGH).new(&db);
    let c = N::builder(33).value_durability(Durability::HIGH).new(&db);

    // Here, `add3` invokes `add(a, b)`, which yields 33.
    assert_eq!(add3(&db, a, b, c), 66);

    a.set_value(&mut db).with_durability(Durability::LOW).to(11);

    // Here, `add3` invokes `add`, which *still* yields 33, but which
    // is no longer of high durability. Since value didn't change, we might
    // preserve `add3` unchanged, not noticing that it is no longer
    // of high durability.

    assert_eq!(add3(&db, a, b, c), 66);

    // In that case, we would not get the correct result here, when
    // 'a' changes *again*.

    a.set_value(&mut db).to(22);

    assert_eq!(add3(&db, a, b, c), 77);
}

static SUMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[salsa::tracked(returns(copy))]
fn counted_add3(db: &dyn Database, a: N, b: N, c: N) -> u32 {
    SUMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    add(db, a, b) + c.value(db)
}

/// A value that became less durable is not backdated: what read it runs again and takes the
/// lower durability, so a change after reaches it, and a write of low durability alone after
/// that is verified without running it.
#[test]
fn less_durable_runs_again() {
    let mut db = salsa::DatabaseImpl::new();
    let runs = || SUMS.load(std::sync::atomic::Ordering::Relaxed);

    let a = N::builder(11).value_durability(Durability::HIGH).new(&db);
    let b = N::builder(22).value_durability(Durability::HIGH).new(&db);
    let c = N::builder(33).value_durability(Durability::HIGH).new(&db);
    assert_eq!(counted_add3(&db, a, b, c), 66);
    assert_eq!(runs(), 1);

    a.set_value(&mut db).with_durability(Durability::LOW).to(11);
    assert_eq!(counted_add3(&db, a, b, c), 66);
    assert_eq!(runs(), 2);

    a.set_value(&mut db).to(22);
    assert_eq!(counted_add3(&db, a, b, c), 77);
    assert_eq!(runs(), 3);

    db.synthetic_write(Durability::LOW);
    assert_eq!(counted_add3(&db, a, b, c), 77);
    assert_eq!(runs(), 3);
}

#[salsa::tracked]
struct Sum<'db> {
    #[tracked]
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy))]
fn sum<'db>(db: &'db dyn Database, a: N, b: N) -> Sum<'db> {
    Sum::new(db, a.value(db) + b.value(db))
}

static READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[salsa::tracked(returns(copy))]
fn read_sum(db: &dyn Database, a: N, b: N) -> u32 {
    READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    sum(db, a, b).value(db)
}

/// A field of a tracked struct made again less durable is changed: what read it runs again,
/// and a change after reaches it.
#[test]
fn less_durable_field_runs_again() {
    let mut db = salsa::DatabaseImpl::new();
    let runs = || READS.load(std::sync::atomic::Ordering::Relaxed);

    let a = N::builder(11).value_durability(Durability::HIGH).new(&db);
    let b = N::builder(22).value_durability(Durability::HIGH).new(&db);
    assert_eq!(read_sum(&db, a, b), 33);
    assert_eq!(runs(), 1);

    a.set_value(&mut db).with_durability(Durability::LOW).to(11);
    assert_eq!(read_sum(&db, a, b), 33);
    assert_eq!(runs(), 2);

    a.set_value(&mut db).to(22);
    assert_eq!(read_sum(&db, a, b), 44);
    assert_eq!(runs(), 3);

    db.synthetic_write(Durability::LOW);
    assert_eq!(read_sum(&db, a, b), 44);
    assert_eq!(runs(), 3);
}
