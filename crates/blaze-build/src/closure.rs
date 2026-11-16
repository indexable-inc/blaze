//! Closures (docs/v2/plan/design/codegen.md, "Closures and the Fn traits"): a child module of
//! walk.rs, so it reads the walk's own state.
//!
//! The Reference, "Closure types": "A closure expression produces a closure value with a unique,
//! anonymous type that cannot be written out. A closure type is approximately equivalent to a
//! struct which contains the captured values." Here that struct is the ENVIRONMENT, a tuple with
//! one field per capture in the checker's order (`Checked::captures`, sorted by binding): `&T`
//! for `ByRef`, `&mut T` for `ByMutRef`, `T` for `ByValue`. "Capture precision" says a closure
//! captures the place it uses (a single field, `x.a`); the checker records whole bindings only
//! (rules/closure.rs `walk_captures`), so the environment holds whole bindings: a gap named, not
//! a guess, since a whole binding is a superset of the place and no program reads the difference
//! but a borrow check's.
//!
//! The ground form of a closure type carries what its layout and its identity need, so any body
//! reads it from the type alone: `Closure(ordinal, (sig, env, FnDef(owner, owner_args), kind))`,
//! named by (owner, ORDINAL, owner's arguments) as the checker names it (typeck.md, "A closure's
//! identity"); the expression id stays inside the one body's walk.
//!
//! The body is the instance of the kind's `Fn*` method at `[C, (A..)]`; core's
//! library/core/src/ops/function.rs: `extern "rust-call" fn call(&self, args: Args) ->
//! Self::Output;`, `extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;`,
//! `extern "rust-call" fn call_once(self, args: Args) -> Self::Output;`. Its tuple's fields
//! cross as separate parameters (lib.rs `Abi::RustCall`), so the body takes `(env, a..)`.

use blaze_typeck::CaptureKind;

use super::*;
use crate::FnTrait;
use crate::layout::LayoutFacts;

/// The closures of `body` with their ORDINALS: a closure's position among its owner body's
/// closures in expression order (typeck.md, "A closure's identity"). The one place the builder
/// derives an ordinal, as the checker will record it; the expression id is a fact local to one
/// body's walk and never leaves it (no key, no ground type holds it).
pub(crate) fn closures_in<'b>(body: &'b BodyView<'_>) -> impl Iterator<Item = (u32, ExprId)> + 'b {
    // An `ExprId` is a `u32`, so every index of `exprs` fits one.
    let ids = body.exprs.iter().enumerate().filter(|(_, x)| matches!(x, Expr::Closure { .. })).map(|(i, _)| ExprId(i as u32));
    (0u32..).zip(ids)
}

/// A ground closure's parts: its identity from its type, its facts from the seam, grounded at
/// the owner's arguments.
#[derive(Clone, Copy)]
pub(crate) struct Parts {
    /// Its position among its owner body's closures ([`closures_in`]).
    pub(crate) ordinal: u32,
    /// Its signature, a `FnPtr`: the inputs, then the output.
    pub(crate) sig: PTy,
    /// Its environment, a tuple.
    pub(crate) env: PTy,
    pub(crate) owner: ItemKey,
    pub(crate) owner_args: List,
    pub(crate) kind: FnTrait,
}


