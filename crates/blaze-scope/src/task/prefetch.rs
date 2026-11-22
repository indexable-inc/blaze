//! The syntactic prefetch (bottom.md 6, "Decision on R"): which modules' scopes each module's
//! scope task will read, found from the declarations records alone before any scope task runs,
//! and the order and groups that follow from it.
//!
//! A module's `use` paths, glob sources, macro paths and `pub(in ..)` paths name, along the
//! module tree, the modules whose scope records its task reads: every module a path passes
//! through (a segment is a name read in that module's scope), the module its last segment is
//! read in, a glob source and, through the sources' own globs, every module a name read there
//! may come from; a name not declared where it is looked up reads that module's glob sources and
//! the standard prelude; a one-segment bang macro reads each ancestor's textual scope and each
//! `#[macro_use]` child's. A path through a named import is followed through that import's own
//! path (at most [`MOST_HOPS`] times). What the text alone does not decide (a path through an
//! item or through a glob, imports and modules a macro writes) adds no further edge: the task may
//! still wait on it and re-enter.
//!
//! The graph is a function of the declarations and the crate root only (rule 25): no schedule,
//! no earlier fill. Its strongly connected components, in dependency order, are the first tasks:
//! each component one task (a group when it has more than one module), entered when every
//! component it reads is finished. The in-memory [`super::Fill`] and the driver both take it
//! from here (rule 30).

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallShape, DeclsBuilder, Edition, ModuleInfo, Path, PathKind, Rename, SurfaceError, UseForm, WrittenVis};

use crate::facts::{Name, ScopeFacts};
use crate::resolver::MAX_MODULE_DEPTH;

/// The most named imports one path walk follows (`use a::b as c; use c::d;`): past it the walk
/// stops where it stands and what is left is read at run time.
pub const MOST_HOPS: usize = 16;

/// The first tasks of a fill.
#[derive(Debug)]
pub struct Prefetch<'a> {
    /// The strongly connected components of the read graph, each sorted, every component after
    /// each one it reads.
    pub components: AVec<'a, AVec<'a, ItemKey>>,
    /// For each component, the earlier components it reads (indices into `components`), sorted.
    pub reads: AVec<'a, AVec<'a, u32>>,
    /// Edges between distinct modules of the graph.
    pub edges: u64,
}

impl Prefetch<'_> {
    /// The size of the largest component.
    pub fn largest(&self) -> usize {
        self.components.iter().map(|c| c.len()).max().unwrap_or(0)
    }
}

struct Node<'f, N> {
    info: ModuleInfo<N>,
    decls: DeclsBuilder<'f, N>,
}

/// What a walk reads last: a module (a glob's source) or a name in a module.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    Module,
    Name,
}

/// Where a path is written: a `use` (2015: a plain path is crate-relative) or elsewhere.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Use,
    Other,
}

struct Tree<'f, 'a, N> {
    nodes: AVec<'a, Node<'f, N>>,
    index: HashMap<ItemKey, u32, Alloc<'a>>,
    root: Option<u32>,
    prelude: Option<u32>,
    edition: Edition,
    here: CrateItemKey,
    /// The extern prelude's names, sorted.
    externs: AVec<'a, N>,
}

/// What one module's walks found: modules read, and modules whose glob closure is read.
struct Found<'a> {
    reads: AVec<'a, u32>,
    closures: AVec<'a, u32>,
}

