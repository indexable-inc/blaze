//! [`Resolver`]: one module's name resolution as a sans-IO state machine, and the loop that
//! runs one machine or the members of a group.
//!
//! The machine holds what the module declares (its own text and each expansion as a
//! *fragment*), the state of each `use`, glob and macro call, and the answers it was given.
//! A step runs passes until nothing moves (each pass plans every update from the state as it
//! is, then applies them all), and then returns what it needs: reads, expansions, or the
//! modules of a cycle. When nothing is pending it forces what is stuck (macro paths first,
//! then imports), as rustc does, and at last re-resolves everything once against the final
//! state: that pass checks each early resolution with rustc's ambiguity kinds and is what the
//! read list is made of.
//!
//! Order independence. An item still pending is *blocked* while what it waits on is an answer
//! not given yet, and a lookup that meets a blocked item waits too, where it would otherwise
//! take the item as undetermined and go on to an outer scope (rustc's weak undeterminacy).
//! So no decision is made on a guess that one more answer would change: a step's result is a
//! function of the records, not of the order or the batches the answers came in. Blocked
//! flags are set conservatively after every move and lowered to their least fixpoint only
//! when no answer is outstanding (`relax`), which is when "undetermined" means what rustc
//! means by it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::cmp::Ordering;

use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{
    AmbiguityKind, BindingState, CallShape, DeclsBuilder, Def, DefKind, Dep, Edition, ExportEntry,
    ExternTarget, Namespace, Origin, Path, PathKind, Rename, TextualDef, Unresolved, UseForm,
    UseSite, Visibility, VisibilityClass, WrittenVis,
};

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::finish::Finish;
use crate::read::{Answer, ExpandRequest, NameAnswer, Read, Reply, Step};
use crate::scope::{Asker, ReadEntry, Resolved, Saw};

/// The most parents a module may have (rule 18); no real crate comes near it.
pub const MAX_MODULE_DEPTH: usize = 128;

/// rustc's default `recursion_limit`: a macro call written at this expansion depth is an error.
pub const RECURSION_LIMIT: u16 = 128;

/// The most macro expansions one module may take in (rule 18).
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

/// A non-glob declaration: an item, a `mod`, an `extern crate`, a macro export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DefEntry {
    pub(crate) def: Def,
    pub(crate) vis: Visibility,
    pub(crate) origin: Origin,
    pub(crate) expansion: u16,
    pub(crate) frag: u32,
}

/// The module's own text, or one call's expansion.
pub(crate) struct Fragment<'a, N> {
    pub(crate) expansion: u16,
    /// The call it is the expansion of, for a lookup that runs off the fragment's start.
    pub(crate) parent: Option<At>,
    pub(crate) slots: AVec<'a, Slot<N>>,
    /// Its non-glob declarations, in the order written.
    pub(crate) items: AVec<'a, (Namespace, N, DefEntry)>,
    /// Its `extern crate`s of no dependency.
    pub(crate) bad_extern: AVec<'a, N>,
    /// Its `pub(in path)`s whose path names no ancestor.
    pub(crate) bad_vis: AVec<'a, Path<'a, N>>,
    /// Its declarations' `pub(in path)`s whose path is not the names of an ancestor as written
    /// (`pub(in crate::alias)`): private while the module resolves, and resolved as paths in
    /// the final pass, which writes the binding with the module the path names.
    pub(crate) deferred_vis: AVec<'a, (Path<'a, N>, Namespace, N, Def)>,
    /// Its `#[macro_export]` `macro_rules!`, in an expansion only.
    pub(crate) exports: AVec<'a, TextualDef<N>>,
}

/// Which scope a hit was found in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Layer {
    Textual,
    Module,
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
    /// The fragment of this machine it was written in; `None` for a binding of another
    /// module or a prelude (which rustc's expansion order puts at the root).
    pub(crate) frag: Option<u32>,
    pub(crate) state: BindingState,
    pub(crate) layer: Layer,
}

impl Hit {
    /// A module named by a path's start (`crate`, `self`, `super`, `$crate`).
    pub(crate) fn module(key: ItemKey) -> Hit {
        Hit {
            def: Def::module(key),
            vis: Visibility::Public,
            origin: Origin::Declared,
            expansion: 0,
            frag: None,
            state: BindingState::Determined,
            layer: Layer::Path,
        }
    }
}

