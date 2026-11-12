//! The driver of the store's collector (docs/gc.md, "The store's budget and collector"): the
//! daemon's loop gives [`blaze_cas::collect::Collector`] the time and the facts, takes one
//! step of it a turn, and runs the batches it hands out on the worker pool
//! ([`Collects::turn`]). The rule of when and what is the core's; nothing here decides.
//!
//! What it costs the loop: a step copies at most `Pace::copy_most` bytes into the store's
//! segment, seals a pack or asks for a commit, each a span (`store gc step`, with what it
//! did); a step past [`STEP_BOUND`] is counted and said. A pack is read, checked and judged
//! on a worker (`store gc read`). The bytes of the daemon's state outside the store (the
//! image, the journal, the logs) are measured on a worker too, once a minute.
//!
//! What a build's end says ([`Collects::storage`]) and when it says it in words
//! ([`Warner`]) are here too: the data on every build, a line only when the home is near its
//! budget or its volume is low.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_api::Storage;
use blaze_cas::{
    Store,
    collect::{CollectBatch, Collector, Did, Facts, FULL, Pace, Standing, Step},
};
use blaze_gc::plan::size_text;
use blaze_graph::pool::Class;
use crossbeam_channel::{Receiver, Sender};

use super::settle::Compress;

/// The bound of one step on the loop (docs/gc.md): past it a step is counted
/// ([`Collects::over_bound`]) and said in the log with what it did.
pub(super) const STEP_BOUND: Duration = Duration::from_millis(1);
/// How often the bytes of the daemon's state outside the store are measured.
const OTHER_EVERY: Duration = Duration::from_secs(60);
/// A request that needs the store's pins waits this long for the scan that reads them
/// ([`Collects::read_pins`]) before it is refused, naming the wait: the scan is a walk of
/// the store's rows on a worker, begun as it is asked, and waits only for the store to be at
/// rest (no commit or index swap in flight).
pub(super) const PINS_BOUND: Duration = Duration::from_secs(30);
/// A lap asked for (`blaze gc`) that has not ended by then is answered as it stands: builds
/// that follow one another leave it no turn to seal a pack in.
const ASKED_BOUND: Duration = Duration::from_secs(120);

/// The pin of the standard library's records the daemon's store holds, and of bstd's
/// ([`keep`]).
pub(crate) const TOOLCHAIN: &str = "toolchain";
pub(crate) const BSTD: &str = "bstd";

/// Every store key the front-end records of a crate are kept under, from the key its image
/// is filed under: `hir_def::deps::front_keys` (the image and hir-def's record kinds), then
/// each record kind of a crate above hir-def keyed over the same key (hir-ty's frozen impl
/// tables). The one function the `toolchain` and `bstd` pins and a pack's export take a
/// crate's front end from: a lane that adds a record kind above hir-def adds its key here.
pub(crate) fn front_keys(filed: blaze_ids::ContentId) -> impl Iterator<Item = blaze_cas::Key> {
    hir_def::deps::front_keys(filed)
        .chain([hir_ty::method_resolution::frozen::key(filed)])
        .map(blaze_cas::Key)
}

/// The store keys of the read-filing records of the crate whose own key is `key`
/// (`hir_def::deps::filing_keys`): what a crate with reads is found through.
pub(crate) fn filing_keys(key: blaze_ids::ContentId) -> impl Iterator<Item = blaze_cas::Key> {
    hir_def::deps::filing_keys(key).map(blaze_cas::Key)
}

/// The keys builds found that a pin keeps, until the daemon's loop takes them
/// ([`Collects::pin`]). The process's one queue: only `blaze serve`'s daemon has a collector
/// that drains it, and it is the process's one daemon with a store on disk.
static KEPT: crossbeam_queue::SegQueue<Kept> = crossbeam_queue::SegQueue::new();

/// Keys queued for a pin: its name, why it is kept, and the keys.
struct Kept {
    name: std::sync::Arc<str>,
    why: std::sync::Arc<str>,
    keys: Vec<blaze_cas::Key>,
}

/// `keys` are pin `name`'s from the next build's end on: called by a build's task as it
/// finds a standard library unit's record, or bstd's, in the store. `name` is one of
/// [`TOOLCHAIN`] and [`BSTD`]; any other name is [`keep_as`]'s.
pub(crate) fn keep(name: &'static str, keys: impl Iterator<Item = blaze_cas::Key>) {
    let why = match name {
        TOOLCHAIN => "the standard library's records, which every build of this toolchain reads",
        BSTD => "bstd's records, which blaze's own crates are built on",
        _ => "kept by name",
    };
    keep_as(name, why, keys);
}

