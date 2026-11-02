//! The functions of each workspace's program as a Flecs world, as its builds' records say them,
//! the snapshot of it a page is sent first, and Flecs's REST over it (docs/web.md).
//!
//! What it holds:
//!
//! - a workspace: `Workspace { root, build }`, unnamed; in it (Flecs's `Parent`) each crate the
//!   build put a function of, `Crate`, named as the crate, and in a crate its modules, `Module`,
//!   named as the module, one in another as in the crate;
//! - a function: `Function { key, name, how, ns, build }`, unnamed, in the module its path is
//!   in ([`each_module`]: a type's path is a module here, as on the page), or the crate when
//!   it is in none, and `Callees`, the functions it calls;
//! - a module's `Callees`, the other modules its functions call, and `(Calls, crate)` on a
//!   crate one of whose functions calls one of that crate's. A call that is gone stays until
//!   the program is written again. Only crates' calls are pairs: anim's 150 000 calls of
//!   functions would hold 266 MB as pairs that fragment and 1.9 GB as pairs that do not, and
//!   with its 20 000 calls of modules as pairs and `ChildOf` for `Parent`, the daemon held
//!   318 MB after anim's build and two edits, against 163 MB.
//!
//! A function placed allocates nothing once its crate and module are known: its name goes to
//! Flecs through a reused buffer, its module is one hash of its path, and a call to an id not
//! yet named waits in a chain of its `(object, id)` rather than in a list every record scans.
//!
//! One thread has the world: [`World`] is not `Send`. Flecs is reached only through the
//! crate's `flecs` module, so this module is safe code.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_protocol::events::{Event, How, Record};

pub use crate::flecs::Answer;
use crate::flecs::{Entity, Flecs, Tag};

/// What a record did to a program.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// It is in the program as the page is told it: the page is sent it.
    Told,
    /// It began the program again: the page is sent it anew.
    Anew,
}

/// A Flecs world of programs.
pub struct World {
    flecs: Flecs,
    scratch: Scratch,
}

/// Buffers reused by every record, so placing a function allocates nothing.
#[derive(Default)]
struct Scratch {
    /// A module's key in [`Program::modules`], and where each of its segments' keys ends.
    path: Vec<u8>,
    ends: Vec<usize>,
    /// A module's segments, as byte ranges of the function's name.
    segments: Vec<(usize, usize)>,
    callees: Vec<Entity>,
    placed: Vec<u32>,
    calls: Vec<(u32, u32)>,
    /// Calls of the record to ids none of its object's records has named yet: the id, the caller.
    unnamed: Vec<(u32, u32)>,
    /// Calls the record named the callee of: when each waited, the caller and the callee.
    named: Vec<(u32, u32, u32)>,
    changed: Vec<u32>,
    changed_modules: Vec<Entity>,
}

/// No call waits after it.
const END: u32 = u32::MAX;

/// A call to an id not yet named, in its `(object, id)`'s chain: 8 bytes.
#[derive(Clone, Copy)]
struct Waiting {
    from: u32,
    next: u32,
}

const _: () = assert!(size_of::<Waiting>() == 8);

/// The program a workspace builds, and the records of its last build.
pub struct Program {
    root: PathBuf,
    entity: Entity,
    build: u64,
    /// One more each time what the snapshot says may have changed.
    changes: u64,
    crates: Vec<String>,
    /// Each crate's entity, by its index in `crates`.
    crates_of: Vec<Entity>,
    crate_of: HashMap<String, (u32, Entity)>,
    /// The crate named last: a record's functions are mostly of one crate.
    last_crate: u32,
    /// Each module by its crate's index (4 bytes, little-endian) and then `ff` and a segment
    /// for each of its segments: a function's module is one lookup of its path, and each
    /// module above it is a prefix of that.
    modules: HashMap<Box<[u8]>, Entity>,
    /// Each function, in the snapshot's order.
    nodes: Vec<Node>,
    by_key: HashMap<u64, u32>,
    /// The nodes of each object of the build under way, by their ids in it, and the maps of
    /// the builds before, emptied, for the next build's objects.
    ids: HashMap<u32, HashMap<u32, u32>>,
    spare_ids: Vec<HashMap<u32, u32>>,
    /// The nodes each node calls.
    out: Calls,
    edges: usize,
    /// The modules that call one another, and the crates.
    calls: blaze_hash::HashSet<(Entity, Entity)>,
    /// The modules each module calls, as its `Callees`.
    module_calls: HashMap<Entity, Vec<Entity>>,
    /// Calls of the build's objects to ids none of its records has named yet: the first of
    /// each `(object, id)` ([`key`]), chained through `waiting` in the order they came. One to
    /// code of another is never named.
    unnamed: HashMap<u64, u32>,
    waiting: Vec<Waiting>,
    /// The records of the last build, but for its functions, each as the page is sent it: a
    /// CBOR sequence, encoded once.
    told: Vec<u8>,
    told_count: usize,
    /// Where the record told last begins in `told`, and how many have been told in all.
    told_last: usize,
    appended: u64,
    /// What encodes each record told: its buffers are reused.
    writer: blaze_protocol::cbor::Writer,
}

