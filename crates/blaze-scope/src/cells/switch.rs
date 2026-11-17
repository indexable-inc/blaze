//! The switch (feature `cells`): the cell core solves a crate, driving its expansions through
//! the same hook the old fill uses, and writes every module's scope ([`Cells::scope`]) in place
//! of the old fill's `done`. What follows (the root's published exports, the records) reads the
//! scopes as before.
//!
//! A module's in-memory read list (`Resolved::reads`) is left empty: no record holds it (the
//! records are written from the scopes; the crate's read list is the questions asked of its
//! dependencies, `crate::asked`), and the old core's incremental re-solve (`again.rs`) does not
//! run on these scopes.

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};

use super::{BuiltDeps, CellError, Cells, EnumNames, FragId};
use crate::Def;
use crate::facts::ScopeFacts;
use crate::resolve::Built;
use crate::scope::{ReadList, Resolved};

/// The most rounds of made modules (blocks and expansions' modules found after a run) one
/// crate's solve takes: a crate past it is refused by name, never cut short.
pub const MOST_ROUNDS: u32 = 64;

/// Why the cell core wrote no scopes for a crate, naming the thing.
#[derive(Clone, Debug)]
pub enum Unwritten {
    Load(CellError),
    Run(CellError),
    /// A module with `count` imports or calls the core holds as outside what it follows, or
    /// left undecided: its scope would not be the answer (rule 3: refused, never guessed).
    Unknown { module: ItemKey, count: u32 },
    /// Made modules kept appearing past [`MOST_ROUNDS`] rounds.
    Rounds,
}

impl core::fmt::Display for Unwritten {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unwritten::Load(e) => write!(f, "the cell core's load: {e}"),
            Unwritten::Run(e) => write!(f, "the cell core's run: {e}"),
            Unwritten::Unknown { module, count } => {
                write!(f, "module {module:?}: {count} imports or macro calls the cell core does not follow or left undecided")
            }
            Unwritten::Rounds => write!(f, "made modules kept appearing past {MOST_ROUNDS} rounds of the cell core"),
        }
    }
}

/// What [`solve_and_write`] gives: every module's scope, and the extern prelude's names the
/// crate's macro-written root `extern crate` items added (R8; the Reference,
/// `names.preludes.extern.decl`: an `extern crate` at the root adds its name to the extern
/// prelude), sorted by name: none of them is a name the prelude had before.
pub(crate) struct Solved<'a> {
    pub done: HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    pub added_externs: AVec<'a, (&'a str, ItemKey)>,
    pub d116_reads: AVec<'a, crate::resolve::D116Read<'a>>,
    pub d116_calls: AVec<'a, crate::resolve::D116Call<'a>>,
}

