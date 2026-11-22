//! The arena: reduced, ordered decision diagrams with complement edges, hash-consed in columns.
//!
//! A [`Cond`] is `node << 1 | complemented`. Node 0 is the constant true, so [`Cond::TRUE`] is 0
//! and [`Cond::FALSE`] is 1, and `not` flips one bit. A node is `(var, low, high)`: the function
//! `if var { high } else { low }`. Canonical form: no node has equal edges (reduced), a child's
//! variable comes after its parent's in the space's order (ordered), no two nodes are equal
//! (shared, by the unique table), and no high edge is complemented (so a function and its
//! negation are one node and two ids). Under a fixed order, equal functions are equal ids.
//!
//! Every operation runs on explicit stacks the arena keeps (no recursion), with a direct-mapped
//! cache of results keyed by the operation and its operands, overwritten on collision.

#![blaze::sans_io]

use blaze_int::{Int, idx};
use blaze_own::{
    AVec, Alloc,
    planted::{NoPlant, Planted},
};

use crate::error::CondError;
use crate::plant::CondPlant;

/// The most nodes one arena may hold: a [`Cond`]'s 31 bits of node, less room for the cache's
/// operation tags above them.
pub const MOST_NODES: u32 = 1 << 30;

/// The most variables a space may have: a node's variable is 16 bits, and the constant's is the
/// one after all of them.
pub const MOST_VARS: u16 = u16::MAX - 1;

/// The constant's variable: after every real one in the order.
const TERMINAL: u16 = u16::MAX;

/// A cfg variable: its place in the space's order (by name). Only within one request; what is
/// stored names the variable by its [`VarKey`](crate::VarKey).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Var(u16);

impl Var {
    /// The variable at `index` in the space's order.
    pub const fn new(index: u16) -> Var {
        Var(index)
    }

    pub const fn index(self) -> u16 {
        self.0
    }
}

/// A condition: an edge into an arena, `node << 1 | complemented`. Ids of one arena only.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct Cond(u32);

impl Cond {
    /// Holds in every configuration: the condition of an entry that is always there.
    pub const TRUE: Cond = Cond(0);
    /// Holds in none.
    pub const FALSE: Cond = Cond(1);

    /// The negation: O(1), the same node.
    #[inline]
    #[must_use]
    pub const fn not(self) -> Cond {
        Cond(self.0 ^ 1)
    }

    pub const fn is_true(self) -> bool {
        self.0 == 0
    }

    pub const fn is_false(self) -> bool {
        self.0 == 1
    }

    pub const fn is_const(self) -> bool {
        self.0 < 2
    }

    /// The id as a number, for a record that holds conditions of its own arena.
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    const fn node(self) -> u32 {
        self.0 >> 1
    }

    #[inline]
    const fn is_neg(self) -> bool {
        self.0 & 1 == 1
    }

    #[inline]
    const fn neg_if(self, neg: bool) -> Cond {
        Cond(self.0 ^ neg as u32)
    }

    #[inline]
    const fn regular(self) -> Cond {
        Cond(self.0 & !1)
    }
}

impl core::ops::Not for Cond {
    type Output = Cond;

    fn not(self) -> Cond {
        Cond::not(self)
    }
}

/// An arena's bounds (rule 18; D165: a cap is a field here, drawn by the simulation down to its
/// smallest legal value, tests/mod.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most nodes, the constant included; past it an operation refuses
    /// ([`CondError::NodeCap`]). At least 1 and at most [`MOST_NODES`].
    pub nodes: u32,
    /// The `ite` cache has 2^`cache_bits` entries of 16 bytes, fixed for the arena's life; at
    /// most 26.
    pub cache_bits: u8,
}

impl Limits {
    /// What a task's arena ships with: a million nodes (18 MB at most), a 256 KB cache.
    pub const SHIPPED: Limits = Limits { nodes: 1 << 20, cache_bits: 14 };
    /// The least legal value of each bound: an arena of the constant alone, a one-entry cache.
    pub const SMALLEST: Limits = Limits { nodes: 1, cache_bits: 0 };
    /// The most `cache_bits` an arena takes.
    pub const MOST_CACHE_BITS: u8 = 26;
}

