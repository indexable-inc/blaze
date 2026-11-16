//! [`Resolver`]: one module's name resolution as a sans-IO state machine.
//!
//! The machine holds what the module declares (its own text and each expansion as a
//! *fragment*), the state of each `use`, glob and macro call, and the answers it was given.
//! A step runs passes until nothing moves (each pass plans every update from the state as it
//! is, then applies them), and then returns what it needs: reads, expansions, or the modules
//! of a cycle. When nothing is pending it forces what is stuck (macro paths first, then
//! imports), as rustc does, and finally re-resolves everything once against the final scope:
//! that last pass checks every early resolution with rustc's ambiguity kinds and is what the
//! read list is made of, so the list is a function of the final state, not of the order the
//! answers came in.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::alloc::Allocator;

use alloc::vec::Vec;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};

use crate::error::{Record, ResolveError};
use crate::facts::{
    Dep, Edition, ExternTarget, ModuleDecls, Name, Path, PathKind, Rename, ScopeFacts, UseLeaf,
    VisDecl,
};
use crate::read::{Answer, ExpandRequest, NameAnswer, Read, Reply, Step};
use crate::scope::{
    Asker, BindingState, Def, DefKind, ExportEntry, MacroKind, Namespace, Origin, Resolved,
    TextualDef, Unresolved, UseSite, Visibility, VisibilityClass,
};

/// The most parents a module may have (rule 18): rust-analyzer's and rustc's module depth
/// limits are of this order, and no real crate comes near it.
pub const MAX_MODULE_DEPTH: usize = 128;

/// rustc's default `recursion_limit`: a macro call written at this expansion depth is an error.
pub const RECURSION_LIMIT: u16 = 128;

/// The most macro expansions one module may absorb (rule 18).
pub const MAX_EXPANSIONS: usize = 1 << 16;

/// A place in a module's text: a fragment and a slot of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct At {
    pub(crate) frag: u32,
    pub(crate) slot: u32,
}

/// One ordered entry of a fragment: what textual order matters for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Slot<N> {
    Rules { name: N, def: ItemKey },
    Call(u32),
    Mod { child: ItemKey, macro_use: bool },
    Use(u32),
    Glob(u32),
}

/// A declaration that is not ordered: an item, a `mod`, an `extern crate`, a macro export.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DefEntry {
    pub(crate) def: Def,
    pub(crate) vis: Visibility,
    pub(crate) origin: Origin,
    pub(crate) expansion: u16,
}

/// The module's own text, or one call's expansion.
pub(crate) struct Fragment<N, A: Allocator> {
    /// The call it is the expansion of; `None` for the module's own text.
    pub(crate) call: Option<ItemKey>,
    pub(crate) expansion: u16,
    /// Where the call is, for a textual lookup that runs off the fragment's start.
    pub(crate) parent: Option<At>,
    pub(crate) slots: Vec<Slot<N>, A>,
    /// Its unordered declarations, in the order written, for the final pass.
    pub(crate) items: Vec<(Namespace, N, DefEntry), A>,
    /// Its `extern crate`s of no dependency.
    pub(crate) bad_extern: Vec<N, A>,
    /// Its `pub(in path)`s whose path names no ancestor.
    pub(crate) bad_vis: Vec<Path<N, A>, A>,
}

/// Which scope layer a hit was found in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Layer {
    Textual,
    Module,
    CrateRoot,
    ExternPrelude,
    MacroUsePrelude,
    StdPrelude,
    /// Another module's scope, through a path.
    Path,
}

/// A binding a lookup found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hit {
    pub(crate) def: Def,
    pub(crate) vis: Visibility,
    pub(crate) origin: Origin,
    pub(crate) expansion: u16,
    pub(crate) state: BindingState,
    pub(crate) layer: Layer,
}

impl DefEntry {
    pub(crate) fn hit(&self) -> Hit {
        Hit {
            def: self.def,
            vis: self.vis,
            origin: self.origin,
            expansion: self.expansion,
            state: BindingState::Determined,
            layer: Layer::Module,
        }
    }
}

/// A lookup's outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Res {
    Found(Hit),
    /// Determined: no binding.
    None,
    /// Not determined yet. `weak`: nothing but a glob or an unexpanded macro can still define
    /// it, so an early lexical lookup may go on to outer scopes (rustc's `Weak::Yes`).
    Undet { weak: bool },
    /// An answer is needed first (asked, or in progress); never forced.
    Wait,
}

