//! The deterministic simulation: a model program (functions with typed sites and roles, a call
//! graph whose edges are inlined or not, tests with an entry and the facets of behaviour they
//! observe), mutation sessions run against it by a model host on a virtual clock with shuffled
//! completions and jittered latencies, and the oracles:
//!
//! - a mutant is killed exactly when some test that executes the function observes what the
//!   mutation changes, and by a test that does; it survives with exactly the tests that execute
//!   the function, none of which observes it;
//! - nothing arid is made; every arid skip is of an arid site; with an ample budget and no caps
//!   every mutation of every behaviour site is decided;
//! - only tests that execute the function run;
//! - a verdict decided statically is true of the model (equivalent: the mutation changes nothing;
//!   not reached: no test executes it);
//! - no more mutants are in flight than slots, and nothing is started at or after the deadline;
//! - two hosts with different completion orders give the same verdicts;
//! - after an edit, a mutant whose body and reaching tests are unchanged is not made again.
//!
//! What a mutation changes is the model's own: a hash of the body, the site and the mutation
//! picks a facet of behaviour (or none: an equivalent mutant no rule finds), except where the
//! model's restatement of the equivalence rules says it changes nothing.

use std::collections::{BTreeMap, BTreeSet};

use blaze_ids::ContentId;
use blaze_keyed::machine::Machine;
use blaze_program::key::{BodyKey, ItemId};
use blaze_program::record::TestKey;

use crate::Plant;
use crate::command::{Request, mutants, summarize};
use crate::operator::{Mutation, Operator, mutations};
use crate::report::{LineTable, render};
use crate::run::{Completion, Effect, Mutant, Outcome, Session, TestOutcome, Ticket};
use crate::select::{Budget, Change, Changed, Config, MutantRef, PATCH_NS, Scope, Universe, plan};
use crate::site::{
    Absorb, ArithOp, CmpOp, Effectful, FnFacts, ItemPos, Mark, MirLoc, ProgramRef, Role, Site, SiteKind, TestInfo, TyClass,
};
use crate::verdict::{KilledBy, Verdict, VerdictKey, VerdictRecord};

const FACETS: u64 = 8;
const STEP_LIMIT: u32 = 200_000;
const MS: u64 = 1_000_000;
/// A budget no session reaches.
const AMPLE: u64 = 1 << 50;

pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

struct Fun {
    facts: FnFacts,
    version: u32,
    /// Callees, and whether the edge is inlined at the tier under test.
    calls: Vec<(usize, bool)>,
}

struct Test {
    entry: usize,
    observes: BTreeSet<u64>,
    version: u32,
    program: u8,
    recorded_ns: u64,
}

pub(super) struct World {
    funs: Vec<Fun>,
    tests: Vec<Test>,
    facts: Vec<FnFacts>,
    infos: Vec<TestInfo>,
}

const ARID_ROLES: [Role; 10] = [
    Role::Logging,
    Role::Tracing,
    Role::DebugFormat,
    Role::Message,
    Role::Metrics,
    Role::PerfHint,
    Role::CfgTest,
    Role::Assertion,
    Role::Unreachable,
    Role::Generated,
];
const CMPS: [CmpOp; 6] = [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne];
const ARITHS: [ArithOp; 10] = [
    ArithOp::Add,
    ArithOp::Sub,
    ArithOp::Mul,
    ArithOp::Div,
    ArithOp::Rem,
    ArithOp::BitAnd,
    ArithOp::BitOr,
    ArithOp::BitXor,
    ArithOp::Shl,
    ArithOp::Shr,
];

fn item_of(index: usize) -> ItemId {
    ItemId(ContentId::of("blaze mutants world item", &(index as u64).to_le_bytes()))
}

fn body_of(index: usize, version: u32) -> BodyKey {
    let mut bytes = (index as u64).to_le_bytes().to_vec();
    bytes.extend_from_slice(&version.to_le_bytes());
    BodyKey(ContentId::of("blaze mutants world body", &bytes))
}

