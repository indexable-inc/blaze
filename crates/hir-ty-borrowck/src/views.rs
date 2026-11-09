//! `#![blaze::views]`: a method's receiver names the fields of `Self` it borrows, `&self.{a, b}`
//! or `&mut self.{a}`, and a call of it borrows only those fields of the receiver place, so two
//! calls whose views are disjoint do not conflict (docs/v2/plan/design/edition.md, change 3).
//!
//! - **Call sites.** [`call_views`] finds, in a body, each borrow made to be the receiver of a
//!   call of a method with a view. `Loans::gather` (`loans.rs`) makes that borrow a loan of
//!   each field of the view in place of one loan of the whole place, and the borrow's access is
//!   checked field by field (`check.rs`, `consume_rvalue`). That is the loan model rustc has
//!   for `(&mut x.a, &x.b)`; there is no second conflict check.
//! - **Bodies.** [`check_method`]: inside a method with a written view, `self` is used only
//!   through the fields of the view. A use of another field, or of `self` whole, is an error
//!   naming the field and the view. The one whole use allowed is as the receiver of a method
//!   whose view is within this one, so a call of it borrows no more.
//! - **Inference.** A method of an inherent impl written with no visibility and a `&self` or
//!   `&mut self` receiver, in a crate with the attribute, has the view its body uses
//!   ([`self_view`]): the fields it touches through `self`, and those of the methods it calls
//!   on `self`, transitively, over at most [`BOUND`] methods. Recursion, a bound reached, or
//!   `self` used whole is the whole receiver, with the reason ([`Whole`]). A `pub` method,
//!   `pub(crate)` included, has exactly the view it writes.
//! - **Traits.** A view on a method of a trait or of a trait impl is refused: callers through a
//!   bound or `dyn` cannot see the impl's view.
//!
//! The view is only the borrow checker's. Nothing that lowers a signature reads it: the
//! method's type, layout, ABI and MIR are what they are without it, and the method still gets
//! the whole pointer, so its code identity (`hir-ty-identity`, from its item key and MIR) is
//! unchanged. Whether a receiver has a view is a bit of its signature
//! ([`hir_def::signatures::FnFlags::SELF_VIEW`]), in the signature's hash and so in the
//! method's own borrow check key and in every caller's. The fields are read through
//! [`self_view`] and [`hir_def::signatures::written_self_view`], tracked queries the item
//! store has no rule for, so a borrow check that read a view is never stored
//! (`blaze_item_hash::reads`: a read no rule names is refused).

use base_db::{Crate, Db};
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, StructId,
    VariantId,
    item_tree::FieldsShape,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature, VariantFields, written_self_view},
    visibility::{Visibility, VisibilityExplicitness},
};
use hir_ty::{
    InferBodyId,
    db::HirDatabase,
    mir::{
        AsmOperand, BasicBlockId, FieldIndex, LocalId, MirBody, MirSpan, Operand, OperandKind,
        ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::TyKind,
};
use rustc_type_ir::inherent::IntoKind;

use crate::{
    BorrowckDiagnostic,
    cfg::Location,
    mem::{self, FMap, FVec, Frame},
};

/// The code a view's errors are reported with.
pub(crate) const CODE: &str = "VIEW";

/// The most methods [`self_view`] reads to infer one view: past it the receiver is whole.
pub(crate) const BOUND: usize = 64;

/// What a method borrows of its receiver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SelfView {
    /// The whole receiver, as in Rust, and why.
    Whole(Whole),
    /// Only these fields.
    Fields(View),
    /// The view written is refused: the error, naming the method, what is wrong and the rule.
    Refused(String),
}

/// A view of fields of `Self`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct View {
    pub strukt: StructId,
    /// The fields, ascending, each once.
    pub fields: Box<[FieldIndex]>,
    /// `&mut self.{..}`.
    pub mutable: bool,
    /// Inferred from the body, not written.
    pub inferred: bool,
}

