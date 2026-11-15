//! The deterministic simulation: a model program is built, then edited step by step (body,
//! signature, added call, removed function, changed type argument, static value, span move,
//! tier-up), each step walked with shuffled completions over one to three shards and placed,
//! and its row change applied to a model of the linker. After every step the oracles check,
//! against a fresh build of the same revision from an empty store that holds the same
//! functions at the top tier (built by the same seeds rule, [`Program::seeds`] with
//! [`Program::top_tier`]):
//!
//! - the two hold the same nodes, each with the same row (key, tier, calls, statics, plan key
//!   and what it took in); a failure names the first node that differs and how
//!   ([`first_difference`]);
//! - the code records made are exactly the keys the program holds that the store did not;
//! - the linker, given only the row changes, holds exactly the program's rows, each with the
//!   current span of every item its code is made from ([`Linked`]);
//! - a test's key moved exactly when something it reaches changed;
//! - the manifest round-trips (whole and chunked).
//!
//! Every choice (the model, the edits, the order of completions) is drawn from one tape, so a
//! failure shrinks to its plan and its schedule together. Every planted bug is caught by some
//! case ([`super::controls`]).

use std::collections::{BTreeMap, BTreeSet};

use blaze_sim::{Config, Flight, Nanos, Tape};

use super::model::{Model, ModelFront, TYPES, chance, fn_symbol, item, static_symbol};
use crate::Plant;
use crate::change::{Row, RowChange};
use crate::codec::{Digest, Hasher, Payload};
use crate::key::{DebugInfo, InstanceKey, ItemId, Node, Panic, PlanKey, Settings, Symbol, Tier};
use crate::program::{Delta, FnRow, Program};
use crate::reach::{reach, reach_digest};
use crate::record::{CodeHead, Plan};
use crate::walk::{Answer, Ask, Fresh, Front, Prior, Seed, Span, Walk, WalkConfig, Walked, shard_of};

/// The store: plans and code heads by key, write-once. A code record's bytes are codegen's;
/// the walk reads only its head.
#[derive(Default)]
pub(super) struct Store {
    plans: BTreeMap<PlanKey, Vec<u8>>,
    code: BTreeMap<InstanceKey, CodeHead>,
}

/// The spans of the items a function's code is made from: its own item's and those of every
/// instance it took in, by item, as `front` gives them now.
fn spans_of(symbol: &Symbol, row: &FnRow, front: &ModelFront) -> Vec<(ItemId, Span)> {
    let mut items = BTreeSet::new();
    items.insert(symbol.item);
    items.extend(row.inlined.iter().map(|(inlinee, _)| inlinee.item));
    items.into_iter().filter_map(|item| front.span(&item).map(|span| (item, span))).collect()
}

/// The linker as a row change sees it: what it holds, by node and key (it removes by key, as
/// blaze-link does), with the spans its positions were filled from.
#[derive(Default, PartialEq, Eq, Debug)]
pub(super) struct Linked(BTreeMap<(Node, Digest), Vec<(ItemId, Span)>>);

impl Linked {
    /// Applies `change` as a linker would, reading the program only for what came (its code,
    /// in the linker's case) and the front for spans.
    fn apply(&mut self, change: &RowChange, program: &Program, front: &ModelFront) -> Result<(), String> {
        if change.whole && !self.0.is_empty() {
            return Err(format!("a whole change given to a linker holding {} rows", self.0.len()));
        }
        for row in &change.went {
            if self.0.remove(&(row.node, row.key)).is_none() {
                return Err(format!("{} went under {}, which the linker does not hold", row.node, row.key));
            }
        }
        for Row { node, key } in &change.came {
            let spans = match node {
                Node::Function(symbol) => match program.functions().get(symbol) {
                    Some(row) => spans_of(symbol, row, front),
                    None => return Err(format!("{node} came, and the program holds no row of it")),
                },
                Node::Static(_) => Vec::new(),
            };
            if self.0.insert((*node, *key), spans).is_some() {
                return Err(format!("{node} came under {key}, which the linker already holds"));
            }
        }
        for (moved, symbol) in &change.moved {
            let node = Node::Function(*symbol);
            let Some(key) = program.key_of(&node) else {
                return Err(format!("{node} moved, and the program holds no row of it"));
            };
            let Some(spans) = self.0.get_mut(&(node, key)) else {
                return Err(format!("{node} moved under {key}, which the linker does not hold"));
            };
            let Some(entry) = spans.iter_mut().find(|(item, _)| item == moved) else {
                return Err(format!("{node} moved with item {}, which its code is not made from", moved.0));
            };
            match front.span(moved) {
                Some(span) => entry.1 = span,
                None => return Err(format!("item {} moved, and the front has no span of it", moved.0)),
            }
        }
        Ok(())
    }

    /// What a linker holding `program` holds, from nothing.
    fn of(program: &Program, front: &ModelFront) -> Linked {
        let mut out = BTreeMap::new();
        for (symbol, row) in program.functions() {
            out.insert((Node::Function(*symbol), row.key.0), spans_of(symbol, row, front));
        }
        for (symbol, row) in program.statics() {
            out.insert((Node::Static(*symbol), row.key.0), Vec::new());
        }
        Linked(out)
    }

