//! The crate's tests. The oracle is a truth table over at most 10 variables: every operation
//! equals the table's, equal tables are equal ids, export then import gives equal bytes and an
//! equal id, `implies` is inclusion, `sat_count` the popcount, the cover covers exactly. Each
//! planted bug ([`CondPlant`]) is caught by its control, by the invariant it names, and the
//! arena's node cap is drawn down to its smallest legal value and reached.

use core::fmt;
use std::{format, string::String, vec::Vec as StdVec};

use blaze_own::{AVec, Alloc, planted::Planted};
use blaze_sim::{Broken, Config, NoPlant, Point, Tape, check, controls};

use crate::cfg::{self, Op};
use crate::isop::Cover;
use crate::portable::{export, import};
use crate::{
    Atom, Bdd, Cond, CondError, CondPlant, ImportError, Limits, PlantedBdd, Space, SpaceBuilder,
    Var,
};

mod space;

const MOST_VARS: usize = 10;

/// A truth table: bit `j` is the function at the assignment whose bit `i` is variable `i`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Tt {
    n: usize,
    w: [u64; 1 << (MOST_VARS - 6)],
}

impl Tt {
    fn rows(n: usize) -> usize {
        1 << n
    }

    fn from_fn(n: usize, f: impl Fn(usize) -> bool) -> Tt {
        let mut t = Tt { n, w: [0; 1 << (MOST_VARS - 6)] };
        for j in 0..Tt::rows(n) {
            if f(j) {
                t.w[j / 64] |= 1 << (j % 64);
            }
        }
        t
    }

    fn get(&self, j: usize) -> bool {
        self.w[j / 64] >> (j % 64) & 1 == 1
    }

    fn var(n: usize, i: usize) -> Tt {
        Tt::from_fn(n, |j| j >> i & 1 == 1)
    }

    fn map2(self, o: Tt, f: impl Fn(bool, bool) -> bool) -> Tt {
        Tt::from_fn(self.n, |j| f(self.get(j), o.get(j)))
    }

    fn not(self) -> Tt {
        Tt::from_fn(self.n, |j| !self.get(j))
    }

    fn restrict(self, i: usize, v: bool) -> Tt {
        Tt::from_fn(self.n, |j| self.get(if v { j | 1 << i } else { j & !(1 << i) }))
    }

    fn depends(self, i: usize) -> bool {
        self.restrict(i, false) != self.restrict(i, true)
    }

    fn count(self) -> u128 {
        self.w.iter().map(|w| u128::from(w.count_ones())).sum()
    }

    fn within(self, o: Tt) -> bool {
        (0..Tt::rows(self.n)).all(|j| !self.get(j) || o.get(j))
    }
}

