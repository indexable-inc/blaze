//! The driver's walk seam (blaze-keyed's `program::Walker`) over this crate's one walk
//! ([`Walk`]) and manifest ([`Program`]): the row rule is the crate's (lib.rs), never a second
//! one here. This module only translates.
//!
//! **Names.** The seam names an instance by its plan's name (`Key<PlanRecord>`, the digest of
//! the front end's ask, its type arguments by content included), the walk by a [`Symbol`]. The
//! walker gives the walk `Symbol { item: ItemId(name), args: ArgsKey::NONE }`: one value per
//! instance, so a kept row's lack of type arguments loses nothing (the name holds them).
//!
//! **What the walk is told.**
//! - A body's key ([`Front::body_key`]) is the instance's name: the seam finds a plan by its
//!   address after the work, never by a key before it, so a plan's content cannot be in the
//!   walk's plan key. What moved comes in as the delta instead: each `moved` instance is an
//!   edited item, and [`Program::seeds`] gives the stale nodes and how each is walked.
//! - Every plan's facts say it is never taken in: codegen's code key is never a callee's
//!   (blaze-keyed's `CodeRecord`), so no code takes a callee in, and a head's targets are the
//!   plan's edges. So the delta says [`Callers::NeverTakeIn`]: an edited instance's callers are
//!   not walked again (their code moves only when their own plan does, and then they are
//!   moved themselves). What brings the callers' revisit back: code that takes a callee in
//!   (facts read from the plan, `Callers::MayTakeIn`), or a [`Reader::code_key`] that reads of
//!   a callee's plan something that can move while the callee's name does not.
//! - A static is an instance with a plan and code like a function's, so it is a function node
//!   to the walk: the walk's own static path ([`Node::Static`]) takes a value from the front end
//!   at once and asks no plan or code, which the seam's static has. The seam is told statics
//!   as statics: a plan's edges say which they are ([`Target`]), and a code ask gives them
//!   apart from the callees, as the kept row keeps them. The missing piece for the walk to hold
//!   them as statics too: a static path in [`Walk`] that asks a plan and code.
//! - The tier is the walk's ([`Walk::tier_of`]): a kept row's comes back from its number
//!   ([`Tier::of_number`]), a code ask carries the one the code is made at, and the reader's code
//!   key holds it.
//! - The kept rows become a [`Program`] ([`Program::of_rows`]): the walk's prior and the seeds'
//!   reverse tables. A row's key is its code key and its plan key its plan's result, so
//!   [`Program::place`] gives exactly the rows whose code came, moved or went.
//! - A code ask carries the span tables its code can name ([`Reader::spans`]): read off the
//!   plan with no database, keyed by the reader as it keys code, sorted by anchor, each once
//!   (one anchor under two keys or owners is refused). With no callee taken in they are the
//!   plan's own anchors; code that takes a callee in adds the taken-in bodies' anchors, which
//!   the plan lists too, so this needs no change then.
//!
//! **Asks.** The walk's plan lookups are the seam's plan asks (the driver finds or makes, so
//! the walk never asks to make one); its code lookups are code asks, given once every target's
//! plan is in (the walker asks a missing one itself), with the code key the [`Reader`] computes
//! from the plan and its targets' plans. A kept instance, moved or not, is asked with no bytes:
//! the driver gives its plan task the ask its kept row holds, or reads its kept plan and runs
//! nothing when nothing it read moved. A new instance is asked with the ask of the edge that
//! names it (a root's is given when the walker is made). One the walk reaches that no kept row
//! holds and no plan this walk read names is refused at once, naming the rule.
//!
//! **The floor, and the cost against it.** A walk must ask the plan of every instance the row
//! rule walks, and of each target whose code key reads it, and the code of every walked
//! instance: one ask each, which this walker gives, and no other (a one-body edit asks that
//! body's plan and code). Past the floor: building the [`Program`] from the kept rows,
//! `O(R log R)` per request, as the seam keeps no reverse tables.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::{BTreeMap, VecDeque};

use blaze_ids::ContentId;
use blaze_keyed::driver::TaskFailed;
use blaze_keyed::kind::{CodeRecord, Key, PlanRecord, UnitManifest};
use blaze_keyed::program::{Kept, Planned, Row, SpanAt, WalkAsk, WalkEnd, Walker};
use blaze_keyed::record::StableId;

