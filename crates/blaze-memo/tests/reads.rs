//! Read lists kept lazily (2026-11-14): an ask, hit or run, appends one row to its asker's
//! list, and an entry's flat list (every input and boundary row its value depends on) is made
//! when something asks for it. Three properties, each with the planted bug its oracle catches:
//! (a) over random DAGs of queries with boundaries, hits and re-asks, every entry's flat list
//!     is the one the eager lists give (the reference model below: what blaze-memo kept before);
//! (b) an ask costs its asker one row, whatever the asked entry read;
//! (c) the members of a fixpoint flatten to one set, their component's.

#![feature(allocator_api)]

mod common;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use blaze_ids::ContentId;
use blaze_mem::Tag;
use blaze_memo::{
    Alone, Config, Database, Fact, Gid, Memo, Plant, Read, Reader, Reads, record, record_readers, take_counts,
    testing::flat_reads_behind,
};
use blaze_sim::{Broke, Tape};

use common::{CRATES, Db, LEAVES, World, crate_fact, facts_of, leaf_fact, reach};

// The graph world: one query kind run as an ordinary query (`inner`) and one marked a fact
// boundary (`wall`), both walking a DAG the case draws.

/// A node of the drawn DAG: the facts it reads and the nodes it asks (each `(callee, through
/// the boundary kind)`); a callee's index is above its asker's.
struct Node {
    facts: Vec<u32>,
    calls: Vec<(u32, bool)>,
}

struct Graph(Vec<Node>);

trait Graphed: Database {
    fn graph(&self) -> &Graph;
}

struct GraphDb {
    memo: Memo,
    graph: Graph,
}

impl Database for GraphDb {
    fn memo(&self) -> &Memo {
        &self.memo
    }
}

impl Graphed for GraphDb {
    fn graph(&self) -> &Graph {
        &self.graph
    }
}

fn fact(n: u32) -> Fact {
    Fact {
        id: ContentId::of("blaze-memo reads test fact", &n.to_le_bytes()),
        digest: ContentId::of("blaze-memo reads test digest", &n.to_le_bytes()),
    }
}

fn walk(db: &dyn Graphed, id: u32) -> u64 {
    let node = &db.graph().0[id as usize];
    for n in &node.facts {
        blaze_memo::read(db.memo(), fact(*n));
    }
    let mut sum = u64::from(id);
    for &(callee, boundary) in &node.calls {
        let value = if boundary { wall(db, callee) } else { inner(db, callee) };
        sum = sum.wrapping_add(value);
    }
    sum
}

#[blaze_memo::tracked(returns(copy))]
fn inner(db: &dyn Graphed, id: u32) -> u64 {
    walk(db, id)
}

#[blaze_memo::tracked(returns(copy))]
fn wall(db: &dyn Graphed, id: u32) -> u64 {
    walk(db, id)
}

fn graph_memo(plant: Plant) -> Memo {
    let mut builder = Memo::builder(Config {
        arenas: 1,
        reads: Reads::Kept,
        flight: Arc::new(Alone::new()),
        cap_bytes: u64::MAX,
        plant,
    });
    builder.mark_boundary(wall::KIND_NAME);
    builder.arena(0, Tag::Query);
    builder.build()
}

/// A row of a list, as the model names it: a fact, or the boundary node a `Read::Query` names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Row {
    Fact(Fact),
    Wall(u32),
}

/// `rows` as the model's rows, each once.
fn rows_of(memo: &Memo, rows: &[Read]) -> Result<BTreeSet<Row>, String> {
    let mut out = BTreeSet::new();
    for row in rows {
        let it = match *row {
            Read::Input { fact, .. } => Row::Fact(fact),
            Read::Query { .. } => match wall::key_of(memo, *row) {
                Some((id,)) => Row::Wall(id),
                None => return Err(format!("a boundary row {row:?} names no `wall` entry")),
            },
        };
        if !out.insert(it) {
            return Err(format!("the row {it:?} is listed twice: {rows:?}"));
        }
    }
    Ok(out)
}

