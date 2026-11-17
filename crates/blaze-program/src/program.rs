//! A program is a manifest: which nodes have rows, under which keys, and what refers to what.
//! [`Program::place`] takes what a walk gave (a demand: a whole program; or a delta: an edit's
//! neighbourhood, or a tier-up) and gives a [`RowChange`]: the rows that came, went or moved.
//! The linker turns it into its own change (blaze-link's `Change`) and patches the file; where
//! a row's bytes go is the linker's, never this crate's, and nothing is relinked.
//!
//! **The manifest** (each table ordered by its key, so it is a function of the program):
//!
//! | table | row |
//! |---|---|
//! | functions | symbol → instance key, tier, the build it came in, callees, statics, plan key, instances it took in |
//! | statics | symbol → value key, functions and statics it points at |
//! | referrers | (target, referrer): every edge of the rows, reversed; what the seeds search upward |
//! | inliners | (inlinee, inliner), at any depth: what an edit of an inlinee reseeds |
//!
//! The first two are the record; the last two are derived from them, on place and on decode, and
//! never stored: rebuilding them is one pass over the rows a decode reads anyway, against bytes
//! that would double the manifest.
//!
//! It is itself a record ([`Program`] is a [`Payload`], filed by [`crate::keyed`]), keyed by its
//! roots and settings ([`program_key`]), and is cut into content-defined chunks
//! ([`Program::chunks`]) so a store keeps an edit's few changed chunks, not a second manifest.
//!
//! **Retiring.** A node no root reaches any more leaves the manifest (it went); the mark that
//! finds them runs only when an edit removed an edge or the walk left a held node unreached.
//!
//! **Tiers.** A row holds its instance's tier. [`Program::tier_ups`] names the workspace
//! instances to make again at the top tier: every one still at the fast tier that came in an
//! earlier build and has not changed since. The chosen rule, and why: an instance an edit just
//! made is likely to be edited again (an agent's edits come in runs), so optimizing it first is
//! wasted; a measured-hot rule needs counters in fast-tier code and a run to fill them, which a
//! build that runs nothing has not got; "everything unchanged for one build, at low priority
//! when the pool is idle" reaches the maximally optimized program with no measurement and never
//! delays an edit. A tier-up is a [`Delta::tier_up`]: no source changed, the same body key, a new
//! instance key; tiering never changes behaviour.
//!
//! **The floor, and the cost against it.** A placement must at least read what the walk gave
//! (`W` rows) and write the change (`C` rows): `O(W + C)`. It is `O((W + C) log R)` in a program
//! of `R` rows (the tables are ordered maps), plus, when an edge was removed, the mark: `O(R)`,
//! the one step not proportional to the edit (estimate: about 0.1 µs a row, so 10 ms at
//! `R = 100 000`, against a sub-millisecond floor for a one-function edit; it runs only on an
//! edit that drops a call). The seeds search upward from each stale node through the referrers
//! table and stop at the first unstale root path: proportional to the edit's ancestors.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::Plant;
use crate::change::{Row, RowChange};
use crate::codec::{CodecError, Digest, In, Out, Payload};
use crate::key::{InstanceKey, ItemId, Node, PlanKey, Settings, Symbol, Tier};
use crate::record::ValueKey;
use crate::walk::{Prior, Seed, Stale, Walked};

/// A function's row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FnRow {
    pub key: InstanceKey,
    pub tier: Tier,
    /// The build ([`Program::generation`]) it came in under this key.
    pub since: u64,
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
    /// Its plan key: with [`FnRow::inlined`], the tier-free identity of the code a test runs
    /// through this row ([`crate::reach`]).
    pub plan: PlanKey,
    /// Every instance it took in at any depth, sorted by symbol, each once, with its plan key.
    pub inlined: Vec<(Symbol, PlanKey), Global>,
}

/// A static's row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StaticRow {
    pub key: ValueKey,
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
}