impl DefEntry {
    pub(crate) fn hit(&self) -> Hit {
        Hit {
            def: self.def,
            vis: self.vis,
            origin: self.origin,
            expansion: self.expansion,
            frag: Some(self.frag),
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
    /// Not determined. `weak`: only a glob or a macro expansion can still define it, so an
    /// early lexical lookup goes on to outer scopes (rustc's `Weak::Yes`).
    Undet { weak: bool },
    /// An answer not given yet decides it.
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

pub(crate) struct Import<'a, N> {
    pub(crate) site: UseSite,
    pub(crate) path: Path<'a, N>,
    /// What it binds; `None` for `as _`.
    pub(crate) bound: Option<N>,
    pub(crate) types_only: bool,
    pub(crate) vis: Visibility,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) ns: [NsRes; 3],
    pub(crate) failed: Option<(u32, Unresolved)>,
    /// What its first segment resolved to, early.
    pub(crate) first: Option<Def>,
    pub(crate) blocked: bool,
}

impl<N> Import<'_, N> {
    /// Whether it imports namespace `ns`.
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

/// A glob's source.
pub(crate) enum Source<'a> {
    Pending,
    /// A module or an enum, read in the visibility classes the importer can see; an enum's
    /// variants are as visible as the enum (`clamp`).
    Resolved { def: Def, clamp: Option<Visibility>, classes: AVec<'a, VisibilityClass> },
    /// `use self::*`: imports nothing new.
    Itself,
    Failed(u32, Unresolved),
}

pub(crate) struct Glob<'a, N> {
    pub(crate) site: UseSite,
    pub(crate) path: Path<'a, N>,
    pub(crate) vis: Visibility,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) source: Source<'a>,
    pub(crate) first: Option<Def>,
    pub(crate) blocked: bool,
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

impl CallState {
    /// Whether the call may still add names.
    pub(crate) fn unexpanded(self) -> bool {
        matches!(self, CallState::Pending | CallState::Resolved(_))
    }
}

pub(crate) struct Call<'a, N> {
    pub(crate) key: ItemKey,
    pub(crate) path: Path<'a, N>,
    pub(crate) kind: CallShape,
    pub(crate) at: At,
    /// The item it is on, in its fragment (an attribute or a derive).
    pub(crate) item: Option<u32>,
    pub(crate) state: CallState,
    pub(crate) first: Option<Def>,
    pub(crate) blocked: bool,
}

/// An answer as kept.
pub(crate) enum Cached<'a, N> {
    Name(NameAnswer),
    Exports { entries: AVec<'a, ExportEntry<N>>, digest: ContentId },
    Textual { defs: AVec<'a, TextualDef<N>>, digest: ContentId },
    Helpers { names: AVec<'a, N>, digest: ContentId },
    InProgress,
}

/// What a pass is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scan {
    /// Resolve what can be resolved.
    Early,
    /// Expansion is stuck: a macro path still undetermined fails (rustc's `force`).
    Force,
    /// The last pass: every layer is visited for ambiguity, and the reads used are kept.
    Final,
}

/// An ambiguity an early resolution meets in the final pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Amb<N> {
    pub(crate) ns: Namespace,
    pub(crate) name: N,
    pub(crate) kind: AmbiguityKind,
    pub(crate) first: Def,
    pub(crate) second: Def,
}

/// Where a pass's reads go: those it needs and has no answer to, those answered `InProgress`,
/// and in the final pass those it used with what they saw.
pub(crate) struct Sink<'a, N> {
    pub(crate) scan: Scan,
    pub(crate) need: AVec<'a, Read<N>>,
    pub(crate) waits: AVec<'a, Read<N>>,
    pub(crate) used: AVec<'a, ReadEntry<N>>,
    pub(crate) amb: Option<Amb<N>>,
    /// The group's glob fixpoints of this pass, by `(ns, name, ignored import)`: for each glob
    /// edge, a fellow `f` and an importer `i` (member indexes, sorted), what `f` binds the
    /// name to as `i` sees it. The state does not change while a pass plans, so one is
    /// computed once.
    pub(crate) memo: HashMap<(Namespace, N, Option<(ItemKey, u32)>), AVec<'a, (usize, usize, Res)>, Alloc<'a>>,
    /// A fixpoint ran past its bound: the pass stops with a typed error.
    pub(crate) overrun: bool,
    /// The earliest glob of the module being finished that left a name undetermined.
    pub(crate) undet_glob: Option<(ItemKey, At, u32)>,
    /// The planted bug of the read-list control: a read that found nothing is not kept.
    #[cfg(test)]
    pub(crate) forget_negative: bool,
}

impl<'a, N: Name> Sink<'a, N> {
    pub(crate) fn new_in(alloc: Alloc<'a>, scan: Scan) -> Sink<'a, N> {
        Sink {
            scan,
            need: Vec::new_in(alloc.clone()),
            waits: Vec::new_in(alloc.clone()),
            used: Vec::new_in(alloc.clone()),
            amb: None,
            memo: HashMap::new_in(alloc.clone()),
            overrun: false,
            undet_glob: None,
            #[cfg(test)]
            forget_negative: false,
        }
    }

    /// The typed error of a fixpoint that ran past its bound, if one did.
    pub(crate) fn check(&self, module: ItemKey) -> Result<(), ResolveError> {
        if self.overrun { Err(ResolveError::GlobFixpointBound { module }) } else { Ok(()) }
    }
}

/// The machines a lookup reads live: the members of the group being solved, sorted by module
/// (a machine alone is a group of one).
pub(crate) struct Env<'e, 'a, N> {
    pub(crate) members: &'e [Resolver<'a, N>],
}

impl<N> Clone for Env<'_, '_, N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N> Copy for Env<'_, '_, N> {}