fn site(rng: &mut Rng, index: u32) -> Site {
    let kind = match rng.below(9) {
        0 => SiteKind::Return {
            default: Some(b"0".to_vec()),
            args: if rng.chance(1, 2) { vec![(0, b"a".to_vec())] } else { Vec::new() },
            fields: if rng.chance(1, 2) { vec![(0, b"len".to_vec())] } else { Vec::new() },
        },
        1 => SiteKind::Branch,
        2 => SiteKind::Compare { op: CMPS[rng.below(6) as usize], ty: TyClass::Int { signed: true, bits: 32 } },
        3 => SiteKind::Arith { op: ARITHS[rng.below(10) as usize], ty: TyClass::Int { signed: false, bits: 64 } },
        4 => SiteKind::Const {
            value: rng.below(5) as i128,
            ty: TyClass::Int { signed: false, bits: 8 },
            absorb: if rng.chance(1, 2) { Absorb::Clamp { lo: 0, hi: 2 } } else { Absorb::None },
        },
        5 => SiteKind::Effect(Effectful::Call { pure: rng.chance(1, 2), result_used: rng.chance(1, 2) }),
        6 => SiteKind::Effect(Effectful::Write),
        7 => SiteKind::EarlyReturn,
        _ => SiteKind::Construct { variant: 0, alternatives: vec![(0, b"A".to_vec()), (1, b"B".to_vec())] },
    };
    let role = if rng.chance(1, 6) { ARID_ROLES[rng.below(10) as usize] } else { Role::Behaviour };
    let text = b"left op right".to_vec();
    Site {
        kind,
        at: MirLoc { block: index, statement: 0 },
        pos: ItemPos { line: 1 + rng.below(4) as u32, col: 4 },
        role,
        read: rng.chance(5, 6),
        token: (5, 7),
        text,
    }
}

impl World {
    pub(super) fn new(rng: &mut Rng) -> World {
        let count = 2 + rng.below(5) as usize;
        let mut funs = Vec::new();
        for index in 0..count {
            let sites = (0..1 + rng.below(6) as u32).map(|at| site(rng, at)).collect();
            let mut marks = Vec::new();
            if rng.chance(1, 8) {
                marks.push(Mark {
                    operator: Operator::ALL[rng.below(13) as usize],
                    line: Some(1 + rng.below(4) as u32),
                    reason: b"the model says so".to_vec(),
                });
            }
            let mut calls = Vec::new();
            for callee in index + 1..count {
                if rng.chance(1, 3) {
                    calls.push((callee, rng.chance(1, 3)));
                }
            }
            funs.push(Fun {
                facts: FnFacts {
                    item: item_of(index),
                    body: body_of(index, 0),
                    path: format!("world::f{index}").into_bytes(),
                    role: if rng.chance(1, 10) { Role::Generated } else { Role::Behaviour },
                    sites,
                    inliners: Vec::new(),
                    marks,
                    make_ns: MS + rng.below(4 * MS),
                },
                version: 0,
                calls,
            });
        }
        let mut tests = Vec::new();
        for _ in 0..1 + rng.below(4) {
            tests.push(Test {
                entry: rng.below(count as u64) as usize,
                observes: (0..FACETS).filter(|_| rng.chance(1, 2)).collect(),
                version: 0,
                program: rng.below(2) as u8,
                recorded_ns: MS + rng.below(19 * MS),
            });
        }
        let mut world = World { funs, tests, facts: Vec::new(), infos: Vec::new() };
        world.refresh();
        world
    }

    /// An edit: one function's body, and maybe one test.
    fn edit(&mut self, rng: &mut Rng) -> usize {
        let k = rng.below(self.funs.len() as u64) as usize;
        let fun = &mut self.funs[k];
        fun.version += 1;
        fun.facts.body = body_of(k, fun.version);
        if rng.chance(1, 2) {
            let t = rng.below(self.tests.len() as u64) as usize;
            let test = &mut self.tests[t];
            test.version += 1;
            test.observes = (0..FACETS).filter(|_| rng.chance(1, 2)).collect();
        }
        self.refresh();
        k
    }

