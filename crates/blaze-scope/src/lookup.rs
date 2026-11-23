//! Lookups and the plan of a pass: a name in a module's scope (its own, a fellow's live, or
//! another's by a read), a path's first segment through the lexical layers, a path, the
//! `macro_rules!` textual scope, and the resolution of each import, glob and call.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::cmp::Ordering;

use blaze_id::ItemKey;
use blaze_surface::{
    AmbiguityKind, BindingState, CallShape, Def, DefKind, Edition, GlobEdge, Namespace, Origin, Path,
    PathKind, TextualDef, Unresolved, Visibility,
};

use crate::error::{GlobBug, ResolveError};
use crate::facts::{Name, ScopeFacts};
use crate::read::{NameAnswer, Read};
use crate::resolver::{
    Amb, At, Blame, CallState, Cached, Env, Hit, ImportOutcome, Layer, NsRes,
    Query, Res, Resolver, Scan, Sink, Slot, Source,
};
use blaze_surface::{Node, Passed, SeenFrom, Sources, Through, through};
use crate::work::Key;

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
    pub(crate) ignore: Option<(ItemKey, Asking)>,
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
    /// A record some glob's walk reads is not given yet: nothing is decided until it is.
    pub(crate) answer: bool,
}

/// A walk back through textual scope.
enum Tex {
    Hit(Hit),
    /// Ran off the start of what was walked: go on outward.
    Start,
    Undet,
    Wait,
}

pub(crate) struct GlobOutcome {
    pub(crate) source: Option<Source>,
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

/// The item of a module whose own path a lookup resolves, which never sees itself (rustc's
/// `ignore_import`): a named import or a glob, by its index among the module's imports or
/// globs. A glob that waited on itself left `use crate::m::m::*` (with `m` reached through
/// `m`'s own `use crate::*`) unresolved (the cold-fill oracle, scope lead 20).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Asking {
    Import(u32),
    Glob(u32),
    /// A call's own path, which does not wait on the call itself.
    Call(u32),
}

/// Why a source of a lookup through globs cannot be read now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Stop {
    /// A record it is read from is not given yet: never an open node (the record is a fact
    /// still to arrive, not an item undecided).
    Answer,
    /// A pending item of a member, blocked on an answer, may still bind it.
    Wait,
    /// A pending item of a member may still bind it, and nothing outstanding decides it.
    Undet,
}

/// The sources of a lookup through globs as a machine sees them: itself by its own bindings
/// (its globs are its own lookup's), a fellow live, any other module or enum by its records
/// (`Name` for its own binding, `Globs` for its edges), each read kept in the read list.
struct Live<'e, 's, 'a, N> {
    at: &'e Resolver<'a, N>,
    ns: Namespace,
    name: N,
    ignore: Option<(ItemKey, Asking)>,
    env: Env<'e, 'a, N>,
    sink: &'s mut Sink<'a, N>,
    /// A source the walk follows has a glob still pending: if the walk finds nothing, it is
    /// undecided (the stronger of the stops met), not absent.
    pend: Option<Stop>,
}

/// The stronger of two stops: an answer outstanding, then a wait, then undetermined.
fn join(a: Option<Stop>, b: Stop) -> Option<Stop> {
    match (a, b) {
        (Some(Stop::Answer), _) | (_, Stop::Answer) => Some(Stop::Answer),
        (Some(Stop::Wait), _) | (_, Stop::Wait) => Some(Stop::Wait),
        (Some(Stop::Undet) | None, Stop::Undet) => Some(Stop::Undet),
    }
}

impl<'e, N> Live<'e, '_, '_, N> {
    /// The walk's result with what its sources still had pending: nothing found and a glob
    /// pending is undecided.
    fn settle(&self, walked: Result<Option<Passed>, Through<Stop>>) -> Result<Option<Passed>, Through<Stop>> {
        match (walked, self.pend) {
            (Ok(None), Some(stop)) => Err(Through::Stop(stop)),
            (Err(Through::Stop(a)), Some(b)) => Err(Through::Stop(join(Some(a), b).unwrap_or(a))),
            (other, _) => other,
        }
    }
}

impl<'e, 'a, N: Name> Sources<'e> for Live<'e, '_, 'a, N> {
    type Stop = Stop;

    /// A member's source held by a pending item gives no value: a binding found elsewhere is
    /// not held by it (the stall rule: never a glob behind a glob once bound). A record not
    /// given yet stops the walk.
    fn open(stop: &Stop) -> bool {
        *stop != Stop::Answer
    }