/// What changed since the program was placed, as the front end knows it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Delta {
    /// Every item whose body key moved (a body or signature edit of it, an edit of a callee's
    /// signature or of a type it names).
    pub edited: Vec<ItemId, Global>,
    /// Every item that is gone from the program's source.
    pub gone: Vec<ItemId, Global>,
    /// Every static whose value changed.
    pub statics: Vec<Symbol, Global>,
    /// Every item that moved in its file with its body key unchanged.
    pub moved: Vec<ItemId, Global>,
    /// Workspace instances to make again at the top tier, their bodies unchanged
    /// ([`Program::tier_ups`]).
    pub tier_up: Vec<Symbol, Global>,
    /// Whether a caller's code can take a callee in, so a callee's edit can move its callers'
    /// code ([`Program::seeds`]).
    pub callers: Callers,
}

/// Whether the code of a program's instances can take a callee in: what decides if an edited
/// instance's callers are walked again ([`Stale::Revisit`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Callers {
    /// An edit can change a caller's inline decision or what it took in: its callers are
    /// walked again, at their tier.
    #[default]
    MayTakeIn,
    /// No code takes a callee in, and a code key reads of a callee's plan only what moves
    /// with the callee's own name (its place's kind): a caller's code moves only when its own
    /// plan does, so the callers of an edited instance are not walked again. blaze-keyed's
    /// walker ([`crate::walker`]), whose code key is never a callee's.
    NeverTakeIn,
}

impl Delta {
    /// The tier a workspace node new to the program is made at in this delta's walk
    /// ([`crate::walk::WalkConfig::new_at`]): `Release` when the delta only tiers up (a
    /// background walk: what it reaches first is made once, at the tier the program is moving
    /// to), `Edit` when it carries any edit (nothing an edit needs waits on the top tier).
    pub fn new_at(&self) -> Tier {
        let edits = !self.edited.is_empty() || !self.gone.is_empty() || !self.statics.is_empty() || !self.moved.is_empty();
        match edits || self.tier_up.is_empty() {
            true => Tier::Edit,
            false => Tier::Release,
        }
    }
}

/// Why a placement was refused, before anything changed. Each names the node it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramError {
    /// The walk gave `node` twice.
    Twice { node: Node },
    /// `referrer` refers to `target`, which the program would not hold.
    Unplaced { referrer: Node, target: Node },
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProgramError::Twice { node } => write!(f, "the walk gave {node} twice"),
            ProgramError::Unplaced { referrer, target } => {
                write!(f, "{referrer} refers to {target}, which the program would not hold")
            }
        }
    }
}

/// The domain of a program's key.
pub const PROGRAM_DOMAIN: &str = "blaze program manifest 2";
/// The domain of a manifest entry's chunk-boundary hash.
pub const CHUNK_DOMAIN: &str = "blaze program chunk 1";
/// An entry whose boundary hash's first byte is under this ends a chunk: about one entry in 16.
const CHUNK_CUT: u8 = 16;

/// The key of the program of `roots` under `settings`.
pub fn program_key(roots: &[Node], settings: &Settings) -> Digest {
    let mut sorted = Vec::with_capacity_in(roots.len(), Global);
    sorted.extend_from_slice(roots);
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Out::new();
    out.count(sorted.len());
    for root in &sorted {
        root.encode(&mut out);
    }
    settings.encode(&mut out);
    let bytes = out.finish();
    Digest::of(PROGRAM_DOMAIN, &bytes)
}

/// A manifest cut into chunks: the header, then the entries, each chunk ending after an entry
/// whose boundary hash says so. A one-function edit changes the chunks holding its rows and
/// nothing else.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Chunks {
    pub header: Vec<u8, Global>,
    pub chunks: Vec<Vec<u8, Global>, Global>,
}

impl Chunks {
    /// Each chunk's digest, in order: what a store files the manifest under.
    pub fn directory(&self) -> Vec<Digest, Global> {
        let mut out = Vec::with_capacity_in(self.chunks.len(), Global);
        for chunk in &self.chunks {
            out.push(Digest::of(CHUNK_DOMAIN, chunk));
        }
        out
    }
}

