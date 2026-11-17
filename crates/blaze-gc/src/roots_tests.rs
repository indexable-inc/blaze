//! The home's pass under simulation (AGENTS.md rules 43 and 64): a model home of named entries
//! and a trash, the real core ([`roots::account`], [`roots::grace_at`], [`roots::trash_name`])
//! deciding, the driver's part played on the model, a crash after every rename and every unlink,
//! then a restart. Seeded: a failure names its seed, and the same seed plays the same steps.
//!
//! The model stands in for `roots_fs.rs`'s file system work only (a listing, a rename into
//! trash) and for the collector's trash phase (an unlink that a crash leaves half done, the
//! entry still in trash). It is its own small model, not `blaze_core::sim::Sim`: the simulated
//! host has files by name and no directory rename, which is the one operation this pass does.

use std::collections::BTreeMap;

use crate::roots::{self, Entry, Fate, Keep, Owner, Rules};

const S: u64 = 1_000_000_000;
const MINUTE: u64 = 60 * S;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
/// The fake clock starts here: far enough from 0 that "long ago" is a time.
const T0: u64 = 1_000 * DAY;

/// splitmix64: the simulation's one source of choice.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
    bytes: u64,
    used_ns: u64,
    held: bool,
}

/// A home's entries by name, and its trash's.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    home: BTreeMap<String, Node>,
    trash: BTreeMap<String, Node>,
    /// Bytes unlinked so far.
    freed: u64,
}

impl Model {
    fn bytes(&self) -> u64 {
        self.home.values().chain(self.trash.values()).map(|it| it.bytes).sum::<u64>() + self.freed
    }

    fn list(&self) -> Vec<Entry> {
        self.home
            .iter()
            .map(|(name, it)| Entry {
                name: name.clone(), // clone: the listing's own name
                bytes: it.bytes,
                used_ns: it.used_ns,
                held: it.held,
            })
            .collect()
    }
}

/// A bug planted in the driver's part, which the oracle must catch (the controls).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Planted {
    None,
    /// The grace is not shortened under pressure.
    GraceWhole,
    /// What no blaze made is left out of the listing, so out of the account.
    ForeignLeftOut,
    /// A held lock is not seen.
    HeldUnseen,
}

/// The names a home may hold: every owner, and the names found on the machine whose disk
/// filled (`coord`, `runner-blaze`, `lanes`, a backup of `v2`).
const NAMES: &[&str] = &[
    "build",
    "v2",
    "std",
    "sources",
    "gc.json",
    "coord",
    "runner-blaze",
    "lanes",
    "workers",
    "checkers",
    "store",
    "daemon.log",
    "v2.store-format-3.backup-20261111",
    "v2.store-format-2.backup-20261001",
];

const AGES: &[u64] = &[30 * S, 10 * MINUTE, 2 * HOUR, 3 * DAY, 10 * DAY, 20 * DAY];
const PRESSURES: &[u32] = &[0, 300, 600, 1000];

fn pick<T: Copy>(rng: &mut Rng, of: &[T]) -> T {
    let count = u64::try_from(of.len()).unwrap_or(1);
    of[usize::try_from(rng.below(count)).unwrap_or(0)]
}

fn home_of(rng: &mut Rng) -> Model {
    let mut home = BTreeMap::new();
    for name in NAMES {
        if rng.below(4) == 0 {
            continue;
        }
        let node = Node {
            bytes: 1 + rng.below(1000),
            used_ns: T0 - pick(rng, AGES),
            held: rng.below(5) == 0,
        };
        home.insert((*name).to_owned(), node);
    }
    Model { home, trash: BTreeMap::new(), freed: 0 }
}

fn rules(planted: Planted) -> Rules {
    match planted {
        // The planted bug: the least grace is the whole grace, so pressure shortens nothing.
        Planted::GraceWhole => Rules { least_grace_ns: HOUR, ..Rules::default() },
        _ => Rules::default(),
    }
}