    fn node(&mut self, source: Def) -> Result<Node<'e>, Stop> {
        let at = self.at;
        let passed = |r: Res| match r {
            Res::Found(hit) => Ok(Node::Value(Some(Passed { def: hit.def, vis: hit.vis, state: hit.state }))),
            Res::None => Ok(Node::Value(None)),
            Res::Undet { .. } => Err(Stop::Undet),
            Res::Wait => Err(Stop::Wait),
        };
        if source.key == at.module {
            self.sink.touch(Key::Name(at.module, self.name));
            let q = Query { asker: at.asker(), restricted: false, ignore: self.ignore };
            return match at.own_lookup(self.ns, self.name, q) {
                Some(r @ (Res::Undet { .. } | Res::Wait)) => {
                    at.blame_named(self.ns, self.name, q, self.sink);
                    passed(r)
                }
                Some(r) => passed(r),
                None => Ok(Node::Value(None)),
            };
        }
        if let Some(f) = self.env.fellow(source.key) {
            return at.member_node(f, self.ns, self.name, self.ignore, &mut self.pend, self.sink);
        }
        let (ns, name) = (self.ns, self.name);
        match at.cached(Read::Name { module: source.key, ns, name }, self.sink) {
            Some(Cached::Name(NameAnswer::Found { def, vis, state, .. })) => {
                #[cfg(test)]
                let vis = if at.plant == crate::resolver::Plant::GlobLeaksPrivate { &Visibility::Public } else { vis };
                return Ok(Node::Value(Some(Passed { def: *def, vis: *vis, state: *state })));
            }
            Some(Cached::Name(NameAnswer::Absent)) if source.kind == DefKind::Enum => return Ok(Node::Value(None)),
            Some(Cached::Name(NameAnswer::Absent)) => {}
            Some(Cached::Globs { .. } | Cached::Textual { .. } | Cached::Helpers { .. } | Cached::InProgress(_)) | None => {
                return Err(Stop::Answer);
            }
        }
        match at.cached(Read::Globs { module: source.key }, self.sink) {
            Some(Cached::Globs { edges, krate, chain, .. }) => Ok(Node::Edges { edges: blaze_surface::Globs::of(edges), krate: *krate, chain }),
            Some(Cached::Name(_) | Cached::Textual { .. } | Cached::Helpers { .. } | Cached::InProgress(_)) | None => {
                Err(Stop::Answer)
            }
        }
    }
}

impl<'a, N: Name> Resolver<'a, N> {
    /// What [`Resolver::calls_but`] answers from: calls not expanded, and pending calls blocked,
    /// each counted up to two. A change to either is a `CallsState` event. `calls_but` takes
    /// away at most one call (the asker's own) and asks "more than zero", so a count past two
    /// changes no reader's answer: counted in full, each expansion of a module with N calls
    /// woke every walk that read the module (N events; libc's linux module, 2026-11-16).
    pub(crate) fn calls_state(&self) -> (u32, u32) {
        (self.unexpanded.min(2), self.blocked_calls.min(2))
    }

    /// Whether a call but `except` may still add names, and whether one pending but `except`
    /// waits on an answer: constant time, from the counts the setters keep.
    pub(crate) fn calls_but(&self, except: Option<u32>) -> (bool, bool) {
        let (mut unexpanded, mut blocked) = (self.unexpanded, self.blocked_calls);
        if let Some(c) = except {
            let call = &self.calls[c as usize];
            if call.state.unexpanded() {
                unexpanded -= 1;
            }
            if call.state == CallState::Pending && call.blocked {
                blocked -= 1;
            }
        }
        (unexpanded > 0, blocked > 0)
    }

    /// The call of this module a lookup is made for, which never waits on itself.
    fn asking_call(&self, ignore: Option<(ItemKey, Asking)>) -> Option<u32> {
        match ignore {
            Some((m, Asking::Call(c))) if m == self.module => Some(c),
            Some(_) | None => None,
        }
    }