    fn executed(&self, test: &Test) -> BTreeSet<usize> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![test.entry];
        while let Some(at) = stack.pop() {
            if seen.insert(at) {
                stack.extend(self.funs[at].calls.iter().map(|(callee, _)| *callee));
            }
        }
        seen
    }

    fn inliners(&self, of: usize) -> Vec<usize> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![of];
        while let Some(at) = stack.pop() {
            for (caller, fun) in self.funs.iter().enumerate() {
                if fun.calls.contains(&(at, true)) && seen.insert(caller) {
                    stack.push(caller);
                }
            }
        }
        seen.into_iter().collect()
    }

    /// The facts and test infos the planner reads, made again after an edit. A test's key
    /// covers the bodies it executes, as `blaze_program::reach::test_key` covers the instance
    /// keys it reaches.
    fn refresh(&mut self) {
        for index in 0..self.funs.len() {
            let mut inliners: Vec<ItemId> = self.inliners(index).into_iter().map(item_of).collect();
            inliners.sort();
            self.funs[index].facts.inliners = inliners;
        }
        self.facts = self.funs.iter().map(|fun| fun.facts.clone()).collect();
        let mut infos = Vec::new();
        for (index, test) in self.tests.iter().enumerate() {
            let executed = self.executed(test);
            let mut reach = BTreeSet::new();
            reach.insert(item_of(test.entry));
            for at in &executed {
                for (callee, inlined) in &self.funs[*at].calls {
                    if !inlined {
                        reach.insert(item_of(*callee));
                    }
                }
            }
            let mut bytes = (index as u64).to_le_bytes().to_vec();
            bytes.extend_from_slice(&test.version.to_le_bytes());
            for at in &executed {
                bytes.extend_from_slice(&self.funs[*at].facts.body.0.0);
            }
            infos.push(TestInfo {
                key: TestKey(ContentId::of("blaze mutants world test", &bytes)),
                name: format!("world::test{index}").into_bytes(),
                program: ProgramRef(ContentId::of("blaze mutants world program", &[test.program])),
                recorded_ns: test.recorded_ns,
                reach: reach.into_iter().collect(),
            });
        }
        self.infos = infos;
    }

    pub(super) fn universe(&self) -> Universe<'_> {
        Universe { functions: &self.facts, tests: &self.infos }
    }

    fn fun_of(&self, item: ItemId) -> Result<usize, String> {
        self.funs.iter().position(|fun| fun.facts.item == item).ok_or_else(|| format!("no function {}", item.0))
    }

    fn test_of(&self, key: TestKey) -> Result<usize, String> {
        self.infos.iter().position(|info| info.key == key).ok_or_else(|| format!("no current test {}", key.0))
    }

    /// The keys of the tests that execute function `fun`, sorted.
    fn executing(&self, fun: usize) -> Vec<TestKey> {
        let mut keys: Vec<TestKey> = (0..self.tests.len())
            .filter(|t| self.executed(&self.tests[*t]).contains(&fun))
            .map(|t| self.infos[t].key)
            .collect();
        keys.sort();
        keys
    }

    fn arid(&self, fun: usize, site: u32) -> bool {
        let fun = &self.funs[fun];
        fun.facts.role != Role::Behaviour || fun.facts.sites[site as usize].role != Role::Behaviour
    }

    /// The model's restatement of what changes nothing.
    fn changes_nothing(&self, fun: usize, site: u32, mutation: &Mutation) -> bool {
        let facts = &self.funs[fun].facts;
        let site = &facts.sites[site as usize];
        let operator = mutation.operator(site);
        if facts.marks.iter().any(|mark| mark.operator == operator && mark.line.is_none_or(|line| line == site.pos.line)) {
            return true;
        }
        match (&site.kind, mutation) {
            (SiteKind::Effect(Effectful::Call { pure, result_used }), Mutation::DeleteEffect) => *pure && !*result_used,
            (SiteKind::Effect(Effectful::Call { .. }), _) => false,
            (SiteKind::Return { .. } | SiteKind::Branch | SiteKind::EarlyReturn, _) => false,
            (SiteKind::Const { value, absorb: Absorb::Clamp { lo, hi }, .. }, Mutation::Const { to })
                if (*value).clamp(*lo, *hi) == (*to).clamp(*lo, *hi) =>
            {
                true
            }
            _ => !site.read,
        }
    }

    /// The facet a mutation changes, and whether the change hangs or crashes; `None` when it
    /// changes nothing.
    fn truth(&self, fun: usize, site: u32, mutation: &Mutation) -> Option<(u64, bool, bool)> {
        if self.changes_nothing(fun, site, mutation) {
            return None;
        }
        let mut bytes = self.funs[fun].facts.body.0.0.to_vec();
        bytes.extend_from_slice(&site.to_le_bytes());
        let mut encoded = Vec::new();
        mutation.encode(&mut encoded);
        bytes.extend_from_slice(&encoded);
        let hash = ContentId::of("blaze mutants world truth", &bytes).0;
        if hash[0] % 5 == 0 {
            return None;
        }
        Some((u64::from(hash[1]) % FACETS, hash[2] % 23 == 0, hash[3] % 17 == 0))
    }

    /// How test `t` ends with the mutant in place, and how long it takes (`None`: forever).
    fn run(&self, t: usize, at: &MutantRef) -> Result<(TestOutcome, Option<u64>), String> {
        let fun = self.fun_of(at.item)?;
        let test = &self.tests[t];
        let pass = (TestOutcome::Passed, Some(test.recorded_ns));
        if !self.executed(test).contains(&fun) {
            return Ok(pass);
        }
        Ok(match self.truth(fun, at.site, &at.mutation) {
            Some((facet, hang, crash)) if test.observes.contains(&facet) => match (hang, crash) {
                (true, _) => (TestOutcome::TimedOut, None),
                (false, true) => (TestOutcome::Crashed, Some(test.recorded_ns / 3)),
                (false, false) => (TestOutcome::Failed, Some(test.recorded_ns / 2)),
            },
            _ => pass,
        })
    }
}

