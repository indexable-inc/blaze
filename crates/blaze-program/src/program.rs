//! A program is a manifest: which node is where, and what refers to what. [`Program::place`]
//! takes what a walk gave (a demand: a whole program; or a delta: an edit's neighbourhood, or a
//! tier-up) and gives a [`PlacementChange`] of bytes proportional to the edit. Nothing is ever
//! relinked: the file is patched and the program run again.
//!
//! **The manifest** (each table ordered by its key, so it is a function of the program):
//!
//! | table | row |
//! |---|---|
//! | functions | symbol → instance key, tier, code place (offset, size, capacity), location block place, the build it was placed in, callees, statics, fields pointing at nodes, instances it took in, items of its locations |
//! | statics | symbol → value key, place, functions and statics it points at, its fields |
//! | files | file name → its bytes' place in [`Section::Data`] |
//! | spans | item → its file and first line, as the placed locations say |
//! | sites | (target, referrer, relocation) → where the field is: what a move repatches |
//! | location items | (item, function, location) → line within the item: what a span move rewrites |
//! | inliners | (inlinee, inliner), at any depth: what an edit of an inlinee reseeds |
//! | externs | name → import slot |
//!
//! It is itself a record ([`Program`] is a [`Payload`], filed by [`crate::keyed`]), keyed by its roots and settings
//! ([`program_key`]), and is cut into content-defined chunks ([`Program::chunks`]) so a store
//! keeps an edit's few changed chunks, not a second manifest.
//!
//! **The layout policy (file only).** Calls are direct. Each function, location block and static
//! gets a capacity a quarter over its size (16-aligned). New bytes that fit are written in place;
//! bytes that do not are appended at the section's tail and every field that pointed at the old
//! place is repatched from the sites table (4 or 8 bytes a site, with no referrer read or
//! remade): an outgrown function costs its own bytes plus 4 bytes per call site of it. The
//! space it left is dead; when the dead bytes of a section exceed its live bytes
//! ([`Program::compaction_due`]) the file is laid out whole again ([`Program::relayout`]), so the
//! file stays under twice its live size plus slack and the rewrite's cost is amortized over the
//! edits that made the dead space. A section's reserve outgrown is [`ProgramError::Outgrown`],
//! refused before anything changes; the relayout then doubles the reserve. A node no root
//! reaches any more is retired from the manifest (its bytes are dead); the mark that finds them
//! runs only when an edit removed an edge.
//!
//! **Tiers.** A row holds its instance's tier. [`Program::tier_ups`] names the workspace
//! instances to make again at the top tier: every one still at the fast tier that was placed in
//! an earlier build and has not changed since. The chosen rule, and why: an instance an edit
//! just made is likely to be edited again (an agent's edits come in runs), so optimizing it
//! first is wasted; a measured-hot rule needs counters in fast-tier code and a run to fill them,
//! which a build that runs nothing has not got; "everything unchanged for one build, at low
//! priority when the pool is idle" reaches the maximally optimized program with no measurement
//! and never delays an edit. A tier-up is a [`Delta::tier_up`]: no source changed, the same
//! body key, a new instance key; tiering never changes behaviour (the tests compare a program's
//! output at both tiers).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use crate::Plant;
use crate::change::{Import, LineWrite, PlacementChange, Section, Write};
use crate::key::{InstanceKey, ItemId, Node, PlanKey, Settings, Symbol, Tier};
use crate::record::{InstanceCode, RelocError, RelocKind, StaticValue, Target, ValueKey};
use crate::codec::{CodecError, Digest, In, Out, Payload};
use crate::walk::{Front, Prior, Seed, Span, Stale, Walked};

/// The instruction set (hosts: macOS arm64, Linux arm64 and x86_64).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Arch {
    X86_64,
    Arm64,
}

/// The machine a program is laid out for: its instruction set, its page size, and the address
/// space each section reserves (section `i` starts at `(i + 1) * reserve`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Machine {
    pub arch: Arch,
    pub page: u64,
    pub reserve: u64,
}

impl Machine {
    pub fn base(&self, section: Section) -> u64 {
        (section.index() as u64 + 1) * self.reserve
    }

    pub fn doubled(&self) -> Machine {
        Machine { reserve: self.reserve * 2, ..*self }
    }
}

/// A node's bytes in a section.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Place {
    pub offset: u64,
    pub size: u64,
    pub capacity: u64,
}

/// A function's row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FnRow {
    pub key: InstanceKey,
    pub tier: Tier,
    /// In [`Section::Text`]: the code, then its unwind rows.
    pub code: Place,
    /// Its location records, in [`Section::Locations`].
    pub locs: Place,
    /// The build ([`Program::generation`]) its code was placed in.
    pub since: u64,
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
    /// The fields of its code that point at functions and statics, by relocation index.
    pub out: Vec<(Node, u32), Global>,
    /// Its plan key: with [`FnRow::inlined`], the tier-free identity of the code a test runs
    /// through this row ([`crate::reach`]).
    pub plan: PlanKey,
    /// Every instance it took in at any depth, sorted by symbol, each once, with its plan key.
    pub inlined: Vec<(Symbol, PlanKey), Global>,
    /// The item each of its locations is in, in order.
    pub loc_items: Vec<ItemId, Global>,
}

/// A static's row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StaticRow {
    pub key: ValueKey,
    /// In [`Section::Data`].
    pub place: Place,
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
    pub out: Vec<(Node, u32), Global>,
}

/// Where a field pointing at a node is, so a move rewrites it without reading its referrer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Site {
    pub section: Section,
    pub offset: u64,
    pub kind: RelocKind,
    pub addend: i64,
    /// The field's bytes before relocation (an arm64 branch's opcode is in them).
    pub template: [u8; 4],
}

