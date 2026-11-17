//! A crate's names on the pool (names/scope.rs, drive.md RUNNER 17): the records do not depend
//! on how the crates were batched or how many workers there were (rule 25). A child of
//! main.rs's tests: its helpers are theirs.
//!
//! What the simulation can and cannot show: its contexts run every pool task inline (bstd
//! task.rs, `Layout::Inline`), so this proves the records are a function of each crate's job
//! whatever the rounds hold (one worker: one batch a round; four: up to four batches, so crates
//! of a round share its read records); that a job shares nothing a thread could race on is
//! held by its types (`ScopeJob` is `Sync`: record bytes and read-only views, checked by the
//! compiler at `may_overlap`).

use super::*;
use crate::names::scope_made;
use blaze_ids::ContentId;

/// Four members: `x` and `y` alone, `u` on `x`, `v` on `y` (so `u` and `v` are ready in one
/// round and read different records at the same place of their lists), each with a module
/// file, a `macro_rules!` called across crates and an `env!`.
fn workspace(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"x\", \"y\", \"u\", \"v\"]\nresolver = \"2\"\n");
    for (name, dep, body) in [
        ("x", None, "pub mod m;\n#[macro_export]\nmacro_rules! twice { ($e:expr) => { $e + $e } }\npub const NAME: &str = env!(\"CARGO_PKG_NAME\");\n"),
        ("y", None, "pub mod m;\npub struct Y;\npub const NAME: &str = env!(\"CARGO_PKG_NAME\");\n"),
        ("u", Some("x"), "pub mod m;\npub use x::m::f;\npub fn g() -> u32 { x::twice!(f()) }\n"),
        // `use y::Y`: a name only `y` exports, so a record read in `y`'s stead is an unresolved
        // import (`y::m::f` alone is not: `x` has an `m::f` at the same place, and a type path
        // in a signature is not the scope's to resolve; the control stopped failing that way,
        // 2026-11-16 20:49, once the sysroot's crates joined each list).
        ("v", Some("y"), "pub mod m;\npub use y::m::f;\npub use y::Y;\npub fn g() -> Y { Y }\n"),
    ] {
        let deps = dep.map_or(std::string::String::new(), |dep| std::format!("\n[dependencies]\n{dep} = {{ path = \"../{dep}\" }}\n"));
        write(io, &std::format!("{name}/Cargo.toml"), &std::format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{deps}"));
        write(io, &std::format!("{name}/src/lib.rs"), body);
        write(io, &std::format!("{name}/src/m.rs"), "pub fn f() -> u32 { 1 }\n");
    }
}

/// The workspace checked in a fresh world with `workers`, planting `plant`: each crate's
/// record (subject, out, payload digest), sorted, and the check.
fn records(workers: u32, plant: work::Plant) -> (std::vec::Vec<(ContentId, ContentId, ContentId)>, Checked) {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    workspace(&io);
    scope_made().with(|made| made.borrow_mut().clear());
    WORKERS.with(|w| w.set(workers));
    let checked = check(&io, work::Sizes { plant, ..work::Sizes::USUAL });
    WORKERS.with(|w| w.set(1));
    let mut made = scope_made().with(|made| core::mem::take(&mut *made.borrow_mut()));
    made.sort_unstable();
    made.dedup();
    (made, checked)
}

/// The oracle: both checks clean (every name resolved: an unresolved path is an error) and
/// every crate's record equal byte for byte.
fn pooled_equals_serial(plant: work::Plant) {
    let (serial, one) = records(1, plant);
    let (pooled, four) = records(4, plant);
    assert!(one.failed.is_empty() && four.failed.is_empty(), "{:?} {:?}", one.failed, four.failed);
    assert!(serial.len() >= 4, "only {} crate records:\n{}", serial.len(), one.profile);
    assert_eq!((one.errors, four.errors), (0, 0), "ORACLE: an unresolved name: {:?} / {:?}\n{}", one.words, four.words, four.profile);
    assert_eq!(serial, pooled, "ORACLE: the pooled check wrote other records than the serial one");
}

/// Expected line: `test tests::pooled_scope_tests::a_pooled_check_writes_the_records_a_serial_one_does ... ok`.
#[test]
fn a_pooled_check_writes_the_records_a_serial_one_does() {
    pooled_equals_serial(work::Plant::None);
}

/// Control: with the round's records shared by place in each crate's list, not by key, the
/// oracle fails (`v` reads `x`'s exports where `y`'s belong).
#[test]
#[should_panic(expected = "ORACLE")]
fn the_pooled_records_oracle_catches_records_shared_by_place() {
    pooled_equals_serial(work::Plant::ScopeShareByPlace);
}