    /// The first entry in which this and `expected` differ.
    fn difference(&self, expected: &Linked, front: &ModelFront) -> Option<String> {
        let mut entries: BTreeSet<&(Node, Digest)> = self.0.keys().collect();
        entries.extend(expected.0.keys());
        for entry in entries {
            let named = format!("{} under {}", name(front, &entry.0), entry.1);
            match (self.0.get(entry), expected.0.get(entry)) {
                (Some(_), None) => return Some(format!("the linker holds {named}, which the program does not")),
                (None, Some(_)) => return Some(format!("the linker lacks {named}, which the program holds")),
                (Some(held), Some(want)) if held != want => {
                    return Some(format!("the linker's spans of {named} are {held:?}, and the front's are {want:?}"));
                }
                _ => {}
            }
        }
        None
    }
}

pub(super) fn settings() -> Settings {
    Settings {
        target: Digest::of("blaze program test target 1", b"model"),
        features: Digest::default(),
        toolchain: Digest::of("blaze program test toolchain 1", b"model"),
        tier: Tier::Edit,
        panic: Panic::Unwind,
        debuginfo: DebugInfo::Lines,
        overflow_checks: true,
    }
}

/// One walk, driven to its end over `shards` shards with completions in an order drawn from
/// `tape`, making nodes new to the program at `new_at`. Gives what was walked, the stale nodes
/// never reached, and the code records made, in order.
#[expect(clippy::too_many_arguments, reason = "a test driver; each argument is one knob of the case")]
pub(super) fn walk(
    front: &ModelFront,
    store: &mut Store,
    seeds: &[(Node, Seed)],
    prior: &impl Prior,
    shards: u32,
    plant: Plant,
    new_at: Tier,
    tape: &Tape,
) -> Result<(Vec<Walked>, Vec<Node>, Vec<InstanceKey>), String> {
    let mut walks = Vec::new();
    for shard in 0..shards {
        walks.push(Walk::new(WalkConfig { settings: front.settings, shard, shards, fuel: 200_000, plant, new_at }));
    }
    for (node, seed) in seeds {
        walks[shard_of(&node.symbol(), shards) as usize].reach(*node, *seed, prior);
    }
    // Every answer is due at once: which comes next is the tape's choice.
    let mut pending: Flight<(usize, Answer)> = Flight::new();
    let mut makes = Vec::new();
    for _ in 0..1_000_000 {
        let mut asked = false;
        for shard in 0..walks.len() {
            loop {
                let ask = walks[shard].next_ask(front, prior).map_err(|error| error.to_string())?;
                let Some(ask) = ask else { break };
                asked = true;
                match ask {
                    Ask::Route { node, shard: to, seed } => walks[to as usize].reach(node, seed, prior),
                    Ask::LookupPlan { symbol, key } => {
                        let plan = match store.plans.get(&key) {
                            Some(bytes) => Some(Plan::decode(bytes).map_err(|error| error.to_string())?),
                            None => None,
                        };
                        pending.ask((shard, Answer::Plan { symbol, key, plan }));
                    }
                    Ask::MakePlan { symbol, key } => {
                        let plan = front.plan(&symbol);
                        let bytes = plan.encode();
                        if let Some(held) = store.plans.get(&key) {
                            if *held != bytes {
                                return Err(format!("two plans of {symbol} under one key"));
                            }
                        }
                        store.plans.insert(key, bytes);
                        pending.ask((shard, Answer::Plan { symbol, key, plan: Some(plan) }));
                    }
                    Ask::LookupCode { symbol, key } => {
                        let head = store.code.get(&key).cloned();
                        pending.ask((shard, Answer::Code { symbol, key, head }));
                    }
                    Ask::MakeCode { symbol, key, tier, inline, .. } => {
                        if store.code.contains_key(&key) {
                            return Err(format!("the code of {symbol} was made again under a key the store holds"));
                        }
                        let head = front.make_head(&symbol, tier, &inline);
                        store.code.insert(key, head.clone());
                        makes.push(key);
                        pending.ask((shard, Answer::Code { symbol, key, head: Some(head) }));
                    }
                }
            }
        }
        if pending.is_empty() {
            if walks.iter().all(Walk::is_idle) {
                let mut walked = Vec::new();
                let mut unreached = Vec::new();
                for walk in walks {
                    let ended = walk.end().map_err(|error| error.to_string())?;
                    walked.extend(ended.walked);
                    unreached.extend(ended.unreached);
                }
                unreached.sort_unstable();
                return Ok((walked, unreached, makes));
            }
            if !asked {
                return Err("a walk is not idle, yet asks nothing and waits for nothing".to_string());
            }
            continue;
        }
        let Some((shard, answer)) = pending.deliver(tape, Nanos(0)) else {
            return Err("answers are in flight, yet none is due".to_string());
        };
        walks[shard].answer(answer, front, prior).map_err(|error| error.to_string())?;
    }
    Err("the walk's driver ran out of steps".to_string())
}

pub(super) fn roots(model: &Model) -> Vec<Node> {
    model.roots().into_iter().map(|index| Node::Function(fn_symbol(model, index, &[]))).collect()
}