/// Why a method borrows its whole receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Whole {
    /// It has no `self` parameter.
    NotAMethod,
    /// Its crate does not say `#![blaze::views]`, and its receiver has no view written.
    NotGated,
    /// It is written with a visibility: it has the view it writes, and it writes none.
    Visible,
    /// It is a method of a trait or of a trait impl.
    Trait,
    /// `Self` is not a struct with named fields.
    NotAStruct,
    /// Its receiver is not a reference, `self` or `self: Box<Self>`.
    NotByReference,
    /// It, or a method it calls on `self`, uses `self` whole: passes it on, copies it, or calls
    /// a method on it with no view.
    UsesWhole,
    /// It calls itself on `self`, through the methods it calls or directly.
    Recursive,
    /// Inference read [`BOUND`] methods and did not finish.
    Bound,
    /// Its MIR, or that of a method it calls, is not built.
    NoMir,
}

/// The view of `func`'s receiver: written, inferred or the whole receiver.
#[blaze_query::tracked(returns(ref))]
pub fn self_view(db: &Db, func: FunctionId) -> SelfView {
    let signature = FunctionSignature::of(db, func);
    if !signature.has_self_param() {
        return SelfView::Whole(Whole::NotAMethod);
    }
    if signature.has_self_view() {
        return written(db, func);
    }
    match inferred(db, func) {
        Ok(view) => SelfView::Fields(view),
        Err(why) => SelfView::Whole(why),
    }
}

/// The view `func` writes, checked: in a crate with the attribute, on a method of an inherent
/// impl of a struct with named fields, each name a field, once.
fn written(db: &Db, func: FunctionId) -> SelfView {
    let Some(written) = written_self_view(db, func) else {
        return SelfView::Whole(Whole::NotAMethod);
    };
    let name = FunctionSignature::of(db, func).name.as_str();
    if !crate_def_map(db, func.krate(db)).views() {
        return SelfView::Refused(format!(
            "method `{name}` writes a view of `self`, `&self.{{..}}`, in a crate without \
             `#![blaze::views]`: add `#![blaze::views]` to the crate root to borrow only those \
             fields (rule views-gated)"
        ));
    }
    let strukt = match self_struct(db, func) {
        Ok(it) => it,
        Err(Whole::Trait) => {
            return SelfView::Refused(format!(
                "method `{name}` of a trait or a trait impl writes a view of `self`: views on \
                 trait methods are refused, since a caller through a bound or `dyn` cannot see \
                 which fields an impl borrows (rule views-not-on-traits)"
            ));
        }
        Err(_) => {
            return SelfView::Refused(format!(
                "method `{name}` writes a view of `self`, but `Self` is not a struct with named \
                 fields: a view names fields of a struct (rule views-of-structs)"
            ));
        }
    };
    let strukt_name = StructSignature::of(db, strukt).name.as_str();
    let variant = VariantFields::of(db, VariantId::StructId(strukt));
    let mut fields: Vec<FieldIndex> = Vec::with_capacity(written.fields.len()); // alloc: the memo's fields, once per method with a view
    for field in written.fields.iter() {
        let Some(id) = variant.field(field) else {
            return SelfView::Refused(format!(
                "`{}` is not a field of `{strukt_name}`, in the view of `self` of method \
                 `{name}` (rule views-name-fields)",
                field.as_str()
            ));
        };
        let index = FieldIndex::from(id);
        if fields.contains(&index) {
            return SelfView::Refused(format!(
                "field `{}` is named twice in the view of `self` of method `{name}` (rule \
                 views-name-fields)",
                field.as_str()
            ));
        }
        fields.push(index);
    }
    fields.sort_unstable();
    SelfView::Fields(View {
        strukt,
        fields: fields.into_boxed_slice(),
        mutable: written.mutable,
        inferred: false,
    })
}

/// The struct `Self` is for a method of an inherent impl; `Trait` for a method of a trait or a
/// trait impl, `NotAStruct` for one of another type.
fn self_struct(db: &Db, func: FunctionId) -> Result<StructId, Whole> {
    let ItemContainerId::ImplId(impl_) = func.lookup(db).container else {
        return Err(match func.lookup(db).container {
            ItemContainerId::TraitId(_) => Whole::Trait,
            _ => Whole::NotAMethod,
        });
    };
    if db.impl_trait(impl_).is_some() {
        return Err(Whole::Trait);
    }
    let TyKind::Adt(adt, _) = db.impl_self_ty(impl_).skip_binder().kind() else {
        return Err(Whole::NotAStruct);
    };
    let hir_def::AdtId::StructId(strukt) = adt.def_id() else { return Err(Whole::NotAStruct) };
    if VariantFields::of(db, VariantId::StructId(strukt)).shape != FieldsShape::Record {
        return Err(Whole::NotAStruct);
    }
    Ok(strukt)
}