use crate::Plant;
use crate::inline::{InlineAttr, OwnFacts, SizeClass};
use crate::key::{ArgsKey, BodyKey, InstanceKey, ItemId, Node, PlanKey, Settings, Symbol, Tier};
use crate::program::{Callers, Delta, FnRow, Program};
use crate::record::{CodeHead, Plan, StaticValue};
use crate::walk::{Answer, Ask, BodyError, Front, Span, Walk, WalkConfig, WalkError};

/// What an edge of a plan refers to: an instance it calls, or a static it reads the address of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Call,
    Static,
}

/// One edge of a plan as the walk follows it: the instance it names, what it is to the plan,
/// the item whose body that instance's plan reads (`None` for one with no body), and the length
/// of its ask's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Edge {
    pub instance: Key<PlanRecord>,
    pub kind: Target,
    pub body: Option<StableId>,
    pub ask_len: u32,
}

/// What the walker reads of the front end's plans: blaze-command implements it over
/// hir-ty-codegen's plan records and `code::code_key`, the simulation over a model.
pub trait Reader {
    /// The edges of `instance`'s plan `payload`, in the plan's order, each instance once, into
    /// `edges`; each edge's ask bytes appended to `asks` in the same order.
    fn edges<E: Extend<Edge>, B: Extend<u8>>(
        &self,
        instance: Key<PlanRecord>,
        payload: &[u8],
        edges: &mut E,
        asks: &mut B,
    ) -> Result<(), TaskFailed>;

    /// The key of `instance`'s code made at `tier` (which the key holds), from its plan and its
    /// targets' plans (in edge order, calls and statics alike). It reads of a target's plan only
    /// what moves with the target's name (its place's kind): see the module doc for why.
    fn code_key(
        &self,
        instance: Key<PlanRecord>,
        tier: Tier,
        plan: &[u8],
        targets: &[&[u8]],
    ) -> Result<Key<CodeRecord>, TaskFailed>;

    /// The span tables `instance`'s code can name, read off its plan `plan` with no database:
    /// the plan's own anchors and those of every body its code takes in, each with its owner
    /// and its key (`SpanTable::key(&SpanTableInputs { tool, anchor })`, the reader holding the
    /// toolchain as it does for [`Self::code_key`]), appended to `spans` in any order, an
    /// anchor more than once allowed: the walker sorts them by anchor and keeps each once.
    fn spans<E: Extend<SpanAt>>(
        &self,
        instance: Key<PlanRecord>,
        plan: &[u8],
        spans: &mut E,
    ) -> Result<(), TaskFailed>;
}

/// A root of a program, as the front end names it.
#[derive(Clone, Copy, Debug)]
pub struct Root<'a> {
    pub instance: Key<PlanRecord>,
    pub ask: &'a [u8],
    pub body: Option<StableId>,
}

/// The walker of one request: each program's roots, and the walk of each begun one.
pub struct ProgramWalker<R> {
    reader: R,
    settings: Settings,
    /// The most asks and answers one walk takes (sized by the driver to its programs).
    fuel: u64,
    plant: Plant,
    roots: Vec<Roots, Global>,
    runs: Vec<Option<Run>, Global>,
}

struct Roots {
    instances: Vec<(ContentId, AskOf), Global>,
    bytes: Vec<u8, Global>,
}

/// Where an instance's ask is in a run's bytes, and the body its plan reads.
#[derive(Clone, Copy, Debug)]
struct AskOf {
    at: u32,
    len: u32,
    body: Option<StableId>,
}

/// A plan this walk read: its result, its payload and its edges, by range.
#[derive(Clone, Copy, Debug)]
struct Held {
    result: ContentId,
    at: u32,
    len: u32,
    edges_at: u32,
    edges_len: u32,
}

/// An ask given (or ready to give) to the driver, by its number.
#[derive(Clone, Copy, Debug)]
enum Pending {
    /// A plan ask; `walk` is the walk's lookup key when the walk asked it too.
    Plan { instance: ContentId, walk: Option<PlanKey> },
    /// A code ask; its targets are `targets[at..at + len]`, the first `call_len` its callees,
    /// and its span tables `spans[spans_at..spans_at + spans_len]`.
    Code {
        symbol: Symbol,
        key: InstanceKey,
        code: Key<CodeRecord>,
        tier: Tier,
        at: u32,
        call_len: u32,
        len: u32,
        spans_at: u32,
        spans_len: u32,
    },
    Answered,
}

