//! The machine's shape as the scheduler is given it: each core's SMT group, L3 group, NUMA
//! node, socket and capacity, and the kernel's node distance table. From them each core gets
//! its **tiers**: every other core, grouped nearest first (its SMT siblings, its L3 group, its
//! node, the other nodes of its socket by distance, then the other sockets' nodes by
//! distance), one bitmask per tier, so a search for the nearest parked core or the nearest
//! core with work is a few word ANDs per tier.
//!
//! Nothing here reads the machine. On Linux the driver reads `cpu_capacity`, the hybrid
//! `cpu_core`/`cpu_atom` lists and `/sys/devices/system/node/nodeN/distance`; on macOS, which
//! pins no thread to a core, [`Cores::classes`] makes one worker per thread of each QoS class
//! with no locality between them, and the scheduler decides per class, not per core id.

use std::fmt;

/// How far apart two cores are, nearest first: a steal or a handoff looks at each tier in turn
/// and reaches the next only when the one before has nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Level {
    /// The core's SMT siblings: one physical core, its L1 and L2.
    Sibling = 0,
    /// The rest of its L3 group (a CCD, a cluster).
    L3 = 1,
    /// The rest of its NUMA node.
    Node = 2,
    /// The other nodes of its socket (sub-NUMA clustering), nearest by distance first.
    Socket = 3,
    /// Every other socket's cores, nearest by distance first.
    Remote = 4,
}

pub const LEVELS: usize = 5;

/// One core: the ids of the groups it is in, and its speed. Group ids are any `u32` (equal ids
/// are one group); node ids index the distance table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct CoreInfo {
    pub smt: u32,
    pub l3: u32,
    pub node: u32,
    pub socket: u32,
    /// Relative speed of one thread here (Linux's `cpu_capacity` scale: the fastest is 1024).
    pub capacity: u32,
}

/// The kernel's node distance table, `nodes × nodes`, row-major: `table[from * nodes + to]`.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: once per scheduler made
pub struct Distances {
    nodes: u32,
    table: Vec<u32>,
}

impl Distances {
    /// # Errors
    ///
    /// Where `table` is not `nodes × nodes`.
    pub fn new(nodes: u32, table: Vec<u32>) -> Result<Distances, ShapeError> {
        if nodes == 0 || table.len() != nodes as usize * nodes as usize {
            return Err(ShapeError::Distances { nodes, len: table.len() });
        }
        Ok(Distances { nodes, table })
    }

    /// The table of a machine with one node: Linux's `node0/distance` reads `10`.
    pub fn one_node() -> Distances {
        let mut table = Vec::with_capacity(1);
        table.push(10);
        Distances { nodes: 1, table }
    }

    pub fn nodes(&self) -> u32 {
        self.nodes
    }

    pub fn get(&self, from: u32, to: u32) -> u32 {
        self.table[from as usize * self.nodes as usize + to as usize]
    }
}

/// The shape is not nested (SMT siblings in two L3 groups, an L3 group on two nodes, a node
/// on two sockets, which would let a "nearer" tier cross a node), or names a node the
/// distance table does not have.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeError {
    NoCores,
    TooManyCores(usize),
    SiblingsSplit { core: u32, other: u32 },
    L3Split { core: u32, other: u32 },
    NodeSplit { core: u32, other: u32 },
    Distances { nodes: u32, len: usize },
    NodeOutOfTable { core: u32, node: u32 },
    NoCapacity { core: u32 },
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShapeError::NoCores => f.write_str("a scheduler's shape has no core"),
            ShapeError::TooManyCores(n) => write!(f, "a scheduler's shape has {n} cores, past {MAX_CORES}"),
            ShapeError::SiblingsSplit { core, other } => write!(
                f,
                "cores {core} and {other} are SMT siblings in different L3 groups, nodes or sockets"
            ),
            ShapeError::L3Split { core, other } => {
                write!(f, "cores {core} and {other} share an L3 group but not a NUMA node")
            }
            ShapeError::NodeSplit { core, other } => {
                write!(f, "cores {core} and {other} share a NUMA node but not a socket")
            }
            ShapeError::Distances { nodes, len } => {
                write!(f, "a node distance table of {len} entries is not {nodes} × {nodes}")
            }
            ShapeError::NodeOutOfTable { core, node } => {
                write!(f, "core {core} is on node {node}, which the distance table does not have")
            }
            ShapeError::NoCapacity { core } => write!(f, "core {core} has capacity 0"),
        }
    }
}