/// A program: its manifest and the machine that changes it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Program {
    settings: Settings,
    roots: Vec<Node, Global>,
    fns: BTreeMap<Symbol, FnRow, Global>,
    statics: BTreeMap<Symbol, StaticRow, Global>,
    /// Derived: (target, referrer) for every edge of every row.
    referrers: BTreeSet<(Node, Node), Global>,
    /// Derived: (inlinee, inliner) for every instance a row took in.
    inliners: BTreeSet<(Symbol, Symbol), Global>,
    /// The number of placements made.
    generation: u64,
    plant: Plant,
}

const LEAST_NODE: Node = Node::Function(Symbol {
    item: ItemId(Digest([0; 32])),
    args: crate::key::ArgsKey(Digest([0; 32])),
});

/// A row taken out of the manifest: its key and what it referred to.
struct Forgotten {
    key: Digest,
    calls: Vec<Symbol, Global>,
    statics: Vec<Symbol, Global>,
}

/// What a row or a walked entry refers to: its calls, then its statics.
fn targets_of<'a>(calls: &'a [Symbol], statics: &'a [Symbol]) -> impl Iterator<Item = Node> + 'a {
    calls.iter().map(|callee| Node::Function(*callee)).chain(statics.iter().map(|target| Node::Static(*target)))
}

impl Program {
    pub fn new(settings: Settings, roots: &[Node], plant: Plant) -> Program {
        let mut sorted = Vec::with_capacity_in(roots.len(), Global);
        sorted.extend_from_slice(roots);
        sorted.sort_unstable();
        sorted.dedup();
        Program {
            settings,
            roots: sorted,
            fns: BTreeMap::new_in(Global),
            statics: BTreeMap::new_in(Global),
            referrers: BTreeSet::new_in(Global),
            inliners: BTreeSet::new_in(Global),
            generation: 0,
            plant,
        }
    }

    /// A program of `roots` holding `rows` (each function's row, by its symbol): a manifest
    /// kept in another form (blaze-keyed's kept rows, as [`crate::walker`] reads them), its
    /// derived tables rebuilt. Cost: `O(R log R)`, a decode's.
    pub fn of_rows(
        settings: Settings,
        roots: &[Node],
        rows: impl IntoIterator<Item = (Symbol, FnRow)>,
        plant: Plant,
    ) -> Program {
        let mut program = Program::new(settings, roots, plant);
        for (symbol, row) in rows {
            program.admit_function(symbol, row);
        }
        program
    }