/// The reference model: the eager lists blaze-memo kept before 2026-11-14. An entry's list is
/// what its run read, one row for each boundary it asked, and the whole list of every other
/// query it asked, merged.
fn eager(graph: &Graph, id: u32, made: &mut BTreeMap<u32, BTreeSet<Row>>) -> BTreeSet<Row> {
    if let Some(it) = made.get(&id) {
        return it.clone();
    }
    let node = &graph.0[id as usize];
    let mut out: BTreeSet<Row> = node.facts.iter().map(|n| Row::Fact(fact(*n))).collect();
    for &(callee, boundary) in &node.calls {
        if boundary {
            out.insert(Row::Wall(callee));
        } else {
            out.extend(eager(graph, callee, made));
        }
    }
    made.insert(id, out.clone());
    out
}

fn value(graph: &Graph, id: u32) -> u64 {
    graph.0[id as usize]
        .calls
        .iter()
        .fold(u64::from(id), |sum, (callee, _)| sum.wrapping_add(value(graph, *callee)))
}

fn draw_graph(tape: &Tape) -> Graph {
    let n = tape.int_in(1u32..=10, "nodes");
    let nodes = (0..n)
        .map(|i| Node {
            facts: tape.vec(0..=3, "facts", |t| t.int_in(0u32..=7, "fact")),
            calls: if i + 1 < n {
                tape.vec(0..=3, "calls", |t| (t.int_in(i + 1..=n - 1, "callee"), t.coin("through the boundary")))
            } else {
                Vec::new()
            },
        })
        .collect();
    Graph(nodes)
}

/// How many entries [`with_fan`] puts under node 0.
const FAN: u32 = 4;

/// `graph` with a fan under node 0: node 0 also asks [`FAN`] new entries (ordinary queries),
/// each reading a fact of its own and asking one new shared entry, which reads one too. The
/// shape a flat list's walk reaches an entry twice in, whatever order the callee lists are in
/// (by address): the walk from node 0 meets the shared entry again at the second fan entry it
/// visits, with the others still to visit. No node of `graph` asks into the fan (a callee's
/// index is above its asker's, and the fan's are above every node of `graph`).
fn with_fan(mut graph: Graph) -> Graph {
    let n = graph.0.len() as u32;
    let shared = n + FAN;
    graph.0[0].calls.extend((n..shared).map(|fan| (fan, false)));
    graph.0.extend((n..shared).map(|fan| Node { facts: vec![OWN_FACTS + fan], calls: vec![(shared, false)] }));
    graph.0.push(Node { facts: vec![OWN_FACTS + shared], calls: Vec::new() });
    graph
}

/// Where the facts a fan node reads alone start: above every fact [`draw_graph`] draws.
const OWN_FACTS: u32 = 1000;

// (a) Flat lists are the eager lists.

/// Draws a DAG (with a fan under node 0 where `fan`) and some asks (so the checks below meet
/// hits, runs and re-asks), then checks every node through both kinds: an ordinary query's
/// recorded list, and a boundary's own flat list behind the one row its asker lists.
fn flat_is_eager(tape: &Tape, plant: Plant, fan: bool) -> Result<(), String> {
    let graph = match fan {
        true => with_fan(draw_graph(tape)),
        false => draw_graph(tape),
    };
    let n = graph.0.len() as u32;
    let asks = tape.vec(0..=6, "asks first", |t| (t.int_in(0..=n - 1, "asked"), t.coin("the boundary kind")));
    let db = GraphDb { memo: graph_memo(plant), graph };
    for &(id, boundary) in &asks {
        if boundary {
            wall(&db, id);
        } else {
            inner(&db, id);
        }
    }
    let mut made = BTreeMap::new();
    for id in 0..n {
        let want = eager(&db.graph, id, &mut made);
        let (got, rows) = record(&db.memo, || inner(&db, id)).map_err(|it| it.to_string())?;
        if got != value(&db.graph, id) {
            return Err(format!("inner({id}) answered {got}, the model {}", value(&db.graph, id)));
        }
        let listed = rows_of(&db.memo, &rows)?;
        if listed != want {
            return Err(format!("inner({id}): the flat list {listed:?} is not the eager lists' {want:?}"));
        }
        let (_, rows) = record(&db.memo, || wall(&db, id)).map_err(|it| it.to_string())?;
        let [row] = rows.as_slice() else {
            return Err(format!("wall({id}) asked under a recorder listed {rows:?}, not one boundary row"));
        };
        let behind = flat_reads_behind(&db.memo, *row, std::alloc::Global)
            .ok_or_else(|| format!("wall({id})'s row {row:?} names no entry"))?;
        let listed = rows_of(&db.memo, &behind)?;
        if listed != want {
            return Err(format!("wall({id}): the flat list {listed:?} is not the eager lists' {want:?}"));
        }
    }
    Ok(())
}

