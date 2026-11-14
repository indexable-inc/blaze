//! The planner: which mutants, in which order, with which tests.
//!
//! **What is in scope.** For a change: the functions whose body key moved since the base (the
//! caller computes that from the two snapshots' body records), with the item-relative lines that
//! changed. For one function: all of it. For a crate: every function in it.
//!
//! **What a mutant is.** One mutation of one site, applied to every instance of the function and
//! remade in every instance that inlined it at the tier under test ([`FnFacts::inliners`]). Its
//! tests are every test whose reach lists the function or one of its inliners (inlined code runs
//! without a call edge, so a reach that lists only an inliner still runs it).
//!
//! **Order** (deterministic, seeded by the change so reruns agree; the seed is the hash of the
//! scope's items with their body keys):
//!
//! 1. One extreme mutant per function ([`Priority::Extreme`]): return a default, else an
//!    argument, else a field's default. The cheapest and highest-yield: a survivor is a
//!    pseudo-tested function.
//! 2. Per changed line, its most telling mutant ([`Priority::Line`], [`Operator::telling`]),
//!    ties broken by the seed: Google's "one mutant per line".
//! 3. The rest on changed lines ([`Priority::Rest`]), in seeded order, up to the per-function cap.
//!
//! Across functions, all of one priority come before the next, functions in seeded order.
//!
//! **Tests per mutant**: by reach size, then key: a test reaching fewer functions is likelier to
//! be a unit test of this one (likelier to kill it) and cheaper. Recorded times are not used for
//! the order, because they differ between machines and the verdict, which names the killing test,
//! must not (rule 25); they set each test's timeout and the budget's estimates.
//!
//! **Budget** ([`Budget`]): a hard wall-clock deadline, the same on every machine (it is what a
//! person waits for), and as many mutants in flight as the machine has cores, each in its own
//! process: a 128-core machine does about ten times the mutants of a 12-core Mac in the same
//! seconds. Before running, [`fit`] packs the uncached mutants' estimates into the cores by list
//! scheduling in priority order and says which do not fit; the session ([`crate::run`]) then holds
//! the deadline itself, whatever the estimates said. What was not reached is reported as not run.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_keyed::kind::Kind;
use blaze_keyed::machine::Machine;
use blaze_program::key::{BodyKey, ItemId};
use blaze_program::record::TestKey;

use crate::Plant;
use crate::arid::{AridRule, arid, equivalent};
use crate::operator::{Mutation, Operator, mutations};
use crate::site::{FnFacts, ProgramRef, TestInfo};
use crate::verdict::{MutantVerdict, Verdict, VerdictInputs, VerdictKey};

/// The wall-clock budget of a run after an edit: what a person waits for.
pub const EDIT_WALL_NS: u64 = 4_000_000_000;
/// The budget of a run over one function, asked for by name.
pub const FUNCTION_WALL_NS: u64 = 30_000_000_000;
/// What one placement patch of a program costs (an estimate until measured; target.md's patch is
/// bytes proportional to the edit).
pub const PATCH_NS: u64 = 200_000;
/// A test's timeout is this many times its recorded time, plus [`TIMEOUT_SLACK_NS`].
pub const TIMEOUT_FACTOR: u64 = 4;
pub const TIMEOUT_SLACK_NS: u64 = 200_000_000;

const SEED_DOMAIN: &str = "blaze mutants seed 1";
const TIE_DOMAIN: &str = "blaze mutants tie 1";

/// A test's timeout from its recorded time.
pub fn timeout_of(recorded_ns: u64) -> u64 {
    recorded_ns.saturating_mul(TIMEOUT_FACTOR).saturating_add(TIMEOUT_SLACK_NS)
}

/// How long a session may take, and how many mutants run at once.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Budget {
    pub wall_ns: u64,
    /// Mutants in flight at once, each in its own process.
    pub slots: u32,
}

impl Budget {
    /// `wall_ns` on `machine`: a slot per core.
    pub fn of(machine: &Machine, wall_ns: u64) -> Budget {
        Budget { wall_ns, slots: machine.cores.max(1) }
    }
}

/// How many mutants per function and per line, and the budget.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Config {
    pub per_function: u32,
    pub per_line: u32,
    pub budget: Budget,
    pub patch_ns: u64,
}

impl Config {
    /// After an edit: a few mutants per changed function, one per changed line, a few seconds.
    pub fn change(machine: &Machine) -> Config {
        Config { per_function: 4, per_line: 1, budget: Budget::of(machine, EDIT_WALL_NS), patch_ns: PATCH_NS }
    }