/// A mutant, with its function's body and the keys of the tests executing it: equal tuples
/// must share a verdict without a second run.
type Identity = (ItemId, BodyKey, u32, Mutation, Vec<TestKey>);

struct Host<'w> {
    world: &'w World,
    rng: Rng,
    now: u64,
    seq: u64,
    events: BTreeMap<(u64, u64, u64), (Ticket, Completion)>,
    in_flight: u32,
    slots: u32,
    deadline: u64,
    mutants: BTreeMap<Mutant, MutantRef>,
    forbid: Option<&'w BTreeSet<Identity>>,
}

impl Host<'_> {
    fn at(&mut self, delay: u64, ticket: Ticket, completion: Completion) {
        let tie = self.rng.next();
        self.seq += 1;
        self.events.insert((self.now.saturating_add(delay), tie, self.seq), (ticket, completion));
    }

    fn handle(&mut self, effect: Effect, store: &mut BTreeMap<VerdictKey, Vec<u8>>) -> Result<(), String> {
        match effect {
            Effect::Lookup { ticket, keys } => {
                let values = keys.iter().map(|key| store.get(key).cloned()).collect();
                self.at(100_000, ticket, Completion::Found(values));
            }
            Effect::Make { ticket, mutant, at, inliners, deadline } => {
                let fun = self.world.fun_of(at.item)?;
                if self.world.arid(fun, at.site) {
                    return Err(format!("made a mutant of arid site {} of {}", at.site, at.item.0));
                }
                if self.now >= self.deadline || deadline != self.deadline {
                    return Err(format!("made a mutant at {} with the deadline at {}", self.now, self.deadline));
                }
                if inliners != self.world.funs[fun].facts.inliners {
                    return Err(format!("the mutant of {} remakes the wrong inliners", at.item.0));
                }
                if let Some(forbid) = self.forbid {
                    let identity = (at.item, at.body, at.site, at.mutation, self.world.executing(fun));
                    if forbid.contains(&identity) {
                        return Err(format!("mutated site {} of unchanged {} again", at.site, at.item.0));
                    }
                }
                self.in_flight += 1;
                if self.in_flight > self.slots {
                    return Err(format!("{} mutants in flight on {} slots", self.in_flight, self.slots));
                }
                self.mutants.insert(mutant, at);
                let mut cost = self.world.funs[fun].facts.make_ns;
                for inliner in &inliners {
                    cost += self.world.funs[self.world.fun_of(*inliner)?].facts.make_ns;
                }
                cost += self.rng.below(50_000);
                self.finish_by(cost, ticket, Completion::Made);
            }
            Effect::Patch { ticket, mutant, deadline, .. } => {
                if !self.mutants.contains_key(&mutant) || deadline != self.deadline {
                    return Err(format!("a patch for mutant {} that was not made", mutant.0));
                }
                let cost = PATCH_NS + self.rng.below(20_000);
                self.finish_by(cost, ticket, Completion::Patched);
            }
            Effect::Run { ticket, mutant, test, program, timeout_ns, deadline } => {
                let at = *self.mutants.get(&mutant).ok_or_else(|| format!("a run for unmade mutant {}", mutant.0))?;
                let fun = self.world.fun_of(at.item)?;
                let t = self.world.test_of(test)?;
                if !self.world.executed(&self.world.tests[t]).contains(&fun) {
                    return Err(format!("ran test {} that does not reach {}", test.0, at.item.0));
                }
                if program != self.world.infos[t].program {
                    return Err(format!("ran test {} in the wrong program", test.0));
                }
                if self.now.saturating_add(timeout_ns) > deadline {
                    return Err(format!("a run of {} whose timeout passes the deadline", test.0));
                }
                let (outcome, took) = self.world.run(t, &at)?;
                match took {
                    Some(took) if took <= timeout_ns => self.at(took, ticket, Completion::Ran(outcome)),
                    _ => self.at(timeout_ns, ticket, Completion::Ran(TestOutcome::TimedOut)),
                }
            }
            Effect::Put { key, bytes } => {
                VerdictRecord::decode(&bytes).map_err(|error| format!("a put of bytes that do not decode: {error}"))?;
                if let Some(old) = store.insert(key, bytes.clone())
                    && old != bytes
                {
                    return Err(format!("verdict {key:?} put twice with different bytes"));
                }
            }
            Effect::Drop { mutant } => {
                if self.mutants.remove(&mutant).is_none() {
                    return Err(format!("a drop of mutant {} not in flight", mutant.0));
                }
                self.in_flight -= 1;
            }
        }
        Ok(())
    }

    /// Complete with `completion` after `cost`, or with expired at the deadline.
    fn finish_by(&mut self, cost: u64, ticket: Ticket, completion: Completion) {
        match self.now.saturating_add(cost) > self.deadline {
            true => self.at(self.deadline - self.now, ticket, Completion::Expired),
            false => self.at(cost, ticket, completion),
        }
    }
}

