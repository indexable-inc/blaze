//! THE METER CHANGES NO STORED BYTE (D169, rule 25): one workspace checked twice on two fresh
//! worlds of the same files, once with the meter reading and once with its readers stopped
//! (`blaze_meter::testing::stop`): the digest of every record put (key, length and bytes:
//! engine_store.rs `Ledger::digest`), the printed lines but the closing one (which holds the
//! time), the listed findings and the status are equal.
//!
//! CONTROL (rule 23): the switch did something: the stretch around the run with the meter on
//! counted store asks, bytes hashed and allocations, and the one with it stopped counted none;
//! were both runs metered alike, the equality above would prove nothing about the meter.
//!
//! The switch is the whole process's: another test reading the meter at that instant sees
//! zeros, which changes no record of its own either (the property here).
//!
//! A child of main.rs's tests, beside engine_check_tests.rs, whose helpers it uses.

use super::*;
use super::engine_check_tests::{engine_opts, opts};

use blaze_meter::{Count, Meter};

/// Two crates, `b` over `a`, with a type error, so the run checks bodies and prints findings.
fn workspace(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", "pub fn one() -> u8 {\n    1\n}\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(io, "b/src/lib.rs", "pub fn h() -> u8 {\n    a::one()\n}\n\npub fn wrong() -> u8 {\n    true\n}\n");
}

/// What one check stored and said, and what the meter saw of the stretch on this thread.
struct Run {
    records: blaze_ids::ContentId,
    lines: std::vec::Vec<std::string::String>,
    ids: std::vec::Vec<(std::string::String, std::string::String)>,
    status: u8,
    used: blaze_meter::Used,
}

fn run(stopped: bool) -> Run {
    blaze_meter::testing::stop(stopped);
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    workspace(&io);
    let mut o = opts(&io, 1, true);
    o.toolchain = lang_shelf(&io);
    let before = stdout(&io).len();
    let metered = Meter::now();
    let (ids, checked) = engine_opts(&io, o);
    // Both snapshots are inside one throw of the switch (one epoch): a pair, never refused.
    let used = metered.used().expect("one thread's pair in one epoch of the switch");
    blaze_meter::testing::stop(false);
    let said = stdout(&io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = said.lines().map(str::to_owned).collect();
    lines.pop();
    Run { records: checked.records.digest(), lines, ids, status: checked.status.0, used }
}

#[test]
fn stored_bytes_are_equal_with_the_meter_on_and_off() {
    let on = run(false);
    let off = run(true);
    assert_eq!(on.records, off.records, "the meter on and off put the same records, key and bytes (rule 25)");
    assert_eq!(on.lines, off.lines, "the meter on and off print the same lines");
    assert_eq!(on.ids, off.ids, "the meter on and off list the same findings");
    assert_eq!(on.status, off.status, "the meter on and off end with the same status");

    // CONTROL: the switch is seen by the readers.
    println!("meter on, this thread: {}", on.used);
    assert!(on.used.allocations().is_ok_and(|allocs| allocs > 0), "control: the run metered counted allocations: {}", on.used);
    assert!(on.used.count(Count::Hashed) > 0, "control: the run metered counted bytes hashed: {}", on.used);
    assert!(on.used.count(Count::StoreGets) + on.used.count(Count::StorePuts) > 0, "control: the run metered counted the store: {}", on.used);
    assert_eq!(off.used.allocations().expect("a stopped stretch sums nothing") + off.used.count(Count::Hashed) + off.used.count(Count::StorePuts), 0, "control: the run stopped read nothing: {}", off.used);
}