/// One program's walk.
struct Run {
    walk: Option<Walk>,
    program: Program,
    pending: Vec<Pending, Global>,
    ready: VecDeque<u32, Global>,
    /// Plan asks given and not answered, by instance.
    inflight: BTreeMap<ContentId, u32, Global>,
    held: BTreeMap<ContentId, Held, Global>,
    payloads: Vec<u8, Global>,
    edges: Vec<Edge, Global>,
    /// The asks of the new instances this walk can name: the roots', and each read edge's.
    asks: BTreeMap<ContentId, AskOf, Global>,
    ask_bytes: Vec<u8, Global>,
    /// The walk's code lookups waiting for a target's plan.
    coding: Vec<(Symbol, InstanceKey), Global>,
    targets: Vec<Key<PlanRecord>, Global>,
    /// The code asks' span tables, each ask's sorted by anchor, each once.
    spans: Vec<SpanAt, Global>,
    /// [`Plant::SpanLeftOut`]: each code ask leaves its last span table out.
    plant: Plant,
    codes: BTreeMap<ContentId, Key<CodeRecord>, Global>,
    rows: Vec<Row, Global>,
    gone: Vec<Key<PlanRecord>, Global>,
    failed: Option<TaskFailed>,
}

/// The walk's front end, as the seam gives it (see the module doc).
struct Names;

impl Front for Names {
    fn body_key(&self, item: &ItemId) -> Result<BodyKey, BodyError> {
        Ok(BodyKey(item.0))
    }

    fn in_workspace(&self, _: &ItemId) -> bool {
        true
    }

    fn static_value(&self, _: &Symbol) -> Option<StaticValue> {
        None
    }

    fn span(&self, _: &ItemId) -> Option<Span> {
        None
    }
}

/// Every plan's facts: never taken in (see the module doc).
const NOT_TAKEN_IN: OwnFacts = OwnFacts { size: SizeClass::Large, attr: InlineAttr::Never, cold: false };

fn symbol_of(instance: ContentId) -> Symbol {
    Symbol { item: ItemId(instance), args: ArgsKey::NONE }
}

fn refused(what: &'static str) -> TaskFailed {
    TaskFailed::Refused { what }
}

/// A walk's stop, as the seam's failure: the rule it broke.
fn walk_failed(error: &WalkError) -> TaskFailed {
    refused(match error {
        WalkError::NoBody { .. } => "the walk reached an instance with no body",
        WalkError::Refused { .. } => "the walk reached an instance whose body fails its check",
        WalkError::NoStatic { .. } => "the walk reached a static with no value",
        WalkError::WrongKey { .. } => "a record answered to the walk under a key it did not ask",
        WalkError::WrongPlan { .. } => "a plan answered to the walk is another instance's",
        WalkError::Unasked { .. } => "an answer to the walk for an instance it did not ask about",
        WalkError::Fuel { .. } => "the walk took more steps than its fuel without ending",
        WalkError::Unfinished { .. } => "the walk was ended with an instance still waiting",
        WalkError::NoPlan { .. } => "an instance was keyed without its plan in hand",
    })
}

impl<R: Reader> ProgramWalker<R> {
    pub fn new(reader: R, settings: Settings, fuel: u64, plant: Plant) -> ProgramWalker<R> {
        ProgramWalker { reader, settings, fuel, plant, roots: Vec::new_in(Global), runs: Vec::new_in(Global) }
    }

    /// Adds a program by its roots; its number is the driver's index of it.
    pub fn add_program(&mut self, roots: &[Root<'_>]) -> u32 {
        let mut out = Roots { instances: Vec::with_capacity_in(roots.len(), Global), bytes: Vec::new_in(Global) };
        for root in roots {
            let at = out.bytes.len() as u32;
            out.bytes.extend_from_slice(root.ask);
            out.instances.push((root.instance.id(), AskOf { at, len: root.ask.len() as u32, body: root.body }));
        }
        self.roots.push(out);
        self.runs.push(None);
        (self.roots.len() - 1) as u32
    }

    fn run(&mut self, program: u32) -> Result<&mut Run, TaskFailed> {
        match self.runs.get_mut(program as usize) {
            Some(Some(run)) => Ok(run),
            _ => Err(refused("an answer for a program this walker has not begun")),
        }
    }
}

impl Run {
    /// Whether the kept manifest holds a row of `instance`: asked by its kept ask.
    fn is_kept(&self, instance: ContentId) -> bool {
        self.program.functions().contains_key(&symbol_of(instance))
    }