/// Run `session` to the end against `world`, holding the host's oracles.
fn drive(
    world: &World,
    mut session: Session,
    store: &mut BTreeMap<VerdictKey, Vec<u8>>,
    seed: u64,
    slots: u32,
    start: u64,
    wall: u64,
    forbid: Option<&BTreeSet<Identity>>,
) -> Result<Outcome, String> {
    let mut host = Host {
        world,
        rng: Rng::new(seed),
        now: start,
        seq: 0,
        events: BTreeMap::new(),
        in_flight: 0,
        slots,
        deadline: start + wall,
        mutants: BTreeMap::new(),
        forbid,
    };
    for _ in 0..STEP_LIMIT {
        while let Some(effect) = session.take() {
            host.handle(effect, store)?;
        }
        if session.is_done() {
            if host.in_flight != 0 {
                return Err(format!("done with {} mutants not dropped", host.in_flight));
            }
            return session.finish().map_err(|error| error.to_string());
        }
        match host.events.pop_first() {
            Some(((when, _, _), (ticket, completion))) => {
                host.now = when;
                session.complete(when, ticket, completion).map_err(|error| error.to_string())?;
            }
            None => {
                // Nothing in flight: only the deadline can move the session.
                let Some(when) = session.next_deadline() else { return Err("not done and no deadline".into()) };
                if when < host.now {
                    return Err(format!("stalled at {} past the deadline {when}", host.now));
                }
                host.now = when;
                session.advance(host.now);
            }
        }
    }
    Err(format!("no end after {STEP_LIMIT} steps"))
}