/// A build from nothing of `front`'s revision into a fresh store, holding `top` (a program's
/// [`Program::top_tier`]) at the top tier: the same seeds rule as every delta, so a whole build
/// and a patched program decide which nodes have rows by one rule. Gives the program, the store
/// and the program's first change.
pub(super) fn fresh(
    front: &ModelFront,
    shards: u32,
    plant: Plant,
    top: &[Symbol],
    tape: &Tape,
) -> Result<(Program, Store, RowChange), String> {
    let mut store = Store::default();
    let delta = Delta { tier_up: top.to_vec(), ..Delta::default() };
    let mut program = Program::new(front.settings, &roots(&front.model), plant);
    let seeds = program.seeds(&delta);
    // A whole build: every node is new, and only `top` is made at the top tier.
    let (walked, unreached, _) = walk(front, &mut store, &seeds, &Fresh, shards, plant, Tier::Edit, tape)?;
    let change = program.place(&walked, &unreached, &[]).map_err(|error| error.to_string())?;
    if !change.whole || !change.went.is_empty() || !change.moved.is_empty() {
        return Err(format!("a first build's change is not whole rows that came: {change:?}"));
    }
    Ok((program, store, change))
}

/// A node as a failure names it: the model's function index and type arguments (or static
/// index), then its symbol.
fn name(front: &ModelFront, node: &Node) -> String {
    match node {
        Node::Function(symbol) => match front.find(symbol) {
            Some((index, args)) if args.is_empty() => format!("fn {index} ({symbol})"),
            Some((index, args)) => format!("fn {index}{args:?} ({symbol})"),
            None => format!("fn ? ({symbol}, an instance this revision does not have)"),
        },
        Node::Static(symbol) => match front.static_index(symbol) {
            Some(index) => format!("static {index} ({symbol})"),
            None => format!("static ? ({symbol})"),
        },
    }
}

fn names(front: &ModelFront, symbols: &[Symbol]) -> String {
    symbols.iter().map(|symbol| name(front, &Node::Function(*symbol))).collect::<Vec<_>>().join(", ")
}

/// What `program` holds of `node`: its key, its tier, its calls and what it took in.
fn row_of(program: &Program, front: &ModelFront, node: &Node) -> String {
    match node {
        Node::Function(symbol) => match program.functions().get(symbol) {
            Some(row) => format!(
                "key {}, {:?} tier, calls [{}], took in [{}]",
                row.key,
                row.tier,
                names(front, &row.calls),
                names(front, &row.inlined.iter().map(|(inlinee, _)| *inlinee).collect::<Vec<_>>())
            ),
            None => "no row".to_string(),
        },
        Node::Static(symbol) => match program.statics().get(symbol) {
            Some(row) => format!("value key {}, points at [{}]", row.key.0, names(front, &row.calls)),
            None => "no row".to_string(),
        },
    }
}

/// What in `program` refers to `node`: its rows that call or point at it, and whether it is a
/// root.
fn referrers(program: &Program, front: &ModelFront, node: &Node) -> String {
    let mut out = Vec::new();
    if program.roots().contains(node) {
        out.push("a root".to_string());
    }
    let refers = |calls: &[Symbol], statics: &[Symbol]| match node {
        Node::Function(target) => calls.contains(target),
        Node::Static(target) => statics.contains(target),
    };
    for (symbol, row) in program.functions() {
        if refers(&row.calls, &row.statics) {
            out.push(name(front, &Node::Function(*symbol)));
        }
    }
    for (symbol, row) in program.statics() {
        if refers(&row.calls, &row.statics) {
            out.push(name(front, &Node::Static(*symbol)));
        }
    }
    out.join(", ")
}

/// A node's row as two programs are compared: everything but the build it came in.
#[derive(PartialEq, Eq, Debug)]
enum Compared<'p> {
    Function { key: InstanceKey, tier: Tier, calls: &'p [Symbol], statics: &'p [Symbol], plan: PlanKey, inlined: &'p [(Symbol, PlanKey)] },
    Static { key: Digest, calls: &'p [Symbol], statics: &'p [Symbol] },
}

fn compared(program: &Program) -> BTreeMap<Node, Compared<'_>> {
    let mut out = BTreeMap::new();
    for (symbol, row) in program.functions() {
        out.insert(
            Node::Function(*symbol),
            Compared::Function {
                key: row.key,
                tier: row.tier,
                calls: &row.calls,
                statics: &row.statics,
                plan: row.plan,
                inlined: &row.inlined,
            },
        );
    }
    for (symbol, row) in program.statics() {
        out.insert(Node::Static(*symbol), Compared::Static { key: row.key.0, calls: &row.calls, statics: &row.statics });
    }
    out
}

/// The first node, in node order, where a patched program and a fresh build differ, and why: a
/// missing row, an extra row, or another row, with both programs' rows of it. `None` when every
/// node is the same.
pub(super) fn first_difference(ours: &Program, theirs: &Program, front: &ModelFront) -> Option<String> {
    let our_rows = compared(ours);
    let their_rows = compared(theirs);
    let mut nodes: BTreeSet<Node> = our_rows.keys().copied().collect();
    nodes.extend(their_rows.keys().copied());
    for node in &nodes {
        let named = name(front, node);
        match (our_rows.get(node), their_rows.get(node)) {
            (None, Some(_)) => {
                return Some(format!(
                    "missing row: {named} has a row in a fresh build ({}; referred to by [{}]) and none in the patched program (referred to by [{}])",
                    row_of(theirs, front, node),
                    referrers(theirs, front, node),
                    referrers(ours, front, node)
                ));
            }
            (Some(_), None) => {
                return Some(format!(
                    "extra row: {named} has a row in the patched program ({}; referred to by [{}]) and none in a fresh build (referred to by [{}])",
                    row_of(ours, front, node),
                    referrers(ours, front, node),
                    referrers(theirs, front, node)
                ));
            }
            (Some(mine), Some(fresh)) if mine != fresh => {
                return Some(format!(
                    "other row: {named} (patched: {mine:?}; fresh: {fresh:?}; patched: {}; fresh: {})",
                    row_of(ours, front, node),
                    row_of(theirs, front, node)
                ));
            }
            _ => {}
        }
    }
    None
}

