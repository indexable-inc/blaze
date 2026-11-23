//! The final pass: once nothing can move, every early resolution is made again against the
//! final state (rustc's `finalize_import` and `finalize_macro_resolutions`), which is where an
//! ambiguity or an inconsistency is found; the scope's bindings, errors and textual sequence
//! are written; and the reads this pass made, and only those, are the read list.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{
    AnonImport, Binding, Def, DefKind, GlobEdge, Namespace, Origin, Path, ScopeBuilder,
    ScopeEntry, ScopeError, Site, TextualDef, TextualMark, Unresolved, Visibility, globs_digest,
    textual_digest,
};

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::read::{NameAnswer, Read};
#[cfg(test)]
use crate::resolver::Plant;
use crate::resolver::{Cached, CallState, Env, NsRes, Query, Res, Resolver, Scan, Sink, Slot, Source};
use crate::scope::{ReadList, Resolved};
use blaze_surface::SeenFrom;

/// What a member's final pass came to.
pub(crate) enum Finish<'a, N> {
    /// It read what has no answer yet (`asks`), or what is in progress (`waits`).
    Need { asks: AVec<'a, Read<N>>, waits: AVec<'a, Read<N>> },
    Done(Resolved<'a, N>),
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The errors of the final pass, in the module's tree order, and the non-glob bindings seen so
/// far for `Duplicate`.
struct Errors<'a, N> {
    list: AVec<'a, ScopeError<N, Path<'a, N>>>,
    seen: HashMap<(Namespace, N), Def, Alloc<'a>>,
}

impl<N: Name> Errors<'_, N> {
    /// A non-glob binding of `(ns, name)`: a second one with another target is `Duplicate`.
    fn bind(&mut self, ns: Namespace, name: N, def: Def) {
        let first = *self.seen.entry((ns, name)).or_insert(def);
        if first != def {
            self.list.push(ScopeError::Duplicate { ns, name, first, second: def });
        }
    }
}

impl<'a, N: Name> Resolver<'a, N> {
    /// The final pass of this machine, the members of `env` read live. Its read list holds
    /// what it read of modules outside `env`: alone, that is every read; a group's members
    /// take theirs from [`settle`], which reads each fellow by its record.
    pub(crate) fn finalize<F: ScopeFacts<Name = N>>(
        &self,
        facts: &F,
        env: Env<'_, 'a, N>,
        stats: &mut crate::resolver::FillStats,
    ) -> Result<Finish<'a, N>, ResolveError> {
        let a = self.alloc;
        let mut sink = Sink::new_in(a, Scan::Final);
        #[cfg(test)]
        {
            sink.forget_negative = self.plant == Plant::ForgetNegativeReads;
            sink.forget_probes = self.plant == Plant::ForgetGlobProbes;
        }
        let mut errors = Errors { list: Vec::new_in(a), seen: HashMap::new_in(a) };
        self.finish_frag(0, facts, env, &mut sink, &mut errors)?;

        let mut names = Vec::new_in(a);
        self.own_names(&mut names);
        names.sort_unstable();
        names.dedup();
        let q = Query { asker: self.asker(), restricted: false, ignore: None };
        let mut bindings = Vec::with_capacity_in(names.len(), a);
        for &(ns, name) in names.iter() {
            sink.undet_glob = None;
            match self.lookup_module(ns, name, q, env, &mut sink) {
                // A failed named import's name reached through a glob: the glob's, not the record's.
                Res::Found(hit) if hit.origin == Origin::Glob => {}
                Res::Found(hit) => bindings.push(ScopeEntry {
                    ns,
                    name,
                    binding: Binding {
                        def: hit.def,
                        vis: hit.vis,
                        origin: hit.origin,
                        expansion: hit.expansion,
                        state: hit.state,
                    },
                }),
                Res::None | Res::Wait => {}
                Res::Undet { .. } => match sink.undet_glob {
                    // Nothing is pending and every answer is in, so a name stays undecided
                    // only through this module's globs around a cycle of its group, where
                    // nothing binds it: at the least fixpoint it is not there (the Reference,
                    // "Use declarations": a glob imports the names its source has), so no
                    // binding and no error. A finished scope never says "undetermined".
                    Some((m, _, _)) if m == self.module => {}
                    other => {
                        return Err(ResolveError::Undecided {
                            module: self.module,
                            glob_of: other.map(|(m, _, _)| m),
                            unexpanded: self.unexpanded,
                        });
                    }
                },
            }
        }
        sink.stats.finals += 1;
        stats.add(&sink.stats);
        sink.stats = crate::resolver::FillStats::default();
        sink.check(self.module)?;

