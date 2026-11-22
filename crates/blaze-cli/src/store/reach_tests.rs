//! Every record a later `blaze run` reads is reached from a root, under a reach sweep
//! (`Liveness::Reached`), on blaze-core's simulation (rules 26, 43, 64): two checkouts' programs
//! over one store, each run a process of its own through the code store (`HomeCodeStore`, the
//! one way code reaches the store), its start list written and its program root set by
//! store/roots.rs `keep_program` exactly as own_process.rs `run_own` does at exit; then the
//! first checkout is not built for longer than a root's life, and idle commands fold one
//! cleanup slice each.
//!
//! The oracle, after the sweep:
//! - the second checkout's next run makes 0 (every entry and pointer it reads was kept, the
//!   ones it shares with the first checkout too), and its start list still reads;
//! - the first checkout's own entries, pointers and list are gone;
//! - the store's segments hold at most twice the live bytes plus a fixed slack.
//!
//! The control (rule 23): the program root planted off (`keep_program` not called) makes the
//! same history's last run make its code again, and the oracle says so.
//!
//! The toolchain's code (`CodeClass::Toolchain`, std's instances) is kept by the toolchain's
//! root (store/roots.rs `keep_toolchain`), not by a program's: a NEW checkout that first runs
//! after the checkout that made std's code has expired finds all of it. Its control: the
//! toolchain root planted off makes that checkout make std's code again.
//!
//! Not here: `engine-check` answering "certified" with 0 tasks entered after a sweep. The
//! command's store has no liveness seam a test can set (store.rs `open_for_command` takes
//! plain_place.rs `config`, `Liveness::Everything` until plan row D5), and `Io::simulated` has
//! no clock a test can move past a root's life, a lease and the grace.

use blaze_ladder::code_records::CodeClass;

use std::time::Duration;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{GRACE_NS, LEASE_NS, Lent, Liveness, ROOT_LIFE_NS, Settings as StoreSettings};
use blaze_core::sim::{Faults, Sim};
use blaze_core::wait::Deadline;
use blaze_core::{Completion, Done, Driver, Effect, FileName, Tag as EffectTag};
use blaze_ids::ContentId;
use blaze_ladder::code_records::{CodeKey, CodeStore};

use super::plain_place::{Facts, StoreAlloc, after_answer, config, open, test_cx};
use super::roots::{CleanupLine, keep_program, keep_toolchain};
use crate::code_store::HomeCodeStore;
use crate::start_list::{self, Certificate, HEAD, ListName, ROW, Today};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const MINUTE: Duration = Duration::from_secs(60);
/// A listing tag no store owns.
const OWN: u64 = 1 << 60;

/// The first checkout's program: its own instances are large, so their segment is mostly dead
/// once it expires; the ones it shares with the second are small.
const A: core::ops::Range<u32> = 0..32;
/// The second checkout's program: half shared with the first.
const B: core::ops::Range<u32> = 16..48;
/// Instances only the first checkout's program has.
const A_ONLY: core::ops::Range<u32> = 0..16;

fn dir() -> FileName {
    FileName::new("store").expect("a name")
}

/// One process's store, as `open_for_command` opens it but with the reach sweep on.
fn process(sim: Sim, n: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut cfg = config(dir(), &StoreSettings::default(), Facts { boot: Some(1), nonce: 7300 + n }, alloc);
    cfg.liveness = Liveness::Reached;
    open(sim, cfg, alloc).expect("the store opens")
}

fn back(lent: Lent<Sim, StoreAlloc>) -> Sim {
    lent.into_store().into_driver()
}

/// Instance `n`'s code key (its symbol and every input, modelled by `n`).
fn code(n: u32) -> CodeKey {
    CodeKey(ContentId::of("reach test code key", &n.to_le_bytes()))
}

/// Instance `n`'s one entry: 40 to 340 bytes, 16 KiB more for the first checkout's own.
fn entry(n: u32) -> std::vec::Vec<u8> {
    let long = if A_ONLY.contains(&n) { 16 * 1024 } else { 0 };
    (0..40 + long + (n as usize * 41) % 300).map(|i| (n as usize).wrapping_mul(151).wrapping_add(i) as u8).collect()
}

fn content(n: u32) -> CodeKey {
    CodeKey(ContentId::of("reach test entry", &entry(n)))
}

