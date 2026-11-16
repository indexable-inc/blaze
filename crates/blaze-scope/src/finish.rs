//! The final pass: once nothing can move, every early resolution is made again against the
//! final state (rustc's `finalize_import` and `finalize_macro_resolutions`), which is where an
//! ambiguity or an inconsistency is found; the scope's bindings, errors and textual sequence
//! are written; and the reads this pass made, and only those, are the read list.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::alloc::Allocator;

use alloc::vec::Vec;
use blaze_hash::HashMap;
use blaze_id::ItemKey;

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::read::Read;
#[cfg(test)]
use crate::resolver::Plant;
use crate::resolver::{Cached, CallState, Env, NsRes, Query, Res, Resolver, Scan, Sink, Slot, Source, Visited};
use crate::scope::{
    Binding, Def, ModuleScope, Namespace, ReadList, Resolved, ScopeEntry, ScopeError, Site,
    TextualDef, TextualMark, Unresolved, Visibility,
};

/// What a member's final pass came to.
pub(crate) enum Finish<N, A: Allocator> {
    /// It read what has no answer yet (`asks`), or what is in progress (`waits`).
    Need { asks: Vec<Read<N>, A>, waits: Vec<Read<N>, A> },
    Done(Resolved<N, A>),
}

fn len32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The errors of the final pass, in the module's tree order, and the non-glob bindings seen so
/// far for `Duplicate`.
struct Errors<N, A: Allocator> {
    list: Vec<ScopeError<N, A>, A>,
    seen: HashMap<(Namespace, N), Def, A>,
}

impl<N: Name, A: Allocator + Clone> Errors<N, A> {
    /// A non-glob binding of `(ns, name)`: a second one with another target is `Duplicate`.
    fn bind(&mut self, ns: Namespace, name: N, def: Def) {
        let first = *self.seen.entry((ns, name)).or_insert(def);
        if first != def {
            self.list.push(ScopeError::Duplicate { ns, name, first, second: def });
        }
    }
}

impl<N: Name, A: Allocator + Clone> Resolver<N, A> {
    /// The final pass of this member of a group of `fellows` (sorted, itself among them).
    pub(crate) fn finalize<F: ScopeFacts<Name = N>>(
        &self,
        facts: &F,
        env: Env<'_, N, A>,
        fellows: &[ItemKey],
    ) -> Result<Finish<N, A>, ResolveError> {
        let a = self.alloc.clone();
        let mut sink = Sink::new_in(&a, Scan::Final);
        #[cfg(test)]
        {
            sink.forget_negative = self.plant == Plant::ForgetNegativeReads;
        }
        let mut errors = Errors { list: Vec::new_in(a.clone()), seen: HashMap::new_in(a.clone()) };
        self.finish_frag(0, facts, env, &mut sink, &mut errors)?;

        let mut names = Vec::new_in(a.clone());
        self.glob_names(env, &mut sink, None, &mut names);
        names.sort();
        names.dedup();
        let q = Query { asker: self.asker(), restricted: false, ignore: None, visited: None };
        let mut bindings = Vec::with_capacity_in(names.len(), a.clone());
        let mut undetermined = false;
        for &(ns, name) in names.iter() {
            match self.lookup_module(ns, name, q, env, &mut sink) {
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
                Res::Undet { .. } => undetermined = true,
            }
        }

        let mut textual = Vec::new_in(a.clone());
        let mut marks = Vec::new_in(a.clone());
        self.flatten(0, &mut textual, Some(&mut marks), env, &mut sink);
        marks.sort_by_key(|m: &TextualMark| m.child);

        if !sink.need.is_empty() || !sink.waits.is_empty() {
            return Ok(Finish::Need { asks: sink.need, waits: sink.waits });
        }
        if undetermined {
            // Nothing is pending and every answer is in, so only an answer that says
            // `Undetermined` (which no finished scope holds) leaves a name undecided: the
            // caller broke the protocol, and the machine stops rather than guess.
            return Err(ResolveError::NoProgressBound { module: self.module, passes: 0 });
        }

        let mut entries = sink.used;
        entries.retain(|e| match self.owner_of(&e.read) {
            Some(m) => !fellows.contains(&m),
            None => true,
        });
        entries.sort_by(|x, y| x.read.cmp(&y.read));
        entries.dedup_by(|x, y| x.read == y.read);
        let mut others = Vec::with_capacity_in(fellows.len(), a);
        others.extend(fellows.iter().copied().filter(|&m| m != self.module));
        others.sort();
        Ok(Finish::Done(Resolved {
            scope: ModuleScope {
                module: self.module,
                bindings,
                errors: errors.list,
                textual,
                marks,
                fellows: others,
            },
            reads: ReadList { entries },
        }))
    }

