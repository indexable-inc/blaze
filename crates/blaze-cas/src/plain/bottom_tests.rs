//! The plain store on both bottoms (D182, rule 78): one crash simulation and one oracle for
//! [`OverDir`] and [`OverBlock`], a differential check of the two, and [`OverBlock`]'s three
//! plants.
//!
//! The programme: rounds of puts committed in durable mode, a compaction now and then, and a
//! power cut drawn at any completion of a commit or of nothing (a crash after the round).
//! After each cut the bottom is opened again over the crashed simulation and the store over
//! it. The oracle:
//!
//! - a record whose commit was acknowledged reads back with equal bytes ([`Bottomed::Lost`],
//!   [`Bottomed::Differs`]);
//! - a record whose commit was cut reads back whole or not at all; a key never put is never
//!   found (nothing half-visible: [`Bottomed::Differs`], [`Bottomed::NeverPut`]);
//! - on [`OverBlock`], the head an open reads is one of the last two generations: the durable
//!   one or the one a flip had started ([`Bottomed::Head`]);
//! - an open or a commit that breaks on its own is [`Bottomed::Broke`], naming why.

use std::{alloc::System, collections::BTreeMap, fmt, path::Path, time::Duration};

use blaze_core::{
    Boot, Completion, Done, Driver, Effect, FileName, NoBoot, Tag as EffectTag,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_sim::{Broken, Config, Tape, check, controls};

use super::{
    Liveness,
    bottom::{Bottom, BottomPlant, Generation, Limits, OverBlock, OverDir},
    core::{PlainConfig, TAGS},
    driven::{Cut, Driven},
};
use crate::Key;

/// What the oracle saw broken.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Bottomed {
    /// An acknowledged record is not found.
    Lost { key: u32 },
    /// A record read back with bytes it was never put with.
    Differs { key: u32 },
    /// A key never put was found.
    NeverPut { key: u32 },
    /// The head an open read is not one of the last two generations.
    Head { got: Generation, durable: Generation, started: Generation },
    /// An open, a commit or a read broke on its own.
    Broke { what: &'static str, why: String },
}

impl fmt::Display for Bottomed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Bottomed::Lost { key } => write!(f, "key {key}: committed and acknowledged, not found"),
            Bottomed::Differs { key } => write!(f, "key {key}: read back with other bytes than it was put with"),
            Bottomed::NeverPut { key } => write!(f, "key {key}: never put, found"),
            Bottomed::Head { got, durable, started } => {
                write!(f, "an open read {got}: not the durable {durable} nor the started {started}")
            }
            Bottomed::Broke { what, why } => write!(f, "{what}: {why}"),
        }
    }
}

fn broke(what: &'static str, why: impl fmt::Display) -> Broken<Bottomed> {
    Broken::Oracle(Bottomed::Broke { what, why: why.to_string() })
}

/// A bottom the programme runs on, over the simulation.
trait Rig: Bottom<Inner = Sim> {
    fn open(sim: Sim, plant: BottomPlant) -> Result<Self, Broken<Bottomed>>;
    /// The durable generation and the started one, where the bottom has generations.
    fn heads(&self) -> Option<(Generation, Generation)>;
}

impl Rig for OverDir<Sim> {
    fn open(sim: Sim, _plant: BottomPlant) -> Result<Self, Broken<Bottomed>> {
        Ok(OverDir::new(sim))
    }

    fn heads(&self) -> Option<(Generation, Generation)> {
        None
    }
}

type Block = OverBlock<Sim, System, BottomPlant>;

fn device() -> FileName {
    FileName::new("bottom/device").expect("a plain name")
}

impl Rig for Block {
    fn open(sim: Sim, plant: BottomPlant) -> Result<Self, Broken<Bottomed>> {
        OverBlock::open(sim, device(), Limits::SIMULATED, System, plant).map_err(|e| broke("opening the device", e))
    }