    /// One function, asked for by name: every mutant of it.
    pub fn function(machine: &Machine) -> Config {
        Config {
            per_function: u32::MAX,
            per_line: u32::MAX,
            budget: Budget::of(machine, FUNCTION_WALL_NS),
            patch_ns: PATCH_NS,
        }
    }

    /// A whole crate within `wall_ns`: the change's caps, over every function.
    pub fn whole(machine: &Machine, wall_ns: u64) -> Config {
        Config { per_function: 4, per_line: 1, budget: Budget::of(machine, wall_ns), patch_ns: PATCH_NS }
    }
}

/// A function whose body key moved since the base, and the item-relative lines that changed
/// (`None`: every line, as for a new function).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Changed {
    pub item: ItemId,
    pub lines: Option<Vec<u32, Global>>,
}

/// What changed since the base.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Change {
    pub functions: Vec<Changed, Global>,
}

/// What a session mutates.
#[derive(Clone, Copy, Debug)]
pub enum Scope<'a> {
    Change(&'a Change),
    Function(ItemId),
    /// Every function listed (a crate's).
    Items(&'a [ItemId]),
}

/// What the planner reads: the functions (at least those in scope and their inliners) and every
/// test of the programs under test.
#[derive(Clone, Copy, Debug)]
pub struct Universe<'u> {
    pub functions: &'u [FnFacts],
    pub tests: &'u [TestInfo],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Priority {
    Extreme,
    Line,
    Rest,
}

/// One mutant, by what it changes: a site of one body.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct MutantRef {
    pub item: ItemId,
    pub body: BodyKey,
    /// The site's index in [`FnFacts::sites`] of that body.
    pub site: u32,
    pub mutation: Mutation,
}

/// A test as the session needs it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PlannedTest {
    pub key: TestKey,
    pub program: ProgramRef,
    pub timeout_ns: u64,
    pub estimate_ns: u64,
}

/// A mutant to run, unless its verdict is filed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub at: MutantRef,
    pub priority: Priority,
    pub key: VerdictKey,
    /// Indices into [`Plan::tests`], in run order.
    pub tests: Vec<u32, Global>,
    pub inliners: Vec<ItemId, Global>,
    /// Codegen of it and its inliners, a patch per program, every test: a survivor's cost.
    pub estimate_ns: u64,
}

/// A mutant decided without a run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Decided {
    pub at: MutantRef,
    pub verdict: Verdict,
}

/// A site not mutated, and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Skipped {
    pub item: ItemId,
    pub site: u32,
    pub rule: AridRule,
}

/// A session's plan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Plan {
    pub seed: ContentId,
    pub candidates: Vec<Candidate, Global>,
    pub decided: Vec<Decided, Global>,
    pub arid: Vec<Skipped, Global>,
    /// Every test of the universe, by its index there.
    pub tests: Vec<PlannedTest, Global>,
    pub budget: Budget,
}

/// Why no plan was made, naming the item or test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// An item in scope the universe has no facts of.
    UnknownItem { item: ItemId },
    /// An inliner of `item` the universe has no facts of.
    UnknownInliner { item: ItemId, inliner: ItemId },
    /// Two facts of one item.
    Twice { item: ItemId },
    /// A test whose reach is not sorted and each once.
    UnsortedReach { test: TestKey },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::UnknownItem { item } => write!(f, "no facts of function {} to mutate", item.0),
            PlanError::UnknownInliner { item, inliner } => {
                write!(f, "function {} is inlined into {}, of which there are no facts", item.0, inliner.0)
            }
            PlanError::Twice { item } => write!(f, "two sets of facts of function {}", item.0),
            PlanError::UnsortedReach { test } => write!(f, "the reach of test {} is not sorted", test.0),
        }
    }
}

