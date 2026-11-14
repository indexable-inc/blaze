//! The deterministic simulation: a model program is built, then edited step by step (body,
//! signature, added call, removed function, changed type argument, static value, span move,
//! tier-up), each step walked with shuffled completions over one to three shards, placed, and
//! applied to a file image. After every step the oracles check, against a fresh build of the
//! same revision from an empty store that holds the same functions at the top tier (built by
//! the same seeds rule, [`Program::seeds`] with [`Program::top_tier`]):
//!
//! - the two hold the same nodes, each under the same key with the same content (its bytes
//!   with each field resolved to what it points at), and every root runs to the same events; a
//!   failure names the first node that differs and how ([`first_difference`]);
//! - the code records made are exactly the keys the program holds that the store did not;
//! - a test's key moved exactly when something it reaches changed;
//! - every page hash of the file is current, and the manifest round-trips (whole and chunked).
//!
//! Every choice (the model, the edits, the order of completions) is drawn from one tape, so a
//! failure shrinks to its plan and its schedule together. Every planted bug is caught by some
//! case ([`super::controls`]).

use std::collections::{BTreeMap, BTreeSet};

use blaze_sim::{Config, Flight, Nanos, Tape};

use super::model::{
    ARM64_BL, Model, ModelFront, OP_CALL, OP_HEAD, OP_PANIC, OP_RET, OP_STATIC, TYPES, chance, fn_symbol, item, static_symbol,
};
use crate::Plant;
use crate::change::{PlacementChange, Section, Sink, apply};
use crate::key::{DebugInfo, InstanceKey, ItemId, Node, Panic, PlanKey, Settings, Symbol, Tier};
use crate::program::{Arch, Delta, Machine, Program, ProgramError, Records};
use crate::reach::{reach, reach_digest};
use crate::record::{CodeHead, InstanceCode, Plan, RelocKind};
use crate::codec::{Digest, Hasher, Payload};
use crate::walk::{Answer, Ask, Fresh, Front, Prior, Seed, Walk, WalkConfig, Walked, shard_of};

/// The store: plans and code records by key, write-once.
#[derive(Default)]
pub(super) struct Store {
    plans: BTreeMap<PlanKey, Vec<u8>>,
    code: BTreeMap<InstanceKey, Vec<u8>>,
}

impl Records for Store {
    fn code(&self, key: &InstanceKey) -> Option<InstanceCode> {
        let bytes = self.code.get(key)?;
        match InstanceCode::decode(bytes) {
            Ok(code) => Some(code),
            Err(error) => panic!("the store's code record {key} does not decode: {error}"),
        }
    }
}

/// A file image: four sections, the imports bound, and a hash per page kept current from the
/// change's pages (the code directory of a signed arm64 macOS file).
pub(super) struct FileImage {
    page: u64,
    sections: [Vec<u8>; 4],
    imports: BTreeMap<u32, Vec<u8>>,
    hashes: BTreeMap<(Section, u64), Digest>,
    error: Option<String>,
}

impl FileImage {
    pub(super) fn new(page: u64) -> FileImage {
        FileImage { page, sections: Default::default(), imports: BTreeMap::new(), hashes: BTreeMap::new(), error: None }
    }

    fn page_hash(&self, section: Section, page: u64) -> Digest {
        let bytes = &self.sections[section.index()];
        let start = (page * self.page) as usize;
        let end = ((page + 1) * self.page).min(bytes.len() as u64) as usize;
        Digest::of("blaze program test page 1", &bytes[start.min(end)..end])
    }

    /// Every page's hash is current: the change named every page it touched.
    fn verify(&self) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let mut expected = BTreeMap::new();
        for section in Section::ALL {
            let len = self.sections[section.index()].len() as u64;
            for page in 0..len.div_ceil(self.page) {
                expected.insert((section, page), self.page_hash(section, page));
            }
        }
        for (page, hash) in &expected {
            if self.hashes.get(page) != Some(hash) {
                return Err(format!("page {page:?} of the file has a stale code-directory hash"));
            }
        }
        Ok(())
    }

    fn bytes(&self, section: Section, offset: u64, len: u64) -> Option<&[u8]> {
        self.sections[section.index()].get(offset as usize..(offset + len) as usize)
    }
}

impl Sink for FileImage {
    fn grow(&mut self, section: Section, len: u64) {
        let bytes = &mut self.sections[section.index()];
        if (bytes.len() as u64) < len {
            bytes.resize(len as usize, 0);
        }
    }

    fn write(&mut self, section: Section, offset: u64, bytes: &[u8]) {
        let target = &mut self.sections[section.index()];
        let end = offset as usize + bytes.len();
        if end > target.len() {
            self.error = Some(format!("a write past the end of {section:?}: {end} > {}", target.len()));
            return;
        }
        target[offset as usize..end].copy_from_slice(bytes);
    }

