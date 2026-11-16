//! The adapter IN (coroutines.md, "From a checked body and back"): a [`Recorder`] turns the SCOPE
//! EVENTS of a body's one walk into the form of [`crate::ir`], and desugars every `.await`, once,
//! here.
//!
//! Who calls it. The code builder's walk of the type-checked body (blaze-build walk.rs) is the
//! one place that decides when a binding is declared, moved, dropped and when its scope ends
//! (`own`, `moved`, `drop_local`, `drop_from`, the block's close). It calls a recorder at those
//! places, so the form holds the walk's own events and not a second copy of the drop rules: a
//! coroutine cannot drop in another order than the same code built as a plain function, because
//! there is one list. At an `.await` the walk hands over its open drop scopes as they stand
//! (`scope_pats`, in declaration order): that SNAPSHOT is the cancel path of the point, the same
//! entries `drop_from` would replay for a `return` there.
//!
//! The facts of a local. The transform needs one: whether dropping the local runs code
//! ([`LocalFacts::needs_drop`]). It is the builder's `needs_drop` of the local's ground type, the
//! answer `own` uses to put the binding in a drop scope, so the two cannot differ. `Copy` is not
//! a fact of a local here: a use is recorded as a move or as a copy by the event
//! ([`Recorder::moved`], [`Recorder::used`]), which the builder decides where it decides it for
//! the code.
//!
//! How long a local is held (decision D4, coarse, stated): a declared local is counted as
//! borrowed from its declaration to the end of its scope, so the state saves every local in
//! scope at some suspension. That is a superset of the exact set and needs no edge of the
//! control flow: whether a scope has ended at a suspension is the same on every path that
//! respects the nesting, so the events may arrive in the walk's order as one line of code. The
//! exact set arrives with [`Recorder::used`] and [`Recorder::borrowed`] called at every read and
//! borrow and the walk's blocks mirrored; it can only save fewer locals.
//!
//! `.await` (the Reference, "Await expressions"), the numbered steps this module writes:
//! "1. Create a future by calling IntoFuture::into_future on the future operand. 2. Evaluate the
//! future to a future tmp; 3. Pin tmp using Pin::new_unchecked; 4. This pinned future is then
//! polled by calling the Future::poll method and passing it the current task context; 5. If the
//! call to poll returns Poll::Pending, then the future returns Poll::Pending, suspending its
//! state so that, when the surrounding async context is re-polled, execution returns to step 3;
//! 6. Otherwise the call to poll must have returned Poll::Ready, in which case the value
//! contained in the Poll::Ready variant is used as the result of the await expression itself."
//! As statements ([`Recorder::await_point`]): `fut = call into_future()` (steps 1 and 2); a head
//! block, which is the resume block, with `pin = &fut` (step 3) and `answer = poll fut` (step
//! 4); a switch on the answer to a block ending in `Suspend` back to the head (step 5) or to
//! the ready block, where the future tmp is dropped and its storage ends (step 6: its
//! approximate desugaring binds the tmp in a `match` arm, whose scope the await's value
//! leaves). The cancel path of the point drops the future tmp first (the innermost binding),
//! then the snapshot, last declared first.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::ir::{Body, BodyBuilder, IrError, LocalId, Operand, PointId, Span32, Stmt, Term};

/// The callee number of `IntoFuture::into_future` in a recorded body's `Call` statements.
pub const INTO_FUTURE: u32 = 0;

/// What a local of a recorded body is. The numbers are the walk's own ids (a pattern's, an
/// expression's): facts local to one body, never part of a key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Capture `n` of the block in the checker's order (`Checked::captures`), or parameter `n`
    /// of an `async fn`.
    Upvar(u32),
    /// The binding of this pattern.
    Binding(u32),
    /// The future tmp of the `.await` at this expression.
    Future(u32),
    /// The temporary of this expression.
    Temp(u32),
    /// One of the recorder's own locals: the target of a borrow or of a moved value, a poll's
    /// answer. Never saved.
    Scratch,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Origin::Upvar(n) => write!(f, "upvar {n}"),
            Origin::Binding(n) => write!(f, "binding {n}"),
            Origin::Future(n) => write!(f, "future {n}"),
            Origin::Temp(n) => write!(f, "temp {n}"),
            Origin::Scratch => write!(f, "scratch"),
        }
    }
}

