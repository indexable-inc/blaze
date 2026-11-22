//! The front end's session memo, `blaze_memo`, on its hot paths (`db/*`; docs/v2/plan.md, "How
//! it is measured"):
//!
//! - `hit/*`: a warm hit of a function of one interned ID returning `u64`, over [`KEYS`] keys,
//!   in order and scattered, one hit an iteration;
//! - `hit_in_query/*`: the same [`KEYS`] hits from inside a query: an iteration is the next
//!   snapshot with the outer query's input (in its own arena) made again with a new value, and
//!   the outer query's run, so divide by [`KEYS`] for a hit;
//! - `next/unrelated`: the next snapshot with an unrelated arena's input made again, then every
//!   one of [`READERS`] readers of [`EDGES`] leaves each, over a pool of [`POOL`] leaves, fetched
//!   again: the readers' arena is carried, so each is a hit and nothing is checked. Divide by
//!   [`READERS`] for a reader;
//! - `next/deep_unrelated`: a memo at depth [`LAYERS`] of a graph of [`LAYERS`] layers of
//!   [`WIDTH`] queries of [`FAN`] reads each (1M reads) over [`LEAVES`] inputs, fetched again
//!   after the next snapshot dropped only an arena outside its closure: one hit;
//! - `interned/hit`: interning a value already interned, scattered over [`KEYS`];
//! - `cycle/fixpoint`: the next snapshot with the input of a cycle of [`CYCLE`] queries made
//!   again, then the cycle run again to its fixpoint from `cycle_initial` (two iterations).
//!
//! blaze-query's `verify/other_durability` and `verify/same_durability` measured proving edges
//! of an earlier revision; the memo has no edges and no durabilities (a carried arena is proved
//! by being carried), so both are the one `next/unrelated`.
//!
//! Every benchmark does the same work on every run, so that `blaze_bench` can pair the binary of
//! a change with the binary of its base.