/// A program run from its own checkout (its own request shape, so its own list and program
/// root, store/roots.rs `ProgramRoot::of`).
#[derive(Clone)]
struct Checkout {
    which: u8,
    program: core::ops::Range<u32>,
}

impl Checkout {
    fn list_name(&self) -> ListName {
        ListName::of(&ContentId::of("reach test shape", &[self.which]))
    }

    /// Its run's today: one certificate a checkout, its run tables two keys of it.
    fn today(&self) -> Today {
        let table = |what: &'static str| CasKey(ContentId::of(what, &[self.which]));
        Today { certificate: Certificate::for_tests([self.which; 32]), name: self.list_name(), tables: [table("reach test ids table"), table("reach test names table")] }
    }

    /// The start list this program's run writes: one code row per instance.
    fn list(&self) -> std::vec::Vec<u8> {
        let mut bytes = std::vec![0u8; HEAD];
        for n in self.program.clone() {
            let mut row = [0u8; ROW];
            row[1..5].copy_from_slice(&n.to_le_bytes());
            row[33..65].copy_from_slice(&code(n).0.0);
            bytes.extend_from_slice(&row);
        }
        bytes
    }
}

/// What one run did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ran {
    made: u32,
    found: u32,
    cleanup: CleanupLine,
}

/// One `blaze run` of `checkout` as process `n`: each instance looked up through its code
/// key's pointer and made (put under its content key, pointed) on a miss; the start list
/// written; the program root set when `keep`; one slice folded; the commit after the answer.
fn run(sim: Sim, n: u64, checkout: Checkout, keep: bool) -> (Sim, Ran) {
    let (sim, ran, _) = run_with(sim, n, checkout, Keep { program: keep, toolchain: true }, 0..0);
    (sim, ran)
}

/// Which roots a run sets at its exit (own_process.rs `run_own` sets both).
#[derive(Clone, Copy)]
struct Keep {
    program: bool,
    toolchain: bool,
}

/// [`run`] with the roots `keep` sets, the program also calling the toolchain's instances
/// `std` (class `Toolchain`, as run_ids.rs `class` answers for a toolchain crate's item):
/// answers what it made of them too.
fn run_with(sim: Sim, n: u64, checkout: Checkout, keep: Keep, std: core::ops::Range<u32>) -> (Sim, Ran, u32) {
    let lent = process(sim, n);
    let (mut made, mut found, mut std_made) = (0, 0, 0);
    let mut codes = HomeCodeStore::new(&lent);
    let called = std.clone().map(|i| (i, CodeClass::Toolchain)).chain(checkout.program.clone().map(|i| (i, CodeClass::Program)));
    for (i, class) in called {
        let hit = match codes.pointed(code(i)).expect("a pointer read") {
            Some(target) => codes.get(target).expect("an entry read") == Some(&entry(i)[..]),
            None => false,
        };
        if hit {
            found += 1;
        } else {
            codes.put(class, content(i), &entry(i)).expect("an entry put");
            codes.point(class, code(i), content(i)).expect("a pointer put");
            match class {
                CodeClass::Toolchain => std_made += 1,
                CodeClass::Program => made += 1,
            }
        }
    }
    let walked = codes.reached();
    let tool_made = codes.toolchain_made();
    let today = checkout.today();
    let list = checkout.list();
    let held = start_list::held(&lent, &today.name).expect("the list read").map(<[u8]>::to_vec);
    start_list::write(&lent, &today.name, held.as_deref(), &list).expect("the list written");
    if keep.program {
        let (_rooted, kept) = keep_program(&lent, &today, Some(&list), &walked).expect("the program's root");
        assert_eq!(kept.stale_rows, 0, "a row just made names no target");
    }
    if keep.toolchain {
        keep_toolchain(&lent, blaze_toolchain::PINNED, &tool_made, &[]).expect("the toolchain's root");
    }
    let after = after_answer(&test_cx(), &lent, 1).expect("the commit after the answer");
    (back(lent), Ran { made, found, cleanup: after.cleanup }, std_made)
}

/// An idle command: one slice folded, its lease let go of.
fn idle(sim: Sim, n: u64) -> Sim {
    let lent = process(sim, n);
    after_answer(&test_cx(), &lent, 1).expect("an idle commit");
    back(lent)
}

