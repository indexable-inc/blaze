//! `module_cycle`: modules of one crate that import from each other, so none can be read, moved
//! or tested without the rest (the acyclic dependencies principle, R. C. Martin, "Granularity",
//! C++ Report, 1996; Lakos, "Large-Scale C++ Software Design", 1996, on levelization).
//!
//! The graph: an edge from module `a` to module `b` for each name `a`'s scope holds of an item
//! `b` defines (`use` included), in the same crate. An edge between a module and its own
//! ancestor (`super::`, a child it declares) is the nesting, not a dependency between peers,
//! and is left out; every other edge is lifted to the two children of the modules' nearest
//! common ancestor, so `a::x` using `b::y` is `a` using `b`, the level the reader moves code
//! between. Each strongly connected group of two or more is one finding (Tarjan's algorithm,
//! "Depth-first search and linear graph algorithms", SIAM J. Comput. 1(2), 1972), its value the
//! modules in it, listed with their edges; the way down is the edge with the fewest names.

use std::alloc::Global;

use blaze_hash::HashMap;
use syntax::TextRange;

use super::{Item, Measure, Pending, judge, report};
use crate::{
    baseline::items,
    blaze::sans_io_core::module_path,
    check::Cx,
};

/// How many edges a finding lists before it says how many more.
const MOST_LISTED: usize = 12;

/// An edge of the lifted graph: from, to (indices into the sorted modules) and the names it is
/// made of, sorted.
struct Edge {
    from: u32,
    to: u32,
    names: Vec<String, Global>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Measure::Cycle.lint()) {
        return;
    }
    let pending = find(cx);
    report(cx, pending);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Pending, Global> {
    let mut out = Vec::new_in(Global);
    let db = cx.db();
    let krate = cx.krate();
    // The crate's modules, in the order of their paths, so every output is in that order.
    let mut modules = Vec::new_in(Global);
    modules.extend(krate.modules(db).into_iter().map(|it| (module_path(db, it), it)));
    modules.sort_by(|a, b| a.0.cmp(&b.0));
    let mut index = HashMap::new_in(Global);
    for (at, &(_, module)) in modules.iter().enumerate() {
        index.insert(module, at as u32);
    }
    // The lifted edges, each with its names; sorted and merged by (from, to).
    let mut raw: Vec<(u32, u32, String), Global> = Vec::new_in(Global);
    for &(_, module) in modules.iter() {
        for (name, def) in module.scope(db, None) {
            let hir::ScopeDef::ModuleDef(def) = def else { continue };
            let target = match def {
                hir::ModuleDef::Module(it) => Some(it),
                other => other.module(db),
            };
            let Some(target) = target else { continue };
            if target.krate(db) != krate || target == module {
                continue;
            }
            let Some((from, to)) = lifted(db, module, target) else { continue };
            let (Some(&from), Some(&to)) = (index.get(&from), index.get(&to)) else { continue };
            raw.push((from, to, name.as_str().to_owned()));
        }
    }
    raw.sort_unstable();
    raw.dedup();
    let mut edges: Vec<Edge, Global> = Vec::new_in(Global);
    for (from, to, name) in raw {
        match edges.last_mut() {
            Some(last) if last.from == from && last.to == to => last.names.push(name),
            _ => {
                let mut names = Vec::new_in(Global);
                names.push(name);
                edges.push(Edge { from, to, names });
            }
        }
    }
    let count = modules.len();
    let mut successors: Vec<Vec<u32, Global>, Global> = Vec::with_capacity_in(count, Global);
    successors.resize_with(count, || Vec::new_in(Global));
    for edge in &edges {
        successors[edge.from as usize].push(edge.to);
    }
    let at = cx
        .root()
        .first_token()
        .map_or(TextRange::empty(0.into()), |it| it.text_range());
    for group in strongly_connected(&successors) {
        if group.len() < 2 {
            continue;
        }
        let in_group = |it: u32| group.binary_search(&it).is_ok();
        let mut paths = Vec::with_capacity_in(group.len(), Global);
        paths.extend(group.iter().map(|&it| modules[it as usize].0.as_str()));
        let item = Item { path: paths.join(" "), fingerprint: items::fingerprint(paths.iter().copied()) };
        let Some(over) = judge(cx, Measure::Cycle, &item, group.len() as u64) else { continue };
        let mut inside = Vec::new_in(Global);
        inside.extend(edges.iter().filter(|it| in_group(it.from) && in_group(it.to)));
        let path = |it: u32| modules[it as usize].0.as_str();
        let mut listed = Vec::with_capacity_in(inside.len().min(MOST_LISTED), Global);
        listed.extend(inside.iter().take(MOST_LISTED).map(|it| {
            format!("`{}` -> `{}` ({})", path(it.from), path(it.to), it.names.join(", "))
        }));
        let more = inside.len().saturating_sub(MOST_LISTED);
        let mut said = format!(
            "{} modules of this crate import from each other: {}",
            group.len(),
            listed.join("; ")
        );
        if more > 0 {
            said.push_str(&format!("; and {more} more edges"));
        }
        let cut = inside.iter().min_by(|a, b| {
            a.names.len().cmp(&b.names.len()).then(a.from.cmp(&b.from)).then(a.to.cmp(&b.to))
        });
        let help = cut.map(|it| {
            format!(
                "cut `{}` -> `{}`, the edge of fewest names ({}): move what it uses into `{}`, or both into a module neither depends on",
                path(it.from),
                path(it.to),
                it.names.len(),
                path(it.from)
            )
        });
        out.push(Pending { at, item, over, said, way: None, help });
    }
    out
}