/// Solves the crate under `root` with the cell core and writes each of its modules' scopes.
/// `expand(call, def)` makes a call's expansion, which `facts.expansion` then hands back;
/// `made` lists every module made so far (expansions' modules, blocks); `places` says where
/// a block's body is written.
#[expect(clippy::too_many_arguments, reason = "the solve's inputs, each named; one caller")]
pub(crate) fn solve_and_write<'a, F: ScopeFacts<Name = &'a str>>(
    facts: &F,
    built: &Built<'_, 'a>,
    enums: &dyn EnumNames<&'a str>,
    places: &dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
    root: ItemKey,
    expand: &dyn Fn(ItemKey, Def),
    made: &dyn Fn(&mut AVec<'a, ItemKey>),
    alloc: Alloc<'a>,
) -> Result<Solved<'a>, Unwritten> {
    let deps = BuiltDeps { built, alloc, asks: true, through: core::cell::Cell::new(None) };
    let mut c = Cells::load_with(facts, root, Some(&deps), alloc).map_err(Unwritten::Load)?;
    c.enums = Some(enums);
    c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    // The modules no `mod` line names (a block's module) and those made after the walk: solved
    // after the rest, round by round, until a run makes none.
    let mut rounds = 0u32;
    loop {
        let mut extra: AVec<'a, ItemKey> = AVec::new_in(alloc);
        made(&mut extra);
        extra.retain(|k| !c.by_key.contains_key(k));
        if extra.is_empty() {
            break;
        }
        rounds += 1;
        if rounds > MOST_ROUNDS {
            return Err(Unwritten::Rounds);
        }
        extra.sort_unstable();
        extra.dedup();
        c.load_more_at(facts, &extra, places).map_err(Unwritten::Load)?;
        c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    }
    let ix = c.write_index(alloc);
    let mut keys: AVec<'a, ItemKey> = AVec::with_capacity_in(c.modules.len(), alloc);
    keys.extend(c.modules.iter().filter(|m| !m.foreign && !m.unread && m.text != FragId::MAX).map(|m| m.key));
    keys.sort_unstable();
    let mut done: HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>> = HashMap::new_in(alloc);
    for &k in keys.iter() {
        let Some((scope, unknown)) = c.scope(&ix, k, alloc) else { continue };
        if unknown > 0 {
            return Err(Unwritten::Unknown { module: k, count: unknown });
        }
        done.insert(k, Resolved { scope, reads: ReadList { entries: AVec::new_in(alloc) } });
    }
    let mut added_externs: AVec<'a, (&'a str, ItemKey)> = AVec::with_capacity_in(c.outer.added.len(), alloc);
    added_externs.extend(c.outer.added.iter().map(|(&name, &root)| (name, root)));
    let mut d116_reads = AVec::with_capacity_in(c.d116_reads.len(), alloc);
    d116_reads.extend(c.d116_reads.iter().map(|(&((r, rk), m, ns, name, pending), &n)| (c.modules[r as usize].key, rk, c.modules[m as usize].key, ns, name, pending, n)));
    let d116_calls = d116::calls(&mut c, alloc);
    Ok(Solved { done, added_externs, d116_reads, d116_calls })
}

/// D116's counts (names example `--d116-counts`): each macro call's path answered again over the
/// settled tables with what expansions wrote left out. Temporary: removed once D116 is decided.
mod d116 {
    use blaze_own::{AVec, Alloc};

    use super::super::{Cells, FragId, ItemId, Layer, ModId, Outcome, Slot, Step, Value, ns_at};
    use crate::resolve::{D116Answer, D116Call};
    use crate::{DefKind, Namespace, PathKind, Unresolved};

    type C<'c, 'x, 'a> = &'c mut Cells<'x, &'a str>;

    fn text(c: &Cells<'_, &str>, f: FragId, limit: u32) -> bool {
        c.frags.get(f as usize).is_some_and(|fr| fr.up.is_none()) || f < limit
    }

