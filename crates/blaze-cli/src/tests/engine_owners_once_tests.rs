//! A CRATE'S LISTING ONCE A REQUEST (engine_heads.rs `Listings`, lane OW1), at the engine's
//! seam. A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE FLOOR: one walk of a crate's heads (`heads_of_crate`) a request, however many head
//! tasks, split jobs, impl tables and Bodies stretches place a read in it. A one-crate
//! workspace whose heads read each other, checked cold on one worker: the profile's count of
//! listings made equals the crates listed (ratio 1.00), and is far below its head tasks.
//! CONTROL (rule 23): `ListingsPlant::PerAsk` takes the table away, each head task lists its
//! crate for itself, and the count rises to at least one a task.

use super::engine_check_tests::{engine_opts, opts};
use super::*;

use crate::engine_heads::{HeadJobs, ListingsPlant, UnitCounts};

const WORKSPACE: &str = "[workspace]\nmembers = [\"one\"]\nresolver = \"2\"\n";
const MANIFEST: &str = "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
/// Ten heads that read each other: two structs, an enum, a trait and its impl, a constant and
/// four fns over them.
const LIB: &str = "pub struct P {\n    pub x: u32,\n}\n\npub struct Q {\n    pub p: P,\n}\n\npub enum E {\n    A(P),\n    B(Q),\n}\n\npub trait Size {\n    fn size(&self) -> u32;\n}\n\nimpl Size for P {\n    fn size(&self) -> u32 {\n        self.x\n    }\n}\n\npub const ONE: u32 = 1;\n\npub fn make(x: u32) -> P {\n    P { x }\n}\n\npub fn wrap(p: P) -> Q {\n    Q { p }\n}\n\npub fn get(q: &Q) -> u32 {\n    q.p.size() + ONE\n}\n\npub fn pick(e: E) -> u32 {\n    match e {\n        E::A(p) => p.x,\n        E::B(q) => get(&q),\n    }\n}\n";

fn one_crate(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "one/Cargo.toml", MANIFEST);
    write(&io, "one/src/lib.rs", LIB);
    io
}

/// One cold request on one worker (no race), its head jobs as `jobs` say: the head counts.
fn request(io: &Io, jobs: HeadJobs) -> UnitCounts {
    let mut o = opts(io, 1, false);
    o.head_jobs = jobs;
    let (_, checked) = engine_opts(io, o);
    checked.seeds.units
}

/// Expected lines: `listings 1 for T tasks (rule: 1 crates, 0 twice, A asks)` with T >= 10,
/// then `listings M for T tasks (control: ...)` with M >= T.
#[test]
fn a_crate_is_listed_once_a_request() {
    let rule = request(&one_crate(31), HeadJobs::RULE);
    let l = rule.listings;
    println!("listings {} for {} tasks (rule: {} crates, {} twice, {} asks)", l.made, rule.tasks, l.crates, l.twice, l.asks);
    assert!(rule.tasks >= 10, "the fixture's head tasks ran: {} (expected at least its ten heads)", rule.tasks);
    assert!(l.crates >= 1, "the crate was listed into the table: {l:?}");
    assert_eq!(l.twice, 0, "one worker: no two jobs make a listing at once: {l:?}");
    assert_eq!(l.made, l.crates, "THE FLOOR: each listing made once a request: {l:?}");
    assert!(l.made < rule.tasks, "a listing is not made a task: {} made for {} tasks", l.made, rule.tasks);
    assert_eq!(l.unequal + l.full, 0, "no listing refused: {l:?}");

    // CONTROL: the table taken away, each head task lists its crate itself.
    let planted = request(&one_crate(31), HeadJobs { listings: ListingsPlant::PerAsk, ..HeadJobs::RULE });
    let p = planted.listings;
    println!("listings {} for {} tasks (control: {} crates, {} asks)", p.made, planted.tasks, p.crates, p.asks);
    assert_eq!(planted.tasks, rule.tasks, "the plant changes no task: {} against {}", planted.tasks, rule.tasks);
    assert_eq!(p.crates, 0, "under the plant the table holds nothing: {p:?}");
    assert!(p.made >= planted.tasks, "CONTROL: a listing a task, {} made for {} tasks", p.made, planted.tasks);
}
