//! The monomorphization walk, streaming: a sans-IO state machine that takes roots in and gives
//! out, one instance at a time, what the program holds ([`Walked`]), asking its driver for
//! records ([`Ask`]) and taking the answers ([`Answer`]).
//!
//! **What it does per function instance.** It asks the front end for the body's key, which is
//! also where checking happens: checking is demand-driven like code, so `run` and `build` check
//! exactly the bodies the program reaches, and a reached body that fails its check stops the
//! walk with [`WalkError::Refused`] naming it (`check` is the command that checks everything
//! asked, outside this crate). Then it asks for the instance's plan by its plan key (the body's
//! key, the type arguments, the settings but the tier: no other instance needed); a miss asks
//! for the plan to be made. With the plans of the instance's inline closure in hand it computes
//! the instance key ([`crate::inline`]) and asks for the code record by it; a hit yields the
//! code's head (the callees and statics after inlining); a miss asks for the code to be made,
//! with the inline tree to take in. The instance's callees and statics then go on the frontier.
//! A callee that is only ever inlined is never placed, so never coded alone.
//!
//! **Tiers.** An instance outside the workspace (a dependency, anything from the shared bucket)
//! is made at [`Tier::Release`] always. A workspace instance is made at [`Tier::Edit`] unless it
//! was seeded as a tier-up ([`Seed::TierUp`]), when it is made at `Release` from the same plan
//! and body key; the callees it reaches that the program does not hold yet stay at `Edit`. A
//! tier-up whose every referrer tiers up too is only promoted ([`Seed::Promote`]): it is made
//! when a walked referrer still calls it, and never when every one takes it in, so a tier-up
//! makes no record the program will not hold (work once: an equal key is never made again,
//! and nothing is made to be thrown away).
//!
//! **What it holds.** The frontier, the set of nodes seen (with each one's phase), the plans it
//! read (the calls only of instances some caller may inline, so the plan cache is the inline
//! closure's, not the program's) and the instance keys made. Per instance seen: the node (65
//! bytes) and its phase in the seen map (about 100 bytes with the map's overhead), its key in
//! [`Keys`] (32 bytes, with its direct inlinees: about 60 bytes and 64 per inlinee), and its plan
//! entry (about 80 bytes; plus 64 per call for an inlinable one). About 250 bytes an instance;
//! tempo's 4.2 M bodies would be about 1 GiB at most, against 34.4 GiB held by one codegen task
//! today. No code, no MIR and no body is held: the work is in the records. A walk is one
//! program's or one edit's; its memory goes when it ends.
//!
//! **Generics are shared by content.** An instance is its symbol (the item and its type
//! arguments' content keys), so two crates' walks that reach `Vec<u8>::push` reach one symbol,
//! one key and one record.
//!
//! **The floors.** A cold build of a program of `I` instances, `H` of them already in the
//! store, does `I` code lookups and `I - H` makes (plus a plan lookup per instance and per
//! inlinable callee, microseconds each against a make's milliseconds). A body edit of an
//! instance inlined nowhere makes exactly 1 code record; one inlined into `K - 1` callers makes
//! `K` (every key that moved and no other). An edit walk is seeded by
//! [`crate::program::Program::seeds`] and stops at every node the prior program holds and the
//! edit did not touch ([`Prior`]), so it reads the edit's neighbourhood, not the program.
//!
//! **128 cores.** A walk is one shard of `shards`: it owns the symbols whose shard is its own
//! ([`shard_of`]), and hands every other node it reaches to its owner ([`Ask::Route`]). Each
//! shard has its own seen set and frontier: no lock, no shared map. Shards read every plan they
//! need themselves (a plan lookup is cheap, and a plan made twice is the same bytes put twice in
//! a write-once store). The asks are item-grain tasks for a pool; a one-instance edit is one
//! shard, run on the calling thread.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::VecDeque;
use std::fmt;