    /// A settled cell's value (made and drained now if it was never read).
    fn settled<'a>(c: C<'_, '_, 'a>, m: ModId, ns: Namespace, name: &'a str) -> Value {
        let id = c.cell(m, ns, name, Layer::Full);
        c.drain_cells();
        c.cells[id as usize].value.unwrap_or(Value::Outside)
    }

    /// `name` in `ns` of `m` from its text, its text's named imports and its text's globs (each
    /// glob's source as settled), with what expansions after `limit` wrote left out.
    fn cell<'x, 'a>(c: C<'_, 'x, 'a>, m: ModId, ns: Namespace, name: &'a str, limit: u32) -> Value {
        if c.modules[m as usize].foreign || c.modules[m as usize].unread {
            return settled(c, m, ns, name);
        }
        if let Some(&(def, vis, _, f)) = c.modules[m as usize].explicit.get(&(ns, name))
            && text(c, f, limit)
        {
            return Value::Found(def, vis);
        }
        let named: AVec<'x, ItemId> = c.modules[m as usize].named.get(&name).map_or_else(|| AVec::new_in(c.alloc), |v| v.clone());
        for &i in named.iter() {
            let it = &c.items[i as usize];
            if !text(c, it.at, limit) {
                continue;
            }
            match it.outcome {
                Some(Outcome::Named(b)) => {
                    if let Some((d, v)) = b[ns_at(ns)] {
                        return Value::Found(d, v);
                    }
                }
                Some(Outcome::Outside) => return Value::Outside,
                _ => {}
            }
        }
        let globs = c.modules[m as usize].glob_items.clone();
        let mut acc = Value::NotFound;
        for &g in globs.iter() {
            if !text(c, c.items[g as usize].at, limit) {
                continue;
            }
            let Some(Outcome::Glob(src)) = c.items[g as usize].outcome else { continue };
            let Some(&s) = c.by_key.get(&src) else { continue };
            match (acc, settled(c, s, ns, name)) {
                (_, Value::NotFound) => {}
                (Value::NotFound, v) => acc = v,
                (Value::Found(a, _), Value::Found(b, _)) if a == b => {}
                (Value::Found(a, v), Value::Found(..) | Value::Ambiguous(..)) => acc = Value::Ambiguous(a, v),
                _ => {}
            }
        }
        acc
    }

    /// The textual scope at `(frag, pos)` with no expansion after `limit` in it.
    fn textual(c: &Cells<'_, &str>, frag: FragId, pos: u32, name: &str, limit: u32) -> Option<crate::Def> {
        let (mut f, mut end) = (frag, pos);
        let mut stack: AVec<'_, (FragId, usize)> = AVec::new_in(c.alloc);
        loop {
            let below = c.frags[f as usize].slots.partition_point(|s| s.0 < end);
            stack.push((f, below));
            while let Some(top) = stack.last_mut() {
                if top.1 == 0 {
                    stack.pop();
                    continue;
                }
                top.1 -= 1;
                match c.frags[top.0 as usize].slots[top.1].1 {
                    Slot::Rules(n, def, _) if n == name => return Some(def),
                    Slot::Mod(child, true) => {
                        let t = c.modules[child as usize].text;
                        stack.push((t, c.frags[t as usize].slots.len()));
                    }
                    Slot::Call(j) => {
                        if let Some(x) = c.items[j as usize].expansion
                            && x < limit
                        {
                            stack.push((x, c.frags[x as usize].slots.len()));
                        }
                    }
                    _ => {}
                }
            }
            let fr = &c.frags[f as usize];
            match fr.up.or(c.modules[fr.module as usize].mod_line) {
                Some((p, at)) => {
                    f = p;
                    end = at;
                }
                None => return None,
            }
        }
    }

    fn answer(v: Value) -> D116Answer {
        match v {
            Value::Found(d, _) => D116Answer::Found(d),
            Value::Ambiguous(d, _) => D116Answer::Ambiguous(d),
            Value::NotFound => D116Answer::NotFound,
            Value::Outside => D116Answer::Outside,
        }
    }

    /// Call `i`'s path with expansions after `limit` left out: the answer and the leaf's module.
    fn walk(c: C<'_, '_, '_>, i: ItemId, limit: u32) -> (D116Answer, ModId) {
        let (m, kind, at) = (c.items[i as usize].module, c.items[i as usize].kind, c.items[i as usize].call);
        let segs = c.items[i as usize].segments.clone();
        let (Some(at), Some(&leaf)) = (at, segs.last()) else { return (D116Answer::Refused, m) };
        let shape = at.shape;
        if kind == PathKind::Plain && segs.len() == 1 {
            if let Some(d) = textual(c, at.frag, at.pos, leaf, limit) {
                return (D116Answer::Found(d), m);
            }
            return match cell(c, m, Namespace::Macros, leaf, limit) {
                Value::NotFound => match c.outer_of(Namespace::Macros, leaf, Some(shape), None) {
                    Step::Done(Some(v)) => (answer(v), m),
                    Step::Done(None) => (D116Answer::NotFound, m),
                    Step::Wait(_) => (D116Answer::Outside, m),
                },
                v => (answer(v), m),
            };
        }
        let mut first = 0;
        let mut cur = match kind {
            PathKind::Crate => c.root,
            PathKind::SelfMod | PathKind::Plain => m,
            PathKind::Super(n) => match c.up(m, n) {
                Some(p) => p,
                None => return (D116Answer::NotFound, m),
            },
            PathKind::DollarCrate(k) if k == c.krate => c.root,
            PathKind::DollarCrate(k) => match c.deps.and_then(|d| d.crate_root(k)).and_then(|r| c.module_at(r)) {
                Some(r) => r,
                None => return (D116Answer::Outside, m),
            },
            PathKind::Abs if c.edition == crate::Edition::E2015 => c.root,
            PathKind::Abs => {
                let s = segs[0];
                let named = match c.outer.externs.binary_search_by(|e| e.0.cmp(&s)) {
                    Ok(k) => Some(c.outer.externs[k].1),
                    Err(_) => c.outer.added.get(&s).copied(),
                };
                first = 1;
                match named.and_then(|r| c.module_at(r)) {
                    Some(r) => r,
                    None => return (D116Answer::Outside, m),
                }
            }
        };
        for k in first..segs.len() - 1 {
            let s = segs[k];
            let v = if kind == PathKind::Plain && k == 0 {
                match cell(c, m, Namespace::Types, s, limit) {
                    Value::NotFound => match c.outer_of(Namespace::Types, s, None, None) {
                        Step::Done(Some(v)) => v,
                        Step::Done(None) => Value::NotFound,
                        Step::Wait(_) => Value::Outside,
                    },
                    v => v,
                }
            } else {
                cell(c, cur, Namespace::Types, s, limit)
            };
            match v {
                Value::Found(def, _) if def.kind == DefKind::Module => match c.module_at(def.key) {
                    Some(next) => cur = next,
                    None => return (D116Answer::Outside, cur),
                },
                Value::Found(..) | Value::Outside => return (D116Answer::Outside, cur),
                v => return (answer(v), cur),
            }
        }
        (answer(cell(c, cur, Namespace::Macros, leaf, limit)), cur)
    }

    pub(super) fn calls<'a>(c: C<'_, '_, 'a>, alloc: Alloc<'a>) -> AVec<'a, D116Call<'a>> {
        let mut out = AVec::new_in(alloc);
        for i in 0..c.items.len() as ItemId {
            let it = &c.items[i as usize];
            let Some(at) = it.call else { continue };
            if it.deep || c.modules[it.module as usize].foreign {
                continue;
            }
            let last = match it.outcome {
                Some(Outcome::Called(d)) => D116Answer::Found(d),
                Some(Outcome::Failed(Unresolved::Ambiguous(_))) => D116Answer::Ambiguous(super::super::BUILTIN_ATTR),
                Some(Outcome::Failed(Unresolved::NotFound)) => D116Answer::NotFound,
                // A built-in attribute or a derive's helper: no macro lookup to wait on.
                Some(Outcome::Inert) => continue,
                Some(Outcome::Outside) | None => D116Answer::Outside,
                Some(_) => D116Answer::Refused,
            };
            let leaf = it.segments.last().copied();
            if it.kind == PathKind::Plain && it.segments.len() == 1 && leaf.is_some_and(|l| c.builtin_attrs.contains(&l)) {
                continue;
            }
            let m = it.module;
            let mut path: AVec<'a, &'a str> = AVec::new_in(alloc);
            path.extend(it.segments.iter().copied());
            let first = c.d116_first.get(&i).copied().unwrap_or(0);
            let (s, sc) = walk(c, i, 0);
            let (d, dc) = walk(c, i, first);
            let mut module_path: AVec<'a, &'a str> = AVec::new_in(alloc);
            let mut at_m = Some(m);
            while let Some(x) = at_m {
                if let Some(n) = c.modules[x as usize].name {
                    module_path.insert(0, n);
                }
                at_m = c.modules[x as usize].parent;
            }
            let _ = at;
            out.push(D116Call { module: c.modules[m as usize].key, module_path, path, cur: [c.modules[sc as usize].key, c.modules[dc as usize].key], spec: [s, d], last });
        }
        out
    }
}