/// An item's span as the placed locations say.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SpanRow {
    pub file: Vec<u8, Global>,
    pub line: u32,
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

/// The code records a program reads, by instance key.
pub trait Records {
    fn code(&self, key: &InstanceKey) -> Option<InstanceCode>;
}

/// Why a placement was refused. Each names the node, item or section it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramError {
    /// The walk gave `node` twice.
    Twice { node: Node },
    /// No code record under the key the walk gave.
    NoRecord { symbol: Symbol, key: InstanceKey },
    /// The front end has no value for a static the walk gave.
    NoStatic { symbol: Symbol },
    /// A static's value has another key than the walk gave: the walk is stale.
    StaleStatic { symbol: Symbol },
    /// The front end has no span for an item a location is in.
    NoSpan { item: ItemId },
    /// `referrer` points at `target`, which the program does not hold.
    Unplaced { referrer: Node, target: Node },
    /// A field cannot hold its target.
    Reloc { node: Node, reloc: u32, error: RelocError },
    /// `section` would need `need` bytes, over its `reserve`.
    Outgrown { section: Section, need: u64, reserve: u64 },
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProgramError::Twice { node } => write!(f, "the walk gave {node} twice"),
            ProgramError::NoRecord { symbol, key } => {
                write!(f, "no code record of function {symbol} under its key {key}")
            }
            ProgramError::NoStatic { symbol } => {
                write!(f, "the front end has no value for static {symbol}")
            }
            ProgramError::StaleStatic { symbol } => {
                write!(f, "static {symbol}'s value is not the one the walk keyed: a stale walk")
            }
            ProgramError::NoSpan { item } => write!(
                f,
                "the front end has no span for item {}, which a panic location is in",
                item.0
            ),
            ProgramError::Unplaced { referrer, target } => {
                write!(f, "{referrer} points at {target}, which the program does not hold")
            }
            ProgramError::Reloc { node, reloc, error } => {
                write!(f, "{node}, relocation {reloc}: its field cannot hold its target ({error:?})")
            }
            ProgramError::Outgrown { section, need, reserve } => write!(
                f,
                "section {section:?} needs {need} bytes, over its reserve of {reserve}: lay the program out again at twice the reserve"
            ),
        }
    }
}

/// The domain of a program's key.
pub const PROGRAM_DOMAIN: &str = "blaze program manifest 1";
/// The domain of a manifest entry's chunk-boundary hash.
pub const CHUNK_DOMAIN: &str = "blaze program chunk 1";
/// An entry whose boundary hash's first byte is under this ends a chunk: about one entry in 16.
const CHUNK_CUT: u8 = 16;

/// The key of the program of `roots` under `settings`, laid out for `machine`.
pub fn program_key(roots: &[Node], settings: &Settings, machine: &Machine) -> Digest {
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
    out.u8(machine.arch as u8);
    out.u64(machine.page);
    out.u64(machine.reserve);
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

/// A program: its manifest and the machine that places nodes in it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Program {
    machine: Machine,
    settings: Settings,
    roots: Vec<Node, Global>,
    fns: BTreeMap<Symbol, FnRow, Global>,
    statics: BTreeMap<Symbol, StaticRow, Global>,
    files: BTreeMap<Vec<u8, Global>, Place, Global>,
    spans: BTreeMap<ItemId, SpanRow, Global>,
    sites: BTreeMap<(Node, Node, u32), Site, Global>,
    /// (item, function, location) → (line within the item, column).
    loc_items: BTreeMap<(ItemId, Symbol, u32), (u32, u32), Global>,
    inliners: BTreeSet<(Symbol, Symbol), Global>,
    externs: BTreeMap<Vec<u8, Global>, u32, Global>,
    lengths: [u64; 4],
    /// Bytes of each section no row holds any more.
    dead: [u64; 4],
    /// The number of placements made.
    generation: u64,
    plant: Plant,
}

