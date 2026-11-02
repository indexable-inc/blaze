//! The front end's query database, `blaze_query::db`, on its hot paths (`db/*`;
//! docs/v2/plan/lanes/db/query-unify.md, "How it is measured"). The numbers, and those of the
//! fork it replaced on the same shapes, are in docs/v2/plan/lanes/db/speed-of-light.md:
//!
//! - `hit/*`: a warm hit of a function of one interned ID returning `u64`, over [`KEYS`] keys,
//!   in order and scattered, one hit an iteration;
//! - `hit_in_query/*`: the same [`KEYS`] hits from inside a query, with the edge each one
//!   records: an iteration is a write to the outer query's input (of a lower durability than
//!   the keys) and its run, so divide by [`KEYS`] for a hit;
//! - `verify/*`: a write to an unrelated input, then every one of [`READERS`] readers of
//!   [`EDGES`] edges each, over a pool of [`POOL`] leaves, fetched again; `other_durability`
//!   writes below the readers' durability, `same_durability` at it, so each reader verifies
//!   its edges. Divide by [`READERS`] × [`EDGES`] for an edge;
//! - `verify/deep_unrelated`: a memo at depth [`LAYERS`] of a graph of [`LAYERS`] layers of
//!   [`WIDTH`] queries of [`FAN`] edges each (1M edges) over [`LEAVES`] inputs, fetched again
//!   after a write to an input of the same durability outside its closure: what a memo's
//!   transitive-input summary passes without a walk (few inputs, so its 128 bits stay sparse);
//! - `interned/hit`: interning a value already interned, scattered over [`KEYS`];
//! - `cycle/fixpoint`: a write to the input of a cycle of [`CYCLE`] queries, then the cycle run
//!   again to its fixpoint from `cycle_initial` (two iterations).
//!
//! Every benchmark does the same work on every run, so that `blaze_bench` can pair the binary of
//! a change with the binary of its base.

use std::hint::black_box;

use blaze_bench::{IntoBenchmarks, benchmark_fn};

const KEYS: usize = 65_536;
const POOL: usize = 4096;
const READERS: usize = 16_384;
const EDGES: usize = 16;
const CYCLE: usize = 8;
const LAYERS: usize = 64;
const WIDTH: usize = 1024;
const FAN: usize = EDGES;
const LEAVES: usize = 4;

const _: () = assert!(LAYERS * WIDTH * FAN == 1 << 20);

/// The `i`th key of a walk over [`KEYS`]: in order, or scattered by an odd stride (a
/// permutation, since [`KEYS`] is a power of two).
fn walk(i: usize, scattered: bool) -> usize {
    if scattered { (i * 40_503) % KEYS } else { i }
}

/// The `e`th of the [`POOL`] entries the `r`th reader reads.
fn pick(r: usize, e: usize) -> usize {
    spread(r, e, POOL)
}

/// The `e`th of `n` entries the `r`th reader reads.
fn spread(r: usize, e: usize, n: usize) -> usize {
    (r * 2_654_435_761 + e * 40_503) % n
}

mod engine {
    use blaze_query::db::{Database, Durability, Id, Setter, Storage};

    use super::{
        CYCLE, EDGES, FAN, KEYS, LAYERS, LEAVES, POOL, READERS, WIDTH, pick, spread, walk,
    };

    /// The storage alone, as `base_db::Db` holds it.
    pub(crate) struct Db(Storage);

    impl Db {
        fn new() -> Db {
            Db(Storage::new(None))
        }
    }

    impl Database for Db {
        fn storage(&self) -> &Storage {
            &self.0
        }

        fn storage_mut(&mut self) -> &mut Storage {
            &mut self.0
        }
    }

    #[blaze_query::interned(unsafe(no_lifetime))]
    pub(crate) struct Key {
        #[returns(copy)]
        value: u32,
    }

    #[blaze_query::tracked(returns(copy))]
    #[inline(never)]
    pub(crate) fn double(db: &Db, key: Key) -> u64 {
        u64::from(key.value(db)) * 2
    }

    /// Interned keys `0..KEYS` and the order a walk takes them in, every hit warm.
    pub(crate) struct Hits {
        pub(crate) db: Db,
        pub(crate) order: Vec<Key>,
    }

    impl Hits {
        pub(crate) fn new(scattered: bool) -> Hits {
            let db = Db::new();
            let keys: Vec<Key> = (0..KEYS).map(|i| Key::new(&db, i as u32)).collect();
            let order: Vec<Key> = (0..KEYS).map(|i| keys[walk(i, scattered)]).collect();
            for &key in &order {
                std::hint::black_box(double(&db, key));
            }
            Hits { db, order }
        }
    }