/// Whether `func` may have its view inferred, and of which struct: a method of an inherent
/// impl of a struct with named fields, written with no visibility, in a crate with the
/// attribute. Its receiver being a reference is [`Uses::receiver`]'s.
fn inferable(db: &Db, func: FunctionId) -> Result<StructId, Whole> {
    if !crate_def_map(db, func.krate(db)).views() {
        return Err(Whole::NotGated);
    }
    let strukt = self_struct(db, func)?;
    if !matches!(
        AssocItemId::FunctionId(func).assoc_visibility(db),
        Visibility::Module(_, VisibilityExplicitness::Implicit)
    ) {
        return Err(Whole::Visible);
    }
    Ok(strukt)
}

/// The view of a method with none written, from its body and those of the methods it calls on
/// `self`: a walk over at most [`BOUND`] of them, depth first, which ends in a view or in why
/// the receiver is whole.
fn inferred(db: &Db, root: FunctionId) -> Result<View, Whole> {
    let strukt = inferable(db, root)?;
    let own = uses(db, root);
    if let Some(why) = own.whole {
        return Err(why);
    }
    let mutable = own.receiver.ok_or(Whole::NotByReference)?;
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        // Each method seen, and whether its walk is done: one not done is on the stack.
        let mut seen: FMap<'_, FunctionId, bool> = mem::map(frame);
        let mut stack: FVec<'_, (FunctionId, usize)> = Vec::new_in(frame);
        let mut fields: FVec<'_, FieldIndex> = Vec::new_in(frame);
        enter(db, root, &mut fields, &mut stack, &mut seen)?;
        while let Some(&(func, next)) = stack.last() {
            let calls = &uses(db, func).calls;
            let Some(&callee) = calls.get(next) else {
                seen.insert(func, true);
                stack.pop();
                continue;
            };
            if let Some(top) = stack.last_mut() {
                top.1 += 1;
            }
            // A method that writes its view adds those fields and is not walked into.
            if FunctionSignature::of(db, callee).has_self_view() {
                match self_view(db, callee) {
                    SelfView::Fields(view) if view.strukt == strukt => {
                        fields.extend_from_slice(&view.fields);
                    }
                    _ => return Err(Whole::UsesWhole),
                }
                continue;
            }
            match seen.get(&callee) {
                Some(false) => return Err(Whole::Recursive),
                Some(true) => continue,
                None => {}
            }
            match inferable(db, callee) {
                Ok(it) if it == strukt => {}
                _ => return Err(Whole::UsesWhole),
            }
            enter(db, callee, &mut fields, &mut stack, &mut seen)?;
        }
        fields.sort_unstable();
        fields.dedup();
        Ok(View {
            strukt,
            // alloc: the memo's fields, once per method whose view is inferred
            fields: fields.iter().copied().collect(),
            mutable,
            inferred: true,
        })
    })
}

/// Starts the walk of [`inferred`] into `func`: its own fields, unless it uses `self` whole or
/// the walk has read [`BOUND`] methods.
fn enter<'f>(
    db: &Db,
    func: FunctionId,
    fields: &mut FVec<'f, FieldIndex>,
    stack: &mut FVec<'f, (FunctionId, usize)>,
    seen: &mut FMap<'f, FunctionId, bool>,
) -> Result<(), Whole> {
    if seen.len() >= BOUND {
        return Err(Whole::Bound);
    }
    let own = uses(db, func);
    if let Some(why) = own.whole {
        return Err(why);
    }
    if own.receiver.is_none() {
        return Err(Whole::NotByReference);
    }
    fields.extend_from_slice(&own.fields);
    seen.insert(func, false);
    stack.push((func, 0));
    Ok(())
}

/// What one method's own body does with `self`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Uses {
    /// `Some(mutable)` for a receiver that is a reference to `Self`, `&self` or `&mut self`.
    pub(crate) receiver: Option<bool>,
    /// The fields of `Self` the body uses through `self`, ascending, each once.
    pub(crate) fields: Box<[FieldIndex]>,
    /// The methods it calls with `self` as the receiver, in the order of the body, each once.
    pub(crate) calls: Box<[FunctionId]>,
    /// Why it uses `self` whole otherwise, if it does.
    pub(crate) whole: Option<Whole>,
}

