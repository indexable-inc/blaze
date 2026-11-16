//! The crate's tests. The oracle is a truth table over at most 10 variables: every operation
//! equals the table's, equal tables are equal ids, export then import gives equal bytes and an
//! equal id, `implies` is inclusion, `sat_count` the popcount, the cover covers exactly. Each
//! planted bug (`crate::plant`) is caught by a named control.

use std::{format, string::String, vec::Vec as StdVec};

use blaze_own::{AVec, Alloc};
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::cfg::{self, Op};
use crate::isop::Cover;
use crate::plant::{self, Plant};
use crate::portable::{export, import};
use crate::{Atom, Bdd, Cond, CondError, ImportError, Limits, Space, SpaceBuilder, Var};

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

fn harness(e: impl core::fmt::Display) -> Broke {
    Broke::Harness(format!("{e}"))
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

fn table(bdd: &Bdd<'_>, c: Cond, n: usize) -> Tt {
    Tt::from_fn(n, |j| bdd.eval(c, |v| j >> v.index() & 1 == 1))
}

/// The table's function built a second way: Shannon expansion over the rows, through `ite`.
fn from_table(bdd: &mut Bdd<'_>, t: Tt) -> Result<Cond, CondError> {
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

/// One case: random operations on a pool of conditions and their tables, then every check.
fn ops_match_tables(tape: &Tape, planted: Plant) -> Result<(), Broke> {
    plant::with(planted, || ops_case(tape))
}

fn ops_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Alloc::global();
    let n = 1 + tape.index(MOST_VARS, "variables");
    let space = switches(n, alloc);
    let cache_bits = 1 + tape.index(10, "cache bits") as u8;
    let mut bdd = Bdd::new_in(n, Limits { nodes: 1 << 20, cache_bits }, alloc).map_err(harness)?;
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
        if table(&bdd, c, n) != t {
            return Err(Broke::Oracle(format!("{op} differs from its table")));
        }
    }
    // Equal tables are equal ids, built by any path.
    for (i, &(ci, ti, _)) in pool.iter().enumerate() {
        for &(cj, tj, _) in &pool[..i] {
            if (ti == tj) != (ci == cj) {
                return Err(Broke::Oracle(format!("equal tables, different ids: {ci:?} {cj:?}")));
            }
        }
    }
    let rebuilt_from = if n <= 6 { 0 } else { pool.len().saturating_sub(4) };
    for &(c, t, op) in &pool[rebuilt_from..] {
        let again = from_table(&mut bdd, t).map_err(harness)?;
        if again != c {
            return Err(Broke::Oracle(format!("equal tables, different ids: {op} {c:?}, its table {again:?}")));
        }
    }
    let valid = pool[tape.index(pool.len(), "valid")];
    let mut support: AVec<'_, Var> = AVec::new_in(alloc);
    let mut witness: AVec<'_, (Var, bool)> = AVec::new_in(alloc);
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    let mut again: AVec<'_, u8> = AVec::new_in(alloc);
    let mut cover = Cover::new_in(alloc);
    for &(c, t, op) in &pool {
        let other = pool[tape.index(pool.len(), "other")];
        if bdd.implies(c, other.0).map_err(harness)? != t.within(other.1) {
            return Err(Broke::Oracle(format!("implies is not inclusion for {op}")));
        }
        if bdd.disjoint(c, other.0).map_err(harness)? != (t.map2(other.1, |x, y| x && y).count() == 0) {
            return Err(Broke::Oracle(format!("disjoint is not an empty intersection for {op}")));
        }
        let within = t.map2(valid.1, |x, y| x && y);
        if bdd.sat_count(c, valid.0).map_err(harness)? != within.count() {
            return Err(Broke::Oracle(format!("sat_count is not the popcount for {op}")));
        }
        let found = bdd.any_sat(c, valid.0, &mut witness).map_err(harness)?;
        let row = witness.iter().filter(|(_, on)| *on).fold(0usize, |j, (v, _)| j | 1 << v.index());
        if found != (within.count() > 0) || (found && !within.get(row)) {
            return Err(Broke::Oracle(format!("any_sat gave no satisfying witness for {op}")));
        }
        bdd.support(c, &mut support).map_err(harness)?;
        let depends: StdVec<u16> = (0..n).filter(|&i| t.depends(i)).map(|i| i as u16).collect();
        let listed: StdVec<u16> = support.iter().map(|v| v.index()).collect();
        if listed != depends {
            return Err(Broke::Oracle(format!("support {listed:?} is not the variables {op} depends on {depends:?}")));
        }
        // Portable: into this arena gives the id; into a fresh one, equal bytes back.
        export(&mut bdd, &space, c, &mut bytes).map_err(harness)?;
        let same = import(&mut bdd, &space, &bytes).map_err(|e| Broke::Oracle(format!("portable: {op}: {e}")))?;
        if same != c {
            return Err(Broke::Oracle(format!("portable: {op} came back as another id")));
        }
        let mut fresh = Bdd::new_in(n, Limits { nodes: 1 << 16, cache_bits: 8 }, alloc).map_err(harness)?;
        let there = import(&mut fresh, &space, &bytes).map_err(|e| Broke::Oracle(format!("portable: {op}: {e}")))?;
        export(&mut fresh, &space, there, &mut again).map_err(harness)?;
        if again.as_slice() != bytes.as_slice() || table(&fresh, there, n) != t {
            return Err(Broke::Oracle(format!("portable: {op} wrote other bytes from another arena")));
        }
        // The cover: exactly the function, and no cube can go.
        let r = cover.compute(&mut bdd, c, c, 1 << 20).map_err(harness)?;
        let cubes = cover_tables(&cover, n);
        let union = cubes.iter().fold(Tt::from_fn(n, |_| false), |u, t| u.map2(*t, |x, y| x || y));
        if r != c || union != t {
            return Err(Broke::Oracle(format!("the cover of {op} is not exactly it")));
        }
        for skip in 0..cubes.len() {
            let rest = cubes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .fold(Tt::from_fn(n, |_| false), |u, (_, t)| u.map2(*t, |x, y| x || y));
            if rest == t {
                return Err(Broke::Oracle(format!("the cover of {op} has a redundant cube")));
            }
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

#[test]
fn operations_equal_their_truth_tables() {
    check(Config::cases(400), |tape| ops_match_tables(tape, Plant::Clean));
}

#[test]
fn control_a_unique_table_without_complement_normalisation_is_caught() {
    control(Config::cases(2000), Plant::Clean, Plant::UniqueSkipsComplement, "different ids", ops_match_tables);
}

#[test]
fn control_an_ite_cache_keyed_without_its_else_is_caught() {
    control(Config::cases(2000), Plant::Clean, Plant::IteCacheSkipsElse, "differs from its table", ops_match_tables);
}

#[test]
fn control_a_restrict_that_takes_the_wrong_branch_is_caught() {
    control(Config::cases(2000), Plant::Clean, Plant::RestrictWrongBranch, "restrict differs", ops_match_tables);
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
    check(Config::cases(400), |tape| {
        let alloc = Alloc::global();
        let n = 1 + tape.index(7, "variables");
        let space = switches(n, alloc);
        let mut bdd = Bdd::new_in(n, Limits::TASK, alloc).map_err(harness)?;
        let mut text = String::new();
        draw_predicate(tape, n, &mut text, 4);
        let expr = cfg::parse(&text, alloc).map_err(|e| Broke::Oracle(format!("`{text}` did not parse: {e}")))?;
        let mut absent = AVec::new_in(alloc);
        let c = cfg::lower(&expr, &space, "pkg", &mut bdd, &mut absent).map_err(harness)?;
        let want = Tt::from_fn(n, |j| eval_ops(&expr.ops, n, j));
        if table(&bdd, c, n) != want {
            return Err(Broke::Oracle(format!("`{text}` lowered to another function")));
        }
        if text.contains(&format!("v{n:02}")) == absent.is_empty() {
            return Err(Broke::Oracle(format!("`{text}`: the unset atom v{n:02} was not reported absent")));
        }
        Ok(())
    });
}

#[test]
fn malformed_portable_rows_are_refused_by_name() {
    let alloc = Alloc::global();
    let space = switches(3, alloc);
    let mut bdd = Bdd::new_in(3, Limits::TASK, alloc).unwrap();
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
    let alloc = Alloc::global();
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