/// The nodes each node calls, in the order its calls were made: each node's a chain through one
/// list of edges, so a node allocates nothing of its own, and the edges of a node said again
/// are reused.
pub(crate) struct Calls {
    /// Each node's first and last edge, or [`END`].
    ends: Vec<(u32, u32)>,
    /// Each edge: the node it calls, and the next of its node's or [`END`].
    edges: Vec<(u32, u32)>,
    /// The first edge free, the rest chained through their next, or [`END`].
    free: u32,
}

impl Calls {
    pub(crate) fn new() -> Calls {
        // alloc: none until they fill
        let (ends, edges) = (Vec::new(), Vec::new());
        Calls { ends, edges, free: END }
    }

    pub(crate) fn nodes(&self) -> usize {
        self.ends.len()
    }

    /// Another node, calling none.
    pub(crate) fn push_node(&mut self) {
        self.ends.push((END, END));
    }

    pub(crate) fn clear(&mut self) {
        self.ends.clear();
        self.edges.clear();
        self.free = END;
    }

    /// Makes node `at` call none, and says how many it called.
    pub(crate) fn clear_node(&mut self, at: u32) -> usize {
        let (first, last) = std::mem::replace(&mut self.ends[at as usize], (END, END));
        if first == END {
            return 0;
        }
        let (mut edge, mut count) = (first, 1);
        while edge != last {
            edge = self.edges[edge as usize].1;
            count += 1;
        }
        self.edges[last as usize].1 = self.free;
        self.free = first;
        count
    }

    pub(crate) fn contains(&self, at: u32, to: u32) -> bool {
        self.of(at).any(|it| it == to)
    }

    /// Makes node `at` call `to` after the others it calls.
    pub(crate) fn push(&mut self, at: u32, to: u32) {
        let edge = match self.free {
            END => {
                self.edges.push((to, END));
                self.edges.len() as u32 - 1
            }
            free => {
                self.free = self.edges[free as usize].1;
                self.edges[free as usize] = (to, END);
                free
            }
        };
        let ends = &mut self.ends[at as usize];
        match ends.1 {
            END => ends.0 = edge,
            last => self.edges[last as usize].1 = edge,
        }
        ends.1 = edge;
    }

    /// The nodes `at` calls, in the order it was made to.
    pub(crate) fn of(&self, at: u32) -> impl Iterator<Item = u32> + '_ {
        let mut edge = self.ends[at as usize].0;
        std::iter::from_fn(move || {
            let &(to, next) = self.edges.get(edge as usize)?;
            edge = next;
            Some(to)
        })
    }
}

struct Node {
    entity: Entity,
    krate: u32,
    /// The module it is in, or its crate.
    module: Entity,
}

impl World {
    pub fn new() -> Result<World, String> {
        Ok(World { flecs: Flecs::new()?, scratch: Scratch::default() })
    }

    /// A program of the workspace at `root`, with no functions yet.
    pub fn program(&mut self, root: &Path) -> Program {
        let entity = self.workspace_of(root, 0);
        // alloc: once per workspace
        let root = root.to_owned();
        // alloc: none until they fill
        let (crates, crates_of, nodes) = (Vec::new(), Vec::new(), Vec::new());
        // alloc: none until they fill
        let (waiting, told, spare_ids) = (Vec::new(), Vec::new(), Vec::new());
        Program {
            root,
            entity,
            build: 0,
            changes: 0,
            crates,
            crates_of,
            crate_of: HashMap::default(),
            last_crate: 0,
            modules: HashMap::default(),
            nodes,
            by_key: HashMap::default(),
            ids: HashMap::default(),
            spare_ids,
            out: Calls::new(),
            edges: 0,
            calls: Default::default(),
            module_calls: HashMap::default(),
            unnamed: HashMap::default(),
            waiting,
            told,
            told_count: 0,
            told_last: 0,
            appended: 0,
            writer: blaze_protocol::cbor::Writer::new(),
        }
    }