/// What an arena has done, for the cost example and the profile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Steps that missed the cache and were expanded (one unique-table probe each, at most).
    pub steps: u64,
    /// Steps the cache answered.
    pub hits: u64,
}

/// A witness: the variables on one satisfying path and their values; any variable not listed
/// may take either value.
pub type Witness<'a> = AVec<'a, (Var, bool)>;

const EMPTY: u32 = u32::MAX;
const OP_RESTRICT: u32 = u32::MAX - 1;
const OP_EXISTS: u32 = u32::MAX - 2;
const OP_DISJOINT: u32 = u32::MAX - 3;

/// One cache entry: an operation's operands and its result. `c` is `ite`'s `h`, or the tag of
/// another operation (above every id, as [`MOST_NODES`] keeps ids below 2^31).
#[derive(Clone, Copy)]
struct Entry {
    a: u32,
    b: u32,
    c: u32,
    r: u32,
}

/// A step of the machine.
#[derive(Clone, Copy)]
enum Frame {
    Ite(Cond, Cond, Cond),
    IteJoin { f: Cond, g: Cond, h: Cond, var: u16, neg: bool },
    Restrict { f: Cond, var: u16, value: bool },
    RestrictJoin { f: Cond, key: u32, var: u16, neg: bool },
    Exists { f: Cond, cube: Cond },
    /// Both cofactors are on the result stack: make the node (or, when `var` is quantified, push
    /// the `or` of them and then [`Frame::ExistsDone`]).
    ExistsJoin { f: Cond, cube: Cond, var: u16, quantify: bool },
    ExistsDone { f: Cond, cube: Cond },
}

/// What [`normalize`] made of an `ite`: an answer, or the canonical triple and whether the
/// answer is to be negated.
enum Norm {
    Done(Cond),
    Go { f: Cond, g: Cond, h: Cond, neg: bool },
}

/// `ite(f, g, h)` in the standard form (Brace, Rudell and Bryant, DAC 1990): the terminal cases
/// answered, operands equal to `f` or `!f` made constants, the commutative `and` and `or`
/// ordered by id, `f` and `g` regular. Equal calls meet in one cache entry.
#[inline]
fn normalize(mut f: Cond, mut g: Cond, mut h: Cond) -> Norm {
    if f.is_true() {
        return Norm::Done(g);
    }
    if f.is_false() {
        return Norm::Done(h);
    }
    if g == f {
        g = Cond::TRUE;
    } else if g == f.not() {
        g = Cond::FALSE;
    }
    if h == f {
        h = Cond::FALSE;
    } else if h == f.not() {
        h = Cond::TRUE;
    }
    if g == h {
        return Norm::Done(g);
    }
    if g.is_true() && h.is_false() {
        return Norm::Done(f);
    }
    if g.is_false() && h.is_true() {
        return Norm::Done(f.not());
    }
    // `f | h` and `f & g` are symmetric: the smaller id first.
    if g.is_true() && h < f {
        core::mem::swap(&mut f, &mut h);
    } else if h.is_false() && g < f {
        core::mem::swap(&mut f, &mut g);
    }
    if f.is_neg() {
        f = f.not();
        core::mem::swap(&mut g, &mut h);
    }
    let neg = g.is_neg();
    if neg {
        g = g.not();
        h = h.not();
    }
    Norm::Go { f, g, h, neg }
}

#[inline]
fn hash64(a: u32, b: u32, c: u32) -> u64 {
    let h = a.widen_to::<u64>().wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ b.widen_to::<u64>().wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ c.widen_to::<u64>().wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^ (h >> 31)
}

#[inline]
fn hash3(a: u32, b: u32, c: u32) -> usize {
    hash64(a, b, c).truncate_to::<usize>()
}

/// A unique-table slot: the node's hash in the high half, its number in the low; 0 is empty.
/// A probe compares hashes first and reads the node only when they agree, so a probe that
/// passes other nodes touches the table's line alone.
#[inline]
fn slot_of(hash: u64, node: u32) -> u64 {
    (hash & 0xFFFF_FFFF_0000_0000) | node.widen_to::<u64>()
}