impl<'e, 'a, N> Env<'e, 'a, N> {
    pub(crate) fn fellow(&self, module: ItemKey) -> Option<&'e Resolver<'a, N>> {
        self.index(module).map(|at| &self.members[at])
    }

    pub(crate) fn index(&self, module: ItemKey) -> Option<usize> {
        self.members.binary_search_by(|m| m.module.cmp(&module)).ok()
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
}

/// A pending item, for its blocked flag.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Item {
    Import(u32),
    Glob(u32),
    Call(u32),
}

/// What resolving an import found, before it is merged into its state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImportOutcome {
    pub(crate) ns: [NsRes; 3],
    pub(crate) failed: Option<(u32, Unresolved)>,
    pub(crate) first: Option<Def>,
    pub(crate) wait: bool,
}

/// The updates one pass plans, applied after.
pub(crate) struct Plan<'a> {
    pub(crate) imports: AVec<'a, (u32, [NsRes; 3], Option<(u32, Unresolved)>, Option<Def>)>,
    pub(crate) globs: AVec<'a, (u32, Source<'a>, Option<Def>)>,
    pub(crate) calls: AVec<'a, (u32, CallState, Option<Def>)>,
    /// The pending items whose plan waited on an answer, sorted.
    pub(crate) waiting: AVec<'a, Item>,
    /// How many imports, globs and calls there were when it was planned.
    pub(crate) upto: [usize; 3],
}

/// What a step hands its caller, kept so the step can lend it.
pub(crate) struct Outbox<'a, N> {
    pub(crate) asks: AVec<'a, Read<N>>,
    pub(crate) expand: AVec<'a, ExpandRequest>,
    pub(crate) cycle: AVec<'a, ItemKey>,
}

impl<'a, N> Outbox<'a, N> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Outbox<'a, N> {
        Outbox {
            asks: Vec::new_in(alloc.clone()),
            expand: Vec::new_in(alloc.clone()),
            cycle: Vec::new_in(alloc.clone()),
        }
    }
}

/// What [`run`] stopped on.
pub(crate) enum Ran<'a, N> {
    Ask,
    Expand,
    Cycle,
    Done(AVec<'a, Resolved<'a, N>>),
}

/// The planted bugs the tests' controls switch on.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The read list leaves out the reads that found nothing.
    ForgetNegativeReads,
}

/// One module's name resolution (the protocol is in the crate's documentation).
pub struct Resolver<'a, N> {
    pub(crate) module: ItemKey,
    pub(crate) krate: CrateItemKey,
    pub(crate) root: ItemKey,
    pub(crate) edition: Edition,
    /// The module, its parent, ..., the crate root.
    pub(crate) chain: AVec<'a, ItemKey>,
    /// The name of each of `chain` in its parent.
    pub(crate) chain_names: AVec<'a, Option<N>>,
    /// No `#[no_implicit_prelude]` on the module or above it.
    pub(crate) implicit_prelude: bool,
    pub(crate) extern_prelude: AVec<'a, (N, ItemKey)>,
    pub(crate) deps: AVec<'a, Dep<N>>,
    pub(crate) macro_use: AVec<'a, ItemKey>,
    pub(crate) prelude: Option<ItemKey>,
    pub(crate) tools: AVec<'a, N>,
    /// Sorted.
    pub(crate) builtin_attrs: AVec<'a, N>,
    pub(crate) frags: AVec<'a, Fragment<'a, N>>,
    /// The non-glob declarations by namespace and name, the first in the module's tree kept.
    pub(crate) defs: HashMap<(Namespace, N), DefEntry, Alloc<'a>>,
    pub(crate) imports: AVec<'a, Import<'a, N>>,
    /// The imports by the name they bind.
    pub(crate) named: HashMap<N, AVec<'a, u32>, Alloc<'a>>,
    pub(crate) globs: AVec<'a, Glob<'a, N>>,
    pub(crate) calls: AVec<'a, Call<'a, N>>,
    /// Calls that may still add names (pending or asked to be expanded).
    pub(crate) unexpanded: u32,
    pub(crate) expansions: usize,
    pub(crate) answers: HashMap<Read<N>, Cached<'a, N>, Alloc<'a>>,
    pub(crate) out: Outbox<'a, N>,
    pub(crate) finished: bool,
    pub(crate) alloc: Alloc<'a>,
    #[cfg(test)]
    pub(crate) plant: Plant,
}

fn count(n: usize, module: ItemKey) -> Result<u32, ResolveError> {
    u32::try_from(n).map_err(|_| ResolveError::TooManyExpansions { module })
}

