//! Panics that unwind: what a panic unwinds with, the cleanup blocks it runs on its way out
//! of each frame, the `catch_unwind` that stops it, and the functions of the panic runtime
//! the evaluator is itself (`__rust_start_panic`, `__rust_panic_cleanup`, and core's
//! `panic_impl`, which goes to the crate graph's `#[panic_handler]` as rustc links it).
//!
//! A panic starts as an `Err(MirEvalError::Panic)` out of a statement, a terminator or a
//! shim ([`Evaluator::raise`]). Unless the frame aborts (it can't unwind, or it is cleaning
//! up after another panic), the frame goes on at the cleanup block the statement or
//! terminator names, and a frame with none, or one whose cleanup ends in `UnwindResume`, is
//! left: its caller unwinds from the call it made. A `catch_unwind`'s `try` frame stops it.

use either::Either;
use hir_def::{
    FunctionId, ItemContainerId, Lookup, ModuleDefId, attrs::AttrFlags, nameres::crate_def_map,
};
use hir_expand::name::Name;
use intern::Symbol;
use la_arena::ArenaMap;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{IntoKind, Ty as _};

use base_db::{Db, Crate};

use super::{
    Address, BasicBlockId, Evaluator, Interval, IntervalAndTy, Locals, MirBody, MirEvalError,
    MirSpan, Origin, OwnedBytes, Result, StackFrame, Then, from_bytes, machine::Machine,
    not_supported,
    word_bytes,
};
use crate::{
    InferBodyId,
    db::{HirDatabase, InternedClosureId},
    mir::optimize::fn_can_unwind,
    next_solver::{GenericArgs, Region, Ty, TyKind},
};

/// What a panic unwinds with.
pub(super) enum Payload {
    /// A panic the evaluator raised itself, or a const panic's message: a failed `Assert`,
    /// an overflow, `panic_display`. `catch_unwind` gives it as a `Box<&'static str>`.
    Message(String),
    /// What `__rust_start_panic` got from `PanicPayload::take_box`: a
    /// `*mut (dyn Any + Send)`, its data pointer and its vtable id.
    Boxed { data: Address, vtable: usize },
}

pub(super) struct Unwinding<'db> {
    payload: Payload,
    /// The error the panic is reported as if nothing catches it, and the frames it unwound
    /// out of in the run of `interpret_mir` it is in. Both are taken when it leaves that
    /// run, whose caller gets the error and puts its own frames on it.
    error: Option<MirEvalError<'db>>,
    trace: Vec<(Either<FunctionId, InternedClosureId<'db>>, MirSpan, InferBodyId<'db>)>,
}

/// A `catch_unwind`'s call of `try_fn(data)`: a panic unwinding out of it calls
/// `catch_fn(data, exception)`, and the intrinsic returns 1.
pub(super) struct Catch {
    data: Address,
    catch_fn: usize,
    destination: Interval,
}

/// The program aborts, as rustc's `UnwindTerminate` does: nothing catches it.
pub(super) fn abort<'db>(reason: &str) -> MirEvalError<'db> {
    // alloc: error path
    MirEvalError::Aborted(reason.to_owned())
}

/// Whether `error` is a panic, which unwinds; every other error stops the evaluation.
fn is_panic(error: &MirEvalError<'_>) -> bool {
    match error {
        MirEvalError::Panic(_) => true,
        MirEvalError::InFunction(inner, _) => is_panic(inner),
        _ => false,
    }
}

fn message(error: &MirEvalError<'_>) -> String {
    match error {
        // clone: error path
        MirEvalError::Panic(message) => message.clone(),
        MirEvalError::InFunction(inner, _) => message(inner),
        _ => String::new(), // alloc: empty, allocates nothing
    }
}