/// [`Uses`] of `func`, from its MIR.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn uses(db: &Db, func: FunctionId) -> Uses {
    let none = |why| Uses {
        receiver: None,
        fields: Box::default(),
        calls: Box::default(),
        whole: Some(why),
    };
    let Ok(body) = db.mir_body(InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func))) else {
        return none(Whole::NoMir);
    };
    let Some(&receiver) = body.param_locals.first() else { return none(Whole::NotAMethod) };
    let mutable = match body.locals[receiver].ty.as_ref().kind() {
        TyKind::Ref(_, _, mutability) => Some(mutability == rustc_ast_ir::Mutability::Mut),
        _ => None,
    };
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let mut fields: FVec<'_, FieldIndex> = Vec::new_in(frame);
        let mut calls: FVec<'_, FunctionId> = Vec::new_in(frame);
        let mut whole = None;
        self_uses(db, body, receiver, frame, |it| match it {
            SelfUse::Field(field, _) => fields.push(field),
            SelfUse::Receiver(callee, _) => {
                if !calls.contains(&callee) {
                    calls.push(callee);
                }
            }
            SelfUse::Whole(_) => whole = Some(Whole::UsesWhole),
        });
        fields.sort_unstable();
        fields.dedup();
        Uses {
            receiver: mutable,
            // alloc: the memo's fields and calls, once per method inferred through
            fields: fields.iter().copied().collect(),
            calls: calls.iter().copied().collect(),
            whole,
        }
    })
}

/// One use of the receiver `self` in a body.
#[derive(Clone, Copy, Debug)]
enum SelfUse {
    /// A place in this field of `*self`.
    Field(FieldIndex, MirSpan),
    /// `self` given whole as the receiver of a call of a method, directly or reborrowed.
    Receiver(FunctionId, MirSpan),
    /// `self` or `*self` used whole otherwise.
    Whole(MirSpan),
}