impl<'f, N: Name> Tree<'f, '_, N> {
    fn parent(&self, at: u32) -> Option<u32> {
        self.nodes[at as usize].info.parent.and_then(|p| self.index.get(&p).copied())
    }

    fn ancestor(&self, at: u32, up: u8) -> Option<u32> {
        let mut cur = at;
        for _ in 0..up {
            cur = self.parent(cur)?;
        }
        Some(cur)
    }

    fn child(&self, at: u32, name: N) -> Option<u32> {
        let m = self.nodes[at as usize].decls.mods.iter().find(|m| m.name == name)?;
        self.index.get(&m.module).copied()
    }

    /// The path of the named import `at` binds `name` by.
    fn alias(&self, at: u32, name: N) -> Option<&Path<'f, N>> {
        let decls = &self.nodes[at as usize].decls;
        decls.uses.iter().find_map(|u| match u.leaf {
            UseForm::Named { rename: Rename::To(n), .. } if n == name => Some(&u.path),
            UseForm::Named { rename: Rename::Same, .. } if u.path.segments.last() == Some(&name) => Some(&u.path),
            _ => None,
        })
    }

    /// Whether `at`'s own text binds `name` (an item, a `mod`, a named import).
    fn declares(&self, at: u32, name: N) -> bool {
        let decls = &self.nodes[at as usize].decls;
        decls.items.iter().any(|i| i.name == name) || decls.mods.iter().any(|m| m.name == name) || self.alias(at, name).is_some()
    }

    /// A name not bound by `at`'s text: it may come through its globs, or the prelude.
    fn fall_through(&self, at: u32, found: &mut Found<'_>) {
        found.closures.push(at);
        if let Some(p) = self.prelude {
            found.closures.push(p);
        }
    }

    /// Walks `path` written in module `from`, recording what it reads; the module it ends in
    /// when the text decides it.
    fn walk(&self, from: u32, path: &Path<'_, N>, site: Site, last: Last, found: &mut Found<'_>, alloc: Alloc<'_>) -> Option<u32> {
        // The segments still to walk, the next one last.
        let mut rest: AVec<'_, N> = AVec::with_capacity_in(path.segments.len(), alloc);
        rest.extend(path.segments.iter().rev().copied());
        let mut kind = path.kind;
        let mut ctx = from;
        for _ in 0..MOST_HOPS {
            let start = match kind {
                PathKind::Crate => self.root,
                PathKind::SelfMod => Some(ctx),
                PathKind::Super(up) => self.ancestor(ctx, up),
                PathKind::Abs if self.edition == Edition::E2015 => self.root,
                PathKind::Abs => None,
                PathKind::DollarCrate(k) if k == self.here => self.root,
                PathKind::DollarCrate(_) => None,
                PathKind::Plain if site == Site::Use && self.edition == Edition::E2015 => self.root,
                PathKind::Plain => {
                    // The first segment, looked up in `ctx`'s scope.
                    let first = rest.pop()?;
                    if rest.is_empty() && last == Last::Name {
                        if !self.declares(ctx, first) {
                            self.fall_through(ctx, found);
                        }
                        return Some(ctx);
                    }
                    if let Some(child) = self.child(ctx, first) {
                        Some(child)
                    } else if let Some(alias) = self.alias(ctx, first) {
                        rest.extend(alias.segments.iter().rev().copied());
                        kind = alias.kind;
                        continue;
                    } else if self.externs.binary_search(&first).is_ok() {
                        return None;
                    } else {
                        self.fall_through(ctx, found);
                        return None;
                    }
                }
            };
            let mut cur = start?;
            loop {
                let Some(segment) = rest.pop() else {
                    // A glob's source, or a path that names a module.
                    found.reads.push(cur);
                    if last == Last::Module {
                        found.closures.push(cur);
                    }
                    return Some(cur);
                };
                found.reads.push(cur);
                if rest.is_empty() && last == Last::Name {
                    if !self.declares(cur, segment) {
                        found.closures.push(cur);
                    }
                    return Some(cur);
                }
                if let Some(child) = self.child(cur, segment) {
                    cur = child;
                    continue;
                }
                if let Some(alias) = self.alias(cur, segment) {
                    rest.extend(alias.segments.iter().rev().copied());
                    kind = alias.kind;
                    ctx = cur;
                    break;
                }
                // An item (an enum, a trait) or a name through a glob: read at run time.
                found.closures.push(cur);
                return None;
            }
        }
        None
    }

    fn vis(&self, at: u32, vis: &WrittenVis<Path<'_, N>>, found: &mut Found<'_>, alloc: Alloc<'_>) {
        if let WrittenVis::In(path) | WrittenVis::WithoutIn(path) = vis {
            self.walk(at, path, Site::Use, Last::Module, found, alloc);
        }
    }
}