        // `pub(in path)` through an alias: resolved as a path now, and written on the binding.
        for (fi, frag) in self.frags.iter().enumerate() {
            for (path, ns, name, def) in frag.deferred_vis.iter() {
                let at = crate::resolver::At { frag: len32(fi), slot: 0 };
                let n = path.segments.len();
                let to = match self.resolve_prefix(path, n, crate::lookup::PathUse::Import, at, None, env, &mut sink) {
                    crate::lookup::Prefix::Found { hit, .. } if self.chain.contains(&hit.def.key) => {
                        Some(self.restricted(hit.def.key))
                    }
                    crate::lookup::Prefix::Found { .. }
                    | crate::lookup::Prefix::Fail(..)
                    | crate::lookup::Prefix::Undet
                    | crate::lookup::Prefix::Wait => None,
                };
                match to {
                    Some(vis) => {
                        if let Ok(k) = bindings.binary_search_by(|e| (e.ns, e.name).cmp(&(*ns, *name))) {
                            let b = &mut bindings[k].binding;
                            if b.def == *def && b.origin == Origin::Declared {
                                b.vis = vis;
                            }
                        }
                    }
                    None => errors.list.push(ScopeError::RestrictedNotAncestor { path: path.copy_in(a) }),
                }
            }
        }

        let mut textual = Vec::new_in(a);
        let mut marks = Vec::new_in(a);
        self.flatten(0, &mut textual, Some(&mut marks), env, &mut sink);
        marks.sort_unstable_by_key(|m: &TextualMark| m.child);
        let mut traits = Vec::new_in(a);
        let mut macro_exports = Vec::new_in(a);
        self.anon_and_exports(0, &mut traits, &mut macro_exports);

        if !sink.need.is_empty() || !sink.waits.is_empty() {
            return Ok(Finish::Need { asks: sink.need, waits: sink.waits });
        }