/// The refinement: over random DAGs of queries with boundaries, hits and re-asks, the flat list
/// of every entry (an ordinary query's through `record`, a boundary's behind its row) equals the
/// list the eager algorithm gives, the reference model `eager`.
#[test]
fn flat_lists_are_the_eager_lists() {
    blaze_sim::check(blaze_sim::Config::cases(300), |tape| flat_is_eager(tape, Plant::None, false));
    blaze_sim::check(blaze_sim::Config::cases(100), |tape| flat_is_eager(tape, Plant::None, true));
}

/// The control: a walk that stops at an entry it reaches a second time skips what was still to
/// visit, and the model sees rows missing. Over DAGs with a fan under node 0 ([`with_fan`]):
/// the walk from node 0 stops at the second fan entry it visits, with at least two of the
/// [`FAN`] still to visit, each holding a fact no other entry reads, so `inner(0)`'s flat list
/// misses them in every case. The drawn DAGs alone (calls and facts each drawn sparse: a list
/// stops at each draw with one chance in two) hold an entry reached twice with another,
/// reading a fact of its own, still to visit so rarely that 300 cases met none (round 26: "the
/// oracle missed FlattenStopsAtRevisit in 300 cases").
#[test]
fn the_oracle_catches_a_flatten_that_skips_a_callee_reached_twice() {
    blaze_sim::control(
        blaze_sim::Config::cases(100),
        Plant::None,
        Plant::FlattenStopsAtRevisit,
        "is not the eager lists'",
        |tape, plant| flat_is_eager(tape, plant, true).map_err(Broke::Oracle),
    );
}

// (b) An ask costs one row.

/// Node 0 asks nodes `1..=asks`, each reading `reads` facts of its own; each is made first, so
/// node 0's asks are hits. Recording node 0 must append one row per ask plus node 0's own row
/// in the recorder, whatever the asked entries read, and still list every fact they read.
fn one_row_an_ask(tape: &Tape, plant: Plant) -> Result<(), String> {
    let asks = tape.int_in(1u32..=12, "asks");
    let reads = tape.int_in(1u32..=12, "rows each");
    let mut nodes = vec![Node { facts: Vec::new(), calls: (1..=asks).map(|i| (i, false)).collect() }];
    for i in 1..=asks {
        nodes.push(Node { facts: ((i - 1) * reads..i * reads).collect(), calls: Vec::new() });
    }
    let db = GraphDb { memo: graph_memo(plant), graph: Graph(nodes) };
    for i in 1..=asks {
        inner(&db, i);
    }
    take_counts();
    let (_, rows) = record(&db.memo, || inner(&db, 0)).map_err(|it| it.to_string())?;
    let appended = take_counts().rows_replayed;
    let all = u64::from(asks * reads);
    if rows.len() as u64 != all {
        return Err(format!("the flat list holds {} rows, not the {all} the asked entries read", rows.len()));
    }
    let want = u64::from(asks) + 1;
    if appended != want {
        return Err(format!(
            "{asks} asks of entries of {reads} rows each appended {appended} rows, not one an ask ({want})"
        ));
    }
    Ok(())
}

/// The invariant: a run that asks N entries whose flat lists hold M rows each appends N rows to
/// its list (and its own ask one to its asker's), never N×M; the recorded flat list still holds
/// all N×M. Counted by `Counts::rows_replayed`.
#[test]
fn an_ask_appends_one_row_whatever_the_entry_read() {
    blaze_sim::check(blaze_sim::Config::cases(64), |tape| one_row_an_ask(tape, Plant::None));
}

/// The control: the eager lists (an ask copies the asked entry's whole flat list) append N×M.
#[test]
fn the_oracle_catches_an_ask_that_copies_the_flat_list() {
    blaze_sim::control(
        blaze_sim::Config::cases(64),
        Plant::None,
        Plant::AskCopiesFlatList,
        "not one an ask",
        |tape, plant| one_row_an_ask(tape, plant).map_err(Broke::Oracle),
    );
}