/// The method of trait `which`, for a refusal's name.
fn method_name(which: FnTrait) -> &'static str {
    match which {
        FnTrait::Fn => "call",
        FnTrait::FnMut => "call_mut",
        FnTrait::FnOnce => "call_once",
    }
}

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// Ground closure type `g` read: its identity (owner, ordinal, the owner's arguments) from
    /// the type, its kind, signature and environment from the seam ([`LayoutFacts::closure`],
    /// the owner's check record) grounded at those arguments, as an ADT's fields are.
    pub(super) fn closure_parts(&mut self, g: PTy) -> Result<Parts, TyErr> {
        let PNode::Closure { owner, ordinal, args } = self.ground_node(g)? else {
            return Err(TyErr::Malformed("a closure's parts asked of another type"));
        };
        let facts = self.adts.facts();
        let c = facts.closure(owner, ordinal).ok_or(TyErr::Unsupported("a closure whose facts the world did not answer"))?;
        let mut a: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        a.extend_from_slice(self.ground.view().list(args).map_err(|_| TyErr::Malformed("a list past the ground table"))?);
        let sig = crate::instance::ground(c.types, c.sig, &a, self.ground, self.scratch, facts).map_err(TyErr::Other)?;
        let env = crate::instance::ground(c.types, c.env, &a, self.ground, self.scratch, facts).map_err(TyErr::Other)?;
        Ok(Parts { ordinal, sig, env, owner, owner_args: args, kind: c.kind })
    }

    /// The parts of ground closure type `g`, met at `e`.
    fn parts_at(&mut self, e: ExprId, g: PTy) -> Result<Parts, BuildError> {
        self.closure_parts(g).map_err(|error| Self::ty_err(e, error))
    }

    /// Closure `e`'s captures in the checker's order: the binding and how.
    fn captures_of(&self, e: ExprId, out: &mut Vec<(PatId, CaptureKind), A>) {
        out.clear();
        let caps = &self.checked.captures;
        for row in 0..caps.len() {
            if caps.column::<0>()[row] == e {
                out.push((caps.column::<1>()[row], caps.column::<2>()[row]));
            }
        }
    }

    /// The offset of field `j` of the struct or tuple laid out as row `l`.
    fn field_offset(&self, l: u32, j: usize) -> Result<u32, BuildError> {
        let Shape::Fields { first, len } = self.layouts[l as usize].shape else {
            return Err(malformed("a closure's environment not laid out as a struct"));
        };
        let rows = self.layout_fields.get(first as usize..(first + len) as usize).ok_or(malformed("a layout's fields past their table"))?;
        rows.iter().find(|f| f.0 == Name(j as u64)).map(|f| f.1).ok_or(malformed("a capture past its environment's fields"))
    }

    /// Where each capture's field is inside a value of closure type `g` (its one field, the
    /// environment, then the capture's field in it), in capture order.
    fn capture_offsets(&mut self, e: ExprId, g: PTy, n: usize, out: &mut Vec<u32, A>) -> Result<(), BuildError> {
        out.clear();
        if n == 0 {
            return Ok(());
        }
        let parts = self.parts_at(e, g)?;
        let lc = self.layout(g).map_err(|error| Self::ty_err(e, error))?;
        let le = self.layout(parts.env).map_err(|error| Self::ty_err(e, error))?;
        let base = self.field_offset(lc, 0)?;
        for j in 0..n {
            out.push(base + self.field_offset(le, j)?);
        }
        Ok(())
    }

    /// `|..| body`: its environment, each by-reference capture the address of the binding's
    /// home, each by-value capture its value moved in (the Reference, "Closure types",
    /// "Capture modes").
    pub(super) fn closure_value(&mut self, e: ExprId) -> Result<(), BuildError> {
        let t = self.expr_ty(e)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
        let repr = self.repr(g).map_err(|error| Self::ty_err(e, error))?;
        let mut caps = Vec::new_in(self.alloc.clone());
        self.captures_of(e, &mut caps);
        if let Repr::None = repr {
            self.vals.push(V::None);
            return Ok(());
        }
        let mut offs = Vec::new_in(self.alloc.clone());
        self.capture_offsets(e, g, caps.len(), &mut offs)?;
        let at = self.temp(repr);
        for (j, &(p, by)) in caps.iter().enumerate() {
            let (home, prepr) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a binding captured before its binding"))?;
            let src = self.home(home);
            let dst = (at.0, at.1 + offs[j] as i32);
            match by {
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    let a = self.address(src);
                    self.store(V::Scalar(a), Repr::Ptr, dst)?;
                }
                CaptureKind::ByValue => {
                    self.moved(e, p)?;
                    let v = self.load(prepr, src);
                    self.store(v, prepr, dst)?;
                }
            }
        }
        self.vals.push(V::Place(at.0, at.1));
        Ok(())
    }

    /// `f(a..)` with `f` a closure: the environment as its kind's `self` (a place for `Fn` and
    /// `FnMut`, whose address is passed; the value, moved, for `FnOnce`), then the arguments,
    /// then the call (`closure_call`).
    pub(super) fn push_closure_call(&mut self, e: ExprId, callee: ExprId, args: &[ExprId]) -> Result<(), BuildError> {
        let t = self.adjusted_ty(callee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(callee, error))?;
        let kind = self.parts_at(callee, g)?.kind;
        self.work.push(Task::Call(e));
        for &a in args.iter().rev() {
            if let Repr::Agg(_) = self.adjusted_repr(a)? {
                self.work.push(Task::ArgCopy(a));
            }
            self.work.push(Task::Eval(a));
        }
        match kind {
            FnTrait::Fn | FnTrait::FnMut => self.work.push(Task::Place(callee)),
            FnTrait::FnOnce => {
                if let Repr::Agg(_) = self.adjusted_repr(callee)? {
                    self.work.push(Task::ArgCopy(callee));
                }
                self.work.push(Task::Eval(callee));
            }
        }
        Ok(())
    }

    /// The tuple of the arguments of ground closure signature `sig`, and its output.
    fn args_tuple(&mut self, e: ExprId, sig: PTy) -> Result<(PTy, PTy), BuildError> {
        let full = |_| malformed("the ground table is full");
        let PNode::FnPtr(l, _) = self.ground_node(sig).map_err(|error| Self::ty_err(e, error))? else {
            return Err(malformed("a closure's signature that is not a fn pointer"));
        };
        let all = self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?;
        let (&output, inputs) = all.split_last().ok_or(malformed("a closure's signature without its output"))?;
        let mut own: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        own.extend_from_slice(inputs);
        let list = self.ground.intern_list(&own).map_err(full)?;
        Ok((self.ground.intern(PNode::Tuple(list)).map_err(full)?, output))
    }

    /// The instance of closure type `g`'s body: its kind's method at `[g, (A..)]`.
    fn body_instance(&mut self, e: ExprId, g: PTy, parts: Parts) -> Result<Instance, BuildError> {
        let item = self.adts.fn_trait_method(parts.kind).ok_or(BuildError::NoLangFn { expr: Some(e), name: method_name(parts.kind) })?;
        let (tuple, _) = self.args_tuple(e, parts.sig)?;
        let args = self.ground.intern_list(&[g, tuple]).map_err(|_| malformed("the ground table is full"))?;
        Ok(Instance { item, args })
    }

    /// The call `push_closure_call` set up: the environment (its address for `Fn`/`FnMut`) and
    /// the arguments on the stack, each crossing as a Rust parameter of its own.
    pub(super) fn closure_call(&mut self, e: ExprId, callee: ExprId, written: &[ExprId]) -> Result<(), BuildError> {
        let t = self.adjusted_ty(callee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(callee, error))?;
        let parts = self.parts_at(callee, g)?;
        let instance = self.body_instance(e, g, parts)?;
        let n = written.len() + 1;
        let base = self.vals.len().checked_sub(n).ok_or(malformed("a closure call's arguments missing from the value stack (a build bug)"))?;
        let mut reprs = Vec::with_capacity_in(n, self.alloc.clone());
        let mut passes = Vec::with_capacity_in(n, self.alloc.clone());
        match parts.kind {
            FnTrait::Fn | FnTrait::FnMut => {
                let V::Place(p, off) = self.vals[base] else {
                    return Err(malformed("a closure called by reference that is not a place"));
                };
                self.vals[base] = V::Scalar(self.address((p, off)));
                reprs.push(Repr::Ptr);
                passes.push(Self::rust_pass(Repr::Ptr)?);
            }
            FnTrait::FnOnce => {
                let repr = self.repr(g).map_err(|error| Self::ty_err(callee, error))?;
                reprs.push(repr);
                passes.push(self.pass(g, repr, Abi::Rust, None)?);
            }
        }
        for &a in written {
            let t = self.adjusted_ty(a)?;
            let ga = self.ground_ty(t).map_err(|error| Self::ty_err(a, error))?;
            let repr = self.repr_at(a, t)?;
            reprs.push(repr);
            passes.push(self.pass(ga, repr, Abi::Rust, None)?);
        }
        let ty = self.expr_ty(e)?;
        let gr = self.ground_ty(ty).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, ty)?;
        let rpass = self.pass(gr, rrepr, Abi::Rust, None)?;
        let place = self.ref_place(instance);
        let reference = self.b.reference(RefKind::Func, place);
        self.emit_call(Callee::Direct(reference), &reprs, &passes, rrepr, rpass)?;
        if let PNode::Never = self.node_at(e, ty)? {
            self.vals.pop();
            self.b.unreachable();
            self.dead_block();
            self.vals.push(V::None);
        }
        Ok(())
    }

    /// The body of `closure` as its kind's method: `(env by kind, a..)`. Each captured binding
    /// lives at its field, through the field's pointer for a by-reference capture; a by-value
    /// capture of an `FnOnce` body is the body's to drop.
    ///
    /// Asked as a weaker trait's method (`which` past the kind: `call_mut` or `call_once` on an
    /// `Fn` closure, `call_once` on an `FnMut` one), it is the thin ENTRY instead
    /// (`run_entry`); asked as a stronger one it is refused: core's `Fn: FnMut: FnOnce` gives a
    /// closure only its kind's method and the weaker ones.
    pub(super) fn run_closure(&mut self, closure: ExprId, which: FnTrait) -> Result<(), BuildError> {
        let t = self.expr_ty(closure)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(closure, error))?;
        let parts = self.parts_at(closure, g)?;
        if which < parts.kind {
            return Err(BuildError::UnsupportedType { expr: closure, what: "a closure called through a trait stronger than its kind" });
        }
        if which > parts.kind {
            return self.run_entry(closure, g, parts, which);
        }
        let (tuple, output) = self.args_tuple(closure, parts.sig)?;
        let PNode::Tuple(l) = self.ground_node(tuple).map_err(|error| Self::ty_err(closure, error))? else {
            return Err(malformed("a closure's arguments not a tuple"));
        };
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        inputs.extend_from_slice(self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?);
        if inputs.len() != self.body.params.len() {
            return Err(malformed("a closure whose signature and parameters differ in number"));
        }
        let env_repr = match parts.kind {
            FnTrait::Fn | FnTrait::FnMut => Repr::Ptr,
            FnTrait::FnOnce => self.repr(g).map_err(|error| Self::ty_err(closure, error))?,
        };
        let mut reprs = Vec::with_capacity_in(inputs.len() + 1, self.alloc.clone());
        let mut passes = Vec::with_capacity_in(inputs.len() + 1, self.alloc.clone());
        reprs.push(env_repr);
        passes.push(match parts.kind {
            FnTrait::Fn | FnTrait::FnMut => Self::rust_pass(Repr::Ptr)?,
            FnTrait::FnOnce => self.pass(g, env_repr, Abi::Rust, None)?,
        });
        for &gi in inputs.iter() {
            let repr = self.repr(gi).map_err(|error| Self::ty_err(closure, error))?;
            reprs.push(repr);
            passes.push(self.pass(gi, repr, Abi::Rust, None)?);
        }
        self.ret = self.repr(output).map_err(|error| Self::ty_err(closure, error))?;
        self.ret_pass = self.pass(output, self.ret, Abi::Rust, None)?;
        let (params, rets) = self.sig_of(&passes, self.ret_pass);
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        let entry = self.create();
        self.reached[entry.index()] = true;
        self.begin(entry);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc.clone());
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        let mut next = values.iter().copied();
        if let Pass::Ref | Pass::Memory { .. } = self.ret_pass {
            self.sret = Some(next.next().ok_or(malformed("a result pointer without its value"))?);
        }
        self.scope_marks.push(0);
        // The environment: a pointer to it, or its value (the caller's copy, the body's own).
        let env = match self.take_passed(env_repr, passes[0], &mut next)? {
            V::Scalar(p) => Some((p, 0)),
            V::Place(p, off) => Some((p, off)),
            V::None => None,
        };
        let mut caps = Vec::new_in(self.alloc.clone());
        self.captures_of(closure, &mut caps);
        let mut offs = Vec::new_in(self.alloc.clone());
        self.capture_offsets(closure, g, caps.len(), &mut offs)?;
        for (j, &(p, by)) in caps.iter().enumerate() {
            let (base, off) = env.ok_or(malformed("a closure that captures with no environment"))?;
            let at = (base, off + offs[j] as i32);
            match by {
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    let V::Scalar(ptr) = self.load(Repr::Ptr, at) else {
                        return Err(malformed("a capture's pointer not a scalar"));
                    };
                    self.bind_at(p, (ptr, 0))?;
                }
                CaptureKind::ByValue => {
                    self.bind_at(p, at)?;
                    if parts.kind == FnTrait::FnOnce {
                        self.own_pat(p)?;
                    }
                }
            }
        }
        for (i, &repr) in reprs.iter().enumerate().skip(1) {
            let p = self.body.params[i - 1];
            match self.take_passed(repr, passes[i], &mut next)? {
                V::Place(at, off) if matches!(self.pat(p)?, Pat::Bind { .. }) => self.bind_at(p, (at, off))?,
                v => self.bind_value(p, v)?,
            }
            self.own_pat(p)?;
        }
        self.walk_root()
    }

    /// The thin ENTRY of closure `g` as trait method `which`, weaker than its kind (codegen.md,
    /// "Closures and the Fn traits", ENTRIES): core's `trait Fn<Args>: FnMut<Args>` and `trait
    /// FnMut<Args>: FnOnce<Args>` make every `Fn` closure `FnMut` and `FnOnce` too.
    /// `call_mut(&mut self, args)` on an `Fn` closure passes its pointer on to the body;
    /// `call_once(self, args)` takes the environment by value, calls the body with its address
    /// and drops the environment after (the Reference, "Closure types", "Drop order": the
    /// captures are dropped when the closure value is). Its own instance, its own key.
    fn run_entry(&mut self, closure: ExprId, g: PTy, parts: Parts, which: FnTrait) -> Result<(), BuildError> {
        let (tuple, output) = self.args_tuple(closure, parts.sig)?;
        let PNode::Tuple(l) = self.ground_node(tuple).map_err(|error| Self::ty_err(closure, error))? else {
            return Err(malformed("a closure's arguments not a tuple"));
        };
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        inputs.extend_from_slice(self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?);
        let env_repr = self.repr(g).map_err(|error| Self::ty_err(closure, error))?;
        let n = inputs.len() + 1;
        let mut reprs = Vec::with_capacity_in(n, self.alloc.clone());
        let mut passes = Vec::with_capacity_in(n, self.alloc.clone());
        match which {
            FnTrait::Fn | FnTrait::FnMut => {
                reprs.push(Repr::Ptr);
                passes.push(Self::rust_pass(Repr::Ptr)?);
            }
            FnTrait::FnOnce => {
                reprs.push(env_repr);
                passes.push(self.pass(g, env_repr, Abi::Rust, None)?);
            }
        }
        for &gi in inputs.iter() {
            let repr = self.repr(gi).map_err(|error| Self::ty_err(closure, error))?;
            reprs.push(repr);
            passes.push(self.pass(gi, repr, Abi::Rust, None)?);
        }
        self.ret = self.repr(output).map_err(|error| Self::ty_err(closure, error))?;
        self.ret_pass = self.pass(output, self.ret, Abi::Rust, None)?;
        let (params, rets) = self.sig_of(&passes, self.ret_pass);
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        let entry = self.create();
        self.reached[entry.index()] = true;
        self.begin(entry);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc.clone());
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        let mut next = values.iter().copied();
        if let Pass::Ref | Pass::Memory { .. } = self.ret_pass {
            self.sret = Some(next.next().ok_or(malformed("a result pointer without its value"))?);
        }
        self.scope_marks.push(0);
        // The environment: its pointer passed on, or the value taken (the caller's copy is the
        // entry's own) and its address passed.
        let owned = match (which, self.take_passed(reprs[0], passes[0], &mut next)?) {
            (FnTrait::FnOnce, V::Place(p, off)) => Some((p, off)),
            (FnTrait::FnOnce, v) => {
                let at = self.temp(env_repr);
                self.store(v, env_repr, at)?;
                Some(at)
            }
            (_, v) => {
                self.vals.push(v);
                None
            }
        };
        if let Some(at) = owned {
            let a = self.address(at);
            self.vals.push(V::Scalar(a));
        }
        for i in 1..n {
            let v = self.take_passed(reprs[i], passes[i], &mut next)?;
            self.vals.push(v);
        }
        reprs[0] = Repr::Ptr;
        passes[0] = Self::rust_pass(Repr::Ptr)?;
        let instance = self.body_instance(closure, g, parts)?;
        let place = self.ref_place(instance);
        let reference = self.b.reference(RefKind::Func, place);
        let (rrepr, rpass) = (self.ret, self.ret_pass);
        self.emit_call(Callee::Direct(reference), &reprs, &passes, rrepr, rpass)?;
        let v = self.pop()?;
        if let PNode::Never = self.ground_node(output).map_err(|error| Self::ty_err(closure, error))? {
            self.b.unreachable();
        } else {
            if let Some(at) = owned
                && self.needs_drop(g).map_err(|error| Self::ty_err(closure, error))?
            {
                self.drop_at(g, at)?;
            }
            self.ret_value(v)?;
        }
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }
}