    #[blaze_query::input]
    pub(crate) struct Sweep {
        #[returns(copy)]
        tick: u32,
        #[returns(ref)]
        order: Vec<Key>,
    }

    #[blaze_query::tracked(returns(copy))]
    #[inline(never)]
    pub(crate) fn sweep(db: &Db, sweep: Sweep) -> u64 {
        let tick = u64::from(sweep.tick(db));
        sweep.order(db).iter().map(|&key| double(db, key)).sum::<u64>() + tick
    }

    /// [`Hits`] behind an input of low durability that a query walks; the keys are interned
    /// outside any query, so their hits are of the highest durability.
    pub(crate) struct InQuery {
        pub(crate) db: Db,
        pub(crate) input: Sweep,
        pub(crate) tick: u32,
    }

    impl InQuery {
        pub(crate) fn new(scattered: bool) -> InQuery {
            let Hits { db, order } = Hits::new(scattered);
            let input = Sweep::builder(0, order).durability(Durability::LOW).new(&db);
            std::hint::black_box(sweep(&db, input));
            InQuery { db, input, tick: 0 }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            self.input.set_tick(&mut self.db).to(self.tick);
            sweep(&self.db, self.input)
        }
    }

    #[blaze_query::input]
    pub(crate) struct Leaf {
        #[returns(copy)]
        value: u32,
    }

    #[blaze_query::input]
    pub(crate) struct Reader {
        #[returns(ref)]
        picks: Vec<Leaf>,
    }

    #[blaze_query::input]
    pub(crate) struct Other {
        #[returns(copy)]
        value: u32,
    }

    #[blaze_query::tracked(returns(copy))]
    #[inline(never)]
    fn leaf(db: &Db, input: Leaf) -> u32 {
        input.value(db).wrapping_mul(3)
    }

    #[blaze_query::tracked(returns(copy))]
    #[inline(never)]
    fn read_all(db: &Db, reader: Reader) -> u64 {
        reader.picks(db).iter().map(|&input| u64::from(leaf(db, input))).sum()
    }

    /// Readers and leaves of medium durability, verified, and an input none of them reads.
    pub(crate) struct Verify {
        db: Db,
        readers: Vec<Reader>,
        other: Other,
        durability: Durability,
        tick: u32,
        pub(crate) expected: u64,
    }

    impl Verify {
        /// `same`: the unrelated input is of the readers' durability, else of a lower one.
        pub(crate) fn new(same: bool) -> Verify {
            let db = Db::new();
            let pool: Vec<Leaf> = (0..POOL)
                .map(|i| Leaf::builder(i as u32).durability(Durability::MEDIUM).new(&db))
                .collect();
            let readers: Vec<Reader> = (0..READERS)
                .map(|r| {
                    let picks = (0..EDGES).map(|e| pool[pick(r, e)]).collect();
                    Reader::builder(picks).durability(Durability::MEDIUM).new(&db)
                })
                .collect();
            let durability = if same { Durability::MEDIUM } else { Durability::LOW };
            let other = Other::builder(0).durability(durability).new(&db);
            let expected = readers.iter().map(|&reader| read_all(&db, reader)).sum();
            Verify { db, readers, other, durability, tick: 0, expected }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            self.other.set_value(&mut self.db).with_durability(self.durability).to(self.tick);
            self.readers.iter().map(|&reader| read_all(&self.db, reader)).sum()
        }
    }

    #[blaze_query::interned(unsafe(no_lifetime))]
    pub(crate) struct Node {
        #[returns(copy)]
        layer: u32,
        #[returns(copy)]
        index: u32,
    }

    /// The deep graph's leaves and its nodes, layer by layer (`layer * WIDTH + index`).
    #[blaze_query::input(singleton)]
    pub(crate) struct Graph {
        #[returns(ref)]
        leaves: Vec<Leaf>,
        #[returns(ref)]
        nodes: Vec<Node>,
    }

    #[blaze_query::tracked(returns(copy))]
    #[inline(never)]
    fn node(db: &Db, at: Node) -> u64 {
        let graph = Graph::get(db);
        let (layer, index) = (at.layer(db) as usize, at.index(db) as usize);
        if layer == 0 {
            return u64::from(leaf(db, graph.leaves(db)[index % LEAVES]));
        }
        let below = &graph.nodes(db)[(layer - 1) * WIDTH..layer * WIDTH];
        (0..FAN)
            .map(|e| node(db, below[spread(index + layer * WIDTH, e, WIDTH)]))
            .fold(0, u64::wrapping_add)
    }

    /// [`LAYERS`] layers of [`WIDTH`] queries of [`FAN`] edges over [`LEAVES`] leaves, all of
    /// medium durability and verified, and the memo on top; and an input of the same
    /// durability that nothing in the graph reads.
    pub(crate) struct Deep {
        db: Db,
        top: Node,
        other: Other,
        tick: u32,
        pub(crate) expected: u64,
    }