    /// Takes the program out of the world.
    pub fn forget(&mut self, program: Program) {
        // Its children go with the workspace.
        self.flecs.delete(program.entity);
    }

    /// Takes in `record`, of build `build` of the program's workspace.
    pub fn apply(&mut self, program: &mut Program, build: u64, record: &Record) -> Applied {
        match &record.event {
            Event::Began { .. } => {
                program.build = build;
                program.changes += 1;
                program.spare_ids.extend(program.ids.drain().map(|(_, mut ids)| {
                    ids.clear();
                    ids
                }));
                program.unnamed.clear();
                program.waiting.clear();
                program.told.clear();
                program.told_count = 0;
                program.tell(record);
                self.set_workspace(program);
                Applied::Told
            }
            Event::Functions { object, whole, functions } => {
                program.changes += 1;
                let anew = *whole && !program.ids.contains_key(object);
                if anew {
                    self.forget_functions(program);
                }
                self.placed(program, *object, functions);
                if anew { Applied::Anew } else { Applied::Told }
            }
            Event::Finished { .. } => {
                program.unnamed.clear();
                program.waiting.clear();
                program.tell(record);
                Applied::Told
            }
            _ => {
                program.tell(record);
                Applied::Told
            }
        }
    }

    /// What Flecs's REST answers `method` of `path` (with its query), read only.
    pub fn rest(&mut self, method: &str, path: &str) -> Result<Answer, String> {
        self.flecs.rest(method, path)
    }