/// The outcome's verdicts hold of the model.
fn check(world: &World, outcome: &Outcome, ample: bool) -> Result<(), String> {
    if let Some((at, error)) = outcome.errors.first() {
        return Err(format!("mutant of {} failed: {error:?}", at.item.0));
    }
    for (at, verdict, _) in &outcome.verdicts {
        let fun = world.fun_of(at.item)?;
        if at.body != world.funs[fun].facts.body {
            return Err(format!("a verdict of an old body of {}", at.item.0));
        }
        let executing = world.executing(fun);
        let name = || format!("site {} of {} by {:?}", at.site, at.item.0, at.mutation);
        match verdict {
            Verdict::Killed { test, how } => {
                if !executing.contains(test) {
                    return Err(format!("{} killed by test {}, which does not execute it now", name(), test.0));
                }
                let (outcome, _) = world.run(world.test_of(*test)?, at)?;
                let agrees = matches!(
                    (how, outcome),
                    (KilledBy::Failed, TestOutcome::Failed)
                        | (KilledBy::Crashed, TestOutcome::Crashed)
                        | (KilledBy::Timeout, TestOutcome::TimedOut)
                );
                if !agrees {
                    return Err(format!("{} killed {how:?} by {}, which ends {outcome:?}", name(), test.0));
                }
            }
            Verdict::Survived { tests } => {
                if *tests != executing {
                    return Err(format!("{} survived {} tests, {} execute it", name(), tests.len(), executing.len()));
                }
                for test in tests {
                    let (outcome, _) = world.run(world.test_of(*test)?, at)?;
                    if outcome != TestOutcome::Passed {
                        return Err(format!("{} survived test {}, which kills it", name(), test.0));
                    }
                }
            }
            Verdict::NotReached => {
                if !executing.is_empty() {
                    return Err(format!("{} not reached, but {} tests execute it", name(), executing.len()));
                }
            }
            Verdict::Equivalent(rule) => {
                if !world.changes_nothing(fun, at.site, &at.mutation) {
                    return Err(format!("{} ruled {rule:?}, but it changes behaviour", name()));
                }
            }
            Verdict::NotRun(why) => {
                if ample {
                    return Err(format!("{} not run ({why:?}) with an ample budget", name()));
                }
            }
        }
    }
    Ok(())
}

/// With no caps and an ample budget, every mutation of every behaviour site is decided (a
/// function no test reaches by one not-reached verdict).
fn complete(world: &World, outcome: &Outcome) -> Result<(), String> {
    let seen: BTreeSet<(ItemId, u32, Mutation)> =
        outcome.verdicts.iter().map(|(at, _, _)| (at.item, at.site, at.mutation)).collect();
    let mut admitted = Vec::new();
    for (index, fun) in world.funs.iter().enumerate() {
        let reached = !world.executing(index).is_empty();
        let mut any = false;
        let mut real = false;
        for (site, facts) in fun.facts.sites.iter().enumerate() {
            let site = site as u32;
            if world.arid(index, site) {
                continue;
            }
            mutations(facts, &mut admitted);
            for mutation in &admitted {
                let present = seen.contains(&(fun.facts.item, site, *mutation));
                any |= present;
                let nothing = world.changes_nothing(index, site, mutation);
                real |= !nothing;
                if (reached || nothing) && !present {
                    return Err(format!("site {site} of {} by {mutation:?} was not decided", fun.facts.item.0));
                }
            }
        }
        if !reached && real && !any {
            return Err(format!("{}, reached by no test, has no verdict", fun.facts.item.0));
        }
    }
    Ok(())
}

fn identities(world: &World, outcome: &Outcome) -> Result<BTreeSet<Identity>, String> {
    let mut out = BTreeSet::new();
    for (at, verdict, _) in &outcome.verdicts {
        if verdict.filed() {
            let fun = world.fun_of(at.item)?;
            out.insert((at.item, at.body, at.site, at.mutation, world.executing(fun)));
        }
    }
    Ok(out)
}

fn machine(slots: u32) -> Machine {
    Machine {
        cores: slots,
        fast_cores: slots,
        page_bytes: 16_384,
        memory_bytes: 1 << 34,
        qos: false,
        huge_pages: false,
    }
}

struct Lines;

impl LineTable for Lines {
    fn locate(&self, _item: ItemId, pos: ItemPos) -> Option<(&[u8], u32, u32)> {
        Some((&b"src/world.rs"[..], 10 + pos.line, 1 + pos.col))
    }
}