/// The facts of a local's type the transform reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LocalFacts {
    /// Dropping a value of the type runs code.
    pub needs_drop: bool,
}

/// A sequence of events no walk makes, named (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// The body built from the events is malformed.
    Ir(IrError),
    /// An upvar was declared after the first other event.
    UpvarAfterStart(Origin),
    /// A scope was closed that was never opened.
    ScopeNotOpen,
    /// The recording ended with this many scopes open.
    ScopesLeftOpen(u32),
    /// A local of a cancel snapshot is not in scope: the walk holds a binding in a drop scope
    /// that it never declared here, so the state would not save what the cancel path drops.
    SnapshotNotInScope(LocalId),
    /// An event names a local the recorder never made.
    NoSuchLocal(LocalId),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecordError::Ir(e) => write!(f, "the recorded body is malformed: {e}"),
            RecordError::UpvarAfterStart(o) => write!(f, "{o} was declared after the body's first event"),
            RecordError::ScopeNotOpen => write!(f, "a scope was closed that was never opened"),
            RecordError::ScopesLeftOpen(n) => write!(f, "the recording ended with {n} scopes open"),
            RecordError::SnapshotNotInScope(l) => {
                write!(f, "{l} is in a cancel snapshot and not in scope: the state would not save it")
            }
            RecordError::NoSuchLocal(l) => write!(f, "{l} is not a local of the recording"),
        }
    }
}

/// What an `.await` was recorded as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Awaited {
    pub point: PointId,
    /// The future tmp.
    pub future: LocalId,
}

/// A coroutine body from its walk's events, with what each part came from.
pub struct Lowered<A: Allocator> {
    pub body: Body<A>,
    /// What each local is.
    pub local_of: Vec<Origin, A>,
    /// The `Expr::Await` of each point.
    pub point_of: Vec<u32, A>,
    /// The future tmp of each point.
    pub future_of: Vec<LocalId, A>,
    /// Per point: its cancel order, a range of `cancels`.
    pub cancel_of: Vec<Span32, A>,
    /// The locals a cancel at a point drops, first dropped first: the future tmp when its type
    /// needs drop, then the walk's snapshot, last declared first.
    pub cancels: Vec<LocalId, A>,
}

impl<A: Allocator> Lowered<A> {
    /// The locals a cancel at `point` drops, in order.
    pub fn cancel(&self, point: PointId) -> &[LocalId] {
        match self.cancel_of.get(point.index()) {
            Some(s) => &self.cancels[s.range()],
            None => &[],
        }
    }
}

/// Records one body. A mistake is kept (the first one) and returned by [`Recorder::finish`].
pub struct Recorder<A: Allocator + Clone> {
    b: BodyBuilder<A>,
    local_of: Vec<Origin, A>,
    point_of: Vec<u32, A>,
    future_of: Vec<LocalId, A>,
    cancel_of: Vec<Span32, A>,
    cancels: Vec<LocalId, A>,
    /// The locals in scope, in declaration order.
    open: Vec<LocalId, A>,
    /// Per open scope: where its locals start in `open`.
    marks: Vec<u32, A>,
    /// Whether each local is in `open`.
    in_scope: Vec<bool, A>,
    /// The scratch locals, made at the first event that is not an upvar.
    scratch: Option<Scratch>,
    error: Option<RecordError>,
}

#[derive(Clone, Copy)]
struct Scratch {
    /// The target of the borrow that holds a declared local to its scope's end.
    keep: LocalId,
    /// The target of a moved or copied value.
    sink: LocalId,
    /// A poll's answer.
    answer: LocalId,
    /// The pinned reference to a future tmp.
    pin: LocalId,
}