impl<'a, N: Name> Resolver<'a, N> {
    /// A machine for `module`, made from its records in `facts`, allocating in `alloc`.
    pub fn new_in<F: ScopeFacts<Name = N>>(
        module: ItemKey,
        facts: &F,
        alloc: Alloc<'a>,
    ) -> Result<Resolver<'a, N>, ResolveError> {
        let surface = |error| ResolveError::Surface { module, error };
        let info = facts.module(module).map_err(surface)?;
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
            let up = facts.module(p).map_err(surface)?;
            chain.push(p);
            chain_names.push(up.name);
            implicit_prelude &= !up.no_implicit_prelude;
            parent = up.parent;
        }
        let root = chain[chain.len() - 1];
        let krate = info.krate;
        let cr = facts.crate_root(krate).map_err(surface)?;
        let mut extern_prelude = Vec::from_slice_in(&cr.extern_prelude, alloc.clone());
        // By name, then key: a total order, so a working form made by hand (which may hold a
        // name twice, where a record cannot) still gives one result.
        extern_prelude.sort_unstable();
        let deps = Vec::from_slice_in(&cr.deps, alloc.clone());
        let macro_use = Vec::from_slice_in(&cr.macro_use, alloc.clone());
        let tools = Vec::from_slice_in(&cr.tools, alloc.clone());
        let mut builtin_attrs = Vec::from_slice_in(&cr.builtin_attrs, alloc.clone());
        builtin_attrs.sort_unstable();
        let mut r = Resolver {
            module,
            krate,
            root,
            edition: cr.head.edition,
            chain,
            chain_names,
            implicit_prelude,
            extern_prelude,
            deps,
            macro_use,
            prelude: cr.head.prelude,
            tools,
            builtin_attrs,
            frags: Vec::new_in(alloc.clone()),
            defs: HashMap::new_in(alloc.clone()),
            imports: Vec::new_in(alloc.clone()),
            named: HashMap::new_in(alloc.clone()),
            globs: Vec::new_in(alloc.clone()),
            calls: Vec::new_in(alloc.clone()),
            unexpanded: 0,
            expansions: 0,
            answers: HashMap::new_in(alloc.clone()),
            out: Outbox::new_in(alloc),
            finished: false,
            alloc,
            #[cfg(test)]
            plant: Plant::None,
        };
        let decls = facts.decls(module).map_err(surface)?;
        r.add_fragment(&decls, None, 0, None)?;
        if module == root {
            // `#[macro_export]` macros are bindings of the crate root (rustc's `MacroExport`).
            for &(name, def) in cr.macro_exports.iter() {
                let entry = DefEntry {
                    def: Def { key: def, kind: DefKind::Macro(CallShape::FnLike) },
                    vis: Visibility::Public,
                    origin: Origin::MacroExport,
                    expansion: 0,
                    frag: 0,
                };
                r.frags[0].items.push((Namespace::Macros, name, entry));
                r.insert_def(Namespace::Macros, name, entry);
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
        if module == self.root { Visibility::Crate(self.krate) } else { Visibility::Restricted(module) }
    }

    /// A written visibility resolved against the module's own chain; a `pub(in path)` naming
    /// no ancestor, and a `pub(path)` written without `in` (which rustc refuses, E0704), go to
    /// `bad` and are private. `pub(self)` is private.
    fn resolve_vis(
        &self,
        vis: &WrittenVis<Path<'_, N>>,
        bad: &mut AVec<'a, Path<'a, N>>,
    ) -> Visibility {
        match vis {
            WrittenVis::Private | WrittenVis::SelfOnly => self.restricted(self.module),
            WrittenVis::WithoutIn(path) => {
                bad.push(path.copy_in(self.alloc.clone()));
                self.restricted(self.module)
            }
            WrittenVis::Pub => Visibility::Public,
            WrittenVis::Crate => Visibility::Crate(self.krate),
            WrittenVis::Super => match self.chain.get(1) {
                Some(&parent) => self.restricted(parent),
                None => {
                    bad.push(Path {
                        kind: PathKind::Super(1),
                        segments: Vec::new_in(self.alloc.clone()),
                    });
                    self.restricted(self.module)
                }
            },
            WrittenVis::In(path) => {
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
                        Some(_) | None => None,
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

    /// A declaration's visibility. A `pub(in path)` from `crate`, `self` or `super` whose
    /// names are not an ancestor's as written may go through an alias, so it is deferred to
    /// the final pass (private until then) rather than refused.
    fn resolve_decl_vis(
        &self,
        vis: &WrittenVis<Path<'_, N>>,
        ns: Namespace,
        name: N,
        def: Def,
        bad: &mut AVec<'a, Path<'a, N>>,
        deferred: &mut AVec<'a, (Path<'a, N>, Namespace, N, Def)>,
    ) -> Visibility {
        let before = bad.len();
        let resolved = self.resolve_vis(vis, bad);
        if let WrittenVis::In(path) = vis {
            let as_path = match path.kind {
                PathKind::Crate | PathKind::SelfMod | PathKind::Super(_) => true,
                PathKind::Plain => self.edition == Edition::E2015,
                PathKind::Abs | PathKind::DollarCrate(_) => false,
            };
            if as_path && bad.len() > before {
                if let Some(p) = bad.pop() {
                    deferred.push((p, ns, name, def));
                }
            }
        }
        resolved
    }

    /// Adds one fragment (the module's text or a call's expansion), returning its index.
    pub(crate) fn add_fragment(
        &mut self,
        decls: &DeclsBuilder<'_, N>,
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
        let mut deferred_vis = Vec::new_in(a.clone());
        let mut exports = Vec::new_in(a.clone());
        let mut ordered: AVec<'a, (u32, Slot<N>)> = Vec::new_in(a.clone());
        let entry = |def: Def, vis: Visibility, origin: Origin| DefEntry {
            def,
            vis,
            origin,
            expansion,
            frag: fid,
        };

        for it in decls.items.iter() {
            let vis = self.resolve_decl_vis(&it.vis, it.ns, it.name, it.def, &mut bad_vis, &mut deferred_vis);
            items.push((it.ns, it.name, entry(it.def, vis, Origin::Declared)));
        }
        for m in decls.mods.iter() {
            let def = Def::module(m.module);
            let vis = self.resolve_decl_vis(&m.vis, Namespace::Types, m.name, def, &mut bad_vis, &mut deferred_vis);
            items.push((Namespace::Types, m.name, entry(def, vis, Origin::Declared)));
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
            match (target, bound) {
                (Some(root), Some(name)) => {
                    items.push((Namespace::Types, name, entry(Def::module(root), vis, Origin::ExternCrate)));
                }
                (Some(_), None) => {}
                (None, _) => {
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
                UseForm::Glob => {
                    let g = count(self.globs.len(), module)?;
                    self.globs.push(Glob {
                        site,
                        path,
                        vis,
                        expansion,
                        at: placeholder,
                        source: Source::Pending,
                        first: None,
                        blocked: true,
                    });
                    ordered.push((u.pos, Slot::Glob(g)));
                }
                UseForm::Named { rename, types_only } => {
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
                        first: None,
                        blocked: true,
                    });
                    ordered.push((u.pos, Slot::Use(i)));
                }
            }
        }
        for mr in decls.macro_rules.iter() {
            ordered.push((mr.pos, Slot::Rules { name: mr.name, def: mr.def }));
            if mr.export && expansion > 0 {
                exports.push(TextualDef { name: mr.name, def: mr.def, expansion });
            }
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
                at: At { frag: fid, slot: 0 },
                item: c.item,
                state,
                first: None,
                blocked: true,
            });
            ordered.push((c.pos, Slot::Call(i)));
        }
        ordered.sort_unstable_by_key(|&(pos, _)| pos);
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
        self.frags.push(Fragment { expansion, parent, slots, items, bad_extern, bad_vis, deferred_vis, exports });
        for k in 0..self.frags[fid as usize].items.len() {
            let (ns, name, e) = self.frags[fid as usize].items[k];
            self.insert_def(ns, name, e);
        }
        Ok(fid)
    }

    /// Keeps `e` as the binding of `(ns, name)` unless one written before it in the module's
    /// tree is kept (the later one is a `Duplicate` error of the final pass).
    fn insert_def(&mut self, ns: Namespace, name: N, e: DefEntry) {
        let replace = match self.defs.get(&(ns, name)).copied() {
            Some(old) => self.frag_before(e.frag, old.frag),
            None => true,
        };
        if replace {
            self.defs.insert((ns, name), e);
        }
    }

    /// The slots of the calls a fragment is inside, from the module's text down.
    fn frag_path(&self, frag: u32) -> AVec<'a, u32> {
        let mut path = Vec::new_in(self.alloc.clone());
        let mut cur = frag;
        while let Some(p) = self.frags[cur as usize].parent {
            path.push(p.slot);
            cur = p.frag;
        }
        path.reverse();
        path
    }

    /// Whether fragment `a` is before `b` in the module's tree (an enclosing fragment first).
    fn frag_before(&self, a: u32, b: u32) -> bool {
        a != b && self.frag_path(a).as_slice() < self.frag_path(b).as_slice()
    }

    /// The order of two places in the module's tree: what "written first" means once
    /// expansions are in, whatever order they were taken in.
    pub(crate) fn tree_cmp(&self, a: At, b: At) -> Ordering {
        let mut pa = self.frag_path(a.frag);
        pa.push(a.slot);
        let mut pb = self.frag_path(b.frag);
        pb.push(b.slot);
        pa.as_slice().cmp(pb.as_slice())
    }

    /// Whether fragment `a` is `b` or inside `b`.
    pub(crate) fn is_desc(&self, a: u32, b: u32) -> bool {
        let mut cur = a;
        loop {
            if cur == b {
                return true;
            }
            match self.frags[cur as usize].parent {
                Some(p) => cur = p.frag,
                None => return false,
            }
        }
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
                    v.sort_unstable_by(|x, y| (x.ns, x.name).cmp(&(y.ns, y.name)));
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
                (Read::DeriveHelpers { .. }, Answer::Helpers { names, digest }) => {
                    let mut v = Vec::with_capacity_in(names.len(), self.alloc.clone());
                    v.extend_from_slice(names);
                    v.sort_unstable();
                    Cached::Helpers { names: v, digest }
                }
                (
                    Read::Name { .. }
                    | Read::Exports { .. }
                    | Read::TextualEntry { .. }
                    | Read::TextualOut { .. }
                    | Read::DeriveHelpers { .. },
                    Answer::Name(_) | Answer::Exports { .. } | Answer::Textual { .. } | Answer::Helpers { .. },
                ) => {
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

    /// The answer to `read`: a missing one is needed, one in progress is waited on, any other
    /// is used (and kept with what it saw, in the final pass).
    pub(crate) fn cached(&self, read: Read<N>, sink: &mut Sink<'a, N>) -> Option<&Cached<'a, N>> {
        let Some(c) = self.answers.get(&read) else {
            sink.need.push(read);
            return None;
        };
        let saw = match c {
            Cached::Name(answer) => Saw::Name(*answer),
            Cached::Exports { digest, .. } | Cached::Textual { digest, .. } | Cached::Helpers { digest, .. } => {
                Saw::Digest(*digest)
            }
            Cached::InProgress => {
                sink.waits.push(read);
                return None;
            }
        };
        if sink.scan == Scan::Final {
            #[cfg(test)]
            let keep = !(sink.forget_negative && saw == Saw::Name(NameAnswer::Absent));
            #[cfg(not(test))]
            let keep = true;
            if keep {
                sink.used.push(ReadEntry { read, saw });
            }
        }
        Some(c)
    }

    /// Applies a plan, then takes in every expansion now given. Whether anything moved.
    pub(crate) fn apply<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        plan: Plan<'a>,
    ) -> Result<bool, ResolveError> {
        let mut moved = false;
        self.set_flags(&plan.waiting, plan.upto);
        for (i, ns, failed, first) in plan.imports {
            let imp = &mut self.imports[i as usize];
            if imp.ns != ns || imp.failed != failed {
                moved = true;
            }
            imp.ns = ns;
            imp.failed = failed;
            if imp.first.is_none() {
                imp.first = first;
            }
        }
        for (i, source, first) in plan.globs {
            let g = &mut self.globs[i as usize];
            g.source = source;
            if g.first.is_none() {
                g.first = first;
            }
            moved = true;
        }
        for (i, state, first) in plan.calls {
            let old = self.calls[i as usize].state;
            if old != state {
                if old.unexpanded() && !state.unexpanded() {
                    self.unexpanded -= 1;
                }
                let call = &mut self.calls[i as usize];
                call.state = state;
                if call.first.is_none() {
                    call.first = first;
                }
                moved = true;
            }
        }
        for c in 0..self.calls.len() {
            let CallState::Resolved(def) = self.calls[c].state else { continue };
            let key = self.calls[c].key;
            let expanded = facts
                .expansion(key)
                .map_err(|error| ResolveError::Surface { module: self.module, error })?;
            let Some(expanded) = expanded else { continue };
            self.expansions += 1;
            if self.expansions > MAX_EXPANSIONS {
                return Err(ResolveError::TooManyExpansions { module: self.module });
            }
            let at = self.calls[c].at;
            let depth = self.frags[at.frag as usize].expansion + 1;
            let frag = self.add_fragment(&expanded, Some(key), depth, Some(at))?;
            self.calls[c].state = CallState::Expanded { frag, def };
            self.unexpanded -= 1;
            moved = true;
        }
        Ok(moved)
    }

    /// Sets each pending item's flag to whether it is in `waiting`; whether a flag changed.
    pub(crate) fn set_flags(&mut self, waiting: &[Item], upto: [usize; 3]) -> bool {
        let mut changed = false;
        let mut set = |flag: &mut bool, item: Item, pending: bool| {
            let new = pending && waiting.binary_search(&item).is_ok();
            changed |= *flag != new;
            *flag = new;
        };
        for (i, imp) in self.imports.iter_mut().enumerate().take(upto[0]) {
            let pending = !imp.done();
            set(&mut imp.blocked, Item::Import(i as u32), pending);
        }
        for (i, g) in self.globs.iter_mut().enumerate().take(upto[1]) {
            let pending = matches!(g.source, Source::Pending);
            set(&mut g.blocked, Item::Glob(i as u32), pending);
        }
        for (i, c) in self.calls.iter_mut().enumerate().take(upto[2]) {
            let pending = c.state == CallState::Pending;
            set(&mut c.blocked, Item::Call(i as u32), pending);
        }
        changed
    }

    /// Every pending item blocked: after a move, nothing is known not to wait.
    pub(crate) fn block_all(&mut self) {
        for imp in self.imports.iter_mut() {
            imp.blocked = !imp.done();
        }
        for g in self.globs.iter_mut() {
            g.blocked = matches!(g.source, Source::Pending);
        }
        for c in self.calls.iter_mut() {
            c.blocked = c.state == CallState::Pending;
        }
    }

    /// The flags of every item, in order, for [`relax`] to compare.
    fn flags_into(&self, out: &mut AVec<'a, bool>) {
        out.extend(self.imports.iter().map(|i| i.blocked));
        out.extend(self.globs.iter().map(|g| g.blocked));
        out.extend(self.calls.iter().map(|c| c.blocked));
    }

    pub(crate) fn upto(&self) -> [usize; 3] {
        [self.imports.len(), self.globs.len(), self.calls.len()]
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
                    NsRes::Pending | NsRes::Absent | NsRes::Private(_) => {}
                }
            }
            if !any {
                let last = u32::try_from(imp.path.segments.len().saturating_sub(1)).unwrap_or(u32::MAX);
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

    /// The most passes that can each move something: every import namespace, glob and call
    /// moves forward only (rule 18).
    pub(crate) fn pass_bound(&self) -> u32 {
        let work = 4 * self.imports.len() + 2 * self.globs.len() + 3 * self.calls.len() + 4;
        u32::try_from(work).unwrap_or(u32::MAX)
    }

    /// One step of the protocol: takes `replies`, runs until it needs something, and says what.
    pub fn step<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        replies: &[Reply<'_, N>],
    ) -> Result<Step<'_, N, Resolved<'a, N>>, ResolveError> {
        if self.finished {
            return Err(ResolveError::Finished { module: self.module });
        }
        self.absorb(replies)?;
        let alloc = self.alloc;
        let mut out = core::mem::replace(&mut self.out, Outbox::new_in(alloc));
        let ran = run(core::slice::from_mut(self), facts, &mut out, alloc);
        self.out = out;
        match ran? {
            Ran::Ask => Ok(Step::Ask(&self.out.asks)),
            Ran::Expand => Ok(Step::Expand(&self.out.expand)),
            Ran::Cycle => Ok(Step::Cycle(&self.out.cycle)),
            Ran::Done(mut all) => {
                all.pop().map(Step::Done).ok_or(ResolveError::BadGroup { module: None })
            }
        }
    }

    /// Switches on a planted bug, for a control.
    #[cfg(test)]
    pub(crate) fn set_plant(&mut self, plant: Plant) {
        self.plant = plant;
    }
}

/// One pass over every member: all plans from the state as it is, then all applied.
fn pass<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    facts: &F,
    scan: Scan,
    alloc: Alloc<'a>,
) -> Result<(bool, Sink<'a, N>), ResolveError> {
    let mut sink = Sink::new_in(alloc, scan);
    let mut plans = Vec::with_capacity_in(ms.len(), alloc.clone());
    {
        let env = Env { members: &*ms };
        for m in ms.iter() {
            plans.push(m.plan(facts, env, &mut sink)?);
        }
    }
    sink.check(ms[0].module)?;
    let mut moved = false;
    for (m, plan) in ms.iter_mut().zip(plans) {
        moved |= m.apply(facts, plan)?;
    }
    Ok((moved, sink))
}