/// What a pass may not touch, by the stated rules (never by the code under test's verdicts):
/// what this pass does not own, what a lock holds, and what was written within the grace at
/// this pressure.
fn protected(model: &Model, now: u64, pressure: u32) -> Vec<String> {
    let whole = Rules::default();
    let grace = roots::grace_at(whole.grace_ns, whole.least_grace_ns, pressure);
    model
        .home
        .iter()
        .filter(|(name, it)| {
            !roots::owner_of(name).0.collected_here()
                || it.held
                || now.saturating_sub(it.used_ns) < grace
        })
        .map(|(name, _)| name.clone()) // clone: the oracle's own list
        .collect()
}

/// What a pass at full pressure must be able to free: this pass's own, not held, past the
/// least grace.
fn garbage(model: &Model, now: u64) -> u64 {
    let least = Rules::default().least_grace_ns;
    model
        .home
        .iter()
        .filter(|(name, it)| {
            roots::owner_of(name).0.collected_here()
                && !it.held
                && now.saturating_sub(it.used_ns) >= least
        })
        .map(|(_, it)| it.bytes)
        .sum()
}

/// One pass and the trash phase after it on `model`, stopping (a crash) once `steps_left`
/// renames and unlinks are done; an unlink the crash interrupts leaves its entry in trash with
/// half its bytes gone. The steps it would take uncrashed, and what the account said.
fn pass(
    model: &mut Model,
    now: u64,
    pressure: u32,
    to_free: u64,
    planted: Planted,
    mut steps_left: Option<u32>,
) -> (u32, roots::Account) {
    let mut entries = model.list();
    match planted {
        Planted::ForeignLeftOut => {
            entries.retain(|it| roots::owner_of(&it.name).0 != Owner::Foreign);
        }
        Planted::HeldUnseen => entries.iter_mut().for_each(|it| it.held = false),
        Planted::None | Planted::GraceWhole => {}
    }
    let account = roots::account(&entries, now, pressure, to_free, &rules(planted));
    let mut steps = 0u32;
    let mut crashed = false;
    let mut step = |steps: &mut u32, crashed: &mut bool| -> bool {
        if *crashed {
            return false;
        }
        if steps_left == Some(0) {
            *crashed = true;
            return false;
        }
        steps_left = steps_left.map(|it| it - 1);
        *steps += 1;
        true
    };
    for (at, row) in account.going().enumerate() {
        if !step(&mut steps, &mut crashed) {
            break;
        }
        let Some(node) = model.home.remove(&row.name) else { continue };
        let to = roots::trash_name(now, u32::try_from(at).unwrap_or(u32::MAX), &row.name);
        assert!(crate::fs::is_trash_name(&to), "{to} is no name of trash");
        assert!(model.trash.insert(to, node).is_none(), "a rename onto an entry of trash");
    }
    // The trash phase: every entry there, this pass's and an earlier crashed one's.
    // clone: the names, so the trash can change as each is unlinked
    let names: Vec<String> = model.trash.keys().cloned().collect();
    for name in names {
        if crashed {
            break;
        }
        if !step(&mut steps, &mut crashed) {
            // The crash lands inside this unlink: half of it is gone, the entry is still trash.
            if let Some(node) = model.trash.get_mut(&name) {
                let half = node.bytes / 2;
                node.bytes -= half;
                model.freed += half;
            }
            break;
        }
        if let Some(node) = model.trash.remove(&name) {
            model.freed += node.bytes;
        }
    }
    (steps, account)
}

/// The oracle after any step. The invariant: nothing protected is gone or changed, nothing in
/// the home is partly removed, no byte is lost from the count, and the account counts every
/// byte of the home it listed.
fn check(
    before: &Model,
    after: &Model,
    kept: &[String],
    account: &roots::Account,
    seed: u64,
) -> Result<(), String> {
    for name in kept {
        if after.home.get(name) != before.home.get(name) {
            return Err(format!("seed {seed}: {name} was protected and is gone or changed"));
        }
    }
    for (name, node) in &after.home {
        if before.home.get(name) != Some(node) {
            return Err(format!("seed {seed}: {name} in the home is not as it was"));
        }
    }
    if after.bytes() != before.bytes() {
        return Err(format!("seed {seed}: {} bytes became {}", before.bytes(), after.bytes()));
    }
    let listed: u64 = before.home.values().map(|it| it.bytes).sum();
    if account.total_bytes != listed {
        return Err(format!(
            "seed {seed}: the account holds {} bytes of the home's {listed}",
            account.total_bytes
        ));
    }
    Ok(())
}

