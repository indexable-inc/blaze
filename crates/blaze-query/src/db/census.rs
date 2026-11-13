//! The durability census: how many rows are of each kind, and which fields hold a row below
//! `HIGH`. A diagnostic for whoever asks why a library's rows verify again after a workspace
//! write (docs/v2/plan/design/restore.md, "Found: a library's rows are not at `HIGH`"): it
//! names the inputs to move, by rows, where a count by kind alone only says there are some.
//!
//! A walk of every node and of the edges of every row below `HIGH`: tens of milliseconds a
//! million rows, so it runs only when asked, never on a build's path.

use std::sync::atomic::Ordering;

use super::{
    Durability,
    function,
    ingredient::{self, Kind},
    row::{UNTRACKED, VIEW_MASK, span},
    seal::SealCensus,
    storage::{FIELD, Storage, key_parts},
};

/// The census's counts by key: two counts a key.
type Table<K> = hashbrown::HashMap<K, (u64, u64), blaze_hash::FixedState>;

/// A field of kind below `HIGH` and the rows it holds below `HIGH` by a read of their own.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Blocker {
    /// The field's struct: an input or a tracked struct.
    pub ingredient: &'static str,
    /// The field's name.
    pub field: &'static str,
    /// The field's kind where it was last seen (`LOW` 0, `MEDIUM` 1).
    pub durability: u8,
    /// How many fields of this name (one a row of the struct) some row below `HIGH` reads.
    pub fields: u64,
    /// The rows that read one themselves, each counted once a blocker.
    pub rows: u64,
}

/// A function and how many of its rows are below `HIGH`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Held {
    pub function: &'static str,
    /// Its rows below `HIGH`.
    pub rows: u64,
    /// Those that read no field below `HIGH` themselves: their kind is a row's they read.
    pub inherited: u64,
}

/// What [`Storage::durability_census`] found.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct Census {
    /// Function rows that ran, by kind (`LOW`, `MEDIUM`, `HIGH`).
    pub rows: [u64; Durability::KINDS],
    /// Of them, rows that made an untracked read (`LOW` whatever they read).
    pub untracked: u64,
    /// Edges the rows of each kind hold.
    pub edges: [u64; Durability::KINDS],
    /// Fields below `HIGH` that rows read themselves, most rows first.
    pub blockers: Vec<Blocker>,
    /// Functions with rows below `HIGH`, most rows first.
    pub held: Vec<Held>,
    /// The rows by seal group and state ([`Storage::seal_census`]).
    pub seals: SealCensus,
}

impl Storage {
    /// Counts the rows by kind and names what holds the rows below `HIGH` there
    /// ([`Census`]). Reads headers and edges with relaxed loads: exact with no run in flight,
    /// a count that may be off by the rows running otherwise.
    pub fn durability_census(&self) -> Census {
        let mut census = Census::default();
        // alloc: the census's own tables, an entry a field name and a function
        let mut blockers: Table<(u32, u32, u8)> = Table::default();
        let mut held: Table<u32> = Table::default();
        let mut fields: hashbrown::HashSet<u32, blaze_hash::FixedState> = Default::default();
        let mut own: Vec<(u32, u32, u8)> = Vec::new(); // alloc: one row's blockers
        for node in 1..self.nodes_made() {
            let key = self.key_of(node);
            if key == 0 {
                continue;
            }
            let (index, id, _) = key_parts(key);
            if ingredient::vtable(index).is_none_or(|it| it.kind != Kind::Function) {
                continue;
            }
            let Some(header) = function::header_of(self, index, id) else { continue };
            if header.verified.load(Ordering::Relaxed) & VIEW_MASK == 0 {
                continue;
            }
            let kind = header.durability().min(Durability::HIGH.0) as usize;
            let edges = header.edges.load(Ordering::Relaxed);
            let (start, len) = (span::start(edges), span::len(edges));
            census.rows[kind] += 1;
            census.edges[kind] += len;
            let untracked = header.meta.load(Ordering::Relaxed) & UNTRACKED != 0;
            census.untracked += u64::from(untracked);
            if kind >= Durability::HIGH.0 as usize {
                continue;
            }
            own.clear();
            for at in start..start + len {
                let read = self.edge(at);
                let color = (self.color_of(read) >> 32) as u32;
                if color < FIELD {
                    continue;
                }
                let durability = (color & 0b11) as u8;
                if durability >= Durability::HIGH.0 {
                    continue;
                }
                let (struct_, _, field) = key_parts(self.key_of(read));
                if fields.insert(read) {
                    blockers.entry((struct_, field, durability)).or_default().0 += 1;
                }
                if !own.contains(&(struct_, field, durability)) {
                    own.push((struct_, field, durability));
                }
            }
            for &blocker in &own {
                blockers.entry(blocker).or_default().1 += 1;
            }
            let entry = held.entry(index).or_default();
            entry.0 += 1;
            entry.1 += u64::from(own.is_empty() && !untracked);
        }
        census.blockers = blockers
            .into_iter()
            .map(|((struct_, field, durability), (fields, rows))| Blocker {
                ingredient: self.ingredient_name(struct_),
                field: self.field_name(struct_, field),
                durability,
                fields,
                rows,
            })
            .collect();
        // Most rows first, then by name: the same list whatever order the table gave.
        census.blockers.sort_by(|a, b| {
            (b.rows, a.ingredient, a.field, a.durability)
                .cmp(&(a.rows, b.ingredient, b.field, b.durability))
        });
        census.held = held
            .into_iter()
            .map(|(index, (rows, inherited))| Held {
                function: self.ingredient_name(index),
                rows,
                inherited,
            })
            .collect();
        census.held.sort_by(|a, b| (b.rows, a.function).cmp(&(a.rows, b.function)));
        census.seals = self.seal_census();
        census
    }
}

impl std::fmt::Display for Census {
    /// One line of totals, then the twenty largest blockers and functions, a line each.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [low, medium, high] = self.rows;
        let [low_edges, medium_edges, high_edges] = self.edges;
        writeln!(
            f,
            "rows: {high} HIGH ({high_edges} edges), {medium} MEDIUM ({medium_edges} edges), \
             {low} LOW ({low_edges} edges), {} of them untracked",
            self.untracked
        )?;
        for it in self.blockers.iter().take(20) {
            let kind = if it.durability == 0 { "LOW" } else { "MEDIUM" };
            writeln!(
                f,
                "  field {}.{} ({kind}): read by {} rows below HIGH themselves, over {} fields",
                it.ingredient, it.field, it.rows, it.fields
            )?;
        }
        for it in self.held.iter().take(20) {
            writeln!(
                f,
                "  function {}: {} rows below HIGH, {} by a row they read alone",
                it.function, it.rows, it.inherited
            )?;
        }
        write!(f, "{}", self.seals)
    }
}