    fn issue(&mut self, instance: ContentId, walk: Option<PlanKey>) -> Result<(), TaskFailed> {
        if !self.is_kept(instance) && !self.asks.contains_key(&instance) {
            return Err(refused(
                "the walk reached an instance no kept row holds and no plan it read names, so no ask is known for it",
            ));
        }
        let ask = self.pending.len() as u32;
        self.pending.push(Pending::Plan { instance, walk });
        self.inflight.insert(instance, ask);
        self.ready.push_back(ask);
        Ok(())
    }

    /// The walk's lookup of `symbol`'s plan: answered at once from a plan held, joined to an
    /// ask in flight, or asked.
    fn walk_plan(&mut self, symbol: Symbol, key: PlanKey) -> Result<(), TaskFailed> {
        let instance = symbol.item.0;
        if self.held.contains_key(&instance) {
            return self.answer_walk_plan(symbol, key);
        }
        match self.inflight.get(&instance).copied() {
            Some(ask) => {
                self.pending[ask as usize] = Pending::Plan { instance, walk: Some(key) };
                Ok(())
            }
            None => self.issue(instance, Some(key)),
        }
    }

    fn edges_of(&self, held: &Held) -> &[Edge] {
        &self.edges[held.edges_at as usize..(held.edges_at + held.edges_len) as usize]
    }

    /// A plan's targets, calls and statics alike, as the walk's calls (see the module doc).
    fn calls_of(&self, held: &Held) -> Vec<Symbol, Global> {
        let edges = self.edges_of(held);
        let mut calls = Vec::with_capacity_in(edges.len(), Global);
        calls.extend(edges.iter().map(|edge| symbol_of(edge.instance.id())));
        calls
    }

    fn answer_walk_plan(&mut self, symbol: Symbol, key: PlanKey) -> Result<(), TaskFailed> {
        let Some(held) = self.held.get(&symbol.item.0).copied() else {
            return Err(refused("the walk's plan answered before it was read"));
        };
        let plan = Plan { symbol, calls: self.calls_of(&held), facts: NOT_TAKEN_IN };
        let Some(walk) = self.walk.as_mut() else { return Err(refused("a plan answered to an ended walk")) };
        walk.answer(Answer::Plan { symbol, key, plan: Some(plan) }, &Names, &self.program)
            .map_err(|error| walk_failed(&error))
    }

    /// Gives the code asks whose targets' plans are all in, asking a target's plan not asked
    /// yet.
    fn release<R: Reader>(&mut self, reader: &R) -> Result<(), TaskFailed> {
        let coding = std::mem::replace(&mut self.coding, Vec::new_in(Global));
        for (symbol, key) in coding {
            let instance = symbol.item.0;
            let Some(held) = self.held.get(&instance).copied() else {
                return Err(refused("the walk asked for code before its instance's plan was in"));
            };
            let mut missing = false;
            for at in held.edges_at..held.edges_at + held.edges_len {
                let target = self.edges[at as usize].instance.id();
                if self.held.contains_key(&target) {
                    continue;
                }
                missing = true;
                if !self.inflight.contains_key(&target) {
                    self.issue(target, None)?;
                }
            }
            if missing {
                self.coding.push((symbol, key));
                continue;
            }
            let Some(walk) = self.walk.as_ref() else { return Err(refused("code asked of an ended walk")) };
            let tier = walk.tier_of(&symbol, &Names);
            let code = {
                let mut plans = Vec::with_capacity_in(held.edges_len as usize, Global);
                for edge in self.edges_of(&held) {
                    let target = &self.held[&edge.instance.id()];
                    plans.push(&self.payloads[target.at as usize..(target.at + target.len) as usize]);
                }
                let own = &self.payloads[held.at as usize..(held.at + held.len) as usize];
                reader.code_key(Key::from_id(instance), tier, own, &plans)?
            };
            let at = self.targets.len();
            let mut call_len = 0;
            for kind in [Target::Call, Target::Static] {
                let from = self.targets.len();
                for edge_at in held.edges_at..held.edges_at + held.edges_len {
                    let edge = self.edges[edge_at as usize];
                    if edge.kind == kind {
                        self.targets.push(edge.instance);
                    }
                }
                self.targets[from..].sort_unstable();
                if kind == Target::Call {
                    call_len = (self.targets.len() - at) as u32;
                }
            }
            let len = (self.targets.len() - at) as u32;
            let spans_at = self.spans.len();
            let own = &self.payloads[held.at as usize..(held.at + held.len) as usize];
            reader.spans(Key::from_id(instance), own, &mut self.spans)?;
            self.spans[spans_at..].sort_unstable_by_key(|span| span.anchor);
            // Each anchor once, in place: a repeat equal in key and owner is dropped, one that
            // differs is refused (one table, two keys).
            let mut kept = spans_at;
            for from in spans_at..self.spans.len() {
                let span = self.spans[from];
                if kept > spans_at && self.spans[kept - 1].anchor == span.anchor {
                    if self.spans[kept - 1] != span {
                        return Err(refused("a plan names one span anchor under two keys or owners"));
                    }
                    continue;
                }
                self.spans[kept] = span;
                kept += 1;
            }
            if self.plant == Plant::SpanLeftOut && kept > spans_at {
                kept -= 1;
            }
            self.spans.truncate(kept);
            let spans_len = (kept - spans_at) as u32;
            let ask = self.pending.len() as u32;
            self.pending.push(Pending::Code {
                symbol,
                key,
                code,
                tier,
                at: at as u32,
                call_len,
                len,
                spans_at: spans_at as u32,
                spans_len,
            });
            self.ready.push_back(ask);
        }
        Ok(())
    }