/// What a test can observe of the model: every instance it can call (by its body key and its
/// type arguments), every static it can read, and the span of every instance's item. From the
/// model alone, never the program: not the tier an instance is held at, nor whether it is held
/// or taken in, as neither changes what the test observes.
fn model_reach(front: &ModelFront, entry: usize) -> Digest {
    let model = &front.model;
    let mut seen = BTreeSet::new();
    let mut statics = BTreeSet::new();
    let mut stack = vec![fn_symbol(model, entry, &[])];
    seen.insert(stack[0]);
    while let Some(symbol) = stack.pop() {
        let (index, args) = front.instance(&symbol);
        let function = &model.fns[index];
        let mut next = Vec::new();
        for call in &function.calls {
            next.push(front.callee(call, &args));
        }
        for target in &function.statics {
            if statics.insert(*target) {
                if let Some(referred) = model.statics[*target].fn_ref {
                    next.push(fn_symbol(model, referred, &[]));
                }
            }
        }
        for callee in next {
            if seen.insert(callee) {
                stack.push(callee);
            }
        }
    }
    let mut hasher = Hasher::new("blaze program test reach 1");
    for symbol in &seen {
        let (index, _) = front.instance(symbol);
        let body = front.body_key(&item(index)).map(|key| key.0).unwrap_or_default();
        hasher.id(&symbol.item.0).id(&symbol.args.0).id(&body);
        let span = front.span(&item(index)).map(|span| (span.file, span.line)).unwrap_or_default();
        hasher.part(&span.0).update(&span.1.to_le_bytes());
    }
    for target in &statics {
        if let Some(value) = front.static_value(&static_symbol(*target)) {
            hasher.id(&value.key().0);
        }
    }
    hasher.finish()
}

/// A program under edit, with everything the oracles compare it to.
pub(super) struct World {
    /// Where every choice of the world comes from.
    pub(super) tape: Tape,
    pub(super) model: Model,
    pub(super) front: ModelFront,
    pub(super) store: Store,
    pub(super) program: Program,
    /// The linker, given every change since the first build.
    pub(super) linked: Linked,
    pub(super) shards: u32,
    pub(super) plant: Plant,
    /// Of the last step: the delta, the code records made and the row change.
    pub(super) last_delta: Delta,
    pub(super) last_makes: Vec<InstanceKey>,
    pub(super) last_change: RowChange,
    /// The function the next edit picks, when a test names one.
    pub(super) pick: Option<usize>,
}

/// One edit of the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Edit {
    Body,
    Signature,
    AddCall,
    RemoveFunction,
    ChangeType,
    StaticValue,
    MoveSpan,
    TierUp,
    /// A body edit that changes no fact a caller reads (only its text): the floor case.
    Text,
    /// A body edit that marks the function `#[inline(always)]`: every caller takes it in.
    Always,
    /// An edit of the type with this index (its content key moves).
    Retype(u8),
}

impl World {
    pub(super) fn new(tape: &Tape, plant: Plant) -> Result<World, String> {
        let model = Model::generate(tape);
        let shards = 1 + tape.below(3, "more shards") as u32;
        World::of(model, tape.clone(), shards, plant)
    }

    /// A world over a model a test wrote.
    pub(super) fn of(model: Model, tape: Tape, shards: u32, plant: Plant) -> Result<World, String> {
        let front = ModelFront::new(&model, settings());
        let (program, store, change) = fresh(&front, shards, plant, &[], &tape)?;
        let mut linked = Linked::default();
        linked.apply(&change, &program, &front)?;
        Ok(World {
            tape,
            model,
            front,
            store,
            program,
            linked,
            shards,
            plant,
            last_delta: Delta::default(),
            last_makes: Vec::new(),
            last_change: change,
            pick: None,
        })
    }

