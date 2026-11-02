//! Interpret intrinsics, lang items and `extern "C"` wellknown functions which their implementation
//! is not available.
//!
use std::cmp::{self, Ordering};

use hir_def::{
    attrs::AttrFlags,
    signatures::{FunctionSignature, StructSignature, UnionSignature},
};
use rustc_abi::ExternAbi;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike, Ty as _};

use crate::{
    db::HirDatabase,
    InferBodyId,
    drop::{DropGlue, has_drop_glue},
    intrinsic::{
        AtomicOp, AtomicOrdering, AtomicOrders, FloatWidth, Intrinsic, SimdOp, atomic_orderings,
        intrinsic_of,
    },
    mir::MirLowerError,
    mir::eval::{
        Address, AdtId, Evaluator, FunctionId, GenericArgs, Interned, InternedLayout,
        Interval, IntervalAndTy, IntervalOrOwned, IsSigned, ItemContainerId, Layout, Locals, Lookup,
        MirEvalError, MirSpan, Mutability, Origin, OwnedBytes, Result, Then, Ty, TyKind, TypeIdOf,
        VTableOfBytes,
        VariantId, from_bytes, not_supported, pad16, try_const_usize,
        machine::{Access, AtomicOrd, Machine, RetagMode, Shadow},
    },
    mir::location::{source_location_in_file, source_path, track_caller},
    next_solver::{Region, RegionExt},
};

mod arch;
mod reflection;
mod simd;
mod thread;

