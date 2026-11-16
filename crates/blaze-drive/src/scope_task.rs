//! The `ModuleScope` task (drive.md, "Tasks and Need"): blaze-scope's machine for one module,
//! driven until it is done or asks for something the task was not handed. Its reads of other
//! modules are answered from the scope records it holds; one it lacks is a `Need` of that
//! module's `ModuleScope`, a macro call it must see expanded is a `Need` of the call's
//! `Expansion`, and the task is entered again from the top with them (bottom.md 9: Need by
//! unwinding). A cycle is the request machine's: the members wait on each other, the machine
//! finds them quiet and runs them as one `Group`.

use core::alloc::Allocator;
use core::fmt;

use blaze_id::ItemKey;
use blaze_own::{Alloc, Vec};
use blaze_scope::{Answer, Read, Reply, ResolveError, Resolved, Resolver, ScopeFacts, Step};
use blaze_surface::SurfaceError;

use crate::key::{Kind, Name};

/// The most steps one entry takes: each step moves the machine forward or asks, and every ask
/// is answered at once from what the task holds, so a module's steps are bounded by its reads
/// (rule 18). Past it, [`ScopeTaskError::Steps`].
pub const MOST_STEPS: u32 = 1 << 16;

/// Answers a read of another module's scope from the records the task was handed.
pub trait ScopeReads<N> {
    /// The answer, or `None` when the task does not hold that module's scope record: the task
    /// then needs it.
    fn answer(&self, read: &Read<N>) -> Result<Option<Answer<'_, N>>, SurfaceError>;
}

/// How an entry of the task ended.
pub enum ScopeOutcome<'a, N> {
    Done(Resolved<'a, N>),
    /// Names to be handed on the next entry, sorted and without repeats.
    Need(Vec<Name, Alloc<'a>>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeTaskError {
    /// The machine refused: the module's records are not what it reads, or a bound was hit.
    Resolve { module: ItemKey, error: ResolveError },
    /// A record the module's own facts are read from is missing: the task's inputs miss a fact
    /// its home should have held (the request machine hands the home; this is a bug there).
    Fact { module: ItemKey, error: SurfaceError },
    /// The machine reported a cycle while every read was answered by a record, not in
    /// progress: a record that says "in progress" was stored, which no task writes.
    Cycle { module: ItemKey },
    Steps { module: ItemKey },
}

impl fmt::Display for ScopeTaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeTaskError::Resolve { module, error } => write!(f, "the scope of module {module:x}: {error}"),
            ScopeTaskError::Fact { module, error } => {
                write!(f, "the scope of module {module:x}: its own facts: {error} (drive.md: a task is handed its home)")
            }
            ScopeTaskError::Cycle { module } => write!(f, "the scope of module {module:x}: a cycle among stored records"),
            ScopeTaskError::Steps { module } => write!(f, "the scope of module {module:x}: more than {MOST_STEPS} steps (rule 18)"),
        }
    }
}

impl core::error::Error for ScopeTaskError {}

/// One entry of the `ModuleScope` task of `module`.
pub fn module_scope<'a, N: blaze_scope::Name, F: ScopeFacts<Name = N>, R: ScopeReads<N>>(
    module: ItemKey,
    facts: &F,
    reads: &R,
    alloc: Alloc<'a>,
) -> Result<ScopeOutcome<'a, N>, ScopeTaskError> {
    let refused = |error: ResolveError| match error {
        ResolveError::Surface { module, error } => ScopeTaskError::Fact { module, error },
        error => ScopeTaskError::Resolve { module, error },
    };
    let mut machine = Resolver::new_in(module, facts, alloc).map_err(refused)?;
    let mut needs: Vec<Name, Alloc<'a>> = Vec::new_in(alloc);
    let mut replies: Vec<Reply<'_, N>, Alloc<'a>> = Vec::new_in(alloc);
    for _ in 0..MOST_STEPS {
        let step = machine.step(facts, &replies).map_err(refused)?;
        replies.clear();
        match step {
            Step::Done(resolved) => {
                return Ok(match needs.is_empty() {
                    true => ScopeOutcome::Done(resolved),
                    false => ScopeOutcome::Need(needs),
                });
            }
            Step::Ask(asks) => {
                let mut lacking = false;
                for read in asks {
                    match reads.answer(read).map_err(|error| ScopeTaskError::Fact { module, error })? {
                        Some(answer) => replies.push(Reply { read: *read, answer }),
                        None => {
                            lacking = true;
                            add(&mut needs, Name::new(Kind::ModuleScope, read.module().0));
                        }
                    }
                }
                if lacking {
                    // Every lack of this entry is gathered at once: the next entry is handed
                    // them all, so a chain of needs costs one entry per depth, not per read.
                    return Ok(ScopeOutcome::Need(needs));
                }
            }
            Step::Expand(calls) => {
                for call in calls {
                    add(&mut needs, Name::new(Kind::Expansion, call.call.0));
                }
                return Ok(ScopeOutcome::Need(needs));
            }
            Step::Cycle(_) => return Err(ScopeTaskError::Cycle { module }),
        }
    }
    Err(ScopeTaskError::Steps { module })
}

