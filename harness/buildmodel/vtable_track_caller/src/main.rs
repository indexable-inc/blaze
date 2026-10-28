//! A by-value method needs a receiver shim when its concrete receiver is passed in
//! registers or is zero-sized. Only a tracked trait passes a caller through its vtable;
//! an impl-only attribute uses the trait method's declaration.
#![feature(unsized_fn_params)]
#![allow(incomplete_features, internal_features)]

use std::{
    hint::black_box,
    panic::Location,
    sync::atomic::{AtomicU64, Ordering::SeqCst},
};

static DROPS: AtomicU64 = AtomicU64::new(0);

struct Guard(u64);

impl Drop for Guard {
    fn drop(&mut self) {
        DROPS.fetch_add(self.0, SeqCst);
    }
}

trait Payload {
    fn take(self) -> (u64, Option<Box<Guard>>);
}

struct Direct(Box<Guard>);
struct Pair(Box<Guard>, Box<Guard>);
struct Empty;
struct Indirect(Box<Guard>, [u64; 4]);

impl Drop for Empty {
    fn drop(&mut self) {
        DROPS.fetch_add(4, SeqCst);
    }
}

impl Payload for Direct {
    fn take(self) -> (u64, Option<Box<Guard>>) {
        (self.0.0, Some(self.0))
    }
}

impl Payload for Pair {
    fn take(self) -> (u64, Option<Box<Guard>>) {
        let Pair(kept, discarded) = self;
        let value = kept.0 * 10 + discarded.0;
        drop(discarded);
        (value, Some(kept))
    }
}

impl Payload for Empty {
    fn take(self) -> (u64, Option<Box<Guard>>) {
        (4, None)
    }
}

impl Payload for Indirect {
    fn take(self) -> (u64, Option<Box<Guard>>) {
        (self.0.0 + self.1[0] + self.1[3], Some(self.0))
    }
}

// The result is returned through an sret pointer, before the receiver pointer.
struct Report {
    caller: &'static Location<'static>,
    value: u64,
    kept: Option<Box<Guard>>,
}

const DEFINITION_CONSUME: u32 = line!() + 2;
trait Definition {
    fn consume(self, bias: u64, extra: u32) -> Report;
    fn line(self, bias: u32) -> u32;
}

struct ImplOnly<T>(T);

impl<T: Payload> Definition for ImplOnly<T> {
    #[track_caller]
    fn consume(self, bias: u64, extra: u32) -> Report {
        let caller = Location::caller();
        let (value, kept) = self.0.take();
        Report { caller, value: value + bias + extra as u64, kept }
    }

    #[track_caller]
    fn line(self, bias: u32) -> u32 {
        Location::caller().line() + bias
    }
}

trait Forwarded {
    #[track_caller]
    fn consume(self, bias: u64, extra: u32) -> Report;
    #[track_caller]
    fn line(self, bias: u32) -> u32;
}

struct Inherited<T>(T);

impl<T: Payload> Forwarded for Inherited<T> {
    // Tracking is inherited from the trait even without an attribute on this impl.
    fn consume(self, bias: u64, extra: u32) -> Report {
        let caller = Location::caller();
        let (value, kept) = self.0.take();
        Report { caller, value: value + bias + extra as u64, kept }
    }

    fn line(self, bias: u32) -> u32 {
        Location::caller().line() + bias
    }
}

#[inline(never)]
#[track_caller]
fn forwarded(value: Box<dyn Forwarded>) -> Report {
    let expected = Location::caller();
    let report = value.consume(7, 11);
    assert_eq!(report.caller.file(), expected.file());
    assert_eq!(report.caller.line(), expected.line());
    assert_eq!(report.caller.column(), expected.column());
    report
}

#[inline(never)]
#[track_caller]
fn forwarded_line(value: Box<dyn Forwarded>) {
    assert_eq!(value.line(17), Location::caller().line() + 17);
}

fn check_report(report: Report, value: u64, before: u64, after: u64) {
    assert_eq!(report.value, value + 18);
    assert_eq!(DROPS.load(SeqCst), before);
    assert_eq!(report.kept.as_ref().map(|it| it.0), if after > before { Some(1) } else { None });
    drop(report);
    assert_eq!(DROPS.load(SeqCst), after);
}

fn check<T: Payload + 'static>(make: fn() -> T, value: u64, before: u64, after: u64) {
    DROPS.store(0, SeqCst);
    let object: Box<dyn Definition> = black_box(Box::new(ImplOnly(make())));
    let report = object.consume(7, 11);
    assert_eq!(report.caller.file(), file!());
    assert_eq!(report.caller.line(), DEFINITION_CONSUME);
    assert_eq!(report.caller.column(), 5);
    check_report(report, value, before, after);

    DROPS.store(0, SeqCst);
    let object: Box<dyn Definition> = black_box(Box::new(ImplOnly(make())));
    assert_eq!(object.line(17), DEFINITION_CONSUME + 1 + 17);
    assert_eq!(DROPS.load(SeqCst), after);

    DROPS.store(0, SeqCst);
    let object: Box<dyn Forwarded> = black_box(Box::new(Inherited(make())));
    check_report(forwarded(object), value, before, after);

    DROPS.store(0, SeqCst);
    let object: Box<dyn Forwarded> = black_box(Box::new(Inherited(make())));
    forwarded_line(object);
    assert_eq!(DROPS.load(SeqCst), after);
}

fn direct() -> Direct {
    Direct(Box::new(Guard(1)))
}

fn pair() -> Pair {
    Pair(Box::new(Guard(1)), Box::new(Guard(2)))
}

fn empty() -> Empty {
    Empty
}

fn indirect() -> Indirect {
    Indirect(Box::new(Guard(1)), [10, 20, 30, 40])
}

fn main() {
    check(direct, 1, 0, 1);
    check(pair, 12, 2, 3);
    check(empty, 4, 4, 4);
    check(indirect, 51, 0, 1);
    println!("vtable caller locations and ownership agree");
}