    /// The module whose record answers `read`.
    fn owner_of(&self, read: &Read<N>) -> Option<ItemKey> {
        match *read {
            Read::TextualEntry { module } if module == self.module => self.chain.get(1).copied(),
            Read::TextualEntry { module }
            | Read::Name { module, .. }
            | Read::Exports { module, .. }
            | Read::TextualOut { module } => Some(module),
        }
    }

    /// One fragment's errors, then its slots' in order, an expansion's where its call is.
    fn finish_frag<F: ScopeFacts<Name = N>>(
        &self,
        frag: u32,
        facts: &F,
        env: Env<'_, N, A>,
        sink: &mut Sink<N, A>,
        errors: &mut Errors<N, A>,
    ) -> Result<(), ResolveError> {
        let f = &self.frags[frag as usize];
        for &(ns, name, e) in f.items.iter() {
            errors.bind(ns, name, e.def);
        }
        for path in f.bad_vis.iter() {
            errors.list.push(ScopeError::RestrictedNotAncestor { path: path.copy_in(self.alloc.clone()) });
        }
        for &name in f.bad_extern.iter() {
            errors.list.push(ScopeError::ExternCrate { name });
        }
        for slot in f.slots.iter() {
            match *slot {
                Slot::Use(i) => self.finish_import(i as usize, env, sink, errors),
                Slot::Glob(g) => self.finish_glob(g as usize, facts, env, sink, errors)?,
                Slot::Call(c) => {
                    self.finish_call(c as usize, env, sink, errors);
                    if let CallState::Expanded { frag: inner, .. } = self.calls[c as usize].state {
                        self.finish_frag(inner, facts, env, sink, errors)?;
                    }
                }
                Slot::Rules { .. } | Slot::Mod { .. } => {}
            }
        }
        Ok(())
    }