    /// The program in the layout of docs/web.md, "The snapshot".
    pub fn snapshot(&self, program: &Program) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + program.nodes.len() * 64 + program.edges * 8);
        out.extend(b"BLZW");
        out.extend(1u32.to_le_bytes());
        text(&mut out, program.root.to_string_lossy().as_bytes());
        out.extend(program.build.to_le_bytes());
        out.extend((program.crates.len() as u32).to_le_bytes());
        for krate in &program.crates {
            text(&mut out, krate.as_bytes());
        }
        out.extend((program.nodes.len() as u32).to_le_bytes());
        for node in &program.nodes {
            // `placed` sets every node's Function as it makes the node.
            let Some(function) = self.flecs.function(node.entity) else {
                panic!("build page: function {:#x} has no Function in the world", node.entity)
            };
            out.extend(function.key.to_le_bytes());
            out.extend(node.krate.to_le_bytes());
            out.push(function.how);
            out.extend(function.ns.to_le_bytes());
            out.extend(function.build.to_le_bytes());
            text(&mut out, function.name);
        }
        out.extend((program.edges as u32).to_le_bytes());
        // alloc: once per snapshot, which is made once per change a page asks for
        let mut sorted = Vec::new();
        for from in 0..program.out.nodes() {
            sorted.clear();
            sorted.extend(program.out.of(from as u32));
            radix::sort(&mut sorted);
            for &to in &sorted {
                out.extend((from as u32).to_le_bytes());
                out.extend(to.to_le_bytes());
            }
        }
        out
    }

    fn workspace_of(&mut self, root: &Path, build: u64) -> Entity {
        let entity = self.flecs.entity();
        self.flecs.set_workspace(entity, root.to_string_lossy().as_bytes(), build);
        entity
    }

    fn set_workspace(&mut self, program: &Program) {
        let root = program.root.to_string_lossy();
        self.flecs.set_workspace(program.entity, root.as_bytes(), program.build);
    }

    /// A build writes the whole program again: what it says is all of it.
    fn forget_functions(&mut self, program: &mut Program) {
        // Its children go with the workspace.
        self.flecs.delete(program.entity);
        program.entity = self.workspace_of(&program.root, program.build);
        program.crates.clear();
        program.crates_of.clear();
        program.crate_of.clear();
        program.last_crate = 0;
        program.modules.clear();
        program.nodes.clear();
        program.by_key.clear();
        // Another object's ids name nodes that are gone, as on the page, which starts over; that
        // it was said stays, so a whole record of it later is merged, not made anew again.
        for ids in program.ids.values_mut() {
            ids.clear();
        }
        program.out.clear();
        program.edges = 0;
        program.calls.clear();
        program.module_calls.clear();
        program.unnamed.clear();
        program.waiting.clear();
    }

    fn placed(
        &mut self,
        program: &mut Program,
        object: u32,
        functions: &[blaze_protocol::events::Function],
    ) {
        let mut placed = std::mem::take(&mut self.scratch.placed);
        placed.clear();
        placed.reserve(functions.len());
        for function in functions {
            let (krate, in_crate) = self.crate_of(program, &function.krate);
            let at = match program.by_key.get(&function.key) {
                Some(&at) => {
                    // What it calls is said again below.
                    program.nodes[at as usize].krate = krate;
                    program.edges -= program.out.clear_node(at);
                    at
                }
                None => {
                    let parent =
                        self.module_of(program, krate, in_crate, &function.krate, &function.name);
                    let entity = self.flecs.entity();
                    self.flecs.set_parent(entity, parent);
                    let at = program.nodes.len() as u32;
                    program.nodes.push(Node { entity, krate, module: parent });
                    program.out.push_node();
                    program.by_key.insert(function.key, at);
                    at
                }
            };
            let (how, ns) = match function.how {
                How::Compiled { ns } => (0u8, ns),
                How::Unit => (1, 0),
                How::Unchanged => (2, 0),
                How::Stored => (3, 0),
            };
            let entity = program.nodes[at as usize].entity;
            let (key, name) = (function.key, function.name.as_bytes());
            self.flecs.set_function(entity, key, name, how, ns, program.build);
            let spare = &mut program.spare_ids;
            let ids = program.ids.entry(object).or_insert_with(|| spare.pop().unwrap_or_default());
            ids.insert(function.id, at);
            placed.push(at);
        }
        let mut calls = std::mem::take(&mut self.scratch.calls);
        let mut unnamed = std::mem::take(&mut self.scratch.unnamed);
        let mut named = std::mem::take(&mut self.scratch.named);
        calls.clear();
        unnamed.clear();
        named.clear();
        let Some(ids) = program.ids.get(&object) else {
            self.scratch.placed = placed;
            self.scratch.calls = calls;
            self.scratch.unnamed = unnamed;
            self.scratch.named = named;
            return;
        };
        for (function, &from) in functions.iter().zip(&placed) {
            for &call in &function.calls {
                match ids.get(&call) {
                    Some(&to) => calls.push((from, to)),
                    None => unnamed.push((call, from)),
                }
            }
        }
        // The calls that waited for an id this record named, in the order they came: none of
        // this record's own waits, since every id it names is in `ids` already.
        for function in functions {
            let Some(mut at) = program.unnamed.remove(&key(object, function.id)) else {
                continue;
            };
            let to = ids[&function.id];
            while at != END {
                let waiting = program.waiting[at as usize];
                named.push((at, waiting.from, to));
                at = waiting.next;
            }
        }
        radix::sort_copy_by_key(&mut named, |it| it.0);
        calls.extend(named.iter().map(|&(_, from, to)| (from, to)));
        for &(call, from) in &unnamed {
            let at = program.waiting.len() as u32;
            let first = program.unnamed.entry(key(object, call)).or_insert(END);
            program.waiting.push(Waiting { from, next: *first });
            *first = at;
        }
        let mut changed = std::mem::take(&mut self.scratch.changed);
        let mut changed_modules = std::mem::take(&mut self.scratch.changed_modules);
        changed.clear();
        changed.extend_from_slice(&placed);
        changed_modules.clear();
        for &(from, to) in &calls {
            if program.out.contains(from, to) {
                continue;
            }
            program.out.push(from, to);
            program.edges += 1;
            changed.push(from);
            let (from, to) = (&program.nodes[from as usize], &program.nodes[to as usize]);
            let crates =
                (program.crates_of[from.krate as usize], program.crates_of[to.krate as usize]);
            // A function at its crate's root is in no module: its calls are its crate's alone.
            let in_modules = from.module != crates.0 && to.module != crates.1;
            if in_modules
                && from.module != to.module
                && program.calls.insert((from.module, to.module))
            {
                program.module_calls.entry(from.module).or_default().push(to.module);
                changed_modules.push(from.module);
            }
            if crates.0 != crates.1 && program.calls.insert(crates) {
                self.flecs.add_calls(crates.0, crates.1);
            }
        }
        radix::sort(&mut changed);
        changed.dedup();
        let mut callees = std::mem::take(&mut self.scratch.callees);
        for &at in &changed {
            callees.clear();
            callees.extend(program.out.of(at).map(|to| program.nodes[to as usize].entity));
            self.flecs.set_callees(program.nodes[at as usize].entity, &callees);
        }
        radix::sort(&mut changed_modules);
        changed_modules.dedup();
        for &module in &changed_modules {
            self.flecs.set_callees(module, &program.module_calls[&module]);
        }
        self.scratch.placed = placed;
        self.scratch.calls = calls;
        self.scratch.unnamed = unnamed;
        self.scratch.named = named;
        self.scratch.changed = changed;
        self.scratch.changed_modules = changed_modules;
        self.scratch.callees = callees;
    }

    fn crate_of(&mut self, program: &mut Program, name: &str) -> (u32, Entity) {
        let last = program.last_crate as usize;
        if program.crates.get(last).is_some_and(|it| it == name) {
            return (last as u32, program.crates_of[last]);
        }
        if let Some(&it) = program.crate_of.get(name) {
            program.last_crate = it.0;
            return it;
        }
        let entity = self.flecs.named(program.entity, name.as_bytes(), Tag::Crate);
        let it = (program.crates.len() as u32, entity);
        program.crates_of.push(entity);
        // alloc: once per crate of the program, its name by index and by name
        program.crates.push(name.to_owned());
        program.crate_of.insert(name.to_owned(), it); // alloc: as above
        program.last_crate = it.0;
        it
    }

    /// The module a function is in ([`each_module`]), under its crate, from the crate's name on.
    fn module_of(
        &mut self,
        program: &mut Program,
        krate: u32,
        in_crate: Entity,
        krate_name: &str,
        name: &str,
    ) -> Entity {
        let mut segments = std::mem::take(&mut self.scratch.segments);
        let mut path = std::mem::take(&mut self.scratch.path);
        let mut ends = std::mem::take(&mut self.scratch.ends);
        segments.clear();
        path.clear();
        ends.clear();
        let mut first = true;
        each_module(name, &mut |segment: &str| {
            if std::mem::take(&mut first) && segment == krate_name {
                return;
            }
            // Every segment is a slice of the name.
            let start = segment.as_ptr() as usize - name.as_ptr() as usize;
            segments.push((start, start + segment.len()));
        });
        path.extend(krate.to_le_bytes());
        for &(start, end) in &segments {
            path.push(0xff);
            path.extend_from_slice(&name.as_bytes()[start..end]);
            ends.push(path.len());
        }
        let found = match program.modules.get(&path[..]) {
            Some(&it) => Some(it),
            None if segments.is_empty() => Some(in_crate),
            None => None,
        };
        let at = found.unwrap_or_else(|| {
            let mut at = in_crate;
            for (&(start, end), &until) in segments.iter().zip(&ends) {
                at = match program.modules.get(&path[..until]) {
                    Some(&it) => it,
                    None => {
                        let segment = &name.as_bytes()[start..end];
                        let it = self.flecs.named(at, segment, Tag::Module);
                        program.modules.insert(path[..until].into(), it);
                        it
                    }
                };
            }
            at
        });
        self.scratch.segments = segments;
        self.scratch.path = path;
        self.scratch.ends = ends;
        at
    }

}