/// `keys` are added to the pin `name` (any name: a build kept as a name, a package's
/// closure) at the next build's end, `why` said with it. Adding keys the pin already names
/// writes nothing.
pub(crate) fn keep_as(name: &str, why: &str, keys: impl Iterator<Item = blaze_cas::Key>) {
    // alloc: once per call, what is queued for the daemon's loop
    let keys: Vec<blaze_cas::Key> = keys.collect();
    if !keys.is_empty() {
        KEPT.push(Kept { name: name.into(), why: why.into(), keys });
    }
}

/// From this share of the budget on, a build's end says so, in percent.
pub const WARN_PERCENT: u64 = 85;
/// A home near its budget says so at most this often, and one over it this often.
pub const WARN_EVERY: Duration = Duration::from_secs(6 * 3600);
pub const WARN_EVERY_OVER: Duration = Duration::from_secs(3600);

/// When a build's end says in words that the home is near its budget: a core on the
/// daemon's clock.
///
/// The invariant: a line is given only when what is held is at [`WARN_PERCENT`] of the
/// budget or more, or the volume's free space is under its comfortable level; never sooner
/// than [`WARN_EVERY`] after the last one ([`WARN_EVERY_OVER`] when at or over the budget);
/// and every line names the command that raises the budget and the one that frees space.
#[derive(Debug, Default)]
pub struct Warner {
    last: Option<Duration>,
}

impl Warner {
    /// The line for a build ending at `now`, if this one says something. `comfortable`: the
    /// volume's comfortable level of free space.
    pub fn line(&mut self, now: Duration, storage: &Storage, comfortable: u64) -> Option<String> {
        if storage.budget_bytes == 0 {
            return None;
        }
        let percent = storage.held_bytes.saturating_mul(100) / storage.budget_bytes;
        let over = storage.held_bytes >= storage.budget_bytes;
        let low = storage.volume_free_bytes < comfortable;
        if percent < WARN_PERCENT && !low {
            return None;
        }
        let every = if over { WARN_EVERY_OVER } else { WARN_EVERY };
        if self.last.is_some_and(|last| now.saturating_sub(last) < every) {
            return None;
        }
        self.last = Some(now);
        let held = format!(
            "blaze holds {} of its budget of {} ({percent}%)",
            size_text(storage.held_bytes),
            size_text(storage.budget_bytes),
        );
        let what = match (over, low) {
            (true, _) if storage.over_budget => format!(
                "{held}: what is left is in use or kept, so nothing more was removed"
            ),
            (true, _) => format!("{held}: the collector is at work"),
            (false, true) => format!(
                "{held}, and the volume has {} free, under its comfortable {}",
                size_text(storage.volume_free_bytes),
                size_text(comfortable),
            ),
            (false, false) => held,
        };
        // alloc: a line said at most once an hour
        Some(format!(
            "{what}; `blaze gc --budget <size>` raises the budget, `blaze clean --unkept` frees what nothing keeps (`blaze gc --dry-run` shows what is kept and why)"
        ))
    }
}

/// What a worker hands back.
enum Back {
    /// A batch worked, or `None` for one a panic lost.
    Batch(Option<CollectBatch>),
    /// The bytes of the daemon's state outside the store, or why they were not measured.
    Other(Result<u64, String>),
    /// A scan that reads the store's pins, asked for by a request, or `None` for one a
    /// panic lost.
    Pins(Option<blaze_cas::collect::PeriodScan>),
}

/// What [`Collects::read_pins`] says of the store's pins.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PinsRead {
    /// They are read: [`Store::pins`] is whole and [`Store::pin_add`] writes.
    Ready,
    /// The scan that reads them is under way: ask again next turn.
    Reading,
    /// It did not come back within [`PINS_BOUND`]: why, for the request's error.
    Failed(String),
}

/// The home's bytes against its budget, as last worked out.
#[derive(Clone, Copy, Debug, Default)]
struct Budget {
    held: u64,
    budget: u64,
    free: u64,
    comfortable: u64,
}

/// A `blaze gc` waiting for the store's lap.
struct Asked {
    since: Duration,
    /// The laps ended when it was asked.
    laps: u64,
    dry_run: bool,
}

/// The store's collector in the daemon: the core, the batch out with a worker, and what a
/// build's end says.
pub struct Collects {
    collector: Collector,
    run: Compress,
    send: Sender<Back>,
    recv: Receiver<Back>,
    /// The daemon's state directory, whose bytes outside `store/` count against the budget.
    state: PathBuf,
    other: u64,
    other_at: Option<Duration>,
    other_out: bool,
    budget: Option<Budget>,
    /// Builds begun, as the core counts them.
    builds: u64,
    was_idle: bool,
    /// The store's bytes when the last build's end was said.
    said_bytes: Option<u64>,
    warner: Warner,
    asked: Option<Asked>,
    /// Steps on the loop, and those past [`STEP_BOUND`].
    steps: u64,
    over_bound: u64,
    /// A scan of the pins a request asked for is with a worker.
    pins_out: bool,
    /// A request waits for the store's pins: since when, and the wait's span
    /// (every-wait-says-what), open until they are read or the bound passes.
    pins_asked: Option<(Duration, tracing::Span)>,
}