/// One node: `if var { high } else { low }`. A row, not three columns: every read of a node
/// reads all three, so a row is one cache line where columns are three (with the hash-tagged
/// unique table, the cost example's random case went from 108 to 93 ns a step).
#[derive(Clone, Copy)]
struct Node {
    low: u32,
    high: u32,
    var: u16,
}

fn spush<T>(v: &mut AVec<'_, T>, x: T) -> Result<(), CondError> {
    v.try_push(x).map_err(|refused| CondError::Alloc(refused.why))
}

/// The arena of one task's conditions over one space's variables, as it ships: no plant.
pub type Bdd<'a> = PlantedBdd<'a, NoPlant>;

/// The arena of one task's conditions over one space's variables, with its plant seam `P`
/// (D165, rule 78): [`NoPlant`] where it ships ([`Bdd`]), a [`CondPlant`] under a control.
pub struct PlantedBdd<'a, P> {
    plant: P,
    vars: u16,
    cap: u32,
    node: AVec<'a, Node>,
    /// Open addressing over nodes ([`slot_of`]), 0 empty (node 0, the constant, is never in
    /// it); at most half full.
    unique: AVec<'a, u64>,
    cache: AVec<'a, Entry>,
    cache_mask: usize,
    frames: AVec<'a, Frame>,
    results: AVec<'a, Cond>,
    /// [`Bdd::disjoint`]'s pairs still to check, and the marks of pairs proven once their
    /// cofactors are.
    pairs: AVec<'a, (Cond, Cond, bool)>,
    /// Traversals: a node is visited in this traversal when its stamp is `epoch`; `slot` is
    /// its place in `order`, from 1.
    stamp: AVec<'a, u32>,
    pub(crate) slot: AVec<'a, u32>,
    epoch: u32,
    walk: AVec<'a, (u32, bool)>,
    pub(crate) order: AVec<'a, u32>,
    counts: AVec<'a, u128>,
    stats: Stats,
    alloc: Alloc<'a>,
}