impl Program {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn build(&self) -> u64 {
        self.build
    }

    /// One more each time what [`World::snapshot`] says of it may have changed.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// The records of the last build, but for its functions, as the page is sent them: a
    /// CBOR sequence of [`Record`]s.
    pub fn told(&self) -> &[u8] {
        &self.told
    }

    /// How many records [`Program::told`] holds.
    pub fn told_count(&self) -> usize {
        self.told_count
    }

    /// How many records have been told in all, and the last of them, as the page is sent it.
    pub fn last_told(&self) -> (u64, &[u8]) {
        (self.appended, &self.told[self.told_last..])
    }

    pub fn functions(&self) -> usize {
        self.nodes.len()
    }

    pub fn calls(&self) -> usize {
        self.edges
    }

    fn tell(&mut self, record: &Record) {
        let at = self.told.len();
        match self.writer.append(&mut self.told, record) {
            Ok(()) => {
                self.told_last = at;
                self.told_count += 1;
                self.appended += 1;
            }
            Err(error) => {
                eprintln!("build page: a record of {} does not encode: {error}", self.root.display())
            }
        }
    }
}

/// The chain of calls waiting for id `id` of object `object`.
fn key(object: u32, id: u32) -> u64 {
    (u64::from(object) << 32) | u64::from(id)
}

/// The segments of [`each_module`], collected.
#[cfg(test)]
pub fn module_path(name: &str) -> Vec<&str> {
    let mut out = Vec::new();
    each_module(name, &mut |it| out.push(it));
    out
}