/// Calls `found` with each use of `receiver`, a reference to `Self`, in `body`, in the order of
/// its blocks. A read that only feeds the borrow checker (`FakeRead`) and storage markers use
/// nothing.
fn self_uses(
    db: &Db,
    body: &MirBody<'_>,
    receiver: LocalId,
    frame: &Frame,
    mut found: impl FnMut(SelfUse),
) {
    // The temporaries assigned once, by `&*self` or `&mut *self`, by where.
    let mut assigned: FMap<'_, LocalId, Option<Location>> = mem::map(frame);
    for (block, data) in body.basic_blocks.iter() {
        for (statement, it) in data.statements.iter().enumerate() {
            let StatementKind::Assign(dest, rvalue) = &it.kind else { continue };
            if !dest.projection.as_slice().is_empty() {
                continue;
            }
            let reborrow = matches!(rvalue, Rvalue::Ref(_, place)
                if place.local == receiver
                    && matches!(place.projection.as_slice(), [ProjectionElem::Deref]));
            let at = reborrow.then_some(Location { block, statement });
            assigned
                .entry(dest.local)
                .and_modify(|it| *it = None)
                .or_insert(at);
        }
    }
    // The reborrows that are a method call's receiver, and the calls given `self` itself.
    let mut receivers: FMap<'_, Location, FunctionId> = mem::map(frame);
    let mut direct: FMap<'_, BasicBlockId, FunctionId> = mem::map(frame);
    for (block, data) in body.basic_blocks.iter() {
        let Some(terminator) = &data.terminator else { continue };
        let TerminatorKind::Call { func, args, .. } = &terminator.kind else { continue };
        let Some((callee, _)) = func.fn_item() else { continue };
        if !FunctionSignature::of(db, callee).has_self_param() {
            continue;
        }
        let Some(local) = args.first().and_then(whole_local) else { continue };
        if local == receiver {
            direct.insert(block, callee);
        } else if let Some(&Some(at)) = assigned.get(&local) {
            receivers.insert(at, callee);
        }
    }
    let mut visit = |place: &StoredPlace, span: MirSpan, receiver_of: Option<FunctionId>| {
        if place.local != receiver {
            return;
        }
        match place.projection.as_slice() {
            [ProjectionElem::Deref, ProjectionElem::Field(field), ..] => {
                found(SelfUse::Field(*field, span));
            }
            _ => match receiver_of {
                Some(callee) => found(SelfUse::Receiver(callee, span)),
                None => found(SelfUse::Whole(span)),
            },
        }
    };
    for (block, data) in body.basic_blocks.iter() {
        for (statement, it) in data.statements.iter().enumerate() {
            let span = it.span;
            match &it.kind {
                StatementKind::Assign(dest, rvalue) => {
                    visit(dest, span, None);
                    if let Rvalue::Ref(_, place)
                    | Rvalue::Len(place)
                    | Rvalue::Discriminant(place)
                    | Rvalue::CopyForDeref(place) = rvalue
                    {
                        let at = Location { block, statement };
                        visit(place, span, receivers.get(&at).copied());
                    }
                    rvalue.for_each_operand(&mut |operand| {
                        if let Some(place) = operand_place(operand) {
                            visit(place, operand.span().unwrap_or(span), None);
                        }
                    });
                }
                StatementKind::InlineAsm(asm) => {
                    for operand in asm.operands.iter() {
                        if let AsmOperand::Reg { output: Some(place), .. } = operand {
                            visit(place, span, None);
                        }
                    }
                    it.kind.for_each_operand(&mut |operand| {
                        if let Some(place) = operand_place(operand) {
                            visit(place, span, None);
                        }
                    });
                }
                StatementKind::FakeRead(_)
                | StatementKind::Deinit(_)
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let Some(terminator) = &data.terminator else { continue };
        let span = terminator.span;
        match &terminator.kind {
            TerminatorKind::Call { func, destination, args, .. } => {
                visit(destination, span, None);
                let callee = direct.get(&block).copied();
                for (at, operand) in args.iter().enumerate() {
                    if let Some(place) = operand_place(operand) {
                        let receiver_of = if at == 0 { callee } else { None };
                        visit(place, operand.span().unwrap_or(span), receiver_of);
                    }
                }
                if let Some(place) = operand_place(func) {
                    visit(place, span, None);
                }
                continue;
            }
            TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                visit(place, span, None);
            }
            TerminatorKind::Yield { resume_arg, .. } => visit(resume_arg, span, None),
            _ => {}
        }
        terminator.kind.for_each_operand(&mut |operand| {
            if let Some(place) = operand_place(operand) {
                visit(place, operand.span().unwrap_or(span), None);
            }
        });
    }
}

/// The place an operand reads, if it reads one.
fn operand_place(operand: &Operand) -> Option<&StoredPlace> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
        _ => None,
    }
}

/// The local an operand gives whole, if it gives one.
fn whole_local(operand: &Operand) -> Option<LocalId> {
    let place = operand_place(operand)?;
    place.projection.as_slice().is_empty().then_some(place.local)
}

/// The fields of the view of the method each borrow in `body` is made for, by where the
/// borrow is: the borrows that are the receiver of a call of a method with a view. Asked of a
/// callee only when it writes a view or may have one inferred (a method of `krate`, when
/// `krate` says `#![blaze::views]`), so a body of a crate without views that calls no method
/// with one reads no view.
pub(crate) fn call_views<'f, 'db>(
    db: &'db Db,
    krate: Crate,
    body: &MirBody<'_>,
    frame: &'f Frame,
) -> FMap<'f, Location, &'db [FieldIndex]> {
    let mut found: FMap<'f, Location, &'db [FieldIndex]> = mem::map(frame);
    let gated = crate_def_map(db, krate).views();
    // The temporaries assigned once, by a borrow, by where: a receiver as lowering makes it.
    let mut borrows: Option<FMap<'_, LocalId, Option<Location>>> = None;
    for (_, data) in body.basic_blocks.iter() {
        let Some(terminator) = &data.terminator else { continue };
        let TerminatorKind::Call { func, args, .. } = &terminator.kind else { continue };
        let Some((callee, _)) = func.fn_item() else { continue };
        let signature = FunctionSignature::of(db, callee);
        if !signature.has_self_param()
            || !(signature.has_self_view() || (gated && callee.krate(db) == krate))
        {
            continue;
        }
        let Some(local) = args.first().and_then(whole_local) else { continue };
        let borrows = borrows.get_or_insert_with(|| {
            let mut map: FMap<'_, LocalId, Option<Location>> = mem::map(frame);
            for (block, data) in body.basic_blocks.iter() {
                for (statement, it) in data.statements.iter().enumerate() {
                    let StatementKind::Assign(dest, rvalue) = &it.kind else { continue };
                    if !dest.projection.as_slice().is_empty() {
                        continue;
                    }
                    let at = matches!(rvalue, Rvalue::Ref(..))
                        .then_some(Location { block, statement });
                    map.entry(dest.local).and_modify(|it| *it = None).or_insert(at);
                }
            }
            map
        });
        let Some(&Some(at)) = borrows.get(&local) else { continue };
        if let SelfView::Fields(view) = self_view(db, callee) {
            found.insert(at, &view.fields[..]);
        }
    }
    found
}