/// The edge `from` -> `to` lifted to the children of their nearest common ancestor; `None`
/// when one is the other's ancestor.
fn lifted(db: &base_db::Db, from: hir::Module, to: hir::Module) -> Option<(hir::Module, hir::Module)> {
    // Root first.
    let mut up = from.path_to_root(db);
    up.reverse();
    let mut down = to.path_to_root(db);
    down.reverse();
    let shared = up.iter().zip(down.iter()).take_while(|(a, b)| a == b).count();
    Some((*up.get(shared)?, *down.get(shared)?))
}

/// The strongly connected components of the graph `successors`, each sorted, by Tarjan's
/// algorithm run without recursion, nodes visited in index order.
pub(crate) fn strongly_connected(successors: &[Vec<u32, Global>]) -> Vec<Vec<u32, Global>, Global> {
    const UNSEEN: u32 = u32::MAX;
    let count = successors.len();
    let mut order = Vec::with_capacity_in(count, Global);
    order.resize(count, UNSEEN);
    let mut low = Vec::with_capacity_in(count, Global);
    low.resize(count, 0u32);
    let mut on_stack = Vec::with_capacity_in(count, Global);
    on_stack.resize(count, false);
    let mut stack: Vec<u32, Global> = Vec::new_in(Global);
    let mut out = Vec::new_in(Global);
    let mut next = 0u32;
    // The walk: a node and how many of its successors it has gone through.
    let mut walk: Vec<(u32, usize), Global> = Vec::new_in(Global);
    for root in 0..count as u32 {
        if order[root as usize] != UNSEEN {
            continue;
        }
        walk.push((root, 0));
        order[root as usize] = next;
        low[root as usize] = next;
        next += 1;
        stack.push(root);
        on_stack[root as usize] = true;
        while let Some(top) = walk.last_mut() {
            let node = top.0;
            if let Some(&to) = successors[node as usize].get(top.1) {
                top.1 += 1;
                let to_at = to as usize;
                if order[to_at] == UNSEEN {
                    order[to_at] = next;
                    low[to_at] = next;
                    next += 1;
                    stack.push(to);
                    on_stack[to_at] = true;
                    walk.push((to, 0));
                } else if on_stack[to_at] {
                    low[node as usize] = low[node as usize].min(order[to_at]);
                }
                continue;
            }
            walk.pop();
            if let Some(&(parent, _)) = walk.last() {
                low[parent as usize] = low[parent as usize].min(low[node as usize]);
            }
            if low[node as usize] == order[node as usize] {
                let mut group = Vec::new_in(Global);
                while let Some(it) = stack.pop() {
                    on_stack[it as usize] = false;
                    group.push(it);
                    if it == node {
                        break;
                    }
                }
                group.sort_unstable();
                out.push(group);
            }
        }
    }
    out.sort_by(|a: &Vec<u32, Global>, b| a.first().cmp(&b.first()));
    out
}
