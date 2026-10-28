//! The graph of a workspace's functions, as its builds' records say them, and the snapshot of
//! it a page is sent first (docs/web.md, "The snapshot").

use std::path::{Path, PathBuf};

use blaze_protocol::events::{Event, Function, How, Record};
use rustc_hash::{FxHashMap, FxHashSet};

/// The functions of the program a workspace builds, and the records of its last build.
pub struct Graph {
    root: PathBuf,
    build: u64,
    crates: Vec<String>,
    crate_of: FxHashMap<String, u32>,
    nodes: Vec<Node>,
    by_key: FxHashMap<u64, u32>,
    /// The nodes of each object of the build under way, by their ids in it.
    ids: FxHashMap<u32, FxHashMap<u32, u32>>,
    edges: FxHashSet<(u32, u32)>,
    /// Calls of the build's objects to ids none of its records has named yet: the object,
    /// the node that calls and the id. One to code of another is never named.
    unresolved: Vec<(u32, u32, u32)>,
    /// The records of the last build, but for its functions.
    records: Vec<Record>,
}

struct Node {
    key: u64,
    krate: u32,
    how: How,
    build: u64,
    name: String,
}

/// What a record did to a graph.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// It is in the graph as the page is told it: the page is sent it.
    Told,
    /// It began the program's graph again: the page is sent the graph anew.
    Anew,
}

impl Graph {
    pub fn new(root: &Path) -> Graph {
        Graph {
            root: root.to_owned(),
            build: 0,
            crates: Vec::new(),
            crate_of: FxHashMap::default(),
            nodes: Vec::new(),
            by_key: FxHashMap::default(),
            ids: FxHashMap::default(),
            edges: FxHashSet::default(),
            unresolved: Vec::new(),
            records: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn build(&self) -> u64 {
        self.build
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn functions(&self) -> usize {
        self.nodes.len()
    }

    pub fn calls(&self) -> usize {
        self.edges.len()
    }

    /// Takes in `record`, of build `build` of the workspace.
    pub fn apply(&mut self, build: u64, record: &Record) -> Applied {
        match &record.event {
            Event::Began { .. } => {
                self.build = build;
                self.ids.clear();
                self.unresolved.clear();
                self.records.clear();
                self.records.push(record.clone());
                Applied::Told
            }
            Event::Fns { object, whole, fns } => {
                let anew = *whole && !self.ids.contains_key(object);
                if anew {
                    self.forget_functions();
                }
                self.placed(*object, fns);
                if anew { Applied::Anew } else { Applied::Told }
            }
            Event::Finished { .. } => {
                self.unresolved.clear();
                self.records.push(record.clone());
                Applied::Told
            }
            _ => {
                self.records.push(record.clone());
                Applied::Told
            }
        }
    }

    /// A build writes the whole program again: what it says is all of it.
    fn forget_functions(&mut self) {
        self.crates.clear();
        self.crate_of.clear();
        self.nodes.clear();
        self.by_key.clear();
        self.edges.clear();
        self.unresolved.clear();
    }

    fn placed(&mut self, object: u32, fns: &[Function]) {
        let mut placed = Vec::with_capacity(fns.len());
        for function in fns {
            let krate = match self.crate_of.get(&function.krate) {
                Some(&krate) => krate,
                None => {
                    let krate = self.crates.len() as u32;
                    self.crates.push(function.krate.clone());
                    self.crate_of.insert(function.krate.clone(), krate);
                    krate
                }
            };
            let node = Node {
                key: function.key,
                krate,
                how: function.how,
                build: self.build,
                name: function.name.clone(),
            };
            let at = match self.by_key.get(&function.key) {
                Some(&at) => {
                    self.nodes[at as usize] = node;
                    at
                }
                None => {
                    let at = self.nodes.len() as u32;
                    self.nodes.push(node);
                    self.by_key.insert(function.key, at);
                    at
                }
            };
            self.ids.entry(object).or_default().insert(function.id, at);
            placed.push(at);
        }
        let ids = &self.ids[&object];
        for (function, &from) in fns.iter().zip(&placed) {
            for &call in &function.calls {
                match ids.get(&call) {
                    Some(&to) => {
                        self.edges.insert((from, to));
                    }
                    None => self.unresolved.push((object, from, call)),
                }
            }
        }
        let edges = &mut self.edges;
        self.unresolved.retain(|&(of, from, call)| {
            let to = (of == object).then(|| ids.get(&call)).flatten();
            match to {
                Some(&to) => {
                    edges.insert((from, to));
                    false
                }
                None => true,
            }
        });
    }

    /// The graph in the layout of docs/web.md, "The snapshot".
    pub fn snapshot(&self) -> Vec<u8> {
        let names: usize = self.nodes.iter().map(|it| it.name.len()).sum();
        let mut out = Vec::with_capacity(64 + names + self.nodes.len() * 33 + self.edges.len() * 8);
        out.extend(b"BLZW");
        out.extend(1u32.to_le_bytes());
        text(&mut out, &self.root.to_string_lossy());
        out.extend(self.build.to_le_bytes());
        out.extend((self.crates.len() as u32).to_le_bytes());
        for krate in &self.crates {
            text(&mut out, krate);
        }
        out.extend((self.nodes.len() as u32).to_le_bytes());
        for node in &self.nodes {
            let (how, ns) = match node.how {
                How::Compiled { ns } => (0u8, ns),
                How::Unit => (1, 0),
                How::Unchanged => (2, 0),
                How::Stored => (3, 0),
            };
            out.extend(node.key.to_le_bytes());
            out.extend(node.krate.to_le_bytes());
            out.push(how);
            out.extend(ns.to_le_bytes());
            out.extend(node.build.to_le_bytes());
            text(&mut out, &node.name);
        }
        let mut edges: Vec<_> = self.edges.iter().copied().collect();
        edges.sort_unstable();
        out.extend((edges.len() as u32).to_le_bytes());
        for (from, to) in edges {
            out.extend(from.to_le_bytes());
            out.extend(to.to_le_bytes());
        }
        out
    }
}

fn text(out: &mut Vec<u8>, text: &str) {
    out.extend((text.len() as u32).to_le_bytes());
    out.extend(text.as_bytes());
}