    pub fn key(&self) -> Digest {
        program_key(&self.roots, &self.settings)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn roots(&self) -> &[Node] {
        &self.roots
    }

    pub fn functions(&self) -> &BTreeMap<Symbol, FnRow, Global> {
        &self.fns
    }

    pub fn statics(&self) -> &BTreeMap<Symbol, StaticRow, Global> {
        &self.statics
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The key `node` is held under, if it has a row.
    pub fn key_of(&self, node: &Node) -> Option<Digest> {
        match node {
            Node::Function(symbol) => self.fns.get(symbol).map(|row| row.key.0),
            Node::Static(symbol) => self.statics.get(symbol).map(|row| row.key.0),
        }
    }

    /// The workspace instances to tier up when the pool is idle: every one at the fast tier
    /// that came in an earlier build and is unchanged since.
    pub fn tier_ups(&self) -> Vec<Symbol, Global> {
        let mut out = Vec::new_in(Global);
        for (symbol, row) in &self.fns {
            if row.tier == Tier::Edit && row.since < self.generation {
                out.push(*symbol);
            }
        }
        out
    }

    /// The stale nodes of a delta, each seeded by the crate's row rule ([`Seed::Walk`] or
    /// [`Seed::IfReached`]): every held instance of an edited item ([`Stale::Edited`]); every
    /// function that calls one or a gone one (unless [`Callers::NeverTakeIn`]), and every instance that took any of those in at
    /// any depth ([`Stale::Revisit`]: its inline decisions or inlinees may have changed, its
    /// tier is kept); every changed static; every root not yet held; and the tier-ups
    /// ([`Stale::TierUp`]). On an empty program with [`Program::top_tier`] of another as
    /// `delta.tier_up`, a whole build that holds the same rows at the same tiers.
    pub fn seeds(&self, delta: &Delta) -> Vec<(Node, Seed), Global> {
        // The held instances of edited items, and of gone ones (whose callers must look).
        let mut edited = BTreeSet::new_in(Global);
        let mut touched = BTreeSet::new_in(Global);
        for (items, live) in [(&delta.edited, true), (&delta.gone, false)] {
            for item in items {
                for (symbol, _) in self.fns.range(Symbol::first_of(*item)..=Symbol::last_of(*item)) {
                    touched.insert(*symbol);
                    if live {
                        edited.insert(*symbol);
                    }
                }
            }
        }
        // Their callers: an inline decision of theirs may change (never, when no code takes a
        // callee in).
        let mut revisit = BTreeSet::new_in(Global);
        let callers_move = delta.callers == Callers::MayTakeIn;
        for symbol in touched.iter().filter(|_| callers_move) {
            let target = Node::Function(*symbol);
            for (at, referrer) in self.referrers.range((target, LEAST_NODE)..) {
                if *at != target {
                    break;
                }
                if let Node::Function(caller) = referrer {
                    revisit.insert(*caller);
                }
            }
        }
        // Every instance that took any of them in, at any depth (the table is transitive). An
        // edited or gone item's instances are looked up by item, not through its held rows:
        // an instance that is only ever inlined has no row, yet every caller that took it in
        // holds a stale copy of it.
        for items in [&delta.edited, &delta.gone] {
            for item in items {
                let first = (Symbol::first_of(*item), Symbol::default());
                for (inlinee, inliner) in self.inliners.range(first..) {
                    if inlinee.item != *item {
                        break;
                    }
                    revisit.insert(*inliner);
                }
            }
        }
        let mut reached = Vec::new_in(Global);
        reached.extend(touched.iter().copied());
        reached.extend(revisit.iter().copied());
        for symbol in &reached {
            for (inlinee, inliner) in self.inliners.range((*symbol, Symbol::default())..) {
                if inlinee != symbol {
                    break;
                }
                revisit.insert(*inliner);
            }
        }
        // The stale nodes, each with how it is walked. An edit wins over a revisit, and both
        // over a tier-up (a changed body starts again at the fast tier; a revisit keeps its
        // tier); a root not held yet is stale as an edit, or as a tier-up when it is one.
        let mut stale = BTreeMap::new_in(Global);
        for symbol in revisit {
            if !touched.contains(&symbol) || edited.contains(&symbol) {
                stale.insert(Node::Function(symbol), Stale::Revisit);
            }
        }
        for symbol in &edited {
            stale.insert(Node::Function(*symbol), Stale::Edited);
        }
        for symbol in &delta.statics {
            stale.insert(Node::Static(*symbol), Stale::Edited);
        }
        let mut tiering = BTreeSet::new_in(Global);
        tiering.extend(delta.tier_up.iter().copied());
        for root in &self.roots {
            if !self.holds(root) {
                let tiers = matches!(root, Node::Function(symbol) if tiering.contains(symbol));
                stale.insert(*root, if tiers { Stale::TierUp } else { Stale::Edited });
            }
        }
        for symbol in &delta.tier_up {
            stale.entry(Node::Function(*symbol)).or_insert(Stale::TierUp);
        }
        // The row rule: a stale node a root or an unstale path from an unstale root holds is in
        // the new set whatever the walk does, and is walked for its own sake; every other one
        // is in it exactly when the walk reaches it.
        // A stale node not held that way is reached, if at all, from a walked node: directly,
        // or through kept rows that are its unstale ancestors (its gates, [`Seed::Gate`]),
        // which forward to what they call when the walk passes them.
        let mut live = BTreeSet::new_in(Global);
        let mut gates = BTreeSet::new_in(Global);
        let mut out = Vec::with_capacity_in(stale.len(), Global);
        for (node, how) in &stale {
            let anchored = self.roots.contains(node) || self.kept_reaches(node, &stale, &mut live, &mut gates);
            out.push((*node, if anchored { Seed::Walk(*how) } else { Seed::IfReached(*how) }));
        }
        for gate in gates {
            out.push((gate, Seed::Gate));
        }
        out
    }

    /// Whether an unstale root reaches `node` through unstale rows only (none of them in
    /// `stale`): nothing on such a path is walked again, so its edges are the new program's
    /// and `node` has a row whatever the walk does. Searched upward from `node` over the
    /// referrers table, so it reads the edit's ancestors, not the program. When none does,
    /// every unstale ancestor it saw is added to `gates`: the walk reaches `node` through them,
    /// if at all. `live` keeps, across the calls of one [`Program::seeds`], unstale nodes known
    /// to reach such a root (every node on a path found).
    fn kept_reaches(
        &self,
        node: &Node,
        stale: &BTreeMap<Node, Stale, Global>,
        live: &mut BTreeSet<Node, Global>,
        gates: &mut BTreeSet<Node, Global>,
    ) -> bool {
        let mut visited = BTreeSet::new_in(Global);
        // Each visited ancestor's node below it on the path from `node`.
        let mut below = BTreeMap::new_in(Global);
        let mut queue = VecDeque::new_in(Global);
        queue.push_back(*node);
        while let Some(at) = queue.pop_front() {
            for (target, referrer) in self.referrers.range((at, LEAST_NODE)..) {
                if *target != at {
                    break;
                }
                if stale.contains_key(referrer) || !visited.insert(*referrer) {
                    continue;
                }
                below.insert(*referrer, at);
                if live.contains(referrer) || self.roots.contains(referrer) {
                    let mut on = *referrer;
                    live.insert(on);
                    while let Some(next) = below.get(&on).copied() {
                        if next == *node {
                            break;
                        }
                        live.insert(next);
                        on = next;
                    }
                    return true;
                }
                queue.push_back(*referrer);
            }
        }
        gates.extend(visited);
        false
    }

    /// Every function the program holds at the top tier: with [`Delta::tier_up`], what a whole
    /// build of the same revision takes to hold the same rows at the same tiers.
    pub fn top_tier(&self) -> Vec<Symbol, Global> {
        let mut out = Vec::new_in(Global);
        for (symbol, row) in &self.fns {
            if row.tier == Tier::Release {
                out.push(*symbol);
            }
        }
        out
    }

    /// Takes in what a walk gave ([`crate::walk::Ended`]: what it walked, and the stale nodes
    /// it never reached, which have no row now and leave with every node only they reached),
    /// and the items of `moved`, giving the rows that came, went or moved. Refused before
    /// anything changes when the walk gave a node twice or a row refers to a node the program
    /// would not hold.
    pub fn place(&mut self, walked: &[Walked], unreached: &[Node], moved: &[ItemId]) -> Result<RowChange, ProgramError> {
        let whole = self.fns.is_empty() && self.statics.is_empty();
        let mut change = RowChange { whole, ..RowChange::default() };

        // The nodes whose row is new or under another key, in node order, each once.
        let mut order = Vec::with_capacity_in(walked.len(), Global);
        order.extend(walked.iter());
        order.sort_unstable_by(|a, b| a.node.cmp(&b.node));
        let mut changing = Vec::with_capacity_in(order.len(), Global);
        for (index, entry) in order.iter().enumerate() {
            if index > 0 && order[index - 1].node == entry.node {
                return Err(ProgramError::Twice { node: entry.node });
            }
            if self.key_of(&entry.node) != Some(entry.key) {
                changing.push(*entry);
            }
        }
        // Every edge's target is held or comes now: refused before anything changes.
        let mut placing = BTreeSet::new_in(Global);
        placing.extend(changing.iter().map(|entry| entry.node));
        for entry in &changing {
            for target in targets_of(&entry.calls, &entry.statics) {
                if !placing.contains(&target) && !self.holds(&target) {
                    return Err(ProgramError::Unplaced { referrer: entry.node, target });
                }
            }
        }
        self.generation += 1;

        // A held node the walk never reached has left the set of rows: the mark runs.
        let mut lost_edge = unreached.iter().any(|node| self.holds(node));
        let mut came = BTreeSet::new_in(Global);
        for entry in &changing {
            let node = entry.node;
            if let Some(previous) = self.forget(&node) {
                lost_edge |= !previous.calls.iter().all(|c| entry.calls.contains(c))
                    || !previous.statics.iter().all(|s| entry.statics.contains(s));
                // Planted: a row whose key moved is not said to have gone under its old key.
                if self.plant != Plant::RekeyedNotGone {
                    change.went.push(Row { node, key: previous.key });
                }
            }
            match node {
                Node::Function(symbol) => self.admit_function(
                    symbol,
                    FnRow {
                        key: InstanceKey(entry.key),
                        tier: entry.tier,
                        since: self.generation,
                        calls: entry.calls.clone(),
                        statics: entry.statics.clone(),
                        plan: entry.plan,
                        inlined: entry.inlined.clone(),
                    },
                ),
                Node::Static(symbol) => self.admit_static(
                    symbol,
                    StaticRow { key: ValueKey(entry.key), calls: entry.calls.clone(), statics: entry.statics.clone() },
                ),
            }
            came.insert(Row { node, key: entry.key });
        }
        if lost_edge {
            for row in self.retire() {
                // A row that came and leaves in one placement is in neither list.
                if !came.remove(&row) {
                    change.went.push(row);
                }
            }
        }
        change.came.extend(came.iter().copied());

        // The rows made from a moved item whose key did not move: its own instances and every
        // instance that took one of them in.
        let mut moved_rows = BTreeSet::new_in(Global);
        for item in moved {
            for (symbol, row) in self.fns.range(Symbol::first_of(*item)..=Symbol::last_of(*item)) {
                if !came.contains(&Row { node: Node::Function(*symbol), key: row.key.0 }) {
                    moved_rows.insert((*item, *symbol));
                }
            }
            for (inlinee, inliner) in self.inliners.range((Symbol::first_of(*item), Symbol::default())..) {
                if inlinee.item != *item {
                    break;
                }
                let held = self.fns.get(inliner).map(|row| row.key.0);
                if held.is_some_and(|key| !came.contains(&Row { node: Node::Function(*inliner), key })) {
                    moved_rows.insert((*item, *inliner));
                }
            }
        }
        change.moved.extend(moved_rows);
        change.went.sort_unstable();
        Ok(change)
    }

    /// The manifest in content-defined chunks.
    pub fn chunks(&self) -> Chunks {
        let mut header = Out::new();
        self.encode_header(&mut header);
        let mut chunks = Vec::new_in(Global);
        let mut chunk = Out::new();
        self.for_each_entry(|entry| {
            chunk.raw(entry);
            if Digest::of(CHUNK_DOMAIN, entry).0[0] < CHUNK_CUT {
                chunks.push(std::mem::take(&mut chunk).finish());
            }
        });
        if chunk.len() > 0 {
            chunks.push(chunk.finish());
        }
        Chunks { header: header.finish(), chunks }
    }

    /// The program the chunks hold.
    pub fn from_chunks(chunks: &Chunks) -> Result<Program, CodecError> {
        let mut input = In::new(Program::NAME, &chunks.header);
        let mut program = Program::decode_header(&mut input)?;
        input.end()?;
        for chunk in &chunks.chunks {
            let mut input = In::new(Program::NAME, chunk);
            while !input.at_end() {
                program.decode_entry(&mut input)?;
            }
        }
        Ok(program)
    }

    fn holds(&self, node: &Node) -> bool {
        match node {
            Node::Function(symbol) => self.fns.contains_key(symbol),
            Node::Static(symbol) => self.statics.contains_key(symbol),
        }
    }

    /// Puts a function's row in, with its derived edges.
    fn admit_function(&mut self, symbol: Symbol, row: FnRow) {
        let node = Node::Function(symbol);
        for target in targets_of(&row.calls, &row.statics) {
            self.referrers.insert((target, node));
        }
        for (inlinee, _) in &row.inlined {
            self.inliners.insert((*inlinee, symbol));
        }
        self.fns.insert(symbol, row);
    }

    /// Puts a static's row in, with its derived edges.
    fn admit_static(&mut self, symbol: Symbol, row: StaticRow) {
        let node = Node::Static(symbol);
        for target in targets_of(&row.calls, &row.statics) {
            self.referrers.insert((target, node));
        }
        self.statics.insert(symbol, row);
    }

    /// Takes `node`'s row out, with its derived edges, giving its key and what it referred to.
    fn forget(&mut self, node: &Node) -> Option<Forgotten> {
        let (key, calls, statics) = match node {
            Node::Function(symbol) => {
                let row = self.fns.remove(symbol)?;
                for (inlinee, _) in &row.inlined {
                    self.inliners.remove(&(*inlinee, *symbol));
                }
                (row.key.0, row.calls, row.statics)
            }
            Node::Static(symbol) => {
                let row = self.statics.remove(symbol)?;
                (row.key.0, row.calls, row.statics)
            }
        };
        for target in targets_of(&calls, &statics) {
            self.referrers.remove(&(target, *node));
        }
        Some(Forgotten { key, calls, statics })
    }

    /// Drops every node no root reaches, giving their rows in node order.
    fn retire(&mut self) -> Vec<Row, Global> {
        let mut marked = BTreeSet::new_in(Global);
        let mut stack = Vec::new_in(Global);
        for root in &self.roots {
            if marked.insert(*root) {
                stack.push(*root);
            }
        }
        while let Some(node) = stack.pop() {
            let (calls, statics) = match &node {
                Node::Function(symbol) => match self.fns.get(symbol) {
                    Some(row) => (&row.calls, &row.statics),
                    None => continue,
                },
                Node::Static(symbol) => match self.statics.get(symbol) {
                    Some(row) => (&row.calls, &row.statics),
                    None => continue,
                },
            };
            for target in targets_of(calls, statics) {
                if marked.insert(target) {
                    stack.push(target);
                }
            }
        }
        let mut unmarked = Vec::new_in(Global);
        unmarked.extend(self.fns.keys().map(|symbol| Node::Function(*symbol)).filter(|node| !marked.contains(node)));
        unmarked.extend(self.statics.keys().map(|symbol| Node::Static(*symbol)).filter(|node| !marked.contains(node)));
        let mut retired = Vec::with_capacity_in(unmarked.len(), Global);
        for node in unmarked {
            if let Some(forgotten) = self.forget(&node) {
                retired.push(Row { node, key: forgotten.key });
            }
        }
        retired
    }

    fn encode_header(&self, out: &mut Out) {
        self.settings.encode(out);
        out.count(self.roots.len());
        for root in &self.roots {
            root.encode(out);
        }
        out.u64(self.generation);
    }

    fn decode_header(input: &mut In<'_>) -> Result<Program, CodecError> {
        let settings = Settings::decode(input)?;
        let count = input.count(33)?;
        let mut roots = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            roots.push(Node::decode(input)?);
        }
        let mut program = Program::new(settings, &roots, Plant::None);
        program.generation = input.u64()?;
        Ok(program)
    }

    /// Each manifest entry's bytes, in table order. The derived tables are not written.
    fn for_each_entry(&self, mut each: impl FnMut(&[u8])) {
        for (symbol, row) in &self.fns {
            let mut out = Out::new();
            out.u8(0);
            symbol.encode(&mut out);
            out.digest(&row.key.0);
            out.u8(row.tier as u8);
            out.u64(row.since);
            encode_symbols(&row.calls, &mut out);
            encode_symbols(&row.statics, &mut out);
            out.digest(&row.plan.0);
            encode_planned(&row.inlined, &mut out);
            each(&out.finish());
        }
        for (symbol, row) in &self.statics {
            let mut out = Out::new();
            out.u8(1);
            symbol.encode(&mut out);
            out.digest(&row.key.0);
            encode_symbols(&row.calls, &mut out);
            encode_symbols(&row.statics, &mut out);
            each(&out.finish());
        }
    }

    fn decode_entry(&mut self, input: &mut In<'_>) -> Result<(), CodecError> {
        match input.u8()? {
            0 => {
                let symbol = Symbol::decode(input)?;
                let key = InstanceKey(input.digest()?);
                let tier = decode_tier(input)?;
                let since = input.u64()?;
                let calls = decode_symbols(input)?;
                let statics = decode_symbols(input)?;
                let plan = PlanKey(input.digest()?);
                let inlined = decode_planned(input)?;
                if self.fns.contains_key(&symbol) {
                    return Err(input.error("a function's row given twice"));
                }
                self.admit_function(symbol, FnRow { key, tier, since, calls, statics, plan, inlined });
            }
            1 => {
                let symbol = Symbol::decode(input)?;
                let key = ValueKey(input.digest()?);
                let calls = decode_symbols(input)?;
                let statics = decode_symbols(input)?;
                if self.statics.contains_key(&symbol) {
                    return Err(input.error("a static's row given twice"));
                }
                self.admit_static(symbol, StaticRow { key, calls, statics });
            }
            _ => return Err(input.error("a manifest entry of a tag no entry has")),
        }
        Ok(())
    }
}

impl Payload for Program {
    const NAME: &'static str = "blaze program manifest";