use crate::Plant;
use crate::inline::{InlineEdge, Keys, OwnFacts, PlanRef, PlanView, Resolution, inlines};
use crate::key::{BodyKey, InstanceKey, ItemId, Node, PlanKey, Settings, SettingsKey, Symbol, Tier, plan_key};
use crate::record::{CodeHead, Plan, StaticValue};
use crate::codec::Digest;

/// Why the front end gives no body key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BodyError {
    /// No such item is in the program.
    Missing,
    /// The body fails its check; `diagnostics` is the key of the diagnostics record.
    Refused { diagnostics: Digest },
}

/// What a walk reads of the front end, each a lookup of something already computed.
pub trait Front {
    /// The key of `item`'s body (decision 2a: its text with positions item-relative, plus
    /// what the names it mentions resolve to), once the body passed its check.
    fn body_key(&self, item: &ItemId) -> Result<BodyKey, BodyError>;
    /// Whether `item` is the workspace's own code (else a dependency's, always optimized).
    fn in_workspace(&self, item: &ItemId) -> bool;
    /// The initial value of the static `symbol`; `None` when there is no such static.
    fn static_value(&self, symbol: &Symbol) -> Option<StaticValue>;
    /// Where `item` starts: its file and its first line.
    fn span(&self, item: &ItemId) -> Option<Span>;
}

/// Where an item starts.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Span {
    pub file: Vec<u8, Global>,
    pub line: u32,
}

/// What a program held before this walk: a node it holds that the edit did not touch is not
/// walked again.
pub trait Prior {
    fn holds(&self, node: &Node) -> bool;
    /// The tier the prior program holds `symbol` at.
    fn tier(&self, _symbol: &Symbol) -> Option<Tier> {
        None
    }
}

/// No prior program: a whole walk.
pub struct Fresh;

impl Prior for Fresh {
    fn holds(&self, _: &Node) -> bool {
        false
    }
}

/// How a node comes into a walk.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Seed {
    /// Reached from a node walked: not walked when the prior program holds it.
    Reached,
    /// A root or an instance of an edited item: walked whatever the prior holds, at the fast
    /// tier if it is the workspace's.
    Edited,
    /// A function whose body is unchanged but whose key may have moved (a callee's or an
    /// inlinee's did): walked whatever the prior holds, at the tier the prior holds it at.
    Revisit,
    /// A workspace instance to make again at the top tier, its body unchanged.
    TierUp,
    /// A workspace instance to make at the top tier only if this walk reaches it: every node
    /// referring to it tiers up too, and may take it in rather than call it, so it is not
    /// walked for its own sake (an instance taken into all its callers is never coded alone).
    /// Given to its owning shard before any answer, like every seed.
    Promote,
}

/// A walk's settings.
#[derive(Clone, Copy, Debug)]
pub struct WalkConfig {
    /// The settings that change code; the tier in it is not read (it is chosen per instance).
    pub settings: Settings,
    /// This walk's shard, of `shards`.
    pub shard: u32,
    pub shards: u32,
    /// The most asks and answers the walk takes before it stops with [`WalkError::Fuel`]: a
    /// walk that does not end is a bug named, never a spin.
    pub fuel: u64,
    pub plant: Plant,
}

/// The shard of `symbol` among `shards`: a function of the symbol alone.
pub fn shard_of(symbol: &Symbol, shards: u32) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&symbol.item.0.0[..4]);
    let item = u32::from_le_bytes(word);
    word.copy_from_slice(&symbol.args.0.0[..4]);
    let args = u32::from_le_bytes(word);
    (item ^ args.rotate_left(16)) % shards.max(1)
}

/// What a walk asks of its driver.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ask {
    /// `node` is shard `shard`'s: hand it to that walk's [`Walk::reach`], with `seed`.
    Route { node: Node, shard: u32, seed: Seed },
    /// Look up the plan record `key`; answer [`Answer::Plan`].
    LookupPlan { symbol: Symbol, key: PlanKey },
    /// Make the plan of `symbol` (from its body), put it under `key`; answer [`Answer::Plan`].
    MakePlan { symbol: Symbol, key: PlanKey },
    /// Look up the code record `key`; answer [`Answer::Code`] with its head or nothing.
    LookupCode { symbol: Symbol, key: InstanceKey },
    /// Make the code of `symbol` at `tier`, taking in `inline` (every edge of its inline tree,
    /// each callee by its key), put it under `key`; answer [`Answer::Code`] with its head.
    MakeCode {
        symbol: Symbol,
        key: InstanceKey,
        tier: Tier,
        plan: PlanKey,
        inline: Vec<InlineEdge, Global>,
    },
}

