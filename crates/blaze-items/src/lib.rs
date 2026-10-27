//! Items by what they are, as Unison has its definitions (`docs/roadmap.md`, "Items by
//! content").
//!
//! Each item of the crates asked about (function, struct, enum, union, trait, impl, const,
//! static, type alias, macro) is written in a normalized form: after macro expansion, `cfg`,
//! name resolution and inference, with locals by number, without comments, formatting or the
//! order items are written in, and with each item it refers to a slot instead of a path. A
//! body's form is its MIR. Four hashes fill the slots differently:
//!
//! - `local`: with nothing. What the item itself is written as.
//! - `interface`: what its dependents see (a signature, a type's definition, a constant's
//!   value), each slot the interface of what fills it.
//! - `code`: a body's key, its form with each slot the interface of what fills it: an edit to
//!   a callee's body leaves it as it was, an edit to the callee's signature changes it.
//! - `deep`: all it is, each slot the deep hash of what fills it, as Unison's hash is.
//!
//! Items that refer to each other are hashed as one cycle, as Unison hashes them: the members
//! are ordered by their hashes with the cycle left out, and each member's hash is the cycle's
//! and its place in it. A hash says what an item is and never which: two `static`s written
//! alike hash alike and are still two.

extern crate ra_ap_rustc_type_ir as rustc_type_ir;

mod form;
mod index;

use std::time::{Duration, Instant};

use base_db::Crate;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, FunctionId, HasModule, ImplId, MacroId, ModuleDefId, StaticId,
    TraitId, TypeAliasId, expr_store::Body, item_scope::ItemScope,
};
use hir_ty::db::HirDatabase;
use rustc_hash::{FxHashMap, FxHashSet};

pub use form::Hash;
pub use index::{
    Code, Entries, Index, Live, Pushed, code_names, codes_of, entries_in, file_of, modules_in,
    modules_of, moved_out, moves_written,
};

/// Runs the work for each of `0..count`, on as many threads as there are, each with a
/// database of its own.
pub type Fan<'a> = dyn Fn(usize, &(dyn Fn(&dyn HirDatabase, usize) + Sync)) + 'a;

/// What items refer to, for many of them in turn.
pub struct Refs<'db>(form::Cx<'db>);

impl<'db> Refs<'db> {
    pub fn new(db: &'db dyn HirDatabase) -> Refs<'db> {
        Refs(form::Cx::refs_only(db))
    }

    /// What an item's interface refers to, and its code, with the coroutines written in it, or
    /// why part of it could not be written.
    pub fn of_code(
        &self,
        id: ItemId,
        body: Option<DefWithBodyId>,
    ) -> Result<Vec<ItemId>, &'static str> {
        let node = self.0.node_with(id, false);
        node.interface.missed.map_or(Ok(()), Err)?;
        let mut refs = node.interface.refs;
        if let Some(body) = body {
            let code = self.0.code(body);
            code.missed.map_or(Ok(()), Err)?;
            refs.extend(code.refs);
        }
        let mut unique = FxHashSet::default();
        refs.retain(|it| unique.insert(*it));
        Ok(refs)
    }

    /// What generic arguments refer to, or why part of them could not be written.
    pub fn of_args(
        &self,
        args: hir_ty::next_solver::GenericArgs<'db>,
    ) -> Result<Vec<ItemId>, &'static str> {
        let sub = self.0.of_args(args);
        sub.missed.map_or(Ok(sub.refs), Err)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ItemId {
    Function(FunctionId),
    Adt(AdtId),
    Trait(TraitId),
    Impl(ImplId),
    Const(ConstId),
    Static(StaticId),
    Alias(TypeAliasId),
    Macro(MacroId),
}

impl ItemId {
    pub fn kind(self) -> &'static str {
        match self {
            ItemId::Function(_) => "fn",
            ItemId::Adt(AdtId::StructId(_)) => "struct",
            ItemId::Adt(AdtId::EnumId(_)) => "enum",
            ItemId::Adt(AdtId::UnionId(_)) => "union",
            ItemId::Trait(_) => "trait",
            ItemId::Impl(_) => "impl",
            ItemId::Const(_) => "const",
            ItemId::Static(_) => "static",
            ItemId::Alias(_) => "type",
            ItemId::Macro(_) => "macro",
        }
    }