/// One seed: a home, a pass crashed after every count of steps, each followed by a restart
/// that runs whole; the oracle after each. Then the dual on the uncrashed pass: at full
/// pressure with bytes to free and garbage there, bytes are freed, and either enough were or
/// no garbage is left.
fn play(seed: u64, planted: Planted) -> Result<(), String> {
    let mut rng = Rng(seed);
    let start = home_of(&mut rng);
    let pressure = pick(&mut rng, PRESSURES);
    let to_free = match pressure {
        0 => 0,
        _ => 1 + rng.below(3000),
    };
    let kept = protected(&start, T0, pressure);
    let mut whole = start.clone(); // clone: each play starts from the same home
    let (steps, account) = pass(&mut whole, T0, pressure, to_free, planted, None);
    check(&start, &whole, &kept, &account, seed)?;
    if !whole.trash.is_empty() {
        return Err(format!("seed {seed}: a whole pass left {} entries in trash", whole.trash.len()));
    }
    if pressure == roots::FULL {
        let there = garbage(&start, T0);
        let left = garbage(&whole, T0);
        if there > 0 && whole.freed == 0 {
            return Err(format!(
                "seed {seed}: full pressure, {there} bytes of garbage, {to_free} to free, none freed"
            ));
        }
        if whole.freed < to_free && left > 0 {
            return Err(format!(
                "seed {seed}: {} of {to_free} bytes freed with {left} bytes of garbage left",
                whole.freed
            ));
        }
    }
    for crash_after in 0..steps {
        let mut model = start.clone(); // clone: each crash point starts from the same home
        let (_, account) = pass(&mut model, T0, pressure, to_free, planted, Some(crash_after));
        check(&start, &model, &kept, &account, seed)?;
        // The restart, a second later: what the crash left in trash is unlinked, nothing else
        // protected at either time is touched.
        let crashed = model.clone(); // clone: the state the restart starts from
        let kept_now = protected(&crashed, T0 + S, pressure);
        let (_, account) = pass(&mut model, T0 + S, pressure, to_free, planted, None);
        check(&crashed, &model, &kept_now, &account, seed)?;
        if !model.trash.is_empty() {
            return Err(format!(
                "seed {seed}: after a crash at step {crash_after} and a restart, trash holds {}",
                model.trash.len()
            ));
        }
    }
    Ok(())
}

/// The invariant: under every seed, and a crash after every rename and unlink, nothing a
/// lock, the grace or another owner keeps is removed or changed, nothing outside trash is
/// partly removed, and a restart empties trash; its dual: at full pressure, garbage is freed.
#[test]
fn nothing_kept_is_removed_and_garbage_goes_under_pressure_at_every_crash_point() {
    for seed in 0..2000 {
        play(seed, Planted::None).unwrap_or_else(|why| panic!("{why}"));
    }
}

/// The controls: each planted bug is caught by the same oracle under some seed.
#[test]
fn the_oracle_catches_each_planted_bug() {
    for planted in [Planted::GraceWhole, Planted::ForeignLeftOut, Planted::HeldUnseen] {
        let caught = (0..2000).find_map(|seed| play(seed, planted).err());
        assert!(caught.is_some(), "{planted:?} was not caught by any seed");
    }
}

