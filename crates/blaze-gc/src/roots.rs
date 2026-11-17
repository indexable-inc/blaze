//! Who owns each byte under a blaze home: the registry of what may be right under it, and the
//! account a pass makes of everything found there (docs/gc.md, "Every byte has an owner").
//!
//! The invariant: **every entry right under a home has exactly one [`Owner`], every byte under
//! the home is counted to its entry's owner, and an entry is renamed into trash only when its
//! owner is one this pass collects ([`Owner::Backup`], [`Owner::Retired`]), nothing holds it and
//! it is past the grace at this pressure ([`grace_at`]).** What another collector owns (`build/`,
//! `std/`, `v2/`, the trash) is counted and left to it; what no blaze made ([`Owner::Foreign`])
//! is counted, never removed, and named as holding space with the reason it stays.
//!
//! A sans-IO core: [`account`] is a function of what the driver listed ([`Entry`]), the time and
//! the pressure. The driver (`roots_fs.rs`) lists, renames and nothing else; the trash it
//! renames into is unlinked by the collector's own trash phase, so there is one unlink path and
//! one crash story (`fs.rs`, `plan.rs`).
#![blaze::sans_io]

use std::fmt;

const SECOND: u64 = 1_000_000_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

/// Pressure, permille, past which the graces shrink ([`grace_at`]): the same half past which a
/// slice runs while builds run (`pace.rs`).
pub const SHRINK_FROM: u32 = 500;
/// Full pressure, permille.
pub const FULL: u32 = 1000;

/// Who answers for an entry right under a home.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Owner {
    /// `build/`: the collector of build dirs and their profiles (`plan.rs`).
    Builds,
    /// `std/`: the collector of the shared standard library stores (`plan.rs`).
    Std,
    /// `v2/`: the daemon's state; its store is collected record by record (blaze-cas), the rest
    /// is bounded by its writers.
    State,
    /// `.blaze-trash/`: unlinked by the collector's trash phase.
    Trash,
    /// What blaze fetched and can fetch again (`sources/`, `toolchains/`): counted, with no
    /// collector yet, so it is named as held for that reason.
    Fetched,
    /// A file of fixed, small size: a socket, a lock, the collector's settings.
    Fixed,
    /// A copy of a registered root set aside by a format change
    /// (`<root>.<label>.backup-<date>`): this pass's, with a stated lifetime.
    Backup,
    /// A name an earlier blaze wrote under the home and this one does not: this pass's, once
    /// nothing has written it for the time to live.
    Retired,
    /// Nothing blaze makes has this name: not blaze's to remove.
    Foreign,
}

impl Owner {
    /// Whether this pass decides the entry's fate; every other owner's entries are counted only.
    pub const fn collected_here(self) -> bool {
        matches!(self, Owner::Backup | Owner::Retired)
    }
}

/// One name blaze may make right under a home.
#[derive(Clone, Copy, Debug)]
pub struct Root {
    pub name: &'static str,
    pub owner: Owner,
    /// What it is and what writes it, for a report.
    pub what: &'static str,
}

/// Every name blaze makes right under a home, and the names earlier blazes made. A name blaze
/// writes that is not here is a bug this list's guard finds ([`unregistered`], run over a home
/// after a build): add the name with its owner, never an exception in the guard.
pub const ROOTS: &[Root] = &[
    Root { name: "build", owner: Owner::Builds, what: "each workspace's build directory (blaze_manifest::home::builds)" },
    Root { name: "std", owner: Owner::Std, what: "the shared standard library stores (serve::machine)" },
    Root { name: "v2", owner: Owner::State, what: "the daemon's state: store, image, journal, profiles (blaze_manifest::home::state)" },
    Root { name: ".blaze-trash", owner: Owner::Trash, what: "what a collector renamed away and has not unlinked yet (blaze_gc::fs::trash_dir)" },
    Root { name: "sources", owner: Owner::Fetched, what: "fetched packages (blaze_manifest::home::sources)" },
    Root { name: "toolchains", owner: Owner::Fetched, what: "blaze's own pinned toolchain (blaze-cargo toolchain::own)" },
    Root { name: "library", owner: Owner::Fetched, what: "the standard library's source (blaze-cargo toolchain::library)" },
    Root { name: ".blaze-collector", owner: Owner::Fixed, what: "the collector's lock, naming its holder (blaze_gc::lease::collector_lock)" },
    Root { name: "gc.json", owner: Owner::Fixed, what: "the collector's settings (blaze_gc::settings)" },
    Root { name: "homes.json", owner: Owner::Fixed, what: "the machine's list of blaze homes" },
    Root { name: "v2.sock", owner: Owner::Fixed, what: "the daemon's socket" },
    Root { name: "v2.sock.lock", owner: Owner::Fixed, what: "the daemon's start lock" },
    Root { name: "v2.inspect.sock", owner: Owner::Fixed, what: "the daemon's inspection socket" },
    Root { name: "v2.inspect.sock.lock", owner: Owner::Fixed, what: "the inspection socket's lock" },
    Root { name: "workers", owner: Owner::Retired, what: "worker logs of the daemon before v2" },
    Root { name: "checkers", owner: Owner::Retired, what: "checker logs of the daemon before v2" },
    Root { name: "spares", owner: Owner::Retired, what: "spare workers of the daemon before v2" },
    Root { name: "sandbox", owner: Owner::Retired, what: "sandbox libraries of the daemon before v2" },
    Root { name: "store", owner: Owner::Retired, what: "the pack store of the daemon before v2" },
    Root { name: "daemon.log", owner: Owner::Retired, what: "the log of the daemon before v2" },
    Root { name: "daemon.sock", owner: Owner::Retired, what: "the socket of the daemon before v2" },
    Root { name: "daemon.sock.lock", owner: Owner::Retired, what: "the start lock of the daemon before v2" },
    Root { name: "last-build", owner: Owner::Retired, what: "the last build's events of the daemon before v2" },
];