impl Collects {
    /// The collector of the store under `state` (`<root>/v2`). `run`: where its batches are
    /// read, as the cold tier's are compressed.
    pub fn new(state: &Path, run: Compress, pace: Pace) -> Collects {
        let (send, recv) = crossbeam_channel::unbounded();
        Collects {
            collector: Collector::new(pace),
            run,
            send,
            recv,
            state: state.to_path_buf(),
            other: 0,
            other_at: None,
            other_out: false,
            budget: None,
            builds: 0,
            was_idle: true,
            said_bytes: None,
            warner: Warner::default(),
            asked: None,
            steps: 0,
            over_bound: 0,
            pins_out: false,
            pins_asked: None,
        }
    }

    /// A build ended (its job is done and answered): what it found are flags a batch scanned
    /// before did not see. The standard library's records and bstd's it found are pinned: each
    /// pin's keys are what it had and what builds found since, written again only when a
    /// key is new.
    pub(super) fn build_ended(&mut self, store: &mut Store) {
        self.builds += 1;
        // Each name's keys, in the order of the names (rule deterministic-output).
        // alloc: once a build's end, what it found to pin
        let mut found: std::collections::BTreeMap<std::sync::Arc<str>, (std::sync::Arc<str>, Vec<blaze_cas::Key>)> =
            std::collections::BTreeMap::new();
        while let Some(Kept { name, why, mut keys }) = KEPT.pop() {
            found.entry(name).or_insert_with(|| (why, Vec::new())).1.append(&mut keys);
        }
        for (name, (why, keys)) in found {
            match store.pin_add(&name, &why, blaze_gc::record::now_ns(), &keys) {
                Ok(blaze_cas::collect::PinAdded::Written(_) | blaze_cas::collect::PinAdded::Unchanged) => {}
                // No scan has read the store's pins since it opened: the keys wait for the
                // next build's end.
                Ok(blaze_cas::collect::PinAdded::NotRead) => KEPT.push(Kept { name, why, keys }),
                Err(mismatch) => {
                    tracing::error!(target: "blaze", "the pin `{name}` was not written: {mismatch:?}");
                }
            }
        }
    }

    /// Steps taken on the loop, and how many took longer than [`STEP_BOUND`].
    pub fn over_bound(&self) -> (u64, u64) {
        (self.steps, self.over_bound)
    }

    /// Whether the loop should turn again soon for it: a batch is out (it comes back over a
    /// channel no driver wakes for), or its core has a step due.
    pub(super) fn next_deadline(&self, now: Duration) -> Option<Duration> {
        if self.collector.out() || self.other_out || self.pins_out || self.pins_asked.is_some() {
            return Some(now + Duration::from_millis(1));
        }
        self.collector.next_deadline()
    }

    /// The store's pins, for a request that needs them now (an import, an export, a status of
    /// the store), whether or not any build has ended: read already, or being read by a scan
    /// this call begins on a worker when none is out. The daemon reads its pins otherwise
    /// only at a period's end, which the first comes after the first build; a daemon that is
    /// never asked scans nothing as it starts. Call it each turn until it answers
    /// [`PinsRead::Ready`]; the wait has a span (`store pins read, asked for`) and a bound
    /// ([`PINS_BOUND`]), past which it answers [`PinsRead::Failed`] naming what it waited on.
    pub(crate) fn read_pins(&mut self, store: &mut Store, now: Duration) -> PinsRead {
        if store.pins_read() {
            self.pins_asked = None;
            return PinsRead::Ready;
        }
        let since = self
            .pins_asked
            .get_or_insert_with(|| {
                // No parent: it ends on a later turn, under whatever request is current then.
                let span = tracing::info_span!(target: "blaze", parent: None, "store pins read, asked for");
                (now, span)
            })
            .0;
        let waited = now.saturating_sub(since);
        if waited > PINS_BOUND {
            self.pins_asked = None;
            // alloc: an error, once
            return PinsRead::Failed(format!(
                "the store's pins were not read in {} s: the scan that reads them {}",
                waited.as_secs(),
                match self.pins_out {
                    true => "is still with its worker",
                    false => "could not be taken (the store has a commit, an index swap or another scan in flight)",
                }
            ));
        }
        if !self.pins_out
            && let Some(mut scan) = store.pins_take()
        {
            self.pins_out = true;
            // clone: a channel's sender, once a scan
            let send = self.send.clone();
            let work = move || {
                let span = tracing::info_span!(
                    target: "blaze",
                    "store pins scan",
                    records = tracing::field::Empty,
                )
                .entered();
                scan.run();
                span.record("records", scan.records());
                scan
            };
            match self.run {
                Compress::Inline => _ = send.send(Back::Pins(Some(work()))),
                Compress::Pool => blaze_graph::exec::shared().submit_as(
                    None,
                    Class::Normal,
                    work,
                    move |done| _ = send.send(Back::Pins(done.ok())),
                ),
            }
        }
        PinsRead::Reading
    }