pub(super) use thread::{Process, Resume, TlsData};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalLangItem {
    BeginPanic,
    SliceLen,
    DropInPlace,
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    pub(super) fn detect_and_exec_special_function(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        span: MirSpan,
    ) -> Result<'db, bool> {
        if self.foreign_claims(def) && self.exec_foreign(def, args, generic_args, destination)? {
            return Ok(true);
        }
        if self.not_special_fn_cache.borrow().contains(&def) {
            return Ok(false);
        }

        match intrinsic_of(self.db, def) {
            Ok(Some(intrinsic)) => {
                return self.exec_intrinsic_call(
                    def,
                    intrinsic,
                    args,
                    generic_args,
                    locals,
                    destination,
                    span,
                );
            }
            Ok(None) => {}
            Err(unknown) => {
                return Err(MirEvalError::MirLowerError(
                    def,
                    MirLowerError::UnknownIntrinsic(unknown.0),
                ));
            }
        }
        let function_data = FunctionSignature::of(self.db, def);
        let attrs = AttrFlags::query(self.db, def.into());
        if let ItemContainerId::ExternBlockId(_) = def.lookup(self.db).container
            && let Some(link_name) = AttrFlags::link_name(self.db, def.into())
            && let Some(name) = link_name.as_str().strip_prefix("llvm.")
        {
            self.exec_llvm_intrinsic(name, args, destination)?;
            return Ok(true);
        }
        if self.exec_panic_runtime(
            def,
            function_data.name.as_str(),
            args,
            locals,
            destination,
            span,
        )? {
            return Ok(true);
        }
        let is_extern_c = match def.lookup(self.db).container {
            hir_def::ItemContainerId::ExternBlockId(block) => {
                matches!(block.abi(self.db), ExternAbi::C { .. })
            }
            _ => false,
        };
        if is_extern_c {
            return self
                .exec_extern_c(
                    function_data.name.as_str(),
                    args,
                    generic_args,
                    destination,
                    locals,
                    span,
                )
                .map(|()| true);
        }

        if attrs.intersects(
            AttrFlags::RUSTC_ALLOCATOR
                | AttrFlags::RUSTC_DEALLOCATOR
                | AttrFlags::RUSTC_REALLOCATOR
                | AttrFlags::RUSTC_ALLOCATOR_ZEROED,
        ) {
            self.exec_alloc_fn(attrs, args, destination)?;
            return Ok(true);
        }
        if let Some(it) = self.detect_lang_function(def) {
            let result = self.exec_lang_item(it, generic_args, args, locals, span)?;
            destination.write_from_bytes(self, &result)?;
            return Ok(true);
        }
        if let ItemContainerId::TraitId(t) = def.lookup(self.db).container
            && Some(t) == self.lang_items().Clone
        {
            let [self_ty] = generic_args.as_slice() else {
                not_supported!("wrong generic arg count for clone");
            };
            let Some(self_ty) = self_ty.ty() else {
                not_supported!("wrong generic arg kind for clone");
            };
            // Clone has special impls for tuples and function pointers
            if matches!(self_ty.kind(), TyKind::FnPtr(..) | TyKind::Tuple(..) | TyKind::Closure(..))
            {
                self.exec_clone(def, args, self_ty, locals, destination, span)?;
                return Ok(true);
            }
            // Return early to prevent caching clone as non special fn.
            return Ok(false);
        }
        self.not_special_fn_cache.borrow_mut().insert(def);
        Ok(false)
    }

    pub(super) fn detect_and_redirect_special_function(
        &mut self,
        def: FunctionId,
    ) -> Result<'db, Option<FunctionId>> {
        // Core's `panic_impl` is the crate graph's `#[panic_handler]`; with none (a
        // constant's evaluation, a graph without std) `PanicFmt` goes to `ConstPanicFmt`, as in
        // rustc's CTFE.
        if Some(def) == self.lang_items().PanicImpl
            && let Some(handler) = self.panic_handler()
        {
            return Ok(Some(handler));
        }
        if Some(def) == self.lang_items().PanicFmt && self.panic_handler().is_none() {
            let Some(const_panic_fmt) = self.lang_items().ConstPanicFmt else {
                not_supported!("const_panic_fmt lang item not found or not a function");
            };
            return Ok(Some(const_panic_fmt));
        }
        Ok(None)
    }

    /// Clone has special impls for tuples and function pointers
    fn exec_clone(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        self_ty: Ty<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        span: MirSpan,
    ) -> Result<'db, ()> {
        match self_ty.kind() {
            TyKind::FnPtr(..) => {
                let [arg] = args else {
                    not_supported!("wrong arg count for clone");
                };
                let addr = Address::from_bytes(arg.get(self)?)?;
                return destination
                    .write_from_interval(self, Interval { addr, size: destination.size });
            }
            TyKind::Closure(_, closure_args) => self.exec_clone(
                def,
                args,
                closure_args.as_closure().tupled_upvars_ty(),
                locals,
                destination,
                span,
            )?,
            TyKind::Tuple(subst) => {
                let [arg] = args else {
                    not_supported!("wrong arg count for clone");
                };
                let addr = Address::from_bytes(arg.get(self)?)?;
                let layout = self.layout(self_ty)?;
                self.exec_clone_for_fields(
                    subst.iter(),
                    layout,
                    addr,
                    def,
                    locals,
                    destination,
                    span,
                )?;
            }
            _ => {
                self.exec_fn_with_args(
                    def,
                    args,
                    GenericArgs::new_from_slice(&[self_ty.into()]),
                    locals,
                    destination,
                    Then::Nested,
                    span,
                )?;
            }
        }
        Ok(())
    }

    fn exec_clone_for_fields(
        &mut self,
        ty_iter: impl Iterator<Item = Ty<'db>>,
        layout: Interned<'static, InternedLayout>,
        addr: Address,
        def: FunctionId,
        locals: &Locals<'db>,
        destination: Interval,
        span: MirSpan,
    ) -> Result<'db, ()> {
        for (i, ty) in ty_iter.enumerate() {
            let size = self.layout(ty)?.size.bytes_usize();
            let tmp = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
            let arg = IntervalAndTy {
                interval: Interval { addr: tmp, size: self.ptr_size() },
                ty: Ty::new_ref(
                    self.interner(),
                    Region::error(self.interner()),
                    ty,
                    Mutability::Not,
                ),
            };
            let offset = layout.fields.offset(i).bytes_usize();
            self.write_memory(tmp, &addr.offset(offset).to_bytes())?;
            self.exec_clone(
                def,
                &[arg],
                ty,
                locals,
                destination.slice(offset..offset + size),
                span,
            )?;
        }
        Ok(())
    }

    fn exec_alloc_fn(
        &mut self,
        alloc_fn: AttrFlags,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
    ) -> Result<'db, ()> {
        match alloc_fn {
            _ if alloc_fn
                .intersects(AttrFlags::RUSTC_ALLOCATOR_ZEROED | AttrFlags::RUSTC_ALLOCATOR) =>
            {
                let [size, align] = args else {
                    return Err(MirEvalError::InternalError(
                        "rustc_allocator args are not provided".into(),
                    ));
                };
                let size = from_bytes!(usize, self.read_scalar(size.interval)?);
                let align = from_bytes!(usize, self.read_scalar(align.interval)?);
                let zeroed = alloc_fn.contains(AttrFlags::RUSTC_ALLOCATOR_ZEROED);
                let result = self.allocate(size, align, Origin::Allocator, zeroed)?;
                self.write_pointer(destination.addr, result)?;
            }
            _ if alloc_fn.contains(AttrFlags::RUSTC_DEALLOCATOR) => {
                let [ptr, ..] = args else {
                    return Err(MirEvalError::InternalError(
                        "rustc_deallocator args are not provided".into(),
                    ));
                };
                let ptr = self.read_pointer(ptr.interval.addr)?;
                self.deallocate(ptr, Origin::Allocator, "__rust_dealloc")?;
            }
            _ if alloc_fn.contains(AttrFlags::RUSTC_REALLOCATOR) => {
                let [ptr, _old_size, _align, new_size] = args else {
                    return Err(MirEvalError::InternalError(
                        "rustc_allocator args are not provided".into(),
                    ));
                };
                let new_size = from_bytes!(usize, self.read_scalar(new_size.interval)?);
                let ptr = self.read_pointer(ptr.interval.addr)?;
                let result = self.reallocate(ptr, new_size, Origin::Allocator, "__rust_realloc")?;
                self.write_pointer(destination.addr, result)?;
            }
            _ => not_supported!("unknown alloc function"),
        }
        Ok(())
    }

    fn detect_lang_function(&self, def: FunctionId) -> Option<EvalLangItem> {
        use EvalLangItem::*;
        let lang_items = self.lang_items();
        let attrs = AttrFlags::query(self.db, def.into());

        // With a `#[panic_handler]` a panic runs the standard library's own code.
        let const_panics = self.panic_handler().is_none();
        if const_panics && attrs.contains(AttrFlags::RUSTC_CONST_PANIC_STR) {
            // `#[rustc_const_panic_str]` is treated like `lang = "begin_panic"` by rustc CTFE.
            return Some(BeginPanic);
        }

        // We want to execute these functions with special logic
        // `PanicFmt` is not detected here as it's redirected later.
        if let Some((_, candidate)) = [
            (lang_items.BeginPanic.filter(|_| const_panics), BeginPanic),
            (lang_items.SliceLen, SliceLen),
            (lang_items.DropInPlace, DropInPlace),
            // `drop_in_place`'s callee, whose body is a placeholder for the type's drop glue.
            (lang_items.DropGlue, DropInPlace),
        ]
        .iter()
        .find(|&(candidate, _)| candidate == Some(def))
        {
            return Some(candidate);
        }

        None
    }

    fn exec_lang_item(
        &mut self,
        it: EvalLangItem,
        generic_args: GenericArgs<'db>,
        args: &[IntervalAndTy<'db>],
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, Vec<u8>> {
        use EvalLangItem::*;
        let mut args = args.iter();
        match it {
            BeginPanic => {
                let mut arg = *args.next().ok_or(MirEvalError::InternalError(
                    "argument of BeginPanic is not provided".into(),
                ))?;
                while let TyKind::Ref(_, ty, _) = arg.ty.kind() {
                    if ty.is_str() {
                        let pointee = self.read_pointer(arg.interval.addr)?;
                        let metadata = &arg.interval.get(self)?[self.ptr_size()..];
                        let len = from_bytes!(usize, metadata);

                        return {
                            Err(MirEvalError::Panic(
                                std::str::from_utf8(self.read_memory(pointee, len)?)
                                    .unwrap()
                                    .to_owned(), // alloc: the panic's message, error path
                            ))
                        };
                    }
                    let size = self.size_of_sized(ty, locals, "begin panic arg")?;
                    let pointee = self.read_pointer(arg.interval.addr)?;
                    arg = IntervalAndTy { interval: Interval::new(pointee, size), ty };
                }
                // alloc: error path
                Err(MirEvalError::Panic(format!("unknown-panic-payload: {:?}", arg.ty.kind())))
            }
            SliceLen => {
                let arg = args.next().ok_or(MirEvalError::InternalError(
                    "argument of <[T]>::len() is not provided".into(),
                ))?;
                let arg = arg.get(self)?;
                let ptr_size = arg.len() / 2;
                Ok(arg[ptr_size..].into())
            }
            DropInPlace => {
                let ty = generic_args.as_slice().first().and_then(|it| it.ty()).ok_or(
                    MirEvalError::InternalError(
                        "generic argument of drop_in_place is not provided".into(),
                    ),
                )?;
                let arg = args.next().ok_or(MirEvalError::InternalError(
                    "argument of drop_in_place is not provided".into(),
                ))?;
                let addr = self.read_pointer(arg.interval.addr)?;
                let metadata = OwnedBytes::from_slice(&arg.interval.get(self)?[self.ptr_size()..]);
                let then = self.call_then;
                self.pending = self.run_drop_glue(ty, locals, addr, &metadata, span, then)?;
                Ok(Vec::new()) // alloc: empty, allocates nothing
            }
        }
    }

    fn exec_syscall(
        &mut self,
        id: i64,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
        _locals: &Locals<'db>,
        _span: MirSpan,
    ) -> Result<'db, ()> {
        if id == self.sys_getrandom()? {
            let [buf, len, _flags] = args else {
                return Err(MirEvalError::InternalError(
                    "SYS_getrandom args are not provided".into(),
                ));
            };
            let addr = Address::from_bytes(buf.get(self)?)?;
            let size = from_bytes!(usize, len.get(self)?);
            self.fill_random(addr, size)?;
            return destination.write_from_interval(self, len.interval);
        }
        not_supported!("Unknown syscall id {id:?}")
    }

    /// The number of `getrandom` in the Linux system call table of the crate's `target_arch`
    /// (the kernel's `unistd.h`): the generic table's, but on the architectures with a table
    /// of their own.
    fn sys_getrandom(&self) -> Result<'db, i64> {
        let cfg = self.crate_id.cfg_options(self.db);
        let Some(arch) = cfg.get_cfg_values("target_arch").next() else {
            return Err(MirEvalError::InternalError(
                "a system call in a crate with no `target_arch`".into(),
            ));
        };
        Ok(match arch.as_str() {
            // x32, x86-64 with 32-bit pointers, sets `__X32_SYSCALL_BIT` in every number.
            "x86_64" if self.ptr_size() == 4 => 0x4000_0000 + 318,
            "x86_64" => 318,
            "x86" => 355,
            "arm" => 384,
            "aarch64" | "riscv64" | "riscv32" | "loongarch64" => 278,
            other => not_supported!("the number of the system call `getrandom` on {other}"),
        })
    }

    fn fill_random(&mut self, addr: Address, size: usize) -> Result<'db, ()> {
        let mut chunk = [0u8; 64];
        let mut done = 0;
        while done < size {
            let len = (size - done).min(chunk.len());
            self.random_state.fill(&mut chunk[..len]);
            self.write_memory(addr.offset(done), &chunk[..len])?;
            done += len;
        }
        Ok(())
    }

    fn exec_extern_c(
        &mut self,
        as_str: &str,
        args: &[IntervalAndTy<'db>],
        _generic_args: GenericArgs<'db>,
        destination: Interval,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        if self.exec_thread_shim(as_str, args, destination, locals, span)? {
            return Ok(());
        }
        match as_str {
            "memcmp" => {
                let [ptr1, ptr2, size] = args else {
                    return Err(MirEvalError::InternalError("memcmp args are not provided".into()));
                };
                let addr1 = self.read_pointer(ptr1.interval.addr)?;
                let addr2 = self.read_pointer(ptr2.interval.addr)?;
                let size = from_bytes!(usize, self.read_scalar(size.interval)?);
                let slice1 = self.read_scalar(Interval::new(addr1, size))?;
                let slice2 = self.read_scalar(Interval::new(addr2, size))?;
                let r: i128 = match slice1.cmp(slice2) {
                    cmp::Ordering::Less => -1,
                    cmp::Ordering::Equal => 0,
                    cmp::Ordering::Greater => 1,
                };
                destination.write_from_bytes(self, &r.to_le_bytes()[..destination.size])
            }
            "memcpy" | "memmove" => {
                let [dst, src, size] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!("{as_str} args are not provided").into(),
                    ));
                };
                let dst = self.read_pointer(dst.interval.addr)?;
                let src = self.read_pointer(src.interval.addr)?;
                let size = from_bytes!(usize, self.read_scalar(size.interval)?);
                self.copy_memory(dst, src, size, 1, as_str == "memmove")?;
                self.write_pointer(destination.addr, dst)
            }
            "memset" => {
                let [dst, byte, size] = args else {
                    return Err(MirEvalError::InternalError("memset args are not provided".into()));
                };
                let dst = self.read_pointer(dst.interval.addr)?;
                // The `c_int`'s low byte.
                let Some(&byte) = self.read_scalar(byte.interval)?.first() else {
                    return Err(MirEvalError::InternalError("memset byte is of no size".into()));
                };
                let size = from_bytes!(usize, self.read_scalar(size.interval)?);
                self.write_memory_using_ref(dst, size)?.fill(byte);
                self.write_pointer(destination.addr, dst)
            }
            "strlen" => {
                let [s] = args else {
                    return Err(MirEvalError::InternalError("strlen args are not provided".into()));
                };
                let s = self.read_pointer(s.interval.addr)?;
                let mut len = 0;
                while self.read_scalar(Interval::new(s.offset(len), 1))? != [0] {
                    len += 1;
                }
                destination.write_from_bytes(self, &super::word_bytes(len))
            }
            "write" => {
                let [fd, ptr, len] = args else {
                    return Err(MirEvalError::InternalError(
                        "libc::write args are not provided".into(),
                    ));
                };
                let fd = u128::from_le_bytes(pad16(fd.get(self)?, IsSigned::No));
                let interval = Interval {
                    addr: self.read_pointer(ptr.interval.addr)?,
                    size: from_bytes!(usize, len.get(self)?),
                };
                match fd {
                    1 => {
                        self.write_to_stdout(interval)?;
                    }
                    2 => {
                        self.write_to_stderr(interval)?;
                    }
                    _ => not_supported!("write to arbitrary file descriptor"),
                }
                destination.write_from_interval(self, len.interval)?;
                Ok(())
            }
            "CCRandomGenerateBytes" => {
                // Apple's CommonCrypto: `CCRNGStatus CCRandomGenerateBytes(void *, size_t)`,
                // `kCCSuccess` (0) when it filled the buffer.
                let [buf, len] = args else {
                    return Err(MirEvalError::InternalError(
                        "CCRandomGenerateBytes args are not provided".into(),
                    ));
                };
                let addr = Address::from_bytes(buf.get(self)?)?;
                let size = from_bytes!(usize, len.get(self)?);
                self.fill_random(addr, size)?;
                destination.write_from_bytes(self, &0i32.to_le_bytes()[0..destination.size])
            }
            "syscall" => {
                let Some((id, rest)) = args.split_first() else {
                    return Err(MirEvalError::InternalError("syscall arg1 is not provided".into()));
                };
                let id = from_bytes!(i64, id.get(self)?);
                self.exec_syscall(id, rest, destination, locals, span)
            }
            "getenv" => {
                let [name] = args else {
                    return Err(MirEvalError::InternalError("getenv args are not provided".into()));
                };
                let mut name_buf = Vec::new(); // alloc: `getenv`'s name, a shim of the edge
                let name = {
                    let mut index = Address::from_bytes(name.get(self)?)?;
                    loop {
                        let byte = self.read_memory(index, 1)?[0];
                        index = index.offset(1);
                        if byte == 0 {
                            break;
                        }
                        name_buf.push(byte);
                    }
                    String::from_utf8_lossy(&name_buf) // alloc: borrows unless not UTF-8
                };
                let value = self.crate_id.env(self.db).get(&name);
                match value {
                    None => {
                        // Write null as fail
                        self.write_memory_using_ref(destination.addr, destination.size)?.fill(0);
                    }
                    Some(mut value) => {
                        value.push('\0');
                        let addr = self.heap_allocate(value.len(), 1)?;
                        self.write_memory(addr, value.as_bytes())?;
                        self.write_memory(destination.addr, &addr.to_bytes())?;
                    }
                }
                Ok(())
            }
            "malloc" => {
                let [size] = args else {
                    return Err(MirEvalError::InternalError("malloc args are not provided".into()));
                };
                let size = from_bytes!(usize, size.get(self)?);
                let addr = self.c_allocate(size, false)?;
                self.write_pointer(destination.addr, addr)
            }
            "calloc" => {
                let [count, size] = args else {
                    return Err(MirEvalError::InternalError("calloc args are not provided".into()));
                };
                let count = from_bytes!(usize, count.get(self)?);
                let size = from_bytes!(usize, size.get(self)?);
                // An overflowing size is C's failure: a null pointer.
                let addr = match count.checked_mul(size) {
                    Some(size) => self.c_allocate(size, true)?,
                    None => Address::from_usize(0),
                };
                self.write_pointer(destination.addr, addr)
            }
            "realloc" => {
                let [ptr, size] = args else {
                    return Err(MirEvalError::InternalError("realloc args are not provided".into()));
                };
                let ptr = self.read_pointer(ptr.interval.addr)?;
                let size = from_bytes!(usize, size.get(self)?);
                let addr = if ptr.to_usize() == 0 {
                    self.c_allocate(size, false)?
                } else if size == 0 {
                    match self.target()?.os {
                        // glibc's: the block is freed and the result is null.
                        thread::Os::Linux => {
                            self.deallocate(ptr, Origin::C, "realloc")?;
                            Address::from_usize(0)
                        }
                        // macOS': a block of the least size in its place.
                        thread::Os::Apple => self.reallocate(ptr, 1, Origin::C, "realloc")?,
                    }
                } else {
                    self.reallocate(ptr, size, Origin::C, "realloc")?
                };
                self.write_pointer(destination.addr, addr)
            }
            "posix_memalign" => {
                let [out, align, size] = args else {
                    return Err(MirEvalError::InternalError(
                        "posix_memalign args are not provided".into(),
                    ));
                };
                let out = self.read_pointer(out.interval.addr)?;
                let align = from_bytes!(usize, align.get(self)?);
                let size = from_bytes!(usize, size.get(self)?);
                // `EINVAL` (22 on every target the evaluator runs): not a power of two, or not
                // a multiple of a pointer's size.
                let code: i32 = if !align.is_power_of_two() || align % self.ptr_size() != 0 {
                    22
                } else {
                    // Of no size, as glibc's and macOS': a block of its own, not null.
                    let addr = self.allocate(size.max(1), align, Origin::C, false)?;
                    self.write_pointer(out, addr)?;
                    0
                };
                destination.write_from_bytes(self, &code.to_le_bytes()[0..destination.size])
            }
            "free" => {
                let [ptr] = args else {
                    return Err(MirEvalError::InternalError("free args are not provided".into()));
                };
                let ptr = self.read_pointer(ptr.interval.addr)?;
                if ptr.to_usize() != 0 {
                    self.deallocate(ptr, Origin::C, "free")?;
                }
                Ok(())
            }
            _ => not_supported!("unknown external function {as_str}"),
        }
    }

    /// `malloc`'s block of `size` bytes (`calloc`'s, initialized to zero, when `zeroed`),
    /// aligned as C's `max_align_t` (two pointers' size). Of no size, as glibc's and macOS'
    /// `malloc(0)`: a block of its own, not null, which `free` takes back.
    fn c_allocate(&mut self, size: usize, zeroed: bool) -> Result<'db, Address> {
        self.allocate(size.max(1), 2 * self.ptr_size(), Origin::C, zeroed)
    }

    /// A call of the intrinsic `intrinsic`, the function `def`: `Ok(false)` when the evaluator
    /// does not implement it and its fallback body is to run, an error when it has none or
    /// must be overridden.
    pub(super) fn exec_intrinsic_call(
        &mut self,
        def: FunctionId,
        intrinsic: Intrinsic,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        span: MirSpan,
    ) -> Result<'db, bool> {
        if self.exec_intrinsic(intrinsic, args, generic_args, destination, locals, span)? {
            return Ok(true);
        }
        let needs_override = !FunctionSignature::of(self.db, def).has_body()
            || AttrFlags::query(self.db, def.into())
                .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN);
        if needs_override {
            not_supported!("intrinsic {intrinsic} is not implemented");
        }
        Ok(false)
    }

    /// Runs `intrinsic`, or says the evaluator does not implement it (`Ok(false)`).
    fn exec_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, bool> {
        match intrinsic {
            Intrinsic::Atomic(op, orders) => {
                return self
                    .exec_atomic_intrinsic(
                        op,
                        orders,
                        args,
                        generic_args,
                        destination,
                        locals,
                        span,
                    )
                    .map(|()| true);
            }
            Intrinsic::Simd(op) => {
                return self
                    .exec_simd_intrinsic(op, args, generic_args, destination, locals, span)
                    .map(|()| true);
            }
            Intrinsic::TypeIdTypeOf
            | Intrinsic::TypeIdVtable
            | Intrinsic::TypeIdIsSigned
            | Intrinsic::TypeIdArrayLen
            | Intrinsic::TypeIdElementTy
            | Intrinsic::TypeIdPointsTo
            | Intrinsic::TypeIdPointsMutably
            | Intrinsic::TypeIdSizeOf
            | Intrinsic::TypeIdVariants
            | Intrinsic::TypeIdVariantName
            | Intrinsic::TypeIdVariantNonExhaustive
            | Intrinsic::TypeIdNonExhaustive
            | Intrinsic::TypeIdFields
            | Intrinsic::TypeIdFieldRepresentingType
            | Intrinsic::FieldRepresentingTypeActualTypeId
            | Intrinsic::FieldRepresentingTypeName
            | Intrinsic::FieldRepresentingTypeOffset
            | Intrinsic::TypeIdGenerics
            | Intrinsic::TypeIdFunctionPtr => {
                return self.exec_reflection_intrinsic(intrinsic, args, destination, locals);
            }
            Intrinsic::Sqrt
            | Intrinsic::Sin
            | Intrinsic::Cos
            | Intrinsic::Exp
            | Intrinsic::Exp2
            | Intrinsic::Log
            | Intrinsic::Log10
            | Intrinsic::Log2
            | Intrinsic::Floor
            | Intrinsic::Ceil
            | Intrinsic::Trunc
            | Intrinsic::Round
            | Intrinsic::RoundTiesEven
            | Intrinsic::Powf
            | Intrinsic::Powi
            | Intrinsic::Fma => {
                let width = float_width(intrinsic, args)?;
                self.exec_float_intrinsic(intrinsic, width, args, destination)
            }
            Intrinsic::CopySign(width @ (FloatWidth::F32 | FloatWidth::F64)) => {
                self.exec_float_intrinsic(intrinsic, width, args, destination)
            }
            Intrinsic::SizeOf => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "size_of generic arg is not provided".into(),
                    ));
                };
                let size = self.size_of_sized(ty, locals, "size_of arg")?;
                destination.write_from_bytes(self, &size.to_le_bytes()[0..destination.size])
            }
            Intrinsic::AlignOf => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "align_of generic arg is not provided".into(),
                    ));
                };
                let align = self.layout(ty)?.align.bytes();
                destination.write_from_bytes(self, &align.to_le_bytes()[0..destination.size])
            }
            Intrinsic::SizeOfVal => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "size_of_val generic arg is not provided".into(),
                    ));
                };
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "size_of_val args are not provided".into(),
                    ));
                };
                if let Some((size, _)) = self.size_align_of(ty, locals)? {
                    destination.write_from_bytes(self, &super::word_bytes(size))
                } else {
                    let metadata = arg.interval.slice(self.ptr_size()..self.ptr_size() * 2);
                    let (size, _) = self.size_align_of_unsized(ty, metadata, locals)?;
                    destination.write_from_bytes(self, &super::word_bytes(size))
                }
            }
            Intrinsic::AlignOfVal => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "align_of_val generic arg is not provided".into(),
                    ));
                };
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "align_of_val args are not provided".into(),
                    ));
                };
                if let Some((_, align)) = self.size_align_of(ty, locals)? {
                    destination.write_from_bytes(self, &super::word_bytes(align))
                } else {
                    let metadata = arg.interval.slice(self.ptr_size()..self.ptr_size() * 2);
                    let (_, align) = self.size_align_of_unsized(ty, metadata, locals)?;
                    destination.write_from_bytes(self, &super::word_bytes(align))
                }
            }
            Intrinsic::TypeId => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "type_id generic arg is not provided".into(),
                    ));
                };
                let ty = self.type_id_ty(ty)?;
                let id = self.type_id(TypeIdOf::Ty(ty));
                destination.write_from_bytes(self, &id)
            }
            Intrinsic::TypeName => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "type_name generic arg is not provided".into(),
                    ));
                };
                let Some(ty_name) = crate::mir::type_name::rustc_type_name(self.db, ty) else {
                    // alloc: error path
                    return Err(MirEvalError::NotSupported(format!(
                        "`type_name::<{ty:?}>()`: the type has no path to print"
                    )));
                };
                let len = ty_name.len();
                let addr = self.heap_allocate(len, 1)?;
                self.write_memory(addr, ty_name.as_bytes())?;
                destination.slice(0..self.ptr_size()).write_from_bytes(self, &addr.to_bytes())?;
                destination
                    .slice(self.ptr_size()..2 * self.ptr_size())
                    .write_from_bytes(self, &super::word_bytes(len))
            }
            Intrinsic::NeedsDrop => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "size_of generic arg is not provided".into(),
                    ));
                };
                let result = match has_drop_glue(&self.infcx, ty, self.param_env.param_env) {
                    DropGlue::HasDropGlue => true,
                    DropGlue::None => false,
                    DropGlue::DependOnParams => {
                        return Err(MirEvalError::InternalError(
                            // alloc: error path
                            format!("`needs_drop::<{ty:?}>()` of a type not monomorphized").into(),
                        ));
                    }
                };
                destination.write_from_bytes(self, &[u8::from(result)])
            }
            Intrinsic::PtrGuaranteedCmp => {
                // FIXME: this is wrong for const eval, it should return 2 in some
                // cases.
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "ptr_guaranteed_cmp args are not provided".into(),
                    ));
                };
                let ans = lhs.get(self)? == rhs.get(self)?;
                destination.write_from_bytes(self, &[u8::from(ans)])
            }
            Intrinsic::SaturatingAdd | Intrinsic::SaturatingSub => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "saturating_add args are not provided".into(),
                    ));
                };
                let add = intrinsic == Intrinsic::SaturatingAdd;
                let signed = matches!(lhs.ty.kind(), TyKind::Int(_));
                let bits = destination.size as u32 * 8;
                let ans = if signed {
                    let lhs = i128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::Yes));
                    let rhs = i128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::Yes));
                    let ans = if add { lhs.saturating_add(rhs) } else { lhs.saturating_sub(rhs) };
                    let max = i128::MAX >> (128 - bits);
                    ans.clamp(-max - 1, max) as u128
                } else {
                    let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                    let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                    let ans = if add { lhs.saturating_add(rhs) } else { lhs.saturating_sub(rhs) };
                    ans.min(u128::MAX >> (128 - bits))
                };
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            Intrinsic::WrappingAdd => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "wrapping_add args are not provided".into(),
                    ));
                };
                let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                let ans = lhs.wrapping_add(rhs);
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            Intrinsic::PtrOffsetFromUnsigned | Intrinsic::PtrOffsetFrom => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "ptr_offset_from args are not provided".into(),
                    ));
                };
                let lhs = self.read_pointer(lhs.interval.addr)?;
                let rhs = self.read_pointer(rhs.interval.addr)?;
                if M::CHECKS {
                    let unsigned = matches!(intrinsic, Intrinsic::PtrOffsetFromUnsigned);
                    self.machine.offset_from(self.ptr(lhs), self.ptr(rhs), unsigned)?;
                }
                let lhs = lhs.to_usize() as i128;
                let rhs = rhs.to_usize() as i128;
                let ans = lhs.wrapping_sub(rhs);
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "ptr_offset_from generic arg is not provided".into(),
                    ));
                };
                let size = self.size_of_sized(ty, locals, "ptr_offset_from arg")? as i128;
                let ans = ans / size;
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            Intrinsic::WrappingSub => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "wrapping_sub args are not provided".into(),
                    ));
                };
                let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                let ans = lhs.wrapping_sub(rhs);
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            Intrinsic::WrappingMul => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "wrapping_mul args are not provided".into(),
                    ));
                };
                let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                let ans = lhs.wrapping_mul(rhs);
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            Intrinsic::UncheckedAdd
            | Intrinsic::UncheckedSub
            | Intrinsic::UncheckedMul
            | Intrinsic::UncheckedShl
            | Intrinsic::UncheckedShr
            | Intrinsic::UncheckedDiv
            | Intrinsic::UncheckedRem
            | Intrinsic::ExactDiv => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!("{intrinsic} args are not provided").into(),
                    ));
                };
                let ans = self.unchecked_int(intrinsic, lhs, rhs)?;
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            Intrinsic::AddWithOverflow
            | Intrinsic::SubWithOverflow
            | Intrinsic::MulWithOverflow => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_eval_select args are not provided".into(),
                    ));
                };
                let result_ty = Ty::new_tup_from_iter(
                    self.interner(),
                    [lhs.ty, Ty::new_bool(self.interner())].into_iter(),
                );
                let op_size = self.size_of_sized(lhs.ty, locals, "operand of add_with_overflow")?;
                let signed = matches!(lhs.ty.kind(), TyKind::Int(_));
                let extend = || if signed { IsSigned::Yes } else { IsSigned::No };
                let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, extend()));
                let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, extend()));
                let (ans, is_overflow) = if signed {
                    let (lhs, rhs) = (lhs as i128, rhs as i128);
                    let (ans, overflow) = match intrinsic {
                        Intrinsic::AddWithOverflow => lhs.overflowing_add(rhs),
                        Intrinsic::SubWithOverflow => lhs.overflowing_sub(rhs),
                        _ => lhs.overflowing_mul(rhs),
                    };
                    // The result fits when sign-extending its low bytes gives it back.
                    let fits = op_size == 16 || {
                        let shift = 128 - 8 * op_size as u32;
                        (ans << shift) >> shift == ans
                    };
                    (ans as u128, overflow || !fits)
                } else {
                    let (ans, overflow) = match intrinsic {
                        Intrinsic::AddWithOverflow => lhs.overflowing_add(rhs),
                        Intrinsic::SubWithOverflow => lhs.overflowing_sub(rhs),
                        _ => lhs.overflowing_mul(rhs),
                    };
                    (ans, overflow || ans.to_le_bytes()[op_size..].iter().any(|&it| it != 0))
                };
                let is_overflow = OwnedBytes::from_slice(&[u8::from(is_overflow)]);
                let layout = self.layout(result_ty)?;
                let result = self.construct_with_layout(
                    layout.size.bytes_usize(),
                    &layout,
                    None,
                    [OwnedBytes::from_slice(&ans.to_le_bytes()[0..op_size]), is_overflow]
                        .into_iter()
                        .map(IntervalOrOwned::Owned),
                )?;
                destination.write_from_bytes(self, &result)
            }
            Intrinsic::Copy | Intrinsic::CopyNonoverlapping => {
                let [src, dst, count] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!("{intrinsic} args are not provided").into(),
                    ));
                };
                let ty = type_arg(generic_args, intrinsic)?;
                self.copy_values(dst, src, count, ty, intrinsic == Intrinsic::Copy, locals)
            }
            Intrinsic::VolatileCopyMemory | Intrinsic::VolatileCopyNonoverlappingMemory => {
                let [dst, src, count] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path
                        format!("{intrinsic} args are not provided").into(),
                    ));
                };
                let ty = type_arg(generic_args, intrinsic)?;
                let overlapping = intrinsic == Intrinsic::VolatileCopyMemory;
                self.copy_values(dst, src, count, ty, overlapping, locals)
            }
            Intrinsic::SliceGetUnchecked => {
                let [slice_ptr, index] = args else {
                    return Err(MirEvalError::InternalError(
                        "slice_get_unchecked args are not provided".into(),
                    ));
                };
                let Some(ty) = generic_args.as_slice().get(2).and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "slice_get_unchecked item type is not provided".into(),
                    ));
                };
                let slice_ptr = slice_ptr.get(self)?;
                let ptr_size = self.ptr_size();
                let Some(data) = slice_ptr.get(..ptr_size) else {
                    return Err(MirEvalError::InternalError(
                        "slice_get_unchecked slice pointer is too small".into(),
                    ));
                };
                let Some(len) = slice_ptr.get(ptr_size..2 * ptr_size) else {
                    return Err(MirEvalError::InternalError(
                        "slice_get_unchecked slice metadata is missing".into(),
                    ));
                };
                let slice_ptr = Address::from_bytes(data)?;
                let len = from_bytes!(usize, len);
                let index = from_bytes!(usize, index.get(self)?);
                if index >= len {
                    // alloc: error path
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "slice_get_unchecked index {index} is out of bounds for slice of length {len}"
                    )));
                }
                let size = self.size_of_sized(ty, locals, "slice_get_unchecked item type")?;
                let offset = index* size;
                let addr = slice_ptr.to_usize() + offset;
                let addr = Address::from_usize(addr);
                destination.write_from_bytes(self, &addr.to_bytes()[..destination.size])
            }
            Intrinsic::Offset | Intrinsic::ArithOffset => {
                let [ptr, offset] = args else {
                    return Err(MirEvalError::InternalError("offset args are not provided".into()));
                };
                let (ty, signed) = if intrinsic == Intrinsic::Offset {
                    // The operands' types, as rustc's codegen reads them: they are the
                    // `Ptr` and `Delta` of `offset<Ptr, Delta>`.
                    let delta = offset.ty;
                    if !matches!(
                        delta.kind(),
                        TyKind::Int(rustc_type_ir::IntTy::Isize)
                            | TyKind::Uint(rustc_type_ir::UintTy::Usize)
                    ) {
                        return Err(MirEvalError::InternalError(
                            "offset's delta is not usize or isize".into(),
                        ));
                    }
                    match ptr.ty.kind() {
                        TyKind::RawPtr(ty, _) => (ty, matches!(delta.kind(), TyKind::Int(_))),
                        _ => {
                            return Err(MirEvalError::InternalError(
                                "offset's pointer is not a raw pointer".into(),
                            ));
                        }
                    }
                } else {
                    let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                        return Err(MirEvalError::InternalError(
                            "arith_offset generic arg is not provided".into(),
                        ));
                    };
                    (ty, true)
                };
                let base = self.read_pointer(ptr.interval.addr)?;
                let count = pad16(self.read_scalar(offset.interval)?, signed.into());
                let size = self.size_of_sized(ty, locals, "offset ptr type")? as i128;
                let by = i128::from_le_bytes(count).wrapping_mul(size);
                // `arith_offset` (`wrapping_offset`) may leave the allocation; `offset` may not.
                if M::CHECKS && intrinsic == Intrinsic::Offset {
                    let Ok(by) = i64::try_from(by) else {
                        // alloc: error path.
                        return Err(MirEvalError::UndefinedBehavior(format!(
                            "`offset` by {by} bytes, which overflows an `isize`"
                        )));
                    };
                    self.machine.offset(self.ptr(base), by)?;
                }
                let ans = (base.to_usize() as u128).wrapping_add(by as u128);
                let to = Address::from_bytes(&ans.to_le_bytes()[..self.ptr_size()])?;
                self.write_pointer(destination.addr, Address { prov: base.prov, ..to })
            }
            Intrinsic::AssertInhabited
            | Intrinsic::AssertZeroValid
            | Intrinsic::AssertMemUninitializedValid => {
                // FIXME: We should actually implement these checks
                Ok(())
            }
            Intrinsic::Forget => {
                // FIXME
                Ok(())
            }
            Intrinsic::Transmute | Intrinsic::TransmuteUnchecked => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "transmute arg is not provided".into(),
                    ));
                };
                destination.write_from_interval(self, arg.interval)
            }
            Intrinsic::BlackBox => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "black_box arg is not provided".into(),
                    ));
                };
                destination.write_from_interval(self, arg.interval)
            }
            Intrinsic::Ctpop => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("ctpop arg is not provided".into()));
                };
                let result = u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)).count_ones();
                destination
                    .write_from_bytes(self, &(result as u128).to_le_bytes()[0..destination.size])
            }
            Intrinsic::Bswap => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("bswap arg is not provided".into()));
                };
                let mut result = OwnedBytes::from_slice(arg.get(self)?);
                result.reverse();
                destination.write_from_bytes(self, &result)
            }
            Intrinsic::Bitreverse => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "bitreverse arg is not provided".into(),
                    ));
                };
                let mut result = OwnedBytes::from_slice(arg.get(self)?);
                result.reverse();
                for byte in &mut result {
                    *byte = byte.reverse_bits();
                }
                destination.write_from_bytes(self, &result)
            }
            Intrinsic::Ctlz | Intrinsic::CtlzNonzero => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("ctlz arg is not provided".into()));
                };
                let result =
                    u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)).leading_zeros() as usize;
                let result = result - (128 - arg.interval.size * 8);
                destination
                    .write_from_bytes(self, &(result as u128).to_le_bytes()[0..destination.size])
            }
            Intrinsic::Cttz | Intrinsic::CttzNonzero => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("cttz arg is not provided".into()));
                };
                let arg: &[u8] = arg.get(self)?;
                let bit_count = arg.len() as u32 * 8;
                let result = u128::from_le_bytes(pad16(arg, IsSigned::No)).trailing_zeros().min(bit_count);
                destination
                    .write_from_bytes(self, &(result as u128).to_le_bytes()[0..destination.size])
            }
            Intrinsic::RotateLeft => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "rotate_left args are not provided".into(),
                    ));
                };
                let lhs = &lhs.get(self)?[0..destination.size];
                let rhs = rhs.get(self)?[0] as u32;
                match destination.size {
                    1 => {
                        let r = from_bytes!(u8, lhs).rotate_left(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    2 => {
                        let r = from_bytes!(u16, lhs).rotate_left(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    4 => {
                        let r = from_bytes!(u32, lhs).rotate_left(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    8 => {
                        let r = from_bytes!(u64, lhs).rotate_left(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    16 => {
                        let r = from_bytes!(u128, lhs).rotate_left(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    s => not_supported!("destination with size {s} for rotate_left"),
                }
            }
            Intrinsic::RotateRight => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "rotate_right args are not provided".into(),
                    ));
                };
                let lhs = &lhs.get(self)?[0..destination.size];
                let rhs = rhs.get(self)?[0] as u32;
                match destination.size {
                    1 => {
                        let r = from_bytes!(u8, lhs).rotate_right(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    2 => {
                        let r = from_bytes!(u16, lhs).rotate_right(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    4 => {
                        let r = from_bytes!(u32, lhs).rotate_right(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    8 => {
                        let r = from_bytes!(u64, lhs).rotate_right(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    16 => {
                        let r = from_bytes!(u128, lhs).rotate_right(rhs);
                        destination.write_from_bytes(self, &r.to_le_bytes())
                    }
                    s => not_supported!("destination with size {s} for rotate_right"),
                }
            }
            Intrinsic::VariantCount => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "variant_count generic arg is not provided".into(),
                    ));
                };
                let count = match ty.kind() {
                    TyKind::Adt(adt_def, _) => match adt_def.def_id() {
                        AdtId::EnumId(id) => id.enum_variants(self.db).variants.len() as u64,
                        AdtId::StructId(_) | AdtId::UnionId(_) => 1,
                    },
                    TyKind::Param(_) | TyKind::Alias(..) => {
                        not_supported!("variant_count of a type that is not known")
                    }
                    _ => 0,
                };
                destination.write_from_bytes(self, &count.to_le_bytes()[0..destination.size])
            }
            Intrinsic::DiscriminantValue => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "discriminant_value arg is not provided".into(),
                    ));
                };
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "discriminant_value generic arg is not provided".into(),
                    ));
                };
                let addr = Address::from_bytes(arg.get(self)?)?;
                let size = self.size_of_sized(ty, locals, "discriminant_value ptr type")?;
                let interval = Interval { addr, size };
                let r = self.compute_discriminant(ty, interval.get(self)?)?;
                destination.write_from_bytes(self, &r.to_le_bytes()[0..destination.size])
            }
            Intrinsic::ConstEvalSelect => {
                let [tuple, const_fn, _] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_eval_select args are not provided".into(),
                    ));
                };
                let mut args: smallvec::SmallVec<[IntervalAndTy<'db>; 4]> = smallvec::SmallVec::new();
                args.push(*const_fn);
                let TyKind::Tuple(fields) = tuple.ty.kind() else {
                    return Err(MirEvalError::InternalError(
                        "const_eval_select arg[0] is not a tuple".into(),
                    ));
                };
                let layout = self.layout(tuple.ty)?;
                for (i, field) in fields.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes_usize();
                    let addr = tuple.interval.addr.offset(offset);
                    args.push(IntervalAndTy::new(addr, field, self, locals)?);
                }
                if let Some(def) = self.lang_items().FnOnce_call_once {
                    let then = self.call_then;
                    self.pending = self.exec_fn_trait(
                        def,
                        &args,
                        // FIXME: wrong for manual impls of `FnOnce`
                        GenericArgs::empty(),
                        locals,
                        destination,
                        then,
                        span,
                    )?;
                    return Ok(true);
                }
                not_supported!("FnOnce was not available for executing const_eval_select");
            }
            Intrinsic::ReadViaCopy | Intrinsic::VolatileLoad | Intrinsic::UnalignedVolatileLoad => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path.
                        format!("{intrinsic} args are not provided").into(),
                    ));
                };
                let src = self.read_pointer(arg.interval.addr)?;
                let ty = type_arg(generic_args, intrinsic)?;
                let src = match intrinsic {
                    Intrinsic::UnalignedVolatileLoad => src,
                    _ => self.aligned_to(src, ty)?,
                };
                self.move_value(destination.addr, src, destination.size)
            }
            Intrinsic::WriteViaMove
            | Intrinsic::VolatileStore
            | Intrinsic::UnalignedVolatileStore => {
                let [ptr, val] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path.
                        format!("{intrinsic} args are not provided").into(),
                    ));
                };
                let dst = self.read_pointer(ptr.interval.addr)?;
                let dst = match intrinsic {
                    Intrinsic::UnalignedVolatileStore => dst,
                    _ => self.aligned_to(dst, type_arg(generic_args, intrinsic)?)?,
                };
                self.move_value(dst, val.interval.addr, val.interval.size)
            }
            Intrinsic::WriteBytes | Intrinsic::VolatileSetMemory => {
                let [dst, val, count] = args else {
                    return Err(MirEvalError::InternalError(
                        // alloc: error path.
                        format!("{intrinsic} args are not provided").into(),
                    ));
                };
                let ty = type_arg(generic_args, intrinsic)?;
                let count = from_bytes!(usize, self.read_scalar(count.interval)?);
                let val = from_bytes!(u8, self.read_scalar(val.interval)?);
                let (size, align) =
                    self.size_align_of_sized(ty, locals, "the type `write_bytes` writes")?;
                let Some(size) = count.checked_mul(size) else {
                    // alloc: error path.
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "overflow computing the total size of `{intrinsic}`"
                    )));
                };
                let dst = self.read_pointer(dst.interval.addr)?.aligned(align);
                self.write_memory_using_ref(dst, size)?.fill(val);
                Ok(())
            }
            Intrinsic::TypedSwapNonoverlapping => {
                let [x, y] = args else {
                    return Err(MirEvalError::InternalError(
                        "typed_swap_nonoverlapping args are not provided".into(),
                    ));
                };
                let ty = type_arg(generic_args, intrinsic)?;
                let size = self.size_of_sized(ty, locals, "the type `typed_swap_nonoverlapping`")?;
                let x = self.aligned_to(self.read_pointer(x.interval.addr)?, ty)?;
                let y = self.aligned_to(self.read_pointer(y.interval.addr)?, ty)?;
                let (a, b) = (x.to_usize(), y.to_usize());
                if size != 0 && a < b + size && b < a + size {
                    return Err(MirEvalError::UndefinedBehavior(
                        // alloc: error path.
                        "`typed_swap_nonoverlapping` called on overlapping ranges".to_owned(),
                    ));
                }
                let (bytes, x_shadow) = self.read_shadowed(x, size)?;
                // alloc: the temporary `ptr::swap_nonoverlapping` would hold, of any size.
                let x_bytes = bytes.to_vec();
                let (bytes, y_shadow) = self.read_shadowed(y, size)?;
                // alloc: as above.
                let y_bytes = bytes.to_vec();
                self.write_shadowed(x, &y_bytes, &y_shadow)?;
                self.write_shadowed(y, &x_bytes, &x_shadow)?;
                if M::CHECKS {
                    self.typed_copy(x, ty, size, RetagMode::None)?;
                    self.typed_copy(y, ty, size, RetagMode::None)?;
                }
                Ok(())
            }
            Intrinsic::RawEq => {
                let [a, b] = args else {
                    return Err(MirEvalError::InternalError("raw_eq args are not provided".into()));
                };
                let ty = type_arg(generic_args, intrinsic)?;
                let size = self.size_of_sized(ty, locals, "the type `raw_eq` compares")?;
                let a = self.aligned_to(self.read_pointer(a.interval.addr)?, ty)?;
                let b = self.aligned_to(self.read_pointer(b.interval.addr)?, ty)?;
                let eq = self.read_scalar(Interval::new(a, size))?
                    == self.read_scalar(Interval::new(b, size))?;
                destination.write_from_bytes(self, &[u8::from(eq)])
            }
            Intrinsic::PtrMask => {
                let [ptr, mask] = args else {
                    return Err(MirEvalError::InternalError(
                        "ptr_mask args are not provided".into(),
                    ));
                };
                let base = self.read_pointer(ptr.interval.addr)?;
                let mask = from_bytes!(usize, self.read_scalar(mask.interval)?);
                let to = Address::from_usize(base.to_usize() & mask);
                self.write_pointer(destination.addr, Address { prov: base.prov, ..to })
            }
            Intrinsic::PtrMetadata => {
                let [ptr] = args else {
                    return Err(MirEvalError::InternalError(
                        "ptr_metadata args are not provided".into(),
                    ));
                };
                let arg = OwnedBytes::from_slice(ptr.interval.get(self)?);
                let metadata = &arg[self.ptr_size()..];
                destination.write_from_bytes(self, metadata)?;
                Ok(())
            }
            Intrinsic::ThreeWayCompare => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "three_way_compare args are not provided".into(),
                    ));
                };
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "three_way_compare generic arg is not provided".into(),
                    ));
                };
                let signed = match ty.kind() {
                    TyKind::Int(_) => true,
                    TyKind::Uint(_) => false,
                    _ => {
                        return Err(MirEvalError::InternalError(
                            "three_way_compare expects an integral type".into(),
                        ));
                    }
                };
                let rhs = rhs.get(self)?;
                let lhs = lhs.get(self)?;
                let mut result = Ordering::Equal;
                for (l, r) in lhs.iter().zip(rhs).rev() {
                    let it = l.cmp(r);
                    if it != Ordering::Equal {
                        result = it;
                        break;
                    }
                }
                if signed
                    && let Some((&l, &r)) = lhs.iter().zip(rhs).next_back()
                    && l != r
                {
                    result = (l as i8).cmp(&(r as i8));
                }
                if let Some(e) = self.lang_items().Ordering {
                    let ty = self.db.ty(e.into()).skip_binder();
                    let r = self.compute_discriminant(ty, &[result as i8 as u8])?;
                    destination.write_from_bytes(self, &r.to_le_bytes()[0..destination.size])?;
                    Ok(())
                } else {
                    Err(MirEvalError::InternalError("Ordering enum not found".into()))
                }
            }
            Intrinsic::AggregateRawPtr => {
                let [data, meta] = args else {
                    return Err(MirEvalError::InternalError(
                        "aggregate_raw_ptr args are not provided".into(),
                    ));
                };
                destination.write_from_interval(self, data.interval)?;
                Interval {
                    addr: destination.addr.offset(data.interval.size),
                    size: destination.size - data.interval.size,
                }
                .write_from_interval(self, meta.interval)?;
                Ok(())
            }
            Intrinsic::Fabs => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "fabs intrinsic signature doesn't match fn (T) -> T".into(),
                    ));
                };
                let mut bytes = OwnedBytes::from_slice(arg.get(self)?);
                if let Some(sign_byte) = bytes.last_mut() {
                    *sign_byte &= 0x7f;
                }
                destination.write_from_bytes(self, &bytes)
            }
            Intrinsic::Unreachable => {
                return Err(MirEvalError::UndefinedBehavior(
                    // alloc: error path
                    "`unreachable` intrinsic executed".to_owned(),
                ));
            }
            Intrinsic::CompareBytes => {
                // `<[u8]>::cmp` and `memcmp` in `core`: rustc's evaluation gives the order as
                // -1, 0 or 1, where at run time only the sign is promised.
                let [left, right, bytes] = args else {
                    return Err(MirEvalError::InternalError(
                        "compare_bytes args are not provided".into(),
                    ));
                };
                let left = self.read_pointer(left.interval.addr)?;
                let right = self.read_pointer(right.interval.addr)?;
                let bytes = from_bytes!(usize, self.read_scalar(bytes.interval)?);
                let left = self.read_scalar(Interval::new(left, bytes))?;
                let order = left.cmp(self.read_scalar(Interval::new(right, bytes))?);
                destination.write_from_bytes(self, &(order as i32).to_le_bytes())
            }
            Intrinsic::ConstAllocate => {
                let [size, align] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_allocate args are not provided".into(),
                    ));
                };
                let size = from_bytes!(usize, self.read_scalar(size.interval)?);
                let align = from_bytes!(usize, self.read_scalar(align.interval)?);
                let origin = Origin::ConstHeap { global: false };
                let result = self.allocate(size, align, origin, false)?;
                self.write_pointer(destination.addr, result)
            }
            Intrinsic::ConstDeallocate => {
                let [ptr, ..] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_deallocate args are not provided".into(),
                    ));
                };
                let ptr = self.read_pointer(ptr.interval.addr)?;
                let origin = Origin::ConstHeap { global: false };
                self.deallocate(ptr, origin, "const_deallocate")
            }
            Intrinsic::ConstMakeGlobal => {
                let [ptr] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_make_global args are not provided".into(),
                    ));
                };
                self.make_global(Address::from_bytes(ptr.get(self)?)?)?;
                destination.write_from_interval(self, ptr.interval)
            }
            Intrinsic::CallerLocation => {
                let site = self.caller_site(locals, span)?;
                let location = self.location_of(site, locals)?;
                destination.write_from_bytes(self, &location.to_bytes())
            }
            Intrinsic::BoxNew => {
                let ty = generic_args.type_at(0);
                let Some((size, align)) = self.size_align_of(ty, locals)? else {
                    not_supported!("unsized box initialization");
                };
                let addr = self.allocate(size, align, Origin::Allocator, false)?;
                self.copy_from_interval(addr.aligned(align), args[0].interval)?;
                self.write_pointer(destination.addr, addr)
            }
            Intrinsic::WriteBoxViaMove => {
                let [boxed, value] = args else {
                    return Err(MirEvalError::InternalError(
                        "write_box_via_move args are not provided".into(),
                    ));
                };
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "write_box_via_move generic arg is not provided".into(),
                    ));
                };
                let size = self.size_of_sized(ty, locals, "write_box_via_move value type")?;
                let Some(ptr) = boxed.get(self)?.get(..self.ptr_size()) else {
                    return Err(MirEvalError::InternalError(
                        "write_box_via_move box is smaller than a pointer".into(),
                    ));
                };
                let addr = Address::from_bytes(ptr)?;
                Interval { addr, size }.write_from_interval(self, value.interval)?;
                destination.write_from_interval(self, boxed.interval)
            }
            _ => return Ok(false),
        }
        .map(|()| true)
    }

    /// A float intrinsic generic over its float type, in the width of `width`.
    fn exec_float_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        width: FloatWidth,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
    ) -> Result<'db, ()> {
        match width {
            FloatWidth::F32 => {
                let result = self.float_intrinsic::<f32>(intrinsic, args)?;
                destination.write_from_bytes(self, &result.to_le_bytes())
            }
            FloatWidth::F64 => {
                let result = self.float_intrinsic::<f64>(intrinsic, args)?;
                destination.write_from_bytes(self, &result.to_le_bytes())
            }
            // FIXME(#17451): Add `f16` and `f128` intrinsics.
            FloatWidth::F16 | FloatWidth::F128 => {
                not_supported!("intrinsic {intrinsic} of `f{}`", width.bits())
            }
        }
    }

    /// The result of the float intrinsic `intrinsic` of `args`, computed in `F` as `std`
    /// rounds it.
    fn float_intrinsic<F: EvalFloat>(
        &self,
        intrinsic: Intrinsic,
        args: &[IntervalAndTy<'db>],
    ) -> Result<'db, F> {
        let float = |arg: &IntervalAndTy<'db>| -> Result<'db, F> {
            F::from_le(arg.get(self)?).ok_or_else(|| {
                MirEvalError::InternalError("mismatched size in constructing a float".into())
            })
        };
        Ok(match intrinsic {
            Intrinsic::Powf | Intrinsic::CopySign(_) => {
                let [arg1, arg2] = args else {
                    return Err(MirEvalError::InternalError(
                        "float intrinsic signature doesn't match fn (T, T) -> T".into(),
                    ));
                };
                let (arg1, arg2) = (float(arg1)?, float(arg2)?);
                if intrinsic == Intrinsic::Powf { arg1.powf(arg2) } else { arg1.copysign(arg2) }
            }
            Intrinsic::Powi => {
                let [arg1, arg2] = args else {
                    return Err(MirEvalError::InternalError(
                        "powi signature doesn't match fn (T, i32) -> T".into(),
                    ));
                };
                float(arg1)?.powi(from_bytes!(i32, arg2.get(self)?))
            }
            Intrinsic::Fma => {
                let [arg1, arg2, arg3] = args else {
                    return Err(MirEvalError::InternalError(
                        "fma signature doesn't match fn (T, T, T) -> T".into(),
                    ));
                };
                float(arg1)?.mul_add(float(arg2)?, float(arg3)?)
            }
            _ => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "float intrinsic signature doesn't match fn (T) -> T".into(),
                    ));
                };
                float(arg)?.unary(intrinsic).ok_or_else(|| {
                    MirEvalError::InternalError(
                        // alloc: error path
                        format!("{intrinsic} is not a float intrinsic of one argument").into(),
                    )
                })?
            }
        })
    }

    fn size_align_of_unsized(
        &mut self,
        ty: Ty<'db>,
        metadata: Interval,
        locals: &Locals<'db>,
    ) -> Result<'db, (usize, usize)> {
        Ok(match ty.kind() {
            TyKind::Str => (from_bytes!(usize, metadata.get(self)?), 1),
            TyKind::Slice(inner) => {
                let len = from_bytes!(usize, metadata.get(self)?);
                let (size, align) = self.size_align_of_sized(inner, locals, "slice inner type")?;
                (size * len, align)
            }
            TyKind::Dynamic(..) => self.size_align_of_sized(
                self.vtable_map.ty_of_bytes(metadata.get(self)?)?,
                locals,
                "dyn concrete type",
            )?,
            TyKind::Adt(adt_def, subst) => {
                let id = adt_def.def_id();
                let layout = self.layout_adt(id, subst)?;
                let id = match id {
                    AdtId::StructId(s) => s,
                    _ => not_supported!("unsized enum or union"),
                };
                let field_types = self.db.field_types(id.into());
                let last_field_ty = field_types
                    .iter()
                    .next_back()
                    .unwrap()
                    .1
                    .ty()
                    .instantiate(self.interner(), subst)
                    .skip_norm_wip();
                let sized_part_size =
                    layout.fields.offset(field_types.iter().count() - 1).bytes_usize();
                let sized_part_align = layout.align.bytes() as usize;
                let (unsized_part_size, unsized_part_align) =
                    self.size_align_of_unsized(last_field_ty, metadata, locals)?;
                let align = sized_part_align.max(unsized_part_align) as isize;
                let size = (sized_part_size + unsized_part_size) as isize;
                // Must add any necessary padding to `size`
                // (to make it a multiple of `align`) before returning it.
                //
                // Namely, the returned size should be, in C notation:
                //
                //   `size + ((size & (align-1)) ? align : 0)`
                //
                // emulated via the semi-standard fast bit trick:
                //
                //   `(size + (align-1)) & -align`
                let size = (size + (align - 1)) & (-align);
                (size as usize, align as usize)
            }
            _ => not_supported!("unsized type other than str, slice, struct and dyn"),
        })
    }

    /// An `unchecked_*` or `exact_div` of integers, as the bits of `lhs`'s type: what the
    /// operation gives where it is defined, and the undefined behavior rustc's const eval
    /// reports where it is not.
    fn unchecked_int(
        &self,
        intrinsic: Intrinsic,
        lhs: &IntervalAndTy<'db>,
        rhs: &IntervalAndTy<'db>,
    ) -> Result<'db, u128> {
        let ub = |what: String| Err(MirEvalError::UndefinedBehavior(what));
        let bits = lhs.interval.size as u32 * 8;
        let signed = |arg: &IntervalAndTy<'db>| matches!(arg.ty.kind(), TyKind::Int(_));
        let int = |arg: &IntervalAndTy<'db>| -> Result<'db, i128> {
            Ok(i128::from_le_bytes(pad16(arg.get(self)?, IsSigned::Yes)))
        };
        let uint = |arg: &IntervalAndTy<'db>| -> Result<'db, u128> {
            Ok(u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)))
        };
        if let Intrinsic::UncheckedShl | Intrinsic::UncheckedShr = intrinsic {
            let amount =
                if signed(rhs) { int(rhs)? } else { uint(rhs)?.min(i128::MAX as u128) as i128 };
            if amount < 0 || amount >= i128::from(bits) {
                return ub(format!("overflowing shift by {amount} in `{intrinsic}`")); // alloc: error path
            }
            return Ok(match (intrinsic, signed(lhs)) {
                (Intrinsic::UncheckedShl, _) => uint(lhs)? << amount,
                (_, true) => (int(lhs)? >> amount) as u128,
                _ => uint(lhs)? >> amount,
            });
        }
        let overflow = || format!("arithmetic overflow in `{intrinsic}`"); // alloc: error path
        let by_zero = || match intrinsic {
            Intrinsic::UncheckedRem => {
                "calculating the remainder with a divisor of zero".to_owned() // alloc: error path
            }
            _ => "dividing by zero".to_owned(), // alloc: error path
        };
        if signed(lhs) {
            let (a, b) = (int(lhs)?, int(rhs)?);
            let (min, max) = (i128::MIN >> (128 - bits), i128::MAX >> (128 - bits));
            let ans = match intrinsic {
                Intrinsic::UncheckedAdd => a.checked_add(b),
                Intrinsic::UncheckedSub => a.checked_sub(b),
                Intrinsic::UncheckedMul => a.checked_mul(b),
                _ if b == 0 => return ub(by_zero()),
                _ if a == min && b == -1 => return ub(overflow()),
                Intrinsic::ExactDiv if a % b != 0 => {
                    // alloc: error path
                    return ub(format!(
                        "exact_div: {a} cannot be divided by {b} without remainder"
                    ));
                }
                Intrinsic::UncheckedRem => Some(a % b),
                _ => Some(a / b),
            };
            match ans {
                Some(ans) if (min..=max).contains(&ans) => Ok(ans as u128),
                _ => ub(overflow()),
            }
        } else {
            let (a, b) = (uint(lhs)?, uint(rhs)?);
            let max = u128::MAX >> (128 - bits);
            let ans = match intrinsic {
                Intrinsic::UncheckedAdd => a.checked_add(b),
                Intrinsic::UncheckedSub => a.checked_sub(b),
                Intrinsic::UncheckedMul => a.checked_mul(b),
                _ if b == 0 => return ub(by_zero()),
                Intrinsic::ExactDiv if a % b != 0 => {
                    // alloc: error path
                    return ub(format!(
                        "exact_div: {a} cannot be divided by {b} without remainder"
                    ));
                }
                Intrinsic::UncheckedRem => Some(a % b),
                _ => Some(a / b),
            };
            match ans {
                Some(ans) if ans <= max => Ok(ans),
                _ => ub(overflow()),
            }
        }
    }

    fn exec_atomic_intrinsic(
        &mut self,
        op: AtomicOp,
        orders: AtomicOrders,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
        locals: &Locals<'db>,
        _span: MirSpan,
    ) -> Result<'db, ()> {
        // One thread runs at a time, so an atomic is a read and a write of its bytes. The
        // machine sees them as atomic accesses of the orderings, which change nothing else.
        let (ord, failure) = match M::CHECKS {
            true => self.atomic_orders(op, orders, generic_args)?,
            false => (AtomicOrd::SeqCst, AtomicOrd::SeqCst),
        };
        if let AtomicOp::Fence | AtomicOp::SingleThreadFence = op {
            if M::CHECKS {
                self.machine.fence(ord, op == AtomicOp::SingleThreadFence)?;
            }
            return Ok(());
        }

        let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
            return Err(MirEvalError::InternalError(
                "atomic intrinsic generic arg is not provided".into(),
            ));
        };
        let Some(arg0) = args.first() else {
            return Err(MirEvalError::InternalError(
                "atomic intrinsic arg0 is not provided".into(),
            ));
        };
        let size = self.size_of_sized(ty, locals, "atomic intrinsic type arg")?;
        // An atomic access is aligned to its size.
        let addr = self.read_pointer(arg0.interval.addr)?.aligned(size);
        let access = |rmw| Access::Atomic { ord, rmw, cxchg_failed: false };
        if op == AtomicOp::Load {
            if !M::CHECKS {
                return destination.write_from_interval(self, Interval::new(addr, size));
            }
            let (latest, shadow) = self.atomic_read(addr, size, access(false))?;
            return match self.machine.atomic_load(self.ptr(addr), size as u64, ord, &latest)? {
                Some(bytes) => {
                    self.write_shadowed(destination.addr, &bytes, &M::Shadow::init(bytes.len()))
                }
                None => self.write_shadowed(destination.addr, &latest, &shadow),
            };
        }
        let Some(arg1) = args.get(1) else {
            return Err(MirEvalError::InternalError(
                "atomic intrinsic arg1 is not provided".into(),
            ));
        };
        let (value, value_shadow) = self.read_shadowed(arg1.interval.addr, arg1.interval.size)?;
        let value = Word::from_slice(value);
        if op == AtomicOp::Store {
            return self.atomic_write(addr, &value, &value_shadow, ord, false);
        }

        if let AtomicOp::Cxchg | AtomicOp::CxchgWeak = op {
            let Some(arg2) = args.get(2) else {
                return Err(MirEvalError::InternalError(
                    "atomic intrinsic arg2 is not provided".into(),
                ));
            };
            // The expected value is compared as a number.
            if M::CHECKS {
                self.machine.require_init(&value_shadow, 0..value.len())?;
            }
            let (new, new_shadow) = self.read_shadowed(arg2.interval.addr, arg2.interval.size)?;
            let new = Word::from_slice(new);
            // The latest store, compared before the machine is told: it sees one read, the
            // read of the read-modify-write when the exchange happens and else the failure's
            // (Miri reads the old value unchecked too).
            let old = Word::from_slice(self.peek_memory(addr, size)?);
            let exchanged = old == value;
            let old_shadow = match M::CHECKS {
                true => {
                    let read = match exchanged {
                        true => access(true),
                        false => Access::Atomic { ord: failure, rmw: true, cxchg_failed: true },
                    };
                    let shadow =
                        self.machine.read(self.ptr(addr), size as u64, addr.align(), read)?;
                    self.machine.require_init(&shadow, 0..size)?;
                    shadow
                }
                false => M::Shadow::default(),
            };
            if exchanged {
                self.atomic_write(addr, &new, &new_shadow, ord, true)?;
            }
            let result_ty = Ty::new_tup_from_iter(
                self.interner(),
                [ty, Ty::new_bool(self.interner())].into_iter(),
            );
            let layout = self.layout(result_ty)?;
            let (at, flag) =
                (layout.fields.offset(0).bytes_usize(), layout.fields.offset(1).bytes_usize());
            let len = layout.size.bytes_usize();
            if len < (at + size).max(flag + 1) {
                return Err(MirEvalError::InternalError(
                    "the result of a compare-exchange is smaller than its fields".into(),
                ));
            }
            let mut result = smallvec::SmallVec::<[u8; 32]>::from_elem(0, len);
            result[at..at + size].copy_from_slice(&old);
            result[flag] = u8::from(exchanged);
            let mut shadow = M::Shadow::init(result.len());
            shadow.put(at, &old_shadow);
            return self.write_shadowed(destination.addr, &result, &shadow);
        }

        let computes = op != AtomicOp::Xchg;
        if M::CHECKS && computes {
            self.machine.require_init(&value_shadow, 0..value.len())?;
        }
        let (old, old_shadow) = self.atomic_read(addr, size, access(true))?;
        let (new, new_shadow) = match computes {
            false => (value, value_shadow),
            true => {
                if M::CHECKS {
                    self.machine.require_init(&old_shadow, 0..size)?;
                }
                let signed = matches!(ty.kind(), TyKind::Int(_));
                let new = rmw_value(op, &old, &value, signed)?;
                // Arithmetic on an atomic pointer keeps the old pointer's provenance.
                let shadow = match ty.kind() {
                    TyKind::RawPtr(..) => M::Shadow::pointer(size, old_shadow.prov_at(0, size)),
                    _ => M::Shadow::init(size),
                };
                (Word::from_slice(&new.to_le_bytes()[..size]), shadow)
            }
        };
        self.atomic_write(addr, &new, &new_shadow, ord, true)?;
        self.write_shadowed(destination.addr, &old, &old_shadow)
    }

    /// The machine's orderings of an atomic: its ordering, and a compare-exchange's failure
    /// ordering (its ordering, for any other atomic).
    fn atomic_orders(
        &self,
        op: AtomicOp,
        orders: AtomicOrders,
        generic_args: GenericArgs<'db>,
    ) -> Result<'db, (AtomicOrd, AtomicOrd)> {
        let Some((ord, failure)) = atomic_orderings(self.db, op, orders, generic_args) else {
            not_supported!("atomic `{op:?}` of orderings that are not known `AtomicOrdering`s")
        };
        let failure = match (op, failure) {
            (AtomicOp::Cxchg | AtomicOp::CxchgWeak, None) => {
                not_supported!("atomic `{op:?}` with no failure ordering")
            }
            (_, failure) => failure.unwrap_or(ord),
        };
        Ok((atomic_ord(ord), atomic_ord(failure)))
    }

    /// The bytes an atomic reads at `addr` and what they hold: one machine `read` of `access`.
    fn atomic_read(
        &self,
        addr: Address,
        size: usize,
        access: Access,
    ) -> Result<'db, (Word, M::Shadow)> {
        let shadow = match M::CHECKS {
            true => self.machine.read(self.ptr(addr), size as u64, addr.align(), access)?,
            false => M::Shadow::default(),
        };
        Ok((Word::from_slice(self.peek_memory(addr, size)?), shadow))
    }

    /// An atomic's write of `bytes` to `addr`: one machine `write`, the bytes, and right after
    /// them the machine's `atomic_store` with the bytes they replaced.
    fn atomic_write(
        &mut self,
        addr: Address,
        bytes: &[u8],
        shadow: &M::Shadow,
        ord: AtomicOrd,
        rmw: bool,
    ) -> Result<'db, ()> {
        if !M::CHECKS {
            self.poke_memory(addr, bytes.len())?.copy_from_slice(bytes);
            return Ok(());
        }
        let ptr = self.ptr(addr);
        let access = Access::Atomic { ord, rmw, cxchg_failed: false };
        self.machine.write(ptr, bytes.len() as u64, addr.align(), access, shadow)?;
        let old = Word::from_slice(self.peek_memory(addr, bytes.len())?);
        self.poke_memory(addr, bytes.len())?.copy_from_slice(bytes);
        self.machine.atomic_store(ptr, ord, bytes, &old)
    }

    /// `addr` accessed as a value of `ty`, at its alignment, which only the machine checks.
    fn aligned_to(&self, addr: Address, ty: Ty<'db>) -> Result<'db, Address> {
        if !M::CHECKS {
            return Ok(addr);
        }
        Ok(addr.aligned(self.layout(ty)?.align.bytes() as usize))
    }

    /// The value of `size` bytes at `src` written to `dst`: a read of it, then a write of what
    /// it holds, provenance included.
    fn move_value(&mut self, dst: Address, src: Address, size: usize) -> Result<'db, ()> {
        if !M::CHECKS {
            return self.copy_from_interval(dst, Interval::new(src, size));
        }
        let (bytes, shadow) = self.read_shadowed(src, size)?;
        // alloc: only under the checker; `Plain` copies in place.
        let bytes = bytes.to_vec();
        self.write_shadowed(dst, &bytes, &shadow)
    }

    /// `count` values of `ty` from the pointer in `src` to the one in `dst`: `copy` and
    /// `volatile_copy_memory` (`overlapping`), `copy_nonoverlapping` and its volatile one.
    fn copy_values(
        &mut self,
        dst: &IntervalAndTy<'db>,
        src: &IntervalAndTy<'db>,
        count: &IntervalAndTy<'db>,
        ty: Ty<'db>,
        overlapping: bool,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let (size, align) = self.size_align_of_sized(ty, locals, "the type of a copy")?;
        let src = self.read_pointer(src.interval.addr)?;
        let dst = self.read_pointer(dst.interval.addr)?;
        let count = from_bytes!(usize, self.read_scalar(count.interval)?);
        let Some(size) = count.checked_mul(size) else {
            return Err(MirEvalError::UndefinedBehavior(
                // alloc: error path.
                "overflow computing the total size of a copy".to_owned(),
            ));
        };
        self.copy_memory(dst, src, size, align, overlapping)
    }

    /// `size` bytes from `src` to `dst`, both accessed at alignment `align`, which the machine
    /// sees as one copy: `overlapping` for `memmove` and `copy`, not for `memcpy` and
    /// `copy_nonoverlapping`, where the machine fails if they overlap.
    fn copy_memory(
        &mut self,
        dst: Address,
        src: Address,
        size: usize,
        align: usize,
        overlapping: bool,
    ) -> Result<'db, ()> {
        let (dst, src) = (dst.aligned(align), src.aligned(align));
        if !M::CHECKS || overlapping {
            return self.copy_from_interval(dst, Interval::new(src, size));
        }
        if size == 0 {
            return Ok(());
        }
        self.machine.copy(self.ptr(dst), self.ptr(src), size as u64, align as u64, false)?;
        self.move_memory(dst, Interval::new(src, size))
    }

    /// The bytes of the `TypeId` of `of`, made when it is the first one asked for.
    fn type_id(&mut self, of: TypeIdOf<'db>) -> [u8; 16] {
        let id = self.type_ids.iter().position(|&it| it == of).unwrap_or_else(|| {
            self.type_ids.push(of);
            self.type_ids.len() - 1
        });
        (id as u128 + 1).to_le_bytes()
    }

    /// What the `TypeId` in `arg` stands for.
    fn type_id_entry(&self, arg: &IntervalAndTy<'db>) -> Result<'db, Option<&TypeIdOf<'db>>> {
        let id = from_bytes!(u128, self.read_scalar(arg.interval)?);
        Ok(id.checked_sub(1).and_then(|it| self.type_ids.get(it as usize)))
    }

    /// The type a `TypeId` in `arg` is of; a field's stands for no type of the program.
    fn type_id_of(&self, arg: &IntervalAndTy<'db>) -> Result<'db, Ty<'db>> {
        match self.type_id_entry(arg)? {
            Some(&TypeIdOf::Ty(ty)) => Ok(ty),
            Some(TypeIdOf::Field { .. }) => not_supported!("reflection of a field's `TypeId`"),
            None => not_supported!("a `TypeId` this evaluation did not make"),
        }
    }

    /// `ty` as a `TypeId` is of it: its aliases normalized and its lifetimes erased, which
    /// a `TypeId` does not tell apart. Those of a binder in it are numbered in their order,
    /// for two spellings of one such type to have one id.
    fn type_id_ty(&self, ty: Ty<'db>) -> Result<'db, Ty<'db>> {
        let ty = crate::mir::monomorphization::normalized_ty(self.db, ty, self.param_env)
            // alloc: error path
            .map_err(|e| MirEvalError::NotSupported(format!("a `TypeId` of {ty:?}: {e:?}")))?;
        Ok(crate::mir::erase_and_anonymize_regions(self.interner(), ty))
    }

    /// Where `caller_location` asked at `span` of the body being run is: as rustc passes each
    /// `#[track_caller]` function the place it is called from, the call of the innermost
    /// function up the stack that does not track its caller.
    fn caller_site(
        &self,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, (MirSpan, InferBodyId<'db>)> {
        let tracks =
            |callee: Option<FunctionId>| callee.is_some_and(|it| track_caller(self.db, it));
        let frames = self.running.into_iter();
        let mut frames = frames.chain(self.code_stack.iter().rev().map(|it| (it.callee, it.span)));
        let mut site = (span, locals.body.owner);
        loop {
            let Some((callee, call)) = frames.next() else {
                // The frames above are of an evaluation this one was started from.
                if tracks(site.1.as_function()) {
                    not_supported!("the caller's location past a call the evaluator nests");
                }
                return Ok(site);
            };
            if !tracks(callee) {
                return Ok(site);
            }
            if call.0 == MirSpan::Unknown {
                not_supported!("the caller's location of a call with no place");
            }
            site = call;
        }
    }

    /// A `&'static Location<'static>` of `span` in `owner`, under the name the build gives its
    /// file, which rustc's `Location::file` says.
    fn location_of(
        &mut self,
        (span, owner): (MirSpan, InferBodyId<'db>),
        locals: &Locals<'db>,
    ) -> Result<'db, Address> {
        let InferBodyId::DefWithBodyId(body) = owner else {
            not_supported!("the caller's location in an anonymous constant");
        };
        let Some((file, line, col)) = *source_location_in_file(self.db, body, span) else {
            not_supported!("the caller's location: no place for {span:?}");
        };
        let Some(name) = source_path(self.db, file) else {
            not_supported!("the caller's location: the build did not name its file");
        };
        let Some(id) = self.lang_items().PanicLocation else {
            not_supported!("no `Location` in core");
        };
        // The name is followed by a NUL that its length leaves out (`Location::file_as_c_str`).
        let addr = self.heap_allocate(name.len() + 1, 1)?;
        self.write_memory(addr, name.as_bytes())?;
        self.write_memory(addr.offset(name.len()), &[0])?;
        let args = GenericArgs::new_from_slice(&[Region::new_erased(self.interner()).into()]);
        let (size, layout, tag) = self.layout_of_variant(id.into(), args, locals)?;
        let mut file_field = None;
        let fields = id.fields(self.db).fields().iter().enumerate().map(|(i, (_, field))| {
            Ok(IntervalOrOwned::Owned(match field.name.as_str() {
                "filename" | "file" => {
                    file_field = Some(i);
                    let mut it = OwnedBytes::from_slice(&addr.to_bytes());
                    it.extend_from_slice(&super::word_bytes(name.len()));
                    it
                }
                "line" => OwnedBytes::from_slice(&line.to_le_bytes()),
                "col" => OwnedBytes::from_slice(&col.to_le_bytes()),
                "_filename" => OwnedBytes::new(),
                other => not_supported!("a field `{other}` of `Location`"),
            }))
        });
        let fields = fields.collect::<Result<'db, Vec<_>>>()?;
        let bytes = self.construct_with_layout(size, &layout, tag, fields.into_iter())?;
        let location = self.heap_allocate(size, layout.align.abi.bytes() as usize)?;
        self.write_memory(location, &bytes)?;
        // The file name's pointer again, with the provenance of the name's allocation.
        if M::CHECKS && let Some(i) = file_field {
            let at = location.offset(layout.fields.offset(i).bytes_usize());
            self.write_pointer(at, addr)?;
        }
        Ok(location)
    }

    /// `text` as a `&'static str` in `destination`.
    fn write_str(&mut self, destination: Interval, text: &str) -> Result<'db, ()> {
        let addr = self.heap_allocate(text.len(), 1)?;
        self.write_memory(addr, text.as_bytes())?;
        self.write_pointer(destination.addr, addr)?;
        destination
            .slice(self.ptr_size()..2 * self.ptr_size())
            .write_from_bytes(self, &super::word_bytes(text.len()))
    }

    /// An `Option<inner>` in `destination`: `Some` of the bytes of a value, or `None`.
    fn write_option(
        &mut self,
        destination: Interval,
        inner: Ty<'db>,
        value: Option<impl AsRef<[u8]>>,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let variant = if value.is_some() {
            self.lang_items().OptionSome
        } else {
            self.lang_items().OptionNone
        };
        let Some(variant) = variant else { not_supported!("no `Option` in core") };
        let args = GenericArgs::new_from_slice(&[inner.into()]);
        let (size, layout, tag) = self.layout_of_variant(variant.into(), args, locals)?;
        let fields = value.map(|it| IntervalOrOwned::Owned(OwnedBytes::from_slice(it.as_ref())));
        let bytes = self.construct_with_layout(size, &layout, tag, fields.into_iter())?;
        destination.write_from_bytes(self, &bytes)
    }
}