impl<'a> PlantedBdd<'a, NoPlant> {
    /// An empty arena over `vars` variables, in `alloc`.
    pub fn new_in(vars: usize, limits: Limits, alloc: Alloc<'a>) -> Result<Bdd<'a>, CondError> {
        PlantedBdd::new_planted(vars, limits, alloc, NoPlant)
    }
}

impl<'a, P: Planted<CondPlant>> PlantedBdd<'a, P> {
    /// An empty arena over `vars` variables, in `alloc`, with the plant `plant` ([`NoPlant`]
    /// where it ships: [`Bdd::new_in`]).
    pub fn new_planted(vars: usize, limits: Limits, alloc: Alloc<'a>, plant: P) -> Result<PlantedBdd<'a, P>, CondError> {
        let Ok(vars) = u16::try_from(vars) else { return Err(CondError::TooManyVars { vars }) };
        if vars > MOST_VARS {
            return Err(CondError::TooManyVars { vars: vars.widen_to::<usize>() });
        }
        if limits.nodes > MOST_NODES || limits.nodes < Limits::SMALLEST.nodes || limits.cache_bits > Limits::MOST_CACHE_BITS {
            return Err(CondError::BadLimits { nodes: limits.nodes, cache_bits: limits.cache_bits });
        }
        let mut bdd = PlantedBdd {
            plant,
            vars,
            cap: limits.nodes,
            node: AVec::new_in(alloc),
            unique: AVec::new_in(alloc),
            cache: AVec::new_in(alloc),
            cache_mask: (1usize << limits.cache_bits) - 1,
            frames: AVec::new_in(alloc),
            results: AVec::new_in(alloc),
            pairs: AVec::new_in(alloc),
            stamp: AVec::new_in(alloc),
            slot: AVec::new_in(alloc),
            epoch: 0,
            walk: AVec::new_in(alloc),
            order: AVec::new_in(alloc),
            counts: AVec::new_in(alloc),
            stats: Stats::default(),
            alloc,
        };
        // Node 0: the constant true; its variable is after every other.
        spush(&mut bdd.node, Node { low: 0, high: 0, var: TERMINAL })?;
        bdd.unique.try_reserve_exact(1024)?;
        bdd.unique.resize(1024, 0);
        let entries = 1usize << limits.cache_bits;
        bdd.cache.try_reserve_exact(entries)?;
        bdd.cache.resize(entries, Entry { a: EMPTY, b: 0, c: 0, r: 0 });
        Ok(bdd)
    }

    /// The variables this arena's conditions range over.
    pub fn vars(&self) -> u16 {
        self.vars
    }

    /// The allocator the arena grows in.
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Nodes held, the constant included.
    pub fn nodes(&self) -> usize {
        self.node.len()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Bytes the arena holds now: its nodes, unique table, cache and traversal scratch.
    pub fn bytes(&self) -> usize {
        self.node.capacity() * size_of::<Node>()
            + self.unique.capacity() * 8
            + self.cache.capacity() * 16
            + (self.stamp.capacity() + self.slot.capacity() + self.order.capacity()) * 4
            + self.counts.capacity() * 16
    }

    /// The variable `var`'s condition: true where it is set.
    pub fn var(&mut self, var: Var) -> Result<Cond, CondError> {
        if var.0 >= self.vars {
            return Err(CondError::UnknownVar { var: var.0, vars: self.vars });
        }
        self.mk(var.0, Cond::FALSE, Cond::TRUE)
    }

    /// The variable a condition tests first, `None` for a constant.
    pub fn top(&self, f: Cond) -> Option<Var> {
        let v = self.node[idx(f.node())].var;
        (v != TERMINAL).then_some(Var(v))
    }

    #[inline]
    fn level(&self, f: Cond) -> u16 {
        self.node[idx(f.node())].var
    }

    /// The cofactors of `f` by `v`, `(low, high)`, when `f` tests `v` first; else `f` twice.
    #[inline]
    fn cof(&self, f: Cond, v: u16) -> (Cond, Cond) {
        let n = self.node[idx(f.node())];
        if n.var != v {
            return (f, f);
        }
        let c = f.0 & 1;
        (Cond(n.low ^ c), Cond(n.high ^ c))
    }

    /// The node `(var, low, high)`, made or found: reduced, its high edge regular.
    pub(crate) fn mk(&mut self, var: u16, low: Cond, high: Cond) -> Result<Cond, CondError> {
        if low == high {
            return Ok(low);
        }
        let (low, high, neg) = if high.is_neg() && !self.plant.has(CondPlant::UniqueSkipsComplement) {
            (low.not(), high.not(), true)
        } else {
            (low, high, false)
        };
        let mask = self.unique.len() - 1;
        let hash = hash64(u32::from(var), low.0, high.0);
        let tag = hash & 0xFFFF_FFFF_0000_0000;
        let mut at = hash.truncate_to::<usize>() & mask;
        loop {
            let slot = self.unique[at];
            if slot == 0 {
                break;
            }
            if slot & 0xFFFF_FFFF_0000_0000 == tag {
                let n = slot.truncate_to::<u32>();
                let node = self.node[idx(n)];
                if node.var == var && node.low == low.0 && node.high == high.0 {
                    return Ok(Cond(n << 1).neg_if(neg));
                }
            }
            at = (at + 1) & mask;
        }
        let n = self.node.len();
        if n >= idx(self.cap) {
            return Err(CondError::NodeCap { cap: self.cap });
        }
        spush(&mut self.node, Node { low: low.0, high: high.0, var })?;
        // `n` is below the cap, which is below 2^30.
        let n32 = n.narrow_or_stop::<u32>();
        self.unique[at] = slot_of(hash, n32);
        if n * 2 > self.unique.len() {
            self.grow_unique()?;
        }
        Ok(Cond(n32 << 1).neg_if(neg))
    }

    /// Doubles the unique table and puts every node back.
    fn grow_unique(&mut self) -> Result<(), CondError> {
        let len = self.unique.len() * 2;
        let mut table: AVec<'a, u64> = AVec::new_in(self.alloc);
        table.try_reserve_exact(len)?;
        table.resize(len, 0);
        let mask = len - 1;
        for (n, node) in self.node.iter().enumerate().skip(1) {
            let hash = hash64(u32::from(node.var), node.low, node.high);
            let mut at = hash.truncate_to::<usize>() & mask;
            while table[at] != 0 {
                at = (at + 1) & mask;
            }
            table[at] = slot_of(hash, n.narrow_or_stop::<u32>());
        }
        self.unique = table;
        Ok(())
    }

    #[inline]
    fn cache_get(&self, a: u32, b: u32, c: u32) -> Option<Cond> {
        let e = &self.cache[hash3(a, b, c) & self.cache_mask];
        (e.a == a && e.b == b && e.c == c).then_some(Cond(e.r))
    }

    #[inline]
    fn cache_put(&mut self, a: u32, b: u32, c: u32, r: Cond) {
        let at = hash3(a, b, c) & self.cache_mask;
        self.cache[at] = Entry { a, b, c, r: r.0 };
    }

    /// `ite`'s cache key for `h`: the whole of it, or nothing under the planted bug.
    #[inline]
    fn ite_key(&self, h: Cond) -> u32 {
        if self.plant.has(CondPlant::IteCacheSkipsElse) { 0 } else { h.0 }
    }

    fn result(&mut self) -> Cond {
        let Some(r) = self.results.pop() else { unreachable!("a join's operands are on the result stack") };
        r
    }

    /// Runs the machine from `first` to its one result.
    fn run(&mut self, first: Frame) -> Result<Cond, CondError> {
        self.frames.clear();
        self.results.clear();
        spush(&mut self.frames, first)?;
        while let Some(frame) = self.frames.pop() {
            match frame {
                Frame::Ite(f, g, h) => match normalize(f, g, h) {
                    Norm::Done(r) => spush(&mut self.results, r)?,
                    Norm::Go { f, g, h, neg } => {
                        if let Some(r) = self.cache_get(f.0, g.0, self.ite_key(h)) {
                            self.stats.hits += 1;
                            spush(&mut self.results, r.neg_if(neg))?;
                            continue;
                        }
                        self.stats.steps += 1;
                        let var = self.level(f).min(self.level(g)).min(self.level(h));
                        let (f0, f1) = self.cof(f, var);
                        let (g0, g1) = self.cof(g, var);
                        let (h0, h1) = self.cof(h, var);
                        spush(&mut self.frames, Frame::IteJoin { f, g, h, var, neg })?;
                        spush(&mut self.frames, Frame::Ite(f1, g1, h1))?;
                        spush(&mut self.frames, Frame::Ite(f0, g0, h0))?;
                    }
                },
                Frame::IteJoin { f, g, h, var, neg } => {
                    let high = self.result();
                    let low = self.result();
                    let r = self.mk(var, low, high)?;
                    let key = self.ite_key(h);
                    self.cache_put(f.0, g.0, key, r);
                    spush(&mut self.results, r.neg_if(neg))?;
                }
                Frame::Restrict { f, var, value } => {
                    let top = self.level(f);
                    if f.is_const() || top > var {
                        spush(&mut self.results, f)?;
                    } else if top == var {
                        let (low, high) = self.cof(f, var);
                        let take = value != self.plant.has(CondPlant::RestrictWrongBranch);
                        spush(&mut self.results, if take { high } else { low })?;
                    } else {
                        let neg = f.is_neg();
                        let f = f.regular();
                        let key = u32::from(var) << 1 | u32::from(value);
                        if let Some(r) = self.cache_get(f.0, key, OP_RESTRICT) {
                            self.stats.hits += 1;
                            spush(&mut self.results, r.neg_if(neg))?;
                            continue;
                        }
                        self.stats.steps += 1;
                        let (low, high) = self.cof(f, top);
                        spush(&mut self.frames, Frame::RestrictJoin { f, key, var: top, neg })?;
                        spush(&mut self.frames, Frame::Restrict { f: high, var, value })?;
                        spush(&mut self.frames, Frame::Restrict { f: low, var, value })?;
                    }
                }
                Frame::RestrictJoin { f, key, var, neg } => {
                    let high = self.result();
                    let low = self.result();
                    let r = self.mk(var, low, high)?;
                    self.cache_put(f.0, key, OP_RESTRICT, r);
                    spush(&mut self.results, r.neg_if(neg))?;
                }
                Frame::Exists { f, mut cube } => {
                    if f.is_const() {
                        spush(&mut self.results, f)?;
                        continue;
                    }
                    let top = self.level(f);
                    // Variables of the cube before `f`'s first are not in `f`.
                    while !cube.is_true() && self.level(cube) < top {
                        cube = Cond(self.node[idx(cube.node())].high);
                    }
                    if cube.is_true() {
                        spush(&mut self.results, f)?;
                        continue;
                    }
                    if let Some(r) = self.cache_get(f.0, cube.0, OP_EXISTS) {
                        self.stats.hits += 1;
                        spush(&mut self.results, r)?;
                        continue;
                    }
                    self.stats.steps += 1;
                    let (low, high) = self.cof(f, top);
                    let quantify = self.level(cube) == top;
                    let rest = if quantify { Cond(self.node[idx(cube.node())].high) } else { cube };
                    spush(&mut self.frames, Frame::ExistsJoin { f, cube, var: top, quantify })?;
                    spush(&mut self.frames, Frame::Exists { f: high, cube: rest })?;
                    spush(&mut self.frames, Frame::Exists { f: low, cube: rest })?;
                }
                Frame::ExistsJoin { f, cube, var, quantify } => {
                    let high = self.result();
                    let low = self.result();
                    if quantify {
                        spush(&mut self.frames, Frame::ExistsDone { f, cube })?;
                        spush(&mut self.frames, Frame::Ite(low, Cond::TRUE, high))?;
                    } else {
                        let r = self.mk(var, low, high)?;
                        self.cache_put(f.0, cube.0, OP_EXISTS, r);
                        spush(&mut self.results, r)?;
                    }
                }
                Frame::ExistsDone { f, cube } => {
                    let r = self.result();
                    self.cache_put(f.0, cube.0, OP_EXISTS, r);
                    spush(&mut self.results, r)?;
                }
            }
        }
        Ok(self.result())
    }

    /// `if f { g } else { h }`.
    pub fn ite(&mut self, f: Cond, g: Cond, h: Cond) -> Result<Cond, CondError> {
        match normalize(f, g, h) {
            Norm::Done(r) => Ok(r),
            Norm::Go { .. } => self.run(Frame::Ite(f, g, h)),
        }
    }

    pub fn and(&mut self, a: Cond, b: Cond) -> Result<Cond, CondError> {
        self.ite(a, b, Cond::FALSE)
    }

    pub fn or(&mut self, a: Cond, b: Cond) -> Result<Cond, CondError> {
        self.ite(a, Cond::TRUE, b)
    }

    /// Exclusive or.
    pub fn xor(&mut self, a: Cond, b: Cond) -> Result<Cond, CondError> {
        self.ite(a, b.not(), b)
    }

    /// Whether every configuration `a` admits, `b` admits: `a & !b` is empty, decided without
    /// making a node ([`disjoint`](Bdd::disjoint)).
    pub fn implies(&mut self, a: Cond, b: Cond) -> Result<bool, CondError> {
        self.disjoint(a, b.not())
    }

    /// Whether no configuration satisfies both: a walk over pairs of cofactors that stops at the
    /// first pair both satisfy, makes no node, and caches each pair it proves disjoint.
    pub fn disjoint(&mut self, a: Cond, b: Cond) -> Result<bool, CondError> {
        self.pairs.clear();
        spush(&mut self.pairs, (a, b, false))?;
        while let Some((f, g, done)) = self.pairs.pop() {
            let (f, g) = if f <= g { (f, g) } else { (g, f) };
            if done {
                self.cache_put(f.0, g.0, OP_DISJOINT, Cond::TRUE);
                continue;
            }
            if f.is_false() || g.is_false() || f == g.not() {
                continue;
            }
            // A function that is not false is satisfiable: true with it, or itself twice, is not
            // disjoint.
            if f.is_true() || g.is_true() || f == g {
                return Ok(false);
            }
            if self.cache_get(f.0, g.0, OP_DISJOINT).is_some() {
                self.stats.hits += 1;
                continue;
            }
            self.stats.steps += 1;
            let var = self.level(f).min(self.level(g));
            let (f0, f1) = self.cof(f, var);
            let (g0, g1) = self.cof(g, var);
            spush(&mut self.pairs, (f, g, true))?;
            spush(&mut self.pairs, (f1, g1, false))?;
            spush(&mut self.pairs, (f0, g0, false))?;
        }
        Ok(true)
    }

    /// `f` with `var` fixed at `value`.
    pub fn restrict(&mut self, f: Cond, var: Var, value: bool) -> Result<Cond, CondError> {
        if var.0 >= self.vars {
            return Err(CondError::UnknownVar { var: var.0, vars: self.vars });
        }
        self.run(Frame::Restrict { f, var: var.0, value })
    }

    /// The conjunction of `vars`, each positive: what [`exists`](Bdd::exists) quantifies by.
    pub fn cube(&mut self, vars: &[Var]) -> Result<Cond, CondError> {
        let mut acc = Cond::TRUE;
        for &v in vars {
            let x = self.var(v)?;
            acc = self.and(acc, x)?;
        }
        Ok(acc)
    }

    /// `f` with each of `vars` quantified away: true where some value of them makes `f` true.
    pub fn exists(&mut self, f: Cond, vars: &[Var]) -> Result<Cond, CondError> {
        let cube = self.cube(vars)?;
        self.run(Frame::Exists { f, cube })
    }

    /// Whether `f` holds where each variable takes `value(var)`.
    pub fn eval(&self, f: Cond, value: impl Fn(Var) -> bool) -> bool {
        let mut e = f;
        while !e.is_const() {
            let n = idx(e.node());
            let c = e.0 & 1;
            let node = self.node[n];
            e = Cond(if value(Var(node.var)) { node.high } else { node.low } ^ c);
        }
        e.is_true()
    }

    /// Readies the traversal columns for every node and starts a traversal.
    fn begin(&mut self) -> Result<(), CondError> {
        let n = self.node.len();
        if self.stamp.len() < n {
            self.stamp.try_reserve(n - self.stamp.len())?;
            self.stamp.resize(n, 0);
            self.slot.try_reserve(n - self.slot.len())?;
            self.slot.resize(n, 0);
        }
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.as_mut_slice().fill(0);
            self.epoch = 1;
        }
        Ok(())
    }

    /// Fills `order` with the nodes reachable from `root` (not the constant), children before
    /// parents, each once, in the one order of a depth-first walk that takes the low edge first;
    /// `slot` of each is its place in `order`, from 1. The canonical order of the portable form.
    pub(crate) fn post_order(&mut self, root: Cond) -> Result<(), CondError> {
        self.begin()?;
        self.order.clear();
        self.walk.clear();
        if root.node() != 0 {
            spush(&mut self.walk, (root.node(), false))?;
        }
        while let Some((n, done)) = self.walk.pop() {
            let at = idx(n);
            if done {
                spush(&mut self.order, n)?;
                self.slot[at] = self.order.len().narrow_or_stop::<u32>();
                continue;
            }
            if self.stamp[at] == self.epoch {
                continue;
            }
            self.stamp[at] = self.epoch;
            spush(&mut self.walk, (n, true))?;
            let (low, high) = (self.node[at].low >> 1, self.node[at].high >> 1);
            if high != 0 && self.stamp[idx(high)] != self.epoch {
                spush(&mut self.walk, (high, false))?;
            }
            if low != 0 && self.stamp[idx(low)] != self.epoch {
                spush(&mut self.walk, (low, false))?;
            }
        }
        Ok(())
    }

    /// The node `n`'s variable and edges, for the portable form.
    pub(crate) fn node_of(&self, n: u32) -> (u16, Cond, Cond) {
        let at = idx(n);
        let node = self.node[at];
        (node.var, Cond(node.low), Cond(node.high))
    }

    /// Edge `e` with its node renamed to its traversal slot: what a row refers to.
    pub(crate) fn slot_ref(&self, e: Cond) -> u32 {
        if e.node() == 0 { e.0 } else { self.slot[idx(e.node())] << 1 | (e.0 & 1) }
    }

    /// The node an edge points at.
    pub(crate) fn node_raw(e: Cond) -> u32 {
        e.node()
    }

    /// The variables `f` depends on, in the space's order, into `out` (cleared first): a
    /// record's cfg read list.
    pub fn support(&mut self, f: Cond, out: &mut AVec<'_, Var>) -> Result<(), CondError> {
        out.clear();
        self.post_order(f)?;
        let words = idx(u32::from(self.vars)).div_ceil(64);
        let mut seen: AVec<'a, u64> = AVec::new_in(self.alloc);
        seen.try_reserve_exact(words)?;
        seen.resize(words, 0);
        for &n in self.order.iter() {
            let v = self.node[idx(n)].var;
            seen[usize::from(v / 64)] |= 1 << (v % 64);
        }
        for (w, &bits) in seen.iter().enumerate() {
            let mut bits = bits;
            while bits != 0 {
                let b = bits.trailing_zeros();
                bits &= bits - 1;
                let v = (w * 64 + idx(b)).narrow_or_stop::<u16>();
                out.try_push(Var(v)).map_err(|r| CondError::Alloc(r.why))?;
            }
        }
        Ok(())
    }

    /// How many assignments of all the arena's variables satisfy `f`.
    pub fn models(&mut self, f: Cond) -> Result<u128, CondError> {
        let n = u32::from(self.vars);
        if n > 127 {
            return Err(CondError::CountTooLarge { vars: self.vars });
        }
        self.post_order(f)?;
        self.counts.clear();
        self.counts.try_reserve(self.order.len())?;
        // Models of edge `e` over the variables from its node's on.
        let from = |bdd: &Self, e: Cond| -> (u128, u32) {
            let lvl = u32::from(bdd.level(e)).min(n);
            let m = if e.node() == 0 { 1 } else { bdd.counts[idx(bdd.slot[idx(e.node())] - 1)] };
            let m = if e.is_neg() { (1u128 << (n - lvl)) - m } else { m };
            (m, lvl)
        };
        for i in 0..self.order.len() {
            let at = idx(self.order[i]);
            let node = self.node[at];
            let v = u32::from(node.var);
            let (ml, ll) = from(self, Cond(node.low));
            let (mh, lh) = from(self, Cond(node.high));
            self.counts.push((ml << (ll - v - 1)) + (mh << (lh - v - 1)));
        }
        let (m, l) = from(self, f);
        Ok(m << l)
    }

    /// How many configurations of `valid` satisfy `f`.
    pub fn sat_count(&mut self, f: Cond, valid: Cond) -> Result<u128, CondError> {
        let g = self.and(f, valid)?;
        self.models(g)
    }

    /// A configuration of `valid` that satisfies `f`, into `out` (cleared first): the
    /// variables on one path, the low edge taken where it can be. `false` if there is none.
    pub fn any_sat(&mut self, f: Cond, valid: Cond, out: &mut Witness<'_>) -> Result<bool, CondError> {
        out.clear();
        let mut e = self.and(f, valid)?;
        if e.is_false() {
            return Ok(false);
        }
        while !e.is_const() {
            let at = idx(e.node());
            let c = e.0 & 1;
            let node = self.node[at];
            let low = Cond(node.low ^ c);
            let v = Var(node.var);
            if low.is_false() {
                out.try_push((v, true)).map_err(|r| CondError::Alloc(r.why))?;
                e = Cond(node.high ^ c);
            } else {
                out.try_push((v, false)).map_err(|r| CondError::Alloc(r.why))?;
                e = low;
            }
        }
        Ok(true)
    }

    /// The cofactors of `f` by `var`, `(low, high)`, without the cache: for the cover.
    pub(crate) fn cofactors(&self, f: Cond, var: u16) -> (Cond, Cond) {
        self.cof(f, var)
    }

    /// The first variable of `f`, the constant's after all others: for the cover.
    pub(crate) fn level_of(&self, f: Cond) -> u16 {
        self.level(f)
    }
}