/// One namespace of a named import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NsRes {
    Pending,
    Absent,
    /// Found, and the importing module may not name it.
    Private(Hit),
    Found(Hit),
}

pub(crate) struct Import<N, A: Allocator> {
    pub(crate) site: UseSite,
    pub(crate) path: Path<N, A>,
    /// What it binds; `None` for `as _`.
    pub(crate) bound: Option<N>,
    pub(crate) types_only: bool,
    pub(crate) vis: Visibility,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) ns: [NsRes; 3],
    pub(crate) failed: Option<(u32, Unresolved)>,
}

impl<N, A: Allocator> Import<N, A> {
    /// Whether namespace `ns` is one it imports.
    pub(crate) fn imports(&self, ns: Namespace) -> bool {
        !self.types_only || ns == Namespace::Types
    }

    pub(crate) fn done(&self) -> bool {
        self.failed.is_some()
            || Namespace::ALL
                .iter()
                .all(|&ns| !self.imports(ns) || self.ns[ns.index()] != NsRes::Pending)
    }
}

pub(crate) enum Source<A: Allocator> {
    Pending,
    Resolved { def: Def, clamp: Option<Visibility>, classes: Vec<VisibilityClass, A> },
    /// `use self::*`: imports nothing new.
    Itself,
    Failed(u32, Unresolved),
}

pub(crate) struct Glob<N, A: Allocator> {
    pub(crate) site: UseSite,
    pub(crate) path: Path<N, A>,
    pub(crate) vis: Visibility,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) source: Source<A>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CallState {
    Pending,
    /// Its path is resolved and its expansion asked for.
    Resolved(Def),
    Expanded { frag: u32, def: Def },
    /// A tool attribute: nothing to expand.
    Inert,
    Failed(u32, Unresolved),
    /// Written at the recursion limit.
    TooDeep,
}

pub(crate) struct Call<N, A: Allocator> {
    pub(crate) key: ItemKey,
    pub(crate) path: Path<N, A>,
    pub(crate) kind: MacroKind,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) state: CallState,
}

/// An answer as kept.
pub(crate) enum Cached<N, A: Allocator> {
    Name(NameAnswer),
    Exports { entries: Vec<ExportEntry<N>, A>, digest: ContentId },
    Textual { defs: Vec<TextualDef<N>, A>, digest: ContentId },
    InProgress,
}

/// Where a pass's reads go: the reads it needs, the reads it used (only in the final pass)
/// and the reads that answered `InProgress`. Off for a lookup into a fellow of a group, whose
/// reads are its own.
pub(crate) struct Sink<N, A: Allocator> {
    pub(crate) on: bool,
    pub(crate) need: Vec<Read<N>, A>,
    pub(crate) used: Vec<Read<N>, A>,
    pub(crate) waits: Vec<Read<N>, A>,
    /// The planted bug of the read-list tests: negative reads are not recorded.
    #[cfg(test)]
    pub(crate) forget_negative: bool,
}

impl<N, A: Allocator + Clone> Sink<N, A> {
    pub(crate) fn new_in(on: bool, alloc: &A) -> Sink<N, A> {
        Sink {
            on,
            need: Vec::new_in(alloc.clone()),
            used: Vec::new_in(alloc.clone()),
            waits: Vec::new_in(alloc.clone()),
            #[cfg(test)]
            forget_negative: false,
        }
    }
}

/// The other machines a lookup may read live: the members of the group being solved (sorted
/// by module), or none.
pub(crate) struct Env<'a, N, A: Allocator + Clone> {
    pub(crate) members: &'a [Resolver<N, A>],
}

impl<N, A: Allocator + Clone> Clone for Env<'_, N, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N, A: Allocator + Clone> Copy for Env<'_, N, A> {}

impl<'a, N, A: Allocator + Clone> Env<'a, N, A> {
    pub(crate) fn solo() -> Env<'a, N, A> {
        Env { members: &[] }
    }

    pub(crate) fn fellow(&self, module: ItemKey) -> Option<&'a Resolver<N, A>> {
        self.members.binary_search_by(|m| m.module.cmp(&module)).ok().map(|at| &self.members[at])
    }
}

/// The modules a glob traversal has passed through: a glob cycle contributes nothing beyond
/// what is bound somewhere on it (the least fixpoint).
#[derive(Clone, Copy)]
pub(crate) struct Visited<'v> {
    pub(crate) module: ItemKey,
    pub(crate) next: Option<&'v Visited<'v>>,
}