impl std::error::Error for ShapeError {}

/// Past every machine made; a core id is a `u32` and a mask a few cache lines.
pub const MAX_CORES: usize = 1 << 16;

/// The cores, with each core's tiers.
pub struct Cores {
    info: Vec<CoreInfo>,
    /// Words of one mask.
    words: usize,
    /// Every core.
    all: Vec<u64>,
    /// The cores of the highest capacity, and the rest.
    big: Vec<u64>,
    small: Vec<u64>,
    /// Core `c`'s tiers are `tier_start[c]..tier_start[c + 1]`.
    tier_start: Vec<u32>,
    tier_level: Vec<Level>,
    /// Tier `t`'s mask: `tier_masks[t * words..][..words]`, never the core itself.
    tier_masks: Vec<u64>,
}

impl Cores {
    /// # Errors
    ///
    /// Where the shape has no core, more than [`MAX_CORES`], a core of capacity 0, a node the
    /// table lacks, or is not nested (see [`ShapeError`]).
    pub fn new(info: Vec<CoreInfo>, distances: &Distances) -> Result<Cores, ShapeError> {
        let n = info.len();
        if n == 0 {
            return Err(ShapeError::NoCores);
        }
        if n > MAX_CORES {
            return Err(ShapeError::TooManyCores(n));
        }
        for (a, x) in info.iter().enumerate() {
            let core = a as u32;
            if x.node >= distances.nodes() {
                return Err(ShapeError::NodeOutOfTable { core, node: x.node });
            }
            if x.capacity == 0 {
                return Err(ShapeError::NoCapacity { core });
            }
            for (b, y) in info.iter().enumerate().skip(a + 1) {
                let other = b as u32;
                if x.smt == y.smt && (x.l3 != y.l3 || x.node != y.node || x.socket != y.socket) {
                    return Err(ShapeError::SiblingsSplit { core, other });
                }
                if x.l3 == y.l3 && x.node != y.node {
                    return Err(ShapeError::L3Split { core, other });
                }
                if x.node == y.node && x.socket != y.socket {
                    return Err(ShapeError::NodeSplit { core, other });
                }
            }
        }
        let words = n.div_ceil(64);
        let mut all = vec![0u64; words];
        let mut big = vec![0u64; words];
        let top = info.iter().map(|it| it.capacity).max().unwrap_or(0);
        for (b, y) in info.iter().enumerate() {
            all[b / 64] |= 1 << (b % 64);
            if y.capacity == top {
                big[b / 64] |= 1 << (b % 64);
            }
        }
        let mut small = Vec::with_capacity(words);
        for w in 0..words {
            small.push(all[w] & !big[w]);
        }
        let mut tier_start = Vec::with_capacity(n + 1);
        let mut tier_level = Vec::with_capacity(n * LEVELS);
        let mut tier_masks = Vec::with_capacity(n * LEVELS * words);
        // alloc: setup, once per scheduler
        let mut keys: Vec<(Level, u32, u32)> = Vec::with_capacity(n);
        for (a, x) in info.iter().enumerate() {
            tier_start.push(tier_level.len() as u32);
            keys.clear();
            for (b, y) in info.iter().enumerate() {
                if a == b {
                    continue;
                }
                let level = Cores::level_of(x, y);
                let distance = match level {
                    Level::Socket | Level::Remote => distances.get(x.node, y.node),
                    _ => 0,
                };
                keys.push((level, distance, b as u32));
            }
            keys.sort_unstable();
            let mut last = None;
            for &(level, distance, b) in &keys {
                if last != Some((level, distance)) {
                    last = Some((level, distance));
                    tier_level.push(level);
                    tier_masks.resize(tier_masks.len() + words, 0u64);
                }
                let at = tier_masks.len() - words;
                tier_masks[at + b as usize / 64] |= 1 << (b % 64);
            }
        }
        tier_start.push(tier_level.len() as u32);
        Ok(Cores { info, words, all, big, small, tier_start, tier_level, tier_masks })
    }

    /// `count` cores of one L3 group on one node and socket, none of them SMT siblings, all
    /// of one capacity.
    pub fn flat(count: usize) -> Result<Cores, ShapeError> {
        Cores::classes(&[(1024, count)])
    }