/// The invariants the truth-table oracle checks; `op` names the operation that made the
/// condition.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CondBroken {
    /// A condition is not its operation's truth table.
    TableDiffers { op: &'static str },
    /// Two conditions of equal tables have different ids (by any path, the Shannon rebuild
    /// among them).
    DifferentIds { op: &'static str, one: Cond, other: Cond },
    /// `implies` is not inclusion of the tables.
    ImpliesNotInclusion { op: &'static str },
    /// `disjoint` is not an empty intersection of the tables.
    DisjointNotEmpty { op: &'static str },
    /// `sat_count` is not the table's popcount within `valid`.
    SatCountNotPopcount { op: &'static str },
    /// `any_sat`'s answer or witness does not satisfy the table within `valid`.
    NoWitness { op: &'static str },
    /// `support` is not the variables the table depends on.
    Support { op: &'static str, listed: StdVec<u16>, depends: StdVec<u16> },
    /// The portable form was refused on import.
    PortableRefused { op: &'static str, error: ImportError },
    /// The portable form came back as another id in its own arena.
    PortableOtherId { op: &'static str },
    /// The portable form written from a fresh arena is other bytes, or another function.
    PortableOtherBytes { op: &'static str },
    /// The cover is not exactly its condition.
    CoverInexact { op: &'static str },
    /// The cover holds a cube without which it is still exact.
    CoverRedundant { op: &'static str },
    /// A drawn cfg predicate did not parse.
    CfgUnparsed { text: String, error: String },
    /// A cfg predicate lowered to another function than its value under every assignment.
    CfgLoweredWrong { text: String },
    /// An atom outside the space was not reported absent, or one inside it was.
    CfgAbsent { text: String, atom: String },
    /// An arena holds more nodes than its cap, or refused below it.
    NodeCap { nodes: usize, cap: u32 },
}

impl fmt::Display for CondBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CondBroken::TableDiffers { op } => write!(f, "{op} differs from its table"),
            CondBroken::DifferentIds { op, one, other } => {
                write!(f, "equal tables, different ids: {op} {one:?} and {other:?}")
            }
            CondBroken::ImpliesNotInclusion { op } => write!(f, "implies is not inclusion for {op}"),
            CondBroken::DisjointNotEmpty { op } => {
                write!(f, "disjoint is not an empty intersection for {op}")
            }
            CondBroken::SatCountNotPopcount { op } => write!(f, "sat_count is not the popcount for {op}"),
            CondBroken::NoWitness { op } => write!(f, "any_sat gave no satisfying witness for {op}"),
            CondBroken::Support { op, listed, depends } => {
                write!(f, "support {listed:?} is not the variables {op} depends on {depends:?}")
            }
            CondBroken::PortableRefused { op, error } => write!(f, "portable: {op}: {error}"),
            CondBroken::PortableOtherId { op } => write!(f, "portable: {op} came back as another id"),
            CondBroken::PortableOtherBytes { op } => {
                write!(f, "portable: {op} wrote other bytes from another arena")
            }
            CondBroken::CoverInexact { op } => write!(f, "the cover of {op} is not exactly it"),
            CondBroken::CoverRedundant { op } => write!(f, "the cover of {op} has a redundant cube"),
            CondBroken::CfgUnparsed { text, error } => write!(f, "`{text}` did not parse: {error}"),
            CondBroken::CfgLoweredWrong { text } => write!(f, "`{text}` lowered to another function"),
            CondBroken::CfgAbsent { text, atom } => {
                write!(f, "`{text}`: the unset atom {atom} was not reported absent")
            }
            CondBroken::NodeCap { nodes, cap } => {
                write!(f, "the arena holds {nodes} nodes against its cap of {cap}")
            }
        }
    }
}

fn harness(e: impl fmt::Display) -> Broken<CondBroken> {
    Broken::Harness(format!("{e}"))
}

/// `Ok` when `held`, else the invariant `broken`.
fn holds(held: bool, broken: impl FnOnce() -> CondBroken) -> Result<(), Broken<CondBroken>> {
    match held {
        true => Ok(()),
        false => Err(Broken::Oracle(broken())),
    }
}

/// A space of `n` switches `v00`, `v01`, ...: their names sort in index order, so variable `i`
/// is `v{i}`.
fn switches(n: usize, alloc: Alloc<'static>) -> Space<'static> {
    let mut b = SpaceBuilder::new_in(alloc);
    let names = ["v00", "v01", "v02", "v03", "v04", "v05", "v06", "v07", "v08", "v09"];
    for name in &names[..n] {
        b.switch(Atom::flag(name)).unwrap_or_else(|e| panic!("{e}"));
    }
    b.build().unwrap_or_else(|e| panic!("{e}"))
}

fn table<P: Planted<CondPlant>>(bdd: &PlantedBdd<'_, P>, c: Cond, n: usize) -> Tt {
    Tt::from_fn(n, |j| bdd.eval(c, |v| j >> v.index() & 1 == 1))
}

/// The table's function built a second way: Shannon expansion over the rows, through `ite`.
fn from_table<P: Planted<CondPlant>>(bdd: &mut PlantedBdd<'_, P>, t: Tt) -> Result<Cond, CondError> {
    let mut level: StdVec<Cond> = (0..Tt::rows(t.n)).map(|j| if t.get(j) { Cond::TRUE } else { Cond::FALSE }).collect();
    for i in (0..t.n).rev() {
        let x = bdd.var(Var::new(i as u16))?;
        let half = 1 << i;
        let mut next = StdVec::with_capacity(half);
        for j in 0..half {
            next.push(bdd.ite(x, level[j | half], level[j])?);
        }
        level = next;
    }
    Ok(level[0])
}

/// One case: random operations on a pool of conditions and their tables, then every check;
/// every arena it makes takes `plant` ([`NoPlant`] for the clean run).
fn ops_match_tables<P: Planted<CondPlant>>(tape: &Tape, plant: P) -> Result<(), Broken<CondBroken>> {
    let alloc = blaze_own::testing::heap().alloc();
    let n = 1 + tape.index(MOST_VARS, "variables");
    let space = switches(n, alloc);
    let cache_bits = 1 + tape.index(10, "cache bits") as u8;
    let mut bdd = PlantedBdd::new_planted(n, Limits { nodes: 1 << 20, cache_bits }, alloc, plant).map_err(harness)?;
    let mut pool: StdVec<(Cond, Tt, &'static str)> = StdVec::from([
        (Cond::TRUE, Tt::from_fn(n, |_| true), "true"),
        (Cond::FALSE, Tt::from_fn(n, |_| false), "false"),
    ]);
    for i in 0..n {
        let c = bdd.var(Var::new(i as u16)).map_err(harness)?;
        pool.push((c, Tt::var(n, i), "var"));
    }
    for _ in 0..tape.below(40, "operations") {
        let pick = |label| tape.index(pool.len(), label);
        let (a, b, c) = (pool[pick("a")], pool[pick("b")], pool[pick("c")]);
        let got = match tape.below(7, "operation") {
            0 => (bdd.and(a.0, b.0).map_err(harness)?, a.1.map2(b.1, |x, y| x && y), "and"),
            1 => (bdd.or(a.0, b.0).map_err(harness)?, a.1.map2(b.1, |x, y| x || y), "or"),
            2 => (a.0.not(), a.1.not(), "not"),
            3 => {
                let t = Tt::from_fn(n, |j| if a.1.get(j) { b.1.get(j) } else { c.1.get(j) });
                (bdd.ite(a.0, b.0, c.0).map_err(harness)?, t, "ite")
            }
            4 => (bdd.xor(a.0, b.0).map_err(harness)?, a.1.map2(b.1, |x, y| x != y), "xor"),
            5 => {
                let i = tape.index(n, "restricted");
                let v = tape.coin("value");
                (bdd.restrict(a.0, Var::new(i as u16), v).map_err(harness)?, a.1.restrict(i, v), "restrict")
            }
            _ => {
                let vars: StdVec<usize> = (0..n).filter(|_| tape.one_in(3, "quantified")).collect();
                let qs: StdVec<Var> = vars.iter().map(|&i| Var::new(i as u16)).collect();
                let t = vars.iter().fold(a.1, |t, &i| t.restrict(i, false).map2(t.restrict(i, true), |x, y| x || y));
                (bdd.exists(a.0, &qs).map_err(harness)?, t, "exists")
            }
        };
        pool.push(got);
    }
    // Every operation equals its table.
    for &(c, t, op) in &pool {
        holds(table(&bdd, c, n) == t, || CondBroken::TableDiffers { op })?;
    }
    // Equal tables are equal ids, built by any path.
    for (i, &(ci, ti, op)) in pool.iter().enumerate() {
        for &(cj, tj, _) in &pool[..i] {
            holds((ti == tj) == (ci == cj), || CondBroken::DifferentIds { op, one: ci, other: cj })?;
        }
    }
    let rebuilt_from = if n <= 6 { 0 } else { pool.len().saturating_sub(4) };
    for &(c, t, op) in &pool[rebuilt_from..] {
        let again = from_table(&mut bdd, t).map_err(harness)?;
        holds(again == c, || CondBroken::DifferentIds { op, one: c, other: again })?;
    }
    let valid = pool[tape.index(pool.len(), "valid")];
    let mut support: AVec<'_, Var> = AVec::new_in(alloc);
    let mut witness: AVec<'_, (Var, bool)> = AVec::new_in(alloc);
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    let mut again: AVec<'_, u8> = AVec::new_in(alloc);
    let mut cover = Cover::new_in(alloc);
    for &(c, t, op) in &pool {
        let other = pool[tape.index(pool.len(), "other")];
        let implies = bdd.implies(c, other.0).map_err(harness)?;
        holds(implies == t.within(other.1), || CondBroken::ImpliesNotInclusion { op })?;
        let disjoint = bdd.disjoint(c, other.0).map_err(harness)?;
        holds(disjoint == (t.map2(other.1, |x, y| x && y).count() == 0), || CondBroken::DisjointNotEmpty { op })?;
        let within = t.map2(valid.1, |x, y| x && y);
        let count = bdd.sat_count(c, valid.0).map_err(harness)?;
        holds(count == within.count(), || CondBroken::SatCountNotPopcount { op })?;
        let found = bdd.any_sat(c, valid.0, &mut witness).map_err(harness)?;
        let row = witness.iter().filter(|(_, on)| *on).fold(0usize, |j, (v, _)| j | 1 << v.index());
        holds(found == (within.count() > 0) && (!found || within.get(row)), || CondBroken::NoWitness { op })?;
        bdd.support(c, &mut support).map_err(harness)?;
        let depends: StdVec<u16> = (0..n).filter(|&i| t.depends(i)).map(|i| i as u16).collect();
        let listed: StdVec<u16> = support.iter().map(|v| v.index()).collect();
        if listed != depends {
            return Err(Broken::Oracle(CondBroken::Support { op, listed, depends }));
        }
        // Portable: into this arena gives the id; into a fresh one, equal bytes back.
        export(&mut bdd, &space, c, &mut bytes).map_err(harness)?;
        let same = import(&mut bdd, &space, &bytes)
            .map_err(|error| Broken::Oracle(CondBroken::PortableRefused { op, error }))?;
        holds(same == c, || CondBroken::PortableOtherId { op })?;
        let mut fresh =
            PlantedBdd::new_planted(n, Limits { nodes: 1 << 16, cache_bits: 8 }, alloc, plant).map_err(harness)?;
        let there = import(&mut fresh, &space, &bytes)
            .map_err(|error| Broken::Oracle(CondBroken::PortableRefused { op, error }))?;
        export(&mut fresh, &space, there, &mut again).map_err(harness)?;
        holds(again.as_slice() == bytes.as_slice() && table(&fresh, there, n) == t, || {
            CondBroken::PortableOtherBytes { op }
        })?;
        // The cover: exactly the function, and no cube can go.
        let r = cover.compute(&mut bdd, c, c, 1 << 20).map_err(harness)?;
        let cubes = cover_tables(&cover, n);
        let union = cubes.iter().fold(Tt::from_fn(n, |_| false), |u, t| u.map2(*t, |x, y| x || y));
        holds(r == c && union == t, || CondBroken::CoverInexact { op })?;
        for skip in 0..cubes.len() {
            let rest = cubes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .fold(Tt::from_fn(n, |_| false), |u, (_, t)| u.map2(*t, |x, y| x || y));
            holds(rest != t, || CondBroken::CoverRedundant { op })?;
        }
    }
    Ok(())
}

fn cover_tables(cover: &Cover<'_>, n: usize) -> StdVec<Tt> {
    (0..cover.len())
        .map(|at| {
            let lits: StdVec<(Var, bool)> = cover.cube(at).collect();
            Tt::from_fn(n, |j| lits.iter().all(|(v, pos)| (j >> v.index() & 1 == 1) == *pos))
        })
        .collect()
}

blaze_sim::plants! {
    /// The bugs planted in the arena, each with the invariant of the truth-table oracle that
    /// must catch it.
    enum ArenaControl breaks CondBroken {
        /// The arena as it is.
        None,
        /// A function and its negation stop sharing a node: two ids for one table.
        UniqueSkipsComplement => CondBroken::DifferentIds { .. },
        /// An `ite` answered from another `ite`'s cache entry: a condition off its table.
        IteCacheSkipsElse => CondBroken::TableDiffers { .. },
        /// `restrict` takes the other cofactor: the restricted condition off its table.
        RestrictWrongBranch => CondBroken::TableDiffers { op: "restrict" },
    }
}

impl ArenaControl {
    /// The arena's plant this control plants.
    fn planted(self) -> CondPlant {
        match self {
            ArenaControl::None => CondPlant::None,
            ArenaControl::UniqueSkipsComplement => CondPlant::UniqueSkipsComplement,
            ArenaControl::IteCacheSkipsElse => CondPlant::IteCacheSkipsElse,
            ArenaControl::RestrictWrongBranch => CondPlant::RestrictWrongBranch,
        }
    }
}

#[test]
fn operations_equal_their_truth_tables() {
    check(Config::cases(400), |tape| ops_match_tables(tape, NoPlant));
}

#[test]
fn every_arena_plant_is_caught_by_its_invariant() {
    controls(Config::cases(2000), |tape, plant: ArenaControl| ops_match_tables(tape, plant.planted()));
}

blaze_sim::points! {
    /// Where the arena's limits simulation has to get.
    enum ArenaPoint {
        /// An operation was refused at the drawn node cap.
        NodeCap,
    }
}

impl blaze_sim::Limits for Limits {
    const SHIPPED: Limits = Limits::SHIPPED;

    fn draw(tape: &Tape) -> Limits {
        Limits {
            nodes: tape.limit(Limits::SHIPPED.nodes, Limits::SMALLEST.nodes, "most nodes"),
            cache_bits: tape.limit(Limits::SHIPPED.cache_bits, Limits::SMALLEST.cache_bits, "cache bits"),
        }
    }
}

/// One case of the limits simulation: the arena's limits drawn, then up to 40 operations over
/// up to 8 variables; every condition made equals its table, the arena never holds more than
/// its cap, and a refusal is the cap's own, at the cap.
fn under_drawn_limits(tape: &Tape) -> Result<(), Broken<CondBroken>> {
    let alloc = blaze_own::testing::heap().alloc();
    let limits: Limits = tape.limits();
    let n = 1 + tape.index(8, "variables");
    let mut bdd = Bdd::new_in(n, limits, alloc).map_err(harness)?;
    let mut pool: StdVec<(Cond, Tt)> =
        StdVec::from([(Cond::TRUE, Tt::from_fn(n, |_| true)), (Cond::FALSE, Tt::from_fn(n, |_| false))]);
    for _ in 0..tape.below(40, "operations") {
        let a = pool[tape.index(pool.len(), "a")];
        let made = match tape.below(3, "operation") {
            0 => {
                let i = tape.index(n, "variable");
                bdd.var(Var::new(i as u16)).map(|c| (c, Tt::var(n, i)))
            }
            1 => {
                let b = pool[tape.index(pool.len(), "b")];
                bdd.and(a.0, b.0).map(|c| (c, a.1.map2(b.1, |x, y| x && y)))
            }
            _ => {
                let b = pool[tape.index(pool.len(), "b")];
                bdd.xor(a.0, b.0).map(|c| (c, a.1.map2(b.1, |x, y| x != y)))
            }
        };
        let held = bdd.nodes();
        holds(held <= limits.nodes as usize, || CondBroken::NodeCap { nodes: held, cap: limits.nodes })?;
        match made {
            Ok((c, t)) => {
                holds(table(&bdd, c, n) == t, || CondBroken::TableDiffers { op: "an operation under drawn limits" })?;
                pool.push((c, t));
            }
            Err(CondError::NodeCap { cap }) if cap == limits.nodes && held == cap as usize => {
                tape.reach(ArenaPoint::NodeCap);
                return Ok(());
            }
            Err(CondError::NodeCap { cap }) => {
                return Err(Broken::Oracle(CondBroken::NodeCap { nodes: held, cap }));
            }
            Err(other) => return Err(harness(other)),
        }
    }
    Ok(())
}

#[test]
fn the_node_cap_is_drawn_down_to_its_smallest_and_reached() {
    check(Config::cases(400).must_reach_points(<ArenaPoint as Point>::IDS), under_drawn_limits);
}

/// A random predicate as text, and its value under an assignment, by Rust's rules.
fn draw_predicate(tape: &Tape, n: usize, out: &mut String, depth: u32) {
    // Explicit stack: each entry is a predicate still to write, or a closing text.
    enum Todo {
        Pred(u32),
        Text(&'static str),
    }
    let mut todo = StdVec::from([Todo::Pred(depth)]);
    while let Some(next) = todo.pop() {
        match next {
            Todo::Text(t) => out.push_str(t),
            Todo::Pred(d) => match if d == 0 { 0 } else { tape.below(6, "predicate") } {
                0 | 1 => {
                    // `v07` is outside a space of fewer variables: an unset cfg.
                    let i = tape.index(n + 1, "atom");
                    out.push_str(&format!("v{i:02}"));
                }
                2 => {
                    out.push_str("not(");
                    todo.push(Todo::Text(")"));
                    todo.push(Todo::Pred(d - 1));
                }
                3 => out.push_str(if tape.coin("constant") { "true" } else { "false" }),
                f => {
                    out.push_str(if f == 4 { "all(" } else { "any(" });
                    todo.push(Todo::Text(")"));
                    let k = tape.below(4, "operands");
                    for i in 0..k {
                        todo.push(Todo::Pred(d - 1));
                        if i + 1 < k {
                            todo.push(Todo::Text(", "));
                        }
                    }
                }
            },
        }
    }
}

/// Evaluates postfix ops under an assignment of the switches `v00`...
fn eval_ops(ops: &[Op<'_>], n: usize, j: usize) -> bool {
    let mut stack: StdVec<bool> = StdVec::new();
    for op in ops {
        match *op {
            Op::True => stack.push(true),
            Op::False => stack.push(false),
            Op::Atom(a) => {
                let i: usize = a.name[1..].parse().unwrap_or(usize::MAX);
                stack.push(a.value.is_none() && i < n && j >> i & 1 == 1);
            }
            Op::Not => {
                let x = stack.pop().unwrap_or(false);
                stack.push(!x);
            }
            Op::All(k) => {
                let at = stack.len() - k as usize;
                let v = stack[at..].iter().all(|x| *x);
                stack.truncate(at);
                stack.push(v);
            }
            Op::Any(k) => {
                let at = stack.len() - k as usize;
                let v = stack[at..].iter().any(|x| *x);
                stack.truncate(at);
                stack.push(v);
            }
        }
    }
    stack.pop().unwrap_or(false)
}

#[test]
fn a_cfg_predicate_lowers_to_its_value_under_every_assignment() {
    check(Config::cases(400), |tape| -> Result<(), Broken<CondBroken>> {
        let alloc = blaze_own::testing::heap().alloc();
        let n = 1 + tape.index(7, "variables");
        let space = switches(n, alloc);
        let mut bdd = Bdd::new_in(n, Limits::SHIPPED, alloc).map_err(harness)?;
        let mut text = String::new();
        draw_predicate(tape, n, &mut text, 4);
        let expr = cfg::parse(&text, alloc).map_err(|e| {
            Broken::Oracle(CondBroken::CfgUnparsed { text: text.clone(), error: format!("{e}") })
        })?;
        let mut absent = AVec::new_in(alloc);
        let c = cfg::lower(&expr, &space, "pkg", &mut bdd, &mut absent).map_err(harness)?;
        let want = Tt::from_fn(n, |j| eval_ops(&expr.ops, n, j));
        holds(table(&bdd, c, n) == want, || CondBroken::CfgLoweredWrong { text: text.clone() })?;
        let atom = format!("v{n:02}");
        holds(text.contains(&atom) != absent.is_empty(), || CondBroken::CfgAbsent { text: text.clone(), atom })?;
        Ok(())
    });
}

#[test]
fn malformed_portable_rows_are_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    let space = switches(3, alloc);
    let mut bdd = Bdd::new_in(3, Limits::SHIPPED, alloc).unwrap();
    let (a, b) = (bdd.var(Var::new(0)).unwrap(), bdd.var(Var::new(1)).unwrap());
    let f = bdd.xor(a, b).unwrap();
    let mut bytes = AVec::new_in(alloc);
    export(&mut bdd, &space, f, &mut bytes).unwrap();
    // Rows: v01 (one node, the root's children's), then v00 (the root).
    assert_eq!(bytes.len(), 8 + 2 * 16);
    let mut bad = |at: usize, value: u32| {
        let mut b: StdVec<u8> = bytes.as_slice().to_vec();
        b[at..at + 4].copy_from_slice(&value.to_le_bytes());
        import(&mut bdd, &space, &b)
    };
    assert_eq!(bad(8 + 8, 4), Err(ImportError::Bounds { row: 1, refers: 2 }));
    assert_eq!(bad(8 + 12, 1), Err(ImportError::Complemented { row: 1 }));
    assert_eq!(bad(8 + 8, 0), Err(ImportError::Redundant { row: 1 }));
    assert_eq!(bad(4, 3), Err(ImportError::Length { len: 40 }));
    assert_eq!(bad(0, 2), Err(ImportError::NotCanonical { row: 2 }));
    let mut b: StdVec<u8> = bytes.as_slice().to_vec();
    b[8..16].copy_from_slice(&7u64.to_le_bytes());
    assert_eq!(import(&mut bdd, &space, &b), Err(ImportError::UnknownVariable { row: 1, key: 7 }));
    // Swap the two rows' variables: the child now tests the earlier variable.
    let mut b: StdVec<u8> = bytes.as_slice().to_vec();
    let (k1, k2) = (b[8..16].to_vec(), b[24..32].to_vec());
    b[8..16].copy_from_slice(&k2);
    b[24..32].copy_from_slice(&k1);
    assert_eq!(import(&mut bdd, &space, &b), Err(ImportError::Order { row: 2 }));
    assert_eq!(import(&mut bdd, &space, &bytes), Ok(f));
}

#[test]
fn the_node_cap_is_a_typed_refusal() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut bdd = Bdd::new_in(8, Limits { nodes: 6, cache_bits: 4 }, alloc).unwrap();
    let mut acc = Cond::FALSE;
    let mut refused = None;
    for i in 0..8 {
        let x = bdd.var(Var::new(i)).and_then(|x| bdd.xor(acc, x));
        match x {
            Ok(c) => acc = c,
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    assert_eq!(refused, Some(CondError::NodeCap { cap: 6 }));
}