    /// In a [`Scan::Stall`] evaluation: blames the pending named imports of `name` that `q`'s
    /// asker sees (what made [`Resolver::own_lookup`] undecided).
    fn blame_named(&self, ns: Namespace, name: N, q: Query<'_>, sink: &mut Sink<'a, N>) {
        if sink.scan != Scan::Stall {
            return;
        }
        let Some(list) = self.named.get(&name) else { return };
        for &i in list.iter() {
            if q.ignore == Some((self.module, Asking::Import(i))) {
                continue;
            }
            let imp = &self.imports[i as usize];
            if imp.imports(ns) && imp.failed.is_none() && imp.ns[ns.index()] == NsRes::Pending && imp.vis.visible_from(q.asker) {
                sink.blame(Blame::Import(self.module, i));
            }
        }
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
        sink.stats.lookups += 1;
        sink.touch(Key::Name(self.module, name));
        if let Some(r) = self.own_lookup(ns, name, q) {
            if matches!(r, Res::Undet { .. } | Res::Wait) {
                self.blame_named(ns, name, q, sink);
            }
            return r;
        }
        let mark = sink.blame.len();
        let r = self.globs_res(ns, name, q, env, sink);
        if matches!(r, Res::Found(_) | Res::None) {
            sink.blame.truncate(mark);
        }
        r
    }

