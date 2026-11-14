//! The simulation of lint work over a generated workspace and its edits, with an oracle per
//! invariant and a planted bug per oracle (rule 43), and the fold's delta against its reference.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use blaze_ids::ContentId;
use blaze_keyed::kind::{Body, Key, Namespace, ToolId};
use blaze_keyed::machine::Machine;
use blaze_keyed::record::StableId;
use hegel::generators as gs;

use crate::finding::{Applicability, Arg, Edit, Finding, FindingId, Findings, Fix, ItemRange, LintId, Writer};
use crate::fold::{Fold, Narrow, Observed, Row, Subject as FoldSubject};
use crate::ladder::{self, Shown, Step};
use crate::pass::{Class, Over, PassDecl, PassId};
use crate::sched::{Command, Effect, Outcome, Scheduler, Subject, Want, WorkId};
use crate::view::{self, Level, View};
use crate::wait::{Reply, Wait};
use crate::{PLANT, Plant};

const GATE: LintId = LintId::of("blaze::sim_gate");
const ADVICE: LintId = LintId::of("blaze::sim_advice");
const FOLDED: LintId = LintId::of("blaze::sim_folded");
const DEEP: LintId = LintId::of("blaze::sim_deep");
const LINTS: [LintId; 4] = [GATE, ADVICE, FOLDED, DEEP];

const P_GATE: PassId = PassId::of("sim_gate");
const P_ADVICE: PassId = PassId::of("sim_advice");
const P_ROWS: PassId = PassId::of("sim_rows");
const P_FOLD: PassId = PassId::of("sim_fold");
const P_DEEP: PassId = PassId::of("sim_deep");

static PASSES: [PassDecl; 5] = [
    PassDecl { id: P_GATE, name: "sim_gate", version: 1, lints: &[GATE], over: Over::Body, class: Class::Cheap, expected_ns: 50_000 },
    PassDecl { id: P_ADVICE, name: "sim_advice", version: 1, lints: &[ADVICE], over: Over::Body, class: Class::Cheap, expected_ns: 50_000 },
    PassDecl { id: P_ROWS, name: "sim_rows", version: 1, lints: &[FOLDED], over: Over::Rows, class: Class::Cheap, expected_ns: 20_000 },
    PassDecl { id: P_FOLD, name: "sim_fold", version: 1, lints: &[FOLDED], over: Over::Fold { rows: P_ROWS }, class: Class::Cheap, expected_ns: 200_000 },
    PassDecl { id: P_DEEP, name: "sim_deep", version: 1, lints: &[DEEP], over: Over::Analysis, class: Class::Deferred, expected_ns: 50_000_000 },
];

const ITEMS: usize = 5;

fn item(i: usize) -> StableId {
    StableId(ContentId([i as u8 + 1; 32]))
}

fn tool() -> ToolId {
    ToolId { tool: ContentId([1; 32]), target: ContentId([2; 32]), namespace: Namespace::PUBLIC }
}

fn machine() -> Machine {
    Machine { cores: 4, fast_cores: 2, page_bytes: 16_384, memory_bytes: 16 << 30, qos: true, huge_pages: false }
}

fn level(n: u8) -> Level {
    match n % 3 {
        0 => Level::Allow,
        1 => Level::Warn,
        _ => Level::Deny,
    }
}