impl Visited<'_> {
    pub(crate) fn contains(&self, module: ItemKey) -> bool {
        let mut at = Some(self);
        while let Some(link) = at {
            if link.module == module {
                return true;
            }
            at = link.next;
        }
        false
    }
}

/// One lookup's context.
#[derive(Clone, Copy)]
pub(crate) struct Query<'q> {
    pub(crate) asker: Asker<'q>,
    /// rustc's restricted shadowing: an early lexical lookup takes a glob binding at once.
    pub(crate) restricted: bool,
    /// The import being resolved, which may not justify itself (rustc's `ignore_binding`).
    pub(crate) ignore: Option<(ItemKey, u32)>,
    pub(crate) visited: Option<&'q Visited<'q>>,
}

/// The planted bugs the tests' controls switch on.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The read list leaves out the reads that found nothing.
    ForgetNegativeReads,
}

/// One module's name resolution (see the crate's documentation for the protocol).
pub struct Resolver<N, A: Allocator + Clone> {
    pub(crate) module: ItemKey,
    pub(crate) krate: CrateItemKey,
    pub(crate) root: ItemKey,
    pub(crate) edition: Edition,
    /// The module, its parent, ..., the crate root.
    pub(crate) chain: Vec<ItemKey, A>,
    /// Each of `chain`'s names in its parent.
    pub(crate) chain_names: Vec<Option<N>, A>,
    /// No `#[no_implicit_prelude]` on the module or above it.
    pub(crate) implicit_prelude: bool,
    pub(crate) extern_prelude: Vec<(N, ItemKey), A>,
    pub(crate) deps: Vec<Dep<N>, A>,
    pub(crate) macro_use: Vec<ItemKey, A>,
    pub(crate) prelude: Option<ItemKey>,
    pub(crate) tools: Vec<N, A>,
    pub(crate) frags: Vec<Fragment<N, A>, A>,
    /// The non-glob declarations by namespace and name, the first written kept.
    pub(crate) defs: HashMap<(Namespace, N), DefEntry, A>,
    pub(crate) imports: Vec<Import<N, A>, A>,
    /// The imports by the name they bind.
    pub(crate) named: HashMap<N, Vec<u32, A>, A>,
    pub(crate) globs: Vec<Glob<N, A>, A>,
    pub(crate) calls: Vec<Call<N, A>, A>,
    /// Calls not expanded yet (pending or asked for).
    pub(crate) unexpanded: u32,
    pub(crate) expansions: usize,
    pub(crate) answers: HashMap<Read<N>, Cached<N, A>, A>,
    /// What the last pass that moved nothing needed.
    pub(crate) asks: Vec<Read<N>, A>,
    pub(crate) wait_reads: Vec<Read<N>, A>,
    pub(crate) waits: Vec<ItemKey, A>,
    pub(crate) expand: Vec<ExpandRequest, A>,
    pub(crate) finished: bool,
    pub(crate) alloc: A,
    #[cfg(test)]
    pub(crate) plant: Plant,
}

/// The updates one pass plans, applied after.
pub(crate) struct Plan<A: Allocator> {
    pub(crate) imports: Vec<(u32, [NsRes; 3], Option<(u32, Unresolved)>), A>,
    pub(crate) globs: Vec<(u32, Source<A>), A>,
    pub(crate) calls: Vec<(u32, CallState), A>,
}

fn count(n: usize, module: ItemKey) -> Result<u32, ResolveError> {
    u32::try_from(n).map_err(|_| ResolveError::TooManyExpansions { module })
}