    /// Works the walk until an ask is ready or it has nothing to ask.
    fn pump<R: Reader>(&mut self, reader: &R) -> Result<(), TaskFailed> {
        while self.ready.is_empty() {
            let Some(walk) = self.walk.as_mut() else { return Ok(()) };
            let Some(ask) = walk.next_ask(&Names, &self.program).map_err(|error| walk_failed(&error))? else {
                return Ok(());
            };
            match ask {
                Ask::LookupPlan { symbol, key } => self.walk_plan(symbol, key)?,
                Ask::LookupCode { symbol, key } => {
                    self.coding.push((symbol, key));
                    self.release(reader)?;
                }
                Ask::MakePlan { .. } | Ask::MakeCode { .. } => {
                    return Err(refused("the walk asked to make a record the driver finds or makes"));
                }
                Ask::Route { .. } => return Err(refused("a walk of one shard routed a node")),
            }
        }
        Ok(())
    }

    fn planned<R: Reader>(&mut self, reader: &R, ask: u32, plan: Planned<'_>) -> Result<(), TaskFailed> {
        let Some(Pending::Plan { instance, walk }) = self.pending.get(ask as usize).copied() else {
            return Err(refused("a plan answered to an ask that is not an open plan ask"));
        };
        if plan.instance.id() != instance {
            return Err(refused("a plan answered under another instance than its ask's"));
        }
        self.pending[ask as usize] = Pending::Answered;
        self.inflight.remove(&instance);
        let at = self.payloads.len() as u32;
        self.payloads.extend_from_slice(plan.payload);
        let edges_at = self.edges.len() as u32;
        let mut asks_at = self.ask_bytes.len() as u32;
        reader.edges(plan.instance, plan.payload, &mut self.edges, &mut self.ask_bytes)?;
        let edges_len = self.edges.len() as u32 - edges_at;
        for edge in &self.edges[edges_at as usize..] {
            self.asks.entry(edge.instance.id()).or_insert(AskOf { at: asks_at, len: edge.ask_len, body: edge.body });
            asks_at += edge.ask_len;
        }
        if asks_at as usize != self.ask_bytes.len() {
            return Err(refused("a plan's edges' ask lengths are not the bytes the reader gave"));
        }
        let held = Held { result: plan.result, at, len: plan.payload.len() as u32, edges_at, edges_len };
        self.held.insert(instance, held);
        if let Some(key) = walk {
            self.answer_walk_plan(symbol_of(instance), key)?;
        }
        self.release(reader)
    }