/// Which of the first checkout's own instances still read: by content key or by pointer.
fn a_left(sim: Sim, n: u64) -> (Sim, u32, bool) {
    let lent = process(sim, n);
    let mut left = 0;
    for i in A_ONLY {
        let by_content = lent.get(&CasKey(content(i).0)).expect("a read").is_some();
        let by_pointer = !matches!(lent.pointed(blaze_keyed::kind::ReservedTag::Code.tag(),&CasKey(code(i).0)), Ok(None) | Err(blaze_cas::plain::LentError::TargetGone { .. }));
        if by_content || by_pointer {
            left += 1;
        }
    }
    let list = start_list::held(&lent, &Checkout { which: 1, program: A }.list_name()).expect("a list read").is_some();
    after_answer(&test_cx(), &lent, 0).expect("the reader's commit");
    (back(lent), left, list)
}

/// The segments' bytes on disk.
fn disk(sim: &mut Sim) -> u64 {
    let mut effects = std::vec![Effect::List { dir: dir().into(), tag: EffectTag(OWN) }];
    let mut done: std::vec::Vec<Completion> = std::vec::Vec::new();
    while done.is_empty() {
        sim.submit(&mut effects);
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
    }
    let Some(Completion { done: Done::Listed(listed), .. }) = done.pop() else { panic!("a listing") };
    listed.into_iter().filter(|(name, meta)| !meta.dir && name.as_str().contains("/seg-")).map(|(name, _)| sim.contents(&name).len() as u64).sum()
}

/// What the history showed: each claim broken, in words, and the last run of the second
/// checkout. `keep` false is the control.
fn history(keep: bool) -> (std::vec::Vec<std::string::String>, Ran) {
    let a = Checkout { which: 1, program: A };
    let b = Checkout { which: 2, program: B };
    let mut problems = std::vec::Vec::new();
    let mut n = 0u64;
    let mut next = || {
        n += 1;
        n
    };
    let sim = Sim::new(43, Faults::default());
    let (sim, first) = run(sim, next(), a, keep);
    let (mut sim, second) = run(sim, next(), b.clone(), keep);
    if (first.made, second.made, second.found) != (32, 16, 16) {
        problems.push(std::format!("the first runs made {} and {} (found {}), not 32 and 16 (16)", first.made, second.made, second.found));
    }
    // Eight days on, the second checkout runs again (renewing its root); the first does not.
    sim.advance(8 * DAY);
    let (mut sim, again) = run(sim, next(), b.clone(), keep);
    if keep && again.made != 0 {
        problems.push(std::format!("the second checkout's run eight days on made {}", again.made));
    }
    // Past the first checkout's root life: idle commands sweep, a lease and a grace apart.
    sim.advance(Duration::from_nanos(ROOT_LIFE_NS) - 8 * DAY + MINUTE);
    for _ in 0..4 {
        sim = idle(sim, next());
        sim.advance(Duration::from_secs(1));
    }
    sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + MINUTE);
    for _ in 0..4 {
        sim = idle(sim, next());
        sim.advance(Duration::from_secs(1));
    }
    sim.advance(Duration::from_nanos(GRACE_NS) + MINUTE);
    for _ in 0..2 {
        sim = idle(sim, next());
    }
    // The oracle.
    let (sim, left, a_list) = a_left(sim, next());
    if left != 0 || a_list {
        problems.push(std::format!("the expired checkout left {left} of its {} own instances (list kept: {a_list})", A_ONLY.len()));
    }
    let (mut sim, last) = run(sim, next(), b.clone(), keep);
    if last.made != 0 {
        problems.push(std::format!("the live checkout's run after the sweep made {} of {}", last.made, B.len()));
    }
    let live: u64 = B.map(|i| entry(i).len() as u64 + 2 * 32 + 64).sum::<u64>() + b.list().len() as u64;
    let slack = 64 * 1024;
    let on_disk = disk(&mut sim);
    if on_disk > 2 * live + slack {
        problems.push(std::format!("the segments hold {on_disk} bytes, past twice the live {live} plus {slack}"));
    }
    (problems, last)
}

/// The toolchain's instances: std's, called by every checkout's program.
const STD: core::ops::Range<u32> = 100..108;