    fn bind(&mut self, slot: u32, name: &[u8]) {
        self.imports.insert(slot, name.to_vec());
    }

    fn touched(&mut self, pages: &[(Section, u64)]) {
        for (section, page) in pages {
            let hash = self.page_hash(*section, *page);
            self.hashes.insert((*section, *page), hash);
        }
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
                        let head = match store.code.get(&key) {
                            Some(bytes) => Some(CodeHead::decode_prefix(bytes).map_err(|error| error.to_string())?),
                            None => None,
                        };
                        pending.ask((shard, Answer::Code { symbol, key, head }));
                    }
                    Ask::MakeCode { symbol, key, tier, inline, .. } => {
                        if store.code.contains_key(&key) {
                            return Err(format!("the code of {symbol} was made again under a key the store holds"));
                        }
                        let code = front.make_code(&symbol, tier, &inline);
                        let head = code.head();
                        store.code.insert(key, code.encode());
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
/// and a patched program decide which nodes have rows by one rule.
pub(super) fn fresh(
    front: &ModelFront,
    machine: Machine,
    shards: u32,
    plant: Plant,
    top: &[Symbol],
    tape: &Tape,
) -> Result<(Program, FileImage, Store), String> {
    let mut store = Store::default();
    let mut machine = machine;
    let delta = Delta { tier_up: top.to_vec(), ..Delta::default() };
    for _ in 0..8 {
        let mut program = Program::new(machine, front.settings, &roots(&front.model), plant);
        let seeds = program.seeds(&delta);
        // A whole build: every node is new, and only `top` is made at the top tier.
        let (walked, unreached, _) = walk(front, &mut store, &seeds, &Fresh, shards, plant, Tier::Edit, tape)?;
        match program.place(&walked, &unreached, &[], front, &store) {
            Ok(change) => {
                let mut image = FileImage::new(machine.page);
                apply(&change, &mut image);
                return Ok((program, image, store));
            }
            Err(ProgramError::Outgrown { .. }) => machine = machine.doubled(),
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("the fresh build outgrew its reserve eight times".to_string())
}

/// What a value or function's bytes point at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Id {
    Function(Symbol),
    Static(Symbol),
    Location { file: Vec<u8>, line: u32, col: u32 },
    Extern(Vec<u8>),
    Dangling(Section, u64),
    Nowhere(u64),
}

fn section_of(machine: &Machine, address: u64) -> Option<(Section, u64)> {
    for section in Section::ALL {
        let base = machine.base(section);
        if address >= base && address < base + machine.reserve {
            return Some((section, address - base));
        }
    }
    None
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(word)
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(word)
}

/// What `address` in `image` is, by `program`'s manifest.
pub(super) fn identify(program: &Program, image: &FileImage, address: u64) -> Id {
    let machine = program.machine();
    let Some((section, offset)) = section_of(machine, address) else { return Id::Nowhere(address) };
    match section {
        Section::Text => {
            for (symbol, row) in program.functions() {
                if row.code.offset == offset {
                    return Id::Function(*symbol);
                }
            }
        }
        Section::Data => {
            for (symbol, row) in program.statics() {
                if row.place.offset == offset {
                    return Id::Static(*symbol);
                }
            }
        }
        Section::Locations => {
            if let Some(record) = image.bytes(Section::Locations, offset, 24) {
                let (file_address, len) = (read_u64(&record[..8]), read_u64(&record[8..16]));
                let (line, col) = (read_u32(&record[16..20]), read_u32(&record[20..24]));
                if let Some((Section::Data, file_offset)) = section_of(machine, file_address) {
                    if let Some(file) = image.bytes(Section::Data, file_offset, len) {
                        return Id::Location { file: file.to_vec(), line, col };
                    }
                }
            }
        }
        Section::Imports => {
            if let Some(name) = image.imports.get(&((offset / 8) as u32)) {
                return Id::Extern(name.clone());
            }
        }
    }
    Id::Dangling(section, offset)
}

/// A function's or static's content in an image: its bytes with every field zeroed, and what
/// each field points at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct View {
    bytes: Vec<u8>,
    targets: Vec<Id>,
}

fn view_of(program: &Program, image: &FileImage, section: Section, offset: u64, len: u64, relocs: &[crate::record::Reloc]) -> Result<View, String> {
    let Some(placed) = image.bytes(section, offset, len) else {
        return Err(format!("{section:?} holds no {len} bytes at {offset}"));
    };
    let mut bytes = placed.to_vec();
    let mut targets = Vec::new();
    let base = program.machine().base(section) + offset;
    for reloc in relocs {
        let at = reloc.at as usize;
        let Some(target) = reloc.kind.read(&bytes, at, base + u64::from(reloc.at), reloc.addend) else {
            return Err(format!("a field at {at} runs past its bytes"));
        };
        targets.push(identify(program, image, target));
        let width = reloc.kind.width();
        match reloc.kind {
            RelocKind::Arm64Branch26 => bytes[at..at + 4].copy_from_slice(&ARM64_BL.to_le_bytes()),
            _ => bytes[at..at + width].fill(0),
        }
    }
    Ok(View { bytes, targets })
}

/// Every node's view in an image, with the key it is held under.
pub(super) fn views(program: &Program, image: &FileImage, store: &Store, front: &ModelFront) -> Result<BTreeMap<Node, (Digest, View)>, String> {
    let mut out = BTreeMap::new();
    for (symbol, row) in program.functions() {
        let Some(code) = store.code(&row.key) else { return Err(format!("no record for {symbol}")) };
        let view = view_of(program, image, Section::Text, row.code.offset, code.placed_len(), code.relocs())?;
        out.insert(Node::Function(*symbol), (row.key.0, view));
    }
    for (symbol, row) in program.statics() {
        let Some(value) = front.static_value(symbol) else { return Err(format!("no value for {symbol}")) };
        let view = view_of(program, image, Section::Data, row.place.offset, value.bytes().len() as u64, value.relocs())?;
        out.insert(Node::Static(*symbol), (row.key.0, view));
    }
    Ok(out)
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

fn show(front: &ModelFront, id: &Id) -> String {
    match id {
        Id::Function(symbol) => name(front, &Node::Function(*symbol)),
        Id::Static(symbol) => name(front, &Node::Static(*symbol)),
        other => format!("{other:?}"),
    }
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

/// The first place two views of one node differ.
fn view_difference(front: &ModelFront, ours: &View, theirs: &View) -> String {
    if ours.bytes.len() != theirs.bytes.len() {
        return format!("{} bytes here, {} in a fresh build", ours.bytes.len(), theirs.bytes.len());
    }
    if let Some(at) = (0..ours.bytes.len()).find(|at| ours.bytes[*at] != theirs.bytes[*at]) {
        return format!("byte {at} is {:#04x} here, {:#04x} in a fresh build", ours.bytes[at], theirs.bytes[at]);
    }
    for (index, (mine, fresh)) in ours.targets.iter().zip(&theirs.targets).enumerate() {
        if mine != fresh {
            return format!("field {index} points at {} here, at {} in a fresh build", show(front, mine), show(front, fresh));
        }
    }
    format!("{} fields here, {} in a fresh build", ours.targets.len(), theirs.targets.len())
}

/// The first node, in node order, where a patched program and a fresh build differ, and why: a
/// missing row, an extra row, another key, or other bytes (the first byte or field that
/// differs), with both programs' rows of it. `None` when every node is the same.
pub(super) fn first_difference(
    ours: (&Program, &FileImage, &Store),
    theirs: (&Program, &FileImage, &Store),
    front: &ModelFront,
) -> Result<Option<String>, String> {
    let our_views = views(ours.0, ours.1, ours.2, front)?;
    let their_views = views(theirs.0, theirs.1, theirs.2, front)?;
    let mut nodes: BTreeSet<Node> = our_views.keys().copied().collect();
    nodes.extend(their_views.keys().copied());
    for node in &nodes {
        let named = name(front, node);
        let both = || format!("patched: {}; fresh: {}", row_of(ours.0, front, node), row_of(theirs.0, front, node));
        match (our_views.get(node), their_views.get(node)) {
            (None, Some(_)) => {
                return Ok(Some(format!(
                    "missing row: {named} has a row in a fresh build ({}; referred to by [{}]) and none in the patched program (referred to by [{}])",
                    row_of(theirs.0, front, node),
                    referrers(theirs.0, front, node),
                    referrers(ours.0, front, node)
                )));
            }
            (Some(_), None) => {
                return Ok(Some(format!(
                    "extra row: {named} has a row in the patched program ({}; referred to by [{}]) and none in a fresh build (referred to by [{}])",
                    row_of(ours.0, front, node),
                    referrers(ours.0, front, node),
                    referrers(theirs.0, front, node)
                )));
            }
            (Some((key, view)), Some((their_key, their_view))) => {
                if key != their_key {
                    return Ok(Some(format!("other key: {named} ({})", both())));
                }
                if view != their_view {
                    return Ok(Some(format!(
                        "other bytes: {named}: {} ({})",
                        view_difference(front, view, their_view),
                        both()
                    )));
                }
            }
            (None, None) => {}
        }
    }
    Ok(None)
}

/// What a run of a program prints. A static read prints the static's content (its bytes with
/// each field zeroed, and what each field points at), not its raw bytes: a field holds an
/// address, which is where the layout put a node, not what the program does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Event {
    Head([u8; 8]),
    Static(View),
    Panic(Id, Id),
    Bad(String),
}

/// Runs `entry` in `image`, giving the first `budget` events.
pub(super) fn trace(program: &Program, image: &FileImage, front: &ModelFront, arch: Arch, entry: &Symbol, budget: usize) -> Vec<Event> {
    let mut out = Vec::new();
    let Some(address) = program.address_of(&Node::Function(*entry)) else {
        return vec![Event::Bad(format!("{entry} is not placed"))];
    };
    execute(program, image, front, arch, address, budget, &mut out);
    out
}

fn execute(program: &Program, image: &FileImage, front: &ModelFront, arch: Arch, address: u64, budget: usize, out: &mut Vec<Event>) {
    let machine = *program.machine();
    let text = &image.sections[Section::Text.index()];
    let Some((Section::Text, mut at)) = section_of(&machine, address) else {
        out.push(Event::Bad(format!("a call to {address:#x}, outside the code")));
        return;
    };
    let (kind, addend) = match arch {
        Arch::X86_64 => (RelocKind::Rel32, -4),
        Arch::Arm64 => (RelocKind::Arm64Branch26, 0),
    };
    let base = machine.base(Section::Text);
    loop {
        if out.len() >= budget {
            return;
        }
        let Some(op) = text.get(at as usize) else {
            out.push(Event::Bad("the code runs off the end of the section".to_string()));
            return;
        };
        at += 4;
        match *op {
            OP_HEAD => {
                let mut head = [0u8; 8];
                head.copy_from_slice(&text[at as usize..at as usize + 8]);
                at += 8;
                out.push(Event::Head(head));
            }
            OP_CALL => {
                let target = kind.read(text, at as usize, base + at, addend);
                at += 4;
                match target.map(|target| identify(program, image, target)) {
                    Some(Id::Function(_)) => {
                        if let Some(target) = target {
                            execute(program, image, front, arch, target, budget, out);
                        }
                    }
                    other => out.push(Event::Bad(format!("a call to {other:?}"))),
                }
            }
            OP_STATIC => {
                let target = RelocKind::Abs64.read(text, at as usize, base + at, 0);
                at += 8;
                match target.map(|target| identify(program, image, target)) {
                    Some(Id::Static(symbol)) => {
                        let row = &program.statics()[&symbol];
                        let Some(value) = front.static_value(&symbol) else {
                            out.push(Event::Bad(format!("a read of static {symbol}, which has no value")));
                            return;
                        };
                        match view_of(program, image, Section::Data, row.place.offset, row.place.size, value.relocs()) {
                            Ok(view) => out.push(Event::Static(view)),
                            Err(why) => {
                                out.push(Event::Bad(format!("a read of static {symbol}: {why}")));
                                return;
                            }
                        }
                    }
                    other => out.push(Event::Bad(format!("a static read of {other:?}"))),
                }
            }
            OP_PANIC => {
                let location = RelocKind::Abs64.read(text, at as usize, base + at, 0);
                at += 8;
                let call = kind.read(text, at as usize, base + at, addend);
                at += 4;
                let location = location.map_or(Id::Nowhere(0), |address| identify(program, image, address));
                let call = call.map_or(Id::Nowhere(0), |address| identify(program, image, address));
                out.push(Event::Panic(location, call));
            }
            OP_RET => return,
            other => {
                out.push(Event::Bad(format!("an op {other:#x} no op is")));
                return;
            }
        }
    }
}

/// What a test can observe of the model: every instance it can call (by its body key and its
/// type arguments), every static it can read, every span its panics print. From the model
/// alone, never the program: not the tier an instance is held at, nor whether it is placed or
/// taken in, as neither changes what the test observes.
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
        if model.fns[index].panics > 0 {
            let span = front.span(&item(index)).map(|span| (span.file, span.line)).unwrap_or_default();
            hasher.part(&span.0).update(&span.1.to_le_bytes());
        }
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
    pub(super) image: FileImage,
    pub(super) shards: u32,
    pub(super) plant: Plant,
    pub(super) arch: Arch,
    /// Of the last step: the delta, the code records made, whether the reserve was outgrown
    /// (and the revision built whole), and the bytes its changes wrote.
    pub(super) last_delta: Delta,
    pub(super) last_makes: Vec<InstanceKey>,
    pub(super) rebuilt: bool,
    pub(super) written: u64,
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
    /// A body edit that adds a panic: the function's code grows by 16 bytes.
    Panic,
    /// A body edit that marks the function `#[inline(always)]`: every caller takes it in.
    Always,
    /// An edit of the type with this index (its content key moves).
    Retype(u8),
}

impl World {
    pub(super) fn new(tape: &Tape, plant: Plant) -> Result<World, String> {
        let model = Model::generate(tape);
        let arch = if chance(tape, 1, 2, "arm64") { Arch::Arm64 } else { Arch::X86_64 };
        let shards = 1 + tape.below(3, "more shards") as u32;
        let reserve = if chance(tape, 1, 3, "small reserve") { 1 << 12 } else { 1 << 16 };
        World::of(model, tape.clone(), arch, shards, reserve, plant)
    }

    /// A world over a model a test wrote.
    pub(super) fn of(model: Model, tape: Tape, arch: Arch, shards: u32, reserve: u64, plant: Plant) -> Result<World, String> {
        let machine = Machine { arch, page: 256, reserve };
        let front = ModelFront::new(&model, settings(), arch);
        let (program, image, store) = fresh(&front, machine, shards, plant, &[], &tape)?;
        Ok(World {
            tape,
            model,
            front,
            store,
            program,
            image,
            shards,
            plant,
            arch,
            last_delta: Delta::default(),
            last_makes: Vec::new(),
            rebuilt: false,
            written: 0,
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
            Edit::Panic => {
                model.fns[pick].text += 1;
                model.fns[pick].panics += 1;
            }
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
        let front = ModelFront::new(&model, settings(), self.arch);
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
            let reached = reach(&self.program, &entry, self.plant);
            out.push(reach_digest(&reached));
        }
        out
    }

    fn model_reaches(&self) -> Vec<Digest> {
        self.model.tests.iter().map(|test| model_reach(&self.front, *test)).collect()
    }

    /// Places `walked`; on an outgrown reserve, builds the revision whole at twice the reserve.
    fn place(&mut self, walked: &[Walked], unreached: &[Node], moved: &[ItemId], makes: &mut Vec<InstanceKey>) -> Result<(), String> {
        match self.program.place(walked, unreached, moved, &self.front, &self.store) {
            Ok(change) => self.apply(&change),
            Err(ProgramError::Outgrown { .. }) => {
                let machine = self.program.machine().doubled();
                let mut program = Program::new(machine, self.front.settings, &roots(&self.model), self.plant);
                let seeds = program.seeds(&Delta::default());
                let (walked, unreached, more) =
                    walk(&self.front, &mut self.store, &seeds, &Fresh, self.shards, self.plant, Tier::Edit, &self.tape)?;
                makes.extend(more);
                let change = program.place(&walked, &unreached, &[], &self.front, &self.store).map_err(|error| error.to_string())?;
                self.program = program;
                self.rebuilt = true;
                self.apply(&change)
            }
            Err(error) => Err(error.to_string()),
        }?;
        if self.program.compaction_due() {
            let (program, change) = self
                .program
                .relayout(*self.program.machine(), &self.front, &self.store)
                .map_err(|error| error.to_string())?;
            self.program = program;
            self.apply(&change)?;
        }
        Ok(())
    }

    fn apply(&mut self, change: &PlacementChange) -> Result<(), String> {
        if change.whole {
            self.image = FileImage::new(self.program.machine().page);
        }
        self.written += change.bytes_written();
        apply(change, &mut self.image);
        Ok(())
    }

    /// One edit, walked, placed and checked by every oracle.
    pub(super) fn step(&mut self, edit: Edit) -> Result<(), String> {
        let tests_before = self.test_reaches();
        let reaches_before = self.model_reaches();
        let delta = self.edit(edit);
        let store_before: BTreeSet<InstanceKey> = self.store.code.keys().copied().collect();
        let seeds = self.program.seeds(&delta);
        let program = self.program.clone();
        self.rebuilt = false;
        self.written = 0;
        let (walked, unreached, mut makes) =
            walk(&self.front, &mut self.store, &seeds, &program, self.shards, self.plant, delta.new_at(), &self.tape)?;
        let first_walk: BTreeSet<InstanceKey> = makes.iter().copied().collect();
        self.place(&walked, &unreached, &delta.moved, &mut makes)?;

        // The makes are exactly the held keys the store did not have (and, when the reserve
        // was outgrown, what the abandoned edit walk made before the whole walk).
        let made: BTreeSet<InstanceKey> = makes.iter().copied().collect();
        if made.len() != makes.len() {
            return Err("a code record was made twice".to_string());
        }
        let held: BTreeSet<InstanceKey> = self.program.functions().values().map(|row| row.key).collect();
        let expected: BTreeSet<InstanceKey> = held.difference(&store_before).copied().collect();
        let extra: BTreeSet<InstanceKey> = made.difference(&expected).copied().collect();
        let abandoned = self.rebuilt && extra.is_subset(&first_walk);
        if !expected.is_subset(&made) || !(extra.is_empty() || abandoned) {
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
            let thrown: Vec<String> = extra.iter().map(|key| named(key)).collect();
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

    /// The image against a fresh build of the same revision from an empty store.
    pub(super) fn check(&mut self) -> Result<(), String> {
        self.image.verify()?;
        // The same revision built whole, holding the same functions at the top tier: every
        // node, its key and its content must be the same.
        let top = self.program.top_tier();
        let (program, image, store) = fresh(&self.front, *self.program.machine(), self.shards, self.plant, &top, &self.tape)?;
        if let Some(why) = first_difference((&self.program, &self.image, &self.store), (&program, &image, &store), &self.front)? {
            return Err(why);
        }
        for root in roots(&self.model) {
            let symbol = root.symbol();
            let our_run = trace(&self.program, &self.image, &self.front, self.arch, &symbol, 300);
            let their_run = trace(&program, &image, &self.front, self.arch, &symbol, 300);
            if our_run != their_run {
                let at = our_run
                    .iter()
                    .zip(&their_run)
                    .position(|(mine, fresh)| mine != fresh)
                    .unwrap_or(our_run.len().min(their_run.len()));
                return Err(format!(
                    "a run of {} differs from a fresh build's at event {at} of {} here and {} there: {:?} here, {:?} in a fresh build",
                    name(&self.front, &root),
                    our_run.len(),
                    their_run.len(),
                    our_run.get(at),
                    their_run.get(at)
                ));
            }
            if let Some(Event::Bad(why)) = our_run.iter().find(|event| matches!(event, Event::Bad(_))) {
                return Err(format!("a run of {symbol}: {why}"));
            }
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
/// revision holding the same functions at the top tier (every node, its key and its content),
/// and runs the same; and a test's key moves exactly when what the test can observe moved (the
/// body keys and type arguments of every instance it can run, the values of the statics it can
/// read, the spans its panics print), which a tier never changes: a tier-up, of any instance a
/// test runs placed or taken in, never moves a test's key, so it never runs a test again.
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
/// nothing else; a span move remakes nothing and writes 4 bytes per location record in the
/// moved item.
#[test]
fn edits_cost_their_floor() {
    // Cases run, and those of them that kept their reserve: more than half must have.
    let mut ran = 0u32;
    let mut tried = 0u32;
    blaze_sim::check(Config::cases(64).first(200), |tape| {
        ran += 1;
        let mut world = match World::new(tape, Plant::None) {
            Ok(world) => world,
            Err(why) => panic!("first build: {why}"),
        };
        // Some cases tier everything up first, so the inliners are the top tier's.
        // (A row tiers up once it has been unchanged for one build, so a build passes first.)
        if tape.coin("tier up first") {
            world.step(Edit::MoveSpan).unwrap_or_else(|why| panic!("span move: {why}"));
            world.step(Edit::TierUp).unwrap_or_else(|why| panic!("tier-up: {why}"));
            if !world.rebuilt {
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
        }
        let live: Vec<usize> = (0..world.model.fns.len()).filter(|index| world.model.fns[*index].alive).collect();
        let target = live[tape.index(live.len(), "target")];
        let target_item = item(target);
        let mut expected = BTreeSet::new();
        for (symbol, row) in world.program.functions() {
            let inlines_it = row.inlined.iter().any(|(inlinee, _)| inlinee.item == target_item);
            if symbol.item == target_item || inlines_it {
                expected.insert(*symbol);
            }
        }
        world.pick = Some(target);
        world.step(Edit::Text).unwrap_or_else(|why| panic!("text edit: {why}"));
        if world.rebuilt {
            return;
        }
        let mut remade = BTreeSet::new();
        for (symbol, row) in world.program.functions() {
            if world.last_makes.contains(&row.key) {
                remade.insert(*symbol);
            }
        }
        assert_eq!(world.last_makes.len(), remade.len(), "a make the program does not hold");
        assert_eq!(remade, expected, "a text edit of function {target} remade other than it and its inliners");

        // A span move of the same function.
        world.pick = Some(target);
        world.step(Edit::MoveSpan).unwrap_or_else(|why| panic!("span move: {why}"));
        assert!(world.last_makes.is_empty(), "a span move made a code record");
        let mut records = 0u64;
        for row in world.program.functions().values() {
            records += row.loc_items.iter().filter(|located| **located == target_item).count() as u64;
        }
        if !world.rebuilt {
            assert_eq!(world.written, 4 * records, "a span move wrote other than 4 bytes per location");
        }
        tried += 1;
    });
    // Of 64 cases, more than 32: more than half.
    assert!(2 * tried > ran, "only {tried} of {ran} cases kept their reserve");
}

/// A reached body that fails its check stops the build with an error naming the body and the
/// check; a body nothing reaches is never checked.
#[test]
fn a_reached_refused_body_stops_the_walk() {
    let tape = Tape::seeded(7);
    let mut model = Model::generate(&tape);
    let machine = Machine { arch: Arch::Arm64, page: 256, reserve: 1 << 16 };
    let reached = model.fns[0].calls[0].callee;
    model.refused = Some(reached);
    let refusing = ModelFront::new(&model, settings(), Arch::Arm64);
    let error = match fresh(&refusing, machine, 1, Plant::None, &[], &tape) {
        Ok(_) => panic!("a build reaching a refused body succeeded"),
        Err(error) => error,
    };
    assert!(error.contains("fails its check"), "the error names the check: {error}");
    assert!(error.contains(&item(reached).0.to_string()), "the error names the body: {error}");
    let unreached = model.fns.len();
    model.fns.push(model.fns[1].clone());
    model.refused = Some(unreached);
    let unreferenced = ModelFront::new(&model, settings(), Arch::Arm64);
    if let Err(why) = fresh(&unreferenced, machine, 1, Plant::None, &[], &tape) {
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
/// took it in. The seeds found an edited item's inliners only through its placed rows, so
/// `main` kept running the old copy of function 3.
#[test]
fn an_edit_of_an_instance_only_inlined_remakes_its_inliners() {
    for arch in [Arch::Arm64, Arch::X86_64] {
        let model = main_calls_three(crate::inline::InlineAttr::Always, 10);
        let mut world = World::of(model, Tape::seeded(1), arch, 1, 1 << 16, Plant::None)
            .unwrap_or_else(|why| panic!("{arch:?}, first build: {why}"));
        assert!(
            !world.program.functions().keys().any(|symbol| symbol.item == item(3)),
            "{arch:?}: function 3 is placed alone, though `main` takes it in at the fast tier"
        );
        world.pick = Some(3);
        world.step(Edit::Text).unwrap_or_else(|why| panic!("{arch:?}, text edit of function 3: {why}"));
        let main = fn_symbol(&world.model, 0, &[]);
        let main_key = world.program.functions()[&main].key;
        assert_eq!(world.last_makes, vec![main_key], "{arch:?}: the edit remakes `main`, and only it");
    }
}

/// Regression (seed 0 of `patched_programs_equal_fresh_builds`, seed 204 of
/// `edits_cost_their_floor`): a tier-up makes only records the program then holds. Function 3
/// is `#[inline]` and small: called at the fast tier, taken into `main` at the top tier.
/// Tiering both up must make `main` alone; making function 3 at the top tier too is a record
/// made to be thrown away (its only caller takes it in).
#[test]
fn a_tier_up_makes_only_what_the_program_holds() {
    for arch in [Arch::Arm64, Arch::X86_64] {
        let model = main_calls_three(crate::inline::InlineAttr::Hint, 40);
        let mut world = World::of(model, Tape::seeded(2), arch, 1, 1 << 16, Plant::None)
            .unwrap_or_else(|why| panic!("{arch:?}, first build: {why}"));
        assert!(
            world.program.functions().keys().any(|symbol| symbol.item == item(3)),
            "{arch:?}: function 3 is not placed at the fast tier, which takes nothing in"
        );
        // A build passes, so every row is old enough to tier up.
        world.pick = Some(1);
        world.step(Edit::MoveSpan).unwrap_or_else(|why| panic!("{arch:?}, span move: {why}"));
        world.step(Edit::TierUp).unwrap_or_else(|why| panic!("{arch:?}, tier-up: {why}"));
        assert!(
            !world.program.functions().keys().any(|symbol| symbol.item == item(3)),
            "{arch:?}: function 3 is still placed alone after `main` took it in"
        );
        let main = fn_symbol(&world.model, 0, &[]);
        assert!(world.last_makes.contains(&world.program.functions()[&main].key), "{arch:?}: `main` was not remade");
        // Test 2 is a dependency's (2 % 3 == 2), at the top tier from the first build.
        assert_eq!(world.last_makes.len(), 2, "{arch:?}: the tier-up makes `main` and test 1, and nothing else");
    }
}

/// Regression (seed 100 of `every_edit_kind_holds`, its Body step): a run reads a static whose
/// value points at a function an edit moved. The function outgrows its place, is appended at
/// the text's tail and its referrers repatched, so its address is not a fresh build's; the
/// static's content (what its field points at) is the same. The run oracle printed a static's
/// raw bytes, its field's address with them, and so failed a correct program. A run prints a
/// static's content now, as the node oracle compares it.
#[test]
fn a_static_pointing_at_a_moved_function_reads_the_same() {
    for arch in [Arch::Arm64, Arch::X86_64] {
        let mut model = main_calls_three(crate::inline::InlineAttr::None, 40);
        model.fns[0].statics = vec![0];
        model.statics = vec![super::model::ModelStatic { bytes: [7; 8], fn_ref: Some(1) }];
        let mut world = World::of(model, Tape::seeded(3), arch, 1, 1 << 16, Plant::None)
            .unwrap_or_else(|why| panic!("{arch:?}, first build: {why}"));
        let one = Node::Function(fn_symbol(&world.model, 1, &[]));
        let before = world.program.address_of(&one);
        world.pick = Some(1);
        world.step(Edit::Panic).unwrap_or_else(|why| panic!("{arch:?}, a panic added to function 1: {why}"));
        assert!(!world.rebuilt, "{arch:?}: the reserve was outgrown, so nothing was patched");
        assert_ne!(world.program.address_of(&one), before, "{arch:?}: function 1 did not move, so the case is not tested");
    }
}

/// Regression (seed 0, step 7 of `patched_programs_equal_fresh_builds`): an edit that makes a
/// function `#[inline(always)]` makes its callers take it in, so its own instance leaves the
/// set of rows. It was seeded as an edit and made for its own sake, then retired: a record
/// made to be thrown away. By the row rule it is stale with only walked referrers, so walked
/// only if reached, and never reached.
#[test]
fn an_instance_its_callers_now_take_in_is_not_made() {
    for arch in [Arch::Arm64, Arch::X86_64] {
        let model = main_calls_three(crate::inline::InlineAttr::None, 40);
        let mut world = World::of(model, Tape::seeded(4), arch, 1, 1 << 16, Plant::None)
            .unwrap_or_else(|why| panic!("{arch:?}, first build: {why}"));
        world.pick = Some(3);
        world.step(Edit::Always).unwrap_or_else(|why| panic!("{arch:?}, function 3 made inline(always): {why}"));
        assert!(
            !world.program.functions().keys().any(|symbol| symbol.item == item(3)),
            "{arch:?}: function 3 kept a row though `main` takes it in"
        );
        let main = fn_symbol(&world.model, 0, &[]);
        assert_eq!(world.last_makes, vec![world.program.functions()[&main].key], "{arch:?}: the edit makes `main`, and only it");
    }
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
    for arch in [Arch::Arm64, Arch::X86_64] {
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
        let mut world = World::of(model, Tape::seeded(5), arch, 1, 1 << 16, Plant::None)
            .unwrap_or_else(|why| panic!("{arch:?}, first build: {why}"));
        let old_four = fn_symbol(&world.model, 4, &[0]);
        assert!(world.program.functions().contains_key(&old_four), "{arch:?}: 4<0> is not placed, so the case is not tested");
        world.step(Edit::Retype(0)).unwrap_or_else(|why| panic!("{arch:?}, type 0 edited: {why}"));
        assert!(
            !world.program.functions().contains_key(&old_four),
            "{arch:?}: the old 4<0> kept a row after its type moved"
        );
    }
}

/// The core's framing: every record kind this crate files is proved the record of its key by
/// the core's one check ([`blaze_keyed::proof::hit`]), gives back the value through the one
/// payload reader, and is refused under another key.
#[test]
fn records_round_trip_through_the_core_framing() {
    use blaze_format::Aligned;
    use blaze_keyed::proof;

    use crate::keyed;

    let world = World::new(&Tape::seeded(3), Plant::None).unwrap_or_else(|why| panic!("{why}"));
    proof::scope(|snap| {
        for (symbol, row) in world.program.functions() {
            let Some(code) = world.store.code(&row.key) else { panic!("no record of {symbol}") };
            let filed = Aligned::copy_from(&keyed::write_code(&row.key, &code));
            let holds = proof::hit(snap, keyed::code_key(&row.key), &filed).unwrap_or_else(|error| panic!("{error}")).holds();
            assert_eq!(keyed::read_code(&holds, &filed), Ok(code.clone()));
            assert_eq!(keyed::read_code_head(&holds, &filed), Ok(code.head()));
            let other = InstanceKey(Digest::of("blaze program test other key 1", &row.key.0.0));
            assert!(proof::hit(snap, keyed::code_key(&other), &filed).is_err(), "a code record proved under another key");
        }
        for (key, bytes) in &world.store.plans {
            let plan = Plan::decode(bytes).unwrap_or_else(|error| panic!("{error}"));
            let filed = Aligned::copy_from(&keyed::write_plan(key, &plan));
            let holds = proof::hit(snap, keyed::plan_key(key), &filed).unwrap_or_else(|error| panic!("{error}")).holds();
            assert_eq!(keyed::read_plan(&holds, &filed), Ok(plan));
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