/// The plan of `scope` over `universe`.
pub fn plan(scope: &Scope<'_>, universe: &Universe<'_>, config: &Config, plant: Plant) -> Result<Plan, PlanError> {
    let mut functions = BTreeMap::new_in(Global);
    for function in universe.functions {
        if functions.insert(function.item, function).is_some() {
            return Err(PlanError::Twice { item: function.item });
        }
    }
    let mut by_item: BTreeMap<ItemId, Vec<u32, Global>, Global> = BTreeMap::new_in(Global);
    let mut tests = Vec::with_capacity_in(universe.tests.len(), Global);
    for (index, test) in universe.tests.iter().enumerate() {
        if !test.reach.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(PlanError::UnsortedReach { test: test.key });
        }
        for item in &test.reach {
            by_item.entry(*item).or_insert_with(|| Vec::new_in(Global)).push(index as u32);
        }
        tests.push(PlannedTest {
            key: test.key,
            program: test.program,
            timeout_ns: timeout_of(test.recorded_ns),
            estimate_ns: test.recorded_ns,
        });
    }

    let mut scoped: Vec<(ItemId, Option<&[u32]>), Global> = Vec::new_in(Global);
    match scope {
        Scope::Change(change) => {
            for changed in &change.functions {
                scoped.push((changed.item, changed.lines.as_deref()));
            }
        }
        Scope::Function(item) => scoped.push((*item, None)),
        Scope::Items(items) => {
            for item in *items {
                scoped.push((*item, None));
            }
        }
    }
    scoped.sort_by_key(|(item, _)| *item);
    scoped.dedup_by_key(|(item, _)| *item);

    let mut seeder = Hasher::new(SEED_DOMAIN);
    seeder.update(&(scoped.len() as u64).to_le_bytes());
    for (item, _) in &scoped {
        let function = functions.get(item).ok_or(PlanError::UnknownItem { item: *item })?;
        seeder.id(&item.0).id(&function.body.0);
    }
    let seed = seeder.finish();

    let mut ordered = Vec::new_in(Global);
    let mut decided = Vec::new_in(Global);
    let mut skipped = Vec::new_in(Global);
    let mut admitted = Vec::new_in(Global);
    for (item, lines) in &scoped {
        let function: &FnFacts = functions.get(item).ok_or(PlanError::UnknownItem { item: *item })?;
        let mut reaching = BTreeSet::new_in(Global);
        for of in core::iter::once(item).chain(function.inliners.iter()) {
            if let Some(list) = by_item.get(of) {
                reaching.extend(list.iter().copied());
            }
        }
        let mut make_ns = function.make_ns;
        for inliner in &function.inliners {
            let facts = functions.get(inliner).ok_or(PlanError::UnknownInliner { item: *item, inliner: *inliner })?;
            make_ns = make_ns.saturating_add(facts.make_ns);
        }

        let mut real = Vec::new_in(Global);
        for (index, site) in function.sites.iter().enumerate() {
            let index = index as u32;
            if let Some(rule) = arid(function, site, plant) {
                skipped.push(Skipped { item: *item, site: index, rule });
                continue;
            }
            mutations(site, &mut admitted);
            for mutation in &admitted {
                let at = MutantRef { item: *item, body: function.body, site: index, mutation: *mutation };
                match equivalent(function, site, mutation) {
                    Some(rule) => decided.push(Decided { at, verdict: Verdict::Equivalent(rule) }),
                    None => real.push((index, *mutation)),
                }
            }
        }
        if real.is_empty() {
            continue;
        }
        let picked = pick(function, &real, *lines, &seed, config);
        if reaching.is_empty() {
            // One static finding per function: no test reaches it. More mutants of it say nothing
            // more.
            if let Some(&(_, site, mutation)) = picked.first() {
                let at = MutantRef { item: *item, body: function.body, site, mutation };
                decided.push(Decided { at, verdict: Verdict::NotReached });
            }
            continue;
        }

        let mut run = Vec::with_capacity_in(reaching.len() + 1, Global);
        run.extend(reaching.iter().copied());
        run.sort_by_key(|&test| {
            let info = &universe.tests[test as usize];
            (info.reach.len(), info.key)
        });
        if plant == Plant::RunsUnreachingTests {
            if let Some(extra) = (0..universe.tests.len() as u32).find(|test| !reaching.contains(test)) {
                run.push(extra);
            }
        }
        // The reaching tests' keys, sorted, each once: the verdict key's list (the plant leaves
        // it out).
        let mut keys = Vec::with_capacity_in(reaching.len(), Global);
        if plant != Plant::VerdictKeyWithoutTests {
            for test in &reaching {
                keys.push(universe.tests[*test as usize].key.0);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        let mut programs = BTreeSet::new_in(Global);
        let mut tests_ns = 0u64;
        for test in &run {
            let planned = &tests[*test as usize];
            programs.insert(planned.program);
            tests_ns = tests_ns.saturating_add(planned.estimate_ns);
        }
        let estimate_ns = make_ns
            .saturating_add(config.patch_ns.saturating_mul(programs.len() as u64))
            .saturating_add(tests_ns);
        let order = tie(&seed, item, u32::MAX, None);
        let mut bytes = Vec::new_in(Global);
        for (local, &(priority, site, mutation)) in picked.iter().enumerate() {
            bytes.clear();
            mutation.encode(&mut bytes);
            let key = MutantVerdict::key(&VerdictInputs { body: function.body.0, site, mutation: &bytes, tests: &keys });
            let candidate = Candidate {
                at: MutantRef { item: *item, body: function.body, site, mutation },
                priority,
                key,
                tests: run.clone(),
                inliners: function.inliners.clone(),
                estimate_ns,
            };
            ordered.push((priority, order, local as u32, candidate));
        }
    }
    ordered.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    let mut candidates = Vec::with_capacity_in(ordered.len(), Global);
    for (_, _, _, candidate) in ordered {
        candidates.push(candidate);
    }
    Ok(Plan { seed, candidates, decided, arid: skipped, tests, budget: config.budget })
}

/// The mutants of one function in the order the module's docs give, capped.
fn pick(
    function: &FnFacts,
    real: &[(u32, Mutation)],
    lines: Option<&[u32]>,
    seed: &ContentId,
    config: &Config,
) -> Vec<(Priority, u32, Mutation), Global> {
    let operator_of = |&(site, mutation): &(u32, Mutation)| mutation.operator(&function.sites[site as usize]);
    let line_of = |&(site, _): &(u32, Mutation)| function.sites[site as usize].pos.line;
    let on_line = |line: u32| lines.is_none_or(|lines| lines.contains(&line));
    let tie_of = |&(site, mutation): &(u32, Mutation)| tie(seed, &function.item, site, Some(&mutation));

    let mut out = Vec::new_in(Global);
    let mut taken = BTreeSet::new_in(Global);
    for want in [Operator::ReturnDefault, Operator::ReturnArg, Operator::ReturnField] {
        if let Some(index) = real.iter().position(|it| operator_of(it) == want) {
            out.push((Priority::Extreme, real[index].0, real[index].1));
            taken.insert(index);
            break;
        }
    }
    let mut site_lines = BTreeSet::new_in(Global);
    for it in real {
        if on_line(line_of(it)) {
            site_lines.insert(line_of(it));
        }
    }
    for line in site_lines {
        let mut chosen = 0u32;
        while chosen < config.per_line {
            let best = real
                .iter()
                .enumerate()
                .filter(|&(index, it)| !taken.contains(&index) && line_of(it) == line)
                .max_by(|x, y| {
                    let (a, b) = (x.1, y.1);
                    (operator_of(a).telling(), tie_of(b)).cmp(&(operator_of(b).telling(), tie_of(a)))
                });
            let Some((index, it)) = best else { break };
            out.push((Priority::Line, it.0, it.1));
            taken.insert(index);
            chosen += 1;
        }
    }
    let mut rest = Vec::new_in(Global);
    for (index, it) in real.iter().enumerate() {
        if !taken.contains(&index) && on_line(line_of(it)) {
            rest.push((tie_of(it), index));
        }
    }
    rest.sort_unstable();
    for (_, index) in rest {
        out.push((Priority::Rest, real[index].0, real[index].1));
    }
    out.truncate(config.per_function as usize);
    out
}

/// The seeded tie-break of a function (`site` `u32::MAX`, no mutation) or of one mutant.
fn tie(seed: &ContentId, item: &ItemId, site: u32, mutation: Option<&Mutation>) -> ContentId {
    let mut hasher = Hasher::new(TIE_DOMAIN);
    hasher.id(seed).id(&item.0).update(&site.to_le_bytes());
    if let Some(mutation) = mutation {
        let mut bytes = Vec::new_in(Global);
        mutation.encode(&mut bytes);
        hasher.part(&bytes);
    }
    hasher.finish()
}

/// Which of `estimates`, taken in order, fit `budget`: each goes to the slot that frees first,
/// and fits if it ends by the wall. One that does not fit takes no slot, so a later, smaller one
/// may still fit.
pub fn fit(estimates: &[u64], budget: &Budget) -> Vec<bool, Global> {
    let slots = budget.slots.max(1) as usize;
    let mut lanes = Vec::with_capacity_in(slots, Global);
    lanes.resize(slots, 0u64);
    let mut out = Vec::with_capacity_in(estimates.len(), Global);
    for &estimate in estimates {
        let mut at = 0;
        for (index, free) in lanes.iter().enumerate() {
            if *free < lanes[at] {
                at = index;
            }
        }
        let end = lanes[at].saturating_add(estimate);
        let fits = end <= budget.wall_ns;
        if fits {
            lanes[at] = end;
        }
        out.push(fits);
    }
    out
}