fn wait(n: u8) -> Wait {
    match n % 3 {
        0 => Wait::Answer,
        1 => Wait::Cheap,
        _ => Wait::All { budget_ns: 10_000_000_000 },
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Edit(u8),
    Level(u8, u8),
    Command(u8),
    Complete(u8),
    Tick(u8),
}

/// One want of the model's last command, owned.
#[derive(Clone, Debug)]
struct Wanted {
    pass: PassId,
    subject: Subject,
    key: ContentId,
    reads: Vec<StableId>,
}

struct Model {
    versions: [u32; ITEMS],
    /// The levels as edited; [`Model::last_view`] is what the last command was given.
    view: View,
    last_view: View,
    sched: Scheduler,
    /// Records filed, by key.
    store: BTreeSet<ContentId>,
    /// What each key was computed from: (pass, subject, the versions it read).
    states: BTreeMap<ContentId, (PassId, Subject, Vec<u32>)>,
    /// Computations per (pass, subject, versions read): the at-most-once oracle's count.
    computed: BTreeMap<(PassId, Subject, Vec<u32>), u32>,
    /// Runs issued and not cancelled, per key.
    issued: BTreeMap<ContentId, u32>,
    running: Vec<(WorkId, ContentId)>,
    moved: BTreeSet<StableId>,
    wants: Vec<Wanted>,
    snapshot: u64,
    now: u64,
}

impl Model {
    fn new() -> Model {
        let mut view = View::new();
        for lint in LINTS {
            view.set(lint, None, Level::Warn);
        }
        Model {
            versions: [0; ITEMS],
            last_view: view.clone(),
            view,
            sched: Scheduler::new(&PASSES, tool(), machine(), false),
            store: BTreeSet::new(),
            states: BTreeMap::new(),
            computed: BTreeMap::new(),
            issued: BTreeMap::new(),
            running: Vec::new(),
            moved: BTreeSet::new(),
            wants: Vec::new(),
            snapshot: 0,
            now: 0,
        }
    }

    fn body_key(&self, i: usize) -> Key<Body> {
        let mut bytes = [0u8; 5];
        bytes[0] = i as u8;
        bytes[1..].copy_from_slice(&self.versions[i].to_le_bytes());
        Key::from_id(ContentId::of("blaze-findings sim body", &bytes))
    }

    fn hashed(&self, pass: PassId, subject: Subject, versions: &[u32]) -> ContentId {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&pass.0.to_le_bytes());
        if let Some(item) = subject.item() {
            bytes.extend_from_slice(&item.0.0);
        }
        for version in versions {
            bytes.extend_from_slice(&version.to_le_bytes());
        }
        ContentId::of("blaze-findings sim key", &bytes)
    }

    /// Every (pass, subject) of the universe with its key, what it read, and its reads.
    fn universe(&self) -> Result<Vec<(Wanted, Vec<u32>)>, String> {
        let mut out = Vec::new();
        for decl in &PASSES {
            if !decl.lints.iter().any(|&lint| self.view.in_universe(lint)) {
                continue;
            }
            match decl.over {
                Over::Body | Over::Rows => {
                    for i in 0..ITEMS {
                        let key = self.sched.key_of(decl.id, self.body_key(i), &[], &self.view).map_err(|e| e.to_string())?;
                        let wanted = Wanted { pass: decl.id, subject: Subject::Item(item(i)), key, reads: vec![item(i)] };
                        out.push((wanted, vec![self.versions[i]]));
                    }
                }
                Over::Analysis => {
                    for i in 0..ITEMS {
                        let next = (i + 1) % ITEMS;
                        let read = vec![self.versions[i], self.versions[next]];
                        let subject = Subject::Item(item(i));
                        let key = self.hashed(decl.id, subject, &read);
                        let mut reads = vec![item(i), item(next)];
                        reads.sort();
                        out.push((Wanted { pass: decl.id, subject, key, reads }, read));
                    }
                }
                Over::Fold { .. } => {
                    let read = self.versions.to_vec();
                    let key = self.hashed(decl.id, Subject::Fold, &read);
                    let reads = (0..ITEMS).map(item).collect();
                    out.push((Wanted { pass: decl.id, subject: Subject::Fold, key, reads }, read));
                }
            }
        }
        Ok(out)
    }

    fn apply(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Edit(i) => {
                let i = usize::from(i) % ITEMS;
                self.versions[i] += 1;
                self.moved.insert(item(i));
                Ok(())
            }
            Op::Level(lint, n) => {
                self.view.set(LINTS[usize::from(lint) % LINTS.len()], None, level(n));
                Ok(())
            }
            Op::Command(n) => self.command(wait(n)),
            Op::Complete(k) => {
                if self.running.is_empty() {
                    return Ok(());
                }
                let (id, key) = self.running.remove(usize::from(k) % self.running.len());
                self.now += 1_000_000;
                self.store.insert(key);
                let state = self.states.get(&key).cloned().ok_or("a run of a key the model never wanted")?;
                let count = self.computed.entry(state.clone()).or_insert(0);
                *count += 1;
                if *count > 1 {
                    return Err(format!("{state:?} computed twice"));
                }
                let effects = self.sched.done(id, 1_000_000, Outcome::Computed, self.now).map_err(|e| e.to_string())?;
                self.feed(effects)
            }
            Op::Tick(n) => {
                self.now += u64::from(n) * 10_000_000;
                let effects = self.sched.advance(self.now);
                self.feed(effects)
            }
        }
    }

    fn command(&mut self, wait: Wait) -> Result<(), String> {
        let mut wants = Vec::new();
        for (wanted, read) in self.universe()? {
            self.states.insert(wanted.key, (wanted.pass, wanted.subject, read));
            if !self.store.contains(&wanted.key) {
                wants.push(wanted);
            }
        }
        let moved: Vec<StableId> = self.moved.iter().copied().collect();
        self.moved.clear();
        let before: Vec<(WorkId, Vec<StableId>, Vec<WorkId>, (PassId, Subject, Option<ContentId>))> = self
            .sched
            .in_flight()
            .map(|w| (w.id, w.reads.to_vec(), w.deps.to_vec(), (w.pass, w.subject, w.key)))
            .collect();
        let refs: Vec<Want<'_>> =
            wants.iter().map(|w| Want { pass: w.pass, subject: w.subject, key: Some(w.key), reads: &w.reads }).collect();
        self.snapshot += 1;
        self.last_view = self.view.clone();
        let command = Command { snapshot: self.snapshot, moved: &moved, wants: &refs, view: &self.view, wait, now_ns: self.now };
        let effects = self.sched.command(&command).map_err(|e| e.to_string())?;
        drop(refs);

        // Exactly what reads a moved item or is no longer wanted, and what depends on those.
        let identities: BTreeSet<_> = wants.iter().map(|w| (w.pass, w.subject, Some(w.key))).collect();
        let mut expected: BTreeSet<WorkId> = before
            .iter()
            .filter(|(_, reads, _, identity)| reads.iter().any(|r| moved.contains(r)) || !identities.contains(identity))
            .map(|(id, ..)| *id)
            .collect();
        loop {
            let grown: Vec<WorkId> = before
                .iter()
                .filter(|(id, _, deps, _)| !expected.contains(id) && deps.iter().any(|d| expected.contains(d)))
                .map(|(id, ..)| *id)
                .collect();
            if grown.is_empty() {
                break;
            }
            expected.extend(grown);
        }
        let cancelled: BTreeSet<WorkId> = self.sched.last_cancelled().iter().copied().collect();
        if cancelled != expected {
            return Err(format!("cancelled {cancelled:?}, not exactly the works that read a moved item {expected:?}"));
        }
        self.wants = wants;
        self.feed(effects)
    }

    fn feed(&mut self, effects: Vec<Effect>) -> Result<(), String> {
        let mut queue: VecDeque<Effect> = effects.into_iter().collect();
        while let Some(effect) = queue.pop_front() {
            match effect {
                Effect::Run { id, pass, key, class, .. } => {
                    let decl = PASSES.iter().find(|d| d.id == pass).ok_or("a run of an unknown pass")?;
                    if decl.lints.iter().any(|&lint| self.last_view.highest(lint) >= Level::Deny) && class != Class::Answer {
                        return Err(format!("a gate lint's pass {} ran in {class:?}", decl.name));
                    }
                    let key = key.ok_or("a run with no key")?;
                    let issued = self.issued.entry(key).or_insert(0);
                    *issued += 1;
                    if *issued > 1 {
                        return Err(format!("{} issued twice for one key", decl.name));
                    }
                    if self.store.contains(&key) {
                        queue.extend(self.sched.done(id, 0, Outcome::Hit, self.now).map_err(|e| e.to_string())?);
                    } else {
                        self.running.push((id, key));
                    }
                }
                Effect::Cancel { id } => {
                    let at = self.running.iter().position(|(r, _)| *r == id).ok_or("a cancel of a work not running")?;
                    let (_, key) = self.running.remove(at);
                    if let Some(issued) = self.issued.get_mut(&key) {
                        *issued -= 1;
                    }
                    queue.extend(self.sched.done(id, 0, Outcome::Cancelled, self.now).map_err(|e| e.to_string())?);
                }
                Effect::Reply(reply) => self.check(&reply)?,
            }
        }
        Ok(())
    }

    fn check(&self, reply: &Reply) -> Result<(), String> {
        // A reply cut short by a newer command may hold anything not yet done.
        if reply.superseded || reply.snapshot != self.snapshot {
            return Ok(());
        }
        for pending in &reply.pending {
            if self.last_view.highest(pending.lint) >= Level::Deny {
                return Err(format!("a gate lint deferred: {pending:?}"));
            }
        }
        for want in &self.wants {
            let decl = PASSES.iter().find(|d| d.id == want.pass).ok_or("an unknown pass")?;
            let stored = self.store.contains(&want.key);
            let pending =
                reply.pending.iter().any(|p| decl.lints.contains(&p.lint) && p.item == want.subject.item());
            if stored == pending {
                return Err(format!("{} of {:?}: stored {stored}, pending {pending}", decl.name, want.subject));
            }
        }
        Ok(())
    }
}

