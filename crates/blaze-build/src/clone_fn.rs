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

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
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
        let mut fields: Vec<PTy, A> = Vec::new_in(self.alloc);
        let mut base = 0u32;
        let mut is_union = false;
        let held = match self.ground_node(me).map_err(sig_err)? {
            PNode::FnDef(..) | PNode::FnPtr(..) => None,
            PNode::Tuple(l) | PNode::Record(_, l) => {
                fields.extend_from_slice(self.ground.view().list(l).map_err(past)?);
                Some(me)
            }
            PNode::Union(l) => {
                is_union = true;
                if l.len == 0 { None } else { Some(me) }
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
            PNode::Adt(adt_key, adt_args) => {
                let facts = self.adts.facts();
                let adt = facts.adt(adt_key).ok_or_else(|| malformed("a builtin `clone` of an ADT whose facts the world did not answer"))?;
                if adt.kind != blaze_typeck::AdtKind::Struct || adt.variants.len() != 1 {
                    return Err(malformed("a builtin `clone` of an ADT that is not a struct"));
                }
                let mut a = blaze_own::Vec::new_in(self.alloc);
                a.extend_from_slice(self.ground.view().list(adt_args).map_err(past)?);
                for f in adt.fields.iter() {
                    let g = crate::instance::ground(adt.types, f.ty, &a, self.ground, self.scratch, facts)?;
                    fields.push(g);
                }
                if fields.is_empty() { None } else { Some(me) }
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
        if let Some(slot) = self.reached.get_mut(entry.index()) {
            *slot = true;
        }
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
        let V::Scalar(at) = self.take_passed(Repr::Ptr, me_pass, &mut next)? else {
            return Err(malformed("a builtin `clone`'s `&self` not passed as a pointer"));
        };
        match held {
            Some(_) if !copy && is_union => self.clone_union(clone, at, me)?,
            Some(held) if !copy => self.clone_fields(clone, at, held, base, &fields)?,
            _ => {
                // `*self`: the bytes, whatever `Self` holds.
                let ret = self.ret;
                let v = self.load(ret, (at, 0));
                self.ret_value(v)?;
            }
        }
        self.finish_builtin()
    }

    fn finish_builtin(&self) -> Result<(), BuildError> {
        if let Some(error) = self.b.error() {
            return Err(BuildError::Lir(error.clone()));
        }
        Ok(())
    }

    /// Clones a tagged `PNode::Union` at `at` by switching on its discriminant and cloning only
    /// the active variant's member, writing its tag into the result.
    fn clone_union(&mut self, clone: ItemKey, at: Value, me: PTy) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let full = |_| malformed("the ground table is full");
        let ptr_pass = self.rust_pass(Repr::Ptr)?;
        if let PNode::Union(l) = self.ground_node(me).map_err(sig_err)?
            && l.len == 1
        {
            let only = self
                .ground
                .view()
                .list(l)
                .ok()
                .and_then(|xs| xs.first().copied())
                .ok_or(malformed("a union list past the ground table"))?;
            self.push_val(V::Scalar(at), None);
            let args = self.ground.intern_list(&[only]).map_err(full)?;
            let rpass = self.rust_pass(self.ret)?;
            self.call_instance(None, Instance { item: clone, args }, &[Repr::Ptr], &[ptr_pass], None, self.ret, rpass)?;
            let v = self.pop()?;
            return self.ret_value(v);
        }
        let Repr::Agg(lh) = self.ret else {
            return self.ret_value(V::None);
        };
        let lh_idx = usize::try_from(lh).map_err(|_| malformed("a layout index past usize"))?;
        let Shape::Enum { id, first, len } = self.layouts.get(lh_idx).ok_or(malformed("a layout index past layouts"))?.shape else {
            return Err(malformed("a union without an enum shape"));
        };
        let to = self.temp(self.ret);
        let variants = self.ls.get(id).variants;
        match variants {
            Variants::Single { index } => {
                if index < len {
                    let vrow = first
                        .checked_add(index)
                        .and_then(|k| usize::try_from(k).ok())
                        .and_then(|k| self.layout_variants.get(k).copied())
                        .ok_or(malformed("a variant past `layout_variants`"))?;
                    if vrow.1 == 1 {
                        let fidx = usize::try_from(vrow.0).map_err(|_| malformed("a field index past usize"))?;
                        let &(_, off, frepr) = self.layout_fields.get(fidx).ok_or(malformed("a field past `layout_fields`"))?;
                        let &ft = self.layout_field_tys.get(fidx).ok_or(malformed("a field past `layout_field_tys`"))?;
                        let off_i32 = i32::try_from(off).map_err(|_| malformed("a field offset past i32"))?;
                        let to_off = to.1.checked_add(off_i32).ok_or(malformed("a field offset past i32"))?;
                        let field = self.address((at, off_i32));
                        self.push_val(V::Scalar(field), None);
                        let args = self.ground.intern_list(&[ft]).map_err(full)?;
                        let rpass = self.rust_pass(frepr)?;
                        self.call_instance(None, Instance { item: clone, args }, &[Repr::Ptr], &[ptr_pass], None, frepr, rpass)?;
                        let cloned = self.pop()?;
                        self.store(cloned, frepr, (to.0, to_off))?;
                    }
                }
                self.ret_value(V::Place(to.0, to.1))
            }
            Variants::Multiple { .. } => {
                let d = self.read_discriminant(id, (at, 0))?;
                let join = self.create();
                let none = self.create();
                let cap = usize::try_from(len).map_err(|_| malformed("too many union variants"))?;
                let mut cases = blaze_own::Vec::with_capacity_in(cap, self.alloc);
                let mut arms = blaze_own::Vec::with_capacity_in(cap, self.alloc);
                for v in 0..len {
                    let block = self.create();
                    cases.push((self.discriminant_case(id, v)?, block));
                    arms.push((v, block));
                }
                self.switch(d, none, &cases);
                self.begin(none);
                self.unreachable_of_enum(id, len, None)?;
                for &(v, block) in arms.iter() {
                    self.begin(block);
                    if self.ls.variant(id, v).is_none() {
                        self.unreachable_of_enum(id, len, None)?;
                        continue;
                    }
                    let vrow = first
                        .checked_add(v)
                        .and_then(|k| usize::try_from(k).ok())
                        .and_then(|k| self.layout_variants.get(k).copied())
                        .ok_or(malformed("a variant past `layout_variants`"))?;
                    if vrow.1 == 1 {
                        let fidx = usize::try_from(vrow.0).map_err(|_| malformed("a field index past usize"))?;
                        let &(_, off, frepr) = self.layout_fields.get(fidx).ok_or(malformed("a field past `layout_fields`"))?;
                        let &ft = self.layout_field_tys.get(fidx).ok_or(malformed("a field past `layout_field_tys`"))?;
                        let off_i32 = i32::try_from(off).map_err(|_| malformed("a field offset past i32"))?;
                        let to_off = to.1.checked_add(off_i32).ok_or(malformed("a field offset past i32"))?;
                        let field = self.address((at, off_i32));
                        self.push_val(V::Scalar(field), None);
                        let args = self.ground.intern_list(&[ft]).map_err(full)?;
                        let rpass = self.rust_pass(frepr)?;
                        self.call_instance(None, Instance { item: clone, args }, &[Repr::Ptr], &[ptr_pass], None, frepr, rpass)?;
                        let cloned = self.pop()?;
                        self.store(cloned, frepr, (to.0, to_off))?;
                    }
                    self.write_tag(Some(id), v, to);
                    self.jump(join, &[]);
                }
                self.begin(join);
                self.ret_value(V::Place(to.0, to.1))
            }
        }
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
        let mut offs: Vec<(u32, Repr), A> = Vec::with_capacity_in(fields.len(), self.alloc);
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

    /// Builtin `From::from(v: Src) -> Dst` for structural error unions (`self.args = [Dst, Src]`),
    /// where `Src` is a strict member or strict sub-union of `Dst`.
    fn run_builtin_from(&mut self) -> Result<(), BuildError> {
        let sig_err = |error: TyErr| match error {
            TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
            TyErr::Malformed(what) => malformed(what),
            TyErr::Other(error) => error,
        };
        let past = |_| malformed("a list past the ground table");
        let &[dst, src] = self.args else {
            return Err(malformed("a builtin `from` whose instance is not `[Self, T]`"));
        };
        let PNode::Union(dst_l_list) = self.ground_node(dst).map_err(sig_err)? else {
            return Err(malformed("a builtin `from` whose `Self` is not a structural union"));
        };
        let mut dst_members: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
        dst_members.extend_from_slice(self.ground.view().list(dst_l_list).map_err(past)?);
        self.ret = self.repr(dst).map_err(sig_err)?;
        self.ret_pass = self.rust_pass(self.ret)?;
        let src_repr = self.repr(src).map_err(sig_err)?;
        let src_pass = self.rust_pass(src_repr)?;
        let (params, rets) = self.sig_of(&[src_pass], self.ret_pass);
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
        let v = self.take_passed(src_repr, src_pass, &mut next)?;
        let mut inject_member: Option<u32> = None;
        let mut src_members: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
        match self.ground_node(src).map_err(sig_err)? {
            PNode::Union(src_l_list) => {
                src_members.extend_from_slice(self.ground.view().list(src_l_list).map_err(past)?);
                if let [only] = src_members.as_slice() {
                    let j = dst_members
                        .iter()
                        .position(|dm| dm == only)
                        .ok_or(malformed("a builtin `from` missing a source union member"))?;
                    inject_member = Some(u32::try_from(j).map_err(|_| malformed("a union variant index past `u32`"))?);
                }
            }
            _ => {
                let j = dst_members
                    .iter()
                    .position(|&dm| dm == src)
                    .ok_or(malformed("a builtin `from` missing a source member"))?;
                inject_member = Some(u32::try_from(j).map_err(|_| malformed("a union variant index past `u32`"))?);
            }
        }
        if let Some(variant) = inject_member {
            if dst_members.len() == 1 {
                if variant != 0 {
                    return Err(malformed("a single-member union injection at a non-zero variant"));
                }
                self.ret_value(v)?;
            } else {
                match self.ret {
                    Repr::None => self.ret_value(V::None)?,
                    Repr::Agg(dst_l) => {
                        let (first, len, tag) = self.variant_rows(dst_l, variant)?;
                        if len != 1 {
                            return Err(malformed("a union variant without exactly one payload field"));
                        }
                        let first_idx = usize::try_from(first).map_err(|_| malformed("a union field index past `usize`"))?;
                        let &(_, field_off, field_repr) = self
                            .layout_fields
                            .get(first_idx)
                            .ok_or(malformed("a union variant's field past `layout_fields`"))?;
                        let field_off_i32 = i32::try_from(field_off).map_err(|_| malformed("a union field offset past `i32`"))?;
                        let dst_at = self.temp(self.ret);
                        self.write_tag(tag, variant, dst_at);
                        self.store(v, field_repr, (dst_at.0, dst_at.1 + field_off_i32))?;
                        self.ret_value(V::Place(dst_at.0, dst_at.1))?;
                    }
                    _ => return Err(malformed("a multi-member union not held as an aggregate")),
                }
            }
        } else {
            match self.ret {
                Repr::None => self.ret_value(V::None)?,
                Repr::Agg(dst_l) => {
                    let Repr::Agg(src_l) = src_repr else {
                        return Err(malformed("a multi-member source union not held as an aggregate"));
                    };
                    let src_at = self.place(v, src_repr)?;
                    let dst_at = self.temp(self.ret);
                    self.remap_union(None, src_l, src_at, &src_members, dst_l, dst_at, &dst_members)?;
                    self.ret_value(V::Place(dst_at.0, dst_at.1))?;
                }
                _ => return Err(malformed("a multi-member union not held as an aggregate")),
            }
        }
        self.finish_builtin()
    }
}

/// Builds the builtin `clone` of `Self` (`subst.args`, one type: a tuple, a closure, a fn item
/// or a fn pointer) at the trait method `clone` (lib.rs `build_builtin_clone`); `copy` is the
/// checker's answer to `Self: Copy`. Its walk is given an empty body named by `clone` and an
/// empty check record: neither is read.
pub(crate) fn build_builtin_clone<A: Allocator + Copy>(
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

/// Builds the builtin `From::from` shim for structural error unions (`subst.args = [Dst, Src]`).
pub(crate) fn build_builtin_from<A: Allocator + Copy>(
    from: ItemKey,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let checked = super::empty_record(subst.scratch);
    let body = super::empty_view(from, checked.types.view());
    let mut w = Walk::new(&body, &checked, subst, adts, alloc);
    w.run_builtin_from()?;
    w.into_built()
}