    /// `blaze gc` asked for a lap now (`unkept`: `blaze clean --unkept`, every record no
    /// root keeps); a dry run only reports.
    pub(super) fn ask(&mut self, now: Duration, dry_run: bool, unkept: bool) {
        if !dry_run {
            self.collector.ask(unkept);
        }
        self.asked = Some(Asked { since: now, laps: self.collector.last_lap().0, dry_run });
    }

    /// Whether a `blaze gc` still waits for the store's lap: its answer is held until this
    /// is false ([`Collects::report`] then says what the lap did).
    pub(super) fn asked_waits(&mut self, now: Duration) -> bool {
        let Some(asked) = &self.asked else { return false };
        !asked.dry_run
            && self.collector.last_lap().0 == asked.laps
            && now.saturating_sub(asked.since) < ASKED_BOUND
    }

    /// What the store holds, what its collector did and what is kept and why, as `blaze gc`
    /// prints it after the build dirs' report.
    pub(super) fn report(&mut self, store: &Store, now: Duration) -> String {
        let usage = store.usage();
        let mut out = String::new(); // alloc: a report asked for
        out.push_str(&format!(
            "store: {} in {} packs, {} of it dead, {} collectable ({} records, {} of them kept by a root)\n",
            size_text(usage.bytes),
            usage.packs,
            size_text(usage.dead_bytes),
            size_text(usage.collectable_bytes()),
            usage.scanned.records,
            size_text(usage.scanned.rooted_bytes),
        ));
        match self.budget {
            Some(it) if it.budget > 0 => out.push_str(&format!(
                "home: {} held of a budget of {} ({}%), {} free on its volume\n",
                size_text(it.held),
                size_text(it.budget),
                it.held.saturating_mul(100) / it.budget,
                size_text(it.free),
            )),
            _ => out.push_str("home: its budget is not measured yet\n"),
        }
        if let Some(asked) = self.asked.take() {
            let (laps, lap) = self.collector.last_lap();
            match (asked.dry_run, laps > asked.laps) {
                (true, _) => out.push_str("store collection: a dry run forgets nothing\n"),
                (false, true) => out.push_str(&format!(
                    "store collection: {} packs retired, {} freed, {} records forgotten, {} copied on, {} packs passed by\n",
                    lap.retired,
                    size_text(lap.freed_bytes),
                    lap.forgotten,
                    size_text(lap.copied_bytes),
                    lap.skipped,
                )),
                (false, false) => {
                    let lap = self.collector.lap();
                    out.push_str(&format!(
                        "store collection: not ended after {} s (a pack is retired only between builds, and its roots are marked by the scan after a build): so far {} packs retired, {} freed; it goes on in the background\n",
                        now.saturating_sub(asked.since).as_secs(),
                        lap.retired,
                        size_text(lap.freed_bytes),
                    ));
                }
            }
        }
        if usage.periods < 2 {
            out.push_str(&format!(
                "store: nothing is forgotten yet: {} of the 2 periods of use it reads have ended since it opened\n",
                usage.periods
            ));
        }
        if let Some(waits) = self.collector.waits() {
            out.push_str(&format!("store collection: it waits for {}\n", waits.said()));
        }
        if let Standing::OverBudget { held, budget } = self.collector.standing() {
            out.push_str(&format!(
                "over budget: {} held of {}, and what is left is in use, kept, or was used within the hour; nothing in use was removed. `blaze gc --budget <size>` raises the budget\n",
                size_text(held),
                size_text(budget),
            ));
        }
        let counts = usage.counts;
        out.push_str(&format!(
            "since the daemon started: {} packs retired ({}), {} records forgotten ({}), {} copied on; {} steps on the loop, {} past {} ms\n",
            counts.packs_retired,
            size_text(counts.retired_bytes),
            counts.records_forgotten,
            size_text(counts.forgotten_bytes),
            size_text(counts.copied_bytes),
            self.steps,
            self.over_bound,
            STEP_BOUND.as_millis(),
        ));
        let mut pins = 0;
        for pin in store.pins() {
            pins += 1;
            out.push_str(&format!(
                "kept: pin `{}`: {} keys, {} ({})\n",
                pin.name,
                pin.keys.len(),
                size_text(store.stored_bytes(&pin.keys)),
                pin.why,
            ));
        }
        if pins == 0 {
            out.push_str("kept: no pin\n");
        }
        out.push_str(
            "kept besides: what a build read or made in the last two periods, what a kept record names (an output's chunks, a derived value's base), each output a build dir holds, and every pack used within the hour\n",
        );
        out
    }