/// The machine whose disk filled, as listed on 2026-11-12: `coord/` and `runner-blaze/` are no
/// blaze's and are named as holding space; the backup and the names of the daemon before v2 go
/// under pressure; `build/`, `v2/` and `std/` are left to their collectors; every byte counts.
#[test]
fn the_home_that_filled_a_disk_is_accounted_whole() {
    const GB: u64 = 1_000_000_000;
    let entry = |name: &str, gb: u64, days: u64| Entry {
        name: name.to_owned(),
        bytes: gb * GB,
        used_ns: T0 - days * DAY,
        held: false,
    };
    let entries = vec![
        entry("coord", 578, 2),
        entry("build", 464, 0),
        entry("v2.store-format-3.backup-20261111", 208, 1),
        entry("runner-blaze", 70, 9),
        entry("workers", 9, 1),
        entry("store", 4, 1),
        entry("lanes", 1, 10),
        entry("v2", 1, 0),
    ];
    let account = roots::account(&entries, T0, 1000, 1_100 * GB, &Rules::default());
    assert_eq!(account.total_bytes, 1_335 * GB);
    assert_eq!(account.foreign_bytes, (578 + 70 + 1) * GB);
    assert_eq!(account.going_bytes, (208 + 9 + 4) * GB);
    let fate = |name: &str| {
        &account.rows.iter().find(|it| it.name == name).unwrap_or_else(|| panic!("{name}")).fate
    };
    assert!(matches!(fate("v2.store-format-3.backup-20261111"), Fate::Go(_)));
    assert_eq!(*fate("coord"), Fate::Keep(Keep::Foreign));
    assert_eq!(*fate("build"), Fate::Keep(Keep::Owned));
    // What holds the space, largest first, each with its reason.
    let holders: Vec<&str> = account.holders(3).iter().map(|it| it.name.as_str()).collect();
    assert_eq!(holders, ["coord", "build", "runner-blaze"]);
    let lines = account.lines("/home", 3, true);
    assert!(lines.iter().any(|it| it.contains("/home/coord") && it.contains("no blaze makes")));
    // Without pressure the backup stays for its lifetime, and goes once past it.
    let calm = roots::account(&entries, T0, 0, 0, &Rules::default());
    assert_eq!(calm.going_bytes, 0);
    let later = roots::account(&entries, T0 + 7 * DAY, 0, 0, &Rules::default());
    assert_eq!(later.going_bytes, 208 * GB);
}

/// The grace is whole up to half pressure, a minute at full, and never grows with pressure.
#[test]
fn the_grace_shrinks_from_half_pressure_to_a_minute_at_full() {
    assert_eq!(roots::grace_at(HOUR, MINUTE, 0), HOUR);
    assert_eq!(roots::grace_at(HOUR, MINUTE, 500), HOUR);
    assert_eq!(roots::grace_at(HOUR, MINUTE, 750), (HOUR + MINUTE) / 2);
    assert_eq!(roots::grace_at(HOUR, MINUTE, 1000), MINUTE);
    assert_eq!(roots::grace_at(HOUR, MINUTE, 4000), MINUTE);
    // A grace shorter than the least is not lengthened.
    assert_eq!(roots::grace_at(S, MINUTE, 1000), S);
    let mut last = u64::MAX;
    for pressure in 0..=1000 {
        let grace = roots::grace_at(DAY, MINUTE, pressure);
        assert!(grace <= last, "the grace grew at {pressure}");
        last = grace;
    }
}

/// Names: a registered one has its owner, a backup is one only of a registered root with a
/// stamp of digits, and anything else is no blaze's; the account does not depend on the order
/// of the listing.
#[test]
fn names_have_one_owner_and_the_account_is_the_same_in_any_order() {
    assert_eq!(roots::owner_of("build").0, Owner::Builds);
    assert_eq!(roots::owner_of("v2.store-format-3.backup-20261111").0, Owner::Backup);
    assert_eq!(roots::backup_of("v2.store-format-3.backup-20261111"), Some("v2"));
    for foreign in ["coord", "v2.backup-1", "mine.x.backup-20261111", "v2.x.backup-", "v2.x.backup-1a", "store.x.backup-1"] {
        assert_eq!(roots::owner_of(foreign).0, Owner::Foreign, "{foreign}");
    }
    assert_eq!(roots::unregistered(["build", "coord", "v2"].into_iter()), ["coord"]);
    for root in roots::ROOTS {
        assert!(roots::fits_trash(root.name), "{} cannot be an entry of trash", root.name);
        assert_eq!(roots::ROOTS.iter().filter(|it| it.name == root.name).count(), 1, "{}", root.name);
    }
    let mut rng = Rng(7);
    let model = home_of(&mut rng);
    let listed = model.list();
    let mut reversed = listed.clone(); // clone: the same listing in another order
    reversed.reverse();
    let rules = Rules::default();
    assert_eq!(
        roots::account(&listed, T0, 1000, 500, &rules),
        roots::account(&reversed, T0, 1000, 500, &rules)
    );
}