    fn heads(&self) -> Option<(Generation, Generation)> {
        Some((self.generation(), self.started()))
    }
}

const TAG: Tag = Tag(1);

fn key(n: u32) -> Key {
    Key(ContentId::of("bottom test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    let len = 1 + (n as usize * 37) % 700;
    (0..len).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

fn cfg(opens: u64) -> PlainConfig<System> {
    let mut path = Vec::new_in(System);
    path.extend_from_slice(b"/sim");
    let mut machine = Vec::new_in(System);
    machine.push(path);
    let mut writes = Vec::new_in(System);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(System);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a plain name"),
        tags: TAGS,
        cap_bytes: u64::MAX,
        boot: opens,
        nonce: 100 + opens,
        durable: true,
        machine,
        writes,
        reads,
        liveness: Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

#[derive(Clone, Copy, Debug)]
struct Round {
    puts: u32,
    compact: bool,
    /// A power cut after this many completions of the commit; or none.
    cut: Option<u32>,
    /// A crash after the round, once it ended.
    crash: bool,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    rounds: Vec<Round>,
}

fn plan(tape: &Tape, cuts: bool) -> Plan {
    let seed = tape.next_u64("sim seed");
    let rounds = (0..1 + tape.below(6, "rounds"))
        .map(|_| Round {
            puts: 1 + tape.below(5, "puts") as u32,
            compact: tape.below(4, "compact") == 0,
            cut: (cuts && tape.below(3, "cut") == 0).then(|| tape.int_in(0..=200u32, "cut after")),
            crash: cuts && tape.coin("crash"),
        })
        .collect();
    Plan { seed, rounds }
}

#[derive(Default)]
struct Ledger {
    acked: BTreeMap<u32, ()>,
    maybe: BTreeMap<u32, ()>,
    next: u32,
}

/// Every key the ledger knows, and one never put, read and judged.
fn judge<R: Rig>(store: &mut Driven<R, System>, ledger: &Ledger) -> Result<(), Broken<Bottomed>> {
    for key_n in 0..=ledger.next {
        let found = store.get(&key(key_n)).map_err(|e| broke("a read", e))?;
        let bytes = found.map(|found| store.got(found).bytes.to_vec());
        let acked = ledger.acked.contains_key(&key_n);
        let maybe = ledger.maybe.contains_key(&key_n);
        match bytes {
            None if acked => return Err(Broken::Oracle(Bottomed::Lost { key: key_n })),
            None => {}
            Some(_) if !acked && !maybe => return Err(Broken::Oracle(Bottomed::NeverPut { key: key_n })),
            Some(bytes) if bytes != value(key_n) => return Err(Broken::Oracle(Bottomed::Differs { key: key_n })),
            Some(_) => {}
        }
    }
    Ok(())
}

/// The store over `bottom`, opened.
fn store<R: Rig>(bottom: R, opens: u64) -> Result<Driven<R, System>, Broken<Bottomed>> {
    Driven::with(bottom, Path::new("/sim"), cfg(opens), System).map_err(|e| broke("opening the store", e))
}

/// A power cut: the store and the bottom let go of, the simulation crashed, both opened again,
/// and the head the new open read judged against the last two generations.
fn crash<R: Rig>(open: Driven<R, System>, plant: BottomPlant, opens: &mut u64) -> Result<Driven<R, System>, Broken<Bottomed>> {
    let bottom = open.into_driver();
    let heads = bottom.heads();
    let mut sim = bottom.into_inner();
    sim.crash();
    let bottom = R::open(sim, plant)?;
    if let (Some((durable, started)), Some((got, _))) = (heads, bottom.heads())
        && got != durable
        && got != started
    {
        return Err(Broken::Oracle(Bottomed::Head { got, durable, started }));
    }
    *opens += 1;
    store(bottom, *opens)
}

/// The programme on `R`: what each judging read saw, for the differential check.
fn run<R: Rig>(plan: &Plan, plant: BottomPlant) -> Result<Vec<Option<Vec<u8>>>, Broken<Bottomed>> {
    let sim = Sim::new(plan.seed, Faults::default());
    let mut opens = 1;
    let mut store = store(R::open(sim, plant)?, opens)?;
    let mut ledger = Ledger::default();
    for round in &plan.rounds {
        let first = ledger.next;
        for n in first..first + round.puts {
            store.put(key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|e| broke("a put", format!("{e:?}")))?;
            ledger.maybe.insert(n, ());
        }
        ledger.next = first + round.puts;
        store.cut(round.cut.map(Cut::After));
        let committed = store.commit_with(&[]);
        store.cut(None);
        match (committed, round.cut) {
            (Ok(_), _) => {
                for n in first..ledger.next {
                    ledger.maybe.remove(&n);
                    ledger.acked.insert(n, ());
                }
            }
            // The cut stopped it: the round's keys stay maybe, and the power is out.
            (Err(_), Some(_)) => {
                store = crash(store, plant, &mut opens)?;
                judge(&mut store, &ledger)?;
                continue;
            }
            (Err(why), None) => return Err(broke("a commit", why)),
        }
        if round.compact {
            store.compact(4).map_err(|e| broke("a compaction", e))?;
        }
        if round.crash {
            store = crash(store, plant, &mut opens)?;
        }
        judge(&mut store, &ledger)?;
    }
    let mut seen = Vec::new();
    for n in 0..=ledger.next {
        let found = store.get(&key(n)).map_err(|e| broke("a read", e))?;
        seen.push(found.map(|found| store.got(found).bytes.to_vec()));
    }
    Ok(seen)
}

blaze_sim::kind_test! {
    /// The crash simulation on today's bottom: the oracle holds.
    simulation fn the_store_over_folders_keeps_what_it_acknowledged() {
        check(Config::cases(100), |tape: &Tape| run::<OverDir<Sim>>(&plan(tape, true), BottomPlant::None).map(|_| ()));
    }
}

blaze_sim::kind_test! {
    /// The same simulation and oracle on one device.
    simulation fn the_store_over_one_device_keeps_what_it_acknowledged() {
        check(Config::cases(100), |tape: &Tape| run::<Block>(&plan(tape, true), BottomPlant::None).map(|_| ()));
    }
}

blaze_sim::kind_test! {
    /// With no power cut the two bottoms read the same records with the same bytes.
    simulation fn both_bottoms_read_the_same() {
        check(Config::cases(100), |tape: &Tape| {
            let plan = plan(tape, false);
            let folders = run::<OverDir<Sim>>(&plan, BottomPlant::None)?;
            let device = run::<Block>(&plan, BottomPlant::None)?;
            match folders.iter().zip(&device).position(|(a, b)| a != b) {
                None => Ok(()),
                Some(key) => Err(Broken::Harness(format!("key {key}: the folders read {:?}, the device {:?}", folders[key].as_ref().map(Vec::len), device[key].as_ref().map(Vec::len)))),
            }
        });
    }
}

blaze_sim::plants! {
    impl BottomPlant breaks Bottomed {
        None,
        BothSlotsInOneStep => Bottomed::Head { .. } | Bottomed::Lost { .. } | Bottomed::Broke { .. },
        FreedBeforeFlip => Bottomed::Lost { .. } | Bottomed::Differs { .. } | Bottomed::Broke { .. },
        FreeMapTrusted => Bottomed::Lost { .. } | Bottomed::Differs { .. } | Bottomed::Broke { .. },
    }
}

blaze_sim::kind_test! {
    /// Each plant caught by the oracle on one device, by the programme built for it
    /// ([`stepped`]). Not by the store's programme ([`run`]): it caught none of the three in
    /// 300 cases (2026-11-20, build 67), because each needs what a store cannot ask of its
    /// bottom. `OverBlock::poll` runs a whole flip inside one outer completion, so a cut counted
    /// in the store's completions never lands between a flip's slot write and its sync
    /// (`BothSlotsInOneStep`); the store never writes a file before the flip that drops another
    /// is durable, since it removes a segment only after the flip that stops naming it (and a
    /// moved log's old run is never handed out before the log's own sync flips:
    /// `FreedBeforeFlip`); and a commit's last flip (the head's sync) places no file, so the
    /// stored map, one flip behind, is behind by nothing an open can hand out again
    /// (`FreeMapTrusted`).
    simulation fn every_bottom_plant_is_caught() {
        controls(Config::cases(300), stepped);
    }
}

/// A driver over the simulation that, once [`Cutter::arm`]ed with `n`, hands back `n` more
/// completions and then nothing: every effect asked after the `n`th is never done, a power cut
/// between two of the bottom's own steps (the device's sync and the slot's write of one flip),
/// which a cut counted in the store's completions cannot reach.
struct Cutter {
    sim: Sim,
    left: Option<u32>,
    cut: bool,
}

impl Cutter {
    fn arm(&mut self, n: u32) {
        self.left = Some(n);
        self.cut = n == 0;
    }
}

impl Driver for Cutter {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        if self.cut {
            // Past the cut: asked, never done.
            effects.clear();
            return;
        }
        self.sim.submit(effects);
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        if self.cut {
            return 0;
        }
        let mut got = Vec::new();
        self.sim.poll(&mut got, until);
        if let Some(left) = &mut self.left {
            // An effect past the last handed back happened or not; its completion is lost
            // with the power, which is a cut the device allows.
            let keep = (*left as usize).min(got.len());
            got.truncate(keep);
            *left -= keep as u32;
            if *left == 0 {
                self.cut = true;
            }
        }
        let n = got.len();
        out.append(&mut got);
        n
    }

    fn in_flight(&self) -> usize {
        if self.cut { 0 } else { self.sim.in_flight() }
    }

    fn now(&self) -> Duration {
        self.sim.now()
    }

    fn wall_ns(&self) -> u64 {
        self.sim.wall_ns()
    }

    fn boot(&self) -> Result<Boot, NoBoot> {
        self.sim.boot()
    }
}

type Stepped = OverBlock<Cutter, System, BottomPlant>;

fn open_stepped(sim: Sim, plant: BottomPlant) -> Result<Stepped, Broken<Bottomed>> {
    OverBlock::open(Cutter { sim, left: None, cut: false }, device(), Limits::SIMULATED, System, plant).map_err(|e| broke("opening the device", e))
}

/// A file of the stepped programme, and its bytes: `len` of them, other bytes for each `n`.
fn file_n(n: u32) -> FileName {
    FileName::new(&format!("files/f{n}")).expect("a plain name")
}

fn bytes_n(n: u32, len: usize) -> Vec<u8> {
    (0..len).map(|i| (n as usize * 131 + i * 7 + 1) as u8).collect()
}

/// `effect` asked of `bottom` and polled to its completion; `None` when the cut stopped it.
fn ask(bottom: &mut Stepped, effect: Effect, tag: EffectTag) -> Result<Option<Done>, Broken<Bottomed>> {
    let mut effects = vec![effect];
    bottom.submit(&mut effects);
    let mut out = Vec::new();
    for _ in 0..100_000 {
        let until = Deadline::after(bottom.now(), Duration::from_secs(60));
        bottom.poll(&mut out, until);
        if let Some(at) = out.iter().position(|it| it.tag == tag) {
            return Ok(Some(out.swap_remove(at).done));
        }
        if bottom.inner().cut {
            return Ok(None);
        }
    }
    Err(Broken::Harness(format!("effect {tag:?} never completed and no cut stopped it")))
}

/// File `n` written whole and durably (`Effect::Write`): `true` when acknowledged, `false`
/// when the cut stopped it.
fn write_n(bottom: &mut Stepped, n: u32, len: usize) -> Result<bool, Broken<Bottomed>> {
    match ask(bottom, Effect::Write { file: file_n(n), bytes: bytes_n(n, len), tag: EffectTag(1) }, EffectTag(1))? {
        None => Ok(false),
        Some(Done::Written) => Ok(true),
        Some(other) => Err(broke("a write", format!("{other:?}"))),
    }
}

/// File `n` read back and judged: when `acked`, it must read its bytes; else it may be gone,
/// and if it is there it reads its bytes.
fn judge_n(bottom: &mut Stepped, n: u32, len: usize, acked: bool) -> Result<(), Broken<Bottomed>> {
    let bytes = match ask(bottom, Effect::ReadAll { file: file_n(n), tag: EffectTag(2) }, EffectTag(2))? {
        Some(Done::Read(bytes)) => bytes,
        Some(other) => return Err(broke("a read", format!("{other:?}"))),
        None => return Err(Broken::Harness("a read stopped with no cut armed".to_string())),
    };
    match bytes.is_empty() {
        true if acked => Err(Broken::Oracle(Bottomed::Lost { key: n })),
        true => Ok(()),
        false if bytes != bytes_n(n, len) => Err(Broken::Oracle(Bottomed::Differs { key: n })),
        false => Ok(()),
    }
}

/// A power cut: the simulation crashed, the device opened again, and the head it read judged
/// against the last two generations.
fn crash_stepped(bottom: Stepped, plant: BottomPlant) -> Result<Stepped, Broken<Bottomed>> {
    let (durable, started) = (bottom.generation(), bottom.started());
    let mut sim = bottom.into_inner().sim;
    sim.crash();
    let bottom = open_stepped(sim, plant)?;
    let got = bottom.generation();
    if got != durable && got != started {
        return Err(Broken::Oracle(Bottomed::Head { got, durable, started }));
    }
    Ok(bottom)
}

/// The programme built for the plants: three histories, each on its own new device, each
/// reaching one plant by construction (no draw decides whether the planted step runs).
///
/// 1. Both slots (`BothSlotsInOneStep`): file 0 written; file 1's write cut after its third
///    step on the device (its data, the device's sync, the slot's write), so the slot is written
///    and not synced. The power cut then keeps, drops or tears that one write; torn, the plant
///    leaves no slot whole (the open breaks: no head), where one slot a flip writes leaves the
///    other. The tear is the simulation's draw (about one cut in six), the only step here that
///    is not built: blaze-core's `Sim` takes no chosen cut.
/// 2. Freed before the flip (`FreedBeforeFlip`): files 0 and 1 written, file 0 removed (the
///    removal is in no durable generation yet), file 2 of file 0's length written and cut after
///    its data and the device's sync: its bytes are durable and its slot is not. Planted, file 0's
///    run was free at once and file 2 took it (first fit), so the durable head names file 0 over
///    file 2's bytes.
/// 3. The stored map trusted (`FreeMapTrusted`): file 0 written, the power cut (nothing
///    unsynced), the device opened again and file 1 of file 0's length written. Planted, the open
///    trusted the map stored with file 0's flip, which is the map before it, and handed file 0's
///    run to file 1.
fn stepped(tape: &Tape, plant: BottomPlant) -> Result<(), Broken<Bottomed>> {
    let seed = tape.next_u64("sim seed");
    let len = tape.int_in(1..=4096u32, "file length") as usize;

    // 1. Both slots in one step.
    let mut bottom = open_stepped(Sim::new(seed, Faults::default()), plant)?;
    if !write_n(&mut bottom, 0, len)? {
        return Err(Broken::Harness("history 1: file 0's write stopped with no cut armed".to_string()));
    }
    bottom.inner().arm(3);
    if write_n(&mut bottom, 1, len)? {
        return Err(Broken::Harness("history 1: file 1's write completed through its cut".to_string()));
    }
    let mut bottom = crash_stepped(bottom, plant)?;
    judge_n(&mut bottom, 0, len, true)?;
    judge_n(&mut bottom, 1, len, false)?;

    // 2. An extent freed before the flip that drops it is durable.
    let mut bottom = open_stepped(Sim::new(seed ^ 0x2222_2222, Faults::default()), plant)?;
    for n in [0, 1] {
        if !write_n(&mut bottom, n, len)? {
            return Err(Broken::Harness(format!("history 2: file {n}'s write stopped with no cut armed")));
        }
    }
    match ask(&mut bottom, Effect::Remove { file: file_n(0), tag: EffectTag(3) }, EffectTag(3))? {
        Some(Done::Removed) => {}
        other => return Err(broke("a removal", format!("{other:?}"))),
    }
    bottom.inner().arm(2);
    if write_n(&mut bottom, 2, len)? {
        return Err(Broken::Harness("history 2: file 2's write completed through its cut".to_string()));
    }
    let mut bottom = crash_stepped(bottom, plant)?;
    judge_n(&mut bottom, 0, len, false)?;
    judge_n(&mut bottom, 1, len, true)?;
    judge_n(&mut bottom, 2, len, false)?;

    // 3. The stored free map trusted at open.
    let mut bottom = open_stepped(Sim::new(seed ^ 0x3333_3333, Faults::default()), plant)?;
    if !write_n(&mut bottom, 0, len)? {
        return Err(Broken::Harness("history 3: file 0's write stopped with no cut armed".to_string()));
    }
    let mut bottom = crash_stepped(bottom, plant)?;
    if !write_n(&mut bottom, 1, len)? {
        return Err(Broken::Harness("history 3: file 1's write stopped with no cut armed".to_string()));
    }
    judge_n(&mut bottom, 0, len, true)?;
    judge_n(&mut bottom, 1, len, true)
}

/// The marks of the drawn cases, against [`Limits::SIMULATED`] (D165): printed for the
/// coordinator's run, and each under its limit.
#[test]
fn the_simulated_limits_hold_what_the_cases_draw() {
    let mut most = super::bottom::HighWater::default();
    for seed in 0..64u64 {
        let rounds = (0..6).map(|n| Round { puts: 5, compact: n % 2 == 1, cut: None, crash: n % 3 == 2 }).collect();
        let plan = Plan { seed, rounds };
        let sim = Sim::new(plan.seed, Faults::default());
        let mut opens = 1;
        let mut store = store(<Block as Rig>::open(sim, BottomPlant::None).expect("the device opens"), opens).expect("the store opens");
        for (r, round) in plan.rounds.iter().enumerate() {
            for n in 0..round.puts {
                let n = r as u32 * 5 + n;
                store.put(key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put");
            }
            store.commit_with(&[]).expect("a commit");
            if round.compact {
                store.compact(4).expect("a compaction");
            }
            let high = store.driver().high_water();
            most = super::bottom::HighWater {
                files: most.files.max(high.files),
                end: most.end.max(high.end),
                table_bytes: most.table_bytes.max(high.table_bytes),
                generation: most.generation.max(high.generation),
            };
            if round.crash {
                store = crash(store, BottomPlant::None, &mut opens).unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
            }
        }
    }
    println!("D165 OverBlock marks over 64 seeds: {most:?} against {:?}", Limits::SIMULATED);
    let limits = Limits::SIMULATED;
    assert!(most.files < limits.files, "{} files at most, the limit {}", most.files, limits.files);
    assert!(most.end <= limits.device_bytes, "the device used to {} of {}", most.end, limits.device_bytes);
    assert!(most.table_bytes < u64::from(limits.super_bytes), "a table of {} bytes, a slot of {}", most.table_bytes, limits.super_bytes);
}