/// The read graph of `modules` (one crate's, from their declarations records and the crate's
/// root record) and its components in dependency order.
pub fn prefetch<'a, N: Name, F: ScopeFacts<Name = N>>(modules: &[ItemKey], facts: &F, alloc: Alloc<'a>) -> Result<Prefetch<'a>, SurfaceError> {
    let n = modules.len();
    let mut out = Prefetch { components: AVec::new_in(alloc), reads: AVec::new_in(alloc), edges: 0 };
    let Some(&first) = modules.first() else { return Ok(out) };
    let here = facts.module(first)?.krate;
    let root = facts.crate_root(here)?;
    let mut index = HashMap::new_in(alloc);
    let mut nodes = AVec::with_capacity_in(n, alloc);
    for (at, &m) in modules.iter().enumerate() {
        index.insert(m, u32::try_from(at).expect("a crate's modules fit a u32"));
        nodes.push(Node { info: facts.module(m)?, decls: facts.decls(m)? });
    }
    let mut externs = AVec::with_capacity_in(root.extern_prelude.len(), alloc);
    externs.extend(root.extern_prelude.iter().map(|(name, _)| *name));
    externs.sort_unstable();
    let tree = Tree {
        root: index.get(&root.head.root_module).copied(),
        prelude: root.head.prelude.and_then(|p| index.get(&p).copied()),
        edition: root.head.edition,
        here,
        externs,
        nodes,
        index,
    };

    // Each module's reads, and the glob sources its glob imports name.
    let mut reads: AVec<'_, AVec<'_, u32>> = AVec::with_capacity_in(n, alloc);
    let mut closures: AVec<'_, AVec<'_, u32>> = AVec::with_capacity_in(n, alloc);
    let mut sources: AVec<'_, AVec<'_, u32>> = AVec::with_capacity_in(n, alloc);
    for at in 0..n {
        let at32 = at as u32;
        let node = &tree.nodes[at];
        let mut found = Found { reads: AVec::new_in(alloc), closures: AVec::new_in(alloc) };
        let mut globs = AVec::new_in(alloc);
        for u in node.decls.uses.iter() {
            match u.leaf {
                UseForm::Glob => {
                    if let Some(source) = tree.walk(at32, &u.path, Site::Use, Last::Module, &mut found, alloc) {
                        globs.push(source);
                    }
                }
                UseForm::Named { .. } => {
                    tree.walk(at32, &u.path, Site::Use, Last::Name, &mut found, alloc);
                }
            }
            tree.vis(at32, &u.vis, &mut found, alloc);
        }
        let mut textual = false;
        for call in node.decls.calls.iter() {
            let one = call.path.kind == PathKind::Plain && call.path.segments.len() == 1;
            if one && call.kind == CallShape::FnLike {
                let name = call.path.segments[0];
                let local = node.decls.macro_rules.iter().any(|m| m.name == name && m.pos < call.pos);
                if !local {
                    textual = true;
                }
            }
            tree.walk(at32, &call.path, Site::Other, Last::Name, &mut found, alloc);
        }
        if textual {
            // `entry`: each ancestor's textual scope; and each `#[macro_use]` child's.
            let mut up = tree.parent(at32);
            for _ in 0..MAX_MODULE_DEPTH {
                let Some(p) = up else { break };
                found.reads.push(p);
                up = tree.parent(p);
            }
            for m in node.decls.mods.iter().filter(|m| m.macro_use) {
                if let Some(&c) = tree.index.get(&m.module) {
                    found.reads.push(c);
                }
            }
        }
        for item in node.decls.items.iter() {
            tree.vis(at32, &item.vis, &mut found, alloc);
        }
        for m in node.decls.mods.iter() {
            tree.vis(at32, &m.vis, &mut found, alloc);
        }
        reads.push(found.reads);
        closures.push(found.closures);
        sources.push(globs);
    }

    // A module's glob closure: itself and, through glob sources, every module a name read
    // there may come from. Computed once per module, by an explicit walk.
    let mut closure_of: AVec<'_, Option<AVec<'_, u32>>> = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        closure_of.push(None);
    }
    let mut seen: AVec<'_, u32> = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        seen.push(u32::MAX);
    }
    let mut stack: AVec<'_, u32> = AVec::new_in(alloc);
    for at in 0..n {
        for c in closures[at].iter() {
            let c = *c as usize;
            if closure_of[c].is_some() {
                continue;
            }
            let mut set = AVec::new_in(alloc);
            stack.push(c as u32);
            seen[c] = c as u32;
            while let Some(v) = stack.pop() {
                set.push(v);
                for &w in sources[v as usize].iter() {
                    if seen[w as usize] != c as u32 {
                        seen[w as usize] = c as u32;
                        stack.push(w);
                    }
                }
            }
            closure_of[c] = Some(set);
        }
    }

    // The edges, without self-loops or repeats.
    let mut edges: AVec<'_, AVec<'_, usize>> = AVec::with_capacity_in(n, alloc);
    for at in 0..n {
        let mut to: AVec<'_, usize> = AVec::new_in(alloc);
        to.extend(reads[at].iter().map(|v| *v as usize));
        for c in closures[at].iter() {
            if let Some(set) = &closure_of[*c as usize] {
                to.extend(set.iter().map(|v| *v as usize));
            }
        }
        to.retain(|v| *v != at);
        to.sort_unstable();
        to.dedup();
        edges.push(to);
    }
    out.edges = edges.iter().map(|e| e.len() as u64).sum();

    // Tarjan gives each component after every component it reaches: dependency order.
    let components = sccs(&edges, alloc);
    let mut component_of: AVec<'_, u32> = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        component_of.push(u32::MAX);
    }
    for (c, component) in components.iter().enumerate() {
        for &v in component.iter() {
            component_of[v] = c as u32;
        }
    }
    for (c, component) in components.iter().enumerate() {
        let mut keys = AVec::with_capacity_in(component.len(), alloc);
        keys.extend(component.iter().map(|&v| modules[v]));
        keys.sort_unstable();
        let mut before = AVec::new_in(alloc);
        for &v in component.iter() {
            before.extend(edges[v].iter().map(|&w| component_of[w]).filter(|&d| d != c as u32));
        }
        before.sort_unstable();
        before.dedup();
        out.components.push(keys);
        out.reads.push(before);
    }
    Ok(out)
}