/// A node's bytes and fields, ready to write.
enum Loaded<'w> {
    Function { walked: &'w Walked, symbol: Symbol, code: InstanceCode },
    Static { walked: &'w Walked, symbol: Symbol, value: StaticValue },
}

fn align16(value: u64) -> u64 {
    value.div_ceil(16) * 16
}

fn capacity(size: u64) -> u64 {
    align16(size + size / 4).max(16)
}

fn node_of(target: &Target) -> Option<Node> {
    match target {
        Target::Function(symbol) => Some(Node::Function(*symbol)),
        Target::Static(symbol) => Some(Node::Static(*symbol)),
        Target::Location(_) | Target::Extern(_) => None,
    }
}

const LEAST_NODE: Node = Node::Function(Symbol {
    item: ItemId(Digest([0; 32])),
    args: crate::key::ArgsKey(Digest([0; 32])),
});

impl Program {
    pub fn new(machine: Machine, settings: Settings, roots: &[Node], plant: Plant) -> Program {
        let mut sorted = Vec::with_capacity_in(roots.len(), Global);
        sorted.extend_from_slice(roots);
        sorted.sort_unstable();
        sorted.dedup();
        Program {
            machine,
            settings,
            roots: sorted,
            fns: BTreeMap::new_in(Global),
            statics: BTreeMap::new_in(Global),
            files: BTreeMap::new_in(Global),
            spans: BTreeMap::new_in(Global),
            sites: BTreeMap::new_in(Global),
            loc_items: BTreeMap::new_in(Global),
            inliners: BTreeSet::new_in(Global),
            externs: BTreeMap::new_in(Global),
            lengths: [0; 4],
            dead: [0; 4],
            generation: 0,
            plant,
        }
    }

    pub fn key(&self) -> Digest {
        program_key(&self.roots, &self.settings, &self.machine)
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
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

    pub fn files(&self) -> &BTreeMap<Vec<u8, Global>, Place, Global> {
        &self.files
    }

    pub fn externs(&self) -> &BTreeMap<Vec<u8, Global>, u32, Global> {
        &self.externs
    }

    pub fn spans(&self) -> &BTreeMap<ItemId, SpanRow, Global> {
        &self.spans
    }

    pub fn lengths(&self) -> [u64; 4] {
        self.lengths
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The address a reference to `node` holds: a function's entry, a static's value.
    pub fn address_of(&self, node: &Node) -> Option<u64> {
        match node {
            Node::Function(symbol) => {
                Some(self.machine.base(Section::Text) + self.fns.get(symbol)?.code.offset)
            }
            Node::Static(symbol) => {
                Some(self.machine.base(Section::Data) + self.statics.get(symbol)?.place.offset)
            }
        }
    }

    /// Whether the dead bytes of some section exceed its live bytes (and a page): the file is
    /// then laid out whole again.
    pub fn compaction_due(&self) -> bool {
        Section::ALL.iter().any(|section| {
            let dead = self.dead[section.index()];
            let live = self.lengths[section.index()] - dead;
            dead > live && dead > self.machine.page
        })
    }

    /// The workspace instances to tier up when the pool is idle: every one at the fast tier
    /// that was placed in an earlier build and is unchanged since.
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
    /// [`Seed::IfReached`]): every placed instance of an edited item ([`Stale::Edited`]); every
    /// function that calls one or a gone one, and every instance that took any of those in at
    /// any depth ([`Stale::Revisit`]: its inline decisions or inlinees may have changed, its
    /// tier is kept); every changed static; every root not yet placed; and the tier-ups
    /// ([`Stale::TierUp`]). On an empty program with [`Program::top_tier`] of another as
    /// `delta.tier_up`, a whole build that holds the same rows at the same tiers.
    pub fn seeds(&self, delta: &Delta) -> Vec<(Node, Seed), Global> {
        // The placed instances of edited items, and of gone ones (whose callers must look).
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
        // Their callers: an inline decision of theirs may change.
        let mut revisit = BTreeSet::new_in(Global);
        for symbol in &touched {
            let target = Node::Function(*symbol);
            for ((at, referrer, _), _) in self.sites.range((target, LEAST_NODE, 0u32)..) {
                if *at != target {
                    break;
                }
                if let Node::Function(caller) = referrer {
                    revisit.insert(*caller);
                }
            }
        }
        // Every instance that took any of them in, at any depth (the table is transitive). An
        // edited or gone item's instances are looked up by item, not through its placed rows:
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
        // tier); a root not placed yet is stale as an edit, or as a tier-up when it is one.
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
    /// and `node` has a row whatever the walk does. Searched upward from `node` over the sites
    /// table, so it reads the edit's ancestors, not the program. When none does, every unstale
    /// ancestor it saw is added to `gates`: the walk reaches `node` through them, if at all.
    /// `live` keeps, across the calls of one [`Program::seeds`], unstale nodes known to reach
    /// such a root (every node on a path found).
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
            for ((target, referrer, _), _) in self.sites.range((at, LEAST_NODE, 0u32)..) {
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

    /// Places what a walk gave ([`crate::walk::Ended`]: what it walked, and the stale nodes it
    /// never reached, which have no row now and are retired with every node only they
    /// reached), and the span moves of `moved`, giving the change to apply.
    /// Refused before anything changes when a record, a value or a span is missing, a field
    /// points at a node the program would not hold, or a reserve would be outgrown. A field
    /// whose distance does not fit ([`ProgramError::Reloc`], only past a reserve of 64 MiB on
    /// arm64 or 1 GiB on x86_64) is found while writing: the program is then to be laid out
    /// whole again.
    pub fn place(
        &mut self,
        walked: &[Walked],
        unreached: &[Node],
        moved: &[ItemId],
        front: &impl Front,
        records: &impl Records,
    ) -> Result<PlacementChange, ProgramError> {
        let whole = self.fns.is_empty() && self.statics.is_empty();
        let before = self.lengths;
        let mut change = PlacementChange { whole, ..PlacementChange::default() };

        // The nodes that changed, in symbol order, each once.
        let mut order = Vec::with_capacity_in(walked.len(), Global);
        for entry in walked {
            order.push(entry);
        }
        order.sort_unstable_by(|a, b| a.node.cmp(&b.node));
        let mut loaded = Vec::new_in(Global);
        for (index, entry) in order.iter().enumerate() {
            if index > 0 && order[index - 1].node == entry.node {
                return Err(ProgramError::Twice { node: entry.node });
            }
            match entry.node {
                Node::Function(symbol) => {
                    if self.fns.get(&symbol).is_some_and(|row| row.key.0 == entry.key) {
                        continue;
                    }
                    let key = InstanceKey(entry.key);
                    let Some(code) = records.code(&key) else {
                        return Err(ProgramError::NoRecord { symbol, key });
                    };
                    loaded.push(Loaded::Function { walked: entry, symbol, code });
                }
                Node::Static(symbol) => {
                    if self.statics.get(&symbol).is_some_and(|row| row.key.0 == entry.key) {
                        continue;
                    }
                    let Some(value) = front.static_value(&symbol) else {
                        return Err(ProgramError::NoStatic { symbol });
                    };
                    if value.key().0 != entry.key {
                        return Err(ProgramError::StaleStatic { symbol });
                    }
                    loaded.push(Loaded::Static { walked: entry, symbol, value });
                }
            }
        }
        // Every field's target is held or placed now: refused before anything changes.
        let mut placing = BTreeSet::new_in(Global);
        for node in &loaded {
            placing.insert(match node {
                Loaded::Function { symbol, .. } => Node::Function(*symbol),
                Loaded::Static { symbol, .. } => Node::Static(*symbol),
            });
        }
        for node in &loaded {
            let (referrer, relocs) = match node {
                Loaded::Function { symbol, code, .. } => (Node::Function(*symbol), code.relocs()),
                Loaded::Static { symbol, value, .. } => (Node::Static(*symbol), value.relocs()),
            };
            for reloc in relocs {
                if let Some(target) = node_of(&reloc.target) {
                    if !placing.contains(&target) && !self.holds(&target) {
                        return Err(ProgramError::Unplaced { referrer, target });
                    }
                }
            }
        }
        let mut moved_items = Vec::with_capacity_in(moved.len(), Global);
        moved_items.extend_from_slice(moved);
        moved_items.sort_unstable();
        moved_items.dedup();

        // The spans every changed location and every moved item needs, read first: a missing
        // one refuses the placement before anything changes.
        let mut spans = BTreeMap::new_in(Global);
        for node in &loaded {
            if let Loaded::Function { code, .. } = node {
                for row in code.locations() {
                    if !spans.contains_key(&row.item) {
                        let Some(span) = front.span(&row.item) else {
                            return Err(ProgramError::NoSpan { item: row.item });
                        };
                        spans.insert(row.item, span);
                    }
                }
            }
        }
        for item in &moved_items {
            if !self.spans.contains_key(item) || spans.contains_key(item) {
                continue;
            }
            let Some(span) = front.span(item) else {
                return Err(ProgramError::NoSpan { item: *item });
            };
            spans.insert(*item, span);
        }
        self.check_room(&loaded, &spans)?;
        self.generation += 1;

        // Pass one: every changed node's place and import slots.
        let mut changed = BTreeSet::new_in(Global);
        // A held node the walk never reached has left the set of rows: the mark runs.
        let mut lost_edge = unreached.iter().any(|node| self.holds(node));
        let mut moved_targets = Vec::new_in(Global);
        for node in &loaded {
            match node {
                Loaded::Function { walked, symbol, code } => {
                    changed.insert(Node::Function(*symbol));
                    let size = code.placed_len();
                    let locs_size = 24 * code.locations().len() as u64;
                    let old = self.fns.get(symbol).map(|row| (row.code, row.locs));
                    if let Some(row) = self.fns.get(symbol) {
                        lost_edge |= !row.calls.iter().all(|c| walked.calls.contains(c))
                            || !row.statics.iter().all(|s| walked.statics.contains(s));
                    }
                    let (code_place, locs_place, moved) = match old {
                        Some((code_place, locs_place)) => {
                            let (code_place, moved) = if size <= code_place.capacity {
                                (Place { size, ..code_place }, false)
                            } else {
                                self.dead[Section::Text.index()] += code_place.capacity;
                                (self.append(Section::Text, size, capacity(size))?, true)
                            };
                            let locs_place = if locs_size <= locs_place.capacity {
                                Place { size: locs_size, ..locs_place }
                            } else {
                                self.dead[Section::Locations.index()] += locs_place.capacity;
                                self.append(Section::Locations, locs_size, capacity(locs_size))?
                            };
                            (code_place, locs_place, moved)
                        }
                        None => (
                            self.append(Section::Text, size, capacity(size))?,
                            self.append(Section::Locations, locs_size, capacity(locs_size))?,
                            false,
                        ),
                    };
                    if moved {
                        moved_targets.push(Node::Function(*symbol));
                    }
                    for name in code.externs() {
                        if !self.externs.contains_key(name) {
                            let slot = self.externs.len() as u32;
                            self.externs.insert(name.clone(), slot);
                            self.lengths[Section::Imports.index()] = 8 * u64::from(slot + 1);
                            change.imports.push(Import { slot, name: name.clone() });
                        }
                    }
                    let mut loc_items = Vec::with_capacity_in(code.locations().len(), Global);
                    for row in code.locations() {
                        loc_items.push(row.item);
                    }
                    let previous = self.fns.insert(
                        *symbol,
                        FnRow {
                            key: InstanceKey(walked.key),
                            tier: walked.tier,
                            code: code_place,
                            locs: locs_place,
                            since: self.generation,
                            calls: walked.calls.clone(),
                            statics: walked.statics.clone(),
                            out: Vec::new_in(Global),
                            plan: walked.plan,
                            inlined: walked.inlined.clone(),
                            loc_items,
                        },
                    );
                    if let Some(previous) = previous {
                        self.forget_function(symbol, &previous);
                    }
                }
                Loaded::Static { walked, symbol, value } => {
                    changed.insert(Node::Static(*symbol));
                    let size = value.bytes().len() as u64;
                    let old = self.statics.get(symbol).map(|row| row.place);
                    if let Some(row) = self.statics.get(symbol) {
                        lost_edge |= !row.calls.iter().all(|c| walked.calls.contains(c))
                            || !row.statics.iter().all(|s| walked.statics.contains(s));
                    }
                    let (place, moved) = match old {
                        Some(place) if size <= place.capacity => (Place { size, ..place }, false),
                        Some(place) => {
                            self.dead[Section::Data.index()] += place.capacity;
                            (self.append(Section::Data, size, capacity(size))?, true)
                        }
                        None => (self.append(Section::Data, size, capacity(size))?, false),
                    };
                    if moved {
                        moved_targets.push(Node::Static(*symbol));
                    }
                    let previous = self.statics.insert(
                        *symbol,
                        StaticRow {
                            key: ValueKey(walked.key),
                            place,
                            calls: walked.calls.clone(),
                            statics: walked.statics.clone(),
                            out: Vec::new_in(Global),
                        },
                    );
                    if let Some(previous) = previous {
                        for (target, reloc) in &previous.out {
                            self.sites.remove(&(*target, Node::Static(*symbol), *reloc));
                        }
                    }
                }
            }
        }
        // The file names of new spans, written once each.
        for span in spans.values() {
            if !self.files.contains_key(&span.file) {
                let len = span.file.len() as u64;
                let place = self.append(Section::Data, len, align16(len).max(16))?;
                self.files.insert(span.file.clone(), place);
                change.writes.push(Write { section: Section::Data, offset: place.offset, bytes: span.file.clone() });
            }
        }
        // Spans: the changed locations' and the moved items'. A moved item's unchanged
        // locations are rewritten in place: 4 bytes each (24 when it changed files).
        for (item, span) in &spans {
            let row = SpanRow { file: span.file.clone(), line: span.line };
            let old = self.spans.insert(*item, row.clone());
            // Any span that moved (listed or not) rewrites the unchanged locations in it.
            if old.as_ref().is_none_or(|old| *old == row) {
                continue;
            }
            let same_file = old.as_ref().is_some_and(|old| old.file == row.file);
            let mut rewrites = Vec::new_in(Global);
            for ((at, function, index), place) in self.loc_items.range((*item, Symbol::default(), 0u32)..) {
                if at != item {
                    break;
                }
                if !changed.contains(&Node::Function(*function)) {
                    rewrites.push((*function, *index, *place));
                }
            }
            for (function, index, (line, col)) in rewrites {
                let Some(fn_row) = self.fns.get(&function) else { continue };
                let offset = fn_row.locs.offset + 24 * u64::from(index);
                if same_file {
                    change.lines.push(LineWrite { offset, line: row.line + line });
                } else {
                    let bytes = self.location_record(&row, line, col);
                    change.writes.push(Write { section: Section::Locations, offset, bytes });
                }
            }
        }

        // Pass two: every changed node's bytes, its fields resolved.
        for node in &loaded {
            match node {
                Loaded::Function { symbol, code, .. } => self.write_function(*symbol, code, &mut change)?,
                Loaded::Static { symbol, value, .. } => self.write_static(*symbol, value, &mut change)?,
            }
        }
        // Repatch every unchanged referrer of a node that moved.
        for target in &moved_targets {
            if self.plant == Plant::SiteNotRepatched {
                // Planted: a moved node's referrers keep pointing at its old place.
                break;
            }
            let Some(address) = self.address_of(target) else { continue };
            let mut patches = Vec::new_in(Global);
            for ((at, referrer, reloc), site) in self.sites.range((*target, LEAST_NODE, 0u32)..) {
                if at != target {
                    break;
                }
                if !changed.contains(referrer) {
                    patches.push((*referrer, *reloc, *site));
                }
            }
            for (referrer, reloc, site) in patches {
                let mut field = [0u8; 8];
                field[..4].copy_from_slice(&site.template);
                let width = site.kind.width();
                let place = self.machine.base(site.section) + site.offset;
                site.kind
                    .write(&mut field[..width], 0, place, address, site.addend)
                    .map_err(|error| ProgramError::Reloc { node: referrer, reloc, error })?;
                let mut bytes = Vec::with_capacity_in(width, Global);
                bytes.extend_from_slice(&field[..width]);
                change.writes.push(Write { section: site.section, offset: site.offset, bytes });
            }
        }
        if lost_edge {
            change.retired = self.retire();
        }
        change.writes.sort_by(|a, b| (a.section, a.offset).cmp(&(b.section, b.offset)));
        change.lines.sort_unstable();
        change.imports.sort();
        change.lengths = self.lengths;
        change.pages = self.pages(&change, &before);
        Ok(change)
    }

    /// The whole program laid out again for `machine`, from its own rows, with `front` at the
    /// revision the program was placed at (its statics' values are read again): what follows
    /// [`Program::compaction_due`]. After an [`ProgramError::Outgrown`] the delta is not placed
    /// yet, so the caller walks the new revision whole into a program at [`Machine::doubled`]
    /// (every record hits; no make).
    pub fn relayout(
        &self,
        machine: Machine,
        front: &impl Front,
        records: &impl Records,
    ) -> Result<(Program, PlacementChange), ProgramError> {
        let mut walked = Vec::new_in(Global);
        for (symbol, row) in &self.fns {
            walked.push(Walked {
                node: Node::Function(*symbol),
                key: row.key.0,
                tier: row.tier,
                plan: row.plan,
                calls: row.calls.clone(),
                statics: row.statics.clone(),
                inlined: row.inlined.clone(),
            });
        }
        for (symbol, row) in &self.statics {
            walked.push(Walked {
                node: Node::Static(*symbol),
                key: row.key.0,
                tier: Tier::Release,
                plan: PlanKey::default(),
                calls: row.calls.clone(),
                statics: row.statics.clone(),
                inlined: Vec::new_in(Global),
            });
        }
        let mut program = Program::new(machine, self.settings, &self.roots, self.plant);
        let change = program.place(&walked, &[], &[], front, records)?;
        // Keep each row's build, so tier-up choices survive a relayout.
        for (symbol, row) in &self.fns {
            if let Some(new) = program.fns.get_mut(symbol) {
                new.since = row.since;
            }
        }
        program.generation = self.generation;
        Ok((program, change))
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

    fn append(&mut self, section: Section, size: u64, capacity: u64) -> Result<Place, ProgramError> {
        let offset = align16(self.lengths[section.index()]);
        let need = offset + capacity;
        if need > self.machine.reserve {
            return Err(ProgramError::Outgrown { section, need, reserve: self.machine.reserve });
        }
        self.lengths[section.index()] = need;
        Ok(Place { offset, size, capacity })
    }

    /// Refuses a placement that could outgrow a reserve, before anything changes: each bound
    /// is at least what pass one appends.
    fn check_room(&self, loaded: &[Loaded<'_>], spans: &BTreeMap<ItemId, Span, Global>) -> Result<(), ProgramError> {
        let mut need = [0u64; 4];
        let mut externs = 0u64;
        for node in loaded {
            match node {
                Loaded::Function { code, .. } => {
                    need[Section::Text.index()] += capacity(code.placed_len()) + 16;
                    need[Section::Locations.index()] += capacity(24 * code.locations().len() as u64) + 16;
                    externs += code.externs().len() as u64;
                }
                Loaded::Static { value, .. } => {
                    need[Section::Data.index()] += capacity(value.bytes().len() as u64) + 16;
                }
            }
        }
        for span in spans.values() {
            need[Section::Data.index()] += align16(span.file.len() as u64).max(16) + 16;
        }
        need[Section::Imports.index()] += externs * 8;
        for section in Section::ALL {
            let total = self.lengths[section.index()] + need[section.index()];
            if total > self.machine.reserve {
                return Err(ProgramError::Outgrown { section, need: total, reserve: self.machine.reserve });
            }
        }
        Ok(())
    }

    /// Drops what the manifest kept of a function's previous code: its fields, its locations'
    /// items, what it took in.
    fn forget_function(&mut self, symbol: &Symbol, previous: &FnRow) {
        for (target, reloc) in &previous.out {
            self.sites.remove(&(*target, Node::Function(*symbol), *reloc));
        }
        for (index, item) in previous.loc_items.iter().enumerate() {
            self.loc_items.remove(&(*item, *symbol, index as u32));
        }
        for (inlinee, _) in &previous.inlined {
            self.inliners.remove(&(*inlinee, *symbol));
        }
    }

    /// A location record: the file name's address and length, the line, the column.
    fn location_record(&self, span: &SpanRow, line: u32, col: u32) -> Vec<u8, Global> {
        let place = self.files.get(&span.file).copied().unwrap_or_default();
        let mut bytes = Vec::with_capacity_in(24, Global);
        bytes.extend_from_slice(&(self.machine.base(Section::Data) + place.offset).to_le_bytes());
        bytes.extend_from_slice(&(span.file.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(span.line + line).to_le_bytes());
        bytes.extend_from_slice(&col.to_le_bytes());
        bytes
    }

    fn write_function(&mut self, symbol: Symbol, code: &InstanceCode, change: &mut PlacementChange) -> Result<(), ProgramError> {
        let node = Node::Function(symbol);
        let Some(row) = self.fns.get(&symbol) else {
            return Err(ProgramError::Unplaced { referrer: node, target: node });
        };
        let (code_place, locs_place) = (row.code, row.locs);
        let mut inlined = Vec::with_capacity_in(row.inlined.len(), Global);
        inlined.extend(row.inlined.iter().map(|(inlinee, _)| *inlinee));
        let base = self.machine.base(Section::Text) + code_place.offset;
        let mut bytes = Vec::with_capacity_in(code.placed_len() as usize, Global);
        bytes.extend_from_slice(code.code());
        bytes.extend_from_slice(code.unwind());
        let mut out = Vec::new_in(Global);
        for (index, reloc) in code.relocs().iter().enumerate() {
            let reloc_index = index as u32;
            let at = reloc.at as usize;
            let target = match reloc.target {
                Target::Function(_) | Target::Static(_) => {
                    let Some(target) = node_of(&reloc.target) else { continue };
                    let Some(address) = self.address_of(&target) else {
                        return Err(ProgramError::Unplaced { referrer: node, target });
                    };
                    address
                }
                Target::Location(at) => {
                    self.machine.base(Section::Locations) + locs_place.offset + 24 * u64::from(at)
                }
                Target::Extern(at) => {
                    let slot = code.externs().get(at as usize).and_then(|name| self.externs.get(name));
                    let Some(slot) = slot else {
                        return Err(ProgramError::Reloc { node, reloc: reloc_index, error: RelocError::OutOfBounds });
                    };
                    self.machine.base(Section::Imports) + 8 * u64::from(*slot)
                }
            };
            let mut template = [0u8; 4];
            template.copy_from_slice(&code.code()[at..at + 4]);
            reloc
                .kind
                .write(&mut bytes, at, base + u64::from(reloc.at), target, reloc.addend)
                .map_err(|error| ProgramError::Reloc { node, reloc: reloc_index, error })?;
            if let Some(target) = node_of(&reloc.target) {
                out.push((target, reloc_index));
                self.sites.insert(
                    (target, node, reloc_index),
                    Site {
                        section: Section::Text,
                        offset: code_place.offset + u64::from(reloc.at),
                        kind: reloc.kind,
                        addend: reloc.addend,
                        template,
                    },
                );
            }
        }
        change.writes.push(Write { section: Section::Text, offset: code_place.offset, bytes });
        // Its location records, each line from the span table.
        let mut records = Vec::with_capacity_in(24 * code.locations().len(), Global);
        for (index, loc) in code.locations().iter().enumerate() {
            let Some(span) = self.spans.get(&loc.item) else {
                return Err(ProgramError::NoSpan { item: loc.item });
            };
            let record = self.location_record(span, loc.line, loc.col);
            records.extend_from_slice(&record);
            self.loc_items.insert((loc.item, symbol, index as u32), (loc.line, loc.col));
        }
        if !records.is_empty() {
            change.writes.push(Write { section: Section::Locations, offset: locs_place.offset, bytes: records });
        }
        for inlinee in &inlined {
            self.inliners.insert((*inlinee, symbol));
        }
        if let Some(row) = self.fns.get_mut(&symbol) {
            row.out = out;
        }
        Ok(())
    }

    fn write_static(&mut self, symbol: Symbol, value: &StaticValue, change: &mut PlacementChange) -> Result<(), ProgramError> {
        let node = Node::Static(symbol);
        let Some(row) = self.statics.get(&symbol) else {
            return Err(ProgramError::Unplaced { referrer: node, target: node });
        };
        let place = row.place;
        let base = self.machine.base(Section::Data) + place.offset;
        let mut bytes = Vec::with_capacity_in(value.bytes().len(), Global);
        bytes.extend_from_slice(value.bytes());
        let mut out = Vec::new_in(Global);
        for (index, reloc) in value.relocs().iter().enumerate() {
            let Some(target) = node_of(&reloc.target) else { continue };
            let Some(address) = self.address_of(&target) else {
                return Err(ProgramError::Unplaced { referrer: node, target });
            };
            let at = reloc.at as usize;
            let mut template = [0u8; 4];
            template.copy_from_slice(&value.bytes()[at..at + 4]);
            reloc
                .kind
                .write(&mut bytes, at, base + u64::from(reloc.at), address, reloc.addend)
                .map_err(|error| ProgramError::Reloc { node, reloc: index as u32, error })?;
            out.push((target, index as u32));
            self.sites.insert(
                (target, node, index as u32),
                Site {
                    section: Section::Data,
                    offset: place.offset + u64::from(reloc.at),
                    kind: reloc.kind,
                    addend: reloc.addend,
                    template,
                },
            );
        }
        change.writes.push(Write { section: Section::Data, offset: place.offset, bytes });
        if let Some(row) = self.statics.get_mut(&symbol) {
            row.out = out;
        }
        Ok(())
    }

    /// Drops every node no root reaches, giving them in order.
    fn retire(&mut self) -> Vec<Node, Global> {
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
            for callee in calls {
                if marked.insert(Node::Function(*callee)) {
                    stack.push(Node::Function(*callee));
                }
            }
            for target in statics {
                if marked.insert(Node::Static(*target)) {
                    stack.push(Node::Static(*target));
                }
            }
        }
        let mut retired = Vec::new_in(Global);
        for symbol in self.fns.keys() {
            if !marked.contains(&Node::Function(*symbol)) {
                retired.push(Node::Function(*symbol));
            }
        }
        for symbol in self.statics.keys() {
            if !marked.contains(&Node::Static(*symbol)) {
                retired.push(Node::Static(*symbol));
            }
        }
        for node in &retired {
            match node {
                Node::Function(symbol) => {
                    if let Some(row) = self.fns.remove(symbol) {
                        self.dead[Section::Text.index()] += row.code.capacity;
                        self.dead[Section::Locations.index()] += row.locs.capacity;
                        self.forget_function(symbol, &row);
                    }
                }
                Node::Static(symbol) => {
                    if let Some(row) = self.statics.remove(symbol) {
                        self.dead[Section::Data.index()] += row.place.capacity;
                        for (target, reloc) in &row.out {
                            self.sites.remove(&(*target, *node, *reloc));
                        }
                    }
                }
            }
        }
        retired
    }

    /// Every page a change touches, and every page a section grew into.
    fn pages(&self, change: &PlacementChange, before: &[u64; 4]) -> Vec<(Section, u64), Global> {
        let page = self.machine.page.max(1);
        let mut pages = BTreeSet::new_in(Global);
        let mut span = |section: Section, offset: u64, len: u64| {
            if len == 0 {
                return;
            }
            for index in offset / page..=(offset + len - 1) / page {
                pages.insert((section, index));
            }
        };
        for write in &change.writes {
            span(write.section, write.offset, write.bytes.len() as u64);
        }
        for line in &change.lines {
            span(Section::Locations, line.offset + 16, 4);
        }
        for import in &change.imports {
            span(Section::Imports, u64::from(import.slot) * 8, 8);
        }
        for section in Section::ALL {
            let (old, new) = (before[section.index()], change.lengths[section.index()]);
            if new > old {
                span(section, old, new - old);
            }
        }
        let mut out = Vec::new_in(Global);
        out.extend(pages);
        out
    }

    fn encode_header(&self, out: &mut Out) {
        out.u8(self.machine.arch as u8);
        out.u64(self.machine.page);
        out.u64(self.machine.reserve);
        self.settings.encode(out);
        out.count(self.roots.len());
        for root in &self.roots {
            root.encode(out);
        }
        for length in self.lengths {
            out.u64(length);
        }
        for dead in self.dead {
            out.u64(dead);
        }
        out.u64(self.generation);
    }

    fn decode_header(input: &mut In<'_>) -> Result<Program, CodecError> {
        let arch = match input.u8()? {
            0 => Arch::X86_64,
            1 => Arch::Arm64,
            _ => return Err(input.error("an instruction set no machine has")),
        };
        let machine = Machine { arch, page: input.u64()?, reserve: input.u64()? };
        let settings = Settings::decode(input)?;
        let count = input.count(33)?;
        let mut roots = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            roots.push(Node::decode(input)?);
        }
        let mut program = Program::new(machine, settings, &roots, Plant::None);
        for length in &mut program.lengths {
            *length = input.u64()?;
        }
        for dead in &mut program.dead {
            *dead = input.u64()?;
        }
        program.generation = input.u64()?;
        Ok(program)
    }

    /// Each manifest entry's bytes, in table order.
    fn for_each_entry(&self, mut each: impl FnMut(&[u8])) {
        macro_rules! emit {
            ($fill:expr) => {{
                let mut entry = Out::new();
                ($fill)(&mut entry);
                each(&entry.finish());
            }};
        }
        for (symbol, row) in &self.fns {
            emit!(|out: &mut Out| {
                out.u8(0);
                symbol.encode(out);
                out.digest(&row.key.0);
                out.u8(row.tier as u8);
                encode_place(&row.code, out);
                encode_place(&row.locs, out);
                out.u64(row.since);
                encode_symbols(&row.calls, out);
                encode_symbols(&row.statics, out);
                encode_out(&row.out, out);
                out.digest(&row.plan.0);
                encode_planned(&row.inlined, out);
                out.count(row.loc_items.len());
                for item in &row.loc_items {
                    out.digest(&item.0);
                }
            });
        }
        for (symbol, row) in &self.statics {
            emit!(|out: &mut Out| {
                out.u8(1);
                symbol.encode(out);
                out.digest(&row.key.0);
                encode_place(&row.place, out);
                encode_symbols(&row.calls, out);
                encode_symbols(&row.statics, out);
                encode_out(&row.out, out);
            });
        }
        for (name, place) in &self.files {
            emit!(|out: &mut Out| {
                out.u8(2);
                out.bytes(name);
                encode_place(place, out);
            });
        }
        for (item, span) in &self.spans {
            emit!(|out: &mut Out| {
                out.u8(3);
                out.digest(&item.0);
                out.bytes(&span.file);
                out.u32(span.line);
            });
        }
        for ((target, referrer, reloc), site) in &self.sites {
            emit!(|out: &mut Out| {
                out.u8(4);
                target.encode(out);
                referrer.encode(out);
                out.u32(*reloc);
                out.u8(site.section as u8);
                out.u64(site.offset);
                out.u8(site.kind.tag());
                out.i64(site.addend);
                out.raw(&site.template);
            });
        }
        for ((item, symbol, index), (line, col)) in &self.loc_items {
            emit!(|out: &mut Out| {
                out.u8(5);
                out.digest(&item.0);
                symbol.encode(out);
                out.u32(*index);
                out.u32(*line);
                out.u32(*col);
            });
        }
        for (inlinee, inliner) in &self.inliners {
            emit!(|out: &mut Out| {
                out.u8(6);
                inlinee.encode(out);
                inliner.encode(out);
            });
        }
        for (name, slot) in &self.externs {
            emit!(|out: &mut Out| {
                out.u8(7);
                out.bytes(name);
                out.u32(*slot);
            });
        }
    }

    fn decode_entry(&mut self, input: &mut In<'_>) -> Result<(), CodecError> {
        match input.u8()? {
            0 => {
                let symbol = Symbol::decode(input)?;
                let key = InstanceKey(input.digest()?);
                let tier = decode_tier(input)?;
                let code = decode_place(input)?;
                let locs = decode_place(input)?;
                let since = input.u64()?;
                let calls = decode_symbols(input)?;
                let statics = decode_symbols(input)?;
                let out = decode_out(input)?;
                let plan = PlanKey(input.digest()?);
                let inlined = decode_planned(input)?;
                let count = input.count(32)?;
                let mut loc_items = Vec::with_capacity_in(count, Global);
                for _ in 0..count {
                    loc_items.push(ItemId(input.digest()?));
                }
                self.fns.insert(symbol, FnRow { key, tier, code, locs, since, calls, statics, out, plan, inlined, loc_items });
            }
            1 => {
                let symbol = Symbol::decode(input)?;
                let key = ValueKey(input.digest()?);
                let place = decode_place(input)?;
                let calls = decode_symbols(input)?;
                let statics = decode_symbols(input)?;
                let out = decode_out(input)?;
                self.statics.insert(symbol, StaticRow { key, place, calls, statics, out });
            }
            2 => {
                let name = input.bytes_owned()?;
                let place = decode_place(input)?;
                self.files.insert(name, place);
            }
            3 => {
                let item = ItemId(input.digest()?);
                let file = input.bytes_owned()?;
                let line = input.u32()?;
                self.spans.insert(item, SpanRow { file, line });
            }
            4 => {
                let target = Node::decode(input)?;
                let referrer = Node::decode(input)?;
                let reloc = input.u32()?;
                let section = match input.u8()? {
                    0 => Section::Text,
                    1 => Section::Data,
                    2 => Section::Locations,
                    3 => Section::Imports,
                    _ => return Err(input.error("a site in a section no program has")),
                };
                let offset = input.u64()?;
                let Some(kind) = RelocKind::from_tag(input.u8()?) else {
                    return Err(input.error("a site of a relocation kind no kind has"));
                };
                let addend = input.i64()?;
                let mut template = [0u8; 4];
                template.copy_from_slice(input.take(4)?);
                self.sites.insert((target, referrer, reloc), Site { section, offset, kind, addend, template });
            }
            5 => {
                let item = ItemId(input.digest()?);
                let symbol = Symbol::decode(input)?;
                let index = input.u32()?;
                let line = input.u32()?;
                let col = input.u32()?;
                self.loc_items.insert((item, symbol, index), (line, col));
            }
            6 => {
                let inlinee = Symbol::decode(input)?;
                let inliner = Symbol::decode(input)?;
                self.inliners.insert((inlinee, inliner));
            }
            7 => {
                let name = input.bytes_owned()?;
                let slot = input.u32()?;
                self.externs.insert(name, slot);
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
        out.extend(calls.iter().map(|callee| Node::Function(*callee)));
        out.extend(statics.iter().map(|target| Node::Static(*target)));
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

fn encode_place(place: &Place, out: &mut Out) {
    out.u64(place.offset);
    out.u64(place.size);
    out.u64(place.capacity);
}

fn decode_place(input: &mut In<'_>) -> Result<Place, CodecError> {
    Ok(Place { offset: input.u64()?, size: input.u64()?, capacity: input.u64()? })
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

fn encode_out(fields: &[(Node, u32)], out: &mut Out) {
    out.count(fields.len());
    for (node, reloc) in fields {
        node.encode(out);
        out.u32(*reloc);
    }
}

fn decode_out(input: &mut In<'_>) -> Result<Vec<(Node, u32), Global>, CodecError> {
    let count = input.count(69)?;
    let mut out = Vec::with_capacity_in(count, Global);
    for _ in 0..count {
        let node = Node::decode(input)?;
        out.push((node, input.u32()?));
    }
    Ok(out)
}
