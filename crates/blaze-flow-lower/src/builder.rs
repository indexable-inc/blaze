//! The lowering's flow builder: blaze-flow's [`FlowBuilder`], every method the lowering uses
//! forwarded as an inherent one (never `Deref`: see the forwards' comment), and its `subset` recorded when a [`Log`] is asked for (the call-rule oracle,
//! oracle.rs). Production asks for none: a subset is then one `Option` test more, nothing kept.
//!
//! WHY A RECORD HERE. The finished [`blaze_flow::Flow`] tags each subset with its statement,
//! but a lowering that refuses finishes nothing, and the oracle must still compare the calls
//! before the refusal. And the two rules number fresh origins differently from the first call
//! on, so a call's constraints are compared by what their endpoints ARE (an argument's slot,
//! the destination's, the caller's region), which only the lowering knows when it makes the
//! call: [`Site`] keeps both, made by the seam (one_call.rs `call_by_rule`).

use core::alloc::Allocator;
use blaze_flow::{Block, BorrowKind, Flow, FlowBuilder, FlowError, Loan, Local, Operand, Origin, Place, PlaceMark, Proj, Span, Stmt, StmtKind};
use blaze_own::Vec;
use blaze_typeck::regions::Region;
use blaze_typeck::{ExprId, ItemKey};

use crate::Refusal;

/// What an origin of a call stands for, the same in both lowerings of one body. Public (as
/// call_census.rs `CallLabel`) so a census row can name the pair a (b) call lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Label {
    /// The `slot`-th origin of value argument `arg` (a method's receiver is argument 0).
    Arg { arg: u32, slot: u32 },
    /// The `slot`-th origin of the call's destination.
    Dest { slot: u32 },
    /// The caller's universal origin of its region `r` (`'static` among them).
    Uni(Region),
    /// `ρ_P`: the origin standing for the caller's type parameter `P`.
    Param(u32),
}

/// One call as the seam saw it: where its subsets are in [`Log::edges`], what its endpoints
/// are, and what the rule answered.
#[derive(Debug)]
pub(crate) struct Site<A: Allocator> {
    pub(crate) e: ExprId,
    pub(crate) callee: Option<ItemKey>,
    /// `edges[first..end]` are the call's.
    pub(crate) first: u32,
    pub(crate) end: u32,
    pub(crate) labels: Vec<(Origin, Label), A>,
    pub(crate) outcome: Result<(), Refusal>,
    /// The call went through the hub (`unify_call`, path 8). Set by [`Builder::log_site`].
    pub(crate) hub: bool,
    /// How many of the call's values (arguments and destination) were left out of `labels`
    /// because their type's positions are not its region slots (one_call.rs `site_labels`:
    /// an opaque type, an alias, a trait object with predicate slots, a fn item with argument
    /// slots, a body's type). The two grains number their origins differently there, so their
    /// pairs mean different regions; counted, never compared as if equal.
    pub(crate) parted: u32,
}

/// Every subset a lowering emitted, in order, and its calls.
#[derive(Debug)]
pub(crate) struct Log<A: Allocator> {
    pub(crate) edges: Vec<(Origin, Origin), A>,
    pub(crate) sites: Vec<Site<A>, A>,
    /// A hub mark not yet taken into a site.
    hub: bool,
}

/// [`FlowBuilder`] with its subsets recorded when asked.
pub(crate) struct Builder<A: Allocator + Copy> {
    inner: FlowBuilder<A>,
    log: Option<Log<A>>,
}

impl<A: Allocator + Copy> Builder<A> {
    pub(crate) fn new(inner: FlowBuilder<A>) -> Builder<A> {
        Builder { inner, log: None }
    }

    /// A new flow in place of this one (a closure's or an async block's own), its record kept.
    pub(crate) fn replace(&mut self, inner: FlowBuilder<A>) {
        self.inner = inner;
    }

    /// `'from: 'into` ([`FlowBuilder::subset`]), recorded when a log is kept.
    pub(crate) fn subset(&mut self, from: Origin, into: Origin) {
        if let Some(log) = self.log.as_mut() {
            log.edges.push((from, into));
        }
        self.inner.subset(from, into);
    }

    pub(crate) fn finish(self) -> Result<Flow<A>, FlowError> {
        self.inner.finish()
    }

    /// Keep a record from now on, in `alloc`.
    pub(crate) fn start_log(&mut self, alloc: A) {
        self.log = Some(Log { edges: Vec::new_in(alloc.clone()), sites: Vec::new_in(alloc), hub: false });
    }