    /// Applies a drawn edit to the model, giving the delta a front end would give.
    fn edit(&mut self, edit: Edit) -> Delta {
        let tape = self.tape.clone();
        let tape = &tape;
        let mut model = self.model.clone();
        let live: Vec<usize> = (0..model.fns.len()).filter(|index| model.fns[*index].alive).collect();
        let drawn = live[tape.index(live.len(), "edited function")];
        let pick = self.pick.take().unwrap_or(drawn);
        match edit {
            Edit::Text => model.fns[pick].text += 1,
            Edit::Always => {
                model.fns[pick].text += 1;
                model.fns[pick].attr = crate::inline::InlineAttr::Always;
            }
            Edit::Retype(ty) => model.types[ty as usize] += 1,
            Edit::Body => {
                let function = &mut model.fns[pick];
                function.text += 1;
                if chance(tape, 1, 2, "new cost") {
                    function.cost = [10, 40, 80, 200][tape.below(4, "cost") as usize];
                }
                if chance(tape, 1, 2, "panics change") {
                    function.panics = (function.panics + 1) % 4;
                }
                if chance(tape, 1, 3, "attr changes") {
                    function.attr = [crate::inline::InlineAttr::None, crate::inline::InlineAttr::Always][tape.below(2, "attr") as usize];
                }
            }
            Edit::Signature => model.fns[pick].sig += 1,
            Edit::AddCall => {
                let call = model.random_call(tape, pick);
                model.fns[pick].calls.push(call);
                model.fns[pick].text += 1;
            }
            Edit::RemoveFunction => {
                let roots = model.roots();
                if let Some(gone) = live.iter().copied().find(|index| !roots.contains(index) && chance(tape, 1, 2, "removed")) {
                    model.fns[gone].alive = false;
                    for function in &mut model.fns {
                        let before = function.calls.len();
                        function.calls.retain(|call| call.callee != gone);
                        if function.calls.len() != before {
                            function.text += 1;
                        }
                    }
                    for value in &mut model.statics {
                        if value.fn_ref == Some(gone) {
                            value.fn_ref = None;
                        }
                    }
                }
            }
            Edit::ChangeType => {
                let ty = tape.below(u64::from(TYPES), "retyped") as usize;
                model.types[ty] += 1;
            }
            Edit::StaticValue => {
                let target = tape.index(model.statics.len(), "static edited");
                model.statics[target].bytes[0] = model.statics[target].bytes[0].wrapping_add(1);
                if chance(tape, 1, 3, "static repointed") {
                    model.statics[target].fn_ref = model.random_plain(tape);
                }
            }
            Edit::MoveSpan => model.fns[pick].line += 3,
            Edit::TierUp => {}
        }
        let front = ModelFront::new(&model, settings());
        let mut delta = Delta::default();
        for index in 0..model.fns.len() {
            let id = item(index);
            let (old, new) = (self.front.body_key(&id), front.body_key(&id));
            if self.model.fns[index].alive && !model.fns[index].alive {
                delta.gone.push(id);
            } else if old != new {
                delta.edited.push(id);
            } else if self.front.span(&id) != front.span(&id) {
                delta.moved.push(id);
            }
        }
        for index in 0..model.statics.len() {
            let symbol = static_symbol(index);
            if self.front.static_value(&symbol).map(|value| value.key()) != front.static_value(&symbol).map(|value| value.key()) {
                delta.statics.push(symbol);
            }
        }
        if edit == Edit::TierUp {
            delta.tier_up = self.program.tier_ups();
        }
        self.model = model;
        self.front = front;
        delta
    }

    /// Each test's reach digest: what a reached-basis result and a mutant verdict are kept under.
    fn test_reaches(&self) -> Vec<Digest> {
        let mut out = Vec::new();
        for test in &self.model.tests {
            let entry = fn_symbol(&self.model, *test, &[]);
            let reached = reach(&self.program, &entry, &self.front, self.plant);
            out.push(reach_digest(&reached));
        }
        out
    }

    fn model_reaches(&self) -> Vec<Digest> {
        self.model.tests.iter().map(|test| model_reach(&self.front, *test)).collect()
    }

    /// One edit, walked, placed and checked by every oracle.
    pub(super) fn step(&mut self, edit: Edit) -> Result<(), String> {
        let tests_before = self.test_reaches();
        let reaches_before = self.model_reaches();
        let delta = self.edit(edit);
        let store_before: BTreeSet<InstanceKey> = self.store.code.keys().copied().collect();
        let seeds = self.program.seeds(&delta);
        let program = self.program.clone();
        let (walked, unreached, makes) =
            walk(&self.front, &mut self.store, &seeds, &program, self.shards, self.plant, delta.new_at(), &self.tape)?;
        let change = self.program.place(&walked, &unreached, &delta.moved).map_err(|error| error.to_string())?;
        self.linked.apply(&change, &self.program, &self.front)?;

        // The makes are exactly the held keys the store did not have.
        let made: BTreeSet<InstanceKey> = makes.iter().copied().collect();
        if made.len() != makes.len() {
            return Err("a code record was made twice".to_string());
        }
        let held: BTreeSet<InstanceKey> = self.program.functions().values().map(|row| row.key).collect();
        let expected: BTreeSet<InstanceKey> = held.difference(&store_before).copied().collect();
        if made != expected {
            // Each key named by the node the walk or the program gave it to.
            let named = |key: &InstanceKey| {
                let node = walked
                    .iter()
                    .find(|entry| entry.key == key.0)
                    .map(|entry| entry.node)
                    .or_else(|| {
                        self.program.functions().iter().find(|(_, row)| row.key == *key).map(|(symbol, _)| Node::Function(*symbol))
                    });
                match node {
                    Some(node) => format!("{} under {key}", name(&self.front, &node)),
                    None => format!("{key}"),
                }
            };
            let unmade: Vec<String> = expected.difference(&made).map(|key| named(key)).collect();
            let thrown: Vec<String> = made.difference(&expected).map(|key| named(key)).collect();
            return Err(format!(
                "{} records were made where {} keys were new; made and not held: [{}]; held and not made: [{}]",
                made.len(),
                expected.len(),
                thrown.join("; "),
                unmade.join("; ")
            ));
        }
        self.last_delta = delta;
        self.last_makes = makes;
        self.last_change = change;

        // A test's reach digest moved exactly when what it reaches changed; a tier-up changes
        // nothing a test observes, so it moves no digest.
        let tests_after = self.test_reaches();
        let reaches_after = self.model_reaches();
        for index in 0..tests_after.len() {
            let moved = tests_before[index] != tests_after[index];
            let changed = reaches_before[index] != reaches_after[index];
            if moved != changed {
                return Err(format!(
                    "test {index}: its reach digest {} but what it reaches {}",
                    if moved { "moved" } else { "stayed" },
                    if changed { "changed" } else { "did not" }
                ));
            }
        }
        self.check()
    }