impl<A: Allocator + Clone> Recorder<A> {
    pub fn new_in(alloc: A) -> Recorder<A> {
        Recorder {
            b: BodyBuilder::new_in(alloc.clone()),
            local_of: Vec::new_in(alloc.clone()),
            point_of: Vec::new_in(alloc.clone()),
            future_of: Vec::new_in(alloc.clone()),
            cancel_of: Vec::new_in(alloc.clone()),
            cancels: Vec::new_in(alloc.clone()),
            open: Vec::new_in(alloc.clone()),
            marks: Vec::new_in(alloc.clone()),
            in_scope: Vec::new_in(alloc),
            scratch: None,
            error: None,
        }
    }

    fn fail(&mut self, e: RecordError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    fn new_local(&mut self, origin: Origin, needs_drop: bool, upvar: bool) -> LocalId {
        let l = if upvar { self.b.upvar(needs_drop) } else { self.b.local(needs_drop) };
        self.local_of.push(origin);
        self.in_scope.push(false);
        l
    }

    /// The scratch locals and the entry block, at the first event that is not an upvar.
    fn started(&mut self) -> Scratch {
        if let Some(s) = self.scratch {
            return s;
        }
        let s = Scratch {
            keep: self.new_local(Origin::Scratch, false, false),
            sink: self.new_local(Origin::Scratch, false, false),
            answer: self.new_local(Origin::Scratch, false, false),
            pin: self.new_local(Origin::Scratch, false, false),
        };
        let entry = self.b.block();
        self.b.begin(entry);
        self.scratch = Some(s);
        s
    }

    fn known(&mut self, l: LocalId) -> bool {
        let ok = l.index() < self.local_of.len();
        if !ok {
            self.fail(RecordError::NoSuchLocal(l));
        }
        ok
    }

    fn enter(&mut self, l: LocalId) {
        if !self.in_scope[l.index()] {
            self.in_scope[l.index()] = true;
            self.open.push(l);
        }
    }

    /// An upvar: initialised when the coroutine is made, in scope for the whole body (it joins
    /// the outermost scope). Every upvar comes before any other event.
    pub fn upvar(&mut self, origin: Origin, facts: LocalFacts) -> LocalId {
        if self.scratch.is_some() {
            self.fail(RecordError::UpvarAfterStart(origin));
        }
        let l = self.new_local(origin, facts.needs_drop, true);
        self.enter(l);
        l
    }

    /// A drop scope opens (a block; the parameters' scope is the body's first).
    pub fn scope_open(&mut self) {
        self.started();
        self.marks.push(self.open.len() as u32);
    }

    /// The innermost scope closes: the storage of each of its locals ends, last declared first.
    /// The drops themselves were recorded before, by [`Recorder::dropped`], from the walk's own
    /// scope end.
    pub fn scope_close(&mut self) {
        self.started();
        let Some(mark) = self.marks.pop() else {
            self.fail(RecordError::ScopeNotOpen);
            return;
        };
        while self.open.len() > mark as usize {
            let Some(l) = self.open.pop() else { break };
            self.in_scope[l.index()] = false;
            self.b.push(Stmt::StorageDead(l));
        }
    }

    /// A new local declared in the innermost scope: a binding at its `let` or pattern, a
    /// temporary where it is made. It is held from here to its scope's end (module doc).
    pub fn declare(&mut self, origin: Origin, facts: LocalFacts) -> LocalId {
        let s = self.started();
        let l = self.new_local(origin, facts.needs_drop, false);
        self.enter(l);
        // A declaration is a definition: no value of the local from before it is read, so the
        // borrow below does not make it live by use backward past its declaration.
        self.b.push(Stmt::Use { dest: l, src: Operand::Const(0) });
        self.b.push(Stmt::Borrow { dest: s.keep, of: l });
        l
    }

    /// Local `l` declared again: a `let` of a loop's body run another time. The same local, in
    /// the innermost scope again.
    pub fn redeclare(&mut self, l: LocalId) {
        let s = self.started();
        if !self.known(l) {
            return;
        }
        self.enter(l);
        // A declaration is a definition: no value of the local from before it is read, so the
        // borrow below does not make it live by use backward past its declaration.
        self.b.push(Stmt::Use { dest: l, src: Operand::Const(0) });
        self.b.push(Stmt::Borrow { dest: s.keep, of: l });
    }

    /// `l` is read by value and stays initialised (its type is `Copy`).
    pub fn used(&mut self, l: LocalId) {
        let s = self.started();
        if self.known(l) {
            self.b.push(Stmt::Use { dest: s.sink, src: Operand::Copy(l) });
        }
    }

    /// `l` is moved out of.
    pub fn moved(&mut self, l: LocalId) {
        let s = self.started();
        if self.known(l) {
            self.b.push(Stmt::Use { dest: s.sink, src: Operand::Move(l) });
        }
    }

    /// `into` is given a reference to `of` (a `let r = &x`, a by-reference binding of a part).
    pub fn borrowed(&mut self, of: LocalId, into: LocalId) {
        self.started();
        if self.known(of) && self.known(into) {
            self.b.push(Stmt::Borrow { dest: into, of });
        }
    }

    /// `l` is dropped here by the walk: at its scope's end, or on an exit that leaves its scope.
    pub fn dropped(&mut self, l: LocalId) {
        self.started();
        if self.known(l) {
            self.b.push(Stmt::Drop(l));
        }
    }

    /// The `.await` at expression `expr`, its operand evaluated (module doc: the six steps).
    /// `snapshot` is the walk's open drop scopes as they stand, in declaration order: every
    /// entry it would drop for a `return` here. Leaves the ready block open: the events after
    /// the await follow in it.
    pub fn await_point(&mut self, expr: u32, future: LocalFacts, snapshot: &[LocalId]) -> Awaited {
        let s = self.started();
        let fut = self.new_local(Origin::Future(expr), future.needs_drop, false);
        let point = self.b.point();
        // Steps 1 and 2: the future tmp.
        self.b.call(fut, INTO_FUTURE, &[]);
        let (head, pending, cancel, ready) = (self.b.block(), self.b.block(), self.b.block(), self.b.block());
        self.b.end(Term::Goto(head));
        // Steps 3 and 4: pinned in place, polled. The head is where a later poll goes on.
        self.b.begin(head);
        self.b.push(Stmt::Borrow { dest: s.pin, of: fut });
        self.b.push(Stmt::Poll { dest: s.answer, fut });
        self.b.switch(s.answer, &[pending, ready]);
        // Step 5.
        self.b.begin(pending);
        self.b.end(Term::Suspend { point, resume: head, drop: cancel });
        // Dropped while parked here: the future tmp, then the snapshot, last declared first.
        self.b.begin(cancel);
        let first = self.cancels.len() as u32;
        if future.needs_drop {
            self.b.push(Stmt::Drop(fut));
            self.cancels.push(fut);
        }
        for &l in snapshot.iter().rev() {
            if l.index() >= self.in_scope.len() {
                self.fail(RecordError::NoSuchLocal(l));
                continue;
            }
            if !self.in_scope[l.index()] {
                self.fail(RecordError::SnapshotNotInScope(l));
            }
            self.b.push(Stmt::Drop(l));
            self.cancels.push(l);
        }
        self.b.end(Term::Cancelled);
        self.cancel_of.push(Span32 { first, len: self.cancels.len() as u32 - first });
        self.point_of.push(expr);
        self.future_of.push(fut);
        // Step 6: the value is the await's; the future tmp's scope ends.
        self.b.begin(ready);
        if future.needs_drop {
            self.b.push(Stmt::Drop(fut));
        }
        self.b.push(Stmt::StorageDead(fut));
        Awaited { point, future: fut }
    }

    /// The body's end: every scope closed, the result returned.
    pub fn finish(mut self) -> Result<Lowered<A>, RecordError> {
        self.started();
        if !self.marks.is_empty() {
            let n = self.marks.len() as u32;
            self.fail(RecordError::ScopesLeftOpen(n));
        }
        if let Some(e) = self.error {
            return Err(e);
        }
        self.b.end(Term::Return(Operand::Const(0)));
        let body = self.b.finish().map_err(RecordError::Ir)?;
        Ok(Lowered {
            body,
            local_of: self.local_of,
            point_of: self.point_of,
            future_of: self.future_of,
            cancel_of: self.cancel_of,
            cancels: self.cancels,
        })
    }
}
