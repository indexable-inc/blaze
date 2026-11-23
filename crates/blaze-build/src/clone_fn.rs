//! The `Clone::clone` the LANGUAGE gives, as a function of its own: a child module of
//! closure.rs (and so of walk.rs), beside the fn shim and the ctor's fn.
//!
//! The Reference, "Special types and traits", `Clone`: "In addition to the implementors listed
//! below, the following types also implement `Clone`: function item types, function pointer
//! types, closures that capture no values or only values that implement `Clone`, tuples of types
//! that implement `Clone`"; `Copy` likewise for those whose parts are `Copy`. core's clone.rs:
//! the builtin impls of tuples, closures, fn items and fn pointers have no text (rustc builds a
//! shim: `CloneShim`). What it does, by the language's rules:
//! - `Self: Copy` (every fn item and fn pointer; a tuple or closure whose parts are all `Copy`):
//!   the value's bytes copied. A field's own `clone` is NOT called: core's clone.rs says a
//!   `Copy` type's `Clone` "is equivalent to `*self`", and a program can observe the difference
//!   (a field whose `clone` has a side effect).
//! - Otherwise each field in order (a tuple's in index order, a closure's captures in capture
//!   order, its environment's fields) through its own `Clone::clone`; if a later one unwinds,
//!   the clones already made are dropped, the later first (the value stack's owned operands:
//!   walk/val_stack.rs, the Reference's "Destructors" on operands of an expression that panics).
//!
//! Whether `Self: Copy` is the checker's answer (the build's seam `FrontEnd::holds`, the solver's
//! builtin rule), handed in as `copy`: the builder never re-derives it.

use super::*;

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// The builtin `clone` of `Self` (`self.args[0]`), `clone` the trait's method its fields'
    /// clones call: one parameter, `&self`, the Rust ABI's; its result `Self`.
    fn run_builtin_clone(&mut self, clone: ItemKey, copy: bool) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let past = |_| malformed("a list past the ground table");
        let &[me] = self.args else {
            return Err(malformed("a builtin `clone` whose instance is not `[Self]`"));
        };
        // The fields to clone, in order, and where `Self`'s layout holds each: a tuple's own,
        // a closure's its environment's inside its one field (closure.rs `capture_offsets`).
        let mut fields: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        let mut base = 0u32;
        let held = match self.ground_node(me).map_err(sig_err)? {
            PNode::FnDef(..) | PNode::FnPtr(..) => None,
            PNode::Tuple(l) | PNode::Record(l) => {
                fields.extend_from_slice(self.ground.view().list(l).map_err(past)?);
                Some(me)
            }
            PNode::Closure { .. } => {
                let parts = self.closure_parts(me).map_err(sig_err)?;
                let PNode::Tuple(l) = self.ground_node(parts.env).map_err(sig_err)? else {
                    return Err(malformed("a closure's environment that is not a tuple"));
                };
                fields.extend_from_slice(self.ground.view().list(l).map_err(past)?);
                if !fields.is_empty() {
                    let lc = self.layout(me).map_err(sig_err)?;
                    base = self.field_offset(lc, 0).map_err(|_| malformed("a closure type's layout without its one field, the environment"))?;
                }
                Some(parts.env)
            }
            _ => return Err(malformed("a builtin `clone` of a type whose `Clone` the language does not give")),
        };
        // A fn item or fn pointer has no field: its clone is its bytes (the ladder asks no
        // `Copy` of it when the program declares none: clone_shim.rs `Clones::note`).
        let copy = copy || held.is_none();
        self.ret = self.repr(me).map_err(sig_err)?;
        self.ret_pass = self.rust_pass(self.ret)?;
        let me_pass = self.rust_pass(Repr::Ptr)?;
        let (params, rets) = self.sig_of(&[me_pass], self.ret_pass);
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
        let V::Scalar(at) = self.take_passed(Repr::Ptr, me_pass, &mut next)? else {
            return Err(malformed("a builtin `clone`'s `&self` not passed as a pointer"));
        };
        match held {
            Some(held) if !copy => self.clone_fields(clone, at, held, base, &fields)?,
            _ => {
                // `*self`: the bytes, whatever `Self` holds.
                let ret = self.ret;
                let v = self.load(ret, (at, 0));
                self.ret_value(v)?;
            }
        }
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }

    /// `(self.0.clone(), self.1.clone(), ..)` of the fields `fields` of `held` (a tuple), at
    /// `base` inside the value `at` points to, then returned as `Self`. Each clone made waits on
    /// the value stack as an owned operand, so a later clone that unwinds drops it (val_stack.rs).
    fn clone_fields(&mut self, clone: ItemKey, at: Value, held: PTy, base: u32, fields: &[PTy]) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let full = |_| malformed("the ground table is full");
        let lh = if fields.is_empty() { None } else { Some(self.layout(held).map_err(sig_err)?) };
        let mut offs: Vec<(u32, Repr), A> = Vec::with_capacity_in(fields.len(), self.alloc.clone());
        let mark = self.vals.len();
        let ptr_pass = self.rust_pass(Repr::Ptr)?;
        for (j, &ft) in fields.iter().enumerate() {
            let l = lh.ok_or(malformed("a field of a type with no layout"))?;
            let off = base + self.field_offset(l, j)?;
            let frepr = self.repr(ft).map_err(sig_err)?;
            let field = self.address((at, off as i32));
            self.push_val(V::Scalar(field), None);
            let args = self.ground.intern_list(&[ft]).map_err(full)?;
            let rpass = self.rust_pass(frepr)?;
            self.call_instance(None, Instance { item: clone, args }, &[Repr::Ptr], &[ptr_pass], None, frepr, rpass)?;
            let v = self.pop()?;
            let owned = self.owned_arg(frepr);
            self.push_val(v, owned);
            offs.push((off, frepr));
        }
        let v = match self.ret {
            Repr::None => V::None,
            repr @ Repr::Agg(_) => {
                let to = self.temp(repr);
                for (k, &(off, frepr)) in offs.iter().enumerate() {
                    let v = self.vals[mark + k];
                    self.store(v, frepr, (to.0, to.1 + off as i32))?;
                }
                V::Place(to.0, to.1)
            }
            _ => return Err(malformed("a builtin `clone` of a tuple or closure held as a scalar")),
        };
        // The clones are moved into the result: nothing waits to be dropped.
        self.truncate_vals(mark);
        self.ret_value(v)
    }
}

/// Builds the builtin `clone` of `Self` (`subst.args`, one type: a tuple, a closure, a fn item
/// or a fn pointer) at the trait method `clone` (lib.rs `build_builtin_clone`); `copy` is the
/// checker's answer to `Self: Copy`. Its walk is given an empty body named by `clone` and an
/// empty check record: neither is read.
pub(crate) fn build_builtin_clone<A: Allocator + Clone>(
    clone: ItemKey,
    copy: bool,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let checked = super::empty_record(subst.scratch);
    let body = super::empty_view(clone, checked.types.view());
    let mut w = Walk::new(&body, &checked, subst, adts, alloc);
    w.run_builtin_clone(clone, copy)?;
    w.into_built()
}