/// The width of the float a float intrinsic generic over its type is of: its first argument's.
fn float_width<'db>(intrinsic: Intrinsic, args: &[IntervalAndTy<'db>]) -> Result<'db, FloatWidth> {
    use rustc_type_ir::FloatTy;
    Ok(match args.first().map(|it| it.ty.kind()) {
        Some(TyKind::Float(FloatTy::F16)) => FloatWidth::F16,
        Some(TyKind::Float(FloatTy::F32)) => FloatWidth::F32,
        Some(TyKind::Float(FloatTy::F64)) => FloatWidth::F64,
        Some(TyKind::Float(FloatTy::F128)) => FloatWidth::F128,
        _ => {
            return Err(MirEvalError::InternalError(
                // alloc: error path
                format!("{intrinsic} of an argument that is not a float").into(),
            ));
        }
    })
}

/// The bytes of an atomic's value: at most a `u128`'s.
type Word = smallvec::SmallVec<[u8; 16]>;

/// The type `intrinsic` is generic over: its first generic argument.
fn type_arg<'db>(generic_args: GenericArgs<'db>, intrinsic: Intrinsic) -> Result<'db, Ty<'db>> {
    match generic_args.as_slice().first().and_then(|it| it.ty()) {
        Some(ty) => Ok(ty),
        None => Err(MirEvalError::InternalError(
            // alloc: error path.
            format!("{intrinsic} generic arg is not provided").into(),
        )),
    }
}