    pub fn krate(self, db: &dyn HirDatabase) -> Crate {
        match self {
            ItemId::Function(it) => it.krate(db),
            ItemId::Adt(it) => it.krate(db),
            ItemId::Trait(it) => it.krate(db),
            ItemId::Impl(it) => it.krate(db),
            ItemId::Const(it) => it.krate(db),
            ItemId::Static(it) => it.krate(db),
            ItemId::Alias(it) => it.krate(db),
            ItemId::Macro(it) => it.krate(db),
        }
    }
}

pub struct Item {
    pub id: ItemId,
    pub local: Hash,
    pub interface: Hash,
    /// For an item with a body: a function's or a static's.
    pub code: Option<Hash>,
    pub deep: Hash,
    /// Why part of the item could not be written: its hashes then say less than it is.
    pub missed: Option<&'static str>,
    /// What its slots refer to, its signature's then its body's, each with the hash its code
    /// hash takes of it.
    pub refs: Vec<(ItemId, Hash)>,
}

pub struct Items {
    pub items: Vec<Item>,
    /// How many items of other crates were referred to, each by the name `external` gave it.
    pub external: usize,
    /// Writing the forms, most of it building the MIR of each body, and hashing them.
    pub forms: Duration,
    pub hashes: Duration,
}

/// Each item of `crates`, and each item of theirs that one of them refers to. `external` names
/// an item by where it is written: an item of any other crate is the same item wherever and
/// whenever that name is, and an item of `crates` whose form missed something is hashed with it.
pub fn of_crates(
    db: &dyn HirDatabase,
    crates: &[Crate],
    external: &dyn Fn(ItemId) -> String,
) -> Items {
    let start = Instant::now();
    let ours: FxHashSet<Crate> = crates.iter().copied().collect();
    let cx = form::Cx::new(db);
    let mut queue = Vec::new();
    for &krate in crates {
        declared(db, krate, &mut queue);
    }
    let mut index = FxHashMap::<ItemId, usize>::default();
    let mut nodes = Vec::new();
    let mut ids = Vec::new();
    while let Some(id) = queue.pop() {
        if index.contains_key(&id) {
            continue;
        }
        index.insert(id, nodes.len());
        let mut node = cx.node(id);
        // What a form could not write could differ between two items whose forms are the
        // same: such a form is this item's alone, by where it is written.
        for sub in std::iter::once(&mut node.interface).chain(node.body.as_mut()) {
            if sub.missed.is_some() {
                let mut hasher = blake3::Hasher::new();
                hasher.update(&sub.digest).update(b"missed").update(external(id).as_bytes());
                sub.digest = *hasher.finalize().as_bytes();
            }
        }
        let body_refs = node.body.iter().flat_map(|it| &it.refs);
        for &r in node.interface.refs.iter().chain(body_refs) {
            if !index.contains_key(&r) && ours.contains(&r.krate(db)) {
                queue.push(r);
            }
        }
        ids.push(id);
        nodes.push(node);
    }
    let forms = start.elapsed();

    let start = Instant::now();
    let mut externals = FxHashMap::<ItemId, Hash>::default();
    let mut target = |id: ItemId| match index.get(&id) {
        Some(&at) => Target::Ours(at),
        None => Target::Fixed(
            *externals
                .entry(id)
                .or_insert_with(|| *blake3::hash(external(id).as_bytes()).as_bytes()),
        ),
    };
    let interface_refs: Vec<Vec<Target>> =
        nodes.iter().map(|it| it.interface.refs.iter().map(|&r| target(r)).collect()).collect();
    let body_refs: Vec<Vec<Target>> = nodes
        .iter()
        .map(|it| it.body.iter().flat_map(|it| &it.refs).map(|&r| target(r)).collect())
        .collect();
    let external = externals.len();
    let local: Vec<Hash> = nodes
        .iter()
        .map(|it| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&it.interface.digest);
            match &it.body {
                Some(body) => hasher.update(&[1]).update(&body.digest),
                None => hasher.update(&[0]),
            };
            *hasher.finalize().as_bytes()
        })
        .collect();

    let interface = by_cycles(
        nodes.len(),
        &|at| interface_refs[at].clone(),
        &|at| nodes[at].interface.digest,
        b"interface",
    );
    let all_refs = |at: usize| {
        let mut refs = interface_refs[at].clone();
        refs.push(Target::Fixed([0; 32]));
        refs.extend(body_refs[at].iter().copied());
        refs
    };
    let deep = by_cycles(nodes.len(), &all_refs, &|at| local[at], b"deep");
    let filled = |target: Target| match target {
        Target::Ours(at) => interface[at],
        Target::Fixed(hash) => hash,
    };
    let target_of = |id: ItemId| match index.get(&id) {
        Some(&at) => Target::Ours(at),
        None => Target::Fixed(externals[&id]),
    };
    let items = (0..nodes.len())
        .map(|at| {
            let code = nodes[at].body.as_ref().map(|_| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(b"code").update(&local[at]);
                for &r in &interface_refs[at] {
                    hasher.update(&filled(r));
                }
                hasher.update(&[0xff]);
                for &r in &body_refs[at] {
                    hasher.update(&filled(r));
                }
                *hasher.finalize().as_bytes()
            });
            let missed = nodes[at]
                .interface
                .missed
                .or_else(|| nodes[at].body.as_ref().and_then(|it| it.missed));
            let refs = nodes[at]
                .interface
                .refs
                .iter()
                .chain(nodes[at].body.iter().flat_map(|it| &it.refs))
                .map(|&r| (r, filled(target_of(r))))
                .collect();
            Item {
                id: ids[at],
                local: local[at],
                interface: interface[at],
                code,
                deep: deep[at],
                missed,
                refs,
            }
        })
        .collect();
    Items { items, external, forms, hashes: start.elapsed() }
}