impl<N: Name, A: Allocator + Clone> Resolver<N, A> {
    /// A machine for `module`, made from its records in `facts`, allocating in `alloc`.
    pub fn new_in<F: ScopeFacts<Name = N>>(
        module: ItemKey,
        facts: &F,
        alloc: A,
    ) -> Result<Resolver<N, A>, ResolveError> {
        let info = facts
            .module(module)
            .ok_or(ResolveError::Missing { record: Record::ModuleInfo, module })?;
        let mut chain = Vec::new_in(alloc.clone());
        let mut chain_names = Vec::new_in(alloc.clone());
        chain.push(module);
        chain_names.push(info.name);
        let mut implicit_prelude = !info.no_implicit_prelude;
        let mut parent = info.parent;
        while let Some(p) = parent {
            if chain.len() >= MAX_MODULE_DEPTH {
                return Err(ResolveError::ModuleTooDeep { module });
            }
            let up = facts
                .module(p)
                .ok_or(ResolveError::Missing { record: Record::ModuleInfo, module: p })?;
            chain.push(p);
            chain_names.push(up.name);
            implicit_prelude &= !up.no_implicit_prelude;
            parent = up.parent;
        }
        let root = chain[chain.len() - 1];
        let krate = info.krate;
        let cr = facts.crate_root(krate).ok_or(ResolveError::NoCrateRoot { module, krate })?;
        let mut extern_prelude = Vec::with_capacity_in(cr.extern_prelude.len(), alloc.clone());
        extern_prelude.extend_from_slice(&cr.extern_prelude);
        extern_prelude.sort_by(|a, b| a.0.cmp(&b.0));
        let mut deps = Vec::with_capacity_in(cr.deps.len(), alloc.clone());
        deps.extend_from_slice(&cr.deps);
        let mut macro_use = Vec::with_capacity_in(cr.macro_use.len(), alloc.clone());
        macro_use.extend_from_slice(&cr.macro_use);
        let mut tools = Vec::with_capacity_in(cr.tools.len(), alloc.clone());
        tools.extend_from_slice(&cr.tools);
        let mut r = Resolver {
            module,
            krate,
            root,
            edition: cr.edition,
            chain,
            chain_names,
            implicit_prelude,
            extern_prelude,
            deps,
            macro_use,
            prelude: cr.prelude,
            tools,
            frags: Vec::new_in(alloc.clone()),
            defs: HashMap::new_in(alloc.clone()),
            imports: Vec::new_in(alloc.clone()),
            named: HashMap::new_in(alloc.clone()),
            globs: Vec::new_in(alloc.clone()),
            calls: Vec::new_in(alloc.clone()),
            unexpanded: 0,
            expansions: 0,
            answers: HashMap::new_in(alloc.clone()),
            asks: Vec::new_in(alloc.clone()),
            wait_reads: Vec::new_in(alloc.clone()),
            waits: Vec::new_in(alloc.clone()),
            expand: Vec::new_in(alloc.clone()),
            finished: false,
            alloc,
            #[cfg(test)]
            plant: Plant::None,
        };
        let decls = facts
            .decls(module)
            .ok_or(ResolveError::Missing { record: Record::ModuleDecls, module })?;
        r.add_fragment(decls, None, 0, None)?;
        if module == root {
            // `#[macro_export]` macros are bindings of the crate root (rustc's `MacroExport`).
            for &(name, def) in cr.macro_exports.iter() {
                let entry = DefEntry {
                    def: Def { key: def, kind: DefKind::Macro(MacroKind::Bang) },
                    vis: Visibility::Public,
                    origin: Origin::MacroExport,
                    expansion: 0,
                };
                r.frags[0].items.push((Namespace::Macros, name, entry));
                r.defs.entry((Namespace::Macros, name)).or_insert(entry);
            }
        }
        Ok(r)
    }

    /// The module this machine resolves.
    pub fn module(&self) -> ItemKey {
        self.module
    }