fn run(ops: &[Op], plant: Plant) -> Result<(), String> {
    PLANT.set(plant);
    let result = (|| {
        let mut model = Model::new();
        for &op in ops {
            model.apply(op)?;
        }
        // Drain: one last command waiting for everything, then finish every run.
        model.apply(Op::Command(2))?;
        for _ in 0..200 {
            if model.running.is_empty() {
                break;
            }
            model.apply(Op::Complete(0))?;
        }
        Ok(())
    })();
    PLANT.set(Plant::None);
    result
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let a = tc.draw(gs::integers::<u8>().max_value(7));
    let b = tc.draw(gs::integers::<u8>().max_value(5));
    match tc.draw(gs::integers::<u8>().max_value(9)) {
        0 | 1 => Op::Edit(a),
        2 => Op::Level(a, b),
        3 | 4 => Op::Command(b),
        5 => Op::Tick(a * 8),
        _ => Op::Complete(a),
    }
}

/// The invariants, each checked as the simulation runs: a finding is computed at most once per
/// (pass, item, what it read) and issued at most once per key; a gate lint's pass runs in the
/// answer class and is never pending; a newer command cancels exactly the works that read a
/// moved item (or that it no longer wants, and their dependents); pending is exactly what was
/// not computed; changing levels recomputes nothing (the first, across level changes).
#[hegel::test(test_cases = 300)]
fn lint_work_is_computed_once_and_pending_is_exact(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(40));
    if let Err(error) = run(&ops, Plant::None) {
        panic!("{error}\nops: {ops:?}");
    }
}