/// Lowers the blocked flags to their least fixpoint: from none, each round blocks what waits
/// on an answer or on a blocked item, until a round blocks nothing new. Whether any flag ends
/// lower than it was, and the last round's sink: the reads the settled flags need. Run only
/// when no answer is outstanding.
fn relax<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    facts: &F,
    alloc: Alloc<'a>,
) -> Result<(bool, Sink<'a, N>), ResolveError> {
    let mut before = Vec::new_in(alloc.clone());
    for m in ms.iter() {
        m.flags_into(&mut before);
    }
    let none: AVec<'a, Item> = Vec::new_in(alloc.clone());
    for m in ms.iter_mut() {
        let upto = m.upto();
        m.set_flags(&none, upto);
    }
    let rounds = before.len() + 2;
    // Every round's reads: an item blocks once a read it makes has no answer, and blocked it
    // no longer makes that read, so the reads come from the rounds before it blocked.
    let mut all = Sink::new_in(alloc, Scan::Early);
    let mut settled = false;
    for _ in 0..rounds {
        let mut sink = Sink::new_in(alloc, Scan::Early);
        let mut plans = Vec::with_capacity_in(ms.len(), alloc.clone());
        {
            let env = Env { members: &*ms };
            for m in ms.iter() {
                plans.push(m.plan(facts, env, &mut sink)?);
            }
        }
        sink.check(ms[0].module)?;
        all.need.extend_from_slice(&sink.need);
        all.waits.extend_from_slice(&sink.waits);
        let mut changed = false;
        for (m, plan) in ms.iter_mut().zip(plans.iter()) {
            changed |= m.set_flags(&plan.waiting, plan.upto);
        }
        if !changed {
            settled = true;
            break;
        }
    }
    if !settled {
        let passes = u32::try_from(rounds).unwrap_or(u32::MAX);
        return Err(ResolveError::NoProgressBound { module: ms[0].module, passes });
    }
    let sink = all;
    let mut after = Vec::new_in(alloc.clone());
    for m in ms.iter() {
        m.flags_into(&mut after);
    }
    Ok((after != before, sink))
}