        let mut entries = sink.used;
        entries.sort_unstable_by(|x, y| x.read.cmp(&y.read));
        entries.dedup_by(|x, y| x.read == y.read);
        // The pass pushed one entry per use; the record keeps one per read.
        entries.shrink_to_fit();
        Ok(Finish::Done(Resolved {
            scope: ScopeBuilder {
                module: self.module,
                bindings,
                globs: self.glob_edges(),
                errors: errors.list,
                textual,
                marks,
                traits,
                macro_exports,
            },
            reads: ReadList { entries },
        }))
    }

    /// One fragment's errors, then its slots' in order, an expansion's where its call is.
    fn finish_frag<F: ScopeFacts<Name = N>>(
        &self,
        frag: u32,
        facts: &F,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
        errors: &mut Errors<'a, N>,
    ) -> Result<(), ResolveError> {
        let mut stack: AVec<'a, (u32, usize, bool)> = AVec::new_in(self.alloc);
        stack.push((frag, 0, false));
        while let Some((cur, idx, entered)) = stack.last_mut() {
            let cur_frag = *cur;
            let f = &self.frags[cur_frag as usize];
            if !*entered {
                *entered = true;
                for &(ns, name, e) in f.items.iter() {
                    errors.bind(ns, name, e.def);
                }
                for path in f.bad_vis.iter() {
                    errors.list.push(ScopeError::RestrictedNotAncestor { path: path.copy_in(self.alloc) });
                }
                for &name in f.bad_extern.iter() {
                    errors.list.push(ScopeError::ExternCrate { name });
                }
            }
            let slot_idx = *idx;
            let Some(&slot) = f.slots.get(slot_idx) else {
                stack.pop();
                continue;
            };
            *idx = slot_idx + 1;
            match slot {
                Slot::Use(i) => self.finish_import(i as usize, env, sink, errors),
                Slot::Glob(g) => self.finish_glob(g as usize, facts, env, sink, errors)?,
                Slot::Call(c) => {
                    self.finish_call(c as usize, env, sink, errors);
                    if let CallState::Expanded { frag: inner, .. } = self.calls[c as usize].state {
                        stack.push((inner, 0, false));
                    }
                }
                Slot::Rules { .. } | Slot::Mod { .. } => {}
            }
        }
        Ok(())
    }

    fn finish_import(&self, i: usize, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>, errors: &mut Errors<'a, N>) {
        let imp = &self.imports[i];
        let site = imp.site;
        let n = imp.path.segments.len();
        let last = len32(n.saturating_sub(1));
        let fail = |segment: u32, why: Unresolved| ScopeError::Import {
            site,
            path: imp.path.copy_in(self.alloc),
            segment,
            why,
        };
        // The early resolution, made again against the final state, whatever it came to: a
        // failed import is determined by its reads as much as a resolved one (a name not found
        // is a negative read, and the scope changes when that name appears).
        sink.amb = None;
        let again = self.resolve_import(i, env, sink);
        // Whether the leaf is there but hidden from this module, by the final state: an early
        // pass skips a pending import the asker cannot see, so it may have taken the leaf as
        // absent before the fellow that binds it settled. Which error it is depends on what
        // is there, never on when it was looked at.
        let hidden_now = again.failed.is_none()
            && again.ns.iter().any(|r| matches!(r, NsRes::Private(_)))
            && !again.ns.iter().any(|r| matches!(r, NsRes::Found(_)));
        if let Some((segment, why)) = imp.failed {
            sink.amb = None;
            let why = if why == Unresolved::NotFound && segment == last && hidden_now { Unresolved::Private } else { why };
            errors.list.push(fail(segment, why));
            return;
        }
        let mut found = false;
        let mut private = hidden_now;
        for ns in Namespace::ALL {
            match imp.ns[ns.index()] {
                NsRes::Found(_) => found = true,
                NsRes::Private(_) => private = true,
                NsRes::Pending | NsRes::Absent => {}
            }
        }
        if !found {
            sink.amb = None;
            errors.list.push(fail(last, if private { Unresolved::Private } else { Unresolved::NotFound }));
            return;
        }
        let leaf = imp.path.segments.last().copied().or(imp.bound);
        if let Some(amb) = sink.amb.take() {
            errors.list.push(ScopeError::Ambiguous {
                site: Site::Import(site),
                ns: amb.ns,
                name: amb.name,
                kind: amb.kind,
                first: amb.first,
                second: amb.second,
            });
        } else if let Some((segment, why)) = again.failed {
            errors.list.push(fail(segment, why));
        } else {
            if let (Some(early), Some(now), Some(&name)) = (imp.first, again.first, imp.path.segments.first()) {
                if early != now {
                    errors.list.push(ScopeError::Inconsistent {
                        site: Site::Import(site),
                        ns: Namespace::Types,
                        name,
                        early,
                        last: now,
                    });
                }
            }
            for ns in Namespace::ALL {
                if let (NsRes::Found(early), NsRes::Found(now), Some(name)) =
                    (imp.ns[ns.index()], again.ns[ns.index()], leaf)
                {
                    if early.def != now.def {
                        errors.list.push(ScopeError::Inconsistent {
                            site: Site::Import(site),
                            ns,
                            name,
                            early: early.def,
                            last: now.def,
                        });
                    }
                }
            }
        }
        for ns in Namespace::ALL {
            let NsRes::Found(hit) = imp.ns[ns.index()] else { continue };
            if let Some(name) = imp.bound {
                errors.bind(ns, name, hit.def);
            }
            // E0364/E0365: a `use` may not make a binding more visible than it is.
            if hit.through_import(imp.vis, self.asker()) != imp.vis {
                if let Some(name) = leaf {
                    errors.list.push(ScopeError::WiderReexport { site, ns, name, def: hit.def });
                }
            }
        }
    }

    fn finish_glob<F: ScopeFacts<Name = N>>(
        &self,
        g: usize,
        facts: &F,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
        errors: &mut Errors<'a, N>,
    ) -> Result<(), ResolveError> {
        let glob = &self.globs[g];
        let site = glob.site;
        let fail = |segment: u32, why: Unresolved| ScopeError::Import {
            site,
            path: glob.path.copy_in(self.alloc),
            segment,
            why,
        };
        // Made again whatever it came to, so a failed glob's reads are kept too.
        sink.amb = None;
        let again = self.resolve_glob(g, facts, env, sink)?;
        let early = match &glob.source {
            Source::Failed(segment, why) => {
                sink.amb = None;
                errors.list.push(fail(*segment, *why));
                return Ok(());
            }
            Source::Itself | Source::Pending => {
                sink.amb = None;
                return Ok(());
            }
            Source::Resolved { def, .. } => *def,
        };
        if let Some(amb) = sink.amb.take() {
            errors.list.push(ScopeError::Ambiguous {
                site: Site::Import(site),
                ns: amb.ns,
                name: amb.name,
                kind: amb.kind,
                first: amb.first,
                second: amb.second,
            });
            return Ok(());
        }
        match again.source {
            Some(Source::Failed(segment, why)) => errors.list.push(fail(segment, why)),
            Some(Source::Resolved { def, .. }) if def != early => {
                if let Some(&name) = glob.path.segments.last() {
                    errors.list.push(ScopeError::Inconsistent {
                        site: Site::Import(site),
                        ns: Namespace::Types,
                        name,
                        early,
                        last: def,
                    });
                }
            }
            Some(Source::Resolved { .. } | Source::Itself | Source::Pending) | None => {
                if let (Some(was), Some(now), Some(&name)) = (glob.first, again.first, glob.path.segments.first()) {
                    if was != now {
                        errors.list.push(ScopeError::Inconsistent {
                            site: Site::Import(site),
                            ns: Namespace::Types,
                            name,
                            early: was,
                            last: now,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn finish_call(&self, c: usize, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>, errors: &mut Errors<'a, N>) {
        let call = &self.calls[c];
        let fail = |segment: u32, why: Unresolved| ScopeError::Macro {
            call: call.key,
            path: call.path.copy_in(self.alloc),
            segment,
            why,
        };
        if call.state == CallState::TooDeep {
            // Never resolved, so it read nothing.
            errors.list.push(ScopeError::RecursionLimit { call: call.key });
            return;
        }
        // Made again whatever it came to, so a failed or inert call's reads are kept too.
        sink.amb = None;
        let again = self.resolve_call(c, env, sink);
        let early = match call.state {
            CallState::Failed(segment, why) => {
                sink.amb = None;
                errors.list.push(fail(segment, why));
                return;
            }
            CallState::Inert | CallState::Pending | CallState::TooDeep => {
                sink.amb = None;
                return;
            }
            CallState::Resolved(def) | CallState::Expanded { def, .. } => def,
        };
        if let Some(amb) = sink.amb.take() {
            errors.list.push(ScopeError::Ambiguous {
                site: Site::Call(call.key),
                ns: amb.ns,
                name: amb.name,
                kind: amb.kind,
                first: amb.first,
                second: amb.second,
            });
            return;
        }
        match again.state {
            Some(CallState::Failed(segment, why)) => errors.list.push(fail(segment, why)),
            Some(CallState::Resolved(now)) if now != early => {
                if let Some(&name) = call.path.segments.last() {
                    errors.list.push(ScopeError::Inconsistent {
                        site: Site::Call(call.key),
                        ns: Namespace::Macros,
                        name,
                        early,
                        last: now,
                    });
                }
            }
            Some(
                CallState::Resolved(_)
                | CallState::Pending
                | CallState::Expanded { .. }
                | CallState::Inert
                | CallState::TooDeep,
            )
            | None => {}
        }
    }

    /// Every `(ns, name)` this module binds of its own: its declarations and its named
    /// imports. A name reached through a glob is not the record's (its glob is an edge).
    fn own_names(&self, out: &mut AVec<'a, (Namespace, N)>) {
        out.extend(self.defs.keys().copied());
        for imp in self.imports.iter() {
            let Some(name) = imp.bound else { continue };
            for ns in Namespace::ALL {
                if matches!(imp.ns[ns.index()], NsRes::Found(_)) {
                    out.push((ns, name));
                }
            }
        }
    }

    /// The module's glob edges, by source: one per source, as visible as the widest glob of
    /// it and as shallow as the shallowest; an enum's narrowed by the enum's own visibility.
    pub(crate) fn glob_edges(&self) -> AVec<'a, GlobEdge> {
        let importer = self.asker();
        let mut out: AVec<'a, GlobEdge> = Vec::new_in(self.alloc);
        for g in self.globs.iter() {
            let Source::Resolved { def, clamp } = g.source else { continue };
            let vis = match clamp {
                Some(c) => Visibility::narrower(g.vis, c, importer),
                None => g.vis,
            };
            match out.iter_mut().find(|e| e.source.key == def.key) {
                Some(e) => {
                    e.vis = Visibility::wider(e.vis, vis, importer);
                    e.expansion = e.expansion.min(g.expansion);
                }
                None => out.push(GlobEdge { source: def, vis, expansion: g.expansion }),
            }
        }
        out.sort_unstable_by_key(|e| e.source.key);
        out
    }

    /// From fragment `frag` on, in tree order: the `use .. as _` imports of a trait, and the
    /// `#[macro_export]` macros an expansion wrote.
    fn anon_and_exports(
        &self,
        frag: u32,
        traits: &mut AVec<'a, AnonImport>,
        exports: &mut AVec<'a, TextualDef<N>>,
    ) {
        let mut stack: AVec<'a, (u32, usize, bool)> = AVec::new_in(self.alloc);
        stack.push((frag, 0, false));
        while let Some((cur, idx, entered)) = stack.last_mut() {
            let cur_frag = *cur;
            let f = &self.frags[cur_frag as usize];
            if !*entered {
                *entered = true;
                exports.extend_from_slice(&f.exports);
            }
            let slot_idx = *idx;
            let Some(&slot) = f.slots.get(slot_idx) else {
                stack.pop();
                continue;
            };
            *idx = slot_idx + 1;
            match slot {
                Slot::Use(i) => {
                    let imp = &self.imports[i as usize];
                    if imp.bound.is_some() || imp.failed.is_some() {
                        continue;
                    }
                    if let NsRes::Found(hit) = imp.ns[Namespace::Types.index()] {
                        if hit.def.kind == DefKind::Trait {
                            traits.push(AnonImport { site: imp.site, def: hit.def, vis: imp.vis });
                        }
                    }
                }
                Slot::Call(c) => {
                    if let CallState::Expanded { frag: inner, .. } = self.calls[c as usize].state {
                        stack.push((inner, 0, false));
                    }
                }
                Slot::Rules { .. } | Slot::Mod { .. } | Slot::Glob(_) => {}
            }
        }
    }

    /// The module's textual sequence from fragment `frag` on: its `macro_rules!` in order,
    /// an expansion's where its call is, a `#[macro_use]` child's where its `mod` line is;
    /// with `marks`, each child's place in it.
    fn flatten(
        &self,
        frag: u32,
        out: &mut AVec<'a, TextualDef<N>>,
        mut marks: Option<&mut AVec<'a, TextualMark>>,
        env: Env<'_, 'a, N>,
        sink: &mut Sink<'a, N>,
    ) {
        let mut stack: AVec<'a, (u32, usize)> = AVec::new_in(self.alloc);
        stack.push((frag, 0));
        while let Some((cur, idx)) = stack.last_mut() {
            let cur_frag = *cur;
            let f = &self.frags[cur_frag as usize];
            let slot_idx = *idx;
            let Some(&slot) = f.slots.get(slot_idx) else {
                stack.pop();
                continue;
            };
            *idx = slot_idx + 1;
            match slot {
                Slot::Rules { name, def, shapes, .. } => out.push(TextualDef { name, def, expansion: f.expansion, shapes }),
                Slot::Call(c) => {
                    if let CallState::Expanded { frag: inner, .. } = self.calls[c as usize].state {
                        stack.push((inner, 0));
                    }
                }
                Slot::Mod { child, macro_use } => {
                    if let Some(m) = marks.as_deref_mut() {
                        m.push(TextualMark { child, len: len32(out.len()) });
                    }
                    if macro_use {
                        self.carry_out(child, out, env, sink);
                    }
                }
                Slot::Use(_) | Slot::Glob(_) => {}
            }
        }
    }

    /// What a `#[macro_use]` child carries out: a fellow's live, another's by its record.
    fn carry_out(&self, child: ItemKey, out: &mut AVec<'a, TextualDef<N>>, env: Env<'_, 'a, N>, sink: &mut Sink<'a, N>) {
        if child != self.module {
            if let Some(f) = env.fellow(child) {
                f.flatten(0, out, None, env, sink);
                return;
            }
        }
        match self.cached(Read::TextualOut { module: child }, sink) {
            Some(Cached::Textual { defs, .. }) => out.extend_from_slice(defs),
            Some(Cached::Name(_) | Cached::Globs { .. } | Cached::Helpers { .. } | Cached::InProgress(_)) | None => {}
        }
    }
}

/// What [`settle_reads`] came to.
pub(crate) enum Settled<'a, N> {
    /// Every member's result, in the order of the members.
    Done(AVec<'a, Resolved<'a, N>>),
    /// A member alone read what no member's record answers and no member was answered: to
    /// ask for (`asks`), or in progress outside the group (`waits`, a larger cycle).
    Need { asks: AVec<'a, Read<N>>, waits: AVec<'a, Read<N>> },
}

/// The read lists of a group's members, whose scopes `group` holds (in the order of `ms`):
/// each member's final pass made again alone, every fellow read by the record the group wrote
/// for it, as a driver answers it. So a member's read list holds what it reads of every other
/// module, a fellow too, by the answers any reader of the records gets: one list whatever the
/// group was (a schedule may solve a module with more or fewer others), and an edit inside a
/// cycle reruns only the members whose reads it reaches. The scope of that pass is the
/// group's, the group's result being a fixpoint of its own records; one that differs is
/// refused.
///
/// A pass that reads a fellow's record it was not given is made again with that answer; the
/// answers of a walk through a fellow's glob edges are given at once, so a pass is made about
/// once per member.
pub(crate) fn settle_reads<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    group: AVec<'a, Resolved<'a, N>>,
    facts: &F,
    stats: &mut crate::resolver::FillStats,
    alloc: Alloc<'a>,
) -> Result<Settled<'a, N>, ResolveError> {
    let mut out = Vec::with_capacity_in(ms.len(), alloc);
    let mut asks = Vec::new_in(alloc);
    let mut waits = Vec::new_in(alloc);
    for i in 0..ms.len() {
        // Each round gives the member at least one answer it lacked, of finitely many: bounded.
        loop {
            
            let lone = Env { members: core::slice::from_ref(&ms[i]) };
            let (need, wait) = match ms[i].finalize(facts, lone, stats)? {
                Finish::Done(r) => {
                    if r.scope != group[i].scope {
                        return Err(ResolveError::Unsettled { module: ms[i].module });
                    }
                    out.push(r);
                    break;
                }
                Finish::Need { asks: need, waits: wait } => (need, wait),
            };
            let mut given: AVec<'a, (Read<N>, Cached<'a, N>)> = Vec::new_in(alloc);
            let mut outside = false;
            for (&read, waited) in need.iter().map(|r| (r, false)).chain(wait.iter().map(|r| (r, true))) {
                match member_answer(ms, &group, read, &mut given, alloc)? {
                    Given::Yes => {}
                    Given::Ask(other) => {
                        outside = true;
                        if waited && other == read { waits.push(read) } else { asks.push(other) }
                    }
                }
            }
            let mut moved = false;
            for (read, answer) in given {
                let fresh = !matches!(ms[i].answers.get(&read), Some(c) if !matches!(c, Cached::InProgress(_)));
                if fresh {
                    ms[i].answers.insert(read, answer);
                    moved = true;
                }
            }
            if !moved {
                if outside {
                    break;
                }
                return Err(ResolveError::MemberUnfinished { module: ms[i].module });
            }
        }
    }
    if !asks.is_empty() || !waits.is_empty() {
        asks.sort_unstable();
        asks.dedup();
        waits.sort_unstable();
        waits.dedup();
        return Ok(Settled::Need { asks, waits });
    }
    Ok(Settled::Done(out))
}

/// Whether [`member_answer`] gave the answer, or the read to ask the driver for instead.
enum Given<N> {
    Yes,
    Ask(Read<N>),
}

/// The member at `module`, by its index in `ms` (sorted by module).
fn member_at<N>(ms: &[Resolver<'_, N>], module: ItemKey) -> Option<usize> {
    ms.binary_search_by(|r| r.module.cmp(&module)).ok()
}

/// A copy of an answer, in `alloc`.
fn copy_cached<'a, N: Copy>(c: &Cached<'a, N>, alloc: Alloc<'a>) -> Cached<'a, N> {
    fn copy<'a, T: Copy>(s: &[T], a: Alloc<'a>) -> AVec<'a, T> {
        let mut v = Vec::with_capacity_in(s.len(), a);
        v.extend_from_slice(s);
        v
    }
    match c {
        Cached::Name(a) => Cached::Name(*a),
        Cached::Globs { edges, krate, chain, digest } => Cached::Globs {
            edges: copy(edges, alloc),
            krate: *krate,
            chain: copy(chain, alloc),
            digest: *digest,
        },
        Cached::Textual { defs, digest } => Cached::Textual { defs: copy(defs, alloc), digest: *digest },
        Cached::Helpers { names, digest } => Cached::Helpers { names: copy(names, alloc), digest: *digest },
        Cached::InProgress(m) => Cached::InProgress(*m),
    }
}