    fn coded(&mut self, ask: u32, code: Key<CodeRecord>) -> Result<(), TaskFailed> {
        let Some(Pending::Code { symbol, key, code: asked, .. }) = self.pending.get(ask as usize).copied() else {
            return Err(refused("code answered to an ask that is not an open code ask"));
        };
        if code != asked {
            return Err(refused("code answered under another key than the walker asked"));
        }
        self.pending[ask as usize] = Pending::Answered;
        self.codes.insert(symbol.item.0, code);
        let Some(held) = self.held.get(&symbol.item.0).copied() else {
            return Err(refused("code answered for an instance whose plan is not in"));
        };
        let head = CodeHead { calls: self.calls_of(&held), statics: Vec::new_in(Global) };
        let Some(walk) = self.walk.as_mut() else { return Err(refused("code answered to an ended walk")) };
        walk.answer(Answer::Code { symbol, key, head: Some(head) }, &Names, &self.program)
            .map_err(|error| walk_failed(&error))
    }

    fn end(&mut self) -> Result<(), TaskFailed> {
        if !self.ready.is_empty() || !self.coding.is_empty() || !self.inflight.is_empty() {
            return Err(refused("the walk was ended with an ask not given or not answered"));
        }
        let Some(walk) = self.walk.take() else { return Err(refused("a walk ended twice")) };
        let ended = walk.end().map_err(|error| walk_failed(&error))?;
        let mut walked = ended.walked;
        for entry in &mut walked {
            let instance = entry.node.symbol().item.0;
            let (Some(code), Some(held)) = (self.codes.get(&instance), self.held.get(&instance)) else {
                return Err(refused("the walk gave a row whose code or plan this walker was not given"));
            };
            entry.key = code.id();
            entry.plan = PlanKey(held.result);
        }
        let change = self
            .program
            .place(&walked, &ended.unreached, &[])
            .map_err(|_| refused("the program's manifest refused the walk's rows"))?;
        self.rows.extend(walked.iter().map(|entry| Row {
            instance: Key::from_id(entry.node.symbol().item.0),
            code: Key::from_id(entry.key),
        }));
        self.rows.sort_unstable();
        for row in &change.went {
            let symbol = row.node.symbol();
            if !self.program.functions().contains_key(&symbol) {
                self.gone.push(Key::from_id(symbol.item.0));
            }
        }
        self.gone.sort_unstable();
        self.gone.dedup();
        Ok(())
    }
}

impl<R: Reader> Walker for ProgramWalker<R> {
    // The roots were given by `add_program`: the units' manifests are the caller's to list
    // them from (blaze-command's `run::Rooted`).
    fn begin(&mut self, program: u32, kept: Kept<'_>, moved: &[Key<PlanRecord>], _units: &[Key<UnitManifest>]) -> Result<(), TaskFailed> {
        let Some(roots) = self.roots.get(program as usize) else {
            return Err(refused("a program begun that the walker was not given"));
        };
        let mut nodes = Vec::with_capacity_in(roots.instances.len(), Global);
        nodes.extend(roots.instances.iter().map(|(instance, _)| Node::Function(symbol_of(*instance))));
        let mut admitted = Vec::with_capacity_in(kept.rows.len(), Global);
        for row in kept.rows {
            let Some(tier) = Tier::of_number(row.tier) else {
                return Err(refused("a kept row's tier is a number no tier has"));
            };
            // Callees, then statics: both the walk's calls (see the module doc).
            let targets = kept.targets_of(row);
            let mut calls = Vec::with_capacity_in(targets.len(), Global);
            calls.extend(targets.iter().map(|target| symbol_of(*target)));
            admitted.push((
                symbol_of(row.instance),
                FnRow {
                    key: InstanceKey(row.code),
                    tier,
                    since: 0,
                    calls,
                    statics: Vec::new_in(Global),
                    plan: PlanKey(row.result),
                    inlined: Vec::new_in(Global),
                },
            ));
        }
        let prior = Program::of_rows(self.settings, &nodes, admitted, Plant::None);
        let mut delta = Delta { callers: Callers::NeverTakeIn, ..Delta::default() };
        if self.plant != Plant::DeltaKeepsStale {
            delta.edited.extend(moved.iter().map(|instance| ItemId(instance.id())));
        }
        let seeds = prior.seeds(&delta);
        let mut walk = Walk::new(WalkConfig {
            settings: self.settings,
            shard: 0,
            shards: 1,
            fuel: self.fuel,
            plant: Plant::None,
            new_at: Tier::Edit,
        });
        for (node, seed) in seeds {
            walk.reach(node, seed, &prior);
        }
        let mut asks = BTreeMap::new_in(Global);
        asks.extend(roots.instances.iter().copied());
        let mut ask_bytes = Vec::with_capacity_in(roots.bytes.len(), Global);
        ask_bytes.extend_from_slice(&roots.bytes);
        let run = Run {
            walk: Some(walk),
            program: prior,
            pending: Vec::new_in(Global),
            ready: VecDeque::new_in(Global),
            inflight: BTreeMap::new_in(Global),
            held: BTreeMap::new_in(Global),
            payloads: Vec::new_in(Global),
            edges: Vec::new_in(Global),
            asks,
            ask_bytes,
            coding: Vec::new_in(Global),
            targets: Vec::new_in(Global),
            spans: Vec::new_in(Global),
            plant: self.plant,
            codes: BTreeMap::new_in(Global),
            rows: Vec::new_in(Global),
            gone: Vec::new_in(Global),
            failed: None,
        };
        self.runs[program as usize] = Some(run);
        Ok(())
    }