/// Passes until one moves nothing and asks nothing new; returns that pass's sink.
fn settle<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    facts: &F,
    alloc: Alloc<'a>,
) -> Result<Sink<'a, N>, ResolveError> {
    let mut passes = 0u32;
    loop {
        let (moved, sink) = pass(ms, facts, Scan::Early, alloc)?;
        passes += 1;
        let bound = ms
            .iter()
            .map(|m| m.pass_bound())
            .fold(4u32, |a, b| a.saturating_add(b.saturating_mul(2)));
        if passes > bound {
            return Err(ResolveError::NoProgressBound { module: ms[0].module, passes });
        }
        if moved {
            for m in ms.iter_mut() {
                m.block_all();
            }
            continue;
        }
        if !sink.need.is_empty() || !sink.waits.is_empty() {
            return Ok(sink);
        }
        let (lowered, relaxed) = relax(ms, facts, alloc)?;
        if lowered {
            continue;
        }
        // The flags were already settled: what the settled plan needs is what to ask.
        return Ok(relaxed);
    }
}

/// The module whose record answers `read`.
fn owner<N: Copy>(ms: &[Resolver<'_, N>], read: &Read<N>) -> Option<ItemKey> {
    match *read {
        Read::TextualEntry { module } => {
            ms.iter().find(|m| m.module == module).and_then(|m| m.chain.get(1).copied())
        }
        Read::Name { .. } | Read::Exports { .. } | Read::TextualOut { .. } => Some(read.module()),
        // A macro's record, never a module in progress.
        Read::DeriveHelpers { .. } => None,
    }
}

/// Fills `out.cycle` with the modules of `waits` that are not members; whether there is one.
fn cycle_of<N: Name>(
    ms: &[Resolver<'_, N>],
    waits: &[Read<N>],
    out: &mut Outbox<'_, N>,
) -> bool {
    out.cycle.clear();
    for read in waits {
        if let Some(m) = owner(ms, read) {
            if ms.binary_search_by(|r| r.module.cmp(&m)).is_err() {
                out.cycle.push(m);
            }
        }
    }
    out.cycle.sort_unstable();
    out.cycle.dedup();
    !out.cycle.is_empty()
}

/// Runs a group of machines (one, for a machine alone) until it needs something or is done.
pub(crate) fn run<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    facts: &F,
    out: &mut Outbox<'a, N>,
    alloc: Alloc<'a>,
) -> Result<Ran<'a, N>, ResolveError> {
    let mut forced = 0u32;
    loop {
        let sink = settle(ms, facts, alloc)?;
        out.asks.clear();
        out.asks.extend_from_slice(&sink.need);
        out.asks.sort_unstable();
        out.asks.dedup();
        if !out.asks.is_empty() {
            return Ok(Ran::Ask);
        }
        out.expand.clear();
        for m in ms.iter() {
            for call in m.calls.iter() {
                if let CallState::Resolved(def) = call.state {
                    out.expand.push(ExpandRequest { call: call.key, def });
                }
            }
        }
        out.expand.sort_unstable();
        out.expand.dedup();
        if !out.expand.is_empty() {
            return Ok(Ran::Expand);
        }
        if cycle_of(ms, &sink.waits, out) {
            return Ok(Ran::Cycle);
        }
        // Stuck: force the macro paths, then the imports (rustc's order).
        let (mut moved, _) = pass(ms, facts, Scan::Force, alloc)?;
        if !moved {
            for m in ms.iter_mut() {
                moved |= m.fail_pending();
            }
        }
        if moved {
            for m in ms.iter_mut() {
                m.block_all();
            }
            forced += 1;
            let bound = ms.iter().map(|m| m.pass_bound()).fold(2u32, u32::saturating_add);
            if forced > bound {
                return Err(ResolveError::NoProgressBound { module: ms[0].module, passes: forced });
            }
            continue;
        }
        let mut fellows = Vec::with_capacity_in(ms.len(), alloc.clone());
        fellows.extend(ms.iter().map(|m| m.module));
        let mut done = Vec::with_capacity_in(ms.len(), alloc.clone());
        let mut asks = Vec::new_in(alloc.clone());
        let mut waits = Vec::new_in(alloc.clone());
        {
            let env = Env { members: &*ms };
            for m in ms.iter() {
                match m.finalize(facts, env, &fellows)? {
                    Finish::Need { asks: a, waits: w } => {
                        asks.extend_from_slice(&a);
                        waits.extend_from_slice(&w);
                    }
                    Finish::Done(resolved) => done.push(resolved),
                }
            }
        }
        if !asks.is_empty() {
            out.asks.clear();
            out.asks.extend_from_slice(&asks);
            out.asks.sort_unstable();
            out.asks.dedup();
            return Ok(Ran::Ask);
        }
        if cycle_of(ms, &waits, out) {
            return Ok(Ran::Cycle);
        }
        if done.len() != ms.len() {
            // A final pass waited on a member: a member's records are read live, never
            // waited on, so this is the machine's own bug, stopped on rather than hidden.
            let module = ms.iter().map(|m| m.module).find(|m| !done.iter().any(|d| d.scope.module == *m));
            return Err(ResolveError::MemberUnfinished { module: module.unwrap_or(ms[0].module) });
        }
        for m in ms.iter_mut() {
            m.finished = true;
        }
        return Ok(Ran::Done(done));
    }
}