// (c) A fixpoint's members flatten to one set.

struct CycleDb {
    memo: Memo,
}

impl Database for CycleDb {
    fn memo(&self) -> &Memo {
        &self.memo
    }
}

impl Db for CycleDb {
    fn pause(&self) {}

    fn ran(&self, _kind: u8, _id: Gid, _extra: u32) {}
}

/// `reach(c, 0)` heads a cycle (`cycle_initial`) through every node its walk reaches; it is
/// recorded where it runs, then each member is recorded (a hit on its published entry).
fn members_flatten_alike(plant: Plant) -> Result<(), String> {
    let values: Vec<Vec<u64>> =
        (0..=CRATES).map(|c| (0..LEAVES).map(|l| u64::from(c) * 10 + l as u64).collect()).collect();
    let mut world = World::new(values.clone());
    let db = CycleDb {
        memo: world.first(Config {
            arenas: CRATES + 1,
            reads: Reads::Kept,
            flight: Arc::new(Alone::new()),
            cap_bytes: u64::MAX,
            plant,
        }),
    };
    let c = 1_u32;
    let krate = world.krate(c);
    // The component: every node the walk from 0 reaches, as `common::Model::reach` walks it.
    let mut component = BTreeSet::new();
    let mut todo = vec![0_u32];
    while let Some(at) = todo.pop() {
        if component.insert(at) {
            todo.push((at + 1) % LEAVES as u32);
            todo.push((at * 2) % LEAVES as u32);
        }
    }
    let mut want: Vec<Fact> = component.iter().map(|l| leaf_fact(c, *l as usize, values[c as usize][*l as usize])).collect();
    want.push(crate_fact(c));
    want.sort_unstable();
    let (_, rows) = record(&db.memo, || reach(&db, krate, 0)).map_err(|it| it.to_string())?;
    let head = facts_of(&rows);
    if head != want {
        return Err(format!("the head reach(0) lists {head:?}, not its component's facts {want:?}"));
    }
    for node in component.iter().copied().filter(|it| *it != 0) {
        let (_, rows) = record(&db.memo, || reach(&db, krate, node)).map_err(|it| it.to_string())?;
        let got = facts_of(&rows);
        if got != head {
            return Err(format!("member reach({node}) flattens to {got:?}, not its cycle's one set {head:?}"));
        }
    }
    Ok(())
}

/// The invariant: every member of a settled fixpoint flattens to the same set as its head, the
/// facts the whole component read in any round (a member's value is a function of its head's
/// provisional value, so it depends on all of them).
#[test]
fn a_fixpoints_members_flatten_to_one_set() {
    members_flatten_alike(Plant::None).unwrap_or_else(|why| panic!("{why}"));
}

// (d) Each row names its reader.

/// A query that reads a fact of its own ([`OUTER_FACT`]) and asks `inner(id)`: the outer of a
/// planted nested read, whose rows below must name `inner`, not this.
#[blaze_memo::tracked(returns(copy))]
fn outer(db: &dyn Graphed, id: u32) -> u64 {
    blaze_memo::read(db.memo(), fact(OUTER_FACT));
    inner(db, id)
}

/// The fact only [`outer`] reads: above every fact the graph draws.
const OUTER_FACT: u32 = 2000;
/// The fact only the recorded work reads, outside any query.
const WORK_FACT: u32 = 3000;