#[derive(Clone, Copy)]
enum Target {
    Ours(usize),
    Fixed(Hash),
}

/// The hash of each of `n` items, of `own` and of the hashes of what `refs` says it refers
/// to, with the items that refer to each other hashed as one cycle.
fn by_cycles(
    n: usize,
    refs: &dyn Fn(usize) -> Vec<Target>,
    own: &dyn Fn(usize) -> Hash,
    tag: &[u8],
) -> Vec<Hash> {
    let edges: Vec<Vec<Target>> = (0..n).map(refs).collect();
    let mut done: Vec<Option<Hash>> = vec![None; n];
    for cycle in components(n, &edges) {
        let members: FxHashSet<usize> = cycle.iter().copied().collect();
        let alone = cycle.len() == 1
            && !edges[cycle[0]].iter().any(|it| matches!(it, Target::Ours(at) if *at == cycle[0]));
        let fill = |target: &Target, in_cycle: &dyn Fn(usize) -> [u8; 5]| match *target {
            Target::Ours(at) if members.contains(&at) => in_cycle(at).to_vec(),
            Target::Ours(at) => {
                done[at].expect("what a cycle refers to is hashed before it").to_vec()
            }
            Target::Fixed(hash) => hash.to_vec(),
        };
        if alone {
            let at = cycle[0];
            let mut hasher = blake3::Hasher::new();
            hasher.update(tag).update(&own(at));
            for target in &edges[at] {
                hasher.update(&fill(target, &|_| unreachable!()));
            }
            done[at] = Some(*hasher.finalize().as_bytes());
            continue;
        }
        // Ordered by what each member is with the cycle left out.
        let mut ordered: Vec<(Hash, usize)> = cycle
            .iter()
            .map(|&at| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(tag).update(b"member").update(&own(at));
                for target in &edges[at] {
                    hasher.update(&fill(target, &|_| [0xff; 5]));
                }
                (*hasher.finalize().as_bytes(), at)
            })
            .collect();
        ordered.sort();
        let place: FxHashMap<usize, u32> =
            ordered.iter().enumerate().map(|(i, &(_, at))| (at, i as u32)).collect();
        let mut hasher = blake3::Hasher::new();
        hasher.update(tag).update(b"cycle").update(&(ordered.len() as u64).to_le_bytes());
        for &(_, at) in &ordered {
            hasher.update(&own(at));
            for target in &edges[at] {
                hasher.update(&fill(target, &|other| {
                    let [a, b, c, d] = place[&other].to_le_bytes();
                    [0xfe, a, b, c, d]
                }));
            }
        }
        let whole = *hasher.finalize().as_bytes();
        for (i, &(_, at)) in ordered.iter().enumerate() {
            let mut hasher = blake3::Hasher::new();
            hasher.update(&whole).update(&(i as u32).to_le_bytes());
            done[at] = Some(*hasher.finalize().as_bytes());
        }
    }
    done.into_iter().map(|it| it.expect("every item is in a component")).collect()
}