/// The value the atomic read-modify-write `op` stores, of the old value `lhs` and the operand
/// `rhs`, as many bytes as `lhs`; `max` and `min` compare as `signed` numbers.
fn rmw_value<'db>(op: AtomicOp, lhs: &[u8], rhs: &[u8], signed: bool) -> Result<'db, u128> {
    let size = lhs.len();
    let int = |bytes: &[u8], sign: bool| {
        u128::from_le_bytes(pad16(&bytes[..size.min(bytes.len())], sign.into()))
    };
    let (l, r) = (int(lhs, false), int(rhs, false));
    Ok(match op {
        AtomicOp::Xadd => l.wrapping_add(r),
        AtomicOp::Xsub => l.wrapping_sub(r),
        AtomicOp::And => l & r,
        AtomicOp::Nand => !(l & r),
        AtomicOp::Or => l | r,
        AtomicOp::Xor => l ^ r,
        AtomicOp::Umax => l.max(r),
        AtomicOp::Umin => l.min(r),
        AtomicOp::Max | AtomicOp::Min => {
            let (l, r) = (int(lhs, signed) as i128, int(rhs, signed) as i128);
            (if op == AtomicOp::Max { l.max(r) } else { l.min(r) }) as u128
        }
        AtomicOp::Xchg
        | AtomicOp::Cxchg
        | AtomicOp::CxchgWeak
        | AtomicOp::Load
        | AtomicOp::Store
        | AtomicOp::Fence
        | AtomicOp::SingleThreadFence => {
            return Err(MirEvalError::InternalError(
                // alloc: error path.
                format!("atomic `{op:?}` taken for an arithmetic read-modify-write").into(),
            ));
        }
    })
}