/// A driver's answer to an [`Ask`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    Plan { symbol: Symbol, key: PlanKey, plan: Option<Plan> },
    Code { symbol: Symbol, key: InstanceKey, head: Option<CodeHead> },
}

/// What the program holds of one node: its key (an instance key, or a static's value key), its
/// tier (a static's is `Release`), the functions and statics it refers to, and (for a function)
/// every instance it took in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Walked {
    pub node: Node,
    pub key: Digest,
    pub tier: Tier,
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
    pub inlined: Vec<Symbol, Global>,
}

/// Why a walk stopped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WalkError {
    /// The front end has no body for a function the program reaches.
    NoBody { symbol: Symbol },
    /// A body the program reaches fails its check.
    Refused { symbol: Symbol, diagnostics: Digest },
    /// The front end has no value for a static the program reaches.
    NoStatic { symbol: Symbol },
    /// A record answered under a key other than the one asked.
    WrongKey { symbol: Symbol },
    /// A plan record of another instance than the one asked.
    WrongPlan { symbol: Symbol },
    /// An answer to nothing this walk asked.
    Unasked { symbol: Symbol },
    /// The walk took `steps` asks and answers without ending.
    Fuel { steps: u64 },
    /// The walk was ended with `symbol` still waiting.
    Unfinished { symbol: Symbol },
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::NoBody { symbol } => {
                write!(f, "the program reaches function {symbol}, which the front end has no body for")
            }
            WalkError::Refused { symbol, diagnostics } => write!(
                f,
                "the program reaches function {symbol}, whose body fails its check (diagnostics {diagnostics})"
            ),
            WalkError::NoStatic { symbol } => {
                write!(f, "the program reaches static {symbol}, which the front end has no value for")
            }
            WalkError::WrongKey { symbol } => {
                write!(f, "a record of {symbol} answered under a key the walk did not ask")
            }
            WalkError::WrongPlan { symbol } => {
                write!(f, "the plan answered for {symbol} is another instance's")
            }
            WalkError::Unasked { symbol } => {
                write!(f, "an answer for {symbol}, which the walk did not ask about")
            }
            WalkError::Fuel { steps } => {
                write!(f, "the walk took {steps} steps without ending (its fuel bound)")
            }
            WalkError::Unfinished { symbol } => {
                write!(f, "the walk was ended with {symbol} still waiting for a record")
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Waiting for plans before its key.
    Resolving,
    /// Its code record is asked for under this key.
    Coding(InstanceKey),
    Done,
}

struct PlanEntry {
    key: PlanKey,
    facts: OwnFacts,
    /// Kept while some caller may inline it, or until it is walked.
    calls: Option<Vec<Symbol, Global>>,
}

struct Plans(blaze_hash::HashMap<Symbol, PlanEntry, Global>);

impl PlanView for Plans {
    fn plan(&self, symbol: &Symbol) -> Option<PlanRef<'_>> {
        let entry = self.0.get(symbol)?;
        Some(PlanRef { key: entry.key, facts: entry.facts, calls: entry.calls.as_deref() })
    }
}