    /// One worker per thread of each class, `(capacity, count)`, with no locality between
    /// them: macOS, where a thread cannot be pinned to a core and the driver sets each
    /// worker's QoS class (user-interactive for the highest capacity, utility for the rest).
    pub fn classes(classes: &[(u32, usize)]) -> Result<Cores, ShapeError> {
        let count = classes.iter().map(|it| it.1).sum::<usize>();
        let mut info = Vec::with_capacity(count);
        for &(capacity, threads) in classes {
            for _ in 0..threads {
                let smt = info.len() as u32;
                info.push(CoreInfo { smt, l3: 0, node: 0, socket: 0, capacity });
            }
        }
        Cores::new(info, &Distances::one_node())
    }

    fn level_of(x: &CoreInfo, y: &CoreInfo) -> Level {
        if x.smt == y.smt {
            Level::Sibling
        } else if x.l3 == y.l3 {
            Level::L3
        } else if x.node == y.node {
            Level::Node
        } else if x.socket == y.socket {
            Level::Socket
        } else {
            Level::Remote
        }
    }

    pub fn len(&self) -> usize {
        self.info.len()
    }

    pub fn is_empty(&self) -> bool {
        self.info.is_empty()
    }

    pub fn info(&self, core: u32) -> CoreInfo {
        self.info[core as usize]
    }

    /// How far `to` is from `from`; `Sibling` for a core and itself.
    pub fn level(&self, from: u32, to: u32) -> Level {
        Cores::level_of(&self.info[from as usize], &self.info[to as usize])
    }

    /// Of the highest capacity in the shape.
    pub fn is_big(&self, core: u32) -> bool {
        get(&self.big, core)
    }

    pub(crate) fn words(&self) -> usize {
        self.words
    }

    pub(crate) fn all(&self) -> &[u64] {
        &self.all
    }

    pub(crate) fn big(&self) -> &[u64] {
        &self.big
    }

    pub(crate) fn small(&self) -> &[u64] {
        &self.small
    }

    /// `core`'s tiers, nearest first: each one's level and mask.
    pub(crate) fn tiers(&self, core: u32) -> impl Iterator<Item = (Level, &[u64])> + '_ {
        let (start, end) = (self.tier_start[core as usize], self.tier_start[core as usize + 1]);
        (start as usize..end as usize)
            .map(move |t| (self.tier_level[t], &self.tier_masks[t * self.words..(t + 1) * self.words]))
    }

    /// Whether `core` is in L3 group `l3`, or node `node`: for a placement hint.
    pub(crate) fn in_l3(&self, core: u32, l3: u32) -> bool {
        self.info[core as usize].l3 == l3
    }

    pub(crate) fn in_node(&self, core: u32, node: u32) -> bool {
        self.info[core as usize].node == node
    }
}

/// The first core set in `bits`, `mask` and `filter` (when given), searching up from `start`
/// and wrapping, so searchers that start at their own id spread over their victims.
pub(crate) fn first_from(bits: &[u64], mask: &[u64], filter: Option<&[u64]>, start: usize) -> Option<u32> {
    let words = bits.len();
    let word = |w: usize| bits[w] & mask[w] & filter.map_or(u64::MAX, |it| it[w]);
    let start = if start >= words * 64 { 0 } else { start };
    let (w0, b0) = (start / 64, start % 64);
    let high = word(w0) & (u64::MAX << b0);
    if high != 0 {
        return Some((w0 * 64) as u32 + high.trailing_zeros());
    }
    for step in 1..words {
        let w = (w0 + step) % words;
        let m = word(w);
        if m != 0 {
            return Some((w * 64) as u32 + m.trailing_zeros());
        }
    }
    let low = word(w0) & !(u64::MAX << b0);
    if low != 0 {
        return Some((w0 * 64) as u32 + low.trailing_zeros());
    }
    None
}

/// Whether any core is set in both.
pub(crate) fn any_in(bits: &[u64], mask: &[u64]) -> bool {
    bits.iter().zip(mask).any(|(a, b)| a & b != 0)
}

pub(crate) fn set(bits: &mut [u64], core: u32, on: bool) {
    let (w, b) = (core as usize / 64, core % 64);
    if on {
        bits[w] |= 1 << b;
    } else {
        bits[w] &= !(1 << b);
    }
}

pub(crate) fn get(bits: &[u64], core: u32) -> bool {
    bits[core as usize / 64] & (1 << (core % 64)) != 0
}