/// One crate's facts for a round of its first fill (drive.md, "Scheduling on many cores" 5;
/// parallel.md change 9): the `CrateRoot` record decoded once, its lists sorted as the resolver
/// sorts them, and handed to every module of the round, so a fill of m modules decodes the root
/// once, not m times per entry. Every other fact goes to the inner facts.
///
/// The resolver still copies the lists it keeps (blaze-scope's `Resolver::new_in`; a borrow
/// there is that crate's change): what is shared here is the decode and the sort, and the
/// resolver's own sort of an already sorted list is one pass.
pub struct SharedRoot<'f, F: ScopeFacts> {
    facts: &'f F,
    krate: blaze_id::CrateItemKey,
    root: blaze_surface::CrateRootBuilder<'f, F::Name>,
}

impl<'f, F: ScopeFacts> SharedRoot<'f, F> {
    pub fn new(facts: &'f F, krate: blaze_id::CrateItemKey) -> Result<SharedRoot<'f, F>, SurfaceError> {
        let mut root = facts.crate_root(krate)?;
        root.extern_prelude.sort_unstable();
        root.builtin_attrs.sort_unstable();
        Ok(SharedRoot { facts, krate, root })
    }
}

impl<F: ScopeFacts> ScopeFacts for SharedRoot<'_, F> {
    type Name = F::Name;

    fn module(&self, module: ItemKey) -> Result<blaze_scope::ModuleInfo<F::Name>, SurfaceError> {
        self.facts.module(module)
    }

    fn decls(&self, module: ItemKey) -> Result<blaze_scope::DeclsBuilder<'_, F::Name>, SurfaceError> {
        self.facts.decls(module)
    }

    fn crate_root(
        &self,
        krate: blaze_id::CrateItemKey,
    ) -> Result<blaze_surface::CrateRootBuilder<'_, F::Name>, SurfaceError> {
        match krate == self.krate {
            true => Ok(self.root.clone()),
            false => self.facts.crate_root(krate),
        }
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<blaze_scope::ExpandedDecls<'_, F::Name>>, SurfaceError> {
        self.facts.expansion(call)
    }
}

/// One round of a crate's first fill: an entry of every module in `modules` over one decoded
/// root, each outcome pushed in order. A module's error is its own (rule 20): it is pushed as
/// that module's outcome, and the round goes on.
pub fn fill_round<'a, N, F, R, B>(
    facts: &F,
    krate: blaze_id::CrateItemKey,
    modules: &[ItemKey],
    reads: &R,
    alloc: Alloc<'a>,
    out: &mut Vec<Result<ScopeOutcome<'a, N>, ScopeTaskError>, B>,
) -> Result<(), SurfaceError>
where
    N: blaze_scope::Name,
    F: ScopeFacts<Name = N>,
    R: ScopeReads<N>,
    B: Allocator,
{
    let shared = SharedRoot::new(facts, krate)?;
    for module in modules {
        out.push(module_scope(*module, &shared, reads, alloc));
    }
    Ok(())
}

fn add<A: Allocator>(needs: &mut Vec<Name, A>, name: Name) {
    if let Err(at) = needs.binary_search(&name) {
        needs.insert(at, name);
    }
}