    fn finish_import(&self, i: usize, env: Env<'_, N, A>, sink: &mut Sink<N, A>, errors: &mut Errors<N, A>) {
        let imp = &self.imports[i];
        let site = imp.site;
        let n = imp.path.segments.len();
        let last = len32(n.saturating_sub(1));
        let fail = |segment: u32, why: Unresolved| ScopeError::Import {
            site,
            path: imp.path.copy_in(self.alloc.clone()),
            segment,
            why,
        };
        if let Some((segment, why)) = imp.failed {
            errors.list.push(fail(segment, why));
            return;
        }
        let mut found = false;
        let mut private = false;
        for ns in Namespace::ALL {
            match imp.ns[ns.index()] {
                NsRes::Found(_) => found = true,
                NsRes::Private(_) => private = true,
                NsRes::Pending | NsRes::Absent => {}
            }
        }
        if !found {
            errors.list.push(fail(last, if private { Unresolved::Private } else { Unresolved::NotFound }));
            return;
        }
        // The early resolution, made again against the final state.
        sink.amb = None;
        let again = self.resolve_import(i, env, sink);
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
            if Visibility::narrower(imp.vis, hit.vis, self.asker()) != imp.vis {
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
        env: Env<'_, N, A>,
        sink: &mut Sink<N, A>,
        errors: &mut Errors<N, A>,
    ) -> Result<(), ResolveError> {
        let glob = &self.globs[g];
        let site = glob.site;
        let fail = |segment: u32, why: Unresolved| ScopeError::Import {
            site,
            path: glob.path.copy_in(self.alloc.clone()),
            segment,
            why,
        };
        let early = match &glob.source {
            Source::Failed(segment, why) => {
                errors.list.push(fail(*segment, *why));
                return Ok(());
            }
            Source::Itself | Source::Pending => return Ok(()),
            Source::Resolved { def, .. } => *def,
        };
        sink.amb = None;
        let again = self.resolve_glob(g, facts, env, sink)?;
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

    fn finish_call(&self, c: usize, env: Env<'_, N, A>, sink: &mut Sink<N, A>, errors: &mut Errors<N, A>) {
        let call = &self.calls[c];
        let fail = |segment: u32, why: Unresolved| ScopeError::Macro {
            call: call.key,
            path: call.path.copy_in(self.alloc.clone()),
            segment,
            why,
        };
        let early = match call.state {
            CallState::Failed(segment, why) => {
                errors.list.push(fail(segment, why));
                return;
            }
            CallState::TooDeep => {
                errors.list.push(ScopeError::RecursionLimit { call: call.key });
                return;
            }
            CallState::Inert | CallState::Pending => return,
            CallState::Resolved(def) | CallState::Expanded { def, .. } => def,
        };
        sink.amb = None;
        let again = self.resolve_call(c, env, sink);
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

    /// Every `(ns, name)` this module may bind: its declarations, its named imports, and what
    /// each glob's source binds (a fellow's live, others' by their exports).
    fn glob_names(
        &self,
        env: Env<'_, N, A>,
        sink: &mut Sink<N, A>,
        visited: Option<&Visited<'_>>,
        out: &mut Vec<(Namespace, N), A>,
    ) {
        out.extend(self.defs.keys().copied());
        for imp in self.imports.iter() {
            let Some(name) = imp.bound else { continue };
            for ns in Namespace::ALL {
                if matches!(imp.ns[ns.index()], NsRes::Found(_)) {
                    out.push((ns, name));
                }
            }
        }
        let me = Visited { module: self.module, next: visited };
        for g in self.globs.iter() {
            let Source::Resolved { def, classes, .. } = &g.source else { continue };
            if let Some(f) = env.fellow(def.key) {
                if def.key != self.module && !visited.is_some_and(|v| v.contains(def.key)) {
                    f.glob_names(env, sink, Some(&me), out);
                }
                continue;
            }
            for &class in classes.iter() {
                if let Some(Cached::Exports { entries, .. }) = self.cached(Read::Exports { module: def.key, class }, sink) {
                    out.extend(entries.iter().map(|e| (e.ns, e.name)));
                }
            }
        }
    }

    /// The module's textual sequence from fragment `frag` on: its `macro_rules!` in order,
    /// an expansion's where its call is, a `#[macro_use]` child's where its `mod` line is;
    /// with `marks`, each child's place in it.
    fn flatten(
        &self,
        frag: u32,
        out: &mut Vec<TextualDef<N>, A>,
        mut marks: Option<&mut Vec<TextualMark, A>>,
        env: Env<'_, N, A>,
        sink: &mut Sink<N, A>,
    ) {
        let f = &self.frags[frag as usize];
        for slot in f.slots.iter() {
            match *slot {
                Slot::Rules { name, def } => out.push(TextualDef { name, def, expansion: f.expansion }),
                Slot::Call(c) => {
                    if let CallState::Expanded { frag: inner, .. } = self.calls[c as usize].state {
                        self.flatten(inner, out, marks.as_deref_mut(), env, sink);
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
    fn carry_out(&self, child: ItemKey, out: &mut Vec<TextualDef<N>, A>, env: Env<'_, N, A>, sink: &mut Sink<N, A>) {
        if child != self.module {
            if let Some(f) = env.fellow(child) {
                f.flatten(0, out, None, env, sink);
                return;
            }
        }
        match self.cached(Read::TextualOut { module: child }, sink) {
            Some(Cached::Textual { defs, .. }) => out.extend_from_slice(defs),
            Some(Cached::Name(_) | Cached::Exports { .. } | Cached::InProgress) | None => {}
        }
    }
}