fn caught(ops: &[Op], plant: Plant, needle: &str) {
    run(ops, Plant::None).unwrap_or_else(|e| panic!("the scenario fails unplanted: {e}"));
    match run(ops, plant) {
        Err(error) if error.contains(needle) => {}
        other => panic!("{plant:?} was not caught as {needle:?}: {other:?}"),
    }
}

#[test]
fn the_oracle_catches_reissued_work() {
    let ops = [Op::Command(1), Op::Command(1)];
    caught(&ops, Plant::ReissuesKeptWork, "issued twice");
}

#[test]
fn the_oracle_catches_a_deferred_gate() {
    let ops = [Op::Level(0, 2), Op::Command(0)];
    caught(&ops, Plant::DefersGate, "gate lint");
}

#[test]
fn the_oracle_catches_pending_without_deferred_work() {
    caught(&[Op::Command(0)], Plant::PendingOmitsDeferred, "pending false");
}

#[test]
fn the_oracle_catches_cancelling_all_deferred_work() {
    let ops = [Op::Command(0), Op::Edit(0), Op::Command(0)];
    caught(&ops, Plant::CancelsAll, "cancelled");
}

#[test]
fn the_oracle_catches_keeping_moved_deferred_work() {
    let ops = [Op::Command(0), Op::Edit(0), Op::Command(0)];
    caught(&ops, Plant::KeepsMoved, "cancelled");
}