/// The strongly connected components of the graph, each after every component it reaches
/// (Tarjan's algorithm, without recursion).
fn components(n: usize, edges: &[Vec<Target>]) -> Vec<Vec<usize>> {
    const UNSEEN: usize = usize::MAX;
    let mut order = vec![UNSEEN; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut found = Vec::new();
    let mut next = 0;
    for root in 0..n {
        if order[root] != UNSEEN {
            continue;
        }
        let mut walk: Vec<(usize, usize)> = vec![(root, 0)];
        order[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (at, ref mut edge)) = walk.last_mut() {
            let ours = edges[at].iter().filter_map(|it| match it {
                Target::Ours(to) => Some(*to),
                Target::Fixed(_) => None,
            });
            if let Some(to) = ours.clone().nth(*edge) {
                *edge += 1;
                if order[to] == UNSEEN {
                    order[to] = next;
                    low[to] = next;
                    next += 1;
                    stack.push(to);
                    on_stack[to] = true;
                    walk.push((to, 0));
                } else if on_stack[to] {
                    low[at] = low[at].min(order[to]);
                }
                continue;
            }
            walk.pop();
            if let Some(&(parent, _)) = walk.last() {
                low[parent] = low[parent].min(low[at]);
            }
            if low[at] == order[at] {
                let mut component = Vec::new();
                loop {
                    let member = stack.pop().expect("the component's members are on the stack");
                    on_stack[member] = false;
                    component.push(member);
                    if member == at {
                        break;
                    }
                }
                found.push(component);
            }
        }
    }
    found
}

/// The items a crate declares: in its modules, in the impls and traits there, and in the
/// blocks of the bodies of all of them.
fn declared(db: &dyn HirDatabase, krate: Crate, out: &mut Vec<ItemId>) {
    let mut modules = vec![hir::Crate::from(krate).root_module(db)];
    let mut bodies = Vec::new();
    while let Some(module) = modules.pop() {
        modules.extend(module.children(db));
        let id = hir_def::ModuleId::from(module);
        in_scope(db, id.def_map(db).module(db, id).scope(db), out, &mut bodies);
    }
    in_bodies(db, bodies, out);
}

/// The items declared in `bodies`, and in the bodies of those.
fn in_bodies(db: &dyn HirDatabase, mut bodies: Vec<DefWithBodyId>, out: &mut Vec<ItemId>) {
    while let Some(body) = bodies.pop() {
        for (_, def_map) in Body::of(db, body).blocks(db) {
            for (_, module) in def_map.modules(db) {
                in_scope(db, module.scope(db), out, &mut bodies);
            }
        }
    }
}

fn in_scope(
    db: &dyn HirDatabase,
    scope: &ItemScope,
    out: &mut Vec<ItemId>,
    bodies: &mut Vec<DefWithBodyId>,
) {
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) => with_body(ItemId::Function(it), it.into(), out, bodies),
            ModuleDefId::AdtId(it) => out.push(ItemId::Adt(it)),
            ModuleDefId::TraitId(it) => {
                out.push(ItemId::Trait(it));
                for &(_, item) in it.trait_items(db).items.iter() {
                    assoc(item, out, bodies);
                }
            }
            ModuleDefId::ConstId(it) => with_body(ItemId::Const(it), it.into(), out, bodies),
            ModuleDefId::StaticId(it) => with_body(ItemId::Static(it), it.into(), out, bodies),
            ModuleDefId::TypeAliasId(it) => out.push(ItemId::Alias(it)),
            ModuleDefId::MacroId(it) => out.push(ItemId::Macro(it)),
            _ => {}
        }
    }
    for (_, macros) in scope.legacy_macros() {
        out.extend(macros.iter().map(|&it| ItemId::Macro(it)));
    }
    for it in scope.unnamed_consts() {
        with_body(ItemId::Const(it), it.into(), out, bodies);
    }
    for imp in scope.impls() {
        out.push(ItemId::Impl(imp));
        for &(_, item) in imp.impl_items(db).items.iter() {
            assoc(item, out, bodies);
        }
    }
}

fn assoc(item: hir_def::AssocItemId, out: &mut Vec<ItemId>, bodies: &mut Vec<DefWithBodyId>) {
    match item {
        hir_def::AssocItemId::FunctionId(it) => with_body(item.into(), it.into(), out, bodies),
        hir_def::AssocItemId::ConstId(it) => with_body(item.into(), it.into(), out, bodies),
        hir_def::AssocItemId::TypeAliasId(_) => out.push(item.into()),
    }
}

fn with_body(
    id: ItemId,
    body: DefWithBodyId,
    out: &mut Vec<ItemId>,
    bodies: &mut Vec<DefWithBodyId>,
) {
    out.push(id);
    bodies.push(body);
}