/// The errors of a method's receiver and body under `#![blaze::views]`: a written view that is
/// refused, at the receiver; in the body of one with a written view, each use of `self`
/// outside it. Nothing for a method with no view written.
pub(crate) fn check_method(
    db: &Db,
    func: FunctionId,
    body: Option<&MirBody<'_>>,
    out: &mut Vec<BorrowckDiagnostic>,
) {
    if !FunctionSignature::of(db, func).has_self_view() {
        return;
    }
    let view = match self_view(db, func) {
        SelfView::Fields(view) => view,
        SelfView::Refused(message) => {
            // clone: the memo's message into the owned diagnostic, on the error path
            out.push(BorrowckDiagnostic::new(CODE, MirSpan::SelfParam, message.clone()));
            return;
        }
        SelfView::Whole(_) => return,
    };
    let Some(body) = body else { return };
    let Some(&receiver) = body.param_locals.first() else { return };
    let name = FunctionSignature::of(db, func).name.as_str();
    let variant = VariantFields::of(db, VariantId::StructId(view.strukt));
    let field_name = |field: FieldIndex| {
        let id = field.to_local_field_id();
        variant.fields().iter().find(|&(it, _)| it == id).map_or("?", |(_, it)| it.name.as_str())
    };
    let shown = || {
        let mut text = String::from("&"); // alloc: error path, the view as written
        if view.mutable {
            text.push_str("mut ");
        }
        text.push_str("self.{");
        for (at, &field) in view.fields.iter().enumerate() {
            if at > 0 {
                text.push_str(", ");
            }
            text.push_str(field_name(field));
        }
        text.push('}');
        text
    };
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let mut reported: FVec<'_, MirSpan> = Vec::new_in(frame);
        self_uses(db, body, receiver, frame, |it| {
            let (span, message) = match it {
                SelfUse::Field(field, span) => {
                    if view.fields.contains(&field) {
                        return;
                    }
                    let message = format!(
                        "method `{name}` uses field `{}` of `self`, which is not in its view \
                         `{}`: a method with a view uses only those fields of `self` (rule \
                         views-body)",
                        field_name(field),
                        shown()
                    );
                    (span, message)
                }
                SelfUse::Receiver(callee, span) => {
                    let within = callee == func
                        || matches!(self_view(db, callee), SelfView::Fields(it)
                            if it.strukt == view.strukt
                                && (view.mutable || !it.mutable)
                                && it.fields.iter().all(|field| view.fields.contains(field)));
                    if within {
                        return;
                    }
                    let callee_name = FunctionSignature::of(db, callee).name.as_str();
                    let message = format!(
                        "method `{name}` passes `self` to `{callee_name}`, which borrows more \
                         of `self` than the view `{}` of `{name}`: a method with a view calls \
                         on `self` only methods whose views are within it (rule views-body)",
                        shown()
                    );
                    (span, message)
                }
                SelfUse::Whole(span) => {
                    let message = format!(
                        "method `{name}` uses `self` whole, but its view is `{}`: a method \
                         with a view uses only those fields of `self` (rule views-body)",
                        shown()
                    );
                    (span, message)
                }
            };
            if reported.contains(&span) {
                return;
            }
            reported.push(span);
            out.push(BorrowckDiagnostic::new(CODE, span, message));
        });
    });
}
