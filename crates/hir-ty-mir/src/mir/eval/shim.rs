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
use stdx::never;

use crate::{
    InferBodyId,
    drop::{DropGlue, has_drop_glue},
    mir::eval::{
        Address, AdtId, Arc, Evaluator, FunctionId, GenericArgs, HasModule, Interval,
        IntervalAndTy, IntervalOrOwned, IsSigned, ItemContainerId, Layout, Locals, Lookup,
        MirEvalError, MirSpan, Mutability, Origin, Result, Ty, TyKind, TypeIdOf, VTableOfBytes,
        VariantId, from_bytes, not_supported, pad16, try_const_usize,
    },
    mir::location::{source_location_in_file, source_path, track_caller},
    next_solver::Region,
};

mod arch;
mod reflection;
mod simd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalLangItem {
    BeginPanic,
    SliceLen,
    DropInPlace,
}

impl<'db> Evaluator<'db> {
    pub(super) fn detect_and_exec_special_function(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        locals: &Locals<'db>,
        destination: Interval,
        span: MirSpan,
    ) -> Result<'db, bool> {
        if self.not_special_fn_cache.borrow().contains(&def) {
            return Ok(false);
        }

        let function_data = FunctionSignature::of(self.db, def);
        let attrs = AttrFlags::query(self.db, def.into());
        let is_intrinsic = FunctionSignature::is_intrinsic(self.db, def);