/// One shard of a streaming walk. Drive it: [`Walk::reach`] the roots, then until
/// [`Walk::is_idle`], take [`Walk::next_ask`]s, answer each with [`Walk::answer`] in any order,
/// and [`Walk::take_walked`] what is done.
pub struct Walk {
    config: WalkConfig,
    plan_settings: SettingsKey,
    /// The instance keys at `Edit` and at `Release`.
    keys: [Keys; 2],
    plans: Plans,
    /// Plans asked for and not yet answered.
    asked: blaze_hash::HashSet<Symbol, Global>,
    seen: blaze_hash::HashMap<Node, Phase, Global>,
    /// Workspace instances seeded to tier up ([`Seed::TierUp`] and [`Seed::Promote`]).
    promoted: blaze_hash::HashSet<Symbol, Global>,
    /// Functions reached and passed over because the prior program holds them.
    passed: blaze_hash::HashSet<Symbol, Global>,
    /// Revisited workspace instances, at the tier the prior program holds them at.
    kept: blaze_hash::HashMap<Symbol, Tier, Global>,
    /// The nodes to start on.
    frontier: VecDeque<Node, Global>,
    /// The instances waiting for each plan.
    waiting: blaze_hash::HashMap<Symbol, Vec<Symbol, Global>, Global>,
    asks: VecDeque<Ask, Global>,
    walked: Vec<Walked, Global>,
    outstanding: usize,
    steps: u64,
    /// Counts nodes done, for the seen-set plant.
    done: u64,
}

fn tier_index(tier: Tier) -> usize {
    match tier {
        Tier::Edit => 0,
        Tier::Release => 1,
    }
}

impl Walk {
    pub fn new(config: WalkConfig) -> Walk {
        Walk {
            plan_settings: config.settings.plan_key(),
            keys: [Keys::new(Tier::Edit, config.plant), Keys::new(Tier::Release, config.plant)],
            plans: Plans(blaze_hash::HashMap::new_in(Global)),
            asked: blaze_hash::HashSet::new_in(Global),
            seen: blaze_hash::HashMap::new_in(Global),
            promoted: blaze_hash::HashSet::new_in(Global),
            passed: blaze_hash::HashSet::new_in(Global),
            kept: blaze_hash::HashMap::new_in(Global),
            frontier: VecDeque::new_in(Global),
            waiting: blaze_hash::HashMap::new_in(Global),
            asks: VecDeque::new_in(Global),
            walked: Vec::new_in(Global),
            outstanding: 0,
            steps: 0,
            done: 0,
            config,
        }
    }

    /// The shard of `node` in this walk's set.
    pub fn owner(&self, node: &Node) -> u32 {
        shard_of(&node.symbol(), self.config.shards)
    }

    /// Takes `node` in: a root, a seed of an edit or a tier-up, or a node another shard routed
    /// here.
    pub fn reach(&mut self, node: Node, seed: Seed, prior: &impl Prior) {
        let shard = self.owner(&node);
        if shard != self.config.shard {
            self.asks.push_back(Ask::Route { node, shard, seed });
            return;
        }
        if seed == Seed::Promote {
            if let Node::Function(symbol) = node {
                self.promoted.insert(symbol);
                // Reached before the promotion came: it was passed over as held; walk it now.
                if self.passed.remove(&symbol) {
                    self.seen.remove(&node);
                    self.reach(node, Seed::Reached, prior);
                }
            }
            return;
        }
        // A seed for a node passed over as held (reached before its seed came): walk it.
        if let (true, Node::Function(symbol)) = (seed != Seed::Reached, node) {
            if self.passed.remove(&symbol) {
                self.seen.remove(&node);
            }
        }
        if self.seen.contains_key(&node) {
            return;
        }
        let promoted = matches!(node, Node::Function(symbol) if self.promoted.contains(&symbol));
        if seed == Seed::Reached && prior.holds(&node) && !promoted {
            if let Node::Function(symbol) = node {
                self.passed.insert(symbol);
            }
            self.seen.insert(node, Phase::Done);
            return;
        }
        match (seed, node) {
            (Seed::TierUp, Node::Function(symbol)) => {
                self.promoted.insert(symbol);
            }
            (Seed::Revisit, Node::Function(symbol)) => {
                if let Some(tier) = prior.tier(&symbol) {
                    self.kept.insert(symbol, tier);
                }
            }
            _ => {}
        }
        self.seen.insert(node, Phase::Resolving);
        self.frontier.push_back(node);
    }