    /// What the store and its home hold as a build ends, for its answer; with the line to
    /// print when this build should say something.
    pub(super) fn storage(&mut self, store: &Store, now: Duration) -> Storage {
        let usage = store.usage();
        let budget = self.budget.unwrap_or_default();
        let before = self.said_bytes.replace(usage.bytes).unwrap_or(usage.bytes);
        let mut storage = Storage {
            store_bytes: usage.bytes,
            store_delta_bytes: usage.bytes as i64 - before as i64,
            held_bytes: budget.held,
            budget_bytes: budget.budget,
            collectable_bytes: usage.collectable_bytes(),
            volume_free_bytes: budget.free,
            over_budget: matches!(self.collector.standing(), Standing::OverBudget { .. }),
            warning: None,
        };
        storage.warning = self.warner.line(now, &storage, budget.comfortable);
        storage
    }

    /// One turn: what came back from the workers is taken, the facts are told, and the core
    /// takes one step. `idle`: no build is under way, queued or being answered. `gc`: the
    /// collector of the build dirs, which measures the volume and counts the home's bytes:
    /// without one there is no budget, and only packs that are half dead bytes are rewritten.
    pub(super) fn turn(
        &mut self,
        store: &mut Store,
        gc: Option<&mut super::gc::Gc>,
        idle: bool,
        now: Duration,
        effects: &mut Vec<blaze_core::Effect>,
    ) {
        while let Ok(back) = self.recv.try_recv() {
            match back {
                Back::Batch(Some(batch)) => self.collector.back(batch),
                Back::Batch(None) => {
                    tracing::error!(
                        target: "blaze",
                        "a batch of the store's collector was lost to a panic while its pack was read: the pack is passed by for this lap"
                    );
                    self.collector.lost(store);
                }
                Back::Pins(scan) => {
                    self.pins_out = false;
                    match scan {
                        Some(scan) => store.pins_put(scan),
                        None => {
                            tracing::error!(
                                target: "blaze",
                                "the scan that reads the store's pins was lost to a panic: it is asked again"
                            );
                            store.plan_lost();
                        }
                    }
                }
                Back::Other(Ok(bytes)) => {
                    self.other_out = false;
                    self.other = bytes;
                }
                Back::Other(Err(why)) => {
                    self.other_out = false;
                    tracing::warn!(target: "blaze", "the bytes of the daemon's state were not measured: {why}");
                }
            }
        }
        if !self.other_out && self.other_at.is_none_or(|at| now.saturating_sub(at) >= OTHER_EVERY) {
            self.other_at = Some(now);
            self.measure_other();
        }
        // A build began since the last turn: what it finds are flags no batch scanned
        // before has seen.
        if self.was_idle && !idle {
            self.builds += 1;
        }
        self.was_idle = idle;
        let usage = store.usage();
        let mine = usage.bytes.saturating_add(self.other);
        self.budget = gc.and_then(|gc| {
            gc.holds(mine);
            let (held, budget, free, comfortable) = gc.budget()?;
            Some(Budget { held, budget, free, comfortable })
        });
        let (held, budget) = match self.budget {
            Some(it) => (it.held, it.budget),
            // No budget: only what is half dead is rewritten.
            None => (0, u64::MAX),
        };
        self.collector.advance_clock(now);
        self.collector.observe(Facts {
            held,
            budget,
            idle,
            builds: self.builds,
            seconds: blaze_gc::record::now_ns() / 1_000_000_000,
        });
        // Nothing to step: no span, no clock read.
        if self.collector.out() || self.collector.resting() {
            return;
        }
        let pressure = Facts { held, budget, ..Facts::default() }.pressure();
        let began = std::time::Instant::now();
        let span = tracing::info_span!(
            target: "blaze",
            "store gc step",
            did = tracing::field::Empty,
            bytes = tracing::field::Empty,
            pressure,
        )
        .entered();
        match self.collector.step(store, effects) {
            Step::Rest => {}
            Step::Work(batch) => {
                span.record("did", "take");
                self.steps += 1;
                self.work(batch, pressure);
            }
            Step::Stepped(did) => {
                span.record("did", did.name());
                match did {
                    Did::Copied(bytes) => _ = span.record("bytes", bytes),
                    Did::Retired { freed, .. } => _ = span.record("bytes", freed),
                    _ => {}
                }
                self.steps += 1;
                let took = began.elapsed();
                if took > STEP_BOUND {
                    self.over_bound += 1;
                    tracing::warn!(
                        target: "blaze",
                        "a step of the store's collector ({}) held the loop for {} us, past its bound of {} us",
                        did.name(),
                        took.as_micros(),
                        STEP_BOUND.as_micros(),
                    );
                }
            }
        }
    }