        if is_intrinsic {
            return self.exec_intrinsic(
                function_data.name.as_str(),
                args,
                generic_args,
                destination,
                locals,
                span,
                !function_data.has_body()
                    || attrs.contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN),
            );
        }
        if let ItemContainerId::ExternBlockId(_) = def.lookup(self.db).container
            && let Some(link_name) = AttrFlags::link_name(self.db, def.into())
            && let Some(name) = link_name.as_str().strip_prefix("llvm.")
        {
            self.exec_llvm_intrinsic(name, args, destination)?;
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
        // `PanicFmt` is redirected to `ConstPanicFmt`
        if Some(def) == self.lang_items().PanicFmt {
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
                    None,
                    span,
                )?;
            }
        }
        Ok(())
    }

    fn exec_clone_for_fields(
        &mut self,
        ty_iter: impl Iterator<Item = Ty<'db>>,
        layout: Arc<Layout>,
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
                let size = from_bytes!(usize, size.get(self)?);
                let align = from_bytes!(usize, align.get(self)?);
                let result = self.allocate(size, align, Origin::Allocator)?;
                destination.write_from_bytes(self, &result.to_bytes())?;
            }
            _ if alloc_fn.contains(AttrFlags::RUSTC_DEALLOCATOR) => {
                let [ptr, ..] = args else {
                    return Err(MirEvalError::InternalError(
                        "rustc_deallocator args are not provided".into(),
                    ));
                };
                let ptr = Address::from_bytes(ptr.get(self)?)?;
                self.deallocate(ptr, Origin::Allocator, "__rust_dealloc")?;
            }
            _ if alloc_fn.contains(AttrFlags::RUSTC_REALLOCATOR) => {
                let [ptr, _old_size, _align, new_size] = args else {
                    return Err(MirEvalError::InternalError(
                        "rustc_allocator args are not provided".into(),
                    ));
                };
                let new_size = from_bytes!(usize, new_size.get(self)?);
                let ptr = Address::from_bytes(ptr.get(self)?)?;
                let result = self.reallocate(ptr, new_size)?;
                destination.write_from_bytes(self, &result.to_bytes())?;
            }
            _ => not_supported!("unknown alloc function"),
        }
        Ok(())
    }

    fn detect_lang_function(&self, def: FunctionId) -> Option<EvalLangItem> {
        use EvalLangItem::*;
        let lang_items = self.lang_items();
        let attrs = AttrFlags::query(self.db, def.into());

        if attrs.contains(AttrFlags::RUSTC_CONST_PANIC_STR) {
            // `#[rustc_const_panic_str]` is treated like `lang = "begin_panic"` by rustc CTFE.
            return Some(BeginPanic);
        }

        // We want to execute these functions with special logic
        // `PanicFmt` is not detected here as it's redirected later.
        if let Some((_, candidate)) = [
            (lang_items.BeginPanic, BeginPanic),
            (lang_items.SliceLen, SliceLen),
            (lang_items.DropInPlace, DropInPlace),
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
                let mut arg = args
                    .next()
                    .ok_or(MirEvalError::InternalError(
                        "argument of BeginPanic is not provided".into(),
                    ))?
                    .clone();
                while let TyKind::Ref(_, ty, _) = arg.ty.kind() {
                    if ty.is_str() {
                        let (pointee, metadata) = arg.interval.get(self)?.split_at(self.ptr_size());
                        let len = from_bytes!(usize, metadata);

                        return {
                            Err(MirEvalError::Panic(
                                std::str::from_utf8(
                                    self.read_memory(Address::from_bytes(pointee)?, len)?,
                                )
                                .unwrap()
                                .to_owned(),
                            ))
                        };
                    }
                    let size = self.size_of_sized(ty, locals, "begin panic arg")?;
                    let pointee = arg.interval.get(self)?;
                    arg = IntervalAndTy {
                        interval: Interval::new(Address::from_bytes(pointee)?, size),
                        ty,
                    };
                }
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
                let arg = arg.interval.get(self)?.to_owned();
                self.run_drop_glue_deep(
                    ty,
                    locals,
                    Address::from_bytes(&arg[0..self.ptr_size()])?,
                    &arg[self.ptr_size()..],
                    span,
                )?;
                Ok(vec![])
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
        for i in 0..size {
            let rand_byte = self.random_state.rand_u64() as u8;
            self.write_memory(addr.offset(i), &[rand_byte])?;
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
        match as_str {
            "memcmp" => {
                let [ptr1, ptr2, size] = args else {
                    return Err(MirEvalError::InternalError("memcmp args are not provided".into()));
                };
                let addr1 = Address::from_bytes(ptr1.get(self)?)?;
                let addr2 = Address::from_bytes(ptr2.get(self)?)?;
                let size = from_bytes!(usize, size.get(self)?);
                let slice1 = self.read_memory(addr1, size)?;
                let slice2 = self.read_memory(addr2, size)?;
                let r: i128 = match slice1.cmp(slice2) {
                    cmp::Ordering::Less => -1,
                    cmp::Ordering::Equal => 0,
                    cmp::Ordering::Greater => 1,
                };
                destination.write_from_bytes(self, &r.to_le_bytes()[..destination.size])
            }
            "write" => {
                let [fd, ptr, len] = args else {
                    return Err(MirEvalError::InternalError(
                        "libc::write args are not provided".into(),
                    ));
                };
                let fd = u128::from_le_bytes(pad16(fd.get(self)?, IsSigned::No));
                let interval = Interval {
                    addr: Address::from_bytes(ptr.get(self)?)?,
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
            "pthread_key_create" => {
                let key = self.thread_local_storage.create_key();
                let Some(arg0) = args.first() else {
                    return Err(MirEvalError::InternalError(
                        "pthread_key_create arg0 is not provided".into(),
                    ));
                };
                let arg0_addr = Address::from_bytes(arg0.get(self)?)?;
                let key_ty = if let Some((ty, ..)) = arg0.ty.as_reference_or_ptr() {
                    ty
                } else {
                    return Err(MirEvalError::InternalError(
                        "pthread_key_create arg0 is not a pointer".into(),
                    ));
                };
                let arg0_interval = Interval::new(
                    arg0_addr,
                    self.size_of_sized(key_ty, locals, "pthread_key_create key arg")?,
                );
                arg0_interval.write_from_bytes(self, &key.to_le_bytes()[0..arg0_interval.size])?;
                // return 0 as success
                destination.write_from_bytes(self, &0u64.to_le_bytes()[0..destination.size])?;
                Ok(())
            }
            "pthread_getspecific" => {
                let Some(arg0) = args.first() else {
                    return Err(MirEvalError::InternalError(
                        "pthread_getspecific arg0 is not provided".into(),
                    ));
                };
                let key = from_bytes!(usize, &pad16(arg0.get(self)?, IsSigned::No)[0..8]);
                let value = self.thread_local_storage.get_key(key)?;
                destination.write_from_bytes(self, &value.to_le_bytes()[0..destination.size])?;
                Ok(())
            }
            "pthread_setspecific" => {
                let Some(arg0) = args.first() else {
                    return Err(MirEvalError::InternalError(
                        "pthread_setspecific arg0 is not provided".into(),
                    ));
                };
                let key = from_bytes!(usize, &pad16(arg0.get(self)?, IsSigned::No)[0..8]);
                let Some(arg1) = args.get(1) else {
                    return Err(MirEvalError::InternalError(
                        "pthread_setspecific arg1 is not provided".into(),
                    ));
                };
                let value = from_bytes!(u128, pad16(arg1.get(self)?, IsSigned::No));
                self.thread_local_storage.set_key(key, value)?;
                // return 0 as success
                destination.write_from_bytes(self, &0u64.to_le_bytes()[0..destination.size])?;
                Ok(())
            }
            "pthread_key_delete" => {
                // we ignore this currently
                // return 0 as success
                destination.write_from_bytes(self, &0u64.to_le_bytes()[0..destination.size])?;
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
            "sched_getaffinity" => {
                let [_pid, _set_size, set] = args else {
                    return Err(MirEvalError::InternalError(
                        "sched_getaffinity args are not provided".into(),
                    ));
                };
                let set = Address::from_bytes(set.get(self)?)?;
                // Only enable core 0 (we are single threaded anyway), which is bitset 0x0000001
                self.write_memory(set, &[1])?;
                // return 0 as success
                self.write_memory_using_ref(destination.addr, destination.size)?.fill(0);
                Ok(())
            }
            "getenv" => {
                let [name] = args else {
                    return Err(MirEvalError::InternalError("getenv args are not provided".into()));
                };
                let mut name_buf = vec![];
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
                    String::from_utf8_lossy(&name_buf)
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
            _ => not_supported!("unknown external function {as_str}"),
        }
    }

    fn exec_intrinsic(
        &mut self,
        name: &str,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
        locals: &Locals<'db>,
        span: MirSpan,
        needs_override: bool,
    ) -> Result<'db, bool> {
        if let Some(name) = name.strip_prefix("atomic_") {
            return self
                .exec_atomic_intrinsic(name, args, generic_args, destination, locals, span)
                .map(|()| true);
        }
        if let Some(name) = name.strip_prefix("simd_") {
            return self
                .exec_simd_intrinsic(name, args, generic_args, destination, locals, span)
                .map(|()| true);
        }
        if self.exec_reflection_intrinsic(name, args, destination, locals)? {
            return Ok(true);
        }
        // FIXME(#17451): Add `f16` and `f128` intrinsics.
        if let Some(name) = name.strip_suffix("f64") {
            let result = match name {
                "sqrt" | "sin" | "cos" | "exp" | "exp2" | "log" | "log10" | "log2" | "fabs"
                | "floor" | "ceil" | "trunc" | "rint" | "nearbyint" | "round" | "roundeven" => {
                    let [arg] = args else {
                        return Err(MirEvalError::InternalError(
                            "f64 intrinsic signature doesn't match fn (f64) -> f64".into(),
                        ));
                    };
                    let arg = from_bytes!(f64, arg.get(self)?);
                    match name {
                        "sqrt" => arg.sqrt(),
                        "sin" => arg.sin(),
                        "cos" => arg.cos(),
                        "exp" => arg.exp(),
                        "exp2" => arg.exp2(),
                        "log" => arg.ln(),
                        "log10" => arg.log10(),
                        "log2" => arg.log2(),
                        "fabs" => arg.abs(),
                        "floor" => arg.floor(),
                        "ceil" => arg.ceil(),
                        "trunc" => arg.trunc(),
                        // FIXME: these rounds should be different, but only `.round()` is stable now.
                        "rint" => arg.round(),
                        "nearbyint" => arg.round(),
                        "round" => arg.round(),
                        "roundeven" => arg.round(),
                        _ => unreachable!(),
                    }
                }
                "pow" | "minnum" | "maxnum" | "copysign" => {
                    let [arg1, arg2] = args else {
                        return Err(MirEvalError::InternalError(
                            "f64 intrinsic signature doesn't match fn (f64, f64) -> f64".into(),
                        ));
                    };
                    let arg1 = from_bytes!(f64, arg1.get(self)?);
                    let arg2 = from_bytes!(f64, arg2.get(self)?);
                    match name {
                        "pow" => arg1.powf(arg2),
                        "minnum" => arg1.min(arg2),
                        "maxnum" => arg1.max(arg2),
                        "copysign" => arg1.copysign(arg2),
                        _ => unreachable!(),
                    }
                }
                "powi" => {
                    let [arg1, arg2] = args else {
                        return Err(MirEvalError::InternalError(
                            "powif64 signature doesn't match fn (f64, i32) -> f64".into(),
                        ));
                    };
                    let arg1 = from_bytes!(f64, arg1.get(self)?);
                    let arg2 = from_bytes!(i32, arg2.get(self)?);
                    arg1.powi(arg2)
                }
                "fma" => {
                    let [arg1, arg2, arg3] = args else {
                        return Err(MirEvalError::InternalError(
                            "fmaf64 signature doesn't match fn (f64, f64, f64) -> f64".into(),
                        ));
                    };
                    let arg1 = from_bytes!(f64, arg1.get(self)?);
                    let arg2 = from_bytes!(f64, arg2.get(self)?);
                    let arg3 = from_bytes!(f64, arg3.get(self)?);
                    arg1.mul_add(arg2, arg3)
                }
                _ if needs_override => not_supported!("unknown f64 intrinsic {name}"),
                _ => return Ok(false),
            };
            return destination.write_from_bytes(self, &result.to_le_bytes()).map(|()| true);
        }
        if let Some(name) = name.strip_suffix("f32") {
            let result = match name {
                "sqrt" | "sin" | "cos" | "exp" | "exp2" | "log" | "log10" | "log2" | "fabs"
                | "floor" | "ceil" | "trunc" | "rint" | "nearbyint" | "round" | "roundeven" => {
                    let [arg] = args else {
                        return Err(MirEvalError::InternalError(
                            "f32 intrinsic signature doesn't match fn (f32) -> f32".into(),
                        ));
                    };
                    let arg = from_bytes!(f32, arg.get(self)?);
                    match name {
                        "sqrt" => arg.sqrt(),
                        "sin" => arg.sin(),
                        "cos" => arg.cos(),
                        "exp" => arg.exp(),
                        "exp2" => arg.exp2(),
                        "log" => arg.ln(),
                        "log10" => arg.log10(),
                        "log2" => arg.log2(),
                        "fabs" => arg.abs(),
                        "floor" => arg.floor(),
                        "ceil" => arg.ceil(),
                        "trunc" => arg.trunc(),
                        // FIXME: these rounds should be different, but only `.round()` is stable now.
                        "rint" => arg.round(),
                        "nearbyint" => arg.round(),
                        "round" => arg.round(),
                        "roundeven" => arg.round(),
                        _ => unreachable!(),
                    }
                }
                "pow" | "minnum" | "maxnum" | "copysign" => {
                    let [arg1, arg2] = args else {
                        return Err(MirEvalError::InternalError(
                            "f32 intrinsic signature doesn't match fn (f32, f32) -> f32".into(),
                        ));
                    };
                    let arg1 = from_bytes!(f32, arg1.get(self)?);
                    let arg2 = from_bytes!(f32, arg2.get(self)?);
                    match name {
                        "pow" => arg1.powf(arg2),
                        "minnum" => arg1.min(arg2),
                        "maxnum" => arg1.max(arg2),
                        "copysign" => arg1.copysign(arg2),
                        _ => unreachable!(),
                    }
                }
                "powi" => {
                    let [arg1, arg2] = args else {
                        return Err(MirEvalError::InternalError(
                            "powif32 signature doesn't match fn (f32, i32) -> f32".into(),
                        ));
                    };
                    let arg1 = from_bytes!(f32, arg1.get(self)?);
                    let arg2 = from_bytes!(i32, arg2.get(self)?);
                    arg1.powi(arg2)
                }
                "fma" => {
                    let [arg1, arg2, arg3] = args else {
                        return Err(MirEvalError::InternalError(
                            "fmaf32 signature doesn't match fn (f32, f32, f32) -> f32".into(),
                        ));
                    };
                    let arg1 = from_bytes!(f32, arg1.get(self)?);
                    let arg2 = from_bytes!(f32, arg2.get(self)?);
                    let arg3 = from_bytes!(f32, arg3.get(self)?);
                    arg1.mul_add(arg2, arg3)
                }
                _ if needs_override => not_supported!("unknown f32 intrinsic {name}"),
                _ => return Ok(false),
            };
            return destination.write_from_bytes(self, &result.to_le_bytes()).map(|()| true);
        }
        match name {
            "size_of" => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "size_of generic arg is not provided".into(),
                    ));
                };
                let size = self.size_of_sized(ty, locals, "size_of arg")?;
                destination.write_from_bytes(self, &size.to_le_bytes()[0..destination.size])
            }
            "align_of" => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "align_of generic arg is not provided".into(),
                    ));
                };
                let align = self.layout(ty)?.align.bytes();
                destination.write_from_bytes(self, &align.to_le_bytes()[0..destination.size])
            }
            "size_of_val" => {
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
            "align_of_val" => {
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
            "type_id" => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "type_id generic arg is not provided".into(),
                    ));
                };
                let ty = self.type_id_ty(ty)?;
                let id = self.type_id(TypeIdOf::Ty(ty));
                destination.write_from_bytes(self, &id)
            }
            "type_name" => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "type_name generic arg is not provided".into(),
                    ));
                };
                let ty_name = crate::mir::type_name::rustc_type_name(self.db, ty)
                    .unwrap_or_else(|| self.db.type_name(ty, locals.body.owner.module(self.db)));
                let len = ty_name.len();
                let addr = self.heap_allocate(len, 1)?;
                self.write_memory(addr, ty_name.as_bytes())?;
                destination.slice(0..self.ptr_size()).write_from_bytes(self, &addr.to_bytes())?;
                destination
                    .slice(self.ptr_size()..2 * self.ptr_size())
                    .write_from_bytes(self, &super::word_bytes(len))
            }
            "needs_drop" => {
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "size_of generic arg is not provided".into(),
                    ));
                };
                let result = match has_drop_glue(&self.infcx, ty, self.param_env.param_env) {
                    DropGlue::HasDropGlue => true,
                    DropGlue::None => false,
                    DropGlue::DependOnParams => {
                        never!("should be fully monomorphized now");
                        true
                    }
                };
                destination.write_from_bytes(self, &[u8::from(result)])
            }
            "ptr_guaranteed_cmp" => {
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
            "saturating_add" | "saturating_sub" => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "saturating_add args are not provided".into(),
                    ));
                };
                let signed = matches!(lhs.ty.kind(), TyKind::Int(_));
                let bits = destination.size as u32 * 8;
                let ans = if signed {
                    let lhs = i128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::Yes));
                    let rhs = i128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::Yes));
                    let ans = match name {
                        "saturating_add" => lhs.saturating_add(rhs),
                        "saturating_sub" => lhs.saturating_sub(rhs),
                        _ => unreachable!(),
                    };
                    let max = i128::MAX >> (128 - bits);
                    ans.clamp(-max - 1, max) as u128
                } else {
                    let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                    let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                    let ans = match name {
                        "saturating_add" => lhs.saturating_add(rhs),
                        "saturating_sub" => lhs.saturating_sub(rhs),
                        _ => unreachable!(),
                    };
                    ans.min(u128::MAX >> (128 - bits))
                };
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            "wrapping_add" => {
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
            "ptr_offset_from_unsigned" | "ptr_offset_from" => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "wrapping_sub args are not provided".into(),
                    ));
                };
                let lhs = i128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                let rhs = i128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
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
            "wrapping_sub" => {
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
            "wrapping_mul" => {
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
            "wrapping_shl" => {
                // FIXME: signed
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "unchecked_shl args are not provided".into(),
                    ));
                };
                let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                let ans = lhs.wrapping_shl(rhs as u32);
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            "wrapping_shr" => {
                // FIXME: signed
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        "unchecked_shr args are not provided".into(),
                    ));
                };
                let signed = matches!(lhs.ty.kind(), TyKind::Int(_));
                let rhs = u128::from_le_bytes(pad16(rhs.get(self)?, IsSigned::No));
                let ans = if signed {
                    let lhs = i128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::Yes));
                    lhs.wrapping_shr(rhs as u32) as u128
                } else {
                    let lhs = u128::from_le_bytes(pad16(lhs.get(self)?, IsSigned::No));
                    lhs.wrapping_shr(rhs as u32)
                };
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            "unchecked_add" | "unchecked_sub" | "unchecked_mul" | "unchecked_shl"
            | "unchecked_shr" | "unchecked_div" | "unchecked_rem" | "exact_div" => {
                let [lhs, rhs] = args else {
                    return Err(MirEvalError::InternalError(
                        format!("{name} args are not provided").into(),
                    ));
                };
                let ans = self.unchecked_int(name, lhs, rhs)?;
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            "add_with_overflow" | "sub_with_overflow" | "mul_with_overflow" => {
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
                    let (ans, overflow) = match name {
                        "add_with_overflow" => lhs.overflowing_add(rhs),
                        "sub_with_overflow" => lhs.overflowing_sub(rhs),
                        "mul_with_overflow" => lhs.overflowing_mul(rhs),
                        _ => unreachable!(),
                    };
                    // The result fits when sign-extending its low bytes gives it back.
                    let fits = op_size == 16 || {
                        let shift = 128 - 8 * op_size as u32;
                        (ans << shift) >> shift == ans
                    };
                    (ans as u128, overflow || !fits)
                } else {
                    let (ans, overflow) = match name {
                        "add_with_overflow" => lhs.overflowing_add(rhs),
                        "sub_with_overflow" => lhs.overflowing_sub(rhs),
                        "mul_with_overflow" => lhs.overflowing_mul(rhs),
                        _ => unreachable!(),
                    };
                    (ans, overflow || ans.to_le_bytes()[op_size..].iter().any(|&it| it != 0))
                };
                let is_overflow = vec![u8::from(is_overflow)];
                let layout = self.layout(result_ty)?;
                let result = self.construct_with_layout(
                    layout.size.bytes_usize(),
                    &layout,
                    None,
                    [ans.to_le_bytes()[0..op_size].to_vec(), is_overflow]
                        .into_iter()
                        .map(IntervalOrOwned::Owned),
                )?;
                destination.write_from_bytes(self, &result)
            }
            "copy" | "copy_nonoverlapping" => {
                let [src, dst, offset] = args else {
                    return Err(MirEvalError::InternalError(
                        "copy_nonoverlapping args are not provided".into(),
                    ));
                };
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "copy_nonoverlapping generic arg is not provided".into(),
                    ));
                };
                let src = Address::from_bytes(src.get(self)?)?;
                let dst = Address::from_bytes(dst.get(self)?)?;
                let offset = from_bytes!(usize, offset.get(self)?);
                let size = self.size_of_sized(ty, locals, "copy_nonoverlapping ptr type")?;
                let size = offset * size;
                let src = Interval { addr: src, size };
                let dst = Interval { addr: dst, size };
                dst.write_from_interval(self, src)
            }
            "slice_get_unchecked" => {
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
            "offset" | "arith_offset" => {
                let [ptr, offset] = args else {
                    return Err(MirEvalError::InternalError("offset args are not provided".into()));
                };
                let ty = if name == "offset" {
                    let Some(ty0) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                        return Err(MirEvalError::InternalError(
                            "offset generic arg is not provided".into(),
                        ));
                    };
                    let Some(ty1) = generic_args.as_slice().get(1).and_then(|it| it.ty()) else {
                        return Err(MirEvalError::InternalError(
                            "offset generic arg is not provided".into(),
                        ));
                    };
                    if !matches!(
                        ty1.kind(),
                        TyKind::Int(rustc_type_ir::IntTy::Isize)
                            | TyKind::Uint(rustc_type_ir::UintTy::Usize)
                    ) {
                        return Err(MirEvalError::InternalError(
                            "offset generic arg is not usize or isize".into(),
                        ));
                    }
                    match ty0.kind() {
                        TyKind::RawPtr(ty, _) => ty,
                        _ => {
                            return Err(MirEvalError::InternalError(
                                "offset generic arg is not a raw pointer".into(),
                            ));
                        }
                    }
                } else {
                    let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                        return Err(MirEvalError::InternalError(
                            "arith_offset generic arg is not provided".into(),
                        ));
                    };
                    ty
                };
                let ptr = u128::from_le_bytes(pad16(ptr.get(self)?, IsSigned::No));
                let offset = u128::from_le_bytes(pad16(offset.get(self)?, IsSigned::No));
                let size = self.size_of_sized(ty, locals, "offset ptr type")? as u128;
                let ans = ptr + offset * size;
                destination.write_from_bytes(self, &ans.to_le_bytes()[0..destination.size])
            }
            "assert_inhabited"
            | "assert_zero_valid"
            | "assert_uninit_valid"
            | "assert_mem_uninitialized_valid" => {
                // FIXME: We should actually implement these checks
                Ok(())
            }
            "forget" => {
                // FIXME
                Ok(())
            }
            "transmute" | "transmute_unchecked" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "transmute arg is not provided".into(),
                    ));
                };
                destination.write_from_interval(self, arg.interval)
            }
            "black_box" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "black_box arg is not provided".into(),
                    ));
                };
                destination.write_from_interval(self, arg.interval)
            }
            "ctpop" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("ctpop arg is not provided".into()));
                };
                let result = u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)).count_ones();
                destination
                    .write_from_bytes(self, &(result as u128).to_le_bytes()[0..destination.size])
            }
            "bswap" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("bswap arg is not provided".into()));
                };
                let mut result = arg.get(self)?.to_vec();
                result.reverse();
                destination.write_from_bytes(self, &result)
            }
            "bitreverse" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "bitreverse arg is not provided".into(),
                    ));
                };
                let mut result = arg.get(self)?.to_vec();
                result.reverse();
                for byte in &mut result {
                    *byte = byte.reverse_bits();
                }
                destination.write_from_bytes(self, &result)
            }
            "ctlz" | "ctlz_nonzero" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("ctlz arg is not provided".into()));
                };
                let result =
                    u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)).leading_zeros() as usize;
                let result = result - (128 - arg.interval.size * 8);
                destination
                    .write_from_bytes(self, &(result as u128).to_le_bytes()[0..destination.size])
            }
            "cttz" | "cttz_nonzero" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError("cttz arg is not provided".into()));
                };
                let arg: &[u8] = arg.get(self)?;
                let bit_count = arg.len() as u32 * 8;
                let result = u128::from_le_bytes(pad16(arg, IsSigned::No)).trailing_zeros().min(bit_count);
                destination
                    .write_from_bytes(self, &(result as u128).to_le_bytes()[0..destination.size])
            }
            "rotate_left" => {
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
            "rotate_right" => {
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
            "variant_count" => {
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
            "discriminant_value" => {
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
            "const_eval_select" => {
                let [tuple, const_fn, _] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_eval_select args are not provided".into(),
                    ));
                };
                let mut args = vec![const_fn.clone()];
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
                    self.exec_fn_trait(
                        def,
                        &args,
                        // FIXME: wrong for manual impls of `FnOnce`
                        GenericArgs::empty(),
                        locals,
                        destination,
                        None,
                        span,
                    )?;
                    return Ok(true);
                }
                not_supported!("FnOnce was not available for executing const_eval_select");
            }
            "read_via_copy" | "volatile_load" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "read_via_copy args are not provided".into(),
                    ));
                };
                let addr = Address::from_bytes(arg.interval.get(self)?)?;
                destination.write_from_interval(self, Interval { addr, size: destination.size })
            }
            "write_via_move" => {
                let [ptr, val] = args else {
                    return Err(MirEvalError::InternalError(
                        "write_via_move args are not provided".into(),
                    ));
                };
                let dst = Address::from_bytes(ptr.get(self)?)?;
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "write_via_copy generic arg is not provided".into(),
                    ));
                };
                let size = self.size_of_sized(ty, locals, "write_via_move ptr type")?;
                Interval { addr: dst, size }.write_from_interval(self, val.interval)?;
                Ok(())
            }
            "write_bytes" => {
                let [dst, val, count] = args else {
                    return Err(MirEvalError::InternalError(
                        "write_bytes args are not provided".into(),
                    ));
                };
                let count = from_bytes!(usize, count.get(self)?);
                let val = from_bytes!(u8, val.get(self)?);
                let Some(ty) = generic_args.as_slice().first().and_then(|it| it.ty()) else {
                    return Err(MirEvalError::InternalError(
                        "write_bytes generic arg is not provided".into(),
                    ));
                };
                let dst = Address::from_bytes(dst.get(self)?)?;
                let size = self.size_of_sized(ty, locals, "copy_nonoverlapping ptr type")?;
                let size = count * size;
                self.write_memory_using_ref(dst, size)?.fill(val);
                Ok(())
            }
            "ptr_metadata" => {
                let [ptr] = args else {
                    return Err(MirEvalError::InternalError(
                        "ptr_metadata args are not provided".into(),
                    ));
                };
                let arg = ptr.interval.get(self)?.to_owned();
                let metadata = &arg[self.ptr_size()..];
                destination.write_from_bytes(self, metadata)?;
                Ok(())
            }
            "three_way_compare" => {
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
            "aggregate_raw_ptr" => {
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
            "fabs" => {
                let [arg] = args else {
                    return Err(MirEvalError::InternalError(
                        "fabs intrinsic signature doesn't match fn (T) -> T".into(),
                    ));
                };
                let mut bytes = arg.get(self)?.to_vec();
                if let Some(sign_byte) = bytes.last_mut() {
                    *sign_byte &= 0x7f;
                }
                destination.write_from_bytes(self, &bytes)
            }
            "unreachable" => {
                return Err(MirEvalError::UndefinedBehavior(
                    "`unreachable` intrinsic executed".to_owned(),
                ));
            }
            "compare_bytes" => {
                // `<[u8]>::cmp` and `memcmp` in `core`: rustc's evaluation gives the order as
                // -1, 0 or 1, where at run time only the sign is promised.
                let [left, right, bytes] = args else {
                    return Err(MirEvalError::InternalError(
                        "compare_bytes args are not provided".into(),
                    ));
                };
                let left = Address::from_bytes(left.get(self)?)?;
                let right = Address::from_bytes(right.get(self)?)?;
                let bytes = from_bytes!(usize, bytes.get(self)?);
                let order = self.read_memory(left, bytes)?.cmp(self.read_memory(right, bytes)?);
                destination.write_from_bytes(self, &(order as i32).to_le_bytes())
            }
            "const_allocate" => {
                let [size, align] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_allocate args are not provided".into(),
                    ));
                };
                let size = from_bytes!(usize, size.get(self)?);
                let align = from_bytes!(usize, align.get(self)?);
                let origin = Origin::ConstHeap { global: false };
                let result = self.allocate(size, align, origin)?;
                destination.write_from_bytes(self, &result.to_bytes())
            }
            "const_deallocate" => {
                let [ptr, ..] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_deallocate args are not provided".into(),
                    ));
                };
                let ptr = Address::from_bytes(ptr.get(self)?)?;
                let origin = Origin::ConstHeap { global: false };
                self.deallocate(ptr, origin, "const_deallocate")
            }
            "const_make_global" => {
                let [ptr] = args else {
                    return Err(MirEvalError::InternalError(
                        "const_make_global args are not provided".into(),
                    ));
                };
                self.make_global(Address::from_bytes(ptr.get(self)?)?)?;
                destination.write_from_interval(self, ptr.interval)
            }
            "caller_location" => {
                let site = self.caller_site(locals, span)?;
                let location = self.location_of(site, locals)?;
                destination.write_from_bytes(self, &location.to_bytes())
            }
            "box_new" => {
                let ty = generic_args.type_at(0);
                let Some((size, align)) = self.size_align_of(ty, locals)? else {
                    not_supported!("unsized box initialization");
                };
                let addr = self.allocate(size, align, Origin::Allocator)?;
                self.copy_from_interval(addr, args[0].interval)?;
                destination.write_from_bytes(self, &addr.to_bytes()[..self.ptr_size()])
            }
            _ if needs_override => not_supported!("intrinsic {name} is not implemented"),
            _ => return Ok(false),
        }
        .map(|()| true)
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
        name: &str,
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
        if let "unchecked_shl" | "unchecked_shr" = name {
            let amount =
                if signed(rhs) { int(rhs)? } else { uint(rhs)?.min(i128::MAX as u128) as i128 };
            if amount < 0 || amount >= i128::from(bits) {
                return ub(format!("overflowing shift by {amount} in `{name}`"));
            }
            return Ok(match (name, signed(lhs)) {
                ("unchecked_shl", _) => uint(lhs)? << amount,
                (_, true) => (int(lhs)? >> amount) as u128,
                _ => uint(lhs)? >> amount,
            });
        }
        let overflow = || format!("arithmetic overflow in `{name}`");
        let by_zero = || match name {
            "unchecked_rem" => "calculating the remainder with a divisor of zero".to_owned(),
            _ => "dividing by zero".to_owned(),
        };
        if signed(lhs) {
            let (a, b) = (int(lhs)?, int(rhs)?);
            let (min, max) = (i128::MIN >> (128 - bits), i128::MAX >> (128 - bits));
            let ans = match name {
                "unchecked_add" => a.checked_add(b),
                "unchecked_sub" => a.checked_sub(b),
                "unchecked_mul" => a.checked_mul(b),
                _ if b == 0 => return ub(by_zero()),
                _ if a == min && b == -1 => return ub(overflow()),
                "exact_div" if a % b != 0 => {
                    return ub(format!(
                        "exact_div: {a} cannot be divided by {b} without remainder"
                    ));
                }
                "unchecked_rem" => Some(a % b),
                _ => Some(a / b),
            };
            match ans {
                Some(ans) if (min..=max).contains(&ans) => Ok(ans as u128),
                _ => ub(overflow()),
            }
        } else {
            let (a, b) = (uint(lhs)?, uint(rhs)?);
            let max = u128::MAX >> (128 - bits);
            let ans = match name {
                "unchecked_add" => a.checked_add(b),
                "unchecked_sub" => a.checked_sub(b),
                "unchecked_mul" => a.checked_mul(b),
                _ if b == 0 => return ub(by_zero()),
                "exact_div" if a % b != 0 => {
                    return ub(format!(
                        "exact_div: {a} cannot be divided by {b} without remainder"
                    ));
                }
                "unchecked_rem" => Some(a % b),
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
        name: &str,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
        locals: &Locals<'db>,
        _span: MirSpan,
    ) -> Result<'db, ()> {
        // We are a single threaded runtime with no UB checking and no optimization, so
        // we can implement atomic intrinsics as normal functions. The pinned nightly takes the
        // orderings as const generics (`atomic_load::<T, ORD, VOLATILE>`); older names carried
        // them as suffixes (`atomic_load_seqcst`).
        let op = name.split('_').next().unwrap_or(name);
        if matches!(op, "fence" | "singlethreadfence") {
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
        let arg0_addr = Address::from_bytes(arg0.get(self)?)?;
        let arg0_interval =
            Interval::new(arg0_addr, self.size_of_sized(ty, locals, "atomic intrinsic type arg")?);
        if op == "load" {
            return destination.write_from_interval(self, arg0_interval);
        }
        let Some(arg1) = args.get(1) else {
            return Err(MirEvalError::InternalError(
                "atomic intrinsic arg1 is not provided".into(),
            ));
        };
        if op == "store" {
            return arg0_interval.write_from_interval(self, arg1.interval);
        }
        if op == "xchg" {
            destination.write_from_interval(self, arg0_interval)?;
            return arg0_interval.write_from_interval(self, arg1.interval);
        }
        let size = arg0_interval.size;
        let signed = matches!(ty.kind(), TyKind::Int(_));
        let read = |this: &Self, interval: Interval, sign: bool| -> Result<'db, u128> {
            let bytes = interval.get(this)?;
            Ok(u128::from_le_bytes(pad16(&bytes[..size.min(bytes.len())], sign.into())))
        };
        let combined = match op {
            "xadd" | "xsub" | "and" | "nand" | "or" | "xor" | "umax" | "umin" => {
                let lhs = read(self, arg0_interval, false)?;
                let rhs = read(self, arg1.interval, false)?;
                Some(match op {
                    "xadd" => lhs.wrapping_add(rhs),
                    "xsub" => lhs.wrapping_sub(rhs),
                    "and" => lhs & rhs,
                    "nand" => !(lhs & rhs),
                    "or" => lhs | rhs,
                    "xor" => lhs ^ rhs,
                    "umax" => lhs.max(rhs),
                    _ => lhs.min(rhs),
                })
            }
            "max" | "min" => {
                let lhs = read(self, arg0_interval, signed)? as i128;
                let rhs = read(self, arg1.interval, signed)? as i128;
                Some((if op == "max" { lhs.max(rhs) } else { lhs.min(rhs) }) as u128)
            }
            _ => None,
        };
        if let Some(ans) = combined {
            destination.write_from_interval(self, arg0_interval)?;
            return arg0_interval.write_from_bytes(self, &ans.to_le_bytes()[0..size]);
        }
        let Some(arg2) = args.get(2) else {
            return Err(MirEvalError::InternalError(
                "atomic intrinsic arg2 is not provided".into(),
            ));
        };
        if matches!(op, "cxchg" | "cxchgweak") {
            let dest = if arg1.get(self)? == arg0_interval.get(self)? {
                arg0_interval.write_from_interval(self, arg2.interval)?;
                (arg1.interval, true)
            } else {
                (arg0_interval, false)
            };
            let result_ty = Ty::new_tup_from_iter(
                self.interner(),
                [ty, Ty::new_bool(self.interner())].into_iter(),
            );
            let layout = self.layout(result_ty)?;
            let result = self.construct_with_layout(
                layout.size.bytes_usize(),
                &layout,
                None,
                [IntervalOrOwned::Borrowed(dest.0), IntervalOrOwned::Owned(vec![u8::from(dest.1)])]
                    .into_iter(),
            )?;
            return destination.write_from_bytes(self, &result);
        }
        not_supported!("unknown atomic intrinsic {name}");
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
        let id = from_bytes!(u128, arg.get(self)?);
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
        use rustc_type_ir::TypeFoldable;
        let ty = crate::mir::monomorphization::normalized_ty(self.db, ty, self.param_env)
            .map_err(|e| MirEvalError::NotSupported(format!("a `TypeId` of {ty:?}: {e:?}")))?;
        let interner = self.interner();
        let erased = Region::new_erased(interner);
        let ty = rustc_type_ir::fold_regions(interner, ty, |region, _| match region.kind() {
            rustc_type_ir::RegionKind::ReBound(..) => region,
            _ => erased,
        });
        Ok(ty.fold_with(&mut Anonymized(interner)))
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
        let fields = id.fields(self.db).fields().iter().map(|(_, field)| {
            Ok(IntervalOrOwned::Owned(match field.name.as_str() {
                "filename" | "file" => [addr.to_bytes(), super::word_bytes(name.len())].concat(),
                "line" => line.to_le_bytes().into(),
                "col" => col.to_le_bytes().into(),
                "_filename" => Vec::new(),
                other => not_supported!("a field `{other}` of `Location`"),
            }))
        });
        let fields = fields.collect::<Result<'db, Vec<_>>>()?;
        let bytes = self.construct_with_layout(size, &layout, tag, fields.into_iter())?;
        let location = self.heap_allocate(size, layout.align.abi.bytes() as usize)?;
        self.write_memory(location, &bytes)?;
        Ok(location)
    }

    /// `text` as a `&'static str` in `destination`.
    fn write_str(&mut self, destination: Interval, text: &str) -> Result<'db, ()> {
        let addr = self.heap_allocate(text.len(), 1)?;
        self.write_memory(addr, text.as_bytes())?;
        destination.slice(0..self.ptr_size()).write_from_bytes(self, &addr.to_bytes())?;
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
        let fields = value.map(|it| IntervalOrOwned::Owned(it.as_ref().to_vec()));
        let bytes = self.construct_with_layout(size, &layout, tag, fields.into_iter())?;
        destination.write_from_bytes(self, &bytes)
    }
}

/// Numbers the variables of every binder in what it folds in their order, with no names.
struct Anonymized<'db>(crate::next_solver::DbInterner<'db>);

impl<'db> rustc_type_ir::TypeFolder<crate::next_solver::DbInterner<'db>> for Anonymized<'db> {
    fn cx(&self) -> crate::next_solver::DbInterner<'db> {
        self.0
    }

    fn fold_binder<T>(
        &mut self,
        binder: rustc_type_ir::Binder<crate::next_solver::DbInterner<'db>, T>,
    ) -> rustc_type_ir::Binder<crate::next_solver::DbInterner<'db>, T>
    where
        T: rustc_type_ir::TypeFoldable<crate::next_solver::DbInterner<'db>>,
    {
        use rustc_type_ir::{Interner, TypeSuperFoldable};
        self.0.anonymize_bound_vars(binder.super_fold_with(self))
    }
}