    /// The program and the linker against a fresh build of the same revision from an empty
    /// store.
    pub(super) fn check(&mut self) -> Result<(), String> {
        if let Some(why) = self.linked.difference(&Linked::of(&self.program, &self.front), &self.front) {
            return Err(why);
        }
        // The same revision built whole, holding the same functions at the top tier: every
        // node and its row must be the same.
        let top = self.program.top_tier();
        let (program, _, _) = fresh(&self.front, self.shards, self.plant, &top, &self.tape)?;
        if let Some(why) = first_difference(&self.program, &program, &self.front) {
            return Err(why);
        }
        if self.plant == Plant::None {
            let decoded = Program::decode(&self.program.encode()).map_err(|error| error.to_string())?;
            if decoded != self.program {
                return Err("the manifest does not round-trip".to_string());
            }
            let chunked = Program::from_chunks(&self.program.chunks()).map_err(|error| error.to_string())?;
            if chunked != self.program {
                return Err("the chunked manifest does not round-trip".to_string());
            }
        }
        Ok(())
    }
}

pub(super) const EDITS: [Edit; 8] = [
    Edit::Body,
    Edit::Signature,
    Edit::AddCall,
    Edit::RemoveFunction,
    Edit::ChangeType,
    Edit::StaticValue,
    Edit::MoveSpan,
    Edit::TierUp,
];

/// Runs `steps` edits drawn from `tape`; the first violation, named with its step (the failure
/// report gives the tape that replays it).
pub(super) fn run(tape: &Tape, steps: usize, plant: Plant) -> Result<(), String> {
    let mut world = World::new(tape, plant).map_err(|why| format!("first build: {why}"))?;
    world.check().map_err(|why| format!("first build: {why}"))?;
    for step in 0..steps {
        let edit = *tape.one_of(&EDITS, "edit");
        world.step(edit).map_err(|why| format!("step {step} ({edit:?}): {why}"))?;
    }
    Ok(())
}

/// The oracles hold over many seeds and edit sequences.
///
/// The invariant: after every edit, the patched program equals a fresh build of the same
/// revision holding the same functions at the top tier (every node and its row), the linker
/// given only the row changes holds exactly its rows with current spans, and a test's key
/// moves exactly when what the test can observe moved (the body keys and type arguments of
/// every instance it can run, the values of the statics it can read, the spans of their
/// items), which a tier never changes: a tier-up never moves a test's key, so it never runs a
/// test again.
#[test]
fn patched_programs_equal_fresh_builds() {
    blaze_sim::check(Config::cases(64), |tape| run(tape, 12, Plant::None));
}

/// Every edit kind, on every case, at least once.
#[test]
fn every_edit_kind_holds() {
    blaze_sim::check(Config::cases(16).first(100), |tape| -> Result<(), String> {
        let mut world = World::new(tape, Plant::None)?;
        for edit in EDITS {
            world.step(edit).map_err(|why| format!("{edit:?}: {why}"))?;
        }
        Ok(())
    });
}

/// The floor of each edit. A text-only edit of a function remakes exactly its instances and
/// their transitive inliners (at the top tier: every instance that took it in at any depth),
/// nothing else; a span move remakes nothing, changes no key, and moves exactly the rows made
/// from the moved item.
#[test]
fn edits_cost_their_floor() {
    blaze_sim::check(Config::cases(64).first(200), |tape| {
        let mut world = match World::new(tape, Plant::None) {
            Ok(world) => world,
            Err(why) => panic!("first build: {why}"),
        };
        // Some cases tier everything up first, so the inliners are the top tier's.
        // (A row tiers up once it has been unchanged for one build, so a build passes first.)
        if tape.coin("tier up first") {
            world.step(Edit::MoveSpan).unwrap_or_else(|why| panic!("span move: {why}"));
            world.step(Edit::TierUp).unwrap_or_else(|why| panic!("tier-up: {why}"));
            let fast: Vec<String> = world
                .program
                .functions()
                .iter()
                .filter(|(_, row)| row.tier == Tier::Edit)
                .map(|(symbol, row)| {
                    let node = Node::Function(*symbol);
                    format!(
                        "{} ({}; since build {} of {}; referred to by [{}])",
                        name(&world.front, &node),
                        row_of(&world.program, &world.front, &node),
                        row.since,
                        world.program.generation(),
                        referrers(&world.program, &world.front, &node)
                    )
                })
                .collect();
            assert!(fast.is_empty(), "a tier-up left rows at the fast tier: {}", fast.join("; "));
        }
        let live: Vec<usize> = (0..world.model.fns.len()).filter(|index| world.model.fns[*index].alive).collect();
        let target = live[tape.index(live.len(), "target")];
        let target_item = item(target);
        let made_from = |program: &Program| -> BTreeSet<Symbol> {
            program
                .functions()
                .iter()
                .filter(|(symbol, row)| symbol.item == target_item || row.inlined.iter().any(|(inlinee, _)| inlinee.item == target_item))
                .map(|(symbol, _)| *symbol)
                .collect()
        };
        let expected = made_from(&world.program);
        world.pick = Some(target);
        world.step(Edit::Text).unwrap_or_else(|why| panic!("text edit: {why}"));
        let mut remade = BTreeSet::new();
        for (symbol, row) in world.program.functions() {
            if world.last_makes.contains(&row.key) {
                remade.insert(*symbol);
            }
        }
        assert_eq!(world.last_makes.len(), remade.len(), "a make the program does not hold");
        assert_eq!(remade, expected, "a text edit of function {target} remade other than it and its inliners");

        // A span move of the same function.
        let expected: BTreeSet<(ItemId, Symbol)> = made_from(&world.program).into_iter().map(|symbol| (target_item, symbol)).collect();
        world.pick = Some(target);
        world.step(Edit::MoveSpan).unwrap_or_else(|why| panic!("span move: {why}"));
        assert!(world.last_makes.is_empty(), "a span move made a code record");
        let change = &world.last_change;
        assert!(change.came.is_empty() && change.went.is_empty(), "a span move changed a row's key: {change:?}");
        let moved: BTreeSet<(ItemId, Symbol)> = change.moved.iter().copied().collect();
        assert_eq!(moved, expected, "a span move of function {target} moved other than the rows made from it");
    });
}

