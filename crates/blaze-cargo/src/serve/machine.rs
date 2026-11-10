//! The machine's memory ledger, its driver (docs/memory-budget.md, section 7): the file
//! `/tmp/blaze-<uid>/machine`, shared by every blaze daemon of the user on the host whatever
//! its `BLAZE_HOME` (two homes are one machine), read and written whole under its `flock`
//! around one op of the core, [`blaze_graph::ledger`]. The core does the arithmetic; this opens
//! the file, takes the lock, asks the system which pids run and since when, and writes.
//!
//! A daemon joins as its engine starts ([`Machine::join`], which also makes its machine budget
//! and its ceiling), takes a turn as its plane is polled ([`Machine::due`], [`Machine::turn`]),
//! and leaves as it drops. A ledger that cannot be opened, locked, read or written stops the
//! daemon with an error naming the file, never a budget of its own in its place (no-fallback).
//!
//! The file is never synced: it lives in `/tmp`, says only what runs now, and a write is one
//! `pwrite` of the whole file under the lock, which no reader sees half done. A write torn by
//! the machine losing power reads as corrupt, naming the file, and is removed by hand.

use std::{
    fs::{File, OpenOptions},
    os::unix::fs::FileExt as _,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, anyhow, bail};
use blaze_graph::{
    Budget,
    admission::Predict,
    ledger::{Admits, Alive, Allowance, LEN, Label, Ledger, LedgerError, MOST, Process, Record, Why, fnv1a},
};
use stdx::file_lock::Locked;

use super::plane::{Plane, Work};

/// The variable that sets the machine budget, in GiB, as [`super::MEMORY_VAR`] sets one daemon's
/// ceiling.
pub(crate) const MACHINE_VAR: &str = "BLAZE_MACHINE_MEMORY_GB";

/// How long a charge that moves by little goes unwritten at most.
const EVERY: Duration = Duration::from_millis(250);

/// The floor between two turns for a task the allowance itself holds back: a turn changes
/// nothing for it unless another daemon let go of memory since. A task the room alone holds
/// back, or a lease that runs low, waits for no floor ([`Machine::due`]).
const SOON: Duration = Duration::from_millis(25);

/// How long a turn waits for another daemon's hold of the lock, which lasts microseconds.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// What the machine budget leaves for the kernel and the page cache, past the quarter its 3/4
/// already does, as the daemon's own budget does ([`blaze_graph::arena::machine_budget`]).
const MARGIN: u64 = 1 << 30;

/// This daemon's place in the ledger.
pub struct Machine {
    path: PathBuf,
    /// The ledger, open for the daemon's life: each op locks a handle of it.
    file: File,
    me: Process,
    /// The machine budget this daemon brought.
    budget: u64,
    /// When the last turn was, by the daemon's clock, and what the plane held or had granted
    /// then: the charge it wrote less its lease.
    last: Option<(Duration, u64)>,
    /// The allowance the last turn gave.
    allowance: Option<u64>,
    /// A turn failed and stopped the daemon: no other is taken.
    broken: bool,
    /// What the start says of the machine budget.
    said: String,
    /// Who else holds the machine's memory, as the last turn read the ledger, for a build's
    /// paused line ([`Machine::says`]); empty while no other daemon holds any.
    says: String,
}

/// What a daemon joined with: its place, and the ceiling its plane admits under.
pub(crate) struct Joined {
    pub(crate) machine: Machine,
    /// The most this daemon may grant: what it was told (`--memory-gb`,
    /// [`super::MEMORY_VAR`]) or the machine's limit for one process, never above the machine
    /// budget.
    pub(crate) ceiling: Budget,
}