#[test]
fn the_oracle_catches_levels_in_a_key() {
    let mut ops = vec![Op::Command(2)];
    ops.extend([Op::Complete(0); 30]);
    ops.extend([Op::Level(0, 2), Op::Command(2)]);
    ops.extend([Op::Complete(0); 30]);
    caught(&ops, Plant::KeyCoversLevels, "computed twice");
}

// The fold.

fn subject(s: u8) -> FoldSubject {
    ContentId::of("blaze-findings sim subject", &[s])
}

#[derive(Clone, Copy, Debug)]
enum FoldOp {
    /// Contributor `c` now says these (subject, value) rows, by small numbers.
    Set { c: u8, rows: [(u8, u8); 3], len: u8 },
}

fn observed(v: u8) -> Observed {
    match v % 5 {
        0 => Observed::unknown(),
        1 => Observed::declared(true, 32),
        n => Observed::value(i128::from(n) * 100 - 250),
    }
}

#[hegel::composite]
fn fold_op(tc: &hegel::TestCase) -> FoldOp {
    let mut rows = [(0u8, 0u8); 3];
    for row in &mut rows {
        *row = (tc.draw(gs::integers::<u8>().max_value(7)), tc.draw(gs::integers::<u8>().max_value(9)));
    }
    FoldOp::Set { c: tc.draw(gs::integers::<u8>().max_value(3)), rows, len: tc.draw(gs::integers::<u8>().max_value(3)) }
}

fn fold_run(ops: &[FoldOp], plant: Plant) -> Result<(), String> {
    PLANT.set(plant);
    let result = (|| {
        let mut fold: Fold<Narrow> = Fold::new(4);
        let mut said: BTreeMap<u8, Vec<(FoldSubject, Observed)>> = BTreeMap::new();
        for &FoldOp::Set { c, rows, len } in ops {
            let new: Vec<(FoldSubject, Observed)> =
                rows[..usize::from(len)].iter().map(|&(s, v)| (subject(s), observed(v))).collect();
            let old = said.insert(c, new.clone()).unwrap_or_default();
            fold.apply(StableId(ContentId([c; 32])), &old, &new);
            let mut all = Vec::new();
            for (&c, rows) in &said {
                for &(subject, value) in rows {
                    all.push(Row { subject, contributor: StableId(ContentId([c; 32])), value });
                }
            }
            let scratch: Fold<Narrow> = Fold::from_scratch(4, &all);
            if !fold.same_as(&scratch) || fold.root_digest() != scratch.root_digest() {
                return Err("the fold after a delta is not the fold from scratch".to_owned());
            }
        }
        Ok(())
    })();
    PLANT.set(Plant::None);
    result
}

/// The invariant: a fold after any sequence of deltas equals the fold of the same rows from
/// scratch, leaf for leaf and verdict for verdict.
#[hegel::test(test_cases = 300)]
fn a_fold_after_a_delta_is_the_fold_from_scratch(tc: hegel::TestCase) {
    let ops: Vec<FoldOp> = tc.draw(gs::vecs(fold_op()).max_size(20));
    if let Err(error) = fold_run(&ops, Plant::None) {
        panic!("{error}\nops: {ops:?}");
    }
}

#[test]
fn the_oracle_catches_a_fold_that_keeps_stale_rows() {
    let ops = [
        FoldOp::Set { c: 0, rows: [(1, 2), (0, 0), (0, 0)], len: 1 },
        FoldOp::Set { c: 0, rows: [(1, 3), (0, 0), (0, 0)], len: 1 },
    ];
    fold_run(&ops, Plant::None).unwrap();
    assert!(fold_run(&ops, Plant::FoldKeepsStaleRows).is_err());
}

#[test]
fn narrow_decides_unsigned_nonzero_and_bits() {
    let rows = [
        Row { subject: subject(0), contributor: StableId(ContentId([9; 32])), value: Observed::declared(true, 64) },
        Row { subject: subject(0), contributor: StableId(ContentId([1; 32])), value: Observed::value(3) },
        Row { subject: subject(0), contributor: StableId(ContentId([2; 32])), value: Observed::value(200) },
    ];
    let fold: Fold<Narrow> = Fold::from_scratch(1, &rows);
    let verdict = fold.verdict(&subject(0)).expect("a verdict");
    assert!(verdict.unsigned && verdict.nonzero);
    assert_eq!(verdict.bits, Some(8));
    let mut unknown = rows.to_vec();
    unknown.push(Row { subject: subject(0), contributor: StableId(ContentId([3; 32])), value: Observed::unknown() });
    assert_eq!(Fold::<Narrow>::from_scratch(1, &unknown).verdict(&subject(0)), None);
}