    /// What this module's own bindings decide of `name` in `ns`: a declaration, else a named
    /// import, with rustc's determinacy (`None`: nothing of its own, the globs decide).
    pub(crate) fn own_lookup(&self, ns: Namespace, name: N, q: Query<'_>) -> Option<Res> {
        if let Some(e) = self.defs.get(&(ns, name)) {
            return Some(Res::Found(e.hit()));
        }
        let mut best: Option<(At, Hit)> = None;
        let mut undet = false;
        let mut wait = false;
        if let Some(list) = self.named.get(&name) {
            for &i in list.iter() {
                if q.ignore == Some((self.module, Asking::Import(i))) {
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
                            vis: target.through_import(imp.vis, self.asker()),
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
            return Some(Res::Found(hit));
        }
        if wait {
            return Some(Res::Wait);
        }
        if undet {
            return Some(Res::Undet { weak: false });
        }
        None
    }

    /// What this module's globs decide of `name` in `ns`, by the stall rule's wait relation
    /// (bottom.md section 6): bound through globs, it waits only on the calls not expanded yet
    /// (outside the macro namespace), never on a glob; unbound, on the globs still pending
    /// and the calls. The asking call never waits on itself.
    fn globs_res(&self, ns: Namespace, name: N, q: Query<'_>, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let g = self.glob_lookup(ns, name, q, env, sink);
        sink.touch(Key::CallsState(self.module));
        let except = self.asking_call(q.ignore);
        let (unexpanded, calls_wait) = self.calls_but(except);
        match g.found {
            Some((_, hit)) => {
                if g.answer {
                    Res::Wait
                } else if q.restricted || ns == Namespace::Macros || !unexpanded {
                    sink.via_glob = true;
                    Res::Found(hit)
                } else {
                    sink.blame(Blame::Calls(self.module, except));
                    if calls_wait { Res::Wait } else { Res::Undet { weak: false } }
                }
            }
            None => {
                if unexpanded {
                    sink.blame(Blame::Calls(self.module, except));
                }
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
        let mut out = GlobFound { found: None, undet: false, wait: false, answer: false };
        sink.touch(Key::Globs(self.module));
        let importer = self.asker();
        for (gi, g) in self.globs.iter().enumerate() {
            if q.ignore == Some((self.module, Asking::Glob(seg(gi)))) {
                continue;
            }
            let (def, clamp) = match &g.source {
                Source::Pending => {
                    if g.vis.visible_from(q.asker) {
                        sink.blame(Blame::Glob(self.module, seg(gi)));
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
                Source::Resolved { def, clamp } => (*def, *clamp),
            };
            let cand = if def.key == self.module {
                None
            } else {
                #[cfg(test)]
                let was = core::mem::replace(&mut sink.probing, true);
                let mut sources = Live { at: self, ns, name, ignore: q.ignore, env, sink: &mut *sink, pend: None };
                let walked = through(&mut sources, def, self.alloc);
                let walked = sources.settle(walked);
                #[cfg(test)]
                {
                    sink.probing = was;
                }
                match walked {
                    Ok(Some(p)) if p.vis.visible_from(importer) => Some((p.def, p.vis, p.state)),
                    Ok(_) => None,
                    Err(Through::Stop(Stop::Undet)) => {
                        out.undet = true;
                        self.note_undet(gi, sink);
                        None
                    }
                    Err(Through::Stop(Stop::Wait)) => {
                        out.wait = true;
                        None
                    }
                    Err(Through::Stop(Stop::Answer)) => {
                        out.wait = true;
                        out.answer = true;
                        None
                    }
                    Err(Through::Unbounded) => {
                        sink.broke(GlobBug::ThroughUnbounded);
                        None
                    }
                }
            };
            let Some((d, vis, state)) = cand else { continue };
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

    /// Member `f` of this module's group as a source of a walk through globs, by its CURRENT
    /// facts (docs/v2/plan/design/bottom.md section 6, "one walk"): its own binding if it has
    /// one, else its resolved glob edges, the walk's least fixpoint joining them with every
    /// other source, member or finished record alike. A member's facts only grow while it is
    /// solved (a binding is added, never taken back), so the walk is monotone. What may still
    /// add a binding that shadows its globs (a named import, a glob or a call not decided yet,
    /// that the asker could see) stops the walk: `Wait` while an answer is outstanding,
    /// `Undet` otherwise, rustc's determinacy. Every fact read is touched, so a change to it
    /// wakes the item that read it.
    fn member_node<'e>(
        &self,
        f: &'e Resolver<'a, N>,
        ns: Namespace,
        name: N,
        ignore: Option<(ItemKey, Asking)>,
        pend: &mut Option<Stop>,
        sink: &mut Sink<'a, N>,
    ) -> Result<Node<'e>, Stop> {
        sink.touch(Key::Name(f.module, name));
        let q = Query { asker: self.asker(), restricted: false, ignore };
        match f.own_lookup(ns, name, q) {
            Some(Res::Found(hit)) => return Ok(Node::Value(Some(Passed { def: hit.def, vis: hit.vis, state: hit.state }))),
            Some(Res::None) => return Ok(Node::Value(None)),
            Some(Res::Wait) => {
                f.blame_named(ns, name, q, sink);
                return Err(Stop::Wait);
            }
            Some(Res::Undet { .. }) => {
                f.blame_named(ns, name, q, sink);
                return Err(Stop::Undet);
            }
            None => {}
        }
        sink.touch(Key::Globs(f.module));
        sink.touch(Key::CallsState(f.module));
        // A call not expanded yet may add a binding: the source is open (no value, no edges);
        // in the macro namespace a macro found through globs is taken (a call's own path must
        // not wait on the calls).
        let except = f.asking_call(ignore);
        let (unexpanded, calls_wait) = f.calls_but(except);
        if unexpanded && ns != Namespace::Macros {
            sink.blame(Blame::Calls(f.module, except));
            return Err(if calls_wait { Stop::Wait } else { Stop::Undet });
        }
        // Its globs still pending leave the walk undecided only if it finds nothing: its
        // decided edges are followed.
        for (gi, g) in f.globs.iter().enumerate() {
            if ignore == Some((f.module, Asking::Glob(seg(gi)))) {
                continue;
            }
            if matches!(g.source, Source::Pending) && g.vis.visible_from(q.asker) {
                sink.blame(Blame::Glob(f.module, seg(gi)));
                *pend = join(*pend, if g.blocked { Stop::Wait } else { Stop::Undet });
            }
        }
        let edges: &'e [GlobEdge] = f.edges();
        sink.stats.member_nodes += 1;
        sink.stats.member_edge_bytes += core::mem::size_of_val(edges) as u64;
        let asker = f.asker();
        Ok(Node::Edges { edges: blaze_surface::Globs::of(edges), krate: asker.krate, chain: asker.chain })
    }

    /// The binding of `name` in `ns` of the module or enum `def`, from this module.
    pub(crate) fn lookup_in(
        &self,
        def: Def,
        ns: Namespace,
        name: N,
        ignore: Option<(ItemKey, Asking)>,
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
        self.read_name(def, ns, name, Layer::Path, ignore, env, sink)
    }

    /// The binding of `name` in `ns` of `module` as a lexical layer (the standard prelude, a
    /// `#[macro_use]` crate root) sees it: this module's or a fellow's live, another's by a
    /// `Name` read. A prelude may be a module of the asker's own cycle (`core`'s is), and a
    /// member is never asked for: its answer would be `InProgress` for good.
    fn layer_name(&self, module: ItemKey, ns: Namespace, name: N, layer: Layer, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let live = if module == self.module { Some(self) } else { env.fellow(module) };
        let Some(f) = live else { return self.read_name(Def::module(module), ns, name, layer, None, env, sink) };
        let q = Query { asker: self.asker(), restricted: false, ignore: None };
        match f.lookup_module(ns, name, q, env, sink) {
            Res::Found(hit) => Res::Found(Hit { origin: Origin::Prelude, expansion: 0, frag: None, layer, ..hit }),
            other => other,
        }
    }

    /// `name` in `ns` of another module or an enum, `def`, by its records: its own binding by
    /// a `Name` read, else through its glob edges. `ignore` (the import being resolved) is
    /// passed on: a walk that comes back to this module through another's globs must not be
    /// held up by the import it is resolving.
    fn read_name(
        &self,
        def: Def,
        ns: Namespace,
        name: N,
        layer: Layer,
        ignore: Option<(ItemKey, Asking)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Res {
        let mut sources = Live { at: self, ns, name, ignore, env, sink: &mut *sink, pend: None };
        let walked = through(&mut sources, def, self.alloc);
        match sources.settle(walked) {
            Ok(None) => Res::None,
            Ok(Some(p)) => Res::Found(Hit {
                def: p.def,
                vis: p.vis,
                origin: Origin::Prelude,
                expansion: 0,
                frag: None,
                state: p.state,
                layer,
            }),
            Err(Through::Stop(Stop::Undet)) => Res::Undet { weak: false },
            Err(Through::Stop(Stop::Wait | Stop::Answer)) => Res::Wait,
            Err(Through::Unbounded) => {
                sink.broke(GlobBug::ThroughUnbounded);
                Res::Wait
            }
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

    /// `name` in the extern prelude (the Reference, "Extern prelude"; "Extern crate
    /// declarations"): the crate's dependencies, then an `extern crate` item of the crate ROOT,
    /// also one a macro's expansion wrote (libc's root `cfg_if! { extern crate
    /// rustc_std_workspace_core as core; }`). So the extern prelude is not fixed before
    /// expansion: while the root is solved in this group (itself or a fellow) and has calls not
    /// expanded, a miss waits on those calls as a module lookup does. Only the root's own
    /// `extern crate` entries count (never a `use .. as name`, never a glob), and a pending
    /// import or glob of the root is not waited on: neither can add to the extern prelude. A
    /// root solved outside this group is read by a `Name` read of the root: a binding whose
    /// origin is `extern crate` is in the extern prelude (the crate's other modules, libc's
    /// `unix`, reach the root's macro-written `core` this way).
    /// The miss is a WEAK undeterminacy (rustc's `Determinacy::determined(unexpanded
    /// invocations of the root are none)`): a lexical lookup goes on outward, so `Option`
    /// still comes from the standard prelude. The asking call is not waited on (a derive of
    /// the root asking for `::core` must not wait on itself).
    fn extern_prelude_res(&self, name: N, ignore: Option<(ItemKey, Asking)>, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        if let Some(hit) = self.extern_prelude_hit(name) {
            return Res::Found(hit);
        }
        let live = if self.root == self.module { Some(self) } else { env.fellow(self.root) };
        let Some(root) = live else {
            return match self.cached(Read::Name { module: self.root, ns: Namespace::Types, name }, sink) {
                Some(Cached::Name(NameAnswer::Found { def, origin: Origin::ExternCrate, .. })) => Res::Found(Hit {
                    def: *def,
                    vis: Visibility::Public,
                    origin: Origin::Prelude,
                    expansion: 0,
                    frag: None,
                    state: BindingState::Determined,
                    layer: Layer::ExternPrelude,
                }),
                Some(Cached::Name(_)) => Res::None,
                Some(Cached::Globs { .. } | Cached::Textual { .. } | Cached::Helpers { .. } | Cached::InProgress(_)) | None => Res::Wait,
            };
        };
        sink.touch(Key::Name(root.module, name));
        if let Some(e) = root.defs.get(&(Namespace::Types, name)) {
            if e.origin == Origin::ExternCrate {
                return Res::Found(Hit {
                    def: e.def,
                    vis: Visibility::Public,
                    origin: Origin::Prelude,
                    expansion: e.expansion,
                    frag: None,
                    state: BindingState::Determined,
                    layer: Layer::ExternPrelude,
                });
            }
        }
        sink.touch(Key::CallsState(root.module));
        let except = root.asking_call(ignore);
        let (unexpanded, _) = root.calls_but(except);
        if !unexpanded {
            return Res::None;
        }
        sink.blame(Blame::Calls(root.module, except));
        Res::Undet { weak: true }
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
            // `$crate` names the crate that defines the macro, wherever the call is (the
            // Reference, "Macros By Example", "Hygiene"), also one the caller has no extern name
            // for (fluent-bundle: `a`'s macro re-exported by `b`, called in `c`). That crate is in
            // the caller's dependency cone, and every crate root's key is `root_key(krate)`.
            PathKind::DollarCrate(k) => Ok(Hit::module(if k == self.krate { self.root } else { crate::task::root_key(k) })),
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
        ignore: Option<(ItemKey, Asking)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Res {
        let from_root = self.edition == Edition::E2015
            && (kind == PathKind::Abs || (kind == PathKind::Plain && used == PathUse::Import));
        if from_root {
            return self.lookup_in(Def::module(self.root), ns, name, ignore, env, sink);
        }
        if kind == PathKind::Abs {
            return match ns {
                Namespace::Types => match self.extern_prelude_res(name, ignore, env, sink) {
                    Res::Undet { .. } => Res::Undet { weak: false },
                    r => r,
                },
                _ => Res::None,
            };
        }
        // An import's macro reads the `macro_rules!` scope at its place (rustc's
        // `Scope::MacroRules` in the import's parent scope): `macro_rules! m {} pub(crate) use m;`.
        let textual = used == PathUse::Import && ns == Namespace::Macros;
        self.lexical(LexQuery { ns, name, at, want: None, textual, ignore }, env, sink)
    }

    /// A first segment through the lexical layers, innermost first: the `macro_rules!` textual
    /// scope, the module, the extern prelude, the `#[macro_use]` prelude, the standard
    /// prelude (rustc's `early_resolve_ident_in_lexical_scope`). Early, the first binding wins
    /// and a weak undeterminacy goes on outward; in the final pass the outer layers are read
    /// too where they could make the innermost ambiguous, and the ambiguity goes to the sink.
    pub(crate) fn lexical(&self, lq: LexQuery<N>, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        let mark = sink.blame.len();
        let r = self.lexical_layers(lq, env, sink);
        if matches!(r, Res::Found(_) | Res::None) {
            sink.blame.truncate(mark);
        }
        r
    }

    fn lexical_layers(&self, lq: LexQuery<N>, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
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
                let r = self.extern_prelude_res(lq.name, lq.ignore, env, sink);
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
        if lq.ns == Namespace::Types {
            if let Some(def) = crate::builtin_type_in(lq.name.as_ref(), self.arbitrary_ints) {
                let hit = Hit {
                    def,
                    vis: Visibility::Public,
                    origin: Origin::Prelude,
                    expansion: 0,
                    frag: None,
                    state: BindingState::Determined,
                    layer: Layer::Builtin,
                };
                if let Some(stop) = self.layer(Res::Found(hit), &lq, &mut inner, &mut undet, sink) {
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
            Some(k) => matches!(hit.def.kind, DefKind::Macro(s) if s.accepts(k)),
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
                // A call before it not expanded yet may still define the name: the call waits
                // on it (the stall rule's textual read), and no outer layer decides meanwhile.
                Tex::Undet => return Res::Undet { weak: false },
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
        let mut stack: blaze_own::AVec<'a, (u32, usize)> = blaze_own::AVec::new_in(self.alloc);
        stack.push((frag, end));
        while let Some((cur_frag, rem)) = stack.last_mut() {
            if *rem == 0 {
                stack.pop();
                continue;
            }
            let cur_frag = *cur_frag;
            let s = *rem - 1;
            *rem = s;
            let f = &self.frags[cur_frag as usize];
            match f.slots[s] {
                Slot::Rules { name: n, def, export, shapes } if n == name => {
                    let hit = self.textual_hit(&TextualDef { name: n, def, expansion: f.expansion, shapes });
                    // rustc's `define_macro`: a `#[macro_export]` macro's textual binding is
                    // public, so `pub use m;` beside it re-exports it (core's `ub_checks`).
                    let vis = if export { Visibility::Public } else { hit.vis };
                    return Tex::Hit(Hit { frag: Some(cur_frag), vis, ..hit });
                }
                Slot::Call(i) => {
                    let call = &self.calls[i as usize];
                    if call.state.unexpanded() {
                        sink.touch(Key::Call(self.module, i));
                    }
                    match call.state {
                        CallState::Expanded { frag: inner, .. } => {
                            let fr = &self.frags[inner as usize];
                            // Nothing in it defines a name of the textual scope or waits.
                            if !fr.textual && fr.open == 0 {
                                continue;
                            }
                            stack.push((inner, fr.slots.len()));
                        }
                        CallState::Pending => {
                            sink.blame(Blame::Call(self.module, i));
                            return if call.blocked { Tex::Wait } else { Tex::Undet };
                        }
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

    /// A `macro_rules!` of the textual scope. Its binding is visible in the whole crate
    /// (rustc's `define_macro`: `Restricted(CRATE_DEF_ID)`), so `pub(crate) use m;` re-exports
    /// it and `pub use m;` is a wider re-export; one of this module marked `#[macro_export]`
    /// is public (`textual_back`). A record's `TextualDef` does not say export, so one read
    /// from another module stays crate-wide.
    fn textual_hit(&self, d: &TextualDef<N>) -> Hit {
        Hit {
            def: Def { key: d.def, kind: DefKind::Macro(d.shapes) },
            vis: Visibility::Crate(self.krate),
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
            Some(Cached::Name(_) | Cached::Globs { .. } | Cached::Helpers { .. } | Cached::InProgress(_)) | None => {
                Tex::Wait
            }
        }
    }

    /// The `macro_rules!` visible at this module's `mod` line: up the chain of modules, at each
    /// level the parent's own definitions before the child's `mod` line (one `TextualEntry`
    /// read of the child, which waits on that parent only), innermost first; at the first
    /// ancestor of this group, its live textual scope at that line (which goes on up itself).
    /// Every read names the one module it waits on, so no read of a finished parent can wait
    /// on a member above it (std's `MemberUnfinished`, bottom.md section 6, scope lead 19).
    fn textual_entry(&self, name: N, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> Res {
        for k in 1..self.chain.len() {
            let (parent, child) = (self.chain[k], self.chain[k - 1]);
            if let Some(p) = env.fellow(parent) {
                sink.touch(Key::Frags(parent));
                return match p.find_mod(child) {
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
            match self.cached(Read::TextualEntry { module: child }, sink) {
                Some(Cached::Textual { defs, .. }) => {
                    if let Some(d) = defs.iter().rev().find(|d| d.name == name) {
                        return Res::Found(self.textual_hit(d));
                    }
                }
                Some(Cached::Name(_) | Cached::Globs { .. } | Cached::Helpers { .. } | Cached::InProgress(_)) | None => {
                    return Res::Wait;
                }
            }
        }
        Res::None
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
        ignore: Option<(ItemKey, Asking)>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Prefix {
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
                    Res::Undet { .. } => return Prefix::Undet,
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
                        BindingState::Determined => {}
                    }
                    if !hit.vis.visible_from(self.asker()) {
                        return Prefix::Fail(s, Unresolved::Private);
                    }
                    cur = hit;
                }
                Res::None => return Prefix::Fail(s, Unresolved::NotFound),
                Res::Undet { .. } => return Prefix::Undet,
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
        let ignore = Some((self.module, Asking::Import(seg(i))));
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

    /// Glob `g`'s source resolved from the state as it is.
    pub(crate) fn resolve_glob<F: ScopeFacts<Name = N>>(
        &self,
        g: usize,
        _facts: &F,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) -> Result<GlobOutcome, ResolveError> {
        let glob = &self.globs[g];
        let n = glob.path.segments.len();
        let pre = if n == 0 {
            match self.start_module(&glob.path) {
                Ok(hit) => Prefix::Found { hit, first: None },
                Err((s, why)) => Prefix::Fail(s, why),
            }
        } else {
            self.resolve_prefix(&glob.path, n, PathUse::Import, glob.at, Some((self.module, Asking::Glob(seg(g)))), env, sink)
        };
        Ok(match pre {
            Prefix::Found { hit, first } => {
                let source = if !hit.def.has_scope() {
                    Source::Failed(seg(n.saturating_sub(1)), Unresolved::NotAModule)
                } else if hit.def.key == self.module {
                    Source::Itself
                } else if hit.def.kind == DefKind::Enum {
                    Source::Resolved { def: hit.def, clamp: Some(hit.vis) }
                } else {
                    Source::Resolved { def: hit.def, clamp: None }
                };
                GlobOutcome { source: Some(source), first, wait: false }
            }
            Prefix::Fail(s, why) => GlobOutcome { source: Some(Source::Failed(s, why)), first: None, wait: false },
            Prefix::Undet => GlobOutcome { source: None, first: None, wait: false },
            Prefix::Wait => GlobOutcome { source: None, first: None, wait: true },
        })
    }

    /// Call `c`'s path resolved from the state as it is; it never waits on itself.
    pub(crate) fn resolve_call(&self, c: usize, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) -> CallOutcome {
        let call = &self.calls[c];
        let ignore = Some((self.module, Asking::Call(seg(c))));
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
        // `crate::m!`, `self::m!`, `super::m!` and `$crate::m!` are one segment after a start
        // module; only a plain one segment goes through the lexical layers. `::m!` alone names
        // an extern crate, never a macro (a 2015 `::m!` is refused here too).
        if n == 0 || (n == 1 && call.path.kind == PathKind::Abs) {
            return fail(0, Unresolved::NotFound);
        }
        let single = n == 1 && call.path.kind == PathKind::Plain;
        let inert = CallOutcome { state: Some(CallState::Inert), first: None, wait: false };
        let one_attr = call.kind == CallShape::Attr && single;
        if one_attr {
            // rustc's innermost scope for an attribute: the helpers of the derives before it
            // on its item.
            match self.derive_helper(c, segs[0], sink) {
                Helper::Is => return inert,
                Helper::Wait => return wait,
                Helper::Undet => return stay,
                Helper::Not => {}
            }
        }
        let last = seg(n - 1);
        let (r, first) = if single {
            let lq = LexQuery {
                ns: Namespace::Macros,
                name: segs[0],
                at: call.at,
                want: Some(call.kind),
                textual: call.kind == CallShape::FnLike,
                ignore,
            };
            (self.lexical(lq, env, sink), None)
        } else {
            match self.resolve_prefix(&call.path, n - 1, PathUse::Macro, call.at, ignore, env, sink) {
                Prefix::Found { hit, first } => {
                    if !hit.def.has_scope() {
                        return fail(last, Unresolved::NotAModule);
                    }
                    (self.lookup_in(hit.def, Namespace::Macros, segs[n - 1], ignore, env, sink), first)
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
                Res::Undet { .. } => stay,
                Res::Wait => wait,
            };
        }
        match r {
            Res::Found(hit) => match hit.state {
                BindingState::Ambiguous(k) => fail(last, Unresolved::Ambiguous(k)),
                BindingState::Determined => {
                    if !single && !hit.vis.visible_from(self.asker()) {
                        fail(last, Unresolved::Private)
                    } else if !self.kind_ok(&hit, Some(call.kind)) {
                        fail(last, Unresolved::WrongMacroKind)
                    } else {
                        CallOutcome {
                            state: Some(CallState::Resolved(hit.def)),
                            first: if single { Some(hit.def) } else { first },
                            wait: false,
                        }
                    }
                }
            },
            Res::None => fail(last, Unresolved::NotFound),
            Res::Undet { .. } => stay,
            Res::Wait => wait,
        }
    }

    /// Whether `name` is a helper attribute of a derive written before call `c` on its item.
    fn derive_helper(&self, c: usize, name: N, sink: &mut Sink<'a, N>) -> Helper {
        let call = &self.calls[c];
        let Some(item) = call.item else { return Helper::Not };
        let mut wait = false;
        let mut undet = false;
        for (di, d) in self.calls.iter().enumerate() {
            if d.kind != CallShape::Derive
                || d.item != Some(item)
                || d.at.frag != call.at.frag
                || d.at.slot >= call.at.slot
            {
                continue;
            }
            if d.state == CallState::Pending {
                sink.touch(Key::Call(self.module, seg(di)));
            }
            match d.state {
                CallState::Resolved(def) | CallState::Expanded { def, .. } => {
                    match self.cached(Read::DeriveHelpers { derive: def.key }, sink) {
                        Some(Cached::Helpers { names, .. }) => {
                            if names.binary_search(&name).is_ok() {
                                return Helper::Is;
                            }
                        }
                        Some(Cached::Name(_) | Cached::Globs { .. } | Cached::Textual { .. } | Cached::InProgress(_))
                        | None => wait = true,
                    }
                }
                // A derive still pending may still give the helper: the call waits on it.
                CallState::Pending => {
                    sink.blame(Blame::Call(self.module, seg(di)));
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
}