/// Each segment of the path a function is in, in order, each a slice of `name`, with nothing
/// allocated. The path is read from how a backtrace names the function: its path but its last
/// segment, without generic arguments or closures, and of a method of `<T as Trait>` or `<T>`,
/// `T`'s path. As the page's `moduleOf` (assets/names.js), so the world's modules are the
/// page's:
///
/// ```text
/// anim::scene::render                                  anim::scene
/// anim::main::{closure#0}                              anim
/// core::ptr::drop_in_place::<anim::Thing>              core::ptr
/// <alloc::vec::Vec<T> as core::ops::drop::Drop>::drop  alloc::vec::Vec
/// <&[u8] as core::fmt::Debug>::fmt                     (none: a slice has no path)
/// ```
fn each_module<'a>(name: &'a str, f: &mut impl FnMut(&'a str)) {
    if name.starts_with('<') {
        if let Some(end) = closing(name, 0) {
            self_path(&name[1..end], f);
        }
        return;
    }
    // Every segment but the last.
    let mut held: Option<&'a str> = None;
    segments(name, &mut |it| {
        if let Some(before) = held.replace(it) {
            f(before);
        }
    });
}

/// Each piece of `name` between the `::`s outside `<>`, `()`, `[]` and `{}`. Every byte it
/// looks for is ASCII, which no byte of a longer UTF-8 character is.
fn split_path<'a>(name: &'a str, f: &mut impl FnMut(&'a str)) {
    let bytes = name.as_bytes();
    let (mut depth, mut start, mut last, mut at) = (0usize, 0, 0u8, 0);
    while at < bytes.len() {
        let c = bytes[at];
        match c {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if last != b'-' => depth = depth.saturating_sub(1),
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => {
                f(&name[start..at]);
                at += 1;
                start = at + 1;
            }
            _ => {}
        }
        last = c;
        at += 1;
    }
    f(&name[start..]);
}

/// The byte of the `>` that closes the `<` at `open`.
fn closing(name: &str, open: usize) -> Option<usize> {
    let (mut depth, mut last) = (0usize, 0u8);
    for (at, &c) in name.as_bytes()[open..].iter().enumerate() {
        if c == b'<' {
            depth += 1;
        } else if c == b'>' && last != b'-' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(open + at);
            }
        }
        last = c;
    }
    None
}

/// The path of `T` in `T as Trait`, or of `T`.
fn self_path<'a>(inner: &'a str, f: &mut impl FnMut(&'a str)) {
    let (mut depth, mut last) = (0usize, 0u8);
    for (at, &c) in inner.as_bytes().iter().enumerate() {
        match c {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if last != b'-' => depth = depth.saturating_sub(1),
            b')' | b']' => depth = depth.saturating_sub(1),
            b' ' if depth == 0 && inner[at..].starts_with(" as ") => {
                return type_path(&inner[..at], f);
            }
            _ => {}
        }
        last = c;
    }
    type_path(inner, f)
}

/// A type's path without its generic arguments, or none for a type that has none: a reference
/// or pointer is its pointee's, a slice, tuple or `fn` pointer has none.
fn type_path<'a>(ty: &'a str, f: &mut impl FnMut(&'a str)) {
    let mut ty = ty.trim();
    loop {
        let before = ty;
        for prefix in ["&mut ", "&", "*const ", "*mut ", "dyn "] {
            ty = ty.strip_prefix(prefix).unwrap_or(ty).trim_start();
        }
        if let Some(rest) = ty.strip_prefix('\'') {
            ty = rest.split_once(' ').map_or(ty, |(_, rest)| rest.trim_start());
        }
        if ty == before {
            break;
        }
    }
    if ty.starts_with('<') {
        if let Some(end) = closing(ty, 0) {
            self_path(&ty[1..end], f);
        }
        return;
    }
    let fn_pointer =
        ty.starts_with("fn(") || ty.starts_with("unsafe ") || ty.starts_with("extern ");
    if fn_pointer || !ty.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return;
    }
    segments(ty, f)
}

/// The named segments of a path: no `::<…>`, generic arguments or `{closure#0}`.
fn segments<'a>(path: &'a str, f: &mut impl FnMut(&'a str)) {
    split_path(path, &mut |it: &'a str| {
        if !it.is_empty() && !it.starts_with('<') && !it.starts_with('{') {
            f(it.find('<').map_or(it, |lt| &it[..lt]));
        }
    });
}

fn text(out: &mut Vec<u8>, text: &[u8]) {
    out.extend((text.len() as u32).to_le_bytes());
    out.extend(text);
}