    /// The tier `symbol` is made at in this walk.
    pub fn tier_of(&self, symbol: &Symbol, front: &impl Front) -> Tier {
        if !front.in_workspace(&symbol.item) || self.promoted.contains(symbol) {
            return Tier::Release;
        }
        self.kept.get(symbol).copied().unwrap_or(Tier::Edit)
    }

    /// Works the frontier until it needs a record, then gives the next ask; `None` when it
    /// has nothing to ask until an answer comes.
    pub fn next_ask(&mut self, front: &impl Front, prior: &impl Prior) -> Result<Option<Ask>, WalkError> {
        loop {
            if let Some(ask) = self.asks.pop_front() {
                self.spend()?;
                if !matches!(ask, Ask::Route { .. }) {
                    self.outstanding += 1;
                }
                return Ok(Some(ask));
            }
            let Some(node) = self.frontier.pop_front() else { return Ok(None) };
            self.spend()?;
            match node {
                Node::Static(symbol) => self.walk_static(symbol, front, prior)?,
                Node::Function(symbol) => self.resolve(symbol, front)?,
            }
        }
    }

    /// Takes the answer to an ask this walk gave.
    pub fn answer(&mut self, answer: Answer, front: &impl Front, prior: &impl Prior) -> Result<(), WalkError> {
        self.spend()?;
        match answer {
            Answer::Plan { symbol, key, plan } => {
                if !self.asked.contains(&symbol) {
                    return Err(WalkError::Unasked { symbol });
                }
                if key != self.plan_key(&symbol, front)? {
                    return Err(WalkError::WrongKey { symbol });
                }
                self.outstanding -= 1;
                let Some(plan) = plan else {
                    self.asks.push_back(Ask::MakePlan { symbol, key });
                    return Ok(());
                };
                if plan.symbol != symbol {
                    return Err(WalkError::WrongPlan { symbol });
                }
                self.asked.remove(&symbol);
                self.plans.0.insert(symbol, PlanEntry { key, facts: plan.facts, calls: Some(plan.calls) });
                if let Some(waiters) = self.waiting.remove(&symbol) {
                    for waiter in waiters {
                        if self.seen.get(&Node::Function(waiter)) == Some(&Phase::Resolving) {
                            self.frontier.push_back(Node::Function(waiter));
                        }
                    }
                }
                Ok(())
            }
            Answer::Code { symbol, key, head } => {
                let node = Node::Function(symbol);
                match self.seen.get(&node) {
                    Some(Phase::Coding(asked)) if *asked == key => {}
                    Some(Phase::Coding(_)) => return Err(WalkError::WrongKey { symbol }),
                    _ => return Err(WalkError::Unasked { symbol }),
                }
                self.outstanding -= 1;
                let tier = self.tier_of(&symbol, front);
                let keys = &self.keys[tier_index(tier)];
                let Some(head) = head else {
                    let Some(plan) = self.plans.0.get(&symbol) else {
                        return Err(WalkError::Unasked { symbol });
                    };
                    let inline = keys.inline_tree(&symbol);
                    self.asks.push_back(Ask::MakeCode { symbol, key, tier, plan: plan.key, inline });
                    return Ok(());
                };
                let inlined = keys.took_in_closure(&symbol);
                self.finish(node);
                if let Some(entry) = self.plans.0.get_mut(&symbol) {
                    // `Release` inlines everything `Edit` does and more.
                    if !inlines(Tier::Release, &entry.facts) {
                        entry.calls = None;
                    }
                }
                for callee in &head.calls {
                    self.reach(Node::Function(*callee), Seed::Reached, prior);
                }
                for target in &head.statics {
                    self.reach(Node::Static(*target), Seed::Reached, prior);
                }
                self.walked.push(Walked {
                    node,
                    key: key.0,
                    tier,
                    calls: head.calls,
                    statics: head.statics,
                    inlined,
                });
                Ok(())
            }
        }
    }

    /// Whether the walk has nothing left to do and nothing asked.
    pub fn is_idle(&self) -> bool {
        self.asks.is_empty() && self.frontier.is_empty() && self.outstanding == 0
    }