/// The toolchain history: the first checkout runs (making std's code and its own), then
/// nothing runs for longer than a program root's life (shorter than the toolchain root's) while
/// idle commands fold slices; then a NEW checkout runs for the first time. Answers what that
/// run made of std's code and the slices' cleanup lines. `toolchain` false is the control.
fn toolchain_history(toolchain: bool) -> (u32, std::vec::Vec<std::string::String>) {
    let keep = Keep { program: true, toolchain };
    let mut n = 0u64;
    let mut next = || {
        n += 1;
        n
    };
    let mut problems = std::vec::Vec::new();
    let sim = Sim::new(47, Faults::default());
    let (mut sim, first, std_first) = run_with(sim, next(), Checkout { which: 1, program: A }, keep, STD);
    if (first.made, std_first) != (32, 8) {
        problems.push(std::format!("the first run made {} and {} of std's, not 32 and 8", first.made, std_first));
    }
    // Past the program root's life, inside the toolchain root's (twice it): idle commands sweep,
    // a lease and a grace apart.
    sim.advance(Duration::from_nanos(ROOT_LIFE_NS) + MINUTE);
    for _ in 0..4 {
        sim = idle(sim, next());
        sim.advance(Duration::from_secs(1));
    }
    sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + MINUTE);
    for _ in 0..4 {
        sim = idle(sim, next());
        sim.advance(Duration::from_secs(1));
    }
    sim.advance(Duration::from_nanos(GRACE_NS) + MINUTE);
    for _ in 0..2 {
        sim = idle(sim, next());
    }
    // The program root expired: the first checkout's own instances went (the sweep ran).
    let (sim, left, _) = a_left(sim, next());
    if left != 0 {
        problems.push(std::format!("the expired checkout left {left} of its {} own instances: no sweep ran", A_ONLY.len()));
    }
    let (_sim, _fresh, std_made) = run_with(sim, next(), Checkout { which: 3, program: 48..56 }, keep, STD);
    (std_made, problems)
}

/// Asserts: a checkout that first runs after the one that made std's code expired makes none
/// of it: the toolchain root kept it past the program root.
/// Prints "reach toolchain: a new checkout after the first expired made 0 of std's 8".
#[test]
fn std_s_code_outlives_the_program_that_made_it() {
    let (std_made, problems) = toolchain_history(true);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(std_made, 0, "the new checkout made std's code again");
    std::println!("reach toolchain: a new checkout after the first expired made {std_made} of std's {}", STD.len());
}

/// The control (rule 23): no toolchain root, so std's code goes with the program root that
/// made it, and the new checkout makes it again.
/// Prints "reach toolchain control: no toolchain root: the new checkout made 8 of std's 8".
#[test]
fn a_toolchain_root_planted_off_makes_std_s_code_again() {
    let (std_made, problems) = toolchain_history(false);
    assert!(problems.is_empty(), "{problems:?}");
    assert!(std_made > 0, "std's code was kept without a toolchain root: the cause is not the root");
    std::println!("reach toolchain control: no toolchain root: the new checkout made {std_made} of std's {}", STD.len());
}

/// Asserts: after the first checkout's root expired and slices folded, the live checkout's run
/// makes nothing, the expired checkout's records are gone, and the disk holds at most twice the
/// live bytes plus slack.
/// Prints "reach: live checkout made 0 after the sweep; expired checkout's 16 own instances gone; <the cleanup line>".
#[test]
fn every_record_a_later_run_reads_is_reached_from_a_root() {
    let (problems, last) = history(true);
    assert!(problems.is_empty(), "{problems:?}");
    assert!(last.cleanup.roots >= 1 && last.cleanup.indexed > 0, "the run's cleanup line names no indexed root: {}", last.cleanup);
    std::println!("reach: live checkout made 0 after the sweep; expired checkout's {} own instances gone; {}", A_ONLY.len(), last.cleanup);
}

/// The control (rule 23): no program root, so the sweep lets every run's code go, and the live
/// checkout's run after it makes its code again.
/// Prints "reach control: no program root: the live checkout's run made N".
#[test]
fn a_program_root_planted_off_makes_the_code_again() {
    let (problems, last) = history(false);
    assert!(last.made > 0, "the control kept the code without a root: the cause is not the root ({problems:?})");
    assert!(problems.iter().any(|it| it.contains("after the sweep made")), "the oracle did not catch the missing root: {problems:?}");
    std::println!("reach control: no program root: the live checkout's run made {}", last.made);
}