impl Machine {
    /// Joins the ledger as this process, under one hold of its lock: the dead pruned, the
    /// machine budget made from what is not blaze's (`told`, [`MACHINE_VAR`], in its place),
    /// the ceiling (`told_ceiling`, as [`blaze_graph::arena::machine_budget`] takes it) cut to
    /// it, and this daemon's record written. `version` and `build` say which blaze it is, `home`
    /// its `BLAZE_HOME`, for whoever reads the ledger.
    pub(crate) fn join(
        told: Option<(u64, String)>,
        told_ceiling: Option<(u64, String)>,
        version: &str,
        build: &blaze_api::Build,
        home: &Path,
    ) -> anyhow::Result<Joined> {
        // alloc: the ledger's path, once per daemon start
        let dir = PathBuf::from(format!("/tmp/blaze-{}", user()));
        crate::socket::private_dir(&dir).with_context(|| {
            format!("making {}, where the machine's memory ledger is, the user's alone", dir.display())
        })?;
        // Named by its layout: a daemon of a blaze that reads another layout keeps a ledger of
        // its own beside this one, and neither refuses to start for the other's file (a blaze
        // upgraded while an older daemon ran, or two pinned versions on one machine, stopped
        // every build with "is of layout 1, and this blaze reads layout 2"). Daemons of two
        // layouts then share no budget; each is still held by its own ceiling.
        let path = dir.join(format!("machine.{}", blaze_graph::ledger::FORMAT)); // alloc: the path, once a daemon start
        let file = crate::socket::private_file(
            OpenOptions::new().read(true).write(true).create(true).truncate(false),
            &path,
        )
        .with_context(|| format!("opening the machine's memory ledger {}", path.display()))?;
        let pid = std::process::id();
        let start = crate::raw::process::started(pid)
            .with_context(|| format!("when this daemon, pid {pid}, started"))?
            .with_context(|| format!("the system has no start time of this daemon, pid {pid}"))?;
        let me = Process { pid, start };
        let mut bytes = [0; LEN];
        let lock = lock(&file, &path)?;
        let mut ledger = read(&file, &path, &mut bytes)?;
        ledger.prune(&Seen::of(&ledger, &path)?);
        let blaze: u64 = ledger.records().map(|it| it.held).sum();
        let budget = match told {
            // alloc: the budget's name, once per daemon start
            Some((bytes, from)) => Budget { bytes, from, kept_back: 0 },
            None => blaze_graph::arena::machine_wide(MARGIN, blaze)
                .context("the machine budget every blaze daemon of this user shares")?,
        };
        let own = blaze_graph::arena::machine_budget(MARGIN, told_ceiling)
            .context("the memory the daemon's tasks may hold")?;
        let ceiling = if own.bytes > budget.bytes {
            Budget {
                bytes: budget.bytes,
                // alloc: the budget's name, once per daemon start
                from: format!(
                    "the machine budget every blaze daemon of this user shares, {}, under this \
                     daemon's own limit of {:.2} GiB ({})",
                    budget.from,
                    gib(own.bytes),
                    own.from
                ),
                kept_back: budget.kept_back,
            }
        } else {
            own
        };
        let record = Record {
            process: me,
            budget: budget.bytes,
            ceiling: ceiling.bytes,
            held: blaze_graph::arena::held(),
            granted: 0,
            // None until its first turn: it grants nothing before one.
            lease: 0,
            at_ns: wall_ns(),
            version: Label::head(
                version.as_bytes(),
                u64::from_le_bytes(build.id.0[..8].try_into().expect("8 bytes of 32")),
            ),
            home: Label::tail(
                home.as_os_str().as_encoded_bytes(),
                fnv1a(home.as_os_str().as_encoded_bytes()),
            ),
        };
        ledger.join(record)?;
        let others = ledger.records().count() - 1;
        let shared = ledger.machine().expect("a ledger with this daemon's record");
        write(&file, &path, &ledger, &mut bytes)?;
        drop(lock);
        // alloc: the start's line, once per daemon start
        let said = format!(
            "machine budget {:.2} GiB ({}), shared with {others} other blaze daemons of this user \
             through {}{}",
            gib(budget.bytes),
            budget.from,
            path.display(),
            if shared < budget.bytes {
                // alloc: as `said`
                format!(", which another holds to {:.2} GiB", gib(shared))
            } else {
                String::new()
            }
        );
        let machine = Machine {
            path,
            file,
            me,
            budget: budget.bytes,
            last: None,
            allowance: None,
            broken: false,
            said,
            says: String::new(),
        };
        Ok(Joined { machine, ceiling })
    }

    /// What the start says of the machine budget and the ledger.
    pub(crate) fn said(&self) -> &str {
        &self.said
    }

    /// Whether a turn is due at `at`, by the daemon's clock: there was none yet, [`EVERY`]
    /// passed, the plane's charge moved by more than 1/64 of the machine budget since the last,
    /// a turn now would change what the plane may grant ([`Plane::turn_wanted`]: the room
    /// alone holds a task back, the lease is in its last quarter, or a lease is held with no
    /// work), or the plane held a task back for memory and [`SOON`] passed. The plane wants no
    /// turn as one returns and until its next step, so the turns it asks for are at most one a
    /// step of it; [`SOON`] bounds the rest.
    pub(crate) fn due<W: Work, P: Predict>(&self, at: Duration, plane: &Plane<W, P>) -> bool {
        if self.broken {
            return false;
        }
        let Some((then, charge)) = self.last else { return true };
        let since = at.saturating_sub(then);
        let now = plane.held().max(plane.granted());
        since >= EVERY
            || now.abs_diff(charge) > self.budget / 64
            || plane.turn_wanted()
            || (plane.held_any_back() && since >= SOON)
    }