    fn ask(&mut self, program: u32) -> Option<WalkAsk<'_>> {
        let reader = &self.reader;
        let run = self.runs.get_mut(program as usize)?.as_mut()?;
        if run.failed.is_some() {
            return None;
        }
        if let Err(failed) = run.pump(reader) {
            run.failed = Some(failed);
            return None;
        }
        let ask = *run.ready.front()?;
        let pending = run.pending[ask as usize];
        // A plan ask: a kept instance by its kept ask (no bytes), a new one by the ask of the
        // edge or root that named it (`issue` refused one with neither).
        let plan = match pending {
            Pending::Plan { instance, .. } => {
                let of = run.asks.get(&instance).copied();
                let kept = run.is_kept(instance);
                if !kept && of.is_none() {
                    run.failed = Some(refused("a plan ask of a new instance with no ask known"));
                    return None;
                }
                Some((instance, kept, of))
            }
            Pending::Code { .. } => None,
            Pending::Answered => {
                run.failed = Some(refused("an ask answered before it was given"));
                return None;
            }
        };
        run.ready.pop_front();
        let run = &*run;
        Some(match (pending, plan) {
            (Pending::Plan { .. }, Some((instance, kept, of))) => WalkAsk::Plan {
                ask,
                instance: Key::from_id(instance),
                bytes: match (kept, of) {
                    (false, Some(of)) => Some(&run.ask_bytes[of.at as usize..(of.at + of.len) as usize]),
                    _ => None,
                },
                body: of.and_then(|of| of.body),
            },
            (Pending::Code { symbol, code, tier, at, call_len, len, spans_at, spans_len, .. }, _) => {
                let targets = &run.targets[at as usize..(at + len) as usize];
                let (callees, statics) = targets.split_at(call_len as usize);
                let spans = &run.spans[spans_at as usize..(spans_at + spans_len) as usize];
                WalkAsk::Code {
                    ask,
                    instance: Key::from_id(symbol.item.0),
                    key: code,
                    tier: tier.number(),
                    callees,
                    statics,
                    spans,
                }
            }
            // Matched above: a plan ask has its pair, an answered one returned.
            _ => return None,
        })
    }

    fn planned(&mut self, program: u32, ask: u32, plan: Planned<'_>) -> Result<(), TaskFailed> {
        let reader = &self.reader;
        let run = match self.runs.get_mut(program as usize) {
            Some(Some(run)) => run,
            _ => return Err(refused("a plan answered for a program this walker has not begun")),
        };
        if let Some(failed) = run.failed.take() {
            return Err(failed);
        }
        run.planned(reader, ask, plan)
    }

    fn coded(&mut self, program: u32, ask: u32, code: Key<CodeRecord>) -> Result<(), TaskFailed> {
        let run = self.run(program)?;
        if let Some(failed) = run.failed.take() {
            return Err(failed);
        }
        run.coded(ask, code)
    }

    fn end(&mut self, program: u32) -> Result<WalkEnd<'_>, TaskFailed> {
        let run = self.run(program)?;
        if let Some(failed) = run.failed.take() {
            return Err(failed);
        }
        run.end()?;
        Ok(WalkEnd { rows: &run.rows, gone: &run.gone })
    }
}
