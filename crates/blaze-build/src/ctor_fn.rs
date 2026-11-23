//! A tuple struct's or tuple variant's CTOR as a function of its own: a child module of
//! closure.rs (and so of walk.rs), beside the fn shim.
//!
//! The Reference, "Structs": "A tuple struct expression ... also defines a constructor function
//! in the value namespace"; "Enumerations": a tuple-like variant's name is likewise a function
//! (`Some` is `fn(T) -> Option<T>`); "Function item types": a ctor's name has a fn item type, so
//! it is a value that coerces to a fn pointer and implements `Fn*` (`.map(Some)`,
//! `.map_err(Error::Io)`, `let p: fn(u8) -> W = W;`, and core's `pattern_methods!` writing
//! `($pmap)(self)`, whose callee is a parenthesized path the direct-call form does not read).
//! A call whose callee IS the ctor's path is built in place (walk.rs `construct`); every other use
//! calls this function: its parameters are the variant's fields in order, its result the value,
//! with the variant's tag written. It runs no code that can unwind, and moves each parameter into
//! its field, so nothing is dropped.

use super::*;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The ctor of variant `variant` (a struct's is 0) of the ADT `sig` returns, at the ADT's
    /// arguments (`self.args`): the Rust ABI's parameters, one a field (lib.rs, "Rust ABI").
    fn run_ctor_fn(&mut self, variant: u32, sig: &FnSig<'_>) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let n = sig.inputs.len();
        let mut reprs = Vec::with_capacity_in(n, self.alloc);
        let mut passes = Vec::with_capacity_in(n, self.alloc);
        for &t in sig.inputs.iter() {
            let g = self.ground_in(sig.types, t).map_err(sig_err)?;
            let repr = self.repr(g).map_err(sig_err)?;
            reprs.push(repr);
            passes.push(self.rust_pass(repr)?);
        }
        let output = self.ground_in(sig.types, sig.output).map_err(sig_err)?;
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
        let mut fields = Vec::with_capacity_in(n, self.alloc);
        for k in 0..n {
            fields.push(self.take_passed(reprs[k], passes[k], &mut next)?);
        }
        match self.ret {
            // A zero-sized value: every field is zero-sized too, and nothing is written.
            Repr::None => self.ret_value(V::None)?,
            repr @ Repr::Agg(l) => {
                let (first, len, tag) = self.variant_rows(l, variant)?;
                if len as usize != n {
                    return Err(malformed("a ctor fn whose parameters are not its variant's fields"));
                }
                let at = self.temp(repr);
                self.write_tag(tag, variant, at);
                for (k, &v) in fields.iter().enumerate() {
                    let (_, off, frepr) = self.layout_fields[first as usize + k];
                    self.store(v, frepr, (at.0, at.1 + off as i32))?;
                }
                self.ret_value(V::Place(at.0, at.1))?;
            }
            _ => return Err(malformed("a ctor fn of a type held as a scalar")),
        }
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }
}

/// Builds the ctor fn of variant `variant` of the ADT `sig` returns (`sig` the ctor's: its
/// inputs the variant's fields, generic over the ADT's parameters) at the instance `subst`
/// names (lib.rs `build_ctor_fn`). Its walk is given an empty body named by `ctor` and an empty
/// check record: neither is read.
pub(crate) fn build_ctor_fn<A: Allocator + Copy>(
    ctor: ItemKey,
    variant: u32,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let generics = sig.parent_count.checked_add(sig.own_count).ok_or(BuildError::Malformed { what: "a generic count past u32" })?;
    if subst.args.len() != generics as usize {
        return Err(BuildError::Malformed { what: "a ctor fn instance whose argument count is not its type's generic count" });
    }
    let checked = super::empty_record(subst.scratch);
    let body = super::empty_view(ctor, checked.types.view());
    let mut w = Walk::new(&body, &checked, subst, adts, alloc);
    w.run_ctor_fn(variant, sig)?;
    w.into_built()
}