    /// `batch` handed to be read; it comes back over the channel. Near the budget the read
    /// runs with the builds' work, else behind it.
    fn work(&mut self, mut batch: CollectBatch, pressure: u32) {
        // clone: a channel's sender, once a batch
        let send = self.send.clone();
        let work = move || {
            let span = tracing::info_span!(
                target: "blaze",
                "store gc read",
                pack = batch.pack(),
                bytes = batch.bytes(),
                kept = tracing::field::Empty,
                forgotten = tracing::field::Empty,
            )
            .entered();
            batch.work();
            span.record("kept", batch.kept_bytes());
            span.record("forgotten", batch.forgotten().1);
            batch
        };
        match self.run {
            Compress::Inline => _ = send.send(Back::Batch(Some(work()))),
            Compress::Pool => {
                let class = if pressure >= FULL / 2 { Class::Normal } else { Class::Background };
                blaze_graph::exec::shared().submit_as(
                    None,
                    class,
                    work,
                    // A panic fails this one job (panics-fail-one-job): the batch is lost,
                    // its pack stays as it is and is passed by for the lap.
                    move |done| _ = send.send(Back::Batch(done.ok())),
                );
            }
        }
    }

    /// Measures the bytes of the daemon's state outside the store on a worker: the image,
    /// the journal, the logs, the profile history.
    fn measure_other(&mut self) {
        // clone: the path and a sender, once a minute
        let (state, send) = (self.state.clone(), self.send.clone());
        let work = move || {
            let _span = tracing::info_span!(target: "blaze", "store gc measure the state").entered();
            other_bytes(&state)
        };
        self.other_out = true;
        match self.run {
            Compress::Inline => _ = send.send(Back::Other(work())),
            Compress::Pool => blaze_graph::exec::shared().submit_as(
                None,
                Class::Background,
                work,
                move |done| {
                    _ = send.send(Back::Other(
                        done.unwrap_or_else(|_| Err("the measure panicked".to_owned())),
                    ));
                },
            ),
        }
    }
}