/// The `#[panic_handler]` of the crate graph `krate` is the root of: the function core's
/// `panic_impl` goes to. rustc allows one in a graph.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn panic_handler(db: &Db, krate: Crate) -> Option<FunctionId> {
    let def_map = crate_def_map(db, krate);
    for (_, module) in def_map.modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::FunctionId(func) = def
                && AttrFlags::query(db, func.into()).contains(AttrFlags::PANIC_HANDLER)
            {
                return Some(func);
            }
        }
    }
    def_map
        .root_module_id()
        .extern_prelude(db)
        .map(|(_, (module, _))| module.krate(db))
        .filter(|&dep| dep != krate)
        .find_map(|dep| panic_handler(db, dep))
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// The `#[panic_handler]` a panic at run time goes to; none in a constant's evaluation,
    /// where core's `panic_fmt` goes to `const_panic_fmt` as in rustc's CTFE.
    pub(super) fn panic_handler(&self) -> Option<FunctionId> {
        if self.limits_frames {
            return None;
        }
        *self.panic_handler.get_or_init(|| panic_handler(self.db, self.crate_id))
    }

    /// The frames of [`Self::code_stack`] a panic's error names, innermost first.
    fn trace(&self) -> Vec<(Either<FunctionId, InternedClosureId<'db>>, MirSpan, InferBodyId<'db>)> {
        self.code_stack
            .iter()
            .rev()
            .filter_map(|frame| {
                let func = frame.locals.body.owner.as_function()?;
                Some((Either::Left(func), frame.span.0, frame.span.1))
            })
            .collect() // alloc: a panic's backtrace, error path
    }

    /// `error` came out of `frame` at `block`, of its statement `statement` or (`None`) its
    /// terminator: a panic unwinds, anything else stops the evaluation. Where unwinding goes
    /// on is pushed on [`Self::code_stack`] with `block` set to it.
    pub(super) fn raise(
        &mut self,
        frame: StackFrame<'db>,
        block: &mut BasicBlockId,
        error: MirEvalError<'db>,
        statement: Option<usize>,
    ) -> Result<'db, ()> {
        if !is_panic(&error) {
            return Err(error);
        }
        let body = frame.locals.body;
        if body.basic_blocks[*block].is_cleanup {
            return Err(abort("panic in a destructor during cleanup"));
        }
        let trace = self.trace();
        match &mut self.unwinding {
            Some(unwinding) if unwinding.error.is_none() => {
                unwinding.error = Some(error);
                unwinding.trace = trace;
            }
            Some(_) => return Err(abort("panic in a destructor during cleanup")),
            None => {
                let payload = Payload::Message(message(&error));
                self.unwinding = Some(Unwinding { payload, error: Some(error), trace });
            }
        }
        let cleanup = match statement {
            Some(index) => body
                .panic_unwinds
                .iter()
                .find(|&&(at, i, _)| at == *block && i == index)
                .map(|&(_, _, cleanup)| cleanup),
            None => body.basic_blocks[*block].terminator.as_ref().and_then(|it| it.kind.unwind()),
        };
        self.unwind_to(frame, block, cleanup)
    }

    /// Unwinding goes on in `frame` at `cleanup`, or leaves it when there is none.
    fn unwind_to(
        &mut self,
        frame: StackFrame<'db>,
        block: &mut BasicBlockId,
        cleanup: Option<BasicBlockId>,
    ) -> Result<'db, ()> {
        match cleanup {
            Some(cleanup) => {
                *block = cleanup;
                self.code_stack.push(frame);
                Ok(())
            }
            None => self.unwind_out(frame, block),
        }
    }

    /// Unwinding leaves `frame`: it goes on in the caller from the call it made, stops at a
    /// `catch_unwind`, or leaves the run of `interpret_mir` as its error.
    pub(super) fn unwind_out(
        &mut self,
        frame: StackFrame<'db>,
        block: &mut BasicBlockId,
    ) -> Result<'db, ()> {
        if let Some(func) = frame.callee
            && !fn_can_unwind(self.db, func)
        {
            return Err(abort("panic in a function that cannot unwind"));
        }
        self.machine.ret()?;
        let StackFrame { locals, then, drops, catch, prev_stack_ptr, .. } = frame;
        self.unused_locals_store.borrow_mut().entry(locals.body.owner).or_default().push(locals);
        if then == Then::Nested {
            let Some(unwinding) = &mut self.unwinding else {
                not_supported!("unwinding with no panic");
            };
            // alloc: error path
            let error = unwinding.error.take().unwrap_or(MirEvalError::Panic(String::new()));
            self.escaped = true;
            // alloc: error path
            return Err(MirEvalError::InFunction(Box::new(error), std::mem::take(&mut unwinding.trace)));
        }
        if self.limits_frames {
            self.stack_depth_limit += 1;
        }
        self.free_stack(prev_stack_ptr)?;
        match then {
            Then::Thread => Err(abort("panic in a thread's start function, which cannot unwind")),
            Then::Resume => Err(abort("panic in a destructor during cleanup")),
            Then::Block(_) | Then::Diverges => {
                if let Some(catch) = catch {
                    return self.caught(catch, then, block);
                }
                if self.finish_drops(drops, Then::Resume, block)? {
                    return Ok(());
                }
                let Some(caller) = self.code_stack.pop() else {
                    not_supported!("unwinding with no caller");
                };
                self.unwind_from(caller, block)
            }
            Then::Nested => unreachable!("handled above"),
        }
    }

    /// Unwinding goes on in `caller` from the call its block `at` made.
    pub(super) fn unwind_from(
        &mut self,
        caller: StackFrame<'db>,
        block: &mut BasicBlockId,
    ) -> Result<'db, ()> {
        let at = &caller.locals.body.basic_blocks[caller.at];
        if at.is_cleanup {
            return Err(abort("panic in a destructor during cleanup"));
        }
        let cleanup = at.terminator.as_ref().and_then(|it| it.kind.unwind());
        self.running = Some((caller.callee, caller.span));
        self.unwind_to(caller, block, cleanup)
    }

    /// Why an `Abort` terminator in `body` aborts.
    pub(super) fn unwind_reason(&self, body: &MirBody<'db>) -> &'static str {
        match self.unwinding.is_some() && body.owner.as_function().is_some() {
            true => "panic in a function that cannot unwind",
            false => "the program called abort",
        }
    }

    /// The panic unwinding out of a `catch_unwind`'s `try` frame stops there: the intrinsic
    /// returns 1, after `catch_fn(data, exception)` runs as a frame going on at `then`.
    fn caught(&mut self, catch: Catch, then: Then, block: &mut BasicBlockId) -> Result<'db, ()> {
        let Some(unwinding) = self.unwinding.take() else {
            not_supported!("catching with no panic");
        };
        let exception = self.exception(unwinding.payload)?;
        catch.destination.write_from_bytes(self, &1i32.to_le_bytes())?;
        let Some(caller) = self.code_stack.last() else {
            not_supported!("a catch_unwind with no caller");
        };
        let (body, span) = (caller.locals.body, caller.span.0);
        let unit = Interval::new(Address::invalid(1), 0);
        let args = [catch.data, exception];
        match self.call_fn_pointer_word(catch.catch_fn, &args, unit, body, span, then)? {
            Some(frame) => {
                self.count_frame()?;
                *block = frame.locals.body.start_block;
                self.code_stack.push(frame);
            }
            None => match then {
                Then::Block(target) => *block = target,
                _ => {
                    return Err(MirEvalError::UndefinedBehavior(
                        // alloc: error path
                        "Diverging function returned".to_owned(),
                    ));
                }
            },
        }
        Ok(())
    }

    /// The exception object `catch_fn` and `__rust_panic_cleanup` get: a heap cell holding
    /// the `*mut (dyn Any + Send)` the panic unwound with. A message becomes a
    /// `Box<&'static str>`, which the program's allocator frees.
    fn exception(&mut self, payload: Payload) -> Result<'db, Address> {
        let size = self.ptr_size();
        let (data, vtable) = match payload {
            Payload::Boxed { data, vtable } => (data, vtable),
            Payload::Message(message) => {
                let text = self.heap_allocate(message.len(), 1)?;
                self.write_memory(text, message.as_bytes())?;
                let boxed = self.allocate(2 * size, size, Origin::Allocator, false)?;
                self.write_pointer(boxed, text)?;
                self.write_memory(boxed.offset(size), &word_bytes(message.len()))?;
                let str_ty = Ty::new(self.interner(), TyKind::Str);
                let static_str = Ty::new_ref(
                    self.interner(),
                    Region::new_static(self.interner()),
                    str_ty,
                    Mutability::Not,
                );
                (boxed, self.vtable_map.id(static_str))
            }
        };
        let cell = self.heap_allocate(2 * size, size)?;
        self.write_pointer(cell, data)?;
        self.write_memory(cell.offset(size), &word_bytes(vtable))?;
        Ok(cell)
    }

    /// The `catch_unwind` intrinsic: `try_fn(data)` runs as a frame going on at `then` that
    /// catches a panic, and the intrinsic returns 0 unless it does.
    pub(super) fn exec_catch_unwind(
        &mut self,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        destination: Interval,
        then: Then,
        span: MirSpan,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let [try_fn, data, catch_fn] = args else {
            return Err(MirEvalError::InternalError("catch_unwind args are not provided".into()));
        };
        if then == Then::Nested {
            not_supported!("catch_unwind called by the evaluator itself");
        }
        let data = self.read_pointer(data.interval.addr)?;
        let catch_fn = from_bytes!(usize, self.read_scalar(catch_fn.interval)?);
        destination.write_from_bytes(self, &0i32.to_le_bytes())?;
        self.moved_args.clear();
        let unit = Interval::new(Address::invalid(1), 0);
        let data_arg = self.byte_pointer_arg(data)?;
        let frame =
            self.exec_fn_pointer(try_fn.interval, unit, &[data_arg], locals, then, span)?;
        Ok(frame.map(|mut frame| {
            frame.catch = Some(Catch { data, catch_fn, destination });
            frame
        }))
    }

    /// A `*mut u8` argument pointing to `to`, in a heap cell of its own.
    pub(super) fn byte_pointer_arg(&mut self, to: Address) -> Result<'db, IntervalAndTy<'db>> {
        let size = self.ptr_size();
        let cell = self.heap_allocate(size, size)?;
        self.write_pointer(cell, to)?;
        let u8_ty = Ty::new_uint(self.interner(), rustc_type_ir::UintTy::U8);
        let ty = Ty::new_ptr(self.interner(), u8_ty, Mutability::Mut);
        Ok(IntervalAndTy { interval: Interval::new(cell, size), ty })
    }

    /// The functions of the panic runtime the evaluator is, which the standard library
    /// declares in an `extern` block: whether `def` is one, and ran.
    pub(super) fn exec_panic_runtime(
        &mut self,
        def: FunctionId,
        name: &str,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        destination: Interval,
        span: MirSpan,
    ) -> Result<'db, bool> {
        if !matches!(def.lookup(self.db).container, ItemContainerId::ExternBlockId(_)) {
            return Ok(false);
        }
        match name {
            // `fn __rust_start_panic(payload: &mut dyn PanicPayload) -> u32`: unwinds with
            // what `payload.take_box()` gives.
            "__rust_start_panic" => {
                let [payload] = args else {
                    not_supported!("__rust_start_panic args are not provided");
                };
                let Some(dyn_ty) = payload.ty.as_reference().map(|(ty, _, _)| ty) else {
                    not_supported!("__rust_start_panic of a {:?}", payload.ty);
                };
                let Some(take_box) = dyn_ty.dyn_trait().and_then(|it| {
                    it.trait_items(self.db).method_by_name(&Name::new_symbol_root(Symbol::intern("take_box")))
                }) else {
                    not_supported!("PanicPayload::take_box not found");
                };
                let size = self.ptr_size();
                let boxed = self.heap_allocate(2 * size, size)?;
                let generic_args = GenericArgs::new_from_slice(&[dyn_ty.into()]);
                self.exec_fn_with_args(
                    take_box,
                    args,
                    generic_args,
                    locals,
                    Interval::new(boxed, 2 * size),
                    Then::Nested,
                    span,
                )?;
                let data = self.read_pointer(boxed)?;
                let vtable = from_bytes!(usize, self.read_scalar(Interval::new(boxed.offset(size), size))?);
                let message = self.payload_message(data, vtable)?;
                let payload = Payload::Boxed { data, vtable };
                self.unwinding = Some(Unwinding { payload, error: None, trace: Vec::new() }); // alloc: empty, allocates nothing
                Err(MirEvalError::Panic(message))
            }
            // `fn __rust_panic_cleanup(exception: *mut u8) -> *mut (dyn Any + Send)`.
            "__rust_panic_cleanup" => {
                let [exception] = args else {
                    not_supported!("__rust_panic_cleanup args are not provided");
                };
                let size = self.ptr_size();
                let cell = self.read_pointer(exception.interval.addr)?;
                let data = self.read_pointer(cell)?;
                let vtable =
                    OwnedBytes::from_slice(self.read_scalar(Interval::new(cell.offset(size), size))?);
                self.write_pointer(destination.addr, data)?;
                self.write_memory(destination.addr.offset(size), &vtable)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// What a panic with a boxed payload is reported as if nothing catches it: the text of
    /// a `&'static str` payload, else `Box<dyn Any>` as rustc's default hook says.
    fn payload_message(&self, data: Address, vtable: usize) -> Result<'db, String> {
        let ty = self.vtable_map.ty(vtable)?;
        if let TyKind::Ref(_, pointee, _) = ty.kind()
            && pointee.is_str()
        {
            let size = self.ptr_size();
            let text = self.read_pointer(data)?;
            let len = from_bytes!(usize, self.read_scalar(Interval::new(data.offset(size), size))?);
            let bytes = self.read_memory(text, len)?;
            return Ok(String::from_utf8_lossy(bytes).into_owned()); // alloc: a panic's message, error path
        }
        // dyn: rustc's text for a payload that is no string, not a trait object
        Ok("Box<dyn Any>".to_owned())
    }

    /// Calls the function pointer `func` (its word) with the `*mut u8` arguments `args`, as
    /// [`Self::exec_fn_pointer`] does.
    pub(super) fn call_fn_pointer_word(
        &mut self,
        func: usize,
        args: &[Address],
        destination: Interval,
        body: &'db MirBody<'db>,
        span: MirSpan,
        then: Then,
    ) -> Result<'db, Option<StackFrame<'db>>> {
        let size = self.ptr_size();
        let func_cell = self.heap_allocate(size, size)?;
        self.write_memory(func_cell, &word_bytes(func))?;
        let args = args.iter().map(|&it| self.byte_pointer_arg(it)).collect::<Result<'db, Vec<_>>>()?;
        let locals = Locals { ptr: ArenaMap::new(), body };
        self.exec_fn_pointer(Interval::new(func_cell, size), destination, &args, &locals, then, span)
    }
}