    /// One turn at `at`, by the daemon's clock, under the lock: the dead pruned, `plane` told
    /// its allowance (what fits starts), and the charge it is left with written, its lease in
    /// it. The allowance
    /// when it differs from the last turn's, for the subsystems' budgets
    /// ([`blaze_graph::admission::set_budgets`]). An error stops the daemon; no turn is taken
    /// after one.
    pub(crate) fn turn<W: Work, P: Predict>(
        &mut self,
        at: Duration,
        plane: &mut Plane<W, P>,
    ) -> anyhow::Result<Option<u64>> {
        let turned = self.turn_locked(at, plane);
        self.broken |= turned.is_err();
        let allowance = turned?;
        let changed = self.allowance != Some(allowance.bytes);
        self.allowance = Some(allowance.bytes);
        Ok(changed.then_some(allowance.bytes))
    }

    fn turn_locked<W: Work, P: Predict>(
        &mut self,
        at: Duration,
        plane: &mut Plane<W, P>,
    ) -> anyhow::Result<Allowance> {
        let mut bytes = [0; LEN];
        let lock = lock(&self.file, &self.path)?;
        let mut ledger = read(&self.file, &self.path, &mut bytes)?;
        let seen = Seen::of(&ledger, &self.path)?;
        let turn = ledger.turn(self.me, wall_ns(), &seen, &mut Turning { plane, at })?;
        says_of(&mut self.says, self.me, &ledger, &turn.allowance);
        write(&self.file, &self.path, &ledger, &mut bytes)?;
        drop(lock);
        if turn.pruned > 0 {
            tracing::info!(target: "blaze", pruned = turn.pruned, ledger = %self.path.display(), "the machine's memory ledger dropped the records of daemons gone");
        }
        self.last = Some((at, turn.charge - turn.lease));
        Ok(turn.allowance)
    }

    /// Who else holds the machine's memory as the last turn read it, for a build's paused line
    /// (`machine 96.0 GiB, 40.1 held by 1 other daemon: pid 4242 of /Users/a/.blaze 40.1`);
    /// `None` while no other daemon holds any, when this daemon's own ceiling is all that
    /// holds it back.
    pub(crate) fn says(&self) -> Option<&str> {
        (!self.says.is_empty()).then_some(self.says.as_str())
    }

    /// Takes this daemon's record out, under the lock: a clean exit.
    fn leave(&mut self) -> anyhow::Result<()> {
        let mut bytes = [0; LEN];
        let lock = lock(&self.file, &self.path)?;
        let mut ledger = read(&self.file, &self.path, &mut bytes)?;
        ledger.leave(self.me)?;
        write(&self.file, &self.path, &ledger, &mut bytes)?;
        drop(lock);
        Ok(())
    }
}

/// The daemon's exit, clean or by a panic that unwinds: its record goes. One killed leaves it,
/// for the next turn of another daemon to prune.
impl Drop for Machine {
    fn drop(&mut self) {
        if let Err(why) = self.leave() {
            tracing::warn!(target: "blaze", ledger = %self.path.display(), "leaving the machine's memory ledger: {why:#}");
            eprintln!("blaze serve: leaving the machine's memory ledger: {why:#}");
        }
    }
}

/// The plane taking a turn ([`Ledger::turn`]).
struct Turning<'a, W: Work, P: Predict> {
    plane: &'a mut Plane<W, P>,
    at: Duration,
}

impl<W: Work, P: Predict> Admits for Turning<'_, W, P> {
    fn allow(&mut self, allowance: &Allowance) {
        self.plane.allow(self.at, allowance.bytes);
    }

    fn held(&self) -> u64 {
        self.plane.held()
    }

    fn granted(&self) -> u64 {
        self.plane.granted()
    }

    fn lease(&self) -> u64 {
        self.plane.lease()
    }
}

/// When each pid of the ledger's records started, as the system said just before the op.
struct Seen(Vec<(u32, Option<u64>)>);

impl Seen {
    fn of(ledger: &Ledger<'_>, path: &Path) -> anyhow::Result<Seen> {
        let mut seen = Vec::with_capacity(MOST); // alloc: at most 64 pids, once a turn
        for record in ledger.records() {
            let pid = record.process.pid;
            if seen.iter().any(|(it, _)| *it == pid) {
                continue;
            }
            let started = crate::raw::process::started(pid).with_context(|| {
                format!(
                    "when pid {pid}, a daemon of the machine's memory ledger {}, started",
                    path.display()
                )
            })?;
            seen.push((pid, started));
        }
        Ok(Seen(seen))
    }
}