/// The bytes on disk of everything under `state` but its `store` directory, each file's
/// blocks counted where the platform says them. A directory that is not there holds nothing;
/// any other error is why.
fn other_bytes(state: &Path) -> Result<u64, String> {
    fn walk(dir: &Path, top: bool, sum: &mut u64) -> Result<(), String> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("{}: {error}", dir.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
            if top && entry.file_name() == "store" {
                continue;
            }
            // A file removed as it is listed is not held.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), false, sum)?;
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;
                    *sum += meta.blocks() * 512;
                }
                #[cfg(not(unix))]
                {
                    *sum += meta.len();
                }
            }
        }
        Ok(())
    }
    let mut sum = 0;
    walk(state, true, &mut sum)?;
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn storage(held: u64, budget: u64, free: u64) -> Storage {
        Storage { held_bytes: held, budget_bytes: budget, volume_free_bytes: free, ..Storage::default() }
    }

    const fn hours(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    /// The invariant of [`Warner`]: nothing under 85% with room on the volume, a line at 85%
    /// that names both commands, no second line within six hours (one hour when over), and a
    /// low volume says so whatever the share.
    #[test]
    fn a_build_says_the_home_is_near_its_budget_only_then_and_not_again_for_hours() {
        let mut warner = Warner::default();
        let comfortable = 30 * GIB;
        assert_eq!(warner.line(hours(0), &storage(54 * GIB, 64 * GIB, 100 * GIB), comfortable), None);
        assert_eq!(warner.line(hours(0), &storage(0, 0, 0), comfortable), None, "no budget measured");
        let line = warner.line(hours(1), &storage(55 * GIB, 64 * GIB, 100 * GIB), comfortable).unwrap();
        assert!(line.contains("55.0GiB of its budget of 64.0GiB (85%)"), "{line}");
        assert!(line.contains("`blaze gc --budget <size>`") && line.contains("`blaze clean --unkept`"), "{line}");
        assert_eq!(warner.line(hours(6), &storage(60 * GIB, 64 * GIB, 100 * GIB), comfortable), None);
        assert!(warner.line(hours(7), &storage(60 * GIB, 64 * GIB, 100 * GIB), comfortable).is_some());
        // Over the budget: once an hour, and it says nothing in use was removed once the
        // collector has looked at everything.
        assert_eq!(warner.line(hours(7) + Duration::from_secs(60), &storage(65 * GIB, 64 * GIB, 100 * GIB), comfortable), None);
        let over = Storage { over_budget: true, ..storage(65 * GIB, 64 * GIB, 100 * GIB) };
        let line = warner.line(hours(8), &over, comfortable).unwrap();
        assert!(line.contains("what is left is in use or kept"), "{line}");
        assert_eq!(warner.line(hours(8) + Duration::from_secs(1800), &over, comfortable), None);
        // A volume low on space says so whatever the share of the budget.
        let mut warner = Warner::default();
        let line = warner.line(hours(0), &storage(GIB, 64 * GIB, 10 * GIB), comfortable).unwrap();
        assert!(line.contains("the volume has 10.0GiB free, under its comfortable 30.0GiB"), "{line}");
    }

    /// A store on the simulation with `count` short records put and committed, enough of
    /// them that its index table holds them (only a record the index holds can be forgotten).
    fn filled(sim: &mut blaze_core::sim::Sim, count: u64) -> Store {
        let mut store = opened(sim);
        let mut effects = Vec::new();
        for n in 0..count {
            store.put(key(n), blaze_cas::Kind::Code, &n.to_le_bytes().repeat(8)).unwrap();
        }
        store.flush(&mut effects);
        driven(sim, &mut store, &mut effects);
        store
    }

    /// The store of `home/v2/store` on the simulation, opened.
    fn opened(sim: &mut blaze_core::sim::Sim) -> Store {
        let name = |it: &str| blaze_core::FileName::new(it).unwrap();
        let config = blaze_cas::Config {
            dir: name("home/v2/store"),
            layers: Vec::new(),
            tags: 1 << 40,
            load_tags: (1 << 40) + (1 << 32),
            loader: blaze_cas::Loader::Pumped,
        };
        let mut effects = Vec::new();
        let mut store = Store::open(config, &mut effects);
        driven(sim, &mut store, &mut effects);
        store
    }

    fn key(n: u64) -> blaze_cas::Key {
        blaze_cas::Key(blaze_ids::ContentId::of("blaze store collect test key", &n.to_le_bytes()))
    }

    fn driven(sim: &mut blaze_core::sim::Sim, store: &mut Store, effects: &mut Vec<blaze_core::Effect>) {
        blaze_core::run_until_idle(sim, effects, |completion, effects| {
            if store.owns(completion.tag) {
                store.complete(completion, effects);
            }
        })
        .expect("the effects end");
    }

    /// Periods ended until `periods` have, each a scan run here.
    fn periods(sim: &mut blaze_core::sim::Sim, store: &mut Store, periods: u32) {
        let mut ended = 0;
        for _ in 0..200 {
            if ended == periods {
                return;
            }
            let mut effects = Vec::new();
            store.flush(&mut effects);
            driven(sim, store, &mut effects);
            ended += u32::from(store.end_period());
        }
        panic!("the store ended {ended} of {periods} periods in 200 asks");
    }

    /// A store filled earlier (by a blaze from before pins, or imported), then one build
    /// that only finds the standard library's records (`found`: it queues them as a build
    /// does, [`keep`]), then every unrooted record collected: whether each of the standard
    /// library's keys is still there, and how many records were forgotten.
    fn found_then_collected(found: bool) -> (Vec<bool>, u64) {
        use blaze_core::Driver as _;
        let mut sim = blaze_core::sim::Sim::new(31, blaze_core::sim::Faults::default());
        // Past the 65,536 records after which the store writes its index table.
        let mut store = filled(&mut sim, 70_000);
        let std: Vec<blaze_cas::Key> = (100..140).map(key).collect();
        let gone = std::env::temp_dir().join(format!("blaze-collect-none-{}", std::process::id()));
        let pace = Pace { grace: Duration::ZERO, ..Pace::default() };
        let mut collects = Collects::new(&gone, Compress::Inline, pace);
        // The first scan reads the store's pins (it has none); then the build.
        periods(&mut sim, &mut store, 1);
        if found {
            keep(TOOLCHAIN, std.iter().copied());
        }
        collects.build_ended(&mut store);
        // (Other tests of the process may have queued keys of their own for the pin: the
        // queue is the process's. They are never this store's.)
        let pinned = |store: &Store| {
            store
                .pins()
                .find(|pin| pin.name == TOOLCHAIN && std.iter().all(|key| pin.keys.contains(key)))
                .map(|pin| (pin.when, pin.keys.len()))
        };
        let when = pinned(&store);
        assert_eq!(when.is_some(), found, "the pin is written by the build that found them, and only then");
        // Again, the same keys found: the pin is not written again (unless another test's
        // keys came with them, which makes it a longer pin).
        if found {
            keep(TOOLCHAIN, std.iter().copied());
            collects.build_ended(&mut store);
            let again = pinned(&store);
            assert!(
                again == when || again.map(|it| it.1) > when.map(|it| it.1),
                "a build that finds what is pinned wrote the pin again: {when:?} then {again:?}"
            );
        }
        // The records go unused for two periods and more, and everything unrooted is asked
        // to go.
        periods(&mut sim, &mut store, 3);
        collects.ask(sim.now(), false, true);
        for _ in 0..20_000 {
            let mut effects = Vec::new();
            collects.turn(&mut store, None, true, sim.now(), &mut effects);
            driven(&mut sim, &mut store, &mut effects);
            if !collects.asked_waits(sim.now()) && store.usage().counts.packs_retired > 0 && !store.committing() {
                break;
            }
            sim.advance(Duration::from_millis(60));
        }
        let guard = blaze_cas::enter();
        let reader = store.reader();
        let there = std.iter().map(|key| reader.get(&guard, key).unwrap().is_some()).collect();
        (there, store.usage().counts.records_forgotten)
    }

    /// The invariant: the standard library's records a build found are pinned by that build,
    /// though it wrote none of them, and a collection of everything unrooted keeps them. The
    /// control is the pin made on write only: the same store and the same collection with
    /// nothing queued by the build that found them, which forgets them.
    #[test]
    fn records_a_build_only_found_are_pinned_and_survive_a_collection() {
        let (there, forgotten) = found_then_collected(true);
        assert!(forgotten > 1000, "the collection forgot {forgotten} records: it did not run");
        assert!(there.iter().all(|it| *it), "a pinned record of the standard library was forgotten: {there:?}");
        let (there, forgotten) = found_then_collected(false);
        assert!(forgotten > 1000, "the control's collection forgot {forgotten} records: it did not run");
        assert!(there.iter().any(|it| !*it), "the control kept every record with no pin: it tests nothing");
    }

    /// A store an earlier daemon left with a pin, opened by a fresh daemon that has built
    /// nothing; then turns of the collector's driver. `asks`: a request that needs the pins
    /// asks for them each turn ([`Collects::read_pins`]). How it ended, and whether the
    /// store's pins were read by then.
    fn pins_of_a_fresh_daemon(asks: bool) -> (PinsRead, bool) {
        use blaze_core::Driver as _;
        let mut sim = blaze_core::sim::Sim::new(41, blaze_core::sim::Faults::default());
        let mut store = filled(&mut sim, 300);
        periods(&mut sim, &mut store, 1);
        store.pin_add("imported", "a pack an earlier daemon imported", 7, &[key(1), key(2)]).unwrap();
        periods(&mut sim, &mut store, 1);
        drop(store);
        let mut store = opened(&mut sim);
        assert!(!store.pins_read() && store.pins().count() == 0, "a store just opened has read no pin");
        let gone = std::env::temp_dir().join(format!("blaze-collect-none-{}", std::process::id()));
        let mut collects = Collects::new(&gone, Compress::Inline, Pace::default());
        let start = sim.now();
        let mut said = PinsRead::Reading;
        // Turns of a daemon no build is asked of, for longer than the bound.
        while sim.now().saturating_sub(start) < PINS_BOUND * 2 {
            if asks {
                said = collects.read_pins(&mut store, sim.now());
                if said != PinsRead::Reading {
                    break;
                }
            }
            let mut effects = Vec::new();
            collects.turn(&mut store, None, true, sim.now(), &mut effects);
            driven(&mut sim, &mut store, &mut effects);
            sim.advance(Duration::from_millis(50));
        }
        (said, store.pins_read() && store.pins().any(|pin| pin.name == "imported" && pin.keys.len() == 2))
    }

    /// The invariant: a request that needs the store's pins gets them from a daemon that has
    /// built nothing, by the scan it starts itself, well within its bound. The control is the
    /// daemon as it was (the pins read only by the scan of a period's end, which follows a
    /// build): with no build they are never read, so a request waiting on them runs out its
    /// bound.
    #[test]
    fn a_request_that_needs_the_pins_reads_them_from_a_daemon_that_has_built_nothing() {
        assert_eq!(pins_of_a_fresh_daemon(true), (PinsRead::Ready, true));
        let (_, read) = pins_of_a_fresh_daemon(false);
        assert!(!read, "the control read the pins with no build and no ask: it tests nothing");
    }

    #[test]
    fn the_bytes_of_the_state_leave_the_store_out() {
        let dir = std::env::temp_dir().join(format!("blaze-collect-other-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("store")).unwrap();
        std::fs::create_dir_all(dir.join("db")).unwrap();
        std::fs::write(dir.join("store/pack-000000"), vec![1u8; 1 << 20]).unwrap();
        std::fs::write(dir.join("db/image"), vec![1u8; 64 << 10]).unwrap();
        std::fs::write(dir.join("journal"), vec![1u8; 8 << 10]).unwrap();
        let bytes = other_bytes(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!((72 << 10..512 << 10).contains(&bytes), "{bytes}");
        assert_eq!(other_bytes(&dir.join("gone")), Ok(0));
    }
}