/// core's `AtomicOrdering` as the machine has it.
fn atomic_ord(ord: AtomicOrdering) -> AtomicOrd {
    match ord {
        AtomicOrdering::Relaxed => AtomicOrd::Relaxed,
        AtomicOrdering::Acquire => AtomicOrd::Acquire,
        AtomicOrdering::Release => AtomicOrd::Release,
        AtomicOrdering::AcqRel => AtomicOrd::AcqRel,
        AtomicOrdering::SeqCst => AtomicOrd::SeqCst,
    }
}

/// A float the evaluator computes float intrinsics in, each in its own width.
trait EvalFloat: Copy {
    fn from_le(bytes: &[u8]) -> Option<Self>;
    fn powf(self, other: Self) -> Self;
    fn powi(self, exponent: i32) -> Self;
    fn copysign(self, sign: Self) -> Self;
    fn mul_add(self, a: Self, b: Self) -> Self;
    /// The float intrinsic of one argument `intrinsic` of `self`, if it is one.
    fn unary(self, intrinsic: Intrinsic) -> Option<Self>;
}

macro_rules! eval_float {
    ($ty:ident) => {
        impl EvalFloat for $ty {
            fn from_le(bytes: &[u8]) -> Option<Self> {
                bytes.try_into().ok().map($ty::from_le_bytes)
            }
            fn powf(self, other: Self) -> Self {
                $ty::powf(self, other)
            }
            fn powi(self, exponent: i32) -> Self {
                $ty::powi(self, exponent)
            }
            fn copysign(self, sign: Self) -> Self {
                $ty::copysign(self, sign)
            }
            fn mul_add(self, a: Self, b: Self) -> Self {
                $ty::mul_add(self, a, b)
            }
            fn unary(self, intrinsic: Intrinsic) -> Option<Self> {
                Some(match intrinsic {
                    Intrinsic::Sqrt => self.sqrt(),
                    Intrinsic::Sin => self.sin(),
                    Intrinsic::Cos => self.cos(),
                    Intrinsic::Exp => self.exp(),
                    Intrinsic::Exp2 => self.exp2(),
                    Intrinsic::Log => self.ln(),
                    Intrinsic::Log10 => self.log10(),
                    Intrinsic::Log2 => self.log2(),
                    Intrinsic::Floor => self.floor(),
                    Intrinsic::Ceil => self.ceil(),
                    Intrinsic::Trunc => self.trunc(),
                    Intrinsic::Round => self.round(),
                    Intrinsic::RoundTiesEven => self.round_ties_even(),
                    _ => return None,
                })
            }
        }
    };
}