#![feature(allocator_ext)]

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
    use std::{alloc::Global, sync::Arc};

    use blaze_ids::ContentId;
    use blaze_mem::Tag;
    use blaze_memo::{Alone, Builder, Config, Database, Fact, Memo, Reads};

    use super::{
        CYCLE, EDGES, FAN, KEYS, LAYERS, LEAVES, POOL, READERS, WIDTH, pick, spread, walk,
    };

    /// The arena of the inputs a benchmark changes or reads; the session arena (0) holds the
    /// interned keys.
    const INPUTS: u32 = 1;
    /// An arena nothing the measured queries read is in.
    const OTHER: u32 = 2;

    /// A snapshot, as `base_db::Db` holds one.
    pub(crate) struct Db(Memo);

    impl Database for Db {
        fn memo(&self) -> &Memo {
            &self.0
        }
    }

    fn config() -> Config {
        Config {
            arenas: 3,
            reads: Reads::Off,
            flight: Arc::new_in(Alone::new(), Global),
            cap_bytes: u64::MAX,
            plant: Default::default(),
        }
    }

    /// The fact an input of benchmark `what` holding `value` is known by.
    fn fact(what: &str, value: u64) -> Fact {
        Fact {
            id: ContentId::of("blaze-bench query_hit input", what.as_bytes()),
            digest: ContentId::of("blaze-bench query_hit value", &value.to_le_bytes()),
        }
    }

    /// The first snapshot: the session arena and the two others, `make` filling them.
    fn first(make: impl FnOnce(&mut Builder)) -> Db {
        let mut builder = Memo::builder(config());
        builder.arena(0, Tag::Query);
        make(&mut builder);
        Db(builder.build())
    }

    /// The next snapshot of `db` with `changed` dropped, `remake` adding it again.
    fn next(db: &Db, changed: u32, remake: impl FnOnce(&mut Builder)) -> Db {
        let id = db.0.arenas().find(|it| it.index() == changed).expect("the arena is in the snapshot");
        let mut builder = db
            .0
            .next(&[id])
            .unwrap_or_else(|refused| {
                panic!("query_hit: the next snapshot, remaking arena {changed}, was refused: {refused}")
            })
            .builder;
        remake(&mut builder);
        Db(builder.build())
    }

    /// Adds arena `index`, borrowing from the session arena (its inputs hold interned keys).
    fn arena(builder: &mut Builder, index: u32) -> blaze_memo::Inputs<'_> {
        // The session arena this builder holds, at the generation it holds: never a constant.
        let session = builder.session();
        let inputs = builder.arena(index, Tag::Query);
        inputs.borrows(session);
        inputs
    }

    #[blaze_memo::interned(unsafe(no_lifetime))]
    pub(crate) struct Key {
        #[returns(copy)]
        value: u32,
    }

    #[blaze_memo::tracked(returns(copy))]
    #[inline(never)]
    pub(crate) fn double(db: &Db, key: Key) -> u64 {
        u64::from(key.value(db)) * 2
    }

    /// Interned keys `0..KEYS` and the order a walk takes them in, every hit warm.
    pub(crate) struct Hits {
        pub(crate) db: Db,
        pub(crate) order: Vec<Key, Global>,
    }

    impl Hits {
        pub(crate) fn new(scattered: bool) -> Hits {
            let db = first(|builder| {
                arena(builder, INPUTS);
                arena(builder, OTHER);
            });
            let mut keys = Vec::with_capacity_in(KEYS, Global);
            keys.extend((0..KEYS).map(|i| Key::new(&db, i as u32)));
            let mut order = Vec::with_capacity_in(KEYS, Global);
            order.extend((0..KEYS).map(|i| keys[walk(i, scattered)]));
            for &key in &order {
                std::hint::black_box(double(&db, key));
            }
            Hits { db, order }
        }
    }

    #[blaze_memo::input]
    pub(crate) struct Sweep {
        #[returns(copy)]
        tick: u32,
        #[returns(ref)]
        order: Vec<Key, Global>,
    }

    #[blaze_memo::tracked(returns(copy))]
    #[inline(never)]
    pub(crate) fn sweep(db: &Db, sweep: Sweep) -> u64 {
        let tick = u64::from(sweep.tick(db));
        sweep.order(db).iter().map(|&key| double(db, key)).sum::<u64>() + tick
    }

    /// [`Hits`] behind an input in its own arena that a query walks; the keys are interned
    /// outside any query, in the session arena, which the next snapshot carries.
    pub(crate) struct InQuery {
        pub(crate) db: Db,
        pub(crate) input: Sweep,
        order: Vec<Key, Global>,
        pub(crate) tick: u32,
    }

    impl InQuery {
        pub(crate) fn new(scattered: bool) -> InQuery {
            let Hits { db, order } = Hits::new(scattered);
            let mut made = None;
            let db = next(&db, INPUTS, |builder| made = Some(InQuery::remake(builder, &order, 0)));
            let input = made.expect("made in the next snapshot");
            std::hint::black_box(sweep(&db, input));
            InQuery { db, input, order, tick: 0 }
        }

        /// The sweep input of `tick`, made in the inputs' arena of the snapshot being built.
        fn remake(builder: &mut Builder, order: &[Key], tick: u32) -> Sweep {
            let inputs = arena(builder, INPUTS);
            let mut kept = Vec::with_capacity_in(order.len(), Global);
            kept.extend_from_slice(order);
            Sweep::new(&inputs, fact("sweep", u64::from(tick)), tick, kept)
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            let (tick, order) = (self.tick, &self.order);
            let mut made = None;
            self.db = next(&self.db, INPUTS, |builder| made = Some(InQuery::remake(builder, order, tick)));
            self.input = made.expect("made in the next snapshot");
            sweep(&self.db, self.input)
        }
    }

    #[blaze_memo::input]
    pub(crate) struct Leaf {
        #[returns(copy)]
        value: u32,
    }

    #[blaze_memo::input]
    pub(crate) struct Reader {
        #[returns(ref)]
        picks: Vec<Leaf, Global>,
    }

    #[blaze_memo::input]
    pub(crate) struct Other {
        #[returns(copy)]
        value: u32,
    }

    #[blaze_memo::tracked(returns(copy))]
    #[inline(never)]
    fn leaf(db: &Db, input: Leaf) -> u32 {
        input.value(db).wrapping_mul(3)
    }

    #[blaze_memo::tracked(returns(copy))]
    #[inline(never)]
    fn read_all(db: &Db, reader: Reader) -> u64 {
        reader.picks(db).iter().map(|&input| u64::from(leaf(db, input))).sum()
    }

    /// Makes the unrelated input in arena [`OTHER`].
    fn other(builder: &mut Builder, tick: u32) -> Other {
        let inputs = arena(builder, OTHER);
        Other::new(&inputs, fact("other", u64::from(tick)), tick)
    }

    /// Readers and leaves in one arena, warm, and an input in another none of them reads.
    pub(crate) struct Next {
        db: Db,
        readers: Vec<Reader, Global>,
        tick: u32,
        pub(crate) expected: u64,
    }

    impl Next {
        pub(crate) fn new() -> Next {
            let mut readers = Vec::with_capacity_in(READERS, Global);
            let db = first(|builder| {
                let inputs = arena(builder, INPUTS);
                let mut pool = Vec::with_capacity_in(POOL, Global);
                pool.extend(
                    (0..POOL).map(|i| Leaf::new(&inputs, fact("leaf", i as u64), i as u32)),
                );
                readers.extend((0..READERS).map(|r| {
                    let mut picks = Vec::with_capacity_in(EDGES, Global);
                    picks.extend((0..EDGES).map(|e| pool[pick(r, e)]));
                    Reader::new(&inputs, fact("reader", r as u64), picks)
                }));
                other(builder, 0);
            });
            let expected = readers.iter().map(|&reader| read_all(&db, reader)).sum();
            Next { db, readers, tick: 0, expected }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            let tick = self.tick;
            self.db = next(&self.db, OTHER, |builder| _ = other(builder, tick));
            self.readers.iter().map(|&reader| read_all(&self.db, reader)).sum()
        }
    }

    #[blaze_memo::interned(unsafe(no_lifetime))]
    pub(crate) struct Node {
        #[returns(copy)]
        layer: u32,
        #[returns(copy)]
        index: u32,
    }

    /// The deep graph's leaves and its nodes, layer by layer (`layer * WIDTH + index`).
    #[blaze_memo::input]
    pub(crate) struct Graph {
        #[returns(ref)]
        leaves: Vec<Leaf, Global>,
        #[returns(ref)]
        nodes: Vec<Node, Global>,
    }

    /// Keyed by the graph as well as the node, so its home is the graph's arena and not the
    /// session's (a session key may not read a crate's arena).
    #[blaze_memo::tracked(returns(copy))]
    #[inline(never)]
    fn node(db: &Db, graph: Graph, at: Node) -> u64 {
        let (layer, index) = (at.layer(db) as usize, at.index(db) as usize);
        if layer == 0 {
            return u64::from(leaf(db, graph.leaves(db)[index % LEAVES]));
        }
        let below = &graph.nodes(db)[(layer - 1) * WIDTH..layer * WIDTH];
        (0..FAN)
            .map(|e| node(db, graph, below[spread(index + layer * WIDTH, e, WIDTH)]))
            .fold(0, u64::wrapping_add)
    }

    /// [`LAYERS`] layers of [`WIDTH`] queries of [`FAN`] reads over [`LEAVES`] leaves, warm,
    /// and the memo on top; and an input in another arena that nothing in the graph reads.
    pub(crate) struct Deep {
        db: Db,
        graph: Graph,
        top: Node,
        tick: u32,
        pub(crate) expected: u64,
    }

    impl Deep {
        pub(crate) fn new() -> Deep {
            // The nodes are interned in the session arena, before the graph's arena is made.
            let mut builder = Memo::builder(config());
            builder.arena(0, Tag::Query);
            let session = Db(builder.build());
            let mut nodes = Vec::with_capacity_in((LAYERS + 1) * WIDTH, Global);
            nodes.extend(
                (0..=LAYERS)
                    .flat_map(|layer| (0..WIDTH).map(move |index| (layer as u32, index as u32)))
                    .map(|(layer, index)| Node::new(&session, layer, index)),
            );
            let top = nodes[LAYERS * WIDTH];
            let mut builder = session
                .0
                .next(&[])
                .unwrap_or_else(|refused| {
                    panic!("query_hit: the deep graph's snapshot after the session's was refused: {refused}")
                })
                .builder;
            let inputs = arena(&mut builder, INPUTS);
            let mut leaves = Vec::with_capacity_in(LEAVES, Global);
            leaves.extend((0..LEAVES).map(|i| Leaf::new(&inputs, fact("leaf", i as u64), i as u32)));
            let graph = Graph::new(&inputs, fact("graph", 0), leaves, nodes);
            other(&mut builder, 0);
            let db = Db(builder.build());
            let expected = node(&db, graph, top);
            Deep { db, graph, top, tick: 0, expected }
        }

        pub(crate) fn run(&mut self) -> u64 {
            self.tick ^= 1;
            let tick = self.tick;
            self.db = next(&self.db, OTHER, |builder| _ = other(builder, tick));
            node(&self.db, self.graph, self.top)
        }
    }

    #[blaze_memo::input]
    pub(crate) struct Seed {
        #[returns(copy)]
        cap: u32,
    }

    fn step(db: &Db, seed: Seed, next: u32) -> u32 {
        (next + 1).min(seed.cap(db))
    }

    #[blaze_memo::tracked(returns(copy), cycle_initial = |_db, _seed| 0)]
    pub(crate) fn c0(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c1(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c1(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c2(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c2(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c3(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c3(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c4(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c4(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c5(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c5(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c6(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c6(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c7(db, seed))
    }

    #[blaze_memo::tracked(returns(copy))]
    fn c7(db: &Db, seed: Seed) -> u32 {
        step(db, seed, c0(db, seed))
    }

    const _: () = assert!(CYCLE == 8);

    fn seed(builder: &mut Builder, cap: u32) -> Seed {
        let inputs = arena(builder, INPUTS);
        Seed::new(&inputs, fact("seed", u64::from(cap)), cap)
    }

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
            let mut made = None;
            let db = first(|builder| made = Some(seed(builder, 3)));
            let seed = made.expect("made in the first snapshot");
            assert_eq!(c0(&db, seed), 3);
            Cycle { db, seed, cap: 3 }
        }

        pub(crate) fn run(&mut self) -> u32 {
            self.cap ^= 7;
            let cap = self.cap;
            let mut made = None;
            self.db = next(&self.db, INPUTS, |builder| made = Some(seed(builder, cap)));
            self.seed = made.expect("made in the next snapshot");
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
        benchmark_fn("db/next/unrelated", |b| {
            let mut next = engine::Next::new();
            b.iter(move || {
                let sum = next.run();
                assert_eq!(sum, next.expected);
                sum
            })
        }),
        benchmark_fn("db/next/deep_unrelated", |b| {
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
