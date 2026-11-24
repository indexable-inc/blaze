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

use blaze_typeck::{CaptureKind, EnvField};

use super::*;
use crate::FnTrait;

// The other function the language makes with no text: a tuple struct's or variant's ctor.
#[path = "ctor_fn.rs"]
mod ctor_fn;
pub(crate) use ctor_fn::build_ctor_fn;

// And the `Clone::clone` it gives a tuple, a closure, a fn item and a fn pointer.
#[path = "clone_fn.rs"]
mod clone_fn;
pub(crate) use clone_fn::{build_builtin_clone, build_builtin_from};

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

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether the checker resolved call `e` through a `Fn*` trait (its callee row names
    /// `call`, `call_mut` or `call_once`): a callee of a type parameter `F: Fn*(..)`, which an
    /// instance may ground to a closure. The Reference, "Call expressions": "for closures and
    /// other types implementing the `Fn*` traits, the call is resolved to the trait's method":
    /// the trait the bound names, not the closure's kind, so an `Fn` closure called through `F:
    /// FnOnce` takes its `call_once` entry (its environment consumed and dropped by the call).
    pub(super) fn through_fn_trait(&self, e: ExprId) -> bool {
        let Some(at) = self.checked.callees.column::<0>().iter().position(|&x| x == e) else {
            return false;
        };
        let item = self.checked.callees.column::<1>()[at];
        [FnTrait::Fn, FnTrait::FnMut, FnTrait::FnOnce].into_iter().any(|w| self.adts.fn_trait_method(w) == Some(item))
    }

    /// Ground closure type `g` read: its identity (owner, ordinal, the owner's arguments) from
    /// the type, its kind, signature and environment from the seam ([`LayoutFacts::closure`],
    /// the owner's check record) grounded at those arguments, as an ADT's fields are.
    pub(super) fn closure_parts(&mut self, g: PTy) -> Result<Parts, TyErr> {
        let PNode::Closure { owner, ordinal, args } = self.ground_node(g)? else {
            return Err(TyErr::Malformed("a closure's parts asked of another type"));
        };
        let facts = self.adts.facts();
        let c = facts.closure(owner, ordinal).ok_or(TyErr::Unsupported("a closure whose facts the world did not answer"))?;
        let mut a: Vec<PTy, A> = Vec::new_in(self.alloc);
        a.extend_from_slice(self.ground.view().list(args).map_err(|_| TyErr::Malformed("a list past the ground table"))?);
        let sig = crate::instance::ground(c.types, c.sig, &a, self.ground, self.scratch, facts).map_err(TyErr::Other)?;
        let env = crate::instance::ground(c.types, c.env, &a, self.ground, self.scratch, facts).map_err(TyErr::Other)?;
        Ok(Parts { ordinal, sig, env, owner, owner_args: args, kind: c.kind })
    }

    /// The parts of ground closure type `g`, met at `e`.
    fn parts_at(&mut self, e: ExprId, g: PTy) -> Result<Parts, BuildError> {
        self.closure_parts(g).map_err(|error| Self::ty_err(e, error))
    }

    /// The offset of field `j` (by declaration index) of the struct or tuple laid out as row
    /// `l`: walk.rs `decl_field`, the one lookup walk/moves/env_places.rs `env_field_offset`
    /// shares.
    pub(super) fn field_offset(&self, l: u32, j: usize) -> Result<u32, BuildError> {
        let Shape::Fields { .. } = self.layouts[l as usize].shape else {
            return Err(malformed("a closure's environment not laid out as a struct"));
        };
        let j = u32::try_from(j).map_err(|_| malformed("a capture past its environment's fields"))?;
        Ok(self.decl_field(l, 0, j)?.0)
    }

    /// The invariant of `layout::captures`, checked where the walk reads an environment: ground
    /// environment `env` of closure or `async` block `e` has exactly `n` fields, one per capture
    /// row. A world whose facts came from another record of the owner breaks it; a debug build
    /// stops naming the block, its owner and both counts, a release one refuses it by name.
    pub(super) fn env_holds(&self, e: ExprId, env: PTy, n: usize) -> Result<(), BuildError> {
        let fields = match self.ground_node(env) {
            Ok(PNode::Tuple(l)) => self.ground.view().list(l).map(<[PTy]>::len).map_err(|_| malformed("a list past the ground table"))?,
            _ => return Err(malformed("an environment that is not a tuple")),
        };
        debug_assert_eq!(
            fields, n,
            "closure or async block {e:?} of {:?}: its environment has {fields} fields, its capture list (`Checked::captures`) {n} rows",
            self.body.owner
        );
        if fields != n {
            return Err(BuildError::Unsupported {
                expr: e,
                what: "an environment whose field count is not its capture list's (the world's facts and the walk read two records of the owner)",
            });
        }
        Ok(())
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
        self.env_holds(e, parts.env, n)?;
        let base = self.field_offset(lc, 0).map_err(|_| malformed("a closure type's layout without its one field, the environment"))?;
        for j in 0..n {
            out.push(base + self.field_offset(le, j)?);
        }
        Ok(())
    }

    /// `|..| body`: its environment, each by-reference capture the address of the binding's
    /// home, each by-value capture its value moved in (the Reference, "Closure types",
    /// "Capture modes"). A field holding a place below its binding (`x.a`, RFC 2229) is the
    /// place's address or value (walk/moves/env_places.rs `store_env_place`).
    pub(super) fn closure_value(&mut self, e: ExprId) -> Result<(), BuildError> {
        let t = self.expr_ty(e)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
        let repr = self.repr(g).map_err(|error| Self::ty_err(e, error))?;
        let mut fields: Vec<EnvField, A> = Vec::new_in(self.alloc);
        for f in self.checked.env_fields_by::<crate::layout::BuildEnv>(e) {
            fields.push(f);
        }
        if let Repr::None = repr {
            for &f in fields.iter() {
                if !f.is_whole() {
                    let zst = self.temp(Repr::None);
                    self.store_env_place(e, f, zst)?;
                }
            }
            self.push_val(V::None, None);
            return Ok(());
        }
        let mut offs = Vec::new_in(self.alloc);
        self.capture_offsets(e, g, fields.len(), &mut offs)?;
        let at = self.temp(repr);
        for (j, &f) in fields.iter().enumerate() {
            let dst = (at.0, at.1 + offs[j] as i32);
            if !f.is_whole() {
                self.store_env_place(e, f, dst)?;
                continue;
            }
            let (p, by) = (f.base, f.how);
            let (home, prepr) = self.slots.get(p.ix()).copied().flatten().ok_or(malformed("a binding captured before its binding"))?;
            let src = self.home(home);
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
        self.push_val(V::Place(at.0, at.1), None);
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
        let mut own: Vec<PTy, A> = Vec::new_in(self.alloc);
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
    /// The fn pointer a closure that captures nothing coerces to (`AdjustKind::ClosureFnPointer`;
    /// the Reference, "Type coercions": "Non capturing closures to `fn` pointers"): the address
    /// of its `call_once` at `[C, (A..)]`, `before` its type. Its environment is zero-sized (the
    /// checker coerces only a closure with no capture), so `self` crosses as nothing and that
    /// function's parameters are the pointer's own: one function per closure, the same one an
    /// `FnOnce` call of it reaches (its body when the kind is `FnOnce`, else its entry).
    pub(super) fn closure_fn_addr(&mut self, e: ExprId, before: PTy) -> Result<Value, BuildError> {
        let g = self.ground_ty(before).map_err(|error| Self::ty_err(e, error))?;
        let parts = self.parts_at(e, g)?;
        let lc = self.layout(g).map_err(|error| Self::ty_err(e, error))?;
        if self.layouts[lc as usize].size != 0 {
            return Err(BuildError::UnsupportedType { expr: e, what: "a closure with captures coerced to a fn pointer" });
        }
        let instance = self.body_instance(e, g, Parts { kind: FnTrait::FnOnce, ..parts })?;
        Ok(self.fn_addr(instance))
    }

    pub(super) fn closure_call(&mut self, e: ExprId, callee: ExprId, written: &[ExprId]) -> Result<(), BuildError> {
        let t = self.adjusted_ty(callee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(callee, error))?;
        let parts = self.parts_at(callee, g)?;
        let instance = self.body_instance(e, g, parts)?;
        let n = written.len() + 1;
        let base = self.vals.len().checked_sub(n).ok_or(malformed("a closure call's arguments missing from the value stack (a build bug)"))?;
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        match parts.kind {
            FnTrait::Fn | FnTrait::FnMut => {
                let V::Place(p, off) = self.vals[base] else {
                    return Err(malformed("a closure called by reference that is not a place"));
                };
                self.vals[base] = V::Scalar(self.address((p, off)));
                reprs.push(Repr::Ptr);
                passes.push(self.rust_pass(Repr::Ptr)?);
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
        self.call_instance(Some(e), instance, &reprs, &passes, None, rrepr, rpass)?;
        if let PNode::Never = self.node_at(e, ty)? {
            self.pop_val();
            self.unreachable_of(TrapKind::AfterDivergingCall, Some(e));
            self.dead_block();
            self.push_val(V::None, None);
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
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc);
        inputs.extend_from_slice(self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?);
        if inputs.len() != self.body.params.len() {
            return Err(malformed("a closure whose signature and parameters differ in number"));
        }
        let env_repr = match parts.kind {
            FnTrait::Fn | FnTrait::FnMut => Repr::Ptr,
            FnTrait::FnOnce => self.repr(g).map_err(|error| Self::ty_err(closure, error))?,
        };
        let mut reprs = Vec::with_capacity_in(inputs.len() + 1, self.alloc);
        let mut passes = Vec::with_capacity_in(inputs.len() + 1, self.alloc);
        reprs.push(env_repr);
        passes.push(match parts.kind {
            FnTrait::Fn | FnTrait::FnMut => self.rust_pass(Repr::Ptr)?,
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
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
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
            V::Unsized(..) => return Err(malformed("a closure's environment as an unsized place")),
        };
        let mut fields: Vec<EnvField, A> = Vec::new_in(self.alloc);
        for f in self.checked.env_fields_by::<crate::layout::BuildEnv>(closure) {
            fields.push(f);
        }
        let mut offs = Vec::new_in(self.alloc);
        self.capture_offsets(closure, g, fields.len(), &mut offs)?;
        for (j, &f) in fields.iter().enumerate() {
            // A place below its binding (`x.a`): its uses read the field (walk/moves/env_places.rs).
            if !f.is_whole() {
                let at = env.map(|(base, off)| (base, off + offs[j] as i32));
                self.bind_env_place(closure, f, at, parts.kind)?;
                continue;
            }
            self.note_env_whole(f);
            let (p, by) = (f.base, f.how);
            // A zero-sized environment passed by value (an `FnOnce` body; the environment is
            // its captures, so each is a by-value capture of a zero-sized value: `move ||
            // marker` with `marker: PhantomData<T>` or a unit struct, as libc-adjacent
            // `OrErrno::true_val_or_errno` writes) is passed as nothing: each capture is
            // bound to its own zero-sized slot, owned as any by-value capture (its `Drop`, if
            // any, runs at the body's end). A by-reference capture is a pointer, never in a
            // zero-sized environment.
            let Some((base, off)) = env else {
                if by != CaptureKind::ByValue {
                    return Err(malformed("a by-reference capture in a zero-sized closure environment"));
                }
                self.bind(p)?;
                if parts.kind == FnTrait::FnOnce {
                    self.own(p, true)?;
                }
                continue;
            };
            let at = (base, off + offs[j] as i32);
            match by {
                CaptureKind::ByRef | CaptureKind::ByMutRef => {
                    let V::Scalar(ptr) = self.load(Repr::Ptr, at) else {
                        return Err(malformed("a capture's pointer not a scalar"));
                    };
                    self.bind_capture_at(p, (ptr, 0))?;
                }
                CaptureKind::ByValue => {
                    self.bind_capture_at(p, at)?;
                    // The variable alone (not its subpattern's bindings: each is its own capture).
                    if parts.kind == FnTrait::FnOnce {
                        self.own(p, true)?;
                    }
                }
            }
        }
        self.settle_env_places(closure)?;
        for (i, &repr) in reprs.iter().enumerate().skip(1) {
            let p = self.body.params[i - 1];
            match self.take_passed(repr, passes[i], &mut next)? {
                V::Place(at, off) if matches!(self.pat(p)?, Pat::Bind { .. }) => {
                    self.bind_at(p, (at, off))?;
                    self.own_pat(p)?;
                }
                // The parameter's own type (`|Foo { a }: &Foo| ..` is recorded at the peeled
                // one); bound and owned as a fn's parameter is (walk/matches/irrefutable.rs
                // `bind_param`: `|_: String| ..` holds its argument to the closure's end).
                v => self.bind_param(p, v, inputs[i - 1])?,
            }
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
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc);
        inputs.extend_from_slice(self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?);
        let env_repr = self.repr(g).map_err(|error| Self::ty_err(closure, error))?;
        let n = inputs.len() + 1;
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        match which {
            FnTrait::Fn | FnTrait::FnMut => {
                reprs.push(Repr::Ptr);
                passes.push(self.rust_pass(Repr::Ptr)?);
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
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
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
            (_, V::Unsized(..)) => return Err(malformed("a closure's environment as an unsized place")),
            (FnTrait::FnOnce, V::Place(p, off)) => Some((p, off)),
            (FnTrait::FnOnce, v) => {
                let at = self.temp(env_repr);
                self.store(v, env_repr, at)?;
                Some(at)
            }
            (_, v) => {
                self.push_val(v, None);
                None
            }
        };
        if let Some(at) = owned {
            let a = self.address(at);
            self.push_val(V::Scalar(a), None);
        }
        for i in 1..n {
            let v = self.take_passed(reprs[i], passes[i], &mut next)?;
            self.push_val(v, None);
        }
        reprs[0] = Repr::Ptr;
        passes[0] = self.rust_pass(Repr::Ptr)?;
        let instance = self.body_instance(closure, g, parts)?;
        let place = self.ref_place(instance);
        let reference = self.b.reference(RefKind::Func, place);
        let (rrepr, rpass) = (self.ret, self.ret_pass);
        self.emit_call(Callee::Direct(reference), &reprs, &passes, rrepr, rpass)?;
        let v = self.pop()?;
        if let PNode::Never = self.ground_node(output).map_err(|error| Self::ty_err(closure, error))? {
            self.unreachable_of(TrapKind::AfterDivergingCall, Some(closure));
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

    /// The SHIM of a fn item or fn pointer `callee` as `Fn*` method `which` at `[callee,
    /// tuple]` (lib.rs `build_fn_shim`): `self` taken as `which` takes it (a pointer for `call`
    /// and `call_mut`, the value for `call_once`), then the tuple's fields, each a Rust parameter
    /// of its own (lib.rs `Abi::RustCall`). A fn item's `self` is zero-sized and ignored, and
    /// the item is called directly; a fn pointer's is loaded (through `self` for `call` and
    /// `call_mut`) and called INDIRECTLY. The fields cross to the callee as its own ABI passes
    /// its parameters, so an `extern "C"` fn item or pointer is called as C.
    fn run_fn_shim(&mut self, callee: PTy, tuple: PTy, which: FnTrait, sig: Option<&FnSig<'_>>) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let past = |_| malformed("a list past the ground table");
        let PNode::Tuple(l) = self.ground_node(tuple).map_err(sig_err)? else {
            return Err(malformed("a fn shim's arguments not a tuple"));
        };
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc);
        inputs.extend_from_slice(self.ground.view().list(l).map_err(past)?);
        // The direct callee (a fn item's instance) or none (a pointer), the callee's ABI, the
        // item a C refusal names, and the output.
        let (target, abi, item, output) = match self.ground_node(callee).map_err(sig_err)? {
            PNode::FnDef(item, args) => {
                let sig = sig.ok_or(malformed("a fn item's shim built without the item's signature"))?;
                if self.ground.view().list(args).map_err(past)? != self.args {
                    return Err(malformed("a fn item's shim whose instance arguments are not the item's"));
                }
                if sig.inputs.len() != inputs.len() {
                    return Err(malformed("a fn item's shim whose tuple and signature differ in number"));
                }
                let output = self.ground_in(sig.types, sig.output).map_err(sig_err)?;
                let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
                (Some(Instance { item, args }), abi, Some(item), output)
            }
            PNode::FnPtr(fl, header) => {
                let all = self.ground.view().list(fl).map_err(past)?;
                let (&output, ins) = all.split_last().ok_or(malformed("a fn pointer type without its output"))?;
                if ins.len() != inputs.len() {
                    return Err(malformed("a fn pointer's shim whose tuple and signature differ in number"));
                }
                let abi = pointer_abi(header.abi).ok_or(BuildError::UnsupportedSig {
                    what: "a fn pointer outside the Rust, rust-call and C-family calling conventions used as an `Fn*` value",
                })?;
                (None, abi, None, output)
            }
            _ => return Err(malformed("a fn shim asked of a type that is neither a fn item nor a fn pointer")),
        };
        // core's function.rs: "rust-call" is not `Fn*`-compatible; the checker never makes this.
        if abi == Abi::RustCall {
            return Err(BuildError::UnsupportedSig { what: "a \"rust-call\" function used as an `Fn*` value" });
        }
        let self_repr = match which {
            FnTrait::Fn | FnTrait::FnMut => Repr::Ptr,
            FnTrait::FnOnce => self.repr(callee).map_err(sig_err)?,
        };
        let n = inputs.len() + 1;
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        reprs.push(self_repr);
        passes.push(self.rust_pass(self_repr)?);
        for &gi in inputs.iter() {
            let repr = self.repr(gi).map_err(sig_err)?;
            reprs.push(repr);
            passes.push(self.rust_pass(repr)?);
        }
        self.ret = self.repr(output).map_err(sig_err)?;
        self.ret_pass = self.rust_pass(self.ret)?;
        let (params, rets) = self.sig_of(&passes, self.ret_pass);
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        let entry = self.create();
        self.reached[entry.index()] = true;
        self.begin(entry);
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        let mut next = values.iter().copied();
        if let Pass::Ref | Pass::Memory { .. } = self.ret_pass {
            self.sret = Some(next.next().ok_or(malformed("a result pointer without its value"))?);
        }
        self.scope_marks.push(0);
        // `self`: nothing to read for a fn item; the pointer's value for a fn pointer.
        let me = self.take_passed(reprs[0], passes[0], &mut next)?;
        // A tracked fn item's hidden last argument: a row of this shim's `Fn*` method's
        // definition site (walk/track_caller.rs `shim_caller_arg`). A fn pointer's target is
        // never tracked directly (reifying a tracked fn points to its `ReifyShim`, which has
        // the plain fn pointer ABI).
        let mut loc = None;
        let callee_ref = match target {
            Some(instance) => {
                loc = self.shim_caller_arg(instance)?;
                let place = self.ref_place(instance);
                Callee::Direct(self.b.reference(RefKind::Func, place))
            }
            None => {
                let p = match (which, me) {
                    (FnTrait::Fn | FnTrait::FnMut, V::Scalar(at)) => match self.load(Repr::Ptr, (at, 0)) {
                        V::Scalar(p) => p,
                        _ => return Err(malformed("a fn pointer loaded as other than a scalar")),
                    },
                    (FnTrait::FnOnce, V::Scalar(p)) => p,
                    _ => return Err(malformed("a fn pointer's `self` not passed as a pointer")),
                };
                Callee::Pointer(p)
            }
        };
        let mut treprs = Vec::with_capacity_in(n - 1, self.alloc);
        let mut tpasses = Vec::with_capacity_in(n - 1, self.alloc);
        for i in 1..n {
            let v = self.take_passed(reprs[i], passes[i], &mut next)?;
            self.push_val(v, None);
            treprs.push(reprs[i]);
            tpasses.push(self.pass(inputs[i - 1], reprs[i], abi, item)?);
        }
        if let Some(loc) = loc {
            // As `Walk::call_instance` passes it: one pointer, last.
            self.push_val(V::Scalar(loc), None);
            treprs.push(Repr::Ptr);
            tpasses.push(track_caller::PASS);
        }
        let rrepr = self.ret;
        let rpass = self.pass(output, rrepr, abi, item)?;
        self.emit_call(callee_ref, &treprs, &tpasses, rrepr, rpass)?;
        let v = self.pop()?;
        if let PNode::Never = self.ground_node(output).map_err(sig_err)? {
            self.unreachable_of(TrapKind::AfterDivergingCall, None);
        } else {
            self.ret_value(v)?;
        }
        self.check_lir_error()
    }

    fn check_lir_error(&self) -> Result<(), BuildError> {
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }

    /// The `ReifyShim` of a `#[track_caller]` fn `callee` at `self.args` (lib.rs
    /// `build_reify_shim`): takes the plain fn pointer ABI (no hidden `&Location` parameter),
    /// materializes a `Location` row for `callee`'s own definition site (`Adts::def_site`), and
    /// calls `callee` directly with that location passed last.
    fn run_reify_shim(&mut self, callee: ItemKey, sig: &FnSig<'_>) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let generics = sig.parent_count.checked_add(sig.own_count).ok_or(malformed("a generic count past u32"))?;
        if u32::try_from(self.args.len()).ok() != Some(generics) {
            return Err(malformed("a reify shim whose argument count is not its fn's generic count"));
        }
        let abi = self.adts.abi(callee).ok_or(BuildError::NoAbi { item: callee })?;
        if abi == Abi::RustCall {
            return Err(BuildError::UnsupportedSig { what: "a \"rust-call\" function taken as a fn pointer" });
        }
        let n = sig.inputs.len();
        let mut reprs = blaze_own::Vec::with_capacity_in(n, self.alloc);
        let mut passes = blaze_own::Vec::with_capacity_in(n, self.alloc);
        for &t in sig.inputs.iter() {
            let g = self.ground_in(sig.types, t).map_err(sig_err)?;
            let repr = self.repr(g).map_err(sig_err)?;
            let pass = self.pass(g, repr, abi, Some(callee))?;
            reprs.push(repr);
            passes.push(pass);
        }
        let output = self.ground_in(sig.types, sig.output).map_err(sig_err)?;
        self.ret = self.repr(output).map_err(sig_err)?;
        self.ret_pass = self.pass(output, self.ret, abi, Some(callee))?;
        let (params, rets) = self.sig_of(&passes, self.ret_pass);
        let own = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own);
        let entry = self.create();
        if let Some(slot) = self.reached.get_mut(entry.index()) {
            *slot = true;
        }
        self.begin(entry);
        let mut values = blaze_own::Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        let mut next = values.iter().copied();
        if let Pass::Ref | Pass::Memory { .. } = self.ret_pass {
            self.sret = Some(next.next().ok_or(malformed("a result pointer without its value"))?);
        }
        self.scope_marks.push(0);
        let args = self.ground.intern_list(self.args).map_err(|_| malformed("the ground table is full"))?;
        let target = Instance { item: callee, args };
        let loc = self.reify_caller_arg(target)?;
        let callee_nounwind = self.adts.nounwind(callee) == Some(true);
        let place = self.ref_place(target);
        let callee_ref = Callee::Direct(self.b.reference(RefKind::Func, place));
        let mut treprs = blaze_own::Vec::with_capacity_in(n + usize::from(loc.is_some()), self.alloc);
        let mut tpasses = blaze_own::Vec::with_capacity_in(n + usize::from(loc.is_some()), self.alloc);
        for (&repr, &pass) in reprs.iter().zip(passes.iter()) {
            let v = self.take_passed(repr, pass, &mut next)?;
            self.push_val(v, None);
            treprs.push(repr);
            tpasses.push(pass);
        }
        if let Some(loc) = loc {
            self.push_val(V::Scalar(loc), None);
            treprs.push(Repr::Ptr);
            tpasses.push(track_caller::PASS);
        }
        let rrepr = self.ret;
        let rpass = self.ret_pass;
        self.emit_call_from(callee_ref, &treprs, &tpasses, None, callee_nounwind, rrepr, rpass)?;
        let v = self.pop()?;
        if let PNode::Never = self.ground_node(output).map_err(sig_err)? {
            self.unreachable_of(TrapKind::AfterDivergingCall, None);
        } else {
            self.ret_value(v)?;
        }
        self.check_lir_error()
    }
}

/// An empty check record: a shim has no body of its own, and its walk reads none.
pub(crate) fn empty_record(a: Alloc<'_>) -> Checked<'_> {
    Checked {
        types: PortableTys::new_in(a),
        expr_ty: blaze_own::Vec::new_in(a),
        pat_ty: blaze_own::Vec::new_in(a),
        local_ty: blaze_own::Vec::new_in(a),
        adjusts: blaze_own::Soa::new_in(a),
        callees: blaze_own::Soa::new_in(a),
        expr_defs: blaze_own::Soa::new_in(a),
        const_args: blaze_own::Soa::new_in(a),
        pat_const_args: blaze_own::Soa::new_in(a),
        pat_defs: blaze_own::Soa::new_in(a),
        union_pats: blaze_own::Soa::new_in(a),
        fields: blaze_own::Soa::new_in(a),
        captures: blaze_own::Soa::new_in(a),
        capture_places: blaze_own::Soa::new_in(a),
        capture_projs: blaze_own::Soa::new_in(a),
        closures: blaze_own::Soa::new_in(a),
        diags: blaze_own::Vec::new_in(a),
        why: blaze_own::Vec::new_in(a),
        reads: blaze_typeck::ReadList { rows: blaze_own::Soa::new_in(a) },
        needs: blaze_typeck::Needs::new_in(a),
        suggests: blaze_typeck::suggest::Suggests::new_in(a),
        hidden: blaze_own::Vec::new_in(a),
        promoted: blaze_own::Vec::new_in(a),
        stalled: blaze_own::Vec::new_in(a),
        coroutine_witnesses: blaze_own::Vec::new_in(a),
        awaited: blaze_own::Vec::new_in(a),
        selected: blaze_own::Vec::new_in(a),
        fru_fields: blaze_own::Soa::new_in(a),
        default_fields: blaze_own::Soa::new_in(a),
        index_mut: blaze_own::Vec::new_in(a),
        findings: blaze_own::Vec::new_in(a),
        undecided: blaze_own::Vec::new_in(a),
        inferred_output: None,
    }
}

/// Builds the shim of fn item or fn pointer `callee` as `Fn*` method `which` at `[callee,
/// tuple]` (lib.rs `build_fn_shim`, `Walk::run_fn_shim`). Its walk is given an empty body,
/// named by the trait method, and an empty check record: neither is read.
pub(crate) fn build_fn_shim<A: Allocator + Copy>(
    callee: PTy,
    tuple: PTy,
    which: FnTrait,
    sig: Option<&FnSig<'_>>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let owner = adts.fn_trait_method(which).ok_or(BuildError::NoLangFn { expr: None, name: method_name(which) })?;
    let checked = empty_record(subst.scratch);
    let body = empty_view(owner, checked.types.view());
    let mut w = Walk::new(&body, &checked, subst, adts, alloc);
    w.run_fn_shim(callee, tuple, which, sig)?;
    w.into_built()
}

/// Builds the `ReifyShim` of `#[track_caller]` fn `callee` (lib.rs `build_reify_shim`,
/// `Walk::run_reify_shim`). Its walk is given an empty body named by `callee` and an empty check
/// record: neither is read.
pub(crate) fn build_reify_shim<A: Allocator + Copy>(
    callee: ItemKey,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let checked = empty_record(subst.scratch);
    let body = empty_view(callee, checked.types.view());
    let mut w = Walk::new(&body, &checked, subst, adts, alloc);
    w.run_reify_shim(callee, sig)?;
    w.into_built()
}

/// The body of a function the language makes, with no text of its own (a fn shim, a ctor's fn:
/// ctor_fn.rs), named by `owner`: nothing in it, so its walk reads none of it.
pub(crate) fn empty_view(owner: ItemKey, types: PTys<'_>) -> BodyView<'_> {
    BodyView {
        owner,
        edition: Edition::E2024,
        params: &[],
        root: ExprId(0),
        exprs: &[],
        expr_ranges: &[],
        pats: &[],
        pat_ranges: &[],
        stmts: &[],
        expr_lists: &[],
        pat_lists: &[],
        stmt_lists: &[],
        arms: &[],
        field_inits: &[],
        field_pats: &[],
        paths: &[],
        segments: &[],
        segment_ranges: &[],
        types,
        type_lists: &[],
        type_list_ranges: &[],
        method_name_ranges: &[],
        opt_types: &[],
        fmt_uses: &[],
        coroutine: None,
        super_lets: &[],
        asm_pieces: &[],
        asm_operands: &[],
        asm_clobbers: &[],
        asm_text: "",
        call_arg_ints: &[],
        legacy_args: &[],
        unsafe_blocks: &[],
        expr_editions: &[],
        pat_editions: &[],
        record_expr_schemas: &[],
        closure_static_params: &[],
    }
}