    /// What is done since the last take, in the order it was done.
    pub fn take_walked(&mut self) -> Vec<Walked, Global> {
        std::mem::replace(&mut self.walked, Vec::new_in(Global))
    }

    /// Ends the walk, giving what is done and not taken; an error names an instance still
    /// waiting.
    pub fn end(self) -> Result<Vec<Walked, Global>, WalkError> {
        let mut waiting = Vec::new_in(Global);
        for (node, phase) in &self.seen {
            if *phase != Phase::Done {
                waiting.push(node.symbol());
            }
        }
        waiting.sort_unstable();
        if let Some(symbol) = waiting.first() {
            return Err(WalkError::Unfinished { symbol: *symbol });
        }
        Ok(self.walked)
    }

    /// The instance keys this walk made at `tier`.
    pub fn keys(&self, tier: Tier) -> &Keys {
        &self.keys[tier_index(tier)]
    }

    fn spend(&mut self) -> Result<(), WalkError> {
        self.steps += 1;
        if self.steps > self.config.fuel {
            return Err(WalkError::Fuel { steps: self.steps });
        }
        Ok(())
    }

    fn finish(&mut self, node: Node) {
        self.done += 1;
        match self.config.plant {
            // Planted: the seen set forgets every other node done, so a second caller walks
            // it again.
            Plant::SeenDrops if self.done % 2 == 0 => {
                self.seen.remove(&node);
            }
            _ => {
                self.seen.insert(node, Phase::Done);
            }
        }
    }

    fn plan_key(&self, symbol: &Symbol, front: &impl Front) -> Result<PlanKey, WalkError> {
        match front.body_key(&symbol.item) {
            Ok(body) => Ok(plan_key(symbol, &body, &self.plan_settings)),
            Err(BodyError::Missing) => Err(WalkError::NoBody { symbol: *symbol }),
            Err(BodyError::Refused { diagnostics }) => {
                Err(WalkError::Refused { symbol: *symbol, diagnostics })
            }
        }
    }

    fn ask_plan(&mut self, symbol: Symbol, waiter: Symbol, front: &impl Front) -> Result<(), WalkError> {
        let waiters = self.waiting.entry(symbol).or_insert_with(|| Vec::new_in(Global));
        if !waiters.contains(&waiter) {
            waiters.push(waiter);
        }
        if self.asked.insert(symbol) {
            let key = self.plan_key(&symbol, front)?;
            self.asks.push_back(Ask::LookupPlan { symbol, key });
        }
        Ok(())
    }

    fn resolve(&mut self, symbol: Symbol, front: &impl Front) -> Result<(), WalkError> {
        if self.seen.get(&Node::Function(symbol)) != Some(&Phase::Resolving) {
            return Ok(());
        }
        let tier = self.tier_of(&symbol, front);
        match self.keys[tier_index(tier)].resolve(&symbol, &self.plans) {
            Resolution::Keyed(key) => {
                self.seen.insert(Node::Function(symbol), Phase::Coding(key));
                self.asks.push_back(Ask::LookupCode { symbol, key });
            }
            Resolution::Needs(needs) => {
                for needed in needs {
                    self.ask_plan(needed, symbol, front)?;
                }
            }
        }
        Ok(())
    }

    fn walk_static(&mut self, symbol: Symbol, front: &impl Front, prior: &impl Prior) -> Result<(), WalkError> {
        let Some(value) = front.static_value(&symbol) else {
            return Err(WalkError::NoStatic { symbol });
        };
        let head = value.head();
        let node = Node::Static(symbol);
        self.finish(node);
        for callee in &head.calls {
            self.reach(Node::Function(*callee), Seed::Reached, prior);
        }
        for target in &head.statics {
            self.reach(Node::Static(*target), Seed::Reached, prior);
        }
        self.walked.push(Walked {
            node,
            key: value.key().0,
            tier: Tier::Release,
            calls: head.calls,
            statics: head.statics,
            inlined: Vec::new_in(Global),
        });
        Ok(())
    }
}
