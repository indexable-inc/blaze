//! Lookups and the plan of a pass: a name in a module's scope (its own, a fellow's live, or
//! another's by a read), a path's first segment through the lexical layers, a path, the
//! `macro_rules!` textual scope, and the resolution of each import, glob and call.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::cmp::Ordering;

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{
    AmbiguityKind, BindingState, CallShape, Def, DefKind, Edition, Namespace, Origin, Path,
    PathKind, TextualDef, Unresolved, Visibility, VisibilityClass,
};

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::read::{NameAnswer, Read};
use crate::resolver::{
    Amb, At, CallState, Cached, Env, Hit, ImportOutcome, Item, Layer, NsRes, Plan, Query, Res,
    Resolver, Scan, Sink, Slot, Source, MAX_MODULE_DEPTH,
};
use crate::scope::SeenFrom;

/// What a path is resolved for: a 2015 `use` path begins at the crate root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PathUse {
    Import,
    Macro,
}

/// A path's first segment, looked up through the lexical layers.
#[derive(Clone, Copy)]
pub(crate) struct LexQuery<N> {
    pub(crate) ns: Namespace,
    pub(crate) name: N,
    pub(crate) at: At,
    /// The macro sub-namespace a macro path needs (rustc's `sub_namespace_match`).
    pub(crate) want: Option<CallShape>,
    /// Whether the `macro_rules!` textual scope is searched (a one-segment bang path).
    pub(crate) textual: bool,
    pub(crate) ignore: Option<(ItemKey, u32)>,
}

/// A path's prefix, resolved to a module or an enum.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Prefix {
    Found { hit: Hit, first: Option<Def> },
    Fail(u32, Unresolved),
    Undet,
    Wait,
}

/// What the globs of a module give one name.
#[derive(Clone, Copy)]
pub(crate) struct GlobFound {
    pub(crate) found: Option<(At, Hit)>,
    pub(crate) undet: bool,
    pub(crate) wait: bool,
}

/// A walk back through textual scope.
enum Tex {
    Hit(Hit),
    /// Ran off the start of what was walked: go on outward.
    Start,
    Undet,
    Wait,
}

pub(crate) struct GlobOutcome<'a> {
    pub(crate) source: Option<Source<'a>>,
    pub(crate) first: Option<Def>,
    pub(crate) wait: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct CallOutcome {
    pub(crate) state: Option<CallState>,
    pub(crate) first: Option<Def>,
    pub(crate) wait: bool,
}

/// Whether an attribute is a derive's helper.
enum Helper {
    Is,
    Not,
    Undet,
    Wait,
}