/// An answer some member was given for `read`, if one was (a member keeps what it was
/// answered before the group was made).
fn answered<'r, 'a, N: Name>(ms: &'r [Resolver<'a, N>], read: Read<N>) -> Option<&'r Cached<'a, N>> {
    ms.iter().find_map(|m| m.answers.get(&read).filter(|c| !matches!(c, Cached::InProgress(_))))
}

/// The answer to `read` that the records of the group's members give, pushed on `given`, with
/// the answers a walk from it through the members' glob edges reads next; else what some
/// member was answered; else the read to ask for.
fn member_answer<'a, N: Name>(
    ms: &[Resolver<'a, N>],
    group: &[Resolved<'a, N>],
    read: Read<N>,
    given: &mut AVec<'a, (Read<N>, Cached<'a, N>)>,
    alloc: Alloc<'a>,
) -> Result<Given<N>, ResolveError> {
    // The member whose finished record answers the read: a `TextualEntry` is its parent's (one
    // level); the parent is found on the chain of any member that has `module` above it.
    let first = match read {
        Read::TextualEntry { module } => ms.iter().find_map(|r| {
            let at = r.chain.iter().position(|&c| c == module)?;
            r.chain.get(at + 1).copied()
        }),
        Read::Name { module, .. } | Read::Globs { module } | Read::TextualOut { module } => Some(module),
        Read::DeriveHelpers { .. } => None,
    };
    let Some(at) = first.and_then(|m| member_at(ms, m)) else {
        return Ok(match answered(ms, read) {
            Some(c) => {
                given.push((read, copy_cached(c, alloc)));
                Given::Yes
            }
            None => Given::Ask(read),
        });
    };
    match read {
        Read::Name { ns, name, .. } => {
            // The walk a lookup makes: a member's own binding, else its edges' sources.
            let mut stack = Vec::new_in(alloc);
            let mut seen = Vec::new_in(alloc);
            stack.push(at);
            while let Some(f) = stack.pop() {
                if seen.contains(&f) {
                    continue;
                }
                seen.push(f);
                let scope = &group[f].scope;
                let module = scope.module;
                let found = scope.get(ns, name).copied();
                given.push((Read::Name { module, ns, name }, Cached::Name(NameAnswer::from(found))));
                if found.is_some() {
                    continue;
                }
                given.push((Read::Globs { module }, globs_answer(ms, group, f, alloc)));
                stack.extend(scope.globs.iter().filter_map(|e| member_at(ms, e.source.key)));
            }
        }
        Read::Globs { .. } => given.push((read, globs_answer(ms, group, at, alloc))),
        Read::TextualOut { .. } => {
            let defs = &group[at].scope.textual;
            let mut v = Vec::with_capacity_in(defs.len(), alloc);
            v.extend_from_slice(defs);
            given.push((read, Cached::Textual { digest: textual_digest(defs, alloc), defs: v }));
        }
        Read::TextualEntry { module } => {
            // One level: the member parent's own definitions before the `mod module` line.
            let scope = &group[at].scope;
            let len = scope.mark(module).ok_or(ResolveError::NoMark { module: scope.module, child: module })? as usize;
            let mut defs = Vec::new_in(alloc);
            defs.extend_from_slice(&scope.textual[..len.min(scope.textual.len())]);
            given.push((read, Cached::Textual { digest: textual_digest(&defs, alloc), defs }));
        }
        // A macro's record, never a member's.
        Read::DeriveHelpers { .. } => {}
    }
    Ok(Given::Yes)
}

/// The `Globs` answer of member `at`'s record: its edges, its crate, its chain.
fn globs_answer<'a, N: Name>(ms: &[Resolver<'a, N>], group: &[Resolved<'a, N>], at: usize, alloc: Alloc<'a>) -> Cached<'a, N> {
    let scope = &group[at].scope;
    let mut edges = Vec::with_capacity_in(scope.globs.len(), alloc);
    edges.extend_from_slice(&scope.globs);
    let mut chain = Vec::with_capacity_in(ms[at].chain.len(), alloc);
    chain.extend_from_slice(&ms[at].chain);
    let digest = globs_digest(&edges, ms[at].krate, &chain, alloc);
    Cached::Globs { edges, krate: ms[at].krate, chain, digest }
}
