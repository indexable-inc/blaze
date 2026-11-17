//! The cell core over a whole test world: every crate solved in dependency order (a dependency
//! read through [`DepNames`] as decided, as the command's ready queue reads its records), and
//! each module's scope as the core writes it. The rule tests (tests/name_rules.rs, more.rs,
//! rules.rs, records.rs) judge these scopes, as they judged the old fill's before it was deleted.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_sim::Tape;

use super::{CellPlant, Cells, DepAnswer, DepNames, FragId, Value};
use crate::Namespace;
use crate::facts::ScopeFacts;
use crate::scope::{ReadList, Resolved};
use crate::tests::{N, World};

/// The crates solved so far, each read as a dependency of the ones after it.
struct SolvedCrates {
    crates: RefCell<Vec<(CrateItemKey, ItemKey, &'static RefCell<Cells<'static, N>>)>>,
}

impl SolvedCrates {
    fn holding(&self, module: ItemKey) -> Option<&'static RefCell<Cells<'static, N>>> {
        self.crates.borrow().iter().map(|c| c.2).find(|c| {
            let c = c.borrow();
            c.by_key.get(&module).is_some_and(|&m| !c.modules[m as usize].foreign)
        })
    }
}

impl DepNames<N> for SolvedCrates {
    fn has_module(&self, module: ItemKey) -> bool {
        self.holding(module).is_some()
    }

    fn name_in(&self, module: ItemKey, ns: Namespace, name: N) -> DepAnswer {
        let Some(cells) = self.holding(module) else { return DepAnswer::Unknown };
        match cells.borrow_mut().ask(module, ns, name) {
            Ok(Some(Value::Found(d, v))) => DepAnswer::Found(d, v),
            Ok(Some(Value::Ambiguous(d, v))) => DepAnswer::Ambiguous(d, v),
            Ok(Some(Value::NotFound)) => DepAnswer::NotFound,
            Ok(Some(Value::Outside) | None) | Err(_) => DepAnswer::Unknown,
        }
    }

    fn crate_root(&self, krate: CrateItemKey) -> Option<ItemKey> {
        self.crates.borrow().iter().find(|c| c.0 == krate).map(|c| c.1)
    }

    fn has_enum(&self, _key: ItemKey) -> bool {
        false
    }
}

/// The crates of `w` with each one's root, a crate after every crate it depends on (ties by
/// crate key). `Err` names a crate whose dependencies are never all solved (a cycle).
fn in_dependency_order(w: &World) -> Result<Vec<(CrateItemKey, ItemKey)>, String> {
    let mut left: BTreeMap<CrateItemKey, (ItemKey, Vec<CrateItemKey>)> = BTreeMap::new();
    for (krate, cr) in w.roots.iter() {
        left.insert(*krate, (cr.head.root_module, cr.deps.iter().map(|d| d.krate).filter(|d| d != krate).collect()));
    }
    let mut out: Vec<(CrateItemKey, ItemKey)> = Vec::new();
    while !left.is_empty() {
        let ready = left.iter().find(|(_, (_, deps))| deps.iter().all(|d| !left.contains_key(d))).map(|(k, (root, _))| (*k, *root));
        let Some((krate, root)) = ready else {
            return Err(format!("the crates {:x?} depend on each other: no order solves them", left.keys().collect::<Vec<_>>()));
        };
        left.remove(&krate);
        out.push((krate, root));
    }
    Ok(out)
}

/// D116's equality check (`Cells::links_agree`) on the crate whose root is `root`, solved as
/// [`solve_world`] solves it: every crate before it in dependency order is solved first and read
/// as its dependency, so the scopes the tables are judged against are the decided ones (a crate
/// run alone leaves what reads its dependencies unknown, which the check refuses).
pub(crate) fn links_agree_world(w: &World, root: ItemKey) -> Result<super::Agreed, super::Disagreement<'static, N>> {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    w.expanded.borrow_mut().clear();
    let solved: &'static SolvedCrates = std::boxed::Box::leak(std::boxed::Box::new(SolvedCrates { crates: RefCell::new(Vec::new()) }));
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    let order = in_dependency_order(w).expect("a test world's crates have no dependency cycle");
    for (krate, at) in order {
        let mut c = Cells::load_planted(w, at, Some(solved), CellPlant::None, alloc).map_err(super::Disagreement::Unsolved)?;
        c.run(w, &expand, &[]).map_err(super::Disagreement::Unsolved)?;
        if at == root {
            return c.links_agree(alloc, alloc);
        }
        let held: &'static RefCell<Cells<'static, N>> = std::boxed::Box::leak(std::boxed::Box::new(RefCell::new(c)));
        solved.crates.borrow_mut().push((krate, at, held));
    }
    panic!("{root:x} is the root of no crate of the world");
}

/// Every module of `w` the cell core writes, by module, its facts read from `facts` (`w` itself,
/// or its records read back): each crate solved in dependency order, its items pushed in the
/// order `tape` draws (none: the load's order). A module the core leaves holding an unknown
/// import or call is an `Err` naming it (the command refuses it the same way:
/// `Unwritten::Unknown`).
pub(crate) fn solve_world<F: ScopeFacts<Name = N>>(
    w: &World,
    facts: &F,
    tape: Option<&Tape>,
    plant: CellPlant,
) -> Result<BTreeMap<ItemKey, Resolved<'static, N>>, String> {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    w.expanded.borrow_mut().clear();
    let solved: &'static SolvedCrates = std::boxed::Box::leak(std::boxed::Box::new(SolvedCrates { crates: RefCell::new(Vec::new()) }));
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    let mut by: BTreeMap<ItemKey, Resolved<'static, N>> = BTreeMap::new();
    for (krate, root) in in_dependency_order(w)? {
        let mut c = Cells::load_planted(facts, root, Some(solved), plant, alloc).map_err(|e| format!("crate {krate:?}: the load: {e}"))?;
        let mut order: Vec<u32> = Vec::new();
        if let Some(tape) = tape {
            let mut left: Vec<u32> = (0..c.items.len() as u32).collect();
            while !left.is_empty() {
                order.push(left.remove(tape.below(left.len() as u64, "push order") as usize));
            }
        }
        c.run(facts, &expand, &order).map_err(|e| format!("crate {krate:?}: the run: {e}"))?;
        let ix = c.write_index(alloc);
        let keys: Vec<ItemKey> = c.modules.iter().filter(|m| !m.foreign && !m.unread && m.text != FragId::MAX).map(|m| m.key).collect();
        for k in keys {
            let Some((scope, unknown)) = c.scope(&ix, k, alloc) else { continue };
            if unknown > 0 {
                return Err(format!("module {k:x}: {unknown} imports or calls the cell core left unknown"));
            }
            by.insert(k, Resolved { scope, reads: ReadList { entries: AVec::new_in(alloc) } });
        }
        let held: &'static RefCell<Cells<'static, N>> = std::boxed::Box::leak(std::boxed::Box::new(RefCell::new(c)));
        solved.crates.borrow_mut().push((krate, root, held));
    }
    Ok(by)
}