/// A reached body that fails its check stops the build with an error naming the body and the
/// check; a body nothing reaches is never checked.
#[test]
fn a_reached_refused_body_stops_the_walk() {
    let tape = Tape::seeded(7);
    let mut model = Model::generate(&tape);
    let reached = model.fns[0].calls[0].callee;
    model.refused = Some(reached);
    let refusing = ModelFront::new(&model, settings());
    let error = match fresh(&refusing, 1, Plant::None, &[], &tape) {
        Ok(_) => panic!("a build reaching a refused body succeeded"),
        Err(error) => error,
    };
    assert!(error.contains("fails its check"), "the error names the check: {error}");
    assert!(error.contains(&item(reached).0.to_string()), "the error names the body: {error}");
    let unreached = model.fns.len();
    model.fns.push(model.fns[1].clone());
    model.refused = Some(unreached);
    let unreferenced = ModelFront::new(&model, settings());
    if let Err(why) = fresh(&unreferenced, 1, Plant::None, &[], &tape) {
        panic!("a build failed on a body it does not reach: {why}");
    }
}

/// A function of a written model: no generics, no statics, no panics, in file 0.
fn written(index: usize, attr: crate::inline::InlineAttr, cost: u32, calls: &[usize]) -> super::model::ModelFn {
    let mut list = Vec::new();
    for callee in calls {
        list.push(super::model::Call { callee: *callee, args: Vec::new() });
    }
    super::model::ModelFn {
        generics: 0,
        text: 0,
        sig: 0,
        attr,
        cost,
        cold: false,
        calls: list,
        statics: Vec::new(),
        panics: 0,
        alive: true,
        file: 0,
        line: 10 * index as u32 + 1,
    }
}

/// `main` (0) calls function 3; the tests (1, 2) call nothing. Function 3 is the workspace's
/// (3 % 3 != 2), as is `main`.
fn main_calls_three(attr: crate::inline::InlineAttr, cost: u32) -> Model {
    use crate::inline::InlineAttr;
    Model {
        fns: vec![
            written(0, InlineAttr::None, 40, &[3]),
            written(1, InlineAttr::None, 10, &[]),
            written(2, InlineAttr::None, 10, &[]),
            written(3, attr, cost, &[]),
        ],
        statics: Vec::new(),
        types: [0; TYPES as usize],
        tests: vec![1, 2],
        refused: None,
    }
}

/// Regression (seed 100 of `every_edit_kind_holds`): an edit of an instance that is only ever
/// inlined (it has no row of its own, so no call site) must still remake every instance that
/// took it in. The seeds found an edited item's inliners only through its held rows, so
/// `main` kept the old copy of function 3.
#[test]
fn an_edit_of_an_instance_only_inlined_remakes_its_inliners() {
    let model = main_calls_three(crate::inline::InlineAttr::Always, 10);
    let mut world = World::of(model, Tape::seeded(1), 1, Plant::None).unwrap_or_else(|why| panic!("first build: {why}"));
    assert!(
        !world.program.functions().keys().any(|symbol| symbol.item == item(3)),
        "function 3 has a row of its own, though `main` takes it in at the fast tier"
    );
    world.pick = Some(3);
    world.step(Edit::Text).unwrap_or_else(|why| panic!("text edit of function 3: {why}"));
    let main = fn_symbol(&world.model, 0, &[]);
    let main_key = world.program.functions()[&main].key;
    assert_eq!(world.last_makes, vec![main_key], "the edit remakes `main`, and only it");
}

/// Regression (seed 0 of `patched_programs_equal_fresh_builds`, seed 204 of
/// `edits_cost_their_floor`): a tier-up makes only records the program then holds. Function 3
/// is `#[inline]` and small: called at the fast tier, taken into `main` at the top tier.
/// Tiering both up must make `main` alone; making function 3 at the top tier too is a record
/// made to be thrown away (its only caller takes it in).
#[test]
fn a_tier_up_makes_only_what_the_program_holds() {
    let model = main_calls_three(crate::inline::InlineAttr::Hint, 40);
    let mut world = World::of(model, Tape::seeded(2), 1, Plant::None).unwrap_or_else(|why| panic!("first build: {why}"));
    assert!(
        world.program.functions().keys().any(|symbol| symbol.item == item(3)),
        "function 3 has no row at the fast tier, which takes nothing in"
    );
    // A build passes, so every row is old enough to tier up.
    world.pick = Some(1);
    world.step(Edit::MoveSpan).unwrap_or_else(|why| panic!("span move: {why}"));
    world.step(Edit::TierUp).unwrap_or_else(|why| panic!("tier-up: {why}"));
    assert!(
        !world.program.functions().keys().any(|symbol| symbol.item == item(3)),
        "function 3 still has a row of its own after `main` took it in"
    );
    let main = fn_symbol(&world.model, 0, &[]);
    assert!(world.last_makes.contains(&world.program.functions()[&main].key), "`main` was not remade");
    // Test 2 is a dependency's (2 % 3 == 2), at the top tier from the first build.
    assert_eq!(world.last_makes.len(), 2, "the tier-up makes `main` and test 1, and nothing else");
}