/// Over drawn DAGs after drawn asks (hits and runs), one run of `outer(id)` under both
/// recorders at once (`record_readers` inside `record`): the rows with readers dropped are
/// `record`'s; the work's own read names the work, `outer`'s own read names `outer`, and every
/// other input row names `inner`, once for each node the model says reads that fact directly
/// (reached from `id` through ordinary asks: a boundary's rows stay behind its one row).
fn readers_are_the_innermost(tape: &Tape) -> Result<(), String> {
    let graph = draw_graph(tape);
    let n = graph.0.len() as u32;
    let asks = tape.vec(0..=6, "asks first", |t| (t.int_in(0..=n - 1, "asked"), t.coin("the boundary kind")));
    let db = GraphDb { memo: graph_memo(Plant::None), graph };
    for &(id, boundary) in &asks {
        if boundary {
            wall(&db, id);
        } else {
            inner(&db, id);
        }
    }
    for id in 0..n {
        let (named, rows) = record(&db.memo, || {
            record_readers(&db.memo, || {
                blaze_memo::read(&db.memo, fact(WORK_FACT));
                outer(&db, id)
            })
        })
        .map_err(|it| it.to_string())?;
        let (got, readers) = named.map_err(|it| it.to_string())?;
        if got != value(&db.graph, id) {
            return Err(format!("outer({id}) answered {got}, the model {}", value(&db.graph, id)));
        }
        let mut bare: Vec<Read> = readers.rows.iter().map(|(read, _)| *read).collect();
        bare.dedup();
        if bare != rows {
            return Err(format!("outer({id}): the readers' rows {bare:?} are not `record`'s {rows:?}"));
        }
        // The model: for each fact, the nodes reached from `id` through ordinary asks that read it.
        let mut reached = BTreeSet::new();
        let mut todo = vec![id];
        while let Some(at) = todo.pop() {
            if reached.insert(at) {
                todo.extend(db.graph.0[at as usize].calls.iter().filter(|(_, wall)| !wall).map(|(callee, _)| *callee));
            }
        }
        let mut want: BTreeMap<Fact, usize> = BTreeMap::new();
        for at in &reached {
            for f in BTreeSet::from_iter(db.graph.0[*at as usize].facts.iter().copied()) {
                *want.entry(fact(f)).or_default() += 1;
            }
        }
        let mut got_by_inner: BTreeMap<Fact, usize> = BTreeMap::new();
        for (read, reader) in &readers.rows {
            let kind = match reader {
                Reader::Work => None,
                Reader::Query { kind, .. } => Some(kind.name()),
            };
            // The path of asks, outermost first: none for the work's own read, `outer` for its
            // own, and `outer` then `inner` (then only `inner`, down the drawn graph) below.
            let mut chain: Vec<&str> = readers.chain(*reader).map(|it| it.name()).collect();
            chain.reverse();
            let shaped = match kind {
                None => chain.is_empty(),
                Some(name) if name == outer::KIND_NAME => chain == [outer::KIND_NAME],
                Some(_) => {
                    chain.len() >= 2
                        && chain[0] == outer::KIND_NAME
                        && chain[1..].iter().all(|it| *it == inner::KIND_NAME)
                        && chain.last() == kind.as_ref()
                }
            };
            if !shaped {
                return Err(format!("outer({id}): {read:?} read by {reader} has the path {chain:?}"));
            }
            match *read {
                Read::Input { fact: f, .. } if f == fact(WORK_FACT) => {
                    if kind.is_some() {
                        return Err(format!("outer({id}): the work's own read names {reader}"));
                    }
                }
                Read::Input { fact: f, .. } if f == fact(OUTER_FACT) => {
                    if kind != Some(outer::KIND_NAME) {
                        return Err(format!("outer({id}): outer's own read names {reader}"));
                    }
                }
                Read::Input { fact: f, .. } => {
                    if kind != Some(inner::KIND_NAME) {
                        return Err(format!("outer({id}): {f:?}, read inside inner, names {reader}, not inner"));
                    }
                    *got_by_inner.entry(f).or_default() += 1;
                }
                Read::Query { .. } => {
                    if kind != Some(inner::KIND_NAME) {
                        return Err(format!("outer({id}): a boundary ask {read:?} names {reader}, not inner"));
                    }
                }
            }
        }
        if got_by_inner != want {
            return Err(format!("outer({id}): readers per fact {got_by_inner:?}, the model {want:?}"));
        }
    }
    Ok(())
}

/// The invariant: `record_readers` gives `record`'s rows for the same run, each named by the
/// innermost query whose run read it, never by an asker above it, with its path of asks from
/// the work (work, `outer`, `inner`, ...).
#[test]
fn each_row_names_its_innermost_reader() {
    blaze_sim::check(blaze_sim::Config::cases(200), readers_are_the_innermost);
}

/// The control: members published with no lists flatten to nothing.
#[test]
fn the_oracle_catches_a_member_without_its_cycles_list() {
    let why = members_flatten_alike(Plant::MemberListsNothing).expect_err("a member without its cycle's list went unnoticed");
    assert!(why.contains("not its cycle's one set"), "{why}");
}