/// Every session of one seed: a full one twice under different hosts, one after an edit over
/// the first's store, one over the edited function's changed line, and one with a tight budget.
pub(super) fn run_seed(seed: u64, plant: Plant) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let mut world = World::new(&mut rng);
    let slots = 1 + rng.below(4) as u32;
    let full = Config { per_function: u32::MAX, per_line: u32::MAX, budget: Budget { wall_ns: AMPLE, slots }, patch_ns: PATCH_NS };
    let items: Vec<ItemId> = world.facts.iter().map(|facts| facts.item).collect();

    let plan_of = |world: &World| plan(&Scope::Items(&items), &world.universe(), &full, plant).map_err(|error| error.to_string());
    let first = plan_of(&world)?;
    for skipped in &first.arid {
        let fun = world.fun_of(skipped.item)?;
        if !world.arid(fun, skipped.site) {
            return Err(format!("site {} of {} ruled {:?}, but it is behaviour", skipped.site, skipped.item.0, skipped.rule));
        }
    }
    let mut store = BTreeMap::new();
    let a = drive(&world, Session::new(first.clone(), 0), &mut store, seed * 2 + 1, slots, 0, AMPLE, None)?;
    check(&world, &a, true)?;
    complete(&world, &a)?;
    let b = drive(&world, Session::new(first, 0), &mut BTreeMap::new(), seed * 2 + 2, slots, 0, AMPLE, None)?;
    if a.verdicts != b.verdicts {
        return Err("two hosts gave different verdicts".into());
    }

    let summary_plan = crate::command::PlanSummary { candidates: 0, decided: 0, arid: Vec::new() };
    let summary = summarize(&a, &summary_plan, &world.universe()).map_err(|error| error.to_string())?;
    let shown = a.verdicts.iter().filter(|(_, verdict, _)| matches!(verdict, Verdict::Survived { .. } | Verdict::NotReached)).count();
    if summary.diagnostics.len() != shown {
        return Err(format!("{} diagnostics for {shown} survivors and unreached", summary.diagnostics.len()));
    }
    for diagnostic in &summary.diagnostics {
        let mut text = Vec::new();
        render(diagnostic, &Lines, &mut text).map_err(|error| error.to_string())?;
        if !text.starts_with(b"warning[mutants::") {
            return Err("a diagnostic printed without its rule".into());
        }
    }

    let filed = identities(&world, &a)?;
    let k = world.edit(&mut rng);
    let second = plan_of(&world)?;
    let c = drive(&world, Session::new(second, 10 * MS), &mut store, seed * 2 + 3, slots, 10 * MS, AMPLE, Some(&filed))?;
    check(&world, &c, true)?;
    complete(&world, &c)?;

    let line = world.funs[k].facts.sites[0].pos.line;
    let change = Change { functions: vec![Changed { item: world.funs[k].facts.item, lines: Some(vec![line]) }] };
    let (session, _) = mutants(&Request::Change(&change), &world.universe(), &machine(slots), 0, plant)
        .map_err(|error| error.to_string())?;
    let d = drive(&world, session, &mut store, seed * 2 + 4, slots, 0, crate::select::EDIT_WALL_NS, None)?;
    check(&world, &d, false)?;
    let mut ran = 0;
    for (at, verdict, _) in &d.verdicts {
        if at.item != world.funs[k].facts.item {
            return Err(format!("a change to {} mutated {}", world.funs[k].facts.item.0, at.item.0));
        }
        if matches!(verdict, Verdict::Equivalent(_)) {
            continue;
        }
        ran += 1;
        let site = &world.funs[k].facts.sites[at.site as usize];
        if !at.mutation.operator(site).extreme() && site.pos.line != line && !matches!(verdict, Verdict::NotReached) {
            return Err(format!("a change to line {line} mutated line {}", site.pos.line));
        }
    }
    if ran > 4 {
        return Err(format!("{ran} mutants of one changed function, over the cap of 4"));
    }

    let wall = 5 * MS + rng.below(25 * MS);
    let tight = Config { budget: Budget { wall_ns: wall, slots }, ..full };
    let third = plan(&Scope::Items(&items), &world.universe(), &tight, plant).map_err(|error| error.to_string())?;
    let e = drive(&world, Session::new(third, 0), &mut BTreeMap::new(), seed * 2 + 5, slots, 0, wall, None)?;
    check(&world, &e, false)?;
    Ok(())
}