/// Regression (seed 0, step 7 of `patched_programs_equal_fresh_builds`): an edit that makes a
/// function `#[inline(always)]` makes its callers take it in, so its own instance leaves the
/// set of rows. It was seeded as an edit and made for its own sake, then retired: a record
/// made to be thrown away. By the row rule it is stale with only walked referrers, so walked
/// only if reached, and never reached.
#[test]
fn an_instance_its_callers_now_take_in_is_not_made() {
    let model = main_calls_three(crate::inline::InlineAttr::None, 40);
    let mut world = World::of(model, Tape::seeded(4), 1, Plant::None).unwrap_or_else(|why| panic!("first build: {why}"));
    world.pick = Some(3);
    world.step(Edit::Always).unwrap_or_else(|why| panic!("function 3 made inline(always): {why}"));
    assert!(
        !world.program.functions().keys().any(|symbol| symbol.item == item(3)),
        "function 3 kept a row though `main` takes it in"
    );
    let main = fn_symbol(&world.model, 0, &[]);
    assert_eq!(world.last_makes, vec![world.program.functions()[&main].key], "the edit makes `main`, and only it");
}

/// Regression (`every_edit_kind_holds`, "the model has no instance"): a type edit moves the
/// symbol of every instance at that type. `main` names type 0, so it is edited and calls
/// `3<0>` at the new type. `3<0>` passes its parameter on to `4<0>`, so its body is unchanged;
/// 4 names type 0 itself (it calls `3<0>` back at a concrete type 0), so the old `4<0>` is an
/// instance of an edited item. The old `4<0>` is reached only through the old `3<0>`, which
/// only `main` called: it is not in the new set of rows and must be neither walked (its symbol
/// is no instance of the new revision) nor kept.
#[test]
fn an_edited_instance_at_a_retyped_argument_is_not_walked() {
    use super::model::{Arg, Call};
    let none = crate::inline::InlineAttr::None;
    let mut model = main_calls_three(none, 40);
    model.fns[0].calls = vec![Call { callee: 3, args: vec![Arg::Concrete(0)] }];
    model.fns[3].generics = 1;
    model.fns[3].calls = vec![Call { callee: 4, args: vec![Arg::Param(0)] }];
    let mut four = written(4, none, 40, &[5]);
    four.generics = 1;
    four.calls.push(Call { callee: 3, args: vec![Arg::Concrete(0)] });
    model.fns.push(four);
    model.fns.push(written(5, none, 40, &[]));
    let mut world = World::of(model, Tape::seeded(5), 1, Plant::None).unwrap_or_else(|why| panic!("first build: {why}"));
    let old_four = fn_symbol(&world.model, 4, &[0]);
    assert!(world.program.functions().contains_key(&old_four), "4<0> has no row, so the case is not tested");
    world.step(Edit::Retype(0)).unwrap_or_else(|why| panic!("type 0 edited: {why}"));
    assert!(!world.program.functions().contains_key(&old_four), "the old 4<0> kept a row after its type moved");
}

/// The core's framing: every record kind this crate files is proved the record of its key by
/// the core's one check ([`blaze_keyed::proof::hit`]) and gives back the value through the one
/// payload reader.
#[test]
fn records_round_trip_through_the_core_framing() {
    use blaze_format::Aligned;
    use blaze_keyed::proof;

    use crate::keyed;

    let world = World::new(&Tape::seeded(3), Plant::None).unwrap_or_else(|why| panic!("{why}"));
    proof::scope(|snap| {
        for (key, bytes) in &world.store.plans {
            let plan = Plan::decode(bytes).unwrap_or_else(|error| panic!("{error}"));
            let filed = Aligned::copy_from(&keyed::write_plan(key, &plan));
            let holds = proof::hit(snap, keyed::plan_key(key), &filed).unwrap_or_else(|error| panic!("{error}")).holds();
            assert_eq!(keyed::read_plan(&holds, &filed), Ok(plan));
            let other = PlanKey(Digest::of("blaze program test other key 1", &key.0.0));
            assert!(proof::hit(snap, keyed::plan_key(&other), &filed).is_err(), "a plan record proved under another key");
        }
        let (head_key, head, chunks) = keyed::write_program(&world.program);
        let head = Aligned::copy_from(&head);
        let mut filed = BTreeMap::new();
        for (key, bytes) in chunks {
            filed.insert(key.id(), (key, Aligned::copy_from(&bytes)));
        }
        let holds = proof::hit(snap, head_key, &head).unwrap_or_else(|error| panic!("{error}")).holds();
        let read = keyed::read_program(&holds, &head, |key| {
            let (filed_key, bytes) = filed.get(&key.id())?;
            let chunk = proof::hit(snap, *filed_key, bytes).unwrap_or_else(|error| panic!("{error}")).holds();
            Some((chunk, &bytes[..]))
        });
        assert_eq!(read, Ok(world.program.clone()));
    });
}