fn seg(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

/// Computes the group's least fixpoint for `key` into `sink.memo`: for each fellow `f` and
/// each member `i` with a glob of `f`, what `f` binds the name to as `i` sees it, each value
/// from the table's last values, from none, until a round changes nothing. A round past the
/// bound sets `sink.overrun`, which the pass stops on.
fn group_fixpoint<'a, N: Name>(
    key: (Namespace, N, Option<(ItemKey, u32)>),
    env: Env<'_, 'a, N>,
    sink: &mut Sink<'a, N>,
    alloc: Alloc<'a>,
) {
    let mut edges: AVec<'a, (usize, usize)> = Vec::new_in(alloc);
    for (i, m) in env.members.iter().enumerate() {
        for g in m.globs.iter() {
            if let Source::Resolved { def, .. } = &g.source {
                if let Some(f) = env.index(def.key) {
                    if f != i {
                        edges.push((f, i));
                    }
                }
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    let mut table = Vec::with_capacity_in(edges.len(), alloc);
    table.extend(edges.iter().map(|&(f, i)| (f, i, Res::None)));
    sink.memo.insert(key, table);
    let (ns, name, ignore) = key;
    // Each value only rises (none, a binding, a wider or ambiguous one, undetermined), so a
    // few rounds per edge settle it.
    let bound = 4 * edges.len() + 4;
    for _ in 0..bound {
        let mut changed = false;
        for (k, &(f, i)) in edges.iter().enumerate() {
            let q = Query { asker: env.members[i].asker(), restricted: false, ignore };
            let r = env.members[f].lookup_module(ns, name, q, env, sink);
            if let Some(table) = sink.memo.get_mut(&key) {
                if table[k].2 != r {
                    table[k].2 = r;
                    changed = true;
                }
            }
        }
        if !changed {
            return;
        }
    }
    sink.overrun = true;
}

impl<'a, N: Name> Resolver<'a, N> {
    /// Whether a call may still add names, and whether one pending waits on an answer.
    fn calls_state(&self) -> (bool, bool) {
        let mut unexpanded = false;
        let mut wait = false;
        for c in self.calls.iter() {
            unexpanded |= c.state.unexpanded();
            wait |= c.state == CallState::Pending && c.blocked;
        }
        (unexpanded, wait)
    }

    /// The binding of `name` in `ns` of this machine's module, as `q` sees it (rustc's
    /// `resolve_ident_in_module_unadjusted`): a declaration, else a named import, else the
    /// globs, with rustc's determinacy.
    pub(crate) fn lookup_module(
        &self,
        ns: Namespace,
        name: N,
        q: Query<'_>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Res {
        if let Some(e) = self.defs.get(&(ns, name)) {
            return Res::Found(e.hit());
        }
        let mut best: Option<(At, Hit)> = None;
        let mut undet = false;
        let mut wait = false;
        if let Some(list) = self.named.get(&name) {
            for &i in list.iter() {
                if q.ignore == Some((self.module, i)) {
                    continue;
                }
                let imp = &self.imports[i as usize];
                if !imp.imports(ns) || imp.failed.is_some() {
                    continue;
                }
                match imp.ns[ns.index()] {
                    NsRes::Found(target) => {
                        let hit = Hit {
                            def: target.def,
                            vis: Visibility::narrower(imp.vis, target.vis, self.asker()),
                            origin: Origin::Import,
                            expansion: imp.expansion,
                            frag: Some(imp.at.frag),
                            state: target.state,
                            layer: Layer::Module,
                        };
                        let earlier = match best {
                            None => true,
                            Some((at, _)) => self.tree_cmp(imp.at, at) == Ordering::Less,
                        };
                        if earlier {
                            best = Some((imp.at, hit));
                        }
                    }
                    NsRes::Pending => {
                        // rustc's `single_import_can_define_name`, for an import the asker sees.
                        if imp.vis.visible_from(q.asker) {
                            if imp.blocked { wait = true } else { undet = true }
                        }
                    }
                    NsRes::Absent | NsRes::Private(_) => {}
                }
            }
        }
        if let Some((_, hit)) = best {
            return Res::Found(hit);
        }
        if wait {
            return Res::Wait;
        }
        if undet {
            return Res::Undet { weak: false };
        }
        let g = self.glob_lookup(ns, name, q, env, sink);
        let (unexpanded, calls_wait) = self.calls_state();
        match g.found {
            Some((_, hit)) => {
                if g.wait {
                    Res::Wait
                } else if q.restricted || (!g.undet && (ns == Namespace::Macros || !unexpanded)) {
                    Res::Found(hit)
                } else if calls_wait {
                    Res::Wait
                } else {
                    Res::Undet { weak: false }
                }
            }
            None => {
                if g.wait || calls_wait {
                    Res::Wait
                } else if g.undet || unexpanded {
                    Res::Undet { weak: true }
                } else {
                    Res::None
                }
            }
        }
    }

    /// What this module's globs give `name` in `ns`: what each source binds that this module
    /// may name, merged (one target under several globs is one binding, as visible as the
    /// widest; two targets are ambiguous, an error only where used).
    pub(crate) fn glob_lookup(
        &self,
        ns: Namespace,
        name: N,
        q: Query<'_>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> GlobFound {
        let mut out = GlobFound { found: None, undet: false, wait: false };
        let importer = self.asker();
        for (gi, g) in self.globs.iter().enumerate() {
            let (def, clamp, classes) = match &g.source {
                Source::Pending => {
                    if g.vis.visible_from(q.asker) {
                        if g.blocked {
                            out.wait = true;
                        } else {
                            out.undet = true;
                            self.note_undet(gi, sink);
                        }
                    }
                    continue;
                }
                Source::Itself | Source::Failed(..) => continue,
                Source::Resolved { def, clamp, classes } => (*def, *clamp, classes),
            };
            let cand = if let Some(f) = env.index(def.key) {
                if def.key == self.module {
                    None
                } else {
                    match self.fellow_res(f, ns, name, q.ignore, env, sink) {
                        Res::Found(hit) if hit.vis.visible_from(importer) => Some((hit.def, hit.vis, hit.state)),
                        Res::Found(_) | Res::None => None,
                        Res::Undet { .. } => {
                            out.undet = true;
                            self.note_undet(gi, sink);
                            None
                        }
                        Res::Wait => {
                            out.wait = true;
                            None
                        }
                    }
                }
            } else {
                let mut found = None;
                for &class in classes.iter() {
                    match self.cached(Read::Exports { module: def.key, class }, sink) {
                        Some(Cached::Exports { entries, .. }) => {
                            if let Ok(at) = entries.binary_search_by(|e| (e.ns, e.name).cmp(&(ns, name))) {
                                let e = entries[at];
                                if e.vis.visible_from(importer) {
                                    found = Some((e.def, e.vis, e.state));
                                }
                            }
                        }
                        Some(Cached::Name(_) | Cached::Textual { .. } | Cached::Helpers { .. } | Cached::InProgress)
                        | None => {
                            out.wait = true;
                        }
                    }
                }
                found
            };
            let Some((d, vis, state)) = cand else { continue };
            if state == BindingState::Undetermined {
                out.undet = true;
                self.note_undet(gi, sink);
                continue;
            }
            let mut vis = Visibility::narrower(g.vis, vis, importer);
            if let Some(c) = clamp {
                vis = Visibility::narrower(vis, c, importer);
            }
            let hit = Hit {
                def: d,
                vis,
                origin: Origin::Glob,
                expansion: g.expansion,
                frag: Some(g.at.frag),
                state,
                layer: Layer::Module,
            };
            out.found = Some(match out.found {
                None => (g.at, hit),
                Some((at0, h0)) => {
                    let (at, mut keep) =
                        if self.tree_cmp(g.at, at0) == Ordering::Less { (g.at, hit) } else { (at0, h0) };
                    if h0.def == hit.def {
                        keep.vis = Visibility::wider(h0.vis, hit.vis, importer);
                        if let BindingState::Ambiguous(k) = hit.state {
                            keep.state = BindingState::Ambiguous(k);
                        }
                        if let BindingState::Ambiguous(k) = h0.state {
                            keep.state = BindingState::Ambiguous(k);
                        }
                    } else {
                        keep.state = BindingState::Ambiguous(AmbiguityKind::GlobVsGlob);
                    }
                    (at, keep)
                }
            });
        }
        out
    }

    /// Keeps glob `g` as the one that left a name undetermined, if it is this module's
    /// earliest so far (a fellow's note, made inside this lookup, gives way to this module's).
    fn note_undet(&self, g: usize, sink: &mut Sink<'a, N>) {
        let at = self.globs[g].at;
        let keep = match sink.undet_glob {
            Some((m, at0, _)) if m == self.module => self.tree_cmp(at, at0) == Ordering::Less,
            Some(_) | None => true,
        };
        if keep {
            sink.undet_glob = Some((self.module, at, seg(g)));
        }
    }

    /// What fellow `f` binds `name` in `ns` to, as this module sees it through a glob: the
    /// group's least fixpoint for the name, computed once per pass (each member's value from
    /// the others' last values, from none, until nothing changes), so a cycle of globs binds
    /// what is bound somewhere on it and nothing more, in time polynomial in the group.
    fn fellow_res(
        &self,
        f: usize,
        ns: Namespace,
        name: N,
        ignore: Option<(ItemKey, u32)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Res {
        let Some(me) = env.index(self.module) else {
            // Only a member looks up through a fellow: a machine's own bug, stopped on.
            sink.overrun = true;
            return Res::Wait;
        };
        let key = (ns, name, ignore);
        if !sink.memo.contains_key(&key) {
            group_fixpoint(key, env, sink, self.alloc);
        }
        let found = sink.memo.get(&key).and_then(|table| {
            table.binary_search_by(|&(tf, ti, _)| (tf, ti).cmp(&(f, me))).ok().map(|k| table[k].2)
        });
        match found {
            Some(r) => r,
            None => {
                sink.overrun = true;
                Res::Wait
            }
        }
    }

    /// The binding of `name` in `ns` of the module or enum `def`, from this module.
    pub(crate) fn lookup_in(
        &self,
        def: Def,
        ns: Namespace,
        name: N,
        ignore: Option<(ItemKey, u32)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Res {
        let q = Query { asker: self.asker(), restricted: false, ignore };
        if def.key == self.module {
            return self.lookup_module(ns, name, q, env, sink);
        }
        if let Some(f) = env.fellow(def.key) {
            return match f.lookup_module(ns, name, q, env, sink) {
                Res::Found(hit) => {
                    let mut hit = hit;
                    hit.frag = None;
                    hit.layer = Layer::Path;
                    Res::Found(hit)
                }
                other => other,
            };
        }
        self.read_name(def.key, ns, name, Layer::Path, sink)
    }

    /// The binding of `name` in `ns` of `module` as a lexical layer (the standard prelude, a
    /// `#[macro_use]` crate root) sees it: this module's or a fellow's live, another's by a
    /// `Name` read. A prelude may be a module of the asker's own cycle (`core`'s is), and a
    /// member is never asked for: its answer would be `InProgress` for good.
    fn layer_name(&self, module: ItemKey, ns: Namespace, name: N, layer: Layer, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let live = if module == self.module { Some(self) } else { env.fellow(module) };
        let Some(f) = live else { return self.read_name(module, ns, name, layer, sink) };
        let q = Query { asker: self.asker(), restricted: false, ignore: None };
        match f.lookup_module(ns, name, q, env, sink) {
            Res::Found(hit) => Res::Found(Hit { origin: Origin::Prelude, expansion: 0, frag: None, layer, ..hit }),
            other => other,
        }
    }

    /// A `Name` read of another module.
    fn read_name(&self, module: ItemKey, ns: Namespace, name: N, layer: Layer, sink: &mut Sink<'a, N>) -> Res {
        match self.cached(Read::Name { module, ns, name }, sink) {
            Some(Cached::Name(NameAnswer::Absent)) => Res::None,
            Some(Cached::Name(NameAnswer::Found { def, vis, state })) => {
                if *state == BindingState::Undetermined {
                    Res::Undet { weak: false }
                } else {
                    Res::Found(Hit {
                        def: *def,
                        vis: *vis,
                        origin: Origin::Prelude,
                        expansion: 0,
                        frag: None,
                        state: *state,
                        layer,
                    })
                }
            }
            Some(Cached::Exports { .. } | Cached::Textual { .. } | Cached::Helpers { .. } | Cached::InProgress)
            | None => Res::Wait,
        }
    }

    fn extern_prelude_hit(&self, name: N) -> Option<Hit> {
        let at = self.extern_prelude.binary_search_by(|(n, _)| n.cmp(&name)).ok()?;
        Some(Hit {
            def: Def::module(self.extern_prelude[at].1),
            vis: Visibility::Public,
            origin: Origin::Prelude,
            expansion: 0,
            frag: None,
            state: BindingState::Determined,
            layer: Layer::ExternPrelude,
        })
    }

    /// The module a path that does not begin with a name begins at.
    pub(crate) fn start_module(&self, path: &Path<'_, N>) -> Result<Hit, (u32, Unresolved)> {
        match path.kind {
            PathKind::Crate => Ok(Hit::module(self.root)),
            PathKind::SelfMod => Ok(Hit::module(self.module)),
            PathKind::Super(n) => match self.chain.get(usize::from(n)) {
                Some(&m) => Ok(Hit::module(m)),
                None => Err((0, Unresolved::NoParent)),
            },
            PathKind::DollarCrate(k) => {
                if k == self.krate {
                    Ok(Hit::module(self.root))
                } else {
                    match self.deps.iter().find(|d| d.krate == k) {
                        Some(d) => Ok(Hit::module(d.root)),
                        None => Err((0, Unresolved::NoCrate)),
                    }
                }
            }
            PathKind::Plain | PathKind::Abs => Err((0, Unresolved::NotFound)),
        }
    }

    /// A path's first segment, for a path that begins with a name.
    pub(crate) fn first_segment(
        &self,
        kind: PathKind,
        used: PathUse,
        ns: Namespace,
        name: N,
        at: At,
        ignore: Option<(ItemKey, u32)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Res {
        let from_root = self.edition == Edition::E2015
            && (kind == PathKind::Abs || (kind == PathKind::Plain && used == PathUse::Import));
        if from_root {
            return self.lookup_in(Def::module(self.root), ns, name, ignore, env, sink);
        }
        if kind == PathKind::Abs {
            return match (ns, self.extern_prelude_hit(name)) {
                (Namespace::Types, Some(hit)) => Res::Found(hit),
                _ => Res::None,
            };
        }
        self.lexical(LexQuery { ns, name, at, want: None, textual: false, ignore }, env, sink)
    }

    /// A first segment through the lexical layers, innermost first: the `macro_rules!` textual
    /// scope, the module, the extern prelude, the `#[macro_use]` prelude, the standard
    /// prelude (rustc's `early_resolve_ident_in_lexical_scope`). Early, the first binding wins
    /// and a weak undeterminacy goes on outward; in the final pass the outer layers are read
    /// too where they could make the innermost ambiguous, and the ambiguity goes to the sink.
    pub(crate) fn lexical(&self, lq: LexQuery<N>, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let mut inner: Option<Hit> = None;
        let mut undet = false;
        if lq.textual && lq.ns == Namespace::Macros {
            let r = self.textual(lq.name, lq.at, env, sink);
            if let Some(stop) = self.layer(r, &lq, &mut inner, &mut undet, sink) {
                return stop;
            }
        }
        let q = Query { asker: self.asker(), restricted: true, ignore: lq.ignore };
        let r = self.lookup_module(lq.ns, lq.name, q, env, sink);
        if sink.scan == Scan::Final && sink.amb.is_none() {
            if let Res::Found(hit) = r {
                // rustc's "forbid expanded shadowing to avoid time travel".
                if hit.origin != Origin::Glob && hit.expansion > 0 && self.kind_ok(&hit, lq.want) {
                    if let Some((_, glob)) = self.glob_lookup(lq.ns, lq.name, q, env, sink).found {
                        if glob.def != hit.def {
                            sink.amb = Some(Amb {
                                ns: lq.ns,
                                name: lq.name,
                                kind: AmbiguityKind::GlobVsExpanded,
                                first: hit.def,
                                second: glob.def,
                            });
                        }
                    }
                }
            }
        }
        if let Some(stop) = self.layer(r, &lq, &mut inner, &mut undet, sink) {
            return stop;
        }
        if self.implicit_prelude {
            if lq.ns == Namespace::Types {
                let r = self.extern_prelude_hit(lq.name).map_or(Res::None, Res::Found);
                if let Some(stop) = self.layer(r, &lq, &mut inner, &mut undet, sink) {
                    return stop;
                }
            }
            if lq.ns == Namespace::Macros {
                let mut r = Res::None;
                for &root in self.macro_use.iter() {
                    match self.layer_name(root, Namespace::Macros, lq.name, Layer::MacroUsePrelude, env, sink) {
                        Res::None => {}
                        other => {
                            r = other;
                            break;
                        }
                    }
                }
                if let Some(stop) = self.layer(r, &lq, &mut inner, &mut undet, sink) {
                    return stop;
                }
            }
            if let Some(prelude) = self.prelude {
                let r = self.layer_name(prelude, lq.ns, lq.name, Layer::StdPrelude, env, sink);
                if let Some(stop) = self.layer(r, &lq, &mut inner, &mut undet, sink) {
                    return stop;
                }
            }
        }
        match inner {
            Some(hit) => Res::Found(hit),
            None if undet => Res::Undet { weak: true },
            None => Res::None,
        }
    }

    fn kind_ok(&self, hit: &Hit, want: Option<CallShape>) -> bool {
        match want {
            None => true,
            Some(k) => hit.def.kind == DefKind::Macro(k),
        }
    }

    /// One layer's result: `Some` to stop the lexical walk with it.
    fn layer(
        &self,
        r: Res,
        lq: &LexQuery<N>,
        inner: &mut Option<Hit>,
        undet: &mut bool,
        sink: &mut Sink<'a, N>,
    ) -> Option<Res> {
        match r {
            Res::Found(hit) if self.kind_ok(&hit, lq.want) => match *inner {
                None => {
                    if sink.scan != Scan::Final || !self.may_be_ambiguous(&hit, lq.at) {
                        return Some(Res::Found(hit));
                    }
                    *inner = Some(hit);
                    None
                }
                Some(first) => {
                    if hit.def != first.def {
                        if let Some(kind) = self.ambiguity(&first, &hit, lq.at) {
                            if sink.amb.is_none() {
                                sink.amb = Some(Amb { ns: lq.ns, name: lq.name, kind, first: first.def, second: hit.def });
                            }
                            return Some(Res::Found(first));
                        }
                    }
                    None
                }
            },
            Res::Found(_) | Res::None => None,
            Res::Undet { weak: true } => {
                *undet = true;
                None
            }
            Res::Undet { weak: false } => {
                if inner.is_some() { None } else { Some(Res::Undet { weak: false }) }
            }
            Res::Wait => Some(Res::Wait),
        }
    }

    /// Whether some outer binding could make the innermost `hit` ambiguous: only then are the
    /// outer layers read in the final pass.
    fn may_be_ambiguous(&self, hit: &Hit, at: At) -> bool {
        (hit.layer == Layer::Textual && hit.frag.is_none())
            || hit.origin == Origin::Glob
            || match hit.frag {
                Some(f) => self.frags[f as usize].expansion > 0 && !self.is_desc(at.frag, f),
                None => false,
            }
    }

    /// rustc's ambiguity kind of an innermost binding and a differing outer one, if any.
    fn ambiguity(&self, inner: &Hit, outer: &Hit, at: At) -> Option<AmbiguityKind> {
        if inner.layer == Layer::Textual && outer.layer == Layer::Module && inner.frag.is_none() {
            // A `macro_rules!` of another module against this module's path-based macro
            // (rustc's `disambiguate_macro_rules_vs_modularized` admits only one module's).
            return Some(AmbiguityKind::MacroRulesVsModularized);
        }
        if inner.origin == Origin::Glob {
            return Some(AmbiguityKind::GlobVsOuter);
        }
        let f = inner.frag?;
        if self.frags[f as usize].expansion == 0 {
            return None;
        }
        // rustc's `may_appear_after`: neither the outer binding nor the site is inside the
        // expansion the inner binding came from.
        let before_other = outer.frag.is_some_and(|o| self.is_desc(o, f));
        let before_site = self.is_desc(at.frag, f);
        if before_other || before_site { None } else { Some(AmbiguityKind::MoreExpandedVsOuter) }
    }

    /// The `macro_rules!` named `name` visible at `at`: back through the fragment, into
    /// expanded calls and `#[macro_use]` children, out through the call a fragment expands,
    /// then the module's entry.
    fn textual(&self, name: N, at: At, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let mut frag = at.frag;
        let mut end = at.slot as usize;
        loop {
            match self.textual_back(name, frag, end, env, sink) {
                Tex::Hit(hit) => return Res::Found(hit),
                Tex::Undet => return Res::Undet { weak: true },
                Tex::Wait => return Res::Wait,
                Tex::Start => {}
            }
            match self.frags[frag as usize].parent {
                Some(p) => {
                    frag = p.frag;
                    end = p.slot as usize;
                }
                None => break,
            }
        }
        self.textual_entry(name, env, sink)
    }

    fn textual_back(&self, name: N, frag: u32, end: usize, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Tex {
        let f = &self.frags[frag as usize];
        for s in (0..end).rev() {
            match f.slots[s] {
                Slot::Rules { name: n, def } if n == name => {
                    return Tex::Hit(Hit {
                        def: Def { key: def, kind: DefKind::Macro(CallShape::FnLike) },
                        vis: self.restricted(self.module),
                        origin: Origin::Declared,
                        expansion: f.expansion,
                        frag: Some(frag),
                        state: BindingState::Determined,
                        layer: Layer::Textual,
                    });
                }
                Slot::Call(i) => {
                    let call = &self.calls[i as usize];
                    match call.state {
                        CallState::Expanded { frag: inner, .. } => {
                            let len = self.frags[inner as usize].slots.len();
                            match self.textual_back(name, inner, len, env, sink) {
                                Tex::Start => {}
                                other => return other,
                            }
                        }
                        CallState::Pending => return if call.blocked { Tex::Wait } else { Tex::Undet },
                        CallState::Resolved(_) => return Tex::Undet,
                        CallState::Inert | CallState::Failed(..) | CallState::TooDeep => {}
                    }
                }
                Slot::Mod { child, macro_use: true } => match self.textual_out(name, child, env, sink) {
                    Tex::Start => {}
                    other => return other,
                },
                Slot::Rules { .. } | Slot::Mod { .. } | Slot::Use(_) | Slot::Glob(_) => {}
            }
        }
        Tex::Start
    }

    fn textual_hit(&self, d: &TextualDef<N>) -> Hit {
        Hit {
            def: Def { key: d.def, kind: DefKind::Macro(CallShape::FnLike) },
            vis: self.restricted(self.module),
            origin: Origin::Declared,
            expansion: d.expansion,
            frag: None,
            state: BindingState::Determined,
            layer: Layer::Textual,
        }
    }

    /// What a `#[macro_use]` child carries out, searched from its end.
    fn textual_out(&self, name: N, child: ItemKey, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Tex {
        if child != self.module {
            if let Some(f) = env.fellow(child) {
                let len = f.frags[0].slots.len();
                return match f.textual_back(name, 0, len, env, sink) {
                    Tex::Hit(hit) => {
                        let mut hit = hit;
                        hit.frag = None;
                        Tex::Hit(hit)
                    }
                    other => other,
                };
            }
        }
        match self.cached(Read::TextualOut { module: child }, sink) {
            Some(Cached::Textual { defs, .. }) => match defs.iter().rev().find(|d| d.name == name) {
                Some(d) => Tex::Hit(self.textual_hit(d)),
                None => Tex::Start,
            },
            Some(Cached::Name(_) | Cached::Exports { .. } | Cached::Helpers { .. } | Cached::InProgress) | None => {
                Tex::Wait
            }
        }
    }

    /// The `macro_rules!` visible at this module's `mod` line.
    fn textual_entry(&self, name: N, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let Some(&parent) = self.chain.get(1) else { return Res::None };
        if let Some(p) = env.fellow(parent) {
            return match p.find_mod(self.module) {
                Some(at) => match p.textual(name, at, env, sink) {
                    Res::Found(hit) => {
                        let mut hit = hit;
                        hit.frag = None;
                        Res::Found(hit)
                    }
                    other => other,
                },
                // The line is in an expansion still to come.
                None => Res::Undet { weak: true },
            };
        }
        match self.cached(Read::TextualEntry { module: self.module }, sink) {
            Some(Cached::Textual { defs, .. }) => match defs.iter().rev().find(|d| d.name == name) {
                Some(d) => Res::Found(self.textual_hit(d)),
                None => Res::None,
            },
            Some(Cached::Name(_) | Cached::Exports { .. } | Cached::Helpers { .. } | Cached::InProgress) | None => {
                Res::Wait
            }
        }
    }

    /// Where the `mod child` line is.
    pub(crate) fn find_mod(&self, child: ItemKey) -> Option<At> {
        for (f, frag) in self.frags.iter().enumerate() {
            for (s, slot) in frag.slots.iter().enumerate() {
                if let Slot::Mod { child: c, .. } = *slot {
                    if c == child {
                        return Some(At { frag: seg(f), slot: seg(s) });
                    }
                }
            }
        }
        None
    }

    /// The first `upto` segments of `path`, resolved to a module or an enum.
    pub(crate) fn resolve_prefix(
        &self,
        path: &Path<'_, N>,
        upto: usize,
        used: PathUse,
        at: At,
        ignore: Option<(ItemKey, u32)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Prefix {
        let force = sink.scan == Scan::Force;
        let segs = &path.segments;
        let (mut cur, mut i, first) = match path.kind {
            PathKind::Plain | PathKind::Abs => {
                let Some(&name) = segs.first() else { return Prefix::Fail(0, Unresolved::NotFound) };
                match self.first_segment(path.kind, used, Namespace::Types, name, at, ignore, env, sink) {
                    Res::Found(hit) => {
                        if let BindingState::Ambiguous(k) = hit.state {
                            return Prefix::Fail(0, Unresolved::Ambiguous(k));
                        }
                        (hit, 1, Some(hit.def))
                    }
                    Res::None => return Prefix::Fail(0, Unresolved::NotFound),
                    Res::Undet { weak } => {
                        return if force {
                            Prefix::Fail(0, if weak { Unresolved::NotFound } else { Unresolved::Undetermined })
                        } else {
                            Prefix::Undet
                        };
                    }
                    Res::Wait => return Prefix::Wait,
                }
            }
            PathKind::Crate | PathKind::SelfMod | PathKind::Super(_) | PathKind::DollarCrate(_) => {
                match self.start_module(path) {
                    Ok(hit) => (hit, 0, None),
                    Err((s, why)) => return Prefix::Fail(s, why),
                }
            }
        };
        while i < upto {
            let s = seg(i);
            if !cur.def.has_scope() {
                return Prefix::Fail(s, Unresolved::NotAModule);
            }
            match self.lookup_in(cur.def, Namespace::Types, segs[i], ignore, env, sink) {
                Res::Found(hit) => {
                    match hit.state {
                        BindingState::Ambiguous(k) => return Prefix::Fail(s, Unresolved::Ambiguous(k)),
                        BindingState::Undetermined => {
                            // Forced, nothing more will decide it: the path fails here.
                            return if force { Prefix::Fail(s, Unresolved::Undetermined) } else { Prefix::Undet };
                        }
                        BindingState::Determined => {}
                    }
                    if !hit.vis.visible_from(self.asker()) {
                        return Prefix::Fail(s, Unresolved::Private);
                    }
                    cur = hit;
                }
                Res::None => return Prefix::Fail(s, Unresolved::NotFound),
                Res::Undet { .. } => {
                    return if force { Prefix::Fail(s, Unresolved::Undetermined) } else { Prefix::Undet };
                }
                Res::Wait => return Prefix::Wait,
            }
            i += 1;
        }
        Prefix::Found { hit: cur, first }
    }

    /// An import's last segment in one namespace.
    fn leaf(&self, r: Res, last: u32, out: &mut ImportOutcome) -> NsRes {
        match r {
            Res::Found(hit) => match hit.state {
                BindingState::Ambiguous(k) => {
                    out.failed = Some((last, Unresolved::Ambiguous(k)));
                    NsRes::Absent
                }
                BindingState::Undetermined => NsRes::Pending,
                BindingState::Determined => {
                    if hit.vis.visible_from(self.asker()) { NsRes::Found(hit) } else { NsRes::Private(hit) }
                }
            },
            Res::None => NsRes::Absent,
            Res::Undet { .. } => NsRes::Pending,
            Res::Wait => {
                out.wait = true;
                NsRes::Pending
            }
        }
    }

    /// Import `i` resolved from the state as it is, every namespace from scratch.
    pub(crate) fn resolve_import(&self, i: usize, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> ImportOutcome {
        let imp = &self.imports[i];
        let ignore = Some((self.module, seg(i)));
        let mut out = ImportOutcome { ns: [NsRes::Pending; 3], failed: None, first: None, wait: false };
        for ns in Namespace::ALL {
            if !imp.imports(ns) {
                out.ns[ns.index()] = NsRes::Absent;
            }
        }
        let n = imp.path.segments.len();
        let last = seg(n.saturating_sub(1));
        let names_first = matches!(imp.path.kind, PathKind::Plain | PathKind::Abs);
        if n == 0 {
            match self.start_module(&imp.path) {
                Ok(hit) => {
                    out.ns = [NsRes::Found(hit), NsRes::Absent, NsRes::Absent];
                }
                Err(fail) => out.failed = Some(fail),
            }
        } else if n == 1 && names_first {
            let name = imp.path.segments[0];
            for ns in Namespace::ALL {
                if imp.imports(ns) {
                    let r = self.first_segment(imp.path.kind, PathUse::Import, ns, name, imp.at, ignore, env, sink);
                    let got = self.leaf(r, last, &mut out);
                    out.ns[ns.index()] = got;
                }
            }
        } else {
            match self.resolve_prefix(&imp.path, n - 1, PathUse::Import, imp.at, ignore, env, sink) {
                Prefix::Found { hit, first } => {
                    out.first = first;
                    if !hit.def.has_scope() {
                        out.failed = Some((seg(n - 1), Unresolved::NotAModule));
                    } else {
                        let name = imp.path.segments[n - 1];
                        for ns in Namespace::ALL {
                            if imp.imports(ns) {
                                let r = self.lookup_in(hit.def, ns, name, ignore, env, sink);
                                let got = self.leaf(r, last, &mut out);
                                out.ns[ns.index()] = got;
                            }
                        }
                    }
                }
                Prefix::Fail(s, why) => out.failed = Some((s, why)),
                Prefix::Undet => {}
                Prefix::Wait => out.wait = true,
            }
        }
        if out.failed.is_some() {
            out.ns = [NsRes::Absent; 3];
        }
        out
    }

    /// The visibility classes of `target` this module can see: what a glob of it reads.
    fn classes_for<F: ScopeFacts<Name = N>>(
        &self,
        target: ItemKey,
        facts: &F,
    ) -> Result<AVec<'a, VisibilityClass>, ResolveError> {
        let mut classes = Vec::new_in(self.alloc.clone());
        classes.push(VisibilityClass::Public);
        if !target.is_of(self.krate) {
            return Ok(classes);
        }
        classes.push(VisibilityClass::Crate);
        let mut cur = Some(target);
        let mut steps = 0usize;
        while let Some(m) = cur {
            if m == self.root {
                break;
            }
            if self.chain.contains(&m) {
                classes.push(VisibilityClass::Restricted(m));
            }
            steps += 1;
            if steps > MAX_MODULE_DEPTH {
                return Err(ResolveError::ModuleTooDeep { module: target });
            }
            cur = facts.module(m).map_err(|error| ResolveError::Surface { module: self.module, error })?.parent;
        }
        Ok(classes)
    }

    /// Glob `g`'s source resolved from the state as it is.
    pub(crate) fn resolve_glob<F: ScopeFacts<Name = N>>(
        &self,
        g: usize,
        facts: &F,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Result<GlobOutcome<'a>, ResolveError> {
        let glob = &self.globs[g];
        let n = glob.path.segments.len();
        let pre = if n == 0 {
            match self.start_module(&glob.path) {
                Ok(hit) => Prefix::Found { hit, first: None },
                Err((s, why)) => Prefix::Fail(s, why),
            }
        } else {
            self.resolve_prefix(&glob.path, n, PathUse::Import, glob.at, None, env, sink)
        };
        Ok(match pre {
            Prefix::Found { hit, first } => {
                let source = if !hit.def.has_scope() {
                    Source::Failed(seg(n.saturating_sub(1)), Unresolved::NotAModule)
                } else if hit.def.key == self.module {
                    Source::Itself
                } else if hit.def.kind == DefKind::Enum {
                    let mut classes = Vec::new_in(self.alloc.clone());
                    classes.push(VisibilityClass::Public);
                    Source::Resolved { def: hit.def, clamp: Some(hit.vis), classes }
                } else {
                    Source::Resolved { def: hit.def, clamp: None, classes: self.classes_for(hit.def.key, facts)? }
                };
                GlobOutcome { source: Some(source), first, wait: false }
            }
            Prefix::Fail(s, why) => GlobOutcome { source: Some(Source::Failed(s, why)), first: None, wait: false },
            Prefix::Undet => GlobOutcome { source: None, first: None, wait: false },
            Prefix::Wait => GlobOutcome { source: None, first: None, wait: true },
        })
    }

    /// Call `c`'s path resolved from the state as it is; forced in a `Force` pass.
    pub(crate) fn resolve_call(&self, c: usize, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> CallOutcome {
        let call = &self.calls[c];
        let force = sink.scan == Scan::Force;
        let segs = &call.path.segments;
        let n = segs.len();
        let fail = |s: u32, why: Unresolved| CallOutcome { state: Some(CallState::Failed(s, why)), first: None, wait: false };
        let stay = CallOutcome { state: None, first: None, wait: false };
        let wait = CallOutcome { state: None, first: None, wait: true };
        if call.kind == CallShape::Attr
            && call.path.kind == PathKind::Plain
            && n >= 2
            && self.implicit_prelude
            && self.tools.contains(&segs[0])
        {
            return CallOutcome { state: Some(CallState::Inert), first: None, wait: false };
        }
        if n == 0 || (n == 1 && call.path.kind != PathKind::Plain) {
            return fail(0, Unresolved::NotFound);
        }
        let inert = CallOutcome { state: Some(CallState::Inert), first: None, wait: false };
        let one_attr = call.kind == CallShape::Attr && n == 1;
        if one_attr {
            // rustc's innermost scope for an attribute: the helpers of the derives before it
            // on its item.
            match self.derive_helper(c, segs[0], force, sink) {
                Helper::Is => return inert,
                Helper::Wait => return wait,
                Helper::Undet => return stay,
                Helper::Not => {}
            }
        }
        let last = seg(n - 1);
        let undetermined = |weak: bool| {
            if force { fail(last, if weak { Unresolved::NotFound } else { Unresolved::Undetermined }) } else { stay }
        };
        let (r, first) = if n == 1 {
            let lq = LexQuery {
                ns: Namespace::Macros,
                name: segs[0],
                at: call.at,
                want: Some(call.kind),
                textual: call.kind == CallShape::FnLike,
                ignore: None,
            };
            (self.lexical(lq, env, sink), None)
        } else {
            match self.resolve_prefix(&call.path, n - 1, PathUse::Macro, call.at, None, env, sink) {
                Prefix::Found { hit, first } => {
                    if !hit.def.has_scope() {
                        return fail(last, Unresolved::NotAModule);
                    }
                    (self.lookup_in(hit.def, Namespace::Macros, segs[n - 1], None, env, sink), first)
                }
                Prefix::Fail(s, why) => return fail(s, why),
                Prefix::Undet => return stay,
                Prefix::Wait => return wait,
            }
        };
        if one_attr && self.builtin_attrs.binary_search(&segs[0]).is_ok() {
            // The built-in attribute scope comes after the `#[macro_use]` prelude: an attribute
            // macro of a nearer scope with its name is ambiguous, one of the standard prelude
            // or none leaves the built-in, which is inert.
            return match r {
                Res::Found(hit)
                    if matches!(hit.layer, Layer::Textual | Layer::Module | Layer::MacroUsePrelude) =>
                {
                    fail(last, Unresolved::Ambiguous(AmbiguityKind::BuiltinAttr))
                }
                Res::Found(_) | Res::None => inert,
                Res::Undet { .. } => {
                    if force { inert } else { stay }
                }
                Res::Wait => wait,
            };
        }
        match r {
            Res::Found(hit) => match hit.state {
                BindingState::Ambiguous(k) => fail(last, Unresolved::Ambiguous(k)),
                BindingState::Undetermined => undetermined(false),
                BindingState::Determined => {
                    if n > 1 && !hit.vis.visible_from(self.asker()) {
                        fail(last, Unresolved::Private)
                    } else if !self.kind_ok(&hit, Some(call.kind)) {
                        fail(last, Unresolved::WrongMacroKind)
                    } else {
                        CallOutcome {
                            state: Some(CallState::Resolved(hit.def)),
                            first: if n == 1 { Some(hit.def) } else { first },
                            wait: false,
                        }
                    }
                }
            },
            Res::None => fail(last, Unresolved::NotFound),
            Res::Undet { weak } => undetermined(weak),
            Res::Wait => wait,
        }
    }

    /// Whether `name` is a helper attribute of a derive written before call `c` on its item.
    fn derive_helper(&self, c: usize, name: N, force: bool, sink: &mut Sink<'a, N>) -> Helper {
        let call = &self.calls[c];
        let Some(item) = call.item else { return Helper::Not };
        let mut wait = false;
        let mut undet = false;
        for d in self.calls.iter() {
            if d.kind != CallShape::Derive
                || d.item != Some(item)
                || d.at.frag != call.at.frag
                || d.at.slot >= call.at.slot
            {
                continue;
            }
            match d.state {
                CallState::Resolved(def) | CallState::Expanded { def, .. } => {
                    match self.cached(Read::DeriveHelpers { derive: def.key }, sink) {
                        Some(Cached::Helpers { names, .. }) => {
                            if names.binary_search(&name).is_ok() {
                                return Helper::Is;
                            }
                        }
                        Some(Cached::Name(_) | Cached::Exports { .. } | Cached::Textual { .. } | Cached::InProgress)
                        | None => wait = true,
                    }
                }
                // Forced, a derive still pending gives no helper: it fails in the same pass.
                CallState::Pending if !force => {
                    if d.blocked { wait = true } else { undet = true }
                }
                CallState::Pending | CallState::Inert | CallState::Failed(..) | CallState::TooDeep => {}
            }
        }
        if wait {
            Helper::Wait
        } else if undet {
            Helper::Undet
        } else {
            Helper::Not
        }
    }

    /// Plans one pass: every import, glob and call that can move, from the state as it is (in
    /// a `Force` pass, only the calls).
    pub(crate) fn plan<F: ScopeFacts<Name = N>>(
        &self,
        facts: &F,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Result<Plan<'a>, ResolveError> {
        let a = self.alloc.clone();
        let mut plan = Plan {
            imports: Vec::new_in(a.clone()),
            globs: Vec::new_in(a.clone()),
            calls: Vec::new_in(a.clone()),
            waiting: Vec::new_in(a),
            upto: self.upto(),
        };
        if sink.scan != Scan::Force {
            for g in 0..self.globs.len() {
                match &self.globs[g].source {
                    Source::Pending => {
                        let o = self.resolve_glob(g, facts, env, sink)?;
                        if o.wait {
                            plan.waiting.push(Item::Glob(seg(g)));
                        }
                        if let Some(source) = o.source {
                            plan.globs.push((seg(g), source, o.first));
                        }
                    }
                    Source::Resolved { def, classes, .. } => {
                        // A glob's source is read whole once known: its names, and the
                        // absence of any other, decide the module's glob bindings.
                        if env.fellow(def.key).is_none() {
                            for &class in classes.iter() {
                                let _ = self.cached(Read::Exports { module: def.key, class }, sink);
                            }
                        }
                    }
                    Source::Itself | Source::Failed(..) => {}
                }
            }
            for i in 0..self.imports.len() {
                let stored = &self.imports[i];
                if stored.done() {
                    continue;
                }
                let o = self.resolve_import(i, env, sink);
                if o.wait {
                    plan.waiting.push(Item::Import(seg(i)));
                }
                let mut ns = stored.ns;
                for k in 0..3 {
                    if ns[k] == NsRes::Pending {
                        ns[k] = o.ns[k];
                    }
                }
                let failed = stored.failed.or(o.failed);
                if ns != stored.ns || failed != stored.failed {
                    plan.imports.push((seg(i), ns, failed, o.first));
                }
            }
        }
        for c in 0..self.calls.len() {
            if self.calls[c].state != CallState::Pending {
                continue;
            }
            let o = self.resolve_call(c, env, sink);
            if o.wait {
                plan.waiting.push(Item::Call(seg(c)));
            }
            if let Some(state) = o.state {
                plan.calls.push((seg(c), state, o.first));
            }
        }
        plan.waiting.sort_unstable();
        Ok(plan)
    }
}