// The record.

/// The invariant: a payload reads back as written, and a finding's id does not move when its
/// range does.
#[test]
fn findings_round_trip_and_ids_ignore_position() {
    let it = StableId(ContentId([7; 32]));
    let edits = [Edit { item: it, at: ItemRange { start: 0, end: 0 }, text: b"#[blaze::pure]\n" }];
    let args = [Arg::Name(b"len"), Arg::Range { lo: 0, hi: 9 }];
    let at = |start| ItemRange { start, end: start + 3 };
    let finding = |start| Finding {
        lint: ADVICE,
        item: it,
        at: at(start),
        message: 0,
        args: &args,
        fix: Some(Fix { applicability: Applicability::MachineApplicable, edits: &edits }),
    };
    let ids = |start| {
        let mut writer = Writer::new();
        writer.push(&finding(start));
        writer.push(&Finding { fix: None, ..finding(start + 10) });
        let bytes = writer.finish();
        let findings = Findings::open(&bytes).expect("a payload that opens");
        let first = findings.get(0).expect("a row");
        assert_eq!(first.lint(), ADVICE);
        assert_eq!(first.at(), at(start));
        assert!(first.args().eq(args.iter().copied()));
        let (applicability, mut fix) = first.fix().expect("a fix");
        assert_eq!(applicability, Applicability::MachineApplicable);
        assert_eq!(fix.next(), Some(edits[0]));
        assert!(findings.get(1).expect("a row").fix().is_none());
        let mut ids: Vec<FindingId> = Vec::new();
        findings.ids(&mut ids);
        ids
    };
    let (here, there) = (ids(4), ids(40));
    assert_eq!(here, there);
    assert_ne!(here[0], here[1]);
}

#[test]
fn a_truncated_payload_is_refused() {
    let mut writer = Writer::new();
    writer.push(&Finding { lint: GATE, item: item(0), at: ItemRange { start: 1, end: 2 }, message: 0, args: &[], fix: None });
    let bytes = writer.finish();
    assert!(Findings::open(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn diff_is_new_and_fixed() {
    let id = |n: u8| FindingId(ContentId([n; 32]));
    let delta = view::diff(&[id(1), id(2), id(2)], &[id(2), id(3)]);
    assert_eq!(delta.new, vec![id(3)]);
    assert_eq!(delta.fixed, vec![id(1), id(2)]);
    assert_eq!(delta.unchanged, 1);
}

// The ladder.

#[test]
fn the_report_orders_crates_by_work_by_hand() {
    let shown = [
        Shown { krate: "b", lint: "blaze::could_be_pure", fixable: true },
        Shown { krate: "a", lint: "blaze::could_be_narrower", fixable: false },
        Shown { krate: "a", lint: "blaze::could_be_narrower", fixable: false },
        Shown { krate: "c", lint: "blaze::could_be_pure", fixable: true },
    ];
    let report = ladder::report(Step::Suggest, &shown);
    assert_eq!(report.findings(), 4);
    assert_eq!(report.fixable(), 2);
    let order: Vec<&str> = report.crates.iter().map(|c| c.krate).collect();
    assert_eq!(order, ["b", "c", "a"]);
    assert_eq!(Step::named("edition"), Some(Step::Edition));
    assert_eq!(Step::Rust.next(), Some(Step::Suggest));
    assert_eq!(ladder::baseline(&shown)[0], ("blaze::could_be_narrower", "a", 2));
}

#[test]
fn every_pass_is_named_by_its_id_and_within_its_class() {
    for decl in &PASSES {
        assert!(!decl.misnamed(), "{}", decl.name);
        assert_eq!(decl.misdeclared(), None, "{}", decl.name);
    }
}