    impl Deep {
        pub(crate) fn new() -> Deep {
            let mut db = Db::new();
            let leaves: Vec<Leaf> = (0..LEAVES)
                .map(|i| Leaf::builder(i as u32).durability(Durability::MEDIUM).new(&db))
                .collect();
            let nodes: Vec<Node> = (0..=LAYERS)
                .flat_map(|layer| (0..WIDTH).map(move |index| (layer as u32, index as u32)))
                .map(|(layer, index)| Node::new(&db, layer, index))
                .collect();
            let top = nodes[LAYERS * WIDTH];
            let _ = Graph::builder(leaves, nodes).durability(Durability::MEDIUM).new(&mut db);
            let other = Other::builder(0).durability(Durability::MEDIUM).new(&db);
            let expected = node(&db, top);
            Deep { db, top, other, tick: 0, expected }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            self.other.set_value(&mut self.db).with_durability(Durability::MEDIUM).to(self.tick);
            node(&self.db, self.top)
        }
    }

    #[blaze_query::input]
    pub(crate) struct Seed {
        #[returns(copy)]
        cap: u32,
    }

    fn initial(_db: &Db, _id: Id, _seed: Seed) -> u32 {
        0
    }

    fn step(db: &Db, seed: Seed, next: u32) -> u32 {
        (next + 1).min(seed.cap(db))
    }

    #[blaze_query::tracked(returns(copy), cycle_initial = initial)]
    pub(crate) fn c0(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c1(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c1(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c2(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c2(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c3(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c3(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c4(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c4(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c5(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c5(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c6(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c6(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c7(db, seed))
    }

    #[blaze_query::tracked(returns(copy))]
    fn c7(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c0(db, seed))
    }

    const _: () = assert!(CYCLE == 8);

    /// The cycle `c0 → c1 → … → c7 → c0` over a seed whose cap each run moves between 3
    /// and 4: from `cycle_initial`'s 0 the first iteration reaches the cap, the second
    /// confirms it.
    pub(crate) struct Cycle {
        db: Db,
        seed: Seed,
        cap: u32,
    }

    impl Cycle {
        pub(crate) fn new() -> Cycle {
            let db = Db::new();
            let seed = Seed::new(&db, 3);
            assert_eq!(c0(&db, seed), 3);
            Cycle { db, seed, cap: 3 }
        }

        pub(crate) fn run(&mut self) -> u32 {
            self.cap ^= 7;
            self.seed.set_cap(&mut self.db).to(self.cap);
            let value = c0(&self.db, self.seed);
            debug_assert_eq!(value, self.cap);
            value
        }
    }
}

fn benchmarks() -> impl IntoBenchmarks {
    [
        benchmark_fn("db/hit/sequential", |b| {
            let hits = engine::Hits::new(false);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % KEYS;
                engine::double(&hits.db, black_box(hits.order[at]))
            })
        }),
        benchmark_fn("db/hit/scattered", |b| {
            let hits = engine::Hits::new(true);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % KEYS;
                engine::double(&hits.db, black_box(hits.order[at]))
            })
        }),
        benchmark_fn("db/hit_in_query/sequential", |b| {
            let mut sweep = engine::InQuery::new(false);
            b.iter(move || sweep.run())
        }),
        benchmark_fn("db/hit_in_query/scattered", |b| {
            let mut sweep = engine::InQuery::new(true);
            b.iter(move || sweep.run())
        }),
        benchmark_fn("db/verify/other_durability", |b| {
            let mut verify = engine::Verify::new(false);
            b.iter(move || {
                let sum = verify.run();
                assert_eq!(sum, verify.expected);
                sum
            })
        }),
        benchmark_fn("db/verify/same_durability", |b| {
            let mut verify = engine::Verify::new(true);
            b.iter(move || {
                let sum = verify.run();
                assert_eq!(sum, verify.expected);
                sum
            })
        }),
        benchmark_fn("db/verify/deep_unrelated", |b| {
            let mut deep = engine::Deep::new();
            b.iter(move || {
                let sum = deep.run();
                assert_eq!(sum, deep.expected);
                sum
            })
        }),
        benchmark_fn("db/interned/hit", |b| {
            let hits = engine::Hits::new(true);
            let mut at = 0;
            b.iter(move || {
                at = (at + 1) % KEYS;
                engine::Key::new(&hits.db, black_box(walk(at, true) as u32))
            })
        }),
        benchmark_fn("db/cycle/fixpoint", |b| {
            let mut cycle = engine::Cycle::new();
            b.iter(move || cycle.run())
        }),
    ]
}

blaze_bench::main!(benchmarks());