/// The mark of a backup's name: `<root>.<label>.backup-<digits>`.
const BACKUP: &str = ".backup-";

/// Whose `name`, an entry right under a home, is: a registered name's owner, a backup of a
/// registered root, else [`Owner::Foreign`]. With what it is, for a report.
pub fn owner_of(name: &str) -> (Owner, &'static str) {
    if let Some(root) = ROOTS.iter().find(|it| it.name == name) {
        return (root.owner, root.what);
    }
    if backup_of(name).is_some() {
        return (Owner::Backup, "a root set aside by a format change");
    }
    (Owner::Foreign, "no blaze makes this name")
}

/// The registered root `name` is a backup of: `v2.store-format-3.backup-20261111` is `v2`'s.
/// The stamp after the mark is digits only and the root is one of [`ROOTS`], so no name of a
/// user's that merely holds the word is taken for one.
pub fn backup_of(name: &str) -> Option<&'static str> {
    let (before, stamp) = name.rsplit_once(BACKUP)?;
    if stamp.is_empty() || !stamp.bytes().all(|it| it.is_ascii_digit()) {
        return None;
    }
    let (root, label) = before.split_once('.')?;
    if label.is_empty() {
        return None;
    }
    ROOTS
        .iter()
        .find(|it| it.name == root && !matches!(it.owner, Owner::Fixed | Owner::Retired))
        .map(|it| it.name)
}

/// The names of `names` no blaze makes: what the guard fails on when it walks a home a build
/// just wrote (a home only blaze wrote into holds none).
pub fn unregistered<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    // alloc: a guard's answer, rarely any
    names.filter(|it| owner_of(it).0 == Owner::Foreign).collect()
}

/// One entry right under a home, as the driver found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// Bytes on disk of it and everything in it.
    pub bytes: u64,
    /// The newest stamp of it and of what is right inside it, ns since the Unix epoch.
    pub used_ns: u64,
    /// A lock file right inside it is held by a live process, or its state could not be told.
    pub held: bool,
}

/// The times this pass decides by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// Nothing goes until it was unwritten this long, at no pressure.
    pub grace_ns: u64,
    /// What the grace shrinks to at full pressure: long enough for a writer that just made a
    /// directory to take its lock in it.
    pub least_grace_ns: u64,
    /// A retired name unwritten this long goes, whatever the pressure.
    pub ttl_ns: u64,
    /// A backup this old goes, whatever the pressure.
    pub backup_ns: u64,
}

impl Default for Rules {
    /// An hour's grace, a minute at full pressure; 14 days for a retired name, as for a build
    /// dir; 7 days for a backup: a format change that went wrong is noticed within days.
    fn default() -> Rules {
        Rules { grace_ns: HOUR, least_grace_ns: MINUTE, ttl_ns: 14 * DAY, backup_ns: 7 * DAY }
    }
}