eval_float!(f32);
eval_float!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes `op` stores, as the evaluator writes them: as many as `lhs`.
    fn stored(op: AtomicOp, lhs: &[u8], rhs: &[u8], signed: bool) -> Option<Vec<u8>> {
        let value = rmw_value(op, lhs, rhs, signed).ok()?;
        Some(value.to_le_bytes()[..lhs.len()].to_vec())
    }

    #[test]
    fn rmw_arithmetic_wraps_at_the_atomic_size() {
        assert_eq!(stored(AtomicOp::Xadd, &[0xff], &[1], false), Some(vec![0]));
        assert_eq!(stored(AtomicOp::Xsub, &[0], &[1], false), Some(vec![0xff]));
        let (lhs, rhs) = (u32::MAX.to_le_bytes(), 2u32.to_le_bytes());
        assert_eq!(stored(AtomicOp::Xadd, &lhs, &rhs, false), Some(1u32.to_le_bytes().to_vec()));
    }

    #[test]
    fn rmw_bitwise() {
        assert_eq!(stored(AtomicOp::And, &[0b1100], &[0b1010], false), Some(vec![0b1000]));
        assert_eq!(stored(AtomicOp::Or, &[0b1100], &[0b1010], false), Some(vec![0b1110]));
        assert_eq!(stored(AtomicOp::Xor, &[0b1100], &[0b1010], false), Some(vec![0b0110]));
        assert_eq!(stored(AtomicOp::Nand, &[0b1100], &[0b1010], false), Some(vec![!0b1000]));
    }

    #[test]
    fn rmw_max_and_min_by_signedness() {
        // -1 and 1 as `i8`, or 255 and 1 as `u8`.
        assert_eq!(stored(AtomicOp::Max, &[0xff], &[1], true), Some(vec![1]));
        assert_eq!(stored(AtomicOp::Min, &[0xff], &[1], true), Some(vec![0xff]));
        assert_eq!(stored(AtomicOp::Max, &[0xff], &[1], false), Some(vec![0xff]));
        assert_eq!(stored(AtomicOp::Umax, &[0xff], &[1], true), Some(vec![0xff]));
        assert_eq!(stored(AtomicOp::Umin, &[0xff], &[1], true), Some(vec![1]));
        let (lhs, rhs) = ((-5i32).to_le_bytes(), 3i32.to_le_bytes());
        assert_eq!(stored(AtomicOp::Min, &lhs, &rhs, true), Some(lhs.to_vec()));
        assert_eq!(stored(AtomicOp::Max, &lhs, &rhs, true), Some(rhs.to_vec()));
    }

    #[test]
    fn rmw_operand_wider_than_the_atomic() {
        // `atomic_xadd::<*mut T, usize>` on a narrower target, and the like: the operand's
        // bytes past the atomic's are not part of it.
        assert_eq!(stored(AtomicOp::Xadd, &[1, 0], &[1, 0, 0xff, 0xff], false), Some(vec![2, 0]));
    }

    #[test]
    fn rmw_refuses_what_is_not_arithmetic() {
        use AtomicOp::*;
        for op in [Xchg, Cxchg, CxchgWeak, Load, Store, Fence, SingleThreadFence] {
            assert_eq!(stored(op, &[0], &[0], false), None, "{op:?}");
        }
    }

    #[test]
    fn orderings_map_one_to_one() {
        let all = [
            (AtomicOrdering::Relaxed, AtomicOrd::Relaxed),
            (AtomicOrdering::Acquire, AtomicOrd::Acquire),
            (AtomicOrdering::Release, AtomicOrd::Release),
            (AtomicOrdering::AcqRel, AtomicOrd::AcqRel),
            (AtomicOrdering::SeqCst, AtomicOrd::SeqCst),
        ];
        for (ordering, ord) in all {
            assert_eq!(atomic_ord(ordering), ord);
        }
        // A suffix intrinsic's orderings, as `atomic_orderings` reads them, map the same way.
        let (success, failure) = AtomicOrders::AcqRelAcquire.named().unwrap();
        assert_eq!(atomic_ord(success), AtomicOrd::AcqRel);
        assert_eq!(failure.map(atomic_ord), Some(AtomicOrd::Acquire));
    }
}