impl Alive for Seen {
    fn started(&self, pid: u32) -> Option<u64> {
        self.0.iter().find(|(it, _)| *it == pid).and_then(|(_, started)| *started)
    }
}

/// The ledger's lock, waiting at most [`LOCK_WAIT`] for another daemon to let go of it.
#[allow(
    clippy::disallowed_methods,
    reason = "a bounded wait on another process's lock, which has no wake to park on"
)]
fn lock(file: &File, path: &Path) -> anyhow::Result<Locked> {
    let _span = tracing::info_span!(target: "blaze", "waits on the machine's memory ledger", ledger = %path.display())
        .entered();
    let mut file = file
        .try_clone()
        .with_context(|| format!("a handle of the machine's memory ledger {}", path.display()))?;
    let until = Instant::now() + LOCK_WAIT;
    let mut spin = blaze_graph::spin::SpinWait::new();
    loop {
        match Locked::try_lock(file) {
            Ok(Ok(locked)) => return Ok(locked),
            Ok(Err(back)) => file = back,
            Err(error) => {
                return Err(anyhow!(
                    "locking the machine's memory ledger {}: {error}",
                    path.display()
                ));
            }
        }
        if Instant::now() >= until {
            bail!(
                "the machine's memory ledger {} stayed locked for {LOCK_WAIT:?}, where a turn \
                 holds it for microseconds: a process of this user holds its lock and does not \
                 let go (`lsof {}` names it)",
                path.display(),
                path.display()
            );
        }
        if !spin.spin() {
            spin = blaze_graph::spin::SpinWait::new();
        }
    }
}

/// The ledger the file holds, read whole into `bytes`.
fn read<'p>(file: &File, path: &'p Path, bytes: &mut [u8; LEN]) -> anyhow::Result<Ledger<'p>> {
    let len = file
        .metadata()
        .with_context(|| format!("the size of the machine's memory ledger {}", path.display()))?
        .len();
    let Some(len) = usize::try_from(len).ok().filter(|&it| it <= LEN) else {
        // alloc: the path an error names
        let why = Why::Length { len: usize::try_from(len).unwrap_or(usize::MAX) };
        return Err(LedgerError { path: path.to_path_buf(), why }.into());
    };
    file.read_exact_at(&mut bytes[..len], 0)
        .with_context(|| format!("reading the machine's memory ledger {}", path.display()))?;
    Ok(Ledger::decode(path, &bytes[..len])?)
}

/// `ledger` written whole over the file, through `bytes`.
fn write(file: &File, path: &Path, ledger: &Ledger<'_>, bytes: &mut [u8; LEN]) -> anyhow::Result<()> {
    ledger.encode(bytes);
    file.write_all_at(&bytes[..], 0)
        .with_context(|| format!("writing the machine's memory ledger {}", path.display()))
}

/// The writer's wall clock, for a person reading the file: no decision reads it.
#[allow(clippy::disallowed_methods, reason = "a time a person reads in the ledger, no decision's")]
fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX))
}

/// [`Machine::says`], made from `ledger` as a turn of `me` read it.
fn says_of(says: &mut String, me: Process, ledger: &Ledger<'_>, allowance: &Allowance) {
    use std::fmt::Write as _;
    says.clear();
    if allowance.others == 0 {
        return;
    }
    let daemons = allowance.daemons;
    _ = write!(
        says,
        "machine {:.1} GiB, {:.1} held by {daemons} other daemon{}:",
        gib(allowance.machine),
        gib(allowance.others),
        if daemons == 1 { "" } else { "s" }
    );
    // The two that hold the most, by pid among equals, so the line is the same for the
    // same ledger.
    let mut top: [Option<&Record>; 2] = [None; 2];
    for it in ledger.records().filter(|it| it.process != me) {
        let key = |r: &Record| (std::cmp::Reverse(r.charge()), r.process.pid);
        if top[0].is_none_or(|t| key(it) < key(t)) {
            top[1] = top[0];
            top[0] = Some(it);
        } else if top[1].is_none_or(|t| key(it) < key(t)) {
            top[1] = Some(it);
        }
    }
    for (at, it) in top.iter().flatten().enumerate() {
        let home = String::from_utf8_lossy(it.home.kept());
        _ = write!(
            says,
            "{} pid {} of {}{home} {:.1}",
            if at > 0 { "," } else { "" },
            it.process.pid,
            if it.home.cut() { "..." } else { "" },
            gib(it.charge())
        );
    }
    if daemons > 2 {
        _ = write!(says, ", and {} more", daemons - 2);
    }
}

fn user() -> u32 {
    // SAFETY: `geteuid` takes nothing, touches no memory of the caller's and can't fail.
    unsafe { libc::geteuid() }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}