/// The grace at `pressure` (permille): `grace_ns` up to half pressure, then linearly down to
/// `least_ns` at full pressure. The one rule every grace of the collector shrinks by, so that a
/// disk cannot fill while what only a grace keeps is there: a lease, a root or a pin keeps what
/// is in use at any pressure, and the grace only covers the moment before a new directory's
/// lease is taken, which `least_ns` still covers.
pub fn grace_at(grace_ns: u64, least_ns: u64, pressure: u32) -> u64 {
    let least = least_ns.min(grace_ns);
    if pressure <= SHRINK_FROM {
        return grace_ns;
    }
    let past = u64::from(pressure.min(FULL) - SHRINK_FROM);
    let span = u64::from(FULL - SHRINK_FROM);
    // The part of the gap given up, computed wide so a grace of years does not overflow.
    let given = u128::from(grace_ns - least) * u128::from(past) / u128::from(span);
    grace_ns - u64::try_from(given).unwrap_or(grace_ns - least)
}

/// Why an entry this pass decides stays, or why one it does not decide is not its to remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Another collector's: it decides inside it, and its report says what it kept.
    Owned,
    /// Fetched, and nothing collects it yet.
    NoCollector,
    /// A small file of fixed size.
    Fixed,
    /// No blaze makes this name, so blaze does not remove it.
    Foreign,
    /// A lock in it is held.
    Held,
    /// Written within the grace at this pressure.
    Grace { left_ns: u64 },
    /// Within its lifetime, and no pressure asks for it.
    Lifetime { left_ns: u64 },
}

/// Why an entry goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Go {
    /// Past its lifetime.
    Expired { for_ns: u64 },
    /// Within its lifetime, taken for the volume's free space: `need_bytes` was still to free
    /// when it was chosen.
    Pressure { need_bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fate {
    Keep(Keep),
    Go(Go),
}

/// One entry's line of the account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub owner: Owner,
    pub what: &'static str,
    pub bytes: u64,
    pub used_ns: u64,
    pub fate: Fate,
}

/// Everything right under a home, each entry with its owner and fate, sorted by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Account {
    pub rows: Vec<Row>,
    /// The bytes of every row: all of the home.
    pub total_bytes: u64,
    /// The bytes of what no blaze made.
    pub foreign_bytes: u64,
    /// The bytes of the rows that go.
    pub going_bytes: u64,
}