/// The strongly connected components of a graph over `0..edges.len()` (Tarjan's, with an
/// explicit stack), each after every component it reaches.
pub(crate) fn sccs<'a>(edges: &[AVec<'_, usize>], alloc: Alloc<'a>) -> AVec<'a, AVec<'a, usize>> {
    let n = edges.len();
    let mut index = AVec::with_capacity_in(n, alloc);
    let mut low = AVec::with_capacity_in(n, alloc);
    let mut on = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        index.push(usize::MAX);
        low.push(0usize);
        on.push(false);
    }
    let mut stack: AVec<'_, usize> = AVec::new_in(alloc);
    let mut call: AVec<'_, (usize, usize)> = AVec::new_in(alloc);
    let mut out = AVec::new_in(alloc);
    let mut next = 0usize;
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        call.push((root, 0));
        while let Some(&(v, ei)) = call.last() {
            if ei < edges[v].len() {
                let w = edges[v][ei];
                if let Some(top) = call.last_mut() {
                    top.1 += 1;
                }
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    call.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            call.pop();
            if let Some(&(p, _)) = call.last() {
                low[p] = low[p].min(low[v]);
            }
            if low[v] == index[v] {
                let mut component = AVec::new_in(alloc);
                while let Some(w) = stack.pop() {
                    on[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                out.push(component);
            }
        }
    }
    out
}