    pub(crate) fn asker(&self) -> Asker<'_> {
        Asker { krate: self.krate, chain: &self.chain }
    }

    /// A visibility restricted to `module`, in its one form.
    pub(crate) fn restricted(&self, module: ItemKey) -> Visibility {
        if module == self.root {
            Visibility::Crate(self.krate)
        } else {
            Visibility::Restricted(module)
        }
    }

    /// A written visibility resolved against the module's chain; a `pub(in path)` naming no
    /// ancestor is reported in `bad` and is private.
    fn resolve_vis<B: Allocator>(
        &self,
        vis: &VisDecl<N, B>,
        bad: &mut Vec<Path<N, A>, A>,
    ) -> Visibility {
        match vis {
            VisDecl::Private => self.restricted(self.module),
            VisDecl::Public => Visibility::Public,
            VisDecl::Crate => Visibility::Crate(self.krate),
            VisDecl::Super => match self.chain.get(1) {
                Some(&parent) => self.restricted(parent),
                None => {
                    bad.push(Path {
                        kind: PathKind::Super(1),
                        segments: Vec::new_in(self.alloc.clone()),
                    });
                    self.restricted(self.module)
                }
            },
            VisDecl::In(path) => {
                let top = self.chain.len() - 1;
                let start = match path.kind {
                    PathKind::Crate => Some(top),
                    PathKind::SelfMod => Some(0),
                    PathKind::Super(n) => {
                        let n = usize::from(n);
                        if n <= top { Some(n) } else { None }
                    }
                    PathKind::Plain if self.edition == Edition::E2015 => Some(top),
                    PathKind::Plain | PathKind::Abs | PathKind::DollarCrate(_) => None,
                };
                let mut at = start;
                for seg in path.segments.iter() {
                    at = match at {
                        Some(i) if i > 0 && self.chain_names[i - 1] == Some(*seg) => Some(i - 1),
                        _ => None,
                    };
                }
                match at {
                    Some(i) => self.restricted(self.chain[i]),
                    None => {
                        bad.push(path.copy_in(self.alloc.clone()));
                        self.restricted(self.module)
                    }
                }
            }
        }
    }

    /// Adds one fragment (the module's text or a call's expansion), returning its index.
    pub(crate) fn add_fragment<B: Allocator>(
        &mut self,
        decls: &ModuleDecls<N, B>,
        call: Option<ItemKey>,
        expansion: u16,
        parent: Option<At>,
    ) -> Result<u32, ResolveError> {
        let module = self.module;
        let fid = count(self.frags.len(), module)?;
        let a = self.alloc.clone();
        let mut items = Vec::new_in(a.clone());
        let mut bad_extern = Vec::new_in(a.clone());
        let mut bad_vis = Vec::new_in(a.clone());
        let mut ordered: Vec<(u32, Slot<N>), A> = Vec::new_in(a.clone());

        for it in decls.items.iter() {
            let vis = self.resolve_vis(&it.vis, &mut bad_vis);
            let entry = DefEntry { def: it.def, vis, origin: Origin::Declared, expansion };
            items.push((it.ns, it.name, entry));
            self.defs.entry((it.ns, it.name)).or_insert(entry);
        }
        for m in decls.mods.iter() {
            let vis = self.resolve_vis(&m.vis, &mut bad_vis);
            let entry =
                DefEntry { def: Def::module(m.module), vis, origin: Origin::Declared, expansion };
            items.push((Namespace::Types, m.name, entry));
            self.defs.entry((Namespace::Types, m.name)).or_insert(entry);
            ordered.push((m.pos, Slot::Mod { child: m.module, macro_use: m.macro_use }));
        }
        for ec in decls.extern_crates.iter() {
            let vis = self.resolve_vis(&ec.vis, &mut bad_vis);
            let (target, written) = match ec.target {
                ExternTarget::Dep(name) => {
                    (self.deps.iter().find(|d| d.name == name).map(|d| d.root), Some(name))
                }
                ExternTarget::SelfCrate => (Some(self.root), None),
            };
            let bound = match ec.rename {
                Rename::Same => written,
                Rename::To(name) => Some(name),
                Rename::Underscore => None,
            };
            match target {
                Some(root) => {
                    if let Some(name) = bound {
                        let entry = DefEntry {
                            def: Def::module(root),
                            vis,
                            origin: Origin::ExternCrate,
                            expansion,
                        };
                        items.push((Namespace::Types, name, entry));
                        self.defs.entry((Namespace::Types, name)).or_insert(entry);
                    }
                }
                None => {
                    if let Some(name) = written {
                        bad_extern.push(name);
                    }
                }
            }
        }
        for (index, u) in decls.uses.iter().enumerate() {
            let site = UseSite { expansion: call, index: count(index, module)? };
            let vis = self.resolve_vis(&u.vis, &mut bad_vis);
            let path = u.path.copy_in(a.clone());
            let placeholder = At { frag: fid, slot: 0 };
            match u.leaf {
                UseLeaf::Glob => {
                    let g = count(self.globs.len(), module)?;
                    self.globs.push(Glob {
                        site,
                        path,
                        vis,
                        expansion,
                        at: placeholder,
                        source: Source::Pending,
                    });
                    ordered.push((u.pos, Slot::Glob(g)));
                }
                UseLeaf::Named { rename, types_only } => {
                    let bound = match rename {
                        Rename::Same => path.segments.last().copied(),
                        Rename::To(name) => Some(name),
                        Rename::Underscore => None,
                    };
                    let i = count(self.imports.len(), module)?;
                    if let Some(name) = bound {
                        self.named.entry(name).or_insert_with(|| Vec::new_in(a.clone())).push(i);
                    }
                    self.imports.push(Import {
                        site,
                        path,
                        bound,
                        types_only,
                        vis,
                        expansion,
                        at: placeholder,
                        ns: [NsRes::Pending; 3],
                        failed: None,
                    });
                    ordered.push((u.pos, Slot::Use(i)));
                }
            }
        }
        for mr in decls.macro_rules.iter() {
            ordered.push((mr.pos, Slot::Rules { name: mr.name, def: mr.def }));
        }
        for c in decls.calls.iter() {
            let i = count(self.calls.len(), module)?;
            let state = if expansion >= RECURSION_LIMIT {
                CallState::TooDeep
            } else {
                self.unexpanded += 1;
                CallState::Pending
            };
            self.calls.push(Call {
                key: c.call,
                path: c.path.copy_in(a.clone()),
                kind: c.kind,
                expansion,
                at: At { frag: fid, slot: 0 },
                state,
            });
            ordered.push((c.pos, Slot::Call(i)));
        }
        ordered.sort_by_key(|&(pos, _)| pos);
        let mut slots = Vec::with_capacity_in(ordered.len(), a);
        for (s, &(_, slot)) in ordered.iter().enumerate() {
            let at = At { frag: fid, slot: count(s, module)? };
            match slot {
                Slot::Use(i) => self.imports[i as usize].at = at,
                Slot::Glob(i) => self.globs[i as usize].at = at,
                Slot::Call(i) => self.calls[i as usize].at = at,
                Slot::Rules { .. } | Slot::Mod { .. } => {}
            }
            slots.push(slot);
        }
        self.frags.push(Fragment { call, expansion, parent, slots, items, bad_extern, bad_vis });
        Ok(fid)
    }

    /// Keeps the answers of `replies`. An `InProgress` never replaces a final answer.
    pub(crate) fn absorb(&mut self, replies: &[Reply<'_, N>]) -> Result<(), ResolveError> {
        for reply in replies {
            let kept = match (reply.read, reply.answer) {
                (_, Answer::InProgress) => Cached::InProgress,
                (Read::Name { .. }, Answer::Name(answer)) => Cached::Name(answer),
                (Read::Exports { .. }, Answer::Exports { entries, digest }) => {
                    let mut v = Vec::with_capacity_in(entries.len(), self.alloc.clone());
                    v.extend_from_slice(entries);
                    v.sort_by(|x, y| (x.ns, x.name).cmp(&(y.ns, y.name)));
                    Cached::Exports { entries: v, digest }
                }
                (
                    Read::TextualEntry { .. } | Read::TextualOut { .. },
                    Answer::Textual { defs, digest },
                ) => {
                    let mut v = Vec::with_capacity_in(defs.len(), self.alloc.clone());
                    v.extend_from_slice(defs);
                    Cached::Textual { defs: v, digest }
                }
                _ => {
                    return Err(ResolveError::WrongAnswer {
                        module: self.module,
                        read_of: reply.read.module(),
                    });
                }
            };
            let keep_old = matches!(kept, Cached::InProgress)
                && matches!(self.answers.get(&reply.read), Some(c) if !matches!(c, Cached::InProgress));
            if !keep_old {
                self.answers.insert(reply.read, kept);
            }
        }
        Ok(())
    }

    /// The answer to `read`, recording it in `sink`: a missing one is needed, one in progress
    /// is waited on, any other is used.
    pub(crate) fn cached(&self, read: Read<N>, sink: &mut Sink<N, A>) -> Option<&Cached<N, A>> {
        match self.answers.get(&read) {
            None => {
                if sink.on {
                    sink.need.push(read);
                }
                None
            }
            Some(Cached::InProgress) => {
                if sink.on {
                    sink.waits.push(read);
                }
                None
            }
            Some(c) => {
                #[cfg(test)]
                let forget = sink.forget_negative && matches!(c, Cached::Name(NameAnswer::Absent));
                #[cfg(not(test))]
                let forget = false;
                if sink.on && !forget {
                    sink.used.push(read);
                }
                Some(c)
            }
        }
    }

    /// Plans one pass: every import, glob and call that can move, from the state as it is.
    pub(crate) fn plan<F: ScopeFacts<Name = N>>(
        &self,
        facts: &F,
        env: Env<'_, N, A>,
        force: bool,
        sink: &mut Sink<N, A>,
    ) -> Result<Plan<A>, ResolveError> {
        let a = self.alloc.clone();
        let mut plan = Plan {
            imports: Vec::new_in(a.clone()),
            globs: Vec::new_in(a.clone()),
            calls: Vec::new_in(a),
        };
        for g in 0..self.globs.len() {
            let i = count(g, self.module)?;
            match &self.globs[g].source {
                Source::Pending => {
                    if let Some(source) = self.plan_glob(i, facts, force, env, sink)? {
                        plan.globs.push((i, source));
                    }
                }
                Source::Resolved { def, classes, .. } => {
                    // A glob's source is read whole once it is known: its names, and the
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
            let i = count(i, self.module)?;
            if let Some(update) = self.plan_import(i, force, env, sink) {
                plan.imports.push(update);
            }
        }
        for c in 0..self.calls.len() {
            let i = count(c, self.module)?;
            if self.calls[c].state == CallState::Pending {
                if let Some(state) = self.plan_call(i, force, env, sink) {
                    plan.calls.push((i, state));
                }
            }
        }
        Ok(plan)
    }

    /// Applies a plan, then takes in every expansion now available. Whether anything moved.
    pub(crate) fn apply<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        plan: Plan<A>,
    ) -> Result<bool, ResolveError> {
        let mut moved = false;
        for (i, ns, failed) in plan.imports {
            let imp = &mut self.imports[i as usize];
            if imp.ns != ns || imp.failed != failed {
                imp.ns = ns;
                imp.failed = failed;
                moved = true;
            }
        }
        for (i, source) in plan.globs {
            self.globs[i as usize].source = source;
            moved = true;
        }
        for (i, state) in plan.calls {
            let call = &mut self.calls[i as usize];
            if call.state != state {
                if matches!(state, CallState::Failed(..) | CallState::Inert) {
                    self.unexpanded -= 1;
                }
                call.state = state;
                moved = true;
            }
        }
        for c in 0..self.calls.len() {
            let CallState::Resolved(def) = self.calls[c].state else { continue };
            let key = self.calls[c].key;
            let Some(expanded) = facts.expansion(key) else { continue };
            self.expansions += 1;
            if self.expansions > MAX_EXPANSIONS {
                return Err(ResolveError::TooManyExpansions { module: self.module });
            }
            let depth = self.calls[c].expansion + 1;
            let at = self.calls[c].at;
            let frag = self.add_fragment(expanded, Some(key), depth, Some(at))?;
            self.calls[c].state = CallState::Expanded { frag, def };
            self.unexpanded -= 1;
            moved = true;
        }
        Ok(moved)
    }

    /// The most passes that can each move something: every import namespace, glob and call
    /// state moves forward only (rule 18).
    pub(crate) fn pass_bound(&self) -> u32 {
        let work = 4 * self.imports.len() + 2 * self.globs.len() + 3 * self.calls.len() + 4;
        u32::try_from(work).unwrap_or(u32::MAX)
    }

    /// Keeps what the last pass, which moved nothing, needed and waited on.
    pub(crate) fn keep_needs(&mut self, sink: Sink<N, A>) {
        let Sink { mut need, mut waits, .. } = sink;
        need.sort();
        need.dedup();
        waits.sort();
        waits.dedup();
        self.asks = need;
        self.wait_reads = waits;
        let mut keys = Vec::with_capacity_in(self.wait_reads.len(), self.alloc.clone());
        for read in self.wait_reads.iter() {
            if let Some(owner) = self.owner(read) {
                keys.push(owner);
            }
        }
        keys.sort();
        keys.dedup();
        self.waits = keys;
    }

    /// The module whose record answers `read`: a `TextualEntry` of this module is its
    /// parent's; any other read is of the module it names.
    pub(crate) fn owner(&self, read: &Read<N>) -> Option<ItemKey> {
        match *read {
            Read::TextualEntry { module } if module == self.module => self.chain.get(1).copied(),
            _ => Some(read.module()),
        }
    }

    /// The calls asked to be expanded, sorted.
    pub(crate) fn collect_expands(&mut self) {
        self.expand.clear();
        for call in self.calls.iter() {
            if let CallState::Resolved(def) = call.state {
                self.expand.push(ExpandRequest { call: call.key, def });
            }
        }
        self.expand.sort();
    }

    /// Passes until one moves nothing.
    fn settle<F: ScopeFacts<Name = N>>(&mut self, facts: &F) -> Result<(), ResolveError> {
        let mut passes = 0u32;
        loop {
            let mut sink = Sink::new_in(true, &self.alloc);
            let plan = self.plan(facts, Env::solo(), false, &mut sink)?;
            let moved = self.apply(facts, plan)?;
            passes += 1;
            if !moved {
                self.keep_needs(sink);
                return Ok(());
            }
            if passes > self.pass_bound() {
                return Err(ResolveError::NoProgressBound { module: self.module, passes });
            }
        }
    }

    /// Whether a call is still pending.
    pub(crate) fn has_pending_calls(&self) -> bool {
        self.calls.iter().any(|c| c.state == CallState::Pending)
    }

    /// Plans the forced resolution of the pending calls (rustc's `force` when expansion is
    /// stuck): a lookup that only a glob or an expansion could still change goes on to outer
    /// scopes, one that something else could change fails.
    pub(crate) fn plan_forced_calls(
        &self,
        env: Env<'_, N, A>,
        sink: &mut Sink<N, A>,
    ) -> Result<Plan<A>, ResolveError> {
        let a = self.alloc.clone();
        let mut plan = Plan {
            imports: Vec::new_in(a.clone()),
            globs: Vec::new_in(a.clone()),
            calls: Vec::new_in(a),
        };
        for c in 0..self.calls.len() {
            if self.calls[c].state == CallState::Pending {
                let i = count(c, self.module)?;
                if let Some(state) = self.plan_call(i, true, env, sink) {
                    plan.calls.push((i, state));
                }
            }
        }
        Ok(plan)
    }

    /// Fails every import and glob still pending: nothing can move them any more.
    pub(crate) fn fail_pending(&mut self) -> bool {
        let mut moved = false;
        for imp in self.imports.iter_mut() {
            if imp.done() {
                continue;
            }
            let mut any = false;
            for ns in Namespace::ALL {
                match imp.ns[ns.index()] {
                    NsRes::Pending if imp.imports(ns) => imp.ns[ns.index()] = NsRes::Absent,
                    NsRes::Found(_) => any = true,
                    _ => {}
                }
            }
            if !any {
                let last = u32::try_from(imp.path.segments.len().saturating_sub(1)).unwrap_or(0);
                imp.failed = Some((last, Unresolved::Undetermined));
            }
            moved = true;
        }
        for g in self.globs.iter_mut() {
            if matches!(g.source, Source::Pending) {
                g.source = Source::Failed(0, Unresolved::Undetermined);
                moved = true;
            }
        }
        moved
    }

    /// One step of the protocol: takes `replies`, runs until it needs something, and says what.
    pub fn step<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        replies: &[Reply<'_, N>],
    ) -> Result<Step<'_, N, Resolved<N, A>>, ResolveError> {
        if self.finished {
            return Err(ResolveError::Finished { module: self.module });
        }
        self.absorb(replies)?;
        let mut forced = 0u32;
        loop {
            self.settle(facts)?;
            if !self.asks.is_empty() {
                return Ok(Step::Ask(&self.asks));
            }
            self.collect_expands();
            if !self.expand.is_empty() {
                return Ok(Step::Expand(&self.expand));
            }
            if !self.waits.is_empty() {
                return Ok(Step::Cycle(&self.waits));
            }
            let mut moved = false;
            if self.has_pending_calls() {
                let mut sink = Sink::new_in(true, &self.alloc);
                let plan = self.plan_forced_calls(Env::solo(), &mut sink)?;
                moved = self.apply(facts, plan)?;
            }
            if !moved {
                moved = self.fail_pending();
            }
            if moved {
                forced += 1;
                if forced > self.pass_bound() {
                    return Err(ResolveError::NoProgressBound { module: self.module, passes: forced });
                }
                continue;
            }
            match self.finalize(Env::solo(), &[])? {
                crate::finish::Finish::Need { asks, waits } => {
                    self.asks = asks;
                    self.wait_reads = waits;
                    let mut keys = Vec::new_in(self.alloc.clone());
                    for read in self.wait_reads.iter() {
                        if let Some(owner) = self.owner(read) {
                            keys.push(owner);
                        }
                    }
                    keys.sort();
                    keys.dedup();
                    self.waits = keys;
                    if !self.asks.is_empty() {
                        return Ok(Step::Ask(&self.asks));
                    }
                    return Ok(Step::Cycle(&self.waits));
                }
                crate::finish::Finish::Done(resolved) => {
                    self.finished = true;
                    return Ok(Step::Done(resolved));
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_plant(&mut self, plant: Plant) {
        self.plant = plant;
    }
}