/// The account of `entries` at `now_ns` and `pressure` (permille), with `to_free_bytes` still
/// to free on the home's volume (0 without pressure). In order: what a lock holds stays; what
/// is within the grace at this pressure stays; what is past its lifetime goes; then, while
/// bytes are still to free, backups and then retired names go, least recently written first.
/// The same inputs give the same account.
pub fn account(
    entries: &[Entry],
    now_ns: u64,
    pressure: u32,
    to_free_bytes: u64,
    rules: &Rules,
) -> Account {
    let grace = grace_at(rules.grace_ns, rules.least_grace_ns, pressure);
    // alloc: a pass, one row an entry
    let mut rows: Vec<Row> = Vec::with_capacity(entries.len());
    for entry in entries {
        let (owner, what) = owner_of(&entry.name);
        let age = now_ns.saturating_sub(entry.used_ns);
        let lifetime = match owner {
            Owner::Backup => rules.backup_ns,
            _ => rules.ttl_ns,
        };
        let fate = match owner {
            Owner::Builds | Owner::Std | Owner::State | Owner::Trash => Fate::Keep(Keep::Owned),
            Owner::Fetched => Fate::Keep(Keep::NoCollector),
            Owner::Fixed => Fate::Keep(Keep::Fixed),
            Owner::Foreign => Fate::Keep(Keep::Foreign),
            Owner::Backup | Owner::Retired if entry.held => Fate::Keep(Keep::Held),
            Owner::Backup | Owner::Retired if age < grace => {
                Fate::Keep(Keep::Grace { left_ns: grace - age })
            }
            Owner::Backup | Owner::Retired if age >= lifetime => {
                Fate::Go(Go::Expired { for_ns: age })
            }
            Owner::Backup | Owner::Retired => {
                Fate::Keep(Keep::Lifetime { left_ns: lifetime - age })
            }
        };
        rows.push(Row {
            name: entry.name.clone(), // clone: the row's own name
            owner,
            what,
            bytes: entry.bytes,
            used_ns: entry.used_ns,
            fate,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    let going = |rows: &[Row]| {
        rows.iter().filter(|it| matches!(it.fate, Fate::Go(_))).map(|it| it.bytes).sum::<u64>()
    };
    if pressure > 0 {
        let mut need = to_free_bytes.saturating_sub(going(&rows));
        // alloc: a pass under pressure, what only its lifetime keeps
        let mut within: Vec<usize> = (0..rows.len())
            .filter(|&it| matches!(rows[it].fate, Fate::Keep(Keep::Lifetime { .. })))
            .collect();
        // A backup before a retired name (it is a copy of what the new format already holds),
        // then least recently written first, then by name: no order comes from a map.
        within.sort_by(|&a, &b| {
            (rows[a].owner, rows[a].used_ns, &rows[a].name)
                .cmp(&(rows[b].owner, rows[b].used_ns, &rows[b].name))
        });
        for at in within {
            if need == 0 {
                break;
            }
            rows[at].fate = Fate::Go(Go::Pressure { need_bytes: need });
            need = need.saturating_sub(rows[at].bytes);
        }
    }
    let total_bytes = rows.iter().map(|it| it.bytes).sum();
    let foreign_bytes =
        rows.iter().filter(|it| it.owner == Owner::Foreign).map(|it| it.bytes).sum();
    let going_bytes = going(&rows);
    Account { rows, total_bytes, foreign_bytes, going_bytes }
}

impl Account {
    /// The names that go, in the order they are renamed: by name.
    pub fn going(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|it| matches!(it.fate, Fate::Go(_)))
    }

    /// The `most` largest rows that stay, largest first (then by name), each with why: what
    /// holds the home's bytes when not enough could be freed.
    pub fn holders(&self, most: usize) -> Vec<&Row> {
        // alloc: a report, the rows that stay
        let mut kept: Vec<&Row> =
            self.rows.iter().filter(|it| matches!(it.fate, Fate::Keep(_))).collect();
        kept.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        kept.truncate(most);
        kept
    }

    /// The account as a report's lines: the totals, what goes, and the `most` largest holders
    /// with the reason each is kept. `home` is the home's path, for the names.
    pub fn lines(&self, home: &str, most: usize, dry_run: bool) -> Vec<String> {
        // alloc: a report's lines
        let mut lines = Vec::new();
        lines.push(format!(
            "{home} holds {} bytes in {} entries; {} bytes are in what no blaze made",
            self.total_bytes,
            self.rows.len(),
            self.foreign_bytes
        ));
        for row in self.going() {
            let Fate::Go(why) = &row.fate else { continue };
            lines.push(format!(
                "{} {home}/{} ({} bytes, {}): {why}",
                if dry_run { "would remove" } else { "removes" },
                row.name,
                row.bytes,
                row.what
            ));
        }
        for row in self.holders(most) {
            let Fate::Keep(why) = &row.fate else { continue };
            lines.push(format!(
                "holds {} bytes: {home}/{} ({}): {why}",
                row.bytes, row.name, row.what
            ));
        }
        lines
    }
}

impl fmt::Display for Keep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Keep::Owned => f.write_str("its own collector decides inside it"),
            Keep::NoCollector => f.write_str(
                "fetched, and nothing collects it yet: remove it by hand and blaze fetches it again",
            ),
            Keep::Fixed => f.write_str("a file of fixed size"),
            Keep::Foreign => f.write_str(
                "no blaze makes this name, so blaze does not remove it: it is yours to remove or move out of blaze's home",
            ),
            Keep::Held => f.write_str("a lock in it is held by a running process"),
            Keep::Grace { left_ns } => {
                write!(f, "written within the grace at this pressure, {} s left", left_ns / SECOND)
            }
            Keep::Lifetime { left_ns } => {
                write!(f, "within its lifetime, {} s left, and no pressure asks for it", left_ns / SECOND)
            }
        }
    }
}

impl fmt::Display for Go {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Go::Expired { for_ns } => {
                write!(f, "unwritten for {} s, past its lifetime", for_ns / SECOND)
            }
            Go::Pressure { need_bytes } => {
                write!(f, "taken for free space, {need_bytes} bytes were still to free")
            }
        }
    }
}

/// The name `name` gets in its home's trash when step `step` of the pass begun at `began_ns`
/// renames it: `<20 digits>-<6 letters or digits>-<name>`, the form the collector's trash phase
/// lists and unlinks (`fs::is_trash_name`). The tag starts with `h` (the home's pass), so it is
/// never one a run's own step makes for the same instant.
pub fn trash_name(began_ns: u64, step: u32, name: &str) -> String {
    format!("{began_ns:020}-h{:05}-{name}", step % 100_000)
}

/// Whether `name` can be an entry of trash: letters, digits, `-`, `_` and `.` only. A name
/// that cannot is kept and said, never renamed under another name.
pub fn fits_trash(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|it| it.is_ascii_alphanumeric() || matches!(it, b'-' | b'_' | b'.'))
}