    pub(crate) fn take_log(&mut self) -> Option<Log<A>> {
        self.log.take()
    }

    /// How many subsets are recorded so far; `None` when no record is kept.
    pub(crate) fn logged(&self) -> Option<u32> {
        self.log.as_ref().map(|l| u32::try_from(l.edges.len()).unwrap_or(u32::MAX))
    }

    /// A call's record, when a log is kept; whether the call went through the hub
    /// (`unify_call`) since the last record is taken into it.
    pub(crate) fn log_site(&mut self, mut site: Site<A>) {
        if let Some(log) = self.log.as_mut() {
            site.hub = core::mem::replace(&mut log.hub, false);
            log.sites.push(site);
        }
    }

    /// The call being lowered went through `unify_call`'s one hub origin (path 8): the oracle
    /// counts its rule-(b) difference as the hub's over-approximation, never silently.
    pub(crate) fn mark_hub(&mut self) {
        if let Some(log) = self.log.as_mut() {
            log.hub = true;
        }
    }

    /// Forget a hub mark made outside a call the seam logs (await_call.rs, a value callee):
    /// the seam calls it as each call starts.
    pub(crate) fn clear_hub(&mut self) {
        if let Some(log) = self.log.as_mut() {
            log.hub = false;
        }
    }
}

/// The rest of [`FlowBuilder`], forwarded as INHERENT methods, never through `Deref`/`DerefMut`:
/// a method call on the plain field `self.b` is a two-phase borrow, so `self.b.stmt(k,
/// self.span(e))` reads `self` in its arguments as it did when the field was the flow builder;
/// an overloaded `deref_mut` takes `&mut self.b` before the arguments run and refuses it (E0502).
impl<A: Allocator + Copy> Builder<A> {
    pub(crate) fn add_local(&mut self) -> Local {
        self.inner.add_local()
    }
    pub(crate) fn local(&self, l: u32) -> Place {
        self.inner.local(l)
    }
    pub(crate) fn proj_of(&self, p: Place) -> Option<Proj> {
        self.inner.proj_of(p)
    }
    pub(crate) fn project_in(&mut self, parent: Place, proj: Proj, mark: PlaceMark) -> Place {
        self.inner.project_in(parent, proj, mark)
    }
    pub(crate) fn origin(&mut self, placeholder: bool) -> Origin {
        self.inner.origin(placeholder)
    }
    pub(crate) fn mentions(&mut self, local: u32, origin: Origin) {
        self.inner.mentions(local, origin)
    }
    pub(crate) fn mentions_no_drop(&mut self, local: u32, origin: Origin) {
        self.inner.mentions_no_drop(local, origin)
    }
    pub(crate) fn known(&mut self, from: Origin, into: Origin) {
        self.inner.known(from, into)
    }
    pub(crate) fn block(&mut self) -> Block {
        self.inner.block()
    }
    pub(crate) fn start(&mut self, b: Block) {
        self.inner.start(b)
    }
    pub(crate) fn assign(&mut self, dest: Place, op: Operand, span: Span) -> Stmt {
        self.inner.assign(dest, op, span)
    }
    pub(crate) fn aggregate(&mut self, dest: Place, ops: &[Operand], span: Span) -> Stmt {
        self.inner.aggregate(dest, ops, span)
    }
    pub(crate) fn borrow(&mut self, dest: Place, kind: BorrowKind, place: Place, origin: Origin, span: Span) -> Loan {
        self.inner.borrow(dest, kind, place, origin, span)
    }
    pub(crate) fn call(&mut self, args: &[Operand], dest: Place, span: Span) -> Stmt {
        self.inner.call(args, dest, span)
    }
    pub(crate) fn mention(&mut self, place: Place, span: Span) -> Stmt {
        self.inner.mention(place, span)
    }
    pub(crate) fn stmt(&mut self, kind: StmtKind, span: Span) -> Stmt {
        self.inner.stmt(kind, span)
    }
    pub(crate) fn goto(&mut self, b: Block, span: Span) -> Stmt {
        self.inner.goto(b, span)
    }
    pub(crate) fn switch(&mut self, on: Place, targets: &[Block], span: Span) -> Stmt {
        self.inner.switch(on, targets, span)
    }
    pub(crate) fn ret(&mut self, span: Span) -> Stmt {
        self.inner.ret(span)
    }
    pub(crate) fn suspend(&mut self, resume: Block, cancel: Block) {
        self.inner.suspend(resume, cancel)
    }
}