    fn encode_body(&self, out: &mut Out) {
        self.encode_header(out);
        let mut entries = Out::new();
        let mut count = 0usize;
        self.for_each_entry(|entry| {
            entries.raw(entry);
            count += 1;
        });
        out.count(count);
        out.raw(&entries.finish());
    }

    fn decode_body(input: &mut In<'_>) -> Result<Program, CodecError> {
        let mut program = Program::decode_header(input)?;
        let count = input.count(1)?;
        for _ in 0..count {
            program.decode_entry(input)?;
        }
        Ok(program)
    }
}

impl Prior for Program {
    fn holds(&self, node: &Node) -> bool {
        Program::holds(self, node)
    }

    fn tier(&self, symbol: &Symbol) -> Option<Tier> {
        self.fns.get(symbol).map(|row| row.tier)
    }

    fn targets(&self, node: &Node) -> Vec<Node, Global> {
        let mut out = Vec::new_in(Global);
        let (calls, statics) = match node {
            Node::Function(symbol) => match self.fns.get(symbol) {
                Some(row) => (&row.calls, &row.statics),
                None => return out,
            },
            Node::Static(symbol) => match self.statics.get(symbol) {
                Some(row) => (&row.calls, &row.statics),
                None => return out,
            },
        };
        out.extend(targets_of(calls, statics));
        out
    }
}

fn decode_tier(input: &mut In<'_>) -> Result<Tier, CodecError> {
    match input.u8()? {
        0 => Ok(Tier::Edit),
        1 => Ok(Tier::Release),
        _ => Err(input.error("a tier no tier has")),
    }
}

fn encode_symbols(symbols: &[Symbol], out: &mut Out) {
    out.count(symbols.len());
    for symbol in symbols {
        symbol.encode(out);
    }
}

fn decode_symbols(input: &mut In<'_>) -> Result<Vec<Symbol, Global>, CodecError> {
    let count = input.count(64)?;
    let mut out = Vec::with_capacity_in(count, Global);
    for _ in 0..count {
        out.push(Symbol::decode(input)?);
    }
    Ok(out)
}

fn encode_planned(planned: &[(Symbol, PlanKey)], out: &mut Out) {
    out.count(planned.len());
    for (symbol, plan) in planned {
        symbol.encode(out);
        out.digest(&plan.0);
    }
}

fn decode_planned(input: &mut In<'_>) -> Result<Vec<(Symbol, PlanKey), Global>, CodecError> {
    let count = input.count(96)?;
    let mut out = Vec::with_capacity_in(count, Global);
    for _ in 0..count {
        let symbol = Symbol::decode(input)?;
        out.push((symbol, PlanKey(input.digest()?)));
    }
    Ok(out)
}
